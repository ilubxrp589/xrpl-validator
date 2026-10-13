//! The SHAMap wire format peers exchange in `TMLedgerData` (independence task 4.1), and a ledger's
//! transaction tree assembled from fetched nodes, each one checked against the hash its parent names.
//!
//! rippled 3.4.1:
//! - a node ID is `SHAMapNodeID::getRawString`: the 32-byte id (the key's leading nibbles to `depth`, the
//!   rest zero) and the depth, 33 bytes (src/libxrpl/shamap/SHAMapNodeID.cpp);
//! - a node is `SHAMapTreeNode::serializeForWire`, its last byte the wire type:
//!   - inner (2): the 16 child hashes;
//!   - compressed inner (3): `hash || branch` for each non-empty branch (fewer than 12 of them);
//!   - transaction with metadata (4): `VL(tx) || VL(meta) || key`;
//!   - account state (1): `data || key`;
//!   - transaction without metadata (0): the transaction.
//!
//! A ledger's transaction tree has only transaction-with-metadata leaves. [`TxTreeFetch`] starts from the
//! header's transaction root and accepts a node only at a position some accepted parent named, and only
//! when its hash is the one the parent holds there. So everything it returns is proven, back to the
//! header (and so to the ledger hash).

use std::collections::{BTreeMap, HashMap};

use super::hash::{
    sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_TRANSACTION_ID,
    HASH_PREFIX_TX_NODE,
};
use super::tx_tree::tx_id;
use xrpl_core::types::Hash256;

pub const WIRE_TRANSACTION: u8 = 0;
pub const WIRE_ACCOUNT_STATE: u8 = 1;
pub const WIRE_INNER: u8 = 2;
pub const WIRE_COMPRESSED_INNER: u8 = 3;
pub const WIRE_TRANSACTION_WITH_META: u8 = 4;

/// The deepest a SHAMap node can sit (64 nibbles).
pub const LEAF_DEPTH: u8 = 64;

/// A node's position: rippled's `SHAMapNodeID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId {
    pub id: [u8; 32],
    pub depth: u8,
}

impl NodeId {
    pub const ROOT: NodeId = NodeId { id: [0; 32], depth: 0 };

    /// The child at `branch` (`SHAMapNodeID::getChildNodeID`): the branch becomes nibble `depth`.
    pub fn child(&self, branch: u8) -> NodeId {
        let mut id = self.id;
        let d = self.depth as usize;
        let b = branch & 0x0f;
        if d & 1 == 0 {
            id[d / 2] |= b << 4;
        } else {
            id[d / 2] |= b;
        }
        NodeId { id, depth: self.depth + 1 }
    }

    /// `getRawString`: the id, then the depth.
    pub fn to_raw(&self) -> Vec<u8> {
        let mut v = self.id.to_vec();
        v.push(self.depth);
        v
    }

    /// `deserializeSHAMapNodeID`: 33 bytes, a depth of at most 64, and no bits set below the depth.
    pub fn from_raw(b: &[u8]) -> Option<NodeId> {
        if b.len() != 33 || b[32] > LEAF_DEPTH {
            return None;
        }
        let (id, depth): ([u8; 32], u8) = (b[..32].try_into().ok()?, b[32]);
        let n = NodeId { id, depth };
        (n.masked() == n).then_some(n)
    }

    fn masked(&self) -> NodeId {
        let mut id = self.id;
        let d = self.depth as usize;
        for (i, byte) in id.iter_mut().enumerate() {
            if 2 * i + 2 <= d {
                continue;
            }
            *byte = if 2 * i + 1 == d { *byte & 0xf0 } else { 0 };
        }
        NodeId { id, depth: self.depth }
    }
}

/// One node as a peer sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireNode {
    Inner(Box<[[u8; 32]; 16]>),
    TxWithMeta { item: Vec<u8>, key: [u8; 32] },
    AccountState { item: Vec<u8>, key: [u8; 32] },
    Transaction { item: Vec<u8> },
}

impl WireNode {
    /// Parse `serializeForWire`'s output. None when it is malformed.
    pub fn parse(data: &[u8]) -> Option<WireNode> {
        let (&kind, body) = data.split_last()?;
        let keyed = |body: &[u8]| -> Option<(Vec<u8>, [u8; 32])> {
            let at = body.len().checked_sub(32)?;
            Some((body[..at].to_vec(), body[at..].try_into().ok()?))
        };
        match kind {
            WIRE_INNER => {
                if body.len() != 16 * 32 {
                    return None;
                }
                let mut h = Box::new([[0u8; 32]; 16]);
                for (i, c) in body.chunks_exact(32).enumerate() {
                    h[i].copy_from_slice(c);
                }
                Some(WireNode::Inner(h))
            }
            WIRE_COMPRESSED_INNER => {
                if body.is_empty() || body.len() % 33 != 0 {
                    return None;
                }
                let mut h = Box::new([[0u8; 32]; 16]);
                for c in body.chunks_exact(33) {
                    let branch = c[32] as usize;
                    if branch >= 16 || h[branch] != [0; 32] {
                        return None;
                    }
                    h[branch].copy_from_slice(&c[..32]);
                }
                Some(WireNode::Inner(h))
            }
            WIRE_TRANSACTION_WITH_META => keyed(body).map(|(item, key)| WireNode::TxWithMeta { item, key }),
            WIRE_ACCOUNT_STATE => keyed(body).map(|(item, key)| WireNode::AccountState { item, key }),
            WIRE_TRANSACTION => Some(WireNode::Transaction { item: body.to_vec() }),
            _ => None,
        }
    }

    /// The node's hash, as its parent records it.
    pub fn hash(&self) -> Hash256 {
        let keyed = |prefix: &[u8; 4], item: &[u8], key: &[u8; 32]| {
            let mut d = Vec::with_capacity(item.len() + 32);
            d.extend_from_slice(item);
            d.extend_from_slice(key);
            sha512_half_prefixed(prefix, &d)
        };
        match self {
            WireNode::Inner(h) => sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, h.as_flattened()),
            WireNode::TxWithMeta { item, key } => keyed(&HASH_PREFIX_TX_NODE, item, key),
            WireNode::AccountState { item, key } => keyed(&HASH_PREFIX_LEAF_NODE, item, key),
            WireNode::Transaction { item } => sha512_half_prefixed(&HASH_PREFIX_TRANSACTION_ID, item),
        }
    }

    /// `serializeForWire` (compressed below 12 branches, as rippled sends it).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            WireNode::Inner(h) => {
                let n = h.iter().filter(|c| **c != [0; 32]).count();
                if n < 12 {
                    for (b, c) in h.iter().enumerate().filter(|(_, c)| **c != [0; 32]) {
                        out.extend_from_slice(c);
                        out.push(b as u8);
                    }
                    out.push(WIRE_COMPRESSED_INNER);
                } else {
                    out.extend_from_slice(h.as_flattened());
                    out.push(WIRE_INNER);
                }
            }
            WireNode::TxWithMeta { item, key } => {
                out.extend_from_slice(item);
                out.extend_from_slice(key);
                out.push(WIRE_TRANSACTION_WITH_META);
            }
            WireNode::AccountState { item, key } => {
                out.extend_from_slice(item);
                out.extend_from_slice(key);
                out.push(WIRE_ACCOUNT_STATE);
            }
            WireNode::Transaction { item } => {
                out.extend_from_slice(item);
                out.push(WIRE_TRANSACTION);
            }
        }
        out
    }
}

/// Read one VL length prefix (`Serializer::addEncoded`): the length and the bytes it took.
fn read_vl(b: &[u8]) -> Option<(usize, usize)> {
    let b0 = *b.first()? as usize;
    match b0 {
        0..=192 => Some((b0, 1)),
        193..=240 => Some((193 + ((b0 - 193) << 8) + *b.get(1)? as usize, 2)),
        241..=254 => Some((12481 + ((b0 - 241) << 16) + ((*b.get(1)? as usize) << 8) + *b.get(2)? as usize, 3)),
        _ => None,
    }
}

/// A transaction-with-metadata item, `VL(tx) || VL(meta)`, split into its two blobs.
pub fn split_tx_meta(item: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (tl, n) = read_vl(item)?;
    let tx = item.get(n..n + tl)?;
    let rest = &item[n + tl..];
    let (ml, m) = read_vl(rest)?;
    let meta = rest.get(m..m + ml)?;
    (m + ml == rest.len()).then(|| (tx.to_vec(), meta.to_vec()))
}

/// What offering a reply's nodes did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Offered {
    /// Proven and taken.
    pub accepted: usize,
    /// At a position no parent named (yet, or ever), or already taken: ignored.
    pub unasked: usize,
    /// Malformed, or not the hash the parent holds there: the peer sent something wrong.
    pub rejected: usize,
}

/// A ledger's transaction tree, assembled from fetched nodes.
#[derive(Debug)]
pub struct TxTreeFetch {
    /// Positions a parent named, with the hash it holds there, not received yet.
    expected: BTreeMap<NodeId, [u8; 32]>,
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    nodes: usize,
}

impl TxTreeFetch {
    /// Start from the header's transaction root. A zero root is an empty tree: already complete.
    pub fn new(root: Hash256) -> Self {
        let mut expected = BTreeMap::new();
        if root.0 != [0; 32] {
            expected.insert(NodeId::ROOT, root.0);
        }
        TxTreeFetch { expected, pairs: Vec::new(), nodes: 0 }
    }

    /// Offer one node at position `id`.
    fn accept(&mut self, id: NodeId, data: &[u8]) -> Result<bool, String> {
        let Some(want) = self.expected.get(&id).copied() else { return Ok(false) };
        let node = WireNode::parse(data).ok_or_else(|| format!("node at depth {} is malformed", id.depth))?;
        if node.hash().0 != want {
            return Err(format!("node at depth {} is not the one its parent names", id.depth));
        }
        match node {
            WireNode::Inner(h) => {
                if id.depth >= LEAF_DEPTH {
                    return Err("an inner node below the deepest level".into());
                }
                for (b, c) in h.iter().enumerate() {
                    if *c != [0; 32] {
                        self.expected.insert(id.child(b as u8), *c);
                    }
                }
            }
            WireNode::TxWithMeta { item, key } => {
                let (tx, meta) = split_tx_meta(&item).ok_or("a leaf's transaction and metadata do not parse")?;
                if tx_id(&tx).0 != key {
                    return Err("a leaf's key is not its transaction's id".into());
                }
                self.pairs.push((tx, meta));
            }
            _ => return Err("a transaction tree holds only transaction-with-metadata leaves".into()),
        }
        self.expected.remove(&id);
        self.nodes += 1;
        Ok(true)
    }

    /// Offer a reply's nodes, in any order: each is taken once its parent has been.
    pub fn offer(&mut self, nodes: impl IntoIterator<Item = (NodeId, Vec<u8>)>) -> Offered {
        let mut out = Offered::default();
        let mut pending: HashMap<NodeId, Vec<u8>> = HashMap::new();
        for (id, data) in nodes {
            pending.insert(id, data);
        }
        loop {
            let ready: Vec<NodeId> = pending.keys().filter(|id| self.expected.contains_key(id)).copied().collect();
            if ready.is_empty() {
                break;
            }
            for id in ready {
                let data = pending.remove(&id).unwrap_or_default();
                match self.accept(id, &data) {
                    Ok(true) => out.accepted += 1,
                    Ok(false) => out.unasked += 1,
                    Err(_) => out.rejected += 1,
                }
            }
        }
        out.unasked += pending.len();
        out
    }

    /// The positions still to ask for.
    pub fn wanted(&self) -> Vec<NodeId> {
        self.expected.keys().copied().collect()
    }

    pub fn is_complete(&self) -> bool {
        self.expected.is_empty()
    }

    /// Nodes taken so far.
    pub fn nodes(&self) -> usize {
        self.nodes
    }

    /// The ledger's transactions and their metadata (in the order they arrived).
    pub fn into_pairs(self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.pairs
    }
}

/// Every node of the transaction tree of `pairs`, at its position, in wire form: what a peer would send
/// for the whole tree (tests, and a server that wants to answer liTX_NODE).
pub fn tx_tree_wire_nodes(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<(NodeId, Vec<u8>)> {
    let leaves: Vec<([u8; 32], WireNode)> = pairs
        .iter()
        .map(|(tx, meta)| {
            let mut item = super::tx_tree::vl_prefix(tx.len());
            item.extend_from_slice(tx);
            item.extend_from_slice(&super::tx_tree::vl_prefix(meta.len()));
            item.extend_from_slice(meta);
            let key = tx_id(tx).0;
            (key, WireNode::TxWithMeta { item, key })
        })
        .collect();
    let mut out = Vec::new();
    if !leaves.is_empty() {
        let refs: Vec<&([u8; 32], WireNode)> = leaves.iter().collect();
        build(&refs, NodeId::ROOT, &mut out);
    }
    out
}

fn build(items: &[&([u8; 32], WireNode)], at: NodeId, out: &mut Vec<(NodeId, Vec<u8>)>) -> [u8; 32] {
    if items.len() == 1 && at.depth > 0 {
        out.push((at, items[0].1.serialize()));
        return items[0].1.hash().0;
    }
    let mut buckets: Vec<Vec<&([u8; 32], WireNode)>> = vec![Vec::new(); 16];
    for it in items {
        buckets[super::tx_tree::nibble(&it.0, at.depth as usize)].push(*it);
    }
    let mut h = Box::new([[0u8; 32]; 16]);
    let slot = out.len();
    out.push((at, Vec::new()));
    for (b, bucket) in buckets.iter().enumerate() {
        if !bucket.is_empty() {
            h[b] = build(bucket, at.child(b as u8), out);
        }
    }
    let node = WireNode::Inner(h);
    out[slot].1 = node.serialize();
    node.hash().0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shamap::tx_tree::compute_tx_tree_root;

    /// Pairs whose tx blobs are distinct (the tree only hashes them; the codec never sees them).
    fn pairs(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n)
            .map(|i| (format!("tx-{i}").repeat(1 + i % 7).into_bytes(), vec![0xEE; 200 + i % 300]))
            .collect()
    }

    #[test]
    fn node_ids_round_trip_as_rippled_spells_them() {
        let c = NodeId::ROOT.child(0xA).child(0x3).child(0xF);
        assert_eq!(c.depth, 3);
        assert_eq!(&c.id[..2], &[0xA3, 0xF0]);
        let raw = c.to_raw();
        assert_eq!(raw.len(), 33);
        assert_eq!(raw[32], 3);
        assert_eq!(NodeId::from_raw(&raw), Some(c));
        let mut bad = raw.clone();
        bad[1] |= 0x01; // a bit below the depth
        assert_eq!(NodeId::from_raw(&bad), None);
        bad = raw.clone();
        bad[32] = 65;
        assert_eq!(NodeId::from_raw(&bad), None);
        assert_eq!(NodeId::from_raw(&raw[..32]), None);
    }

    #[test]
    fn wire_nodes_round_trip_and_hash_as_the_tree_does() {
        let p = pairs(40);
        let nodes = tx_tree_wire_nodes(&p);
        let root = WireNode::parse(&nodes[0].1).unwrap();
        assert_eq!(nodes[0].0, NodeId::ROOT);
        assert_eq!(root.hash(), compute_tx_tree_root(&p), "the wire root hashes to the tree root");
        for (_, d) in &nodes {
            assert_eq!(WireNode::parse(d).unwrap().serialize(), *d);
        }
        assert!(nodes.iter().any(|(_, d)| *d.last().unwrap() == WIRE_COMPRESSED_INNER));
    }

    #[test]
    fn a_full_inner_node_is_sent_uncompressed() {
        let full = WireNode::Inner(Box::new([[7u8; 32]; 16]));
        let w = full.serialize();
        assert_eq!((w.len(), *w.last().unwrap()), (16 * 32 + 1, WIRE_INNER));
        assert_eq!(WireNode::parse(&w), Some(full));
    }

    #[test]
    fn vl_items_split_back_into_their_blobs() {
        for len in [0usize, 1, 192, 193, 500, 12480, 12481, 20000] {
            let (tx, meta) = (vec![1u8; len], vec![2u8; 3]);
            let mut item = crate::shamap::tx_tree::vl_prefix(tx.len());
            item.extend_from_slice(&tx);
            item.extend_from_slice(&crate::shamap::tx_tree::vl_prefix(meta.len()));
            item.extend_from_slice(&meta);
            assert_eq!(split_tx_meta(&item), Some((tx, meta)), "len {len}");
        }
        assert_eq!(split_tx_meta(&[5, 1, 2]), None, "short");
    }

    #[test]
    fn the_whole_tree_in_any_order_assembles_to_the_ledgers_transactions() {
        let p = pairs(300);
        let mut nodes = tx_tree_wire_nodes(&p);
        nodes.reverse(); // children before parents
        let mut f = TxTreeFetch::new(compute_tx_tree_root(&p));
        let o = f.offer(nodes.clone());
        assert_eq!(o, Offered { accepted: nodes.len(), unasked: 0, rejected: 0 });
        assert!(f.is_complete());
        let mut got = f.into_pairs();
        let mut want = p.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn a_partial_reply_names_what_is_still_wanted() {
        let p = pairs(300);
        let nodes = tx_tree_wire_nodes(&p);
        let mut f = TxTreeFetch::new(compute_tx_tree_root(&p));
        // The root and its direct children only (a depth-1 reply).
        let first: Vec<(NodeId, Vec<u8>)> = nodes.iter().filter(|(id, _)| id.depth <= 1).cloned().collect();
        f.offer(first);
        let wanted = f.wanted();
        assert!(!wanted.is_empty() && wanted.iter().all(|id| id.depth == 2));
        f.offer(nodes.iter().filter(|(id, _)| id.depth >= 2).cloned());
        assert!(f.is_complete());
    }

    #[test]
    fn a_node_that_is_not_the_one_its_parent_names_is_rejected() {
        let p = pairs(50);
        let mut nodes = tx_tree_wire_nodes(&p);
        let i = nodes.iter().position(|(_, d)| *d.last().unwrap() == WIRE_TRANSACTION_WITH_META).unwrap();
        let n = nodes[i].1.len();
        nodes[i].1[n - 40] ^= 1; // one bit of one transaction's metadata
        let mut f = TxTreeFetch::new(compute_tx_tree_root(&p));
        let o = f.offer(nodes);
        assert_eq!(o.rejected, 1);
        assert!(!f.is_complete(), "the forged leaf's position is still wanted");
        assert_eq!(f.wanted().len(), 1);
    }

    #[test]
    fn nodes_nobody_asked_for_are_ignored() {
        let p = pairs(20);
        let other = pairs(25);
        let mut f = TxTreeFetch::new(compute_tx_tree_root(&p));
        let o = f.offer(tx_tree_wire_nodes(&other));
        assert_eq!(o.accepted, 0, "another tree's root is not this one's");
        assert!(o.rejected + o.unasked > 0);
        assert!(TxTreeFetch::new(Hash256([0; 32])).is_complete(), "an empty tree needs nothing");
    }
}
