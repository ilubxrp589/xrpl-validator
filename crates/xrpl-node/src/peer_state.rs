//! Independence step 8: a ledger's whole account state fetched from peers, not from .39.
//!
//! A cold start today downloads the state from .39's RPC (`ledger_data`, [`crate::bulk_sync`]). rippled acquires a
//! ledger's state from its peers instead (`InboundLedger::trigger`, 3.4.1): it asks for the state tree's nodes by
//! position (`TMGetLedger`, `liAS_NODE`), and takes each node only when it is the one its parent names. This module
//! is that acquisition; the peer sessions and pacing live in the `peer_state_sync` binary.
//!
//! - [`StateTreeFetch`] starts from the header's `account_hash` and accepts a node only at a position a proven
//!   parent named, and only when it hashes to the hash the parent holds there: an inner node is `MIN\0` over its 16
//!   child hashes, a leaf `MLN\0` over the object and its key (`SHAMapInnerNode::updateHash`,
//!   `SHAMapAccountStateLeafNode::updateHash`), and a leaf's key must lie under its position. Each proven leaf goes
//!   to a sink exactly once. When no position is still wanted, every object of the ledger's state has been proven
//!   back to the header, and so to the ledger hash.
//! - Batches are sized the way a peer answers (`PeerImp::processLedgerRequest`, `SHAMap::getNodeFat`): each
//!   position asked for comes back with up to `queryDepth` levels below it (at most 3, the overlay's
//!   `Tuning::maxQueryDepth`), a reply takes no new position once it holds 8,192 nodes (`softMaxReplyNodes`), and it
//!   stops outright at 12,288 (`hardMaxReplyNodes`). Positions go out shallowest first, one depth per request, with
//!   the query depth that ends each reply on a level 3 below a multiple of 4, so the next positions sit at depth 4,
//!   then 8: for a ~20 M object state the frontier stays near 16^4 = 65,536 positions, not 16^5.
//! - A peer that lacks the ledger, or every node a request names, sends nothing at all (`processLedgerRequest`
//!   returns without a reply when it found no node, and `getNodeFat` fails a whole position when any node below it
//!   is missing). So a timeout is the only "no". A position a reply with room left unanswered counts a miss; after
//!   [`STUBBORN`] misses it is asked at query depth 0, so a node one peer lacks deep below it no longer sinks the
//!   whole subtree.
//! - [`LeafFile`] holds the proven leaves, appended as they arrive; [`Checkpoint`] holds the frontier, so an
//!   interrupted run resumes; [`recheck`] recomputes the root bottom-up from the file alone ([`state_root`]), the
//!   second and independent proof that what is on disk is the ledger's state.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha512};
use xrpl_ledger::ledger::header::{LedgerHeader, HEADER_SIZE};
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::wire::{
    NodeId, Offered, WireNode, LEAF_DEPTH, WIRE_ACCOUNT_STATE, WIRE_COMPRESSED_INNER, WIRE_INNER,
};

use crate::peer::protocol::{TmGetLedger, TmLedgerData, TmLedgerInfoType};

/// rippled's `Tuning::maxQueryDepth`: a deeper `querydepth` is refused as bad data (and charged).
pub const MAX_QUERY_DEPTH: u32 = 3;
/// rippled's `Tuning::softMaxReplyNodes`: a reply takes no further position once it holds this many nodes.
pub const SOFT_MAX_REPLY_NODES: usize = 8192;
/// rippled's `Tuning::hardMaxReplyNodes`: a reply stops here, even in the middle of a position.
pub const HARD_MAX_REPLY_NODES: usize = 12288;
/// Positions in one request at most (33 bytes each on the wire).
pub const MAX_IDS: usize = 512;
/// Misses after which a position is asked on its own terms: query depth 0.
pub const STUBBORN: u8 = 3;
/// Positions in one depth-0 request.
const STUBBORN_IDS: usize = 64;
/// Ask for a little more than a reply holds: positions a full reply skipped go back without a miss.
const OVERASK: f64 = 1.25;

/// The position at `depth` a key sits under: its first `depth` nibbles, the rest zero.
pub fn position_of(key: &[u8; 32], depth: u8) -> NodeId {
    let d = (depth.min(LEAF_DEPTH)) as usize;
    let mut id = [0u8; 32];
    id[..d / 2].copy_from_slice(&key[..d / 2]);
    if d % 2 == 1 {
        id[d / 2] = key[d / 2] & 0xf0;
    }
    NodeId { id, depth: d as u8 }
}

/// Nibble `depth` of a key.
fn nibble(key: &[u8; 32], depth: usize) -> u8 {
    let b = key[depth / 2];
    if depth % 2 == 0 {
        b >> 4
    } else {
        b & 0x0f
    }
}

/// A state leaf's hash as its parent records it: `MLN\0` over the object, then its key.
pub fn leaf_hash(key: &[u8; 32], item: &[u8]) -> [u8; 32] {
    let mut h = Sha512::new();
    h.update(HASH_PREFIX_LEAF_NODE);
    h.update(item);
    h.update(key);
    let full = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&full[..32]);
    out
}

/// One node a peer sent, proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked<'a> {
    /// An inner node: its 16 child hashes (zero where a branch is empty).
    Inner(Box<[[u8; 32]; 16]>),
    /// A leaf: the object's key and its bytes.
    Leaf { key: [u8; 32], item: &'a [u8] },
}

/// A node a peer sent at `pos` (rippled's `serializeForWire`, its last byte the wire type), checked against the hash
/// its parent holds there. Only inner nodes (full, or compressed: `hash || branch` per child) and account-state
/// leaves (`object || key`) belong to a state tree.
pub fn check_node<'a>(pos: NodeId, want: &[u8; 32], data: &'a [u8]) -> Result<Checked<'a>, &'static str> {
    let (&kind, body) = data.split_last().ok_or("an empty node")?;
    match kind {
        WIRE_INNER | WIRE_COMPRESSED_INNER => {
            if pos.depth >= LEAF_DEPTH {
                return Err("an inner node below the deepest level");
            }
            let Some(WireNode::Inner(h)) = WireNode::parse(data) else { return Err("a malformed inner node") };
            if h.iter().all(|c| *c == [0; 32]) {
                return Err("an inner node with no children");
            }
            if sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, h.as_flattened()).0 != *want {
                return Err("an inner node that is not the one its parent names");
            }
            Ok(Checked::Inner(h))
        }
        WIRE_ACCOUNT_STATE => {
            if pos.depth == 0 {
                return Err("a leaf at the root (a SHAMap's root is always an inner node)");
            }
            let at = body.len().checked_sub(32).ok_or("a leaf shorter than its key")?;
            if at == 0 {
                return Err("a leaf with no object");
            }
            // The wire leaf is `object || key`, exactly what its hash covers.
            if sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, body).0 != *want {
                return Err("a leaf that is not the one its parent names");
            }
            let key: [u8; 32] = body[at..].try_into().map_err(|_| "a leaf shorter than its key")?;
            if position_of(&key, pos.depth) != pos {
                return Err("a leaf whose key is not under its position");
            }
            Ok(Checked::Leaf { key, item: &body[..at] })
        }
        _ => Err("a node that does not belong to a state tree"),
    }
}

/// A peer's liBASE reply for ledger `hash`: the header, proven to hash to it, and the state tree's root node as the
/// peer sent it (`PeerImp::sendLedgerBase`: the header, then the state root when the ledger has state, then the
/// transaction root). The root is still only bytes: [`StateTreeFetch::offer`] proves it.
pub fn prove_base(hash: &[u8; 32], d: &TmLedgerData) -> Result<(LedgerHeader, Option<Vec<u8>>), String> {
    if d.ledger_hash != hash {
        return Err("a reply for another ledger".into());
    }
    if let Some(e) = d.error {
        return Err(format!("the peer answered error {e}"));
    }
    let first = d.nodes.first().ok_or("no header node")?;
    if first.nodedata.len() != HEADER_SIZE {
        return Err(format!("a header of {} bytes", first.nodedata.len()));
    }
    let header = LedgerHeader::deserialize(&first.nodedata).map_err(|e| e.to_string())?;
    if header.hash().0 != *hash {
        return Err("the header does not hash to the ledger hash".into());
    }
    let root = (header.account_hash.0 != [0; 32]).then(|| d.nodes.get(1).map(|n| n.nodedata.clone())).flatten();
    Ok((header, root))
}

/// The liBASE request for ledger `hash` (no query depth: rippled refuses one on liBASE).
pub fn base_request(hash: &[u8; 32]) -> TmGetLedger {
    TmGetLedger {
        itype: TmLedgerInfoType::LiBase as i32,
        ltype: None,
        ledger_hash: Some(hash.to_vec()),
        ledger_seq: None,
        node_i_ds: Vec::new(),
        request_cookie: None,
        query_type: None,
        query_depth: None,
    }
}

/// The positions for one liAS_NODE request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub ticket: u64,
    pub ids: Vec<NodeId>,
    pub query_depth: u32,
}

impl Batch {
    /// The request. No ledger sequence (a wrong one is charged as malformed), no query type (that asks the peer to
    /// relay it when it lacks the ledger), no cookie (that marks a relayed request).
    pub fn request(&self, ledger_hash: &[u8; 32]) -> TmGetLedger {
        TmGetLedger {
            itype: TmLedgerInfoType::LiAsNode as i32,
            ltype: None,
            ledger_hash: Some(ledger_hash.to_vec()),
            ledger_seq: None,
            node_i_ds: self.ids.iter().map(NodeId::to_raw).collect(),
            request_cookie: None,
            query_type: None,
            query_depth: Some(self.query_depth),
        }
    }
}

/// The query depth for positions at `depth`: the reply ends 3 levels below a multiple of 4.
pub fn query_depth_for(depth: u8) -> u32 {
    MAX_QUERY_DEPTH - (u32::from(depth) % 4)
}

#[derive(Debug, Clone)]
struct Ticket {
    depth: u8,
    stubborn: bool,
    ids: Vec<NodeId>,
}

/// Running totals for a fetch (kept across a resume).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub inner: u64,
    pub leaves: u64,
    /// The objects' bytes (what the leaves hold, keys not counted).
    pub leaf_bytes: u64,
    /// Nodes refused: malformed, or not what their parent names.
    pub rejected: u64,
    /// Nodes at positions nobody wanted (late, repeated, or never named).
    pub unasked: u64,
    /// Positions a reply with room left unanswered.
    pub misses: u64,
}

impl Counts {
    pub fn nodes(&self) -> u64 {
        self.inner + self.leaves
    }
}

type Key = (u8, [u8; 32]);

fn key_of(p: NodeId) -> Key {
    (p.depth, p.id)
}

/// A ledger's state tree, assembled from fetched nodes, each proven against its parent.
#[derive(Debug)]
pub struct StateTreeFetch {
    account_hash: [u8; 32],
    /// Positions a proven parent named, not received yet, and the hash the parent holds there.
    expected: HashMap<NodeId, [u8; 32]>,
    /// Expected, not in a request now: shallowest first.
    available: BTreeSet<Key>,
    /// The same, for positions past [`STUBBORN`] misses.
    stubborn: BTreeSet<Key>,
    inflight: HashMap<NodeId, u64>,
    misses: HashMap<NodeId, u8>,
    tickets: HashMap<u64, Ticket>,
    next_ticket: u64,
    /// Per depth: nodes replies brought, and positions they answered (to size batches).
    seen: Vec<(u64, u64)>,
    /// The nodes a reply holds before the peer stops taking positions ([`SOFT_MAX_REPLY_NODES`]).
    reply_cap: usize,
    pub counts: Counts,
}

impl StateTreeFetch {
    /// Start from the header's state root. A zero root is an empty state: already complete.
    pub fn new(account_hash: [u8; 32]) -> Self {
        let mut f = StateTreeFetch {
            account_hash,
            expected: HashMap::new(),
            available: BTreeSet::new(),
            stubborn: BTreeSet::new(),
            inflight: HashMap::new(),
            misses: HashMap::new(),
            tickets: HashMap::new(),
            next_ticket: 1,
            seen: vec![(0, 0); LEAF_DEPTH as usize + 1],
            reply_cap: SOFT_MAX_REPLY_NODES,
            counts: Counts::default(),
        };
        if account_hash != [0; 32] {
            f.expect(NodeId::ROOT, account_hash);
        }
        f
    }

    /// Pick up where a [`Checkpoint`] left off: the positions still wanted, and the totals so far.
    pub fn resume(account_hash: [u8; 32], frontier: &[(NodeId, [u8; 32])], counts: Counts) -> Self {
        let mut f = StateTreeFetch::new([0; 32]);
        f.account_hash = account_hash;
        for (p, h) in frontier {
            f.expect(*p, *h);
        }
        f.counts = counts;
        f
    }

    pub fn account_hash(&self) -> [u8; 32] {
        self.account_hash
    }

    /// A peer that stops taking positions at another count than rippled's (tests).
    pub fn set_reply_cap(&mut self, nodes: usize) {
        self.reply_cap = nodes.max(1);
    }

    fn expect(&mut self, pos: NodeId, hash: [u8; 32]) {
        self.expected.insert(pos, hash);
        self.requeue(pos);
    }

    fn requeue(&mut self, pos: NodeId) {
        if self.misses.get(&pos).copied().unwrap_or(0) >= STUBBORN {
            self.stubborn.insert(key_of(pos));
        } else {
            self.available.insert(key_of(pos));
        }
    }

    fn take(&mut self, pos: &NodeId) -> Option<[u8; 32]> {
        let h = self.expected.remove(pos)?;
        self.available.remove(&key_of(*pos));
        self.stubborn.remove(&key_of(*pos));
        self.inflight.remove(pos);
        self.misses.remove(pos);
        Some(h)
    }

    /// Nothing still wanted: every node has been proven.
    pub fn is_complete(&self) -> bool {
        self.expected.is_empty()
    }

    /// Positions still wanted (in a request or not).
    pub fn wanted(&self) -> usize {
        self.expected.len()
    }

    /// Positions in a request now.
    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// Wanted positions past [`STUBBORN`] misses.
    pub fn stubborn(&self) -> usize {
        self.misses.values().filter(|m| **m >= STUBBORN).count()
    }

    /// What is still wanted, for a [`Checkpoint`].
    pub fn frontier(&self) -> Vec<(NodeId, [u8; 32])> {
        let mut v: Vec<(NodeId, [u8; 32])> = self.expected.iter().map(|(p, h)| (*p, *h)).collect();
        v.sort_by_key(|(p, _)| key_of(*p));
        v
    }

    /// Nodes one position at `depth` is expected to bring back at query depth `q`: what replies showed so far,
    /// else a full subtree.
    fn estimate(&self, depth: u8, q: u32) -> f64 {
        match self.seen[depth as usize] {
            (n, p) if p > 0 => (n as f64 / p as f64).max(1.0),
            _ => (0..=q).map(|i| 16f64.powi(i as i32)).sum(),
        }
    }

    /// The next request: positions not in a request now, all at the shallowest such depth, as many as one reply
    /// should hold. Positions past [`STUBBORN`] misses go first, at query depth 0. None when every wanted position
    /// is already out.
    pub fn next_batch(&mut self) -> Option<Batch> {
        let stubborn = !self.stubborn.is_empty();
        let first = if stubborn { *self.stubborn.first()? } else { *self.available.first()? };
        let depth = first.0;
        let (q, cap) = if stubborn {
            (0, STUBBORN_IDS)
        } else {
            let q = query_depth_for(depth);
            let n = (self.reply_cap as f64 * OVERASK / self.estimate(depth, q)).ceil() as usize;
            (q, n.clamp(1, MAX_IDS))
        };
        let set = if stubborn { &mut self.stubborn } else { &mut self.available };
        let mut ids = Vec::new();
        while ids.len() < cap && set.first().is_some_and(|k| k.0 == depth) {
            let Some((d, id)) = set.pop_first() else { break };
            ids.push(NodeId { id, depth: d });
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        for id in &ids {
            self.inflight.insert(*id, ticket);
        }
        self.tickets.insert(ticket, Ticket { depth, stubborn, ids: ids.clone() });
        Some(Batch { ticket, ids, query_depth: q })
    }

    /// The reply to `ticket` (`reply_nodes` nodes) has been offered. Positions it left unanswered go back to be
    /// asked again; when the reply had room (fewer than [`SOFT_MAX_REPLY_NODES`] nodes, so the peer looked at
    /// every position and found nothing), each counts a miss.
    pub fn settle(&mut self, ticket: u64, reply_nodes: usize) {
        let Some(t) = self.tickets.remove(&ticket) else { return };
        let full = reply_nodes >= self.reply_cap;
        let mut answered = 0u64;
        for pos in t.ids {
            if self.inflight.get(&pos) == Some(&ticket) {
                self.inflight.remove(&pos);
                if self.expected.contains_key(&pos) {
                    if !full {
                        let m = self.misses.entry(pos).or_default();
                        *m = m.saturating_add(1);
                        self.counts.misses += 1;
                    }
                    self.requeue(pos);
                }
            } else if !self.expected.contains_key(&pos) {
                answered += 1;
            }
        }
        if !t.stubborn && answered > 0 {
            let s = &mut self.seen[t.depth as usize];
            s.0 += reply_nodes as u64;
            s.1 += answered;
        }
    }

    /// No reply to `ticket` (a timeout, or the peer went away): its positions go back, with no miss.
    pub fn release(&mut self, ticket: u64) {
        let Some(t) = self.tickets.remove(&ticket) else { return };
        for pos in t.ids {
            if self.inflight.get(&pos) == Some(&ticket) {
                self.inflight.remove(&pos);
                if self.expected.contains_key(&pos) {
                    self.requeue(pos);
                }
            }
        }
    }

    /// Offer a reply's nodes, in any order. A node is taken when its position is wanted and it proves out, and then
    /// so are its descendants in the same reply; every proven leaf goes to `sink` (key, object). A node that does not
    /// prove out leaves its position wanted.
    pub fn offer<'a>(
        &mut self,
        nodes: impl IntoIterator<Item = (NodeId, &'a [u8])>,
        sink: &mut dyn FnMut(&[u8; 32], &[u8]),
    ) -> Offered {
        let mut out = Offered::default();
        let mut reply: HashMap<NodeId, &'a [u8]> = HashMap::new();
        for (id, data) in nodes {
            if reply.insert(id, data).is_some() {
                out.unasked += 1;
            }
        }
        // Wanted positions are the frontier: none lies below another, so each start's subtree is its own.
        let starts: Vec<NodeId> = reply.keys().filter(|p| self.expected.contains_key(*p)).copied().collect();
        for start in starts {
            let Some(want) = self.take(&start) else { continue };
            let mut stack = vec![(start, want)];
            while let Some((pos, want)) = stack.pop() {
                let Some(data) = reply.remove(&pos) else {
                    self.expect(pos, want);
                    continue;
                };
                match check_node(pos, &want, data) {
                    Ok(Checked::Inner(children)) => {
                        out.accepted += 1;
                        self.counts.inner += 1;
                        for (b, h) in children.iter().enumerate() {
                            if *h != [0; 32] {
                                stack.push((pos.child(b as u8), *h));
                            }
                        }
                    }
                    Ok(Checked::Leaf { key, item }) => {
                        out.accepted += 1;
                        self.counts.leaves += 1;
                        self.counts.leaf_bytes += item.len() as u64;
                        sink(&key, item);
                    }
                    Err(_) => {
                        out.rejected += 1;
                        self.counts.rejected += 1;
                        self.expect(pos, want);
                    }
                }
            }
        }
        out.unasked += reply.len();
        self.counts.unasked += out.unasked as u64;
        out
    }
}

// ---- the leaves on disk, the checkpoint, and the bottom-up recheck ------------------------------------------

/// The proven leaves, appended as they arrive: per leaf its 32-byte key, the object's length (u32, little endian),
/// then the object. Nothing in it is trusted on its own: [`recheck`] proves the whole file against the root.
pub struct LeafFile {
    w: BufWriter<File>,
    len: u64,
}

impl LeafFile {
    /// Open `path` to append at byte `at` (0 for a fresh file; a checkpoint's length on resume, cutting off what
    /// was written after that checkpoint).
    pub fn open(path: &Path, at: u64) -> io::Result<LeafFile> {
        let mut f = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path)?;
        let have = f.metadata()?.len();
        if have < at {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} holds {have} bytes, the checkpoint names {at}", path.display()),
            ));
        }
        f.set_len(at)?;
        f.seek(SeekFrom::Start(at))?;
        Ok(LeafFile { w: BufWriter::with_capacity(4 << 20, f), len: at })
    }

    pub fn append(&mut self, key: &[u8; 32], item: &[u8]) -> io::Result<()> {
        let n = u32::try_from(item.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "object over 4 GB"))?;
        self.w.write_all(key)?;
        self.w.write_all(&n.to_le_bytes())?;
        self.w.write_all(item)?;
        self.len += 36 + u64::from(n);
        Ok(())
    }

    /// Flush and sync; the length now safely on disk.
    pub fn sync(&mut self) -> io::Result<u64> {
        self.w.flush()?;
        self.w.get_ref().sync_data()?;
        Ok(self.len)
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Every leaf in a [`LeafFile`], in file order. The count read.
pub fn read_leaves(path: &Path, mut f: impl FnMut(&[u8; 32], &[u8])) -> io::Result<u64> {
    let mut r = BufReader::with_capacity(4 << 20, File::open(path)?);
    let mut head = [0u8; 36];
    let mut item = Vec::new();
    let mut n = 0u64;
    loop {
        match r.read_exact(&mut head[..1]) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(n),
            Err(e) => return Err(e),
        }
        r.read_exact(&mut head[1..])?;
        let key: [u8; 32] = head[..32].try_into().expect("32 bytes");
        let len = u32::from_le_bytes(head[32..36].try_into().expect("4 bytes")) as usize;
        item.resize(len, 0);
        r.read_exact(&mut item)?;
        f(&key, &item);
        n += 1;
    }
}

/// The root of a state tree holding these leaves, each (key, leaf hash), sorted by key with no key twice. The same
/// shape rippled's SHAMap has: a leaf sits at the shallowest depth where it is alone under its prefix, an inner
/// node hashes its 16 children (zero where empty), and the root is always an inner node.
pub fn state_root(sorted: &[([u8; 32], [u8; 32])]) -> [u8; 32] {
    if sorted.is_empty() {
        return [0; 32];
    }
    subtree_hash(sorted, 0)
}

fn subtree_hash(s: &[([u8; 32], [u8; 32])], depth: usize) -> [u8; 32] {
    if (s.len() == 1 && depth > 0) || depth >= LEAF_DEPTH as usize {
        return s[0].1;
    }
    let mut children = [[0u8; 32]; 16];
    let mut rest = s;
    for (b, child) in children.iter_mut().enumerate() {
        let n = rest.partition_point(|(k, _)| nibble(k, depth) as usize <= b);
        if n > 0 {
            *child = subtree_hash(&rest[..n], depth + 1);
        }
        rest = &rest[n..];
    }
    sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, children.as_flattened()).0
}

/// What [`recheck`] found in a leaf file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recheck {
    pub leaves: u64,
    /// Keys that appear more than once (a correct fetch writes each leaf once).
    pub repeated: u64,
    /// The root the file's leaves hash to.
    pub root: [u8; 32],
}

/// Recompute the state root bottom-up from a [`LeafFile`] alone: every leaf hashed again from its bytes, sorted by
/// key, and the tree's root rebuilt. 64 bytes a leaf in memory (about 1.3 GB for 20 million).
pub fn recheck(path: &Path, expect_leaves: u64) -> io::Result<Recheck> {
    let mut v: Vec<([u8; 32], [u8; 32])> = Vec::with_capacity(expect_leaves as usize);
    let leaves = read_leaves(path, |k, item| v.push((*k, leaf_hash(k, item))))?;
    v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let before = v.len();
    v.dedup_by(|a, b| a.0 == b.0);
    let repeated = (before - v.len()) as u64;
    Ok(Recheck { leaves, repeated, root: state_root(&v) })
}

/// A fetch's state at one moment, enough to resume it: the proven header, how much of the leaf file is good, the
/// totals, and every position still wanted with the hash its parent named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub header: [u8; HEADER_SIZE],
    pub leaves_len: u64,
    pub counts: Counts,
    pub frontier: Vec<(NodeId, [u8; 32])>,
}

const CHECKPOINT_MAGIC: &[u8; 4] = b"PSS1";

impl Checkpoint {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + HEADER_SIZE + 8 * 8 + self.frontier.len() * 65);
        v.extend_from_slice(CHECKPOINT_MAGIC);
        v.extend_from_slice(&self.header);
        v.extend_from_slice(&self.leaves_len.to_le_bytes());
        let c = &self.counts;
        for x in [c.inner, c.leaves, c.leaf_bytes, c.rejected, c.unasked, c.misses] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(&(self.frontier.len() as u64).to_le_bytes());
        for (p, h) in &self.frontier {
            v.push(p.depth);
            v.extend_from_slice(&p.id);
            v.extend_from_slice(h);
        }
        v
    }

    pub fn from_bytes(b: &[u8]) -> Option<Checkpoint> {
        fn take<'b>(b: &'b [u8], at: &mut usize, n: usize) -> Option<&'b [u8]> {
            let s = b.get(*at..at.checked_add(n)?)?;
            *at += n;
            Some(s)
        }
        let mut at = 0usize;
        if take(b, &mut at, 4)? != CHECKPOINT_MAGIC {
            return None;
        }
        let header: [u8; HEADER_SIZE] = take(b, &mut at, HEADER_SIZE)?.try_into().ok()?;
        let mut u64s = [0u64; 8];
        for x in u64s.iter_mut() {
            *x = u64::from_le_bytes(take(b, &mut at, 8)?.try_into().ok()?);
        }
        let [leaves_len, inner, leaves, leaf_bytes, rejected, unasked, misses, n] = u64s;
        let mut frontier = Vec::with_capacity(n.min(1 << 24) as usize);
        for _ in 0..n {
            let r = take(b, &mut at, 65)?;
            let id = NodeId::from_raw(&[&r[1..33], &r[..1]].concat())?;
            frontier.push((id, r[33..65].try_into().ok()?));
        }
        if at != b.len() {
            return None;
        }
        let counts = Counts { inner, leaves, leaf_bytes, rejected, unasked, misses };
        Some(Checkpoint { header, leaves_len, counts, frontier })
    }

    /// Write it beside `path` and rename it into place, so a crash leaves the old one or the new one, whole.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&self.to_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    pub fn load(path: &Path) -> io::Result<Option<Checkpoint>> {
        match std::fs::read(path) {
            Ok(b) => Checkpoint::from_bytes(&b)
                .map(Some)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{} does not parse", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---- peers --------------------------------------------------------------------------------------------------

/// An address from a peer's TMEndpoints worth dialing: a public IPv4 address and port (an IPv4-mapped IPv6 one is
/// taken as IPv4). Private, loopback, link-local, shared (100.64/10), documentation, multicast and unspecified
/// addresses are skipped, as live_viewer's discovery does.
pub fn dialable(endpoint: &str) -> Option<String> {
    let e = endpoint.trim();
    let e = match e.strip_prefix("[::ffff:") {
        Some(rest) => {
            let (ip, port) = rest.split_once("]:")?;
            format!("{ip}:{port}")
        }
        None => e.to_string(),
    };
    let (host, port) = e.rsplit_once(':')?;
    let ip: std::net::Ipv4Addr = host.parse().ok()?;
    let port: u16 = port.parse().ok()?;
    let o = ip.octets();
    let shared = o[0] == 100 && (o[1] & 0xc0) == 64;
    if port == 0
        || o[0] == 0
        || shared
        || ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
    {
        return None;
    }
    Some(format!("{ip}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use xrpl_core::types::Hash256;
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

    use crate::peer::protocol::TmLedgerNode;

    /// Distinct, well-spread keys, plus a few that share long prefixes (deep leaves and chains of inner nodes).
    fn leaves(n: usize) -> Vec<([u8; 32], Vec<u8>)> {
        let mut v: Vec<([u8; 32], Vec<u8>)> = (0..n)
            .map(|i| {
                let k = sha512_half_prefixed(b"KEY\0", &(i as u64).to_be_bytes()).0;
                (k, vec![(i % 251) as u8; 20 + i % 90])
            })
            .collect();
        let mut deep = v[0].0;
        for j in 0..3u8 {
            deep[31] = deep[31].wrapping_add(j + 1);
            v.push((deep, vec![0xD0 + j; 33]));
        }
        v
    }

    fn wire_leaf(key: &[u8; 32], item: &[u8]) -> Vec<u8> {
        let mut d = item.to_vec();
        d.extend_from_slice(key);
        d.push(WIRE_ACCOUNT_STATE);
        d
    }

    /// Every node of the state tree of `leaves`, at its position, in wire form; and the root hash.
    fn state_tree(leaves: &[([u8; 32], Vec<u8>)]) -> (HashMap<NodeId, Vec<u8>>, [u8; 32]) {
        let mut sorted: Vec<&([u8; 32], Vec<u8>)> = leaves.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = HashMap::new();
        let root = build(&sorted, NodeId::ROOT, &mut out);
        (out, root)
    }

    fn build(items: &[&([u8; 32], Vec<u8>)], at: NodeId, out: &mut HashMap<NodeId, Vec<u8>>) -> [u8; 32] {
        if items.len() == 1 && at.depth > 0 {
            out.insert(at, wire_leaf(&items[0].0, &items[0].1));
            return leaf_hash(&items[0].0, &items[0].1);
        }
        let mut h = Box::new([[0u8; 32]; 16]);
        for (b, slot) in h.iter_mut().enumerate() {
            let bucket: Vec<&([u8; 32], Vec<u8>)> =
                items.iter().filter(|it| nibble(&it.0, at.depth as usize) as usize == b).copied().collect();
            if !bucket.is_empty() {
                *slot = build(&bucket, at.child(b as u8), out);
            }
        }
        let node = WireNode::Inner(h);
        out.insert(at, node.serialize());
        node.hash().0
    }

    fn children_of(data: &[u8]) -> Option<Box<[[u8; 32]; 16]>> {
        match WireNode::parse(data)? {
            WireNode::Inner(h) => Some(h),
            _ => None,
        }
    }

    /// A peer that answers as rippled does: `SHAMap::getNodeFat` per position (fat leaves; the depth drops by one a
    /// level, except through an inner node with a single child; a node it lacks anywhere on the way or below fails
    /// the whole position), and `processLedgerRequest`'s caps. None when it found nothing (rippled sends nothing).
    struct Peer<'t> {
        tree: &'t HashMap<NodeId, Vec<u8>>,
        lacks: HashSet<NodeId>,
        soft: usize,
        hard: usize,
        asked: usize,
    }

    impl<'t> Peer<'t> {
        fn new(tree: &'t HashMap<NodeId, Vec<u8>>) -> Self {
            Peer { tree, lacks: HashSet::new(), soft: SOFT_MAX_REPLY_NODES, hard: HARD_MAX_REPLY_NODES, asked: 0 }
        }

        fn node_fat(&self, wanted: NodeId, depth: u32) -> Option<Vec<(NodeId, Vec<u8>)>> {
            for d in 0..=wanted.depth {
                if self.lacks.contains(&NodeId { id: position_of(&wanted.id, d).id, depth: d }) {
                    return None;
                }
            }
            self.tree.get(&wanted)?;
            let mut out = Vec::new();
            let mut stack = vec![(wanted, depth)];
            while let Some((id, d)) = stack.pop() {
                let data = &self.tree[&id];
                out.push((id, data.clone()));
                let Some(ch) = children_of(data) else { continue };
                let bc = ch.iter().filter(|c| **c != [0; 32]).count();
                if d == 0 && bc != 1 {
                    continue;
                }
                for (b, c) in ch.iter().enumerate() {
                    if *c == [0; 32] {
                        continue;
                    }
                    let cid = id.child(b as u8);
                    if self.lacks.contains(&cid) {
                        return None;
                    }
                    let cdata = &self.tree[&cid];
                    if children_of(cdata).is_some() && (d > 1 || bc == 1) {
                        stack.push((cid, if bc > 1 { d - 1 } else { d }));
                    } else {
                        out.push((cid, cdata.clone()));
                    }
                }
            }
            Some(out)
        }

        fn serve(&mut self, req: &TmGetLedger, hash: &[u8; 32]) -> Option<TmLedgerData> {
            self.asked += 1;
            assert_eq!(req.itype, TmLedgerInfoType::LiAsNode as i32);
            assert!(req.query_depth.unwrap() <= MAX_QUERY_DEPTH);
            let mut nodes = Vec::new();
            for raw in &req.node_i_ds {
                if nodes.len() >= self.soft {
                    break;
                }
                let id = NodeId::from_raw(raw).expect("a well-formed node id");
                for (nid, data) in self.node_fat(id, req.query_depth.unwrap()).unwrap_or_default() {
                    if nodes.len() >= self.hard {
                        break;
                    }
                    nodes.push(TmLedgerNode { nodedata: data, nodeid: Some(nid.to_raw()) });
                }
            }
            (!nodes.is_empty()).then(|| TmLedgerData {
                ledger_hash: hash.to_vec(),
                ledger_seq: 1,
                r#type: TmLedgerInfoType::LiAsNode as i32,
                nodes,
                request_cookie: None,
                error: None,
            })
        }
    }

    fn offer_reply(f: &mut StateTreeFetch, d: &TmLedgerData, got: &mut Vec<([u8; 32], Vec<u8>)>) -> Offered {
        let nodes = d.nodes.iter().filter_map(|n| Some((NodeId::from_raw(n.nodeid.as_deref()?)?, n.nodedata.as_slice())));
        f.offer(nodes, &mut |k, item| got.push((*k, item.to_vec())))
    }

    /// Fetch until complete from `peers` in turn; the leaves as they came, and the requests made.
    fn run(f: &mut StateTreeFetch, peers: &mut [Peer], hash: [u8; 32]) -> (Vec<([u8; 32], Vec<u8>)>, usize) {
        let mut got = Vec::new();
        let mut requests = 0;
        while let Some(b) = f.next_batch() {
            let p = &mut peers[requests % peers.len()];
            requests += 1;
            assert!(requests < 10_000, "no end in sight");
            match p.serve(&b.request(&hash), &hash) {
                Some(d) => {
                    let o = offer_reply(f, &d, &mut got);
                    assert_eq!(o.rejected, 0);
                    f.settle(b.ticket, d.nodes.len());
                }
                None => f.release(b.ticket),
            }
        }
        (got, requests)
    }

    fn sorted(mut v: Vec<([u8; 32], Vec<u8>)>) -> Vec<([u8; 32], Vec<u8>)> {
        v.sort();
        v
    }

    #[test]
    fn the_bottom_up_root_is_the_shamaps_own() {
        let l = leaves(3000);
        let mut map = SHAMap::new(TreeType::State);
        let mut v: Vec<([u8; 32], [u8; 32])> = Vec::new();
        for (k, item) in &l {
            map.insert_hash_only(Hash256(*k), Hash256(leaf_hash(k, item))).unwrap();
            v.push((*k, leaf_hash(k, item)));
        }
        v.sort();
        assert_eq!(state_root(&v), map.root_hash().0, "the same root as xrpl-ledger's SHAMap");
        assert_eq!(state_tree(&l).1, map.root_hash().0, "and the wire fixture's");
        assert_eq!(state_root(&[]), [0; 32]);
        // A lone leaf still hangs below an inner root (rippled's root is always a SHAMapInnerNode, hashed over its
        // 16 slots; tx_tree's root follows the same rule). xrpl-ledger's SHAMap collapses that case to the leaf's
        // hash, which no ledger's state (millions of objects) ever meets, so it is checked here against the rule.
        let one = [([0x7Au8; 32], [9u8; 32])];
        let mut slots = [[0u8; 32]; 16];
        slots[7] = [9; 32];
        assert_eq!(state_root(&one), sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, slots.as_flattened()).0);
    }

    #[test]
    fn a_leaf_hashes_over_its_object_then_its_key() {
        let (k, item) = ([0xAB; 32], b"an object".to_vec());
        let mut body = item.clone();
        body.extend_from_slice(&k);
        assert_eq!(leaf_hash(&k, &item), sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &body).0);
    }

    #[test]
    fn nodes_prove_against_their_parent_and_position() {
        let (k, item) = ([0x5C; 32], vec![1u8; 40]);
        let w = wire_leaf(&k, &item);
        let h = leaf_hash(&k, &item);
        let pos = position_of(&k, 3);
        assert_eq!(pos, NodeId::ROOT.child(5).child(0xC).child(5));
        assert_eq!(check_node(pos, &h, &w), Ok(Checked::Leaf { key: k, item: &item }));
        assert!(check_node(pos, &[0; 32], &w).is_err(), "not the hash the parent holds");
        assert!(check_node(NodeId::ROOT.child(6), &h, &w).is_err(), "a key that is not under its position");
        let mut bad = w.clone();
        bad[3] ^= 1;
        assert!(check_node(pos, &h, &bad).is_err(), "one bit of the object");
        let mut tx = w.clone();
        *tx.last_mut().unwrap() = 4; // transaction-with-metadata
        assert!(check_node(pos, &h, &tx).is_err(), "only state leaves belong to a state tree");
        assert!(check_node(pos, &h, &[]).is_err());

        // Inner nodes: compressed below rippled's cutoff, full above; both hash over all 16 slots.
        let mut few = Box::new([[0u8; 32]; 16]);
        few[2] = [1; 32];
        few[9] = [2; 32];
        let fw = WireNode::Inner(few.clone()).serialize();
        assert_eq!(*fw.last().unwrap(), WIRE_COMPRESSED_INNER);
        let fh = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, few.as_flattened()).0;
        assert_eq!(check_node(NodeId::ROOT, &fh, &fw), Ok(Checked::Inner(few.clone())));
        let full = Box::new([[3u8; 32]; 16]);
        let uw = WireNode::Inner(full.clone()).serialize();
        assert_eq!(*uw.last().unwrap(), WIRE_INNER);
        let uh = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, full.as_flattened()).0;
        assert_eq!(check_node(NodeId::ROOT, &uh, &uw), Ok(Checked::Inner(full)));
        assert!(check_node(NodeId::ROOT, &uh, &fw).is_err());
        let deepest = position_of(&[0x11; 32], LEAF_DEPTH);
        assert!(check_node(deepest, &fh, &fw).is_err(), "no inner node at depth 64");
        let empty = WireNode::Inner(Box::new([[0u8; 32]; 16])).serialize();
        assert!(check_node(NodeId::ROOT, &[0; 32], &empty).is_err(), "no empty inner node");
    }

    #[test]
    fn a_whole_tree_fetches_through_peers_that_answer_as_rippled() {
        let l = leaves(6000);
        let (tree, root) = state_tree(&l);
        let mut f = StateTreeFetch::new(root);
        let mut peers = [Peer::new(&tree), Peer::new(&tree)];
        let (got, requests) = run(&mut f, &mut peers, root);
        assert!(f.is_complete());
        assert_eq!(f.counts.nodes() as usize, tree.len(), "every node, once");
        assert_eq!(f.counts.leaves as usize, l.len());
        assert_eq!((f.counts.rejected, f.counts.misses), (0, 0));
        assert_eq!(sorted(got), sorted(l), "the leaves are the state's objects");
        assert!(requests <= 16, "{requests} requests for {} nodes", tree.len());
    }

    #[test]
    fn the_root_from_the_base_reply_counts_and_positions_then_align_on_fours() {
        let l = leaves(6000);
        let (tree, root) = state_tree(&l);
        let mut f = StateTreeFetch::new(root);
        let o = f.offer([(NodeId::ROOT, tree[&NodeId::ROOT].as_slice())], &mut |_, _| {});
        assert_eq!(o.accepted, 1);
        let b = f.next_batch().unwrap();
        assert_eq!(b.query_depth, 2, "depth-1 positions end their replies at depth 3");
        assert!(b.ids.iter().all(|p| p.depth == 1));
        let req = b.request(&root);
        assert_eq!(
            (req.itype, req.ledger_seq, req.query_type, req.request_cookie, req.query_depth),
            (TmLedgerInfoType::LiAsNode as i32, None, None, None, Some(2))
        );
        assert!(req.node_i_ds.iter().all(|r| r.len() == 33));
        let d = Peer::new(&tree).serve(&req, &root).unwrap();
        offer_reply(&mut f, &d, &mut Vec::new());
        f.settle(b.ticket, d.nodes.len());
        let mut f2 = f;
        while f2.available.first().is_some_and(|k| k.0 < 4) {
            let b = f2.next_batch().unwrap();
            let d = Peer::new(&tree).serve(&b.request(&root), &root).unwrap();
            offer_reply(&mut f2, &d, &mut Vec::new());
            f2.settle(b.ticket, d.nodes.len());
        }
        let b = f2.next_batch().unwrap();
        assert_eq!((b.ids[0].depth, b.query_depth), (4, 3), "the frontier sits at depth 4");
        assert!(b.ids.len() <= MAX_IDS);
    }

    #[test]
    fn batches_fit_what_a_reply_holds_and_full_replies_count_no_misses() {
        let l = leaves(6000);
        let (tree, root) = state_tree(&l);
        let mut f = StateTreeFetch::new(root);
        f.set_reply_cap(60);
        let mut p = Peer::new(&tree);
        (p.soft, p.hard) = (60, 90);
        let (got, requests) = run(&mut f, std::slice::from_mut(&mut p), root);
        assert!(requests > tree.len() / 90, "{requests} requests");
        assert!(f.is_complete(), "replies cut at the caps still finish the tree");
        assert_eq!(f.counts.misses, 0, "positions a full reply skipped are not misses");
        assert_eq!(sorted(got), sorted(l));
    }

    #[test]
    fn a_forged_node_is_refused_and_its_position_asked_again() {
        let l = leaves(500);
        let (tree, root) = state_tree(&l);
        let mut f = StateTreeFetch::new(root);
        let b = f.next_batch().unwrap();
        let mut d = Peer::new(&tree).serve(&b.request(&root), &root).unwrap();
        let i = d.nodes.iter().position(|n| *n.nodedata.last().unwrap() == WIRE_ACCOUNT_STATE).unwrap();
        d.nodes[i].nodedata[0] ^= 1;
        let forged = NodeId::from_raw(d.nodes[i].nodeid.as_deref().unwrap()).unwrap();
        let mut got = Vec::new();
        let o = offer_reply(&mut f, &d, &mut got);
        assert_eq!(o.rejected, 1);
        assert_eq!(f.counts.rejected, 1);
        f.settle(b.ticket, d.nodes.len());
        assert!(f.expected.contains_key(&forged), "the forged leaf's position is still wanted");
        assert!(got.iter().all(|(k, _)| position_of(k, forged.depth) != forged), "and nothing was written for it");
        let (rest, _) = run(&mut f, &mut [Peer::new(&tree)], root);
        got.extend(rest);
        assert!(f.is_complete());
        assert_eq!(sorted(got), sorted(l));
    }

    #[test]
    fn a_node_one_peer_lacks_comes_from_another() {
        let l = leaves(4000);
        let (tree, root) = state_tree(&l);
        // The deepest leaf: the peer that lacks it fails every position above it (rippled's descendThrow).
        let (lacked, _) = tree.iter().filter(|(_, d)| *d.last().unwrap() == WIRE_ACCOUNT_STATE).max_by_key(|(p, _)| p.depth).unwrap();
        assert!(lacked.depth > 8);
        let mut a = Peer::new(&tree);
        a.lacks.insert(*lacked);
        let mut f = StateTreeFetch::new(root);
        let (got, _) = run(&mut f, &mut [a, Peer::new(&tree)], root);
        assert!(f.is_complete());
        assert_eq!(f.counts.rejected, 0);
        assert_eq!(sorted(got), sorted(l));
    }

    #[test]
    fn a_position_replies_keep_leaving_unanswered_is_asked_alone_at_depth_zero() {
        let l = leaves(6000);
        let (tree, root) = state_tree(&l);
        let mut f = StateTreeFetch::new(root);
        f.offer([(NodeId::ROOT, tree[&NodeId::ROOT].as_slice())], &mut |_, _| {});
        let b = f.next_batch().unwrap();
        assert_eq!((b.ids.len(), b.query_depth), (16, 2), "the root's 16 children in one request");
        let x = b.ids[0];
        let mut lacking = Peer::new(&tree);
        lacking.lacks.insert(x);
        let d = lacking.serve(&b.request(&root), &root).unwrap();
        offer_reply(&mut f, &d, &mut Vec::new());
        f.settle(b.ticket, d.nodes.len());
        assert_eq!(f.misses[&x], 1, "a reply with room left it out");
        for _ in 1..STUBBORN {
            let b = f.next_batch().unwrap();
            assert_eq!((b.query_depth, b.ids.as_slice()), (2, [x].as_slice()), "still the shallowest");
            f.settle(b.ticket, 10);
        }
        assert_eq!((f.misses[&x], f.stubborn()), (STUBBORN, 1));
        let b = f.next_batch().unwrap();
        assert_eq!((b.query_depth, b.ids.as_slice()), (0, [x].as_slice()), "asked on its own, depth 0");
        let d = Peer::new(&tree).serve(&b.request(&root), &root).unwrap();
        offer_reply(&mut f, &d, &mut Vec::new());
        f.settle(b.ticket, d.nodes.len());
        assert!(!f.expected.contains_key(&x));
        assert_eq!(f.stubborn(), 0, "its children start afresh");
        let next = f.next_batch().unwrap();
        assert_eq!((next.ids[0].depth, next.query_depth), (2, 1), "in normal batches");
        f.release(next.ticket);
        run(&mut f, &mut [Peer::new(&tree)], root);
        assert!(f.is_complete());
        assert_eq!(f.counts.leaves as usize, l.len());
    }

    #[test]
    fn a_reply_offered_twice_writes_each_leaf_once() {
        let l = leaves(300);
        let (tree, root) = state_tree(&l);
        let mut f = StateTreeFetch::new(root);
        let b = f.next_batch().unwrap();
        let d = Peer::new(&tree).serve(&b.request(&root), &root).unwrap();
        let mut got = Vec::new();
        let first = offer_reply(&mut f, &d, &mut got);
        assert_eq!(first.accepted, d.nodes.len());
        let again = offer_reply(&mut f, &d, &mut got);
        assert_eq!((again.accepted, again.unasked), (0, d.nodes.len()));
        f.settle(b.ticket, d.nodes.len());
        f.settle(b.ticket, d.nodes.len()); // a ticket settles once
        assert_eq!(f.counts.misses, 0);
        let (rest, _) = run(&mut f, &mut [Peer::new(&tree)], root);
        got.extend(rest);
        assert!(f.is_complete());
        assert_eq!(sorted(got), sorted(l));
    }

    #[test]
    fn released_positions_go_out_again() {
        let l = leaves(6000);
        let (tree, root) = state_tree(&l);
        let mut f = StateTreeFetch::new(root);
        f.offer([(NodeId::ROOT, tree[&NodeId::ROOT].as_slice())], &mut |_, _| {});
        let a = f.next_batch().unwrap();
        let n = f.in_flight();
        assert_eq!(n, a.ids.len());
        f.release(a.ticket);
        assert_eq!((f.in_flight(), f.counts.misses), (0, 0));
        let b = f.next_batch().unwrap();
        assert_eq!(b.ids, a.ids, "the same positions, shallowest first");
        assert_ne!(b.ticket, a.ticket);
    }

    #[test]
    fn a_checkpoint_resumes_to_the_same_state() {
        let dir = tempfile::tempdir().unwrap();
        let (lp, cp) = (dir.path().join("leaves.bin"), dir.path().join("checkpoint.bin"));
        let l = leaves(6000);
        let (tree, root) = state_tree(&l);
        let header = [7u8; HEADER_SIZE];
        let mut f = StateTreeFetch::new(root);
        f.set_reply_cap(300);
        let mut file = LeafFile::open(&lp, 0).unwrap();
        let mut p = Peer::new(&tree);
        (p.soft, p.hard) = (300, 400);
        for _ in 0..4 {
            let b = f.next_batch().unwrap();
            let d = p.serve(&b.request(&root), &root).unwrap();
            let nodes = d.nodes.iter().filter_map(|n| Some((NodeId::from_raw(n.nodeid.as_deref()?)?, n.nodedata.as_slice())));
            f.offer(nodes, &mut |k, item| file.append(k, item).unwrap());
            f.settle(b.ticket, d.nodes.len());
        }
        let len = file.sync().unwrap();
        Checkpoint { header, leaves_len: len, counts: f.counts, frontier: f.frontier() }.save(&cp).unwrap();
        // After the checkpoint: one more reply written, then a crash.
        let b = f.next_batch().unwrap();
        let d = p.serve(&b.request(&root), &root).unwrap();
        let nodes = d.nodes.iter().filter_map(|n| Some((NodeId::from_raw(n.nodeid.as_deref()?)?, n.nodedata.as_slice())));
        f.offer(nodes, &mut |k, item| file.append(k, item).unwrap());
        file.sync().unwrap();
        drop(file);

        let c = Checkpoint::load(&cp).unwrap().unwrap();
        assert_eq!((c.header, c.leaves_len), (header, len));
        let mut f = StateTreeFetch::resume(root, &c.frontier, c.counts);
        let mut file = LeafFile::open(&lp, c.leaves_len).unwrap();
        let mut p = Peer::new(&tree);
        while let Some(b) = f.next_batch() {
            let d = p.serve(&b.request(&root), &root).unwrap();
            let nodes = d.nodes.iter().filter_map(|n| Some((NodeId::from_raw(n.nodeid.as_deref()?)?, n.nodedata.as_slice())));
            f.offer(nodes, &mut |k, item| file.append(k, item).unwrap());
            f.settle(b.ticket, d.nodes.len());
        }
        file.sync().unwrap();
        assert!(f.is_complete());
        assert_eq!(f.counts.leaves as usize, l.len());
        let r = recheck(&lp, f.counts.leaves).unwrap();
        assert_eq!(r, Recheck { leaves: l.len() as u64, repeated: 0, root });
        let mut back = Vec::new();
        read_leaves(&lp, |k, item| back.push((*k, item.to_vec()))).unwrap();
        assert_eq!(sorted(back), sorted(l));
    }

    #[test]
    fn a_checkpoint_that_is_cut_short_does_not_parse() {
        let c = Checkpoint {
            header: [1; HEADER_SIZE],
            leaves_len: 99,
            counts: Counts { inner: 1, leaves: 2, leaf_bytes: 3, rejected: 4, unasked: 5, misses: 6 },
            frontier: vec![(NodeId::ROOT.child(3).child(4), [8; 32]), (NodeId::ROOT.child(15), [9; 32])],
        };
        let b = c.to_bytes();
        assert_eq!(Checkpoint::from_bytes(&b), Some(c));
        assert_eq!(Checkpoint::from_bytes(&b[..b.len() - 1]), None);
        let mut x = b.clone();
        x.push(0);
        assert_eq!(Checkpoint::from_bytes(&x), None);
    }

    #[test]
    fn the_recheck_sees_a_missing_or_repeated_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("leaves.bin");
        let l = leaves(500);
        let (_, root) = state_tree(&l);
        let mut file = LeafFile::open(&p, 0).unwrap();
        for (k, item) in &l[1..] {
            file.append(k, item).unwrap();
        }
        file.append(&l[5].0, &l[5].1).unwrap();
        file.sync().unwrap();
        let r = recheck(&p, 0).unwrap();
        assert_eq!((r.leaves, r.repeated), (l.len() as u64, 1));
        assert_ne!(r.root, root, "one object short");
        assert!(LeafFile::open(&p, 1 << 40).is_err(), "a checkpoint past the file's end");
    }

    #[test]
    fn the_base_reply_proves_its_header_and_carries_the_state_root() {
        let header = LedgerHeader {
            sequence: 99_000_123,
            total_coins: 99_985_000_000_000_000,
            parent_hash: Hash256([1; 32]),
            transaction_hash: Hash256([2; 32]),
            account_hash: Hash256([3; 32]),
            parent_close_time: 800_000_000,
            close_time: 800_000_003,
            close_time_resolution: 10,
            close_flags: 0,
        };
        let hash = header.hash().0;
        let node = |d: Vec<u8>| TmLedgerNode { nodedata: d, nodeid: None };
        let d = TmLedgerData {
            ledger_hash: hash.to_vec(),
            ledger_seq: header.sequence,
            r#type: 0,
            nodes: vec![node(header.serialize().to_vec()), node(vec![0xAA; 40]), node(vec![0xBB; 40])],
            request_cookie: None,
            error: None,
        };
        let (h, root) = prove_base(&hash, &d).unwrap();
        assert_eq!((h.sequence, root), (99_000_123, Some(vec![0xAA; 40])));
        assert!(prove_base(&[9; 32], &d).is_err(), "another ledger's reply");
        let mut forged = d.clone();
        forged.nodes[0].nodedata[20] ^= 1;
        assert!(prove_base(&hash, &forged).is_err());
        let mut err = d.clone();
        err.error = Some(1);
        assert!(prove_base(&hash, &err).is_err());
        let req = base_request(&hash);
        assert_eq!((req.itype, req.query_depth, req.node_i_ds.len()), (0, None, 0));
    }

    #[test]
    fn query_depths_end_replies_on_a_level_three_below_a_multiple_of_four() {
        assert_eq!([0u8, 1, 2, 3, 4, 5, 8, 63].map(query_depth_for), [3, 2, 1, 0, 3, 2, 3, 0]);
    }

    #[test]
    fn only_public_ipv4_endpoints_are_dialed() {
        assert_eq!(dialable("34.205.233.231:51235").as_deref(), Some("34.205.233.231:51235"));
        assert_eq!(dialable("[::ffff:34.205.233.231]:51235").as_deref(), Some("34.205.233.231:51235"));
        for e in ["10.0.0.39:51235", "192.168.1.2:51235", "172.16.0.1:51235", "127.0.0.1:51235", "100.64.1.1:51235",
            "0.1.2.3:51235", "[::]:51235", "[2001:db8::1]:51235", "34.205.233.231:0", "s1.ripple.com:51235", "junk"]
        {
            assert_eq!(dialable(e), None, "{e}");
        }
    }
}
