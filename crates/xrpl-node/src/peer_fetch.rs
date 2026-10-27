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
    /// Step 4 stage A: fetched ledgers made into the engine's inputs ([`crate::peer_source`]), and held against
    /// what .39 gave it: the same, different, or not made (a record the codec could not read back).
    pub assembled_same: AtomicU64,
    pub assembled_differ: AtomicU64,
    pub assembly_failed: AtomicU64,
    /// Fetches keyed by the hash our own tally of trusted validations reached quorum on, not .39's.
    pub keyed_by_quorum: AtomicU64,
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
        "wanted {} fetched {} | match {} differ {} | failed {} skipped {} | requests {} rejected nodes {} bad headers {} | peers {} | inputs same {} differ {} not made {} | keyed by quorum {}",
        l(&st.wanted), l(&st.fetched), l(&st.matched), l(&st.differ), l(&st.failed), l(&st.skipped), l(&st.requests),
        l(&st.rejected_nodes), l(&st.bad_headers), l(&st.peers), l(&st.assembled_same), l(&st.assembled_differ),
        l(&st.assembly_failed), l(&st.keyed_by_quorum)
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
        "peers": l(&st.peers), "ms_last": l(&st.ms_last), "keyed_by_quorum": l(&st.keyed_by_quorum),
        "inputs": {"same": l(&st.assembled_same), "differ": l(&st.assembled_differ), "not_made": l(&st.assembly_failed)},
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
        ("xrpl_peer_fetch_inputs_same_total", "Fetched ledgers made into the engine's inputs, the same as .39's.", l(&st.assembled_same)),
        ("xrpl_peer_fetch_inputs_differ_total", "Fetched ledgers made into the engine's inputs, different from .39's.", l(&st.assembled_differ)),
        ("xrpl_peer_fetch_inputs_not_made_total", "Fetched ledgers that could not be made into the engine's inputs.", l(&st.assembly_failed)),
    ] {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
    }
    s
}

/// Step 4's source switch: the ledger data the engine applies comes from peers (`XRPL_LEDGER_SOURCE=peers`,
/// [`crate::ledger_source`]). The Independence page's day count ([`crate::independence_days`]) needs it on all day.
pub fn is_source() -> bool {
    crate::ledger_source::peers()
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
/// Stage A's ledgers waiting beyond this are dropped, oldest first: the compare stays at the live edge. An awaited
/// job (Step 4's source) is never dropped.
pub const QUEUE_CAP: usize = 4;
/// Awaited jobs running at once (a catch-up fetches ahead; a refused ledger's state objects go beside it).
pub const MAX_ACTIVE: usize = 4;
/// A header alone, given up after this long.
pub const HEADER_DEADLINE: Duration = Duration::from_secs(15);

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

/// What a fetch is for, and who waits for it.
enum Kind {
    /// Stage A: compared with .39's copy of the same ledger; nobody waits.
    Compare { theirs: Vec<(Vec<u8>, Vec<u8>)>, inputs: Option<std::sync::Arc<Inputs>> },
    /// Step 4's source: the whole ledger, awaited.
    Ledger(Reply<(LedgerHeader, Vec<(Vec<u8>, Vec<u8>)>)>),
    /// The header alone, awaited.
    Header(Reply<LedgerHeader>),
    /// State objects, awaited.
    State { fetch: crate::peer_state::KeyedStateFetch, reply: Reply<HashMap<[u8; 32], Option<Vec<u8>>>> },
}

type Reply<T> = tokio::sync::oneshot::Sender<Result<T, String>>;

impl Kind {
    fn awaited(&self) -> bool {
        !matches!(self, Kind::Compare { .. })
    }
}

enum Event {
    Peer { addr: String, frames: Option<tokio::sync::mpsc::Sender<Vec<u8>>> },
    Data { addr: String, data: TmLedgerData },
    Want { seq: u32, hash: [u8; 32], theirs: Vec<(Vec<u8>, Vec<u8>)>, inputs: Option<std::sync::Arc<Inputs>> },
    Job { seq: u32, hash: [u8; 32], kind: Kind, deadline: Duration },
}

static EVENTS: OnceLock<tokio::sync::mpsc::UnboundedSender<Event>> = OnceLock::new();

fn send(ev: Event) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(ev);
    }
}

/// Start the driver when `XRPL_PEER_FETCH=1`, or when peers are the ledger source (`XRPL_LEDGER_SOURCE=peers`),
/// inside the tokio runtime. True when it runs.
pub fn start() -> bool {
    let source = crate::ledger_source::peers();
    if !(enabled() || source) || EVENTS.get().is_some() {
        return EVENTS.get().is_some();
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    if EVENTS.set(tx).is_err() {
        return true;
    }
    tokio::spawn(drive(rx, log_path()));
    if source {
        eprintln!("[peer-fetch] ON — the ledger source: every ledger the engine applies comes from peers; receipts -> {}", log_path().display());
    } else {
        eprintln!("[peer-fetch] ON — each validated ledger fetched from peers and compared with .39's copy; receipts -> {}", log_path().display());
    }
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

/// What the engine took from .39 for a ledger: the header, the state root (hex) and every transaction as
/// rippled's JSON in TransactionIndex order (Step 4 stage A holds the inputs made from the peers' copy to it).
#[derive(Debug, Clone)]
pub struct Inputs {
    pub header: crate::ws_sync::LedgerHeader,
    pub account_hash: String,
    pub txs: Vec<Value>,
}

/// .39's copy of validated ledger `seq` (its hash, and every transaction with its metadata, already checked
/// against the header's transaction root): fetch the same ledger from peers and compare. The fetch asks for the
/// hash our own tally of trusted validations reached quorum on when it has one, else .39's. With `inputs`, the
/// fetched ledger is also made into the engine's inputs and held against those.
pub fn offer_network(seq: u32, hash: [u8; 32], theirs: &[(Vec<u8>, Vec<u8>)], inputs: Option<Inputs>) {
    if EVENTS.get().is_some() && hash != [0; 32] {
        let st = stats();
        st.wanted.fetch_add(1, Ordering::Relaxed);
        let hash = match crate::own_validated::quorum_hash(seq) {
            Some(q) => {
                st.keyed_by_quorum.fetch_add(1, Ordering::Relaxed);
                q
            }
            None => hash,
        };
        send(Event::Want { seq, hash, theirs: theirs.to_vec(), inputs: inputs.map(std::sync::Arc::new) });
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

// ---- awaited jobs (Step 4's source) ---------------------------------------------------------------------

fn not_running() -> String {
    "the peer fetch driver is not running".to_string()
}

async fn job<T>(tx: &tokio::sync::mpsc::UnboundedSender<Event>, seq: u32, hash: [u8; 32], deadline: Duration, kind: impl FnOnce(Reply<T>) -> Kind) -> Result<T, String> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(Event::Job { seq, hash, kind: kind(reply), deadline }).map_err(|_| not_running())?;
    rx.await.map_err(|_| "the peer fetch driver stopped".to_string())?
}

/// Ledger `seq` by its hash: the header, proven to `hash` and of sequence `seq`, and every transaction with its record
/// in tree order. Err when no peer delivered it within [`LEDGER_DEADLINE`].
pub async fn fetch_ledger(seq: u32, hash: [u8; 32]) -> Result<(LedgerHeader, Vec<(Vec<u8>, Vec<u8>)>), String> {
    job(EVENTS.get().ok_or_else(not_running)?, seq, hash, LEDGER_DEADLINE, Kind::Ledger).await
}

/// A ledger's header by its hash, proven. Err when no peer delivered it within [`HEADER_DEADLINE`].
pub async fn fetch_header(hash: [u8; 32]) -> Result<LedgerHeader, String> {
    job(EVENTS.get().ok_or_else(not_running)?, 0, hash, HEADER_DEADLINE, Kind::Header).await
}

/// The objects at `keys` in the state of the ledger `hash` (state root `account_hash`), each proven back to that
/// root; None for a key the state does not hold. Err when not complete within [`LEDGER_DEADLINE`].
pub async fn fetch_state_keys(hash: [u8; 32], account_hash: [u8; 32], keys: Vec<[u8; 32]>) -> Result<HashMap<[u8; 32], Option<Vec<u8>>>, String> {
    let fetch = crate::peer_state::KeyedStateFetch::new(account_hash, &keys);
    job(EVENTS.get().ok_or_else(not_running)?, 0, hash, LEDGER_DEADLINE, |reply| Kind::State { fetch, reply }).await
}

struct Active {
    seq: u32,
    fetch: LedgerFetch,
    kind: Kind,
    started: Instant,
    deadline: Duration,
    /// The peer last asked, and when; None when the next request is due.
    asked: Option<(String, Instant)>,
    peers_used: Vec<String>,
}

impl Active {
    fn is_done(&self) -> bool {
        match &self.kind {
            Kind::Header(_) => self.fetch.header().is_some(),
            Kind::State { fetch, .. } => fetch.is_done(),
            _ => self.fetch.is_done(),
        }
    }

    fn next_request(&mut self) -> Option<TmGetLedger> {
        match &mut self.kind {
            Kind::State { fetch, .. } => fetch.next_request(&self.fetch.hash),
            _ if self.fetch.header().is_none() => Some(self.fetch.base_request()),
            Kind::Header(_) => None,
            _ => self.fetch.next_request(),
        }
    }

    fn on_data(&mut self, d: &TmLedgerData) -> Step {
        let Kind::State { fetch, .. } = &mut self.kind else { return self.fetch.on_data(d) };
        if d.ledger_hash != self.fetch.hash || d.r#type != TmLedgerInfoType::LiAsNode as i32 {
            return Step::Ignored;
        }
        if let Some(e) = d.error {
            return Step::PeerError(e);
        }
        let o = fetch.offer(d.nodes.iter().filter_map(|n| Some((NodeId::from_raw(n.nodeid.as_deref()?)?, n.nodedata.clone()))));
        if fetch.is_done() {
            Step::Done
        } else {
            Step::Progress(o)
        }
    }
}

async fn drive(mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>, log: std::path::PathBuf) {
    let st = stats();
    let mut pacer = Pacer::default();
    let mut frames: HashMap<String, tokio::sync::mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut queue: VecDeque<(u32, [u8; 32], Kind, Duration)> = VecDeque::new();
    let mut active: Vec<Active> = Vec::new();
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
                Some(Event::Want { seq, hash, theirs, inputs }) => {
                    queue.push_back((seq, hash, Kind::Compare { theirs, inputs }, LEDGER_DEADLINE));
                    while queue.iter().filter(|q| !q.2.awaited()).count() > QUEUE_CAP {
                        if let Some(i) = queue.iter().position(|q| !q.2.awaited()) {
                            queue.remove(i);
                            st.skipped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Some(Event::Job { seq, hash, kind, deadline }) => queue.push_back((seq, hash, kind, deadline)),
                Some(Event::Data { addr, data }) => {
                    let now = Instant::now();
                    for a in active.iter_mut() {
                        let state = matches!(a.kind, Kind::State { .. });
                        match a.on_data(&data) {
                            Step::Ignored => continue,
                            // A state node that does not prove out: the peer is benched (a transaction tree's are
                            // counted when the fetch ends, as before).
                            Step::Progress(o) if state && o.rejected > 0 => {
                                st.rejected_nodes.fetch_add(o.rejected as u64, Ordering::Relaxed);
                                pacer.missed(&addr, now, MISSES_TO_BENCH);
                            }
                            Step::Progress(_) | Step::Done => pacer.answered(&addr),
                            Step::BadHeader(why) => {
                                st.bad_headers.fetch_add(1, Ordering::Relaxed);
                                pacer.missed(&addr, now, MISSES_TO_BENCH);
                                eprintln!("[peer-fetch] #{}: {addr} sent a header that is not the ledger's ({why})", a.seq);
                            }
                            Step::PeerError(_) => pacer.missed(&addr, now, 1),
                        }
                        if a.asked.as_ref().is_some_and(|(p, _)| *p == addr) {
                            a.asked = None;
                        }
                    }
                }
            },
            _ = tick.tick() => {}
        }
        st.peers.store(pacer.len() as u64, Ordering::Relaxed);
        let now = Instant::now();
        // Start what waits: awaited jobs up to MAX_ACTIVE at once, Stage A's compares one at a time as before.
        let mut i = 0;
        while i < queue.len() && active.len() < MAX_ACTIVE {
            if queue[i].2.awaited() || !active.iter().any(|a| !a.kind.awaited()) {
                let (seq, hash, kind, deadline) = queue.remove(i).expect("queued job");
                active.push(Active { seq, fetch: LedgerFetch::new(hash), kind, started: now, deadline, asked: None, peers_used: Vec::new() });
            } else {
                i += 1;
            }
        }
        let mut k = 0;
        while k < active.len() {
            if active[k].is_done() || now.duration_since(active[k].started) > active[k].deadline {
                finish(active.remove(k), &log);
            } else {
                k += 1;
            }
        }
        // Each job has at most one request out.
        for a in active.iter_mut() {
            if let Some((peer, at)) = a.asked.clone() {
                if now.duration_since(at) < REPLY_TIMEOUT {
                    continue;
                }
                pacer.missed(&peer, now, 1);
                a.asked = None;
            }
            let avoid = a.peers_used.last().cloned();
            let Some(peer) = pacer.pick(now, avoid.as_deref()) else { continue };
            let Some(req) = a.next_request() else { continue };
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
}

fn finish(a: Active, log: &std::path::Path) {
    let st = stats();
    let ms = a.started.elapsed().as_millis() as u64;
    let (requests, peers) = (a.fetch.requests, a.peers_used.len());
    let late = |what: &str| format!("{what}: not complete within the deadline ({requests} requests, {peers} peers)");
    match a.kind {
        Kind::Compare { theirs, inputs } => compared(a.seq, a.fetch, theirs, inputs, ms, peers, log),
        Kind::Ledger(reply) => {
            st.rejected_nodes.fetch_add(a.fetch.rejected as u64, Ordering::Relaxed);
            let r = match a.fetch.into_result() {
                Some((header, pairs)) if header.sequence == a.seq => {
                    st.fetched.fetch_add(1, Ordering::Relaxed);
                    st.ms_last.store(ms, Ordering::Relaxed);
                    st.last_seq.store(u64::from(a.seq), Ordering::Relaxed);
                    Ok((header, pairs))
                }
                Some((header, _)) => Err(format!("the proven header is #{}, not #{}", header.sequence, a.seq)),
                None => {
                    st.failed.fetch_add(1, Ordering::Relaxed);
                    Err(late(&format!("ledger #{}", a.seq)))
                }
            };
            let _ = reply.send(r);
        }
        Kind::Header(reply) => {
            let _ = reply.send(a.fetch.header().cloned().ok_or_else(|| late("header")));
        }
        Kind::State { fetch, reply } => {
            let _ = reply.send(fetch.into_result().ok_or_else(|| late("state objects")));
        }
    }
}

/// Stage A: a fetched ledger against .39's copy (and, with `inputs`, the engine's inputs made from it).
fn compared(seq: u32, fetch: LedgerFetch, theirs: Vec<(Vec<u8>, Vec<u8>)>, inputs: Option<std::sync::Arc<Inputs>>, ms: u64, peers: usize, log: &std::path::Path) {
    let st = stats();
    st.ms_last.store(ms, Ordering::Relaxed);
    st.last_seq.store(u64::from(seq), Ordering::Relaxed);
    st.rejected_nodes.fetch_add(fetch.rejected as u64, Ordering::Relaxed);
    let requests = fetch.requests;
    let hash = hex::encode_upper(fetch.hash);
    match fetch.into_result() {
        Some((header, ours)) => {
            let n = st.fetched.fetch_add(1, Ordering::Relaxed) + 1;
            let verdict = if header.sequence != seq {
                Some(format!("the proven header is #{}, not #{}", header.sequence, seq))
            } else {
                compare(&ours, &theirs)
            };
            match verdict {
                None => {
                    st.matched.fetch_add(1, Ordering::Relaxed);
                }
                Some(d) => {
                    st.differ.fetch_add(1, Ordering::Relaxed);
                    log_receipt(log, &json!({"seq": seq, "hash": hash, "differ": d, "requests": requests, "peers": peers, "ms": ms}));
                }
            }
            if n % 256 == 1 {
                eprintln!("[peer-fetch] heartbeat #{}: {}", seq, summary());
            }
            if let Some(inputs) = inputs {
                // Step 4 stage A, off the driver's thread: decoding and re-encoding a big ledger takes tens of ms.
                let (hash, log) = (hash.clone(), log.to_path_buf());
                tokio::task::spawn_blocking(move || check_inputs(seq, &hash, &header, ours, &inputs, &log));
            }
        }
        None => {
            st.failed.fetch_add(1, Ordering::Relaxed);
            log_receipt(log, &json!({"seq": seq, "hash": hash, "failed": "not complete within the deadline", "requests": requests, "peers": peers, "ms": ms}));
        }
    }
}

/// Step 4 stage A: the fetched ledger made into the engine's inputs, against what .39 gave the engine.
fn check_inputs(seq: u32, hash: &str, header: &LedgerHeader, pairs: Vec<(Vec<u8>, Vec<u8>)>, inputs: &Inputs, log: &std::path::Path) {
    let st = stats();
    match crate::peer_source::assemble(header, pairs) {
        Ok(a) => match crate::peer_source::compare(&a, &inputs.header, &inputs.account_hash, &inputs.txs) {
            None => {
                st.assembled_same.fetch_add(1, Ordering::Relaxed);
            }
            Some(d) => {
                st.assembled_differ.fetch_add(1, Ordering::Relaxed);
                log_receipt(log, &json!({"seq": seq, "hash": hash, "inputs": d}));
            }
        },
        Err(e) => {
            st.assembly_failed.fetch_add(1, Ordering::Relaxed);
            log_receipt(log, &json!({"seq": seq, "hash": hash, "inputs_not_made": e}));
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
    fn a_fetched_ledger_is_made_into_the_engines_inputs_and_held_against_39s() {
        use crate::peer_source::fixtures::{ledger, theirs};
        let (h, pairs) = ledger();
        let a = crate::peer_source::assemble(&h, pairs.clone()).unwrap();
        let (th, ta, tt) = theirs(&a);
        let log = std::env::temp_dir().join(format!("peer_fetch_inputs_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let st = stats();
        let (same, differ) = (st.assembled_same.load(Ordering::Relaxed), st.assembled_differ.load(Ordering::Relaxed));
        check_inputs(1, "AB", &h, pairs.clone(), &Inputs { header: th.clone(), account_hash: ta.clone(), txs: tt.clone() }, &log);
        assert!(st.assembled_same.load(Ordering::Relaxed) > same);
        assert!(!log.exists(), "nothing to report");
        let mut odd = tt.clone();
        odd[0]["Fee"] = json!("99");
        check_inputs(2, "CD", &h, pairs, &Inputs { header: th, account_hash: ta, txs: odd }, &log);
        assert!(st.assembled_differ.load(Ordering::Relaxed) > differ);
        let line: Value = serde_json::from_str(std::fs::read_to_string(&log).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(line["seq"], 2);
        assert!(line["inputs"].as_str().unwrap().contains("Fee: ours \"12\", .39's \"99\""), "{line}");
        let _ = std::fs::remove_file(&log);
    }

    /// The driver on this test's runtime, with `n` peers answering from `ledgers` (transaction trees) and `state` (a
    /// state tree and the ledger hash it answers for), or never when `mute`. Also the requests the peers got.
    fn rig(
        ledgers: Vec<Peer>,
        state: Option<(&'static HashMap<NodeId, Vec<u8>>, [u8; 32])>,
        n: usize,
        mute: bool,
    ) -> (tokio::sync::mpsc::UnboundedSender<Event>, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use prost::Message as _;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let log = std::env::temp_dir().join(format!("peer_fetch_rig_{}_{n}.jsonl", std::process::id()));
        tokio::spawn(drive(rx, log));
        let ledgers = std::sync::Arc::new(ledgers);
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for i in 0..n {
            let addr = format!("10.9.9.{i}:51235");
            let (ftx, mut frx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            tx.send(Event::Peer { addr: addr.clone(), frames: Some(ftx) }).unwrap();
            let (tx2, ledgers, asked) = (tx.clone(), ledgers.clone(), asked.clone());
            tokio::spawn(async move {
                while let Some(f) = frx.recv().await {
                    asked.fetch_add(1, Ordering::Relaxed);
                    if mute {
                        continue;
                    }
                    let req = TmGetLedger::decode(&f[6..]).unwrap();
                    let want = req.ledger_hash.clone().unwrap_or_default();
                    let reply = if req.itype == TmLedgerInfoType::LiAsNode as i32 {
                        state.filter(|(_, h)| want == h[..]).and_then(|(tree, h)| crate::peer_state::tests::Peer::new(tree).serve(&req, &h))
                    } else {
                        ledgers.iter().find(|p| want == p.header.hash().0[..]).map(|p| {
                            if req.itype == TmLedgerInfoType::LiBase as i32 { p.base() } else { p.tx_nodes(&req) }
                        })
                    };
                    if let Some(d) = reply {
                        let _ = tx2.send(Event::Data { addr: addr.clone(), data: d });
                    }
                }
            });
        }
        (tx, asked)
    }

    fn served(n: usize, seq: u32) -> (Peer, Vec<(Vec<u8>, Vec<u8>)>) {
        let p = pairs(n);
        let mut header = header_for(&p);
        header.sequence = seq;
        (Peer { header, wire: tx_tree_wire_nodes(&p) }, p)
    }

    #[tokio::test]
    async fn two_ledgers_fetch_at_once_and_each_is_its_own() {
        let ((a, pa), (b, pb)) = (served(300, 107_000_001), served(40, 107_000_002));
        let (ha, hb) = (a.header.hash().0, b.header.hash().0);
        let (tx, _) = rig(vec![a, b], None, 10, false);
        let (ra, rb) = tokio::join!(
            job(&tx, 107_000_001, ha, LEDGER_DEADLINE, Kind::Ledger),
            job(&tx, 107_000_002, hb, LEDGER_DEADLINE, Kind::Ledger)
        );
        let ((h1, got_a), (h2, got_b)) = (ra.unwrap(), rb.unwrap());
        assert_eq!((h1.sequence, h2.sequence), (107_000_001, 107_000_002));
        assert_eq!(compare(&got_a, &pa), None);
        assert_eq!(compare(&got_b, &pb), None);
    }

    #[tokio::test]
    async fn a_ledger_of_another_sequence_is_an_error() {
        let (a, _) = served(10, 107_000_003);
        let h = a.header.hash().0;
        let (tx, _) = rig(vec![a], None, 4, false);
        let e = job(&tx, 107_000_004, h, LEDGER_DEADLINE, Kind::Ledger).await.unwrap_err();
        assert!(e.contains("is #107000003, not #107000004"), "{e}");
    }

    #[tokio::test]
    async fn a_header_job_takes_one_base_request() {
        let (a, _) = served(500, 107_000_005);
        let h = a.header.hash().0;
        let (tx, asked) = rig(vec![a], None, 4, false);
        let header = job(&tx, 0, h, HEADER_DEADLINE, Kind::Header).await.unwrap();
        assert_eq!(header.hash().0, h);
        assert_eq!(asked.load(Ordering::Relaxed), 1, "the header comes in the first reply");
    }

    #[tokio::test]
    async fn a_state_job_brings_the_named_objects_proven() {
        let lv = crate::peer_state::tests::leaves(1_500);
        let (tree, root) = crate::peer_state::tests::state_tree(&lv);
        let tree: &'static HashMap<NodeId, Vec<u8>> = Box::leak(Box::new(tree));
        let ledger = [0x5A; 32];
        let mut absent = lv[9].0;
        absent[30] ^= 0x33;
        let keys = vec![lv[2].0, lv[1_200].0, absent];
        let (tx, _) = rig(Vec::new(), Some((tree, ledger)), 6, false);
        let fetch = crate::peer_state::KeyedStateFetch::new(root, &keys);
        let got = job(&tx, 0, ledger, LEDGER_DEADLINE, |reply| Kind::State { fetch, reply }).await.unwrap();
        assert_eq!(got.get(&lv[2].0), Some(&Some(lv[2].1.clone())));
        assert_eq!(got.get(&lv[1_200].0), Some(&Some(lv[1_200].1.clone())));
        assert_eq!(got.get(&absent), Some(&None));
    }

    #[tokio::test]
    async fn an_awaited_job_is_not_dropped_by_the_stage_a_cap() {
        let (a, pa) = served(60, 107_000_006);
        let h = a.header.hash().0;
        let (tx, _) = rig(vec![a], None, 10, false);
        let skipped = stats().skipped.load(Ordering::Relaxed);
        for i in 0..6u8 {
            tx.send(Event::Want { seq: 1 + u32::from(i), hash: [0xE0 + i; 32], theirs: Vec::new(), inputs: None }).unwrap();
        }
        let (_, got) = job(&tx, 107_000_006, h, LEDGER_DEADLINE, Kind::Ledger).await.unwrap();
        assert_eq!(compare(&got, &pa), None);
        assert!(stats().skipped.load(Ordering::Relaxed) > skipped, "Stage A still drops its oldest past the cap");
    }

    #[tokio::test]
    async fn a_job_no_peer_answers_ends_in_err_at_its_deadline() {
        let (tx, asked) = rig(Vec::new(), None, 3, true);
        let t0 = Instant::now();
        let e = job(&tx, 0, [7; 32], Duration::from_millis(800), Kind::Header).await.unwrap_err();
        assert!(e.contains("deadline"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(3));
        assert!(asked.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn without_the_driver_an_awaited_fetch_says_so() {
        if EVENTS.get().is_none() {
            assert_eq!(fetch_header([1; 32]).await.unwrap_err(), not_running());
        }
    }

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        for line in metrics_text().lines() {
            assert!(!line.starts_with(' '), "{line:?}");
        }
    }
}
