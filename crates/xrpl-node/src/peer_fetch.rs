//! Independence task 4.1: a ledger fetched from peers.
//!
//! Today every ledger's transactions, their records and its header come from the stock `xrpld` on .39.
//! [`LedgerFetch`] asks a peer instead, with rippled's own requests (`TMGetLedger`, PeerImp.cpp in
//! 3.4.1):
//! 1. `liBASE` for the ledger hash: the reply's first node is the 118-byte header, which must hash to that
//!    ledger hash. A ledger with transactions also sends its transaction tree's root.
//! 2. `liTX_NODE` for the positions still missing, three levels at a time (`kMaxQueryDepth`), until the tree
//!    is complete. Every node must be the one its parent names ([`TxTreeFetch`]).
//!
//! What comes out is proven back to the ledger hash. The check compares it with .39's copy of the same
//! ledger ([`compare`]). Requests carry no cookie: each costs us rippled's `kFeeModerateBurdenPeer` (250)
//! at the peer, which warns at 5,000 and disconnects at 25,000, decaying over 32 s. The driver paces
//! itself well under that.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::shamap::wire::{NodeId, Offered, TxTreeFetch};

use crate::peer::protocol::{TmGetLedger, TmLedgerData, TmLedgerInfoType};

/// rippled's `kMaxQueryDepth`: the deepest a liTX_NODE reply goes below each position asked for.
pub const QUERY_DEPTH: u32 = 3;
/// Positions per liTX_NODE request (rippled answers up to 8,192 nodes in one reply).
pub const MAX_IDS: usize = 512;

/// One ledger being fetched.
#[derive(Debug)]
pub struct LedgerFetch {
    pub hash: [u8; 32],
    header: Option<LedgerHeader>,
    tree: Option<TxTreeFetch>,
    /// Requests made for it.
    pub requests: u32,
    /// Nodes refused (malformed, or not what their parent names).
    pub rejected: usize,
}

/// What a reply did for a fetch.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Not for this ledger (another hash, or a kind it didn't ask for).
    Ignored,
    /// Nodes taken; more to fetch.
    Progress(Offered),
    /// The header and the whole transaction tree are in.
    Done,
    /// The peer's header does not hash to the ledger hash, or would not parse.
    BadHeader(String),
    /// The peer answered with an error.
    PeerError(i32),
}

impl LedgerFetch {
    pub fn new(hash: [u8; 32]) -> Self {
        LedgerFetch { hash, header: None, tree: None, requests: 0, rejected: 0 }
    }

    /// The first request: the header, and the tree's root.
    pub fn base_request(&mut self) -> TmGetLedger {
        self.requests += 1;
        TmGetLedger {
            itype: TmLedgerInfoType::LiBase as i32,
            ltype: None,
            ledger_hash: Some(self.hash.to_vec()),
            ledger_seq: None,
            node_i_ds: Vec::new(),
            request_cookie: None,
            query_type: None,
            query_depth: None,
        }
    }

    /// The next request, while the header is in and the tree is not.
    pub fn next_request(&mut self) -> Option<TmGetLedger> {
        let tree = self.tree.as_ref()?;
        if tree.is_complete() {
            return None;
        }
        let ids: Vec<Vec<u8>> = tree.wanted().iter().take(MAX_IDS).map(NodeId::to_raw).collect();
        self.requests += 1;
        Some(TmGetLedger {
            itype: TmLedgerInfoType::LiTxNode as i32,
            ltype: None,
            ledger_hash: Some(self.hash.to_vec()),
            ledger_seq: None,
            node_i_ds: ids,
            request_cookie: None,
            query_type: None,
            query_depth: Some(QUERY_DEPTH),
        })
    }

    /// Take a peer's reply.
    pub fn on_data(&mut self, d: &TmLedgerData) -> Step {
        if d.ledger_hash != self.hash {
            return Step::Ignored;
        }
        if let Some(e) = d.error {
            return Step::PeerError(e);
        }
        let offered = if d.r#type == TmLedgerInfoType::LiBase as i32 {
            if self.header.is_some() {
                return Step::Ignored;
            }
            let Some(first) = d.nodes.first() else { return Step::BadHeader("no header node".into()) };
            let header = match LedgerHeader::deserialize(&first.nodedata) {
                Ok(h) => h,
                Err(e) => return Step::BadHeader(e.to_string()),
            };
            if first.nodedata.len() != xrpl_ledger::ledger::header::HEADER_SIZE || header.hash().0 != self.hash {
                return Step::BadHeader("the header does not hash to the ledger hash".into());
            }
            let mut tree = TxTreeFetch::new(header.transaction_hash);
            // sendLedgerBase: the header, then the state root when there is state, then the transaction
            // root when there are transactions too.
            let o = match d.nodes.get(2) {
                Some(root) if header.transaction_hash.0 != [0; 32] => tree.offer([(NodeId::ROOT, root.nodedata.clone())]),
                _ => Offered::default(),
            };
            self.header = Some(header);
            self.tree = Some(tree);
            o
        } else if d.r#type == TmLedgerInfoType::LiTxNode as i32 {
            let Some(tree) = self.tree.as_mut() else { return Step::Ignored };
            tree.offer(
                d.nodes
                    .iter()
                    .filter_map(|n| Some((NodeId::from_raw(n.nodeid.as_deref()?)?, n.nodedata.clone()))),
            )
        } else {
            return Step::Ignored;
        };
        self.rejected += offered.rejected;
        if self.is_done() {
            Step::Done
        } else {
            Step::Progress(offered)
        }
    }

    pub fn is_done(&self) -> bool {
        self.tree.as_ref().is_some_and(TxTreeFetch::is_complete)
    }

    pub fn header(&self) -> Option<&LedgerHeader> {
        self.header.as_ref()
    }

    /// The proven header and every transaction with its record, once done.
    pub fn into_result(self) -> Option<(LedgerHeader, Vec<(Vec<u8>, Vec<u8>)>)> {
        let (tree, header) = (self.tree?, self.header?);
        tree.is_complete().then(|| (header, tree.into_pairs()))
    }
}

/// Our fetched transactions against another copy of the same ledger's (each a transaction blob and its
/// metadata). None when they are the same set, byte for byte; else the difference, said in words.
pub fn compare(ours: &[(Vec<u8>, Vec<u8>)], theirs: &[(Vec<u8>, Vec<u8>)]) -> Option<String> {
    let id = |p: &(Vec<u8>, Vec<u8>)| xrpl_ledger::shamap::tx_tree::tx_id(&p.0).0;
    let a: std::collections::BTreeMap<[u8; 32], &(Vec<u8>, Vec<u8>)> = ours.iter().map(|p| (id(p), p)).collect();
    let b: std::collections::BTreeMap<[u8; 32], &(Vec<u8>, Vec<u8>)> = theirs.iter().map(|p| (id(p), p)).collect();
    let only_ours = a.keys().filter(|k| !b.contains_key(*k)).count();
    let only_theirs = b.keys().filter(|k| !a.contains_key(*k)).count();
    let differ: Vec<&[u8; 32]> = a.iter().filter(|(k, p)| b.get(*k).is_some_and(|q| q != *p)).map(|(k, _)| k).collect();
    if only_ours == 0 && only_theirs == 0 && differ.is_empty() && a.len() == ours.len() {
        return None;
    }
    Some(format!(
        "{} ours only, {} theirs only, {} with different bytes{}",
        only_ours,
        only_theirs,
        differ.len(),
        differ.first().map(|k| format!(" (first {})", &hex::encode_upper(k)[..16])).unwrap_or_default()
    ))
}

// ---- counters, the receipt log, /api/engine and /metrics ---------------------------------------------------

/// `XRPL_PEER_FETCH=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_PEER_FETCH").map(|v| v == "1").unwrap_or(false))
}

#[derive(Default)]
pub struct FetchStats {
    pub wanted: AtomicU64,
    pub fetched: AtomicU64,
    pub matched: AtomicU64,
    pub differ: AtomicU64,
    /// Gave up (no peer answered in time, or a bad header from every peer tried).
    pub failed: AtomicU64,
    pub requests: AtomicU64,
    pub rejected_nodes: AtomicU64,
    pub bad_headers: AtomicU64,
    /// Ledgers dropped from the queue to stay at the live edge.
    pub skipped: AtomicU64,
    /// Peers that can be asked now (a gauge).
    pub peers: AtomicU64,
    pub last_seq: AtomicU64,
    pub ms_last: AtomicU64,
}

pub fn stats() -> &'static FetchStats {
    static S: OnceLock<FetchStats> = OnceLock::new();
    S.get_or_init(FetchStats::default)
}

/// `XRPL_PEER_FETCH_LOG`, else /mnt/xrpl-data/peer_fetch.jsonl.
pub fn log_path() -> std::path::PathBuf {
    std::env::var_os("XRPL_PEER_FETCH_LOG").map(Into::into).unwrap_or_else(|| "/mnt/xrpl-data/peer_fetch.jsonl".into())
}

/// Append one receipt line, unless the file has passed 20 MB.
pub fn log_receipt(path: &std::path::Path, line: &Value) {
    if std::fs::metadata(path).map(|m| m.len() > 20 << 20).unwrap_or(false) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn summary() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    format!(
        "wanted {} fetched {} | match {} differ {} | failed {} skipped {} | requests {} rejected nodes {} bad headers {} | peers {}",
        l(&st.wanted), l(&st.fetched), l(&st.matched), l(&st.differ), l(&st.failed), l(&st.skipped), l(&st.requests),
        l(&st.rejected_nodes), l(&st.bad_headers), l(&st.peers)
    )
}

/// The `/api/engine` section.
pub fn json() -> Value {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    json!({
        "enabled": enabled(), "last_seq": l(&st.last_seq), "wanted": l(&st.wanted), "fetched": l(&st.fetched),
        "matched": l(&st.matched), "differ": l(&st.differ), "failed": l(&st.failed), "requests": l(&st.requests),
        "rejected_nodes": l(&st.rejected_nodes), "bad_headers": l(&st.bad_headers), "skipped": l(&st.skipped),
        "peers": l(&st.peers), "ms_last": l(&st.ms_last),
    })
}

/// Extra /metrics lines.
pub fn metrics_text() -> String {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut s = String::new();
    for (name, help, v) in [
        ("xrpl_peer_fetch_fetched_total", "Ledgers fetched from peers and proven to the ledger hash.", l(&st.fetched)),
        ("xrpl_peer_fetch_matched_total", "Fetched ledgers whose transactions and records equal .39's.", l(&st.matched)),
        ("xrpl_peer_fetch_differ_total", "Fetched ledgers whose transactions or records differ from .39's.", l(&st.differ)),
        ("xrpl_peer_fetch_failed_total", "Ledgers no peer delivered in time.", l(&st.failed)),
        ("xrpl_peer_fetch_requests_total", "TMGetLedger requests sent.", l(&st.requests)),
        ("xrpl_peer_fetch_rejected_nodes_total", "Nodes a peer sent that were not what their parent names.", l(&st.rejected_nodes)),
    ] {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
    }
    s
}

// ---- the live driver (XRPL_PEER_FETCH=1) -------------------------------------------------------------------

/// Gap between two requests to one peer. About three requests a ledger, spread over the peers, keep each
/// one's rippled resource balance near 250 × 32 s / 8 s = 1,000 at worst: a fifth of its warning level.
pub const PEER_GAP: Duration = Duration::from_secs(8);
/// A request no reply answered in this long goes to another peer.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(6);
/// A ledger still incomplete after this long is given up.
pub const LEDGER_DEADLINE: Duration = Duration::from_secs(45);
/// A peer that misses this many replies in a row rests for [`BENCH`].
pub const MISSES_TO_BENCH: u32 = 3;
pub const BENCH: Duration = Duration::from_secs(300);
/// Ledgers waiting beyond this are dropped, oldest first: the fetch stays at the live edge.
pub const QUEUE_CAP: usize = 4;

#[derive(Debug, Default, Clone)]
struct PeerState {
    last: Option<Instant>,
    misses: u32,
    benched_until: Option<Instant>,
}

/// Which peer gets the next request.
#[derive(Debug, Default)]
pub struct Pacer {
    peers: HashMap<String, PeerState>,
}

impl Pacer {
    pub fn add(&mut self, addr: &str) {
        self.peers.entry(addr.to_string()).or_default();
    }

    pub fn remove(&mut self, addr: &str) {
        self.peers.remove(addr);
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// The peer rested longest, not benched, not asked within [`PEER_GAP`], and not `avoid` while another
    /// one qualifies. Marks it asked now.
    pub fn pick(&mut self, now: Instant, avoid: Option<&str>) -> Option<String> {
        let ready = |st: &PeerState| {
            st.benched_until.is_none_or(|t| now >= t) && st.last.is_none_or(|t| now.duration_since(t) >= PEER_GAP)
        };
        let mut cands: Vec<(&String, &PeerState)> = self.peers.iter().filter(|(_, st)| ready(st)).collect();
        if cands.len() > 1 {
            cands.retain(|(a, _)| Some(a.as_str()) != avoid);
        }
        // Never asked first, then the longest rested; the address breaks ties so the choice is stable.
        let pick = cands.into_iter().min_by(|x, y| x.1.last.cmp(&y.1.last).then(x.0.cmp(y.0)))?.0.clone();
        if let Some(st) = self.peers.get_mut(&pick) {
            st.last = Some(now);
            st.benched_until = None;
        }
        Some(pick)
    }

    pub fn answered(&mut self, addr: &str) {
        if let Some(st) = self.peers.get_mut(addr) {
            st.misses = 0;
        }
    }

    /// A reply that never came, or one that was wrong (`strikes` at once).
    pub fn missed(&mut self, addr: &str, now: Instant, strikes: u32) {
        if let Some(st) = self.peers.get_mut(addr) {
            st.misses += strikes;
            if st.misses >= MISSES_TO_BENCH {
                st.misses = 0;
                st.benched_until = Some(now + BENCH);
            }
        }
    }
}

enum Event {
    Peer { addr: String, frames: Option<tokio::sync::mpsc::Sender<Vec<u8>>> },
    Data { addr: String, data: TmLedgerData },
    Want { seq: u32, hash: [u8; 32], theirs: Vec<(Vec<u8>, Vec<u8>)> },
}

static EVENTS: OnceLock<tokio::sync::mpsc::UnboundedSender<Event>> = OnceLock::new();

fn send(ev: Event) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(ev);
    }
}

/// Start the driver when `XRPL_PEER_FETCH=1` (inside the tokio runtime). True when it runs.
pub fn start() -> bool {
    if !enabled() || EVENTS.get().is_some() {
        return EVENTS.get().is_some();
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    if EVENTS.set(tx).is_err() {
        return true;
    }
    tokio::spawn(drive(rx, log_path()));
    eprintln!("[peer-fetch] ON — each validated ledger fetched from peers and compared with .39's copy; receipts -> {}", log_path().display());
    true
}

/// A peer connection that can be asked for ledgers: the frames to write to it. None while the driver is off.
pub fn register(addr: &str) -> Option<tokio::sync::mpsc::Receiver<Vec<u8>>> {
    EVENTS.get()?;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    send(Event::Peer { addr: addr.to_string(), frames: Some(tx) });
    Some(rx)
}

pub fn unregister(addr: &str) {
    send(Event::Peer { addr: addr.to_string(), frames: None });
}

/// A peer's mtLEDGER_DATA (read before the overlay's duplicate filter: two peers' replies are byte-identical).
pub fn on_ledger_data(addr: &str, d: &TmLedgerData) {
    if EVENTS.get().is_some() {
        send(Event::Data { addr: addr.to_string(), data: d.clone() });
    }
}

/// .39's copy of validated ledger `seq` (its hash, and every transaction with its metadata, already checked
/// against the header's transaction root): fetch the same ledger from peers and compare.
pub fn offer_network(seq: u32, hash: [u8; 32], theirs: &[(Vec<u8>, Vec<u8>)]) {
    if EVENTS.get().is_some() && hash != [0; 32] {
        stats().wanted.fetch_add(1, Ordering::Relaxed);
        send(Event::Want { seq, hash, theirs: theirs.to_vec() });
    }
}

/// mtGET_LEDGER on the wire: length, type, payload.
pub fn frame(req: &TmGetLedger) -> Vec<u8> {
    use prost::Message as _;
    let p = req.encode_to_vec();
    let mut f = Vec::with_capacity(6 + p.len());
    f.extend_from_slice(&(p.len() as u32).to_be_bytes());
    f.extend_from_slice(&31u16.to_be_bytes());
    f.extend_from_slice(&p);
    f
}

struct Active {
    seq: u32,
    fetch: LedgerFetch,
    theirs: Vec<(Vec<u8>, Vec<u8>)>,
    started: Instant,
    /// The peer last asked, and when; None when the next request is due.
    asked: Option<(String, Instant)>,
    peers_used: Vec<String>,
}

async fn drive(mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>, log: std::path::PathBuf) {
    let st = stats();
    let mut pacer = Pacer::default();
    let mut frames: HashMap<String, tokio::sync::mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut queue: VecDeque<(u32, [u8; 32], Vec<(Vec<u8>, Vec<u8>)>)> = VecDeque::new();
    let mut cur: Option<Active> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                None => return,
                Some(Event::Peer { addr, frames: Some(tx) }) => {
                    pacer.add(&addr);
                    frames.insert(addr, tx);
                }
                Some(Event::Peer { addr, frames: None }) => {
                    pacer.remove(&addr);
                    frames.remove(&addr);
                }
                Some(Event::Want { seq, hash, theirs }) => {
                    queue.push_back((seq, hash, theirs));
                    while queue.len() > QUEUE_CAP {
                        queue.pop_front();
                        st.skipped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Some(Event::Data { addr, data }) => {
                    if let Some(a) = cur.as_mut() {
                        let now = Instant::now();
                        match a.fetch.on_data(&data) {
                            Step::Ignored => {}
                            Step::Progress(_) | Step::Done => {
                                pacer.answered(&addr);
                                if a.asked.as_ref().is_some_and(|(p, _)| *p == addr) {
                                    a.asked = None;
                                }
                            }
                            Step::BadHeader(why) => {
                                st.bad_headers.fetch_add(1, Ordering::Relaxed);
                                pacer.missed(&addr, now, MISSES_TO_BENCH);
                                eprintln!("[peer-fetch] #{}: {addr} sent a header that is not the ledger's ({why})", a.seq);
                                a.asked = None;
                            }
                            Step::PeerError(_) => {
                                pacer.missed(&addr, now, 1);
                                a.asked = None;
                            }
                        }
                    }
                }
            },
            _ = tick.tick() => {}
        }
        st.peers.store(pacer.len() as u64, Ordering::Relaxed);
        let now = Instant::now();
        if cur.is_none() {
            if let Some((seq, hash, theirs)) = queue.pop_front() {
                cur = Some(Active { seq, fetch: LedgerFetch::new(hash), theirs, started: now, asked: None, peers_used: Vec::new() });
            }
        }
        let Some(a) = cur.as_mut() else { continue };
        if a.fetch.is_done() || now.duration_since(a.started) > LEDGER_DEADLINE {
            let a = cur.take().expect("current fetch");
            finish(a, &log);
            continue;
        }
        if let Some((peer, at)) = a.asked.clone() {
            if now.duration_since(at) < REPLY_TIMEOUT {
                continue;
            }
            pacer.missed(&peer, now, 1);
            a.asked = None;
        }
        let avoid = a.peers_used.last().cloned();
        let Some(peer) = pacer.pick(now, avoid.as_deref()) else { continue };
        let req = if a.fetch.header().is_none() { Some(a.fetch.base_request()) } else { a.fetch.next_request() };
        let Some(req) = req else { continue };
        match frames.get(&peer).map(|tx| tx.try_send(frame(&req))) {
            Some(Ok(())) => {
                st.requests.fetch_add(1, Ordering::Relaxed);
                a.asked = Some((peer.clone(), now));
                if !a.peers_used.contains(&peer) {
                    a.peers_used.push(peer);
                }
            }
            _ => pacer.missed(&peer, now, 1),
        }
    }
}

fn finish(a: Active, log: &std::path::Path) {
    let st = stats();
    let ms = a.started.elapsed().as_millis() as u64;
    st.ms_last.store(ms, Ordering::Relaxed);
    st.last_seq.store(u64::from(a.seq), Ordering::Relaxed);
    st.rejected_nodes.fetch_add(a.fetch.rejected as u64, Ordering::Relaxed);
    let (requests, peers) = (a.fetch.requests, a.peers_used.len());
    let hash = hex::encode_upper(a.fetch.hash);
    match a.fetch.into_result() {
        Some((header, ours)) => {
            let n = st.fetched.fetch_add(1, Ordering::Relaxed) + 1;
            let verdict = if header.sequence != a.seq {
                Some(format!("the proven header is #{}, not #{}", header.sequence, a.seq))
            } else {
                compare(&ours, &a.theirs)
            };
            match verdict {
                None => {
                    st.matched.fetch_add(1, Ordering::Relaxed);
                }
                Some(d) => {
                    st.differ.fetch_add(1, Ordering::Relaxed);
                    log_receipt(log, &json!({"seq": a.seq, "hash": hash, "differ": d, "requests": requests, "peers": peers, "ms": ms}));
                }
            }
            if n % 256 == 1 {
                eprintln!("[peer-fetch] heartbeat #{}: {}", a.seq, summary());
            }
        }
        None => {
            st.failed.fetch_add(1, Ordering::Relaxed);
            log_receipt(log, &json!({"seq": a.seq, "hash": hash, "failed": "not complete within the deadline", "requests": requests, "peers": peers, "ms": ms}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::protocol::TmLedgerNode;
    use xrpl_core::types::Hash256;
    use xrpl_ledger::shamap::tx_tree::compute_tx_tree_root;
    use xrpl_ledger::shamap::wire::tx_tree_wire_nodes;

    fn pairs(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n).map(|i| (format!("tx {i}").into_bytes(), vec![(i % 251) as u8; 100 + i % 50])).collect()
    }

    fn header_for(p: &[(Vec<u8>, Vec<u8>)]) -> LedgerHeader {
        LedgerHeader {
            sequence: 107_000_000,
            total_coins: 99_985_000_000_000_000,
            parent_hash: Hash256([1; 32]),
            transaction_hash: compute_tx_tree_root(p),
            account_hash: Hash256([2; 32]),
            parent_close_time: 800_000_000,
            close_time: 800_000_003,
            close_time_resolution: 10,
            close_flags: 0,
        }
    }

    fn node(data: Vec<u8>, id: Option<NodeId>) -> TmLedgerNode {
        TmLedgerNode { nodedata: data, nodeid: id.map(|i| i.to_raw()) }
    }

    /// What a peer sends for liBASE (header, a state root, the transaction root) and for one liTX_NODE
    /// request at `depth` (every node from the positions asked for down to `depth` more levels).
    struct Peer {
        header: LedgerHeader,
        wire: Vec<(NodeId, Vec<u8>)>,
    }

    impl Peer {
        fn base(&self) -> TmLedgerData {
            let mut nodes = vec![node(self.header.serialize().to_vec(), None), node(vec![0xAA; 40], None)];
            if let Some((_, root)) = self.wire.first() {
                nodes.push(node(root.clone(), None));
            }
            TmLedgerData { ledger_hash: self.header.hash().0.to_vec(), ledger_seq: self.header.sequence, r#type: 0, nodes, request_cookie: None, error: None }
        }

        fn tx_nodes(&self, req: &TmGetLedger) -> TmLedgerData {
            let asked: Vec<NodeId> = req.node_i_ds.iter().filter_map(|r| NodeId::from_raw(r)).collect();
            let depth = req.query_depth.unwrap_or(1) as u8;
            let under = |a: &NodeId, n: &NodeId| {
                n.depth >= a.depth && n.depth <= a.depth + depth && {
                    let mut m = *n;
                    while m.depth > a.depth {
                        let d = (m.depth - 1) as usize;
                        if d & 1 == 0 { m.id[d / 2] &= 0x0f } else { m.id[d / 2] &= 0xf0 }
                        m.depth -= 1;
                    }
                    m == *a
                }
            };
            let nodes = self.wire.iter().filter(|(n, _)| asked.iter().any(|a| under(a, n))).map(|(n, d)| node(d.clone(), Some(*n))).collect();
            TmLedgerData { ledger_hash: self.header.hash().0.to_vec(), ledger_seq: self.header.sequence, r#type: 1, nodes, request_cookie: None, error: None }
        }
    }

    #[test]
    fn a_ledger_fetches_in_a_few_requests_and_is_the_ledgers_own() {
        let p = pairs(700);
        let peer = Peer { header: header_for(&p), wire: tx_tree_wire_nodes(&p) };
        let mut f = LedgerFetch::new(peer.header.hash().0);
        let base = f.base_request();
        assert_eq!((base.itype, base.ledger_hash.as_deref()), (0, Some(&f.hash[..])));
        assert!(matches!(f.on_data(&peer.base()), Step::Progress(_)));
        let mut rounds = 0;
        while let Some(req) = f.next_request() {
            assert_eq!(req.query_depth, Some(QUERY_DEPTH));
            rounds += 1;
            assert!(rounds < 10);
            let _ = f.on_data(&peer.tx_nodes(&req));
        }
        assert!(f.is_done());
        assert_eq!(f.rejected, 0);
        assert!(f.requests <= 4, "{} requests", f.requests);
        let (h, got) = f.into_result().unwrap();
        assert_eq!(h.sequence, 107_000_000);
        assert_eq!(compare(&got, &p), None);
    }

    #[test]
    fn a_header_that_is_not_the_ledgers_is_refused() {
        let p = pairs(10);
        let peer = Peer { header: header_for(&p), wire: tx_tree_wire_nodes(&p) };
        let mut f = LedgerFetch::new(peer.header.hash().0);
        let mut d = peer.base();
        d.nodes[0].nodedata[10] ^= 1;
        assert!(matches!(f.on_data(&d), Step::BadHeader(_)));
        assert!(f.next_request().is_none(), "nothing to ask until a header proves out");
        assert!(matches!(f.on_data(&peer.base()), Step::Progress(_)), "another peer's true header still works");
    }

    #[test]
    fn replies_for_other_ledgers_and_errors_are_not_taken() {
        let p = pairs(10);
        let peer = Peer { header: header_for(&p), wire: tx_tree_wire_nodes(&p) };
        let mut f = LedgerFetch::new([9; 32]);
        assert_eq!(f.on_data(&peer.base()), Step::Ignored);
        let mut f = LedgerFetch::new(peer.header.hash().0);
        let mut d = peer.base();
        d.error = Some(1);
        assert_eq!(f.on_data(&d), Step::PeerError(1));
    }

    #[test]
    fn a_ledger_without_transactions_is_done_at_its_header() {
        let header = header_for(&[]);
        let peer = Peer { header, wire: Vec::new() };
        let mut f = LedgerFetch::new(peer.header.hash().0);
        assert_eq!(f.on_data(&peer.base()), Step::Done);
        assert_eq!(f.into_result().unwrap().1.len(), 0);
    }

    #[test]
    fn the_comparison_names_what_differs() {
        let p = pairs(5);
        assert_eq!(compare(&p, &p), None);
        let mut q = p.clone();
        q[2].1[0] ^= 1;
        assert_eq!(compare(&p, &q).unwrap().split(" (first").next().unwrap(), "0 ours only, 0 theirs only, 1 with different bytes");
        assert!(compare(&p[..4], &p).unwrap().starts_with("0 ours only, 1 theirs only"));
    }

    #[test]
    fn the_pacer_spreads_requests_and_rests_each_peer() {
        let t0 = Instant::now();
        let mut p = Pacer::default();
        for a in ["a:1", "b:1", "c:1"] {
            p.add(a);
        }
        let first: Vec<String> = (0..3).map(|_| p.pick(t0, None).unwrap()).collect();
        assert_eq!(first, ["a:1", "b:1", "c:1"], "each once, in a stable order");
        assert_eq!(p.pick(t0 + Duration::from_secs(1), None), None, "every peer was asked within the gap");
        assert_eq!(p.pick(t0 + PEER_GAP, None).as_deref(), Some("a:1"), "the longest rested comes back first");
        assert_eq!(p.pick(t0 + PEER_GAP, Some("b:1")).as_deref(), Some("c:1"), "the one to avoid is skipped while another qualifies");
    }

    #[test]
    fn a_peer_that_keeps_missing_is_benched() {
        let t0 = Instant::now();
        let mut p = Pacer::default();
        p.add("a:1");
        p.add("b:1");
        p.missed("a:1", t0, 1);
        p.missed("a:1", t0, 1);
        p.answered("a:1");
        p.missed("a:1", t0, 1);
        assert!(p.pick(t0, Some("b:1")).as_deref() == Some("a:1"), "an answer clears the misses");
        p.missed("b:1", t0, MISSES_TO_BENCH);
        assert_eq!(p.pick(t0 + PEER_GAP, None).as_deref(), Some("a:1"), "b is benched");
        assert_eq!(p.pick(t0 + BENCH, Some("a:1")).as_deref(), Some("b:1"), "and comes back after the bench");
        p.remove("a:1");
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn a_request_frame_is_rippleds_header_then_the_message() {
        let mut f = LedgerFetch::new([3; 32]);
        let fr = frame(&f.base_request());
        let len = u32::from_be_bytes(fr[..4].try_into().unwrap()) as usize;
        assert_eq!((len + 6, u16::from_be_bytes([fr[4], fr[5]])), (fr.len(), 31));
    }

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        for line in metrics_text().lines() {
            assert!(!line.starts_with(' '), "{line:?}");
        }
    }
}
