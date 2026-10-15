//! Independence step 8, in shadow: a ledger's whole account state from mainnet peers, with no .39 in the path.
//!
//!   peer_state_sync --out DIR [--ledger-hash H | --seq N] [--limit NODES] [--peers a:51235,b:51235]
//!                   [--max-peers 12] [--gap-ms 1500] [--timeout-s 30] [--max-minutes M]
//!                   [--witness http://10.0.0.39:5005 | --witness none] [--fresh] [--no-recheck]
//!
//! 1. Dials mainnet peers itself, with one node identity for the run: live_viewer's bootstrap peers (and --peers),
//!    then the peers they name in TMEndpoints. Pings are answered (a peer drops a session that leaves its ping
//!    unanswered for a minute).
//! 2. Picks the ledger. By default the newest one a quorum of the verified validator list's validators validated,
//!    counted from the signature-checked validations the peers relay (own_validated's tally; the quorum over the
//!    whole list, with no negative UNL to shrink it, so never weaker than rippled's). --seq N takes N once it is
//!    validated, or walks back to it by parent hash from the newest validated ledger, each header proven.
//!    --ledger-hash takes a hash as given.
//! 3. Fetches the header (liBASE) and proves it hashes to the ledger hash; the reply also carries the state root.
//! 4. Fetches the whole state tree (liAS_NODE), every node proven to the hash its parent names (peer_state). Each
//!    peer has at most one request out, and at least --gap-ms between two, so each costs a peer one
//!    `feeModerateBurdenPeer` at a time; a peer whose validated range does not hold the ledger is not asked. A peer
//!    that stalls three times running, or drops us, is replaced after a 10-minute rest; one that sends a node that
//!    does not prove out is dropped for the run.
//! 5. Streams the proven leaves to DIR/leaves.bin (key, length, object) and checkpoints the frontier to
//!    DIR/checkpoint.bin every 30 s and at the end, so a rerun with the same DIR resumes (--fresh starts over).
//! 6. Recomputes the root bottom-up from leaves.bin alone, and prints "ROOT PROVEN: <account_hash>" only when the
//!    fetch left no position wanted and the file's root is the header's account_hash.
//!
//! .39 (--witness) is asked once, at the end, for the same ledger's hashes: printed, never used to build anything.
//! --limit stops after that many nodes (a trial); progress prints every 10 s.

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::Decoder;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::shamap::wire::NodeId;
use xrpl_node::consensus::sig_verify::{verify_validation_blob, SigOutcome};
use xrpl_node::own_validated::Tally;
use xrpl_node::peer::codec::{pong_frame_for, MessageCodec};
use xrpl_node::peer::handshake;
use xrpl_node::peer::identity::NodeIdentity;
use xrpl_node::peer::message::PeerMessage;
use xrpl_node::peer::protocol::{TmLedgerData, TmLedgerInfoType};
use xrpl_node::peer_fetch::frame;
use xrpl_node::peer_state::{self, Checkpoint, LeafFile, StateTreeFetch, SOFT_MAX_REPLY_NODES};

/// live_viewer's bootstrap peers (`MAINNET_PEERS`).
const BOOTSTRAP: &[&str] = &["s1.ripple.com:51235", "s2.ripple.com:51235"];
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const DIALS_AT_ONCE: usize = 4;
/// A peer that failed, stalled or closed rests this long before it is dialed again.
const COOLDOWN: Duration = Duration::from_secs(600);
/// A header request unanswered this long goes to another peer (a peer without the ledger sends nothing).
const BASE_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeouts in a row that drop a peer.
const STRIKES_TO_DROP: u32 = 3;
/// A peer whose validated range has not held the ledger for this long is replaced.
const OUT_OF_RANGE_GRACE: Duration = Duration::from_secs(60);
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
const CHECKPOINT_EVERY: Duration = Duration::from_secs(30);
/// No quorum-validated ledger after this long: give up.
const TARGET_WAIT: Duration = Duration::from_secs(180);
/// No node accepted for this long while fetching the state: give up (the checkpoint stays).
const STALL: Duration = Duration::from_secs(600);
/// Headers walked back at most for --seq.
const WALK_MAX: u32 = 2048;

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

fn flag(name: &str) -> bool {
    std::env::args().any(|x| x == name)
}

fn parse_hash(s: &str) -> Option<[u8; 32]> {
    hex::decode(s.trim()).ok()?.try_into().ok()
}

fn hexu(h: &[u8]) -> String {
    hex::encode_upper(h)
}

fn n(x: u64) -> String {
    let s = x.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn gb(b: u64) -> String {
    format!("{:.2} GB", b as f64 / 1e9)
}

fn hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn rss_gb() -> f64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(0.0, |p| p * 4096.0 / 1e9)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

struct Args {
    out: PathBuf,
    ledger_hash: Option<[u8; 32]>,
    seq: Option<u32>,
    limit: Option<u64>,
    bootstrap: Vec<String>,
    max_peers: usize,
    gap: Duration,
    timeout: Duration,
    max_run: Option<Duration>,
    witness: Option<String>,
    fresh: bool,
    recheck: bool,
}

fn parse_args() -> Result<Args, String> {
    let ledger_hash = match arg("--ledger-hash") {
        Some(h) => Some(parse_hash(&h).ok_or("--ledger-hash: 64 hex digits")?),
        None => None,
    };
    let num = |name: &str| -> Result<Option<u64>, String> {
        arg(name).map(|v| v.parse::<u64>().map_err(|_| format!("{name}: a number"))).transpose()
    };
    let mut bootstrap: Vec<String> = BOOTSTRAP.iter().map(|s| s.to_string()).collect();
    if let Some(p) = arg("--peers") {
        bootstrap.extend(p.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from));
    }
    let witness = match arg("--witness").as_deref() {
        Some("none") | Some("") => None,
        Some(u) => Some(u.to_string()),
        None => Some("http://10.0.0.39:5005".to_string()),
    };
    Ok(Args {
        out: PathBuf::from(arg("--out").unwrap_or_else(|| "peer_state_out".into())),
        ledger_hash,
        seq: num("--seq")?.map(|s| s as u32),
        limit: num("--limit")?,
        bootstrap,
        max_peers: num("--max-peers")?.unwrap_or(12).clamp(1, 64) as usize,
        gap: Duration::from_millis(num("--gap-ms")?.unwrap_or(1500)),
        timeout: Duration::from_secs(num("--timeout-s")?.unwrap_or(30).max(5)),
        max_run: num("--max-minutes")?.map(|m| Duration::from_secs(60 * m)),
        witness,
        fresh: flag("--fresh"),
        recheck: !flag("--no-recheck"),
    })
}

// ---- peer sessions ------------------------------------------------------------------------------------------

enum Ev {
    Up { addr: String, frames: mpsc::Sender<Vec<u8>>, kill: oneshot::Sender<()> },
    Failed { addr: String, why: String },
    Down { addr: String, why: String },
    Data { addr: String, data: TmLedgerData },
    Endpoints { list: Vec<String> },
    Validation { blob: Vec<u8> },
    Status { addr: String, first: u32, last: u32 },
}

/// One peer session: hand the driver what it needs, write the frames it sends, answer pings. Ends when the peer
/// closes, or when the driver drops its slot (the kill sender goes with it).
async fn session(
    addr: String,
    identity: Arc<NodeIdentity>,
    ev: mpsc::Sender<Ev>,
    bytes_in: Arc<AtomicU64>,
    want_validations: Arc<AtomicBool>,
) {
    let hs = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake::outbound_handshake(&addr, &identity, handshake::NETWORK_ID_MAINNET),
    )
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            let _ = ev.send(Ev::Failed { addr, why: e.to_string() }).await;
            return;
        }
        Err(_) => {
            let _ = ev.send(Ev::Failed { addr, why: "handshake timed out".into() }).await;
            return;
        }
    };
    let (mut rd, mut wr) = tokio::io::split(hs.stream);
    let (frames_tx, mut frames_rx) = mpsc::channel::<Vec<u8>>(8);
    let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
    if ev.send(Ev::Up { addr: addr.clone(), frames: frames_tx.clone(), kill: kill_tx }).await.is_err() {
        return;
    }
    let writer = tokio::spawn(async move {
        while let Some(f) = frames_rx.recv().await {
            if wr.write_all(&f).await.is_err() {
                break;
            }
        }
    });
    let mut buf = BytesMut::from(&hs.remaining_bytes[..]);
    let mut codec = MessageCodec;
    let why: String = loop {
        match codec.decode(&mut buf) {
            Ok(Some(msg)) => {
                if let Some(pong) = pong_frame_for(&msg) {
                    let _ = frames_tx.try_send(pong);
                    continue;
                }
                let e = match msg {
                    PeerMessage::LedgerData(d) => Some(Ev::Data { addr: addr.clone(), data: d }),
                    PeerMessage::Endpoints(e) => Some(Ev::Endpoints {
                        list: e.endpoints_v2.into_iter().filter(|x| x.hops <= 3).map(|x| x.endpoint).collect(),
                    }),
                    PeerMessage::Validation(v) if want_validations.load(Ordering::Relaxed) => {
                        Some(Ev::Validation { blob: v.validation })
                    }
                    PeerMessage::StatusChange(s) => match (s.first_seq, s.last_seq) {
                        (Some(first), Some(last)) if first > 0 && last >= first => {
                            Some(Ev::Status { addr: addr.clone(), first, last })
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(e) = e {
                    if ev.send(e).await.is_err() {
                        break "the driver ended".into();
                    }
                }
            }
            Ok(None) => {
                tokio::select! {
                    r = rd.read_buf(&mut buf) => match r {
                        Ok(0) => break "closed by the peer".into(),
                        Ok(k) => { bytes_in.fetch_add(k as u64, Ordering::Relaxed); }
                        Err(e) => break format!("read: {e}"),
                    },
                    _ = &mut kill_rx => break "dropped by us".into(),
                }
            }
            Err(e) => break format!("decode: {e}"),
        }
    };
    writer.abort();
    let _ = ev.send(Ev::Down { addr, why }).await;
}

// ---- the driver ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    Base([u8; 32]),
    State(u64),
}

struct Slot {
    frames: mpsc::Sender<Vec<u8>>,
    _kill: oneshot::Sender<()>,
    asked: Option<(Ask, Instant)>,
    last_sent: Option<Instant>,
    strikes: u32,
    /// The peer's full validated range, from its TMStatusChange.
    range: Option<(u32, u32)>,
    up_at: Instant,
    replies: u64,
}

/// What one peer did over the run (all its sessions).
#[derive(Default)]
struct PeerStats {
    sessions: u32,
    replies: u64,
    nodes: u64,
    bytes: u64,
    max_nodes: usize,
    max_bytes: u64,
    latency_ms: u64,
    timeouts: u64,
    last_close: String,
}

#[derive(Default)]
struct Tallies {
    req_state: u64,
    req_base: u64,
    replies_state: u64,
    replies_base: u64,
    full_replies: u64,
    timeouts: u64,
    peer_errors: u64,
    state_bytes: u64,
    dialed: u64,
    handshake_failed: u64,
    handshake_full: u64,
    connected: u64,
    dropped_bad: u64,
    dropped_stalled: u64,
    dropped_range: u64,
    closed: u64,
    closed_while_asked: u64,
    walk_steps: u32,
    validations: u64,
    bad_sigs: u64,
    endpoints_msgs: u64,
}

enum Phase {
    /// Waiting for a quorum of trusted validations on some ledger.
    Target,
    /// Fetching the header of `want`; `seq` is the sequence the target must have (walk back by parent hash while the
    /// proven header is newer).
    Header { want: [u8; 32], seq: Option<u32> },
    State,
    Done(String),
}

struct Run {
    args: Args,
    identity: Arc<NodeIdentity>,
    ev_tx: mpsc::Sender<Ev>,
    bytes_in: Arc<AtomicU64>,
    want_validations: Arc<AtomicBool>,
    peers: HashMap<String, Slot>,
    candidates: VecDeque<String>,
    known: HashSet<String>,
    dialing: HashSet<String>,
    cooldown: HashMap<String, Instant>,
    banned: HashSet<String>,
    used: HashSet<String>,
    per_peer: HashMap<String, PeerStats>,
    t: Tallies,
    phase: Phase,
    tally: Tally,
    /// Sequences a quorum validated: the hash, or None when two hashes did (never on a healthy network).
    validated: BTreeMap<u32, Option<[u8; 32]>>,
    quorum_seen: (usize, usize),
    header: Option<LedgerHeader>,
    target: Option<[u8; 32]>,
    target_how: String,
    fetch: Option<StateTreeFetch>,
    leaves: Option<LeafFile>,
    io_err: Option<String>,
    started: Instant,
    state_started: Option<Instant>,
    resumed_nodes: u64,
    last_progress: Instant,
    last_checkpoint: Instant,
    last_accept: Instant,
    last_nodes: u64,
}

impl Run {
    fn leaves_path(&self) -> PathBuf {
        self.args.out.join("leaves.bin")
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.args.out.join("checkpoint.bin")
    }

    fn target_seq(&self) -> Option<u32> {
        match &self.phase {
            Phase::Header { seq, .. } => *seq,
            _ => self.header.as_ref().map(|h| h.sequence),
        }
    }

    // -- dialing --

    fn queue(&mut self, addr: String) {
        if !self.candidates.contains(&addr) {
            self.candidates.push_back(addr);
        }
    }

    fn cool(&mut self, addr: &str, now: Instant) {
        self.cooldown.insert(addr.to_string(), now + COOLDOWN);
        self.queue(addr.to_string());
    }

    fn next_candidate(&mut self, now: Instant) -> Option<String> {
        for _ in 0..self.candidates.len() {
            let a = self.candidates.pop_front()?;
            if self.peers.contains_key(&a) || self.dialing.contains(&a) || self.banned.contains(&a) {
                continue;
            }
            if self.cooldown.get(&a).is_some_and(|t| *t > now) {
                self.candidates.push_back(a);
                continue;
            }
            return Some(a);
        }
        None
    }

    fn dial_more(&mut self, now: Instant) {
        while self.peers.len() + self.dialing.len() < self.args.max_peers && self.dialing.len() < DIALS_AT_ONCE {
            let Some(addr) = self.next_candidate(now) else { break };
            self.dialing.insert(addr.clone());
            self.t.dialed += 1;
            tokio::spawn(session(
                addr,
                self.identity.clone(),
                self.ev_tx.clone(),
                self.bytes_in.clone(),
                self.want_validations.clone(),
            ));
        }
    }

    /// Drop a peer: its request goes back, and it rests (or, for bad data, is never dialed again this run).
    fn drop_peer(&mut self, addr: &str, why: &str, ban: bool, now: Instant) {
        if let Some(slot) = self.peers.remove(addr) {
            if let (Some((Ask::State(tk), _)), Some(f)) = (slot.asked, self.fetch.as_mut()) {
                f.release(tk);
            }
        }
        if ban {
            self.banned.insert(addr.to_string());
        } else {
            self.cool(addr, now);
        }
        eprintln!("[peer] - {addr} dropped: {why} ({} up)", self.peers.len());
    }

    // -- events --

    fn on_event(&mut self, ev: Ev, now: Instant) {
        match ev {
            Ev::Up { addr, frames, kill } => {
                self.dialing.remove(&addr);
                if self.banned.contains(&addr) || matches!(self.phase, Phase::Done(_)) {
                    return; // the kill sender drops here: the session ends
                }
                self.t.connected += 1;
                self.per_peer.entry(addr.clone()).or_default().sessions += 1;
                self.peers.insert(
                    addr.clone(),
                    Slot { frames, _kill: kill, asked: None, last_sent: None, strikes: 0, range: None, up_at: now, replies: 0 },
                );
                eprintln!("[peer] + {addr} ({} up)", self.peers.len());
            }
            Ev::Failed { addr, why } => {
                self.dialing.remove(&addr);
                self.t.handshake_failed += 1;
                if why.contains(" 503") {
                    self.t.handshake_full += 1;
                }
                eprintln!("[peer] x {addr}: {}", why.chars().take(100).collect::<String>());
                self.cool(&addr, now);
            }
            Ev::Down { addr, why } => {
                if let Some(slot) = self.peers.remove(&addr) {
                    self.t.closed += 1;
                    if let Some((ask, _)) = slot.asked {
                        self.t.closed_while_asked += 1;
                        if let (Ask::State(tk), Some(f)) = (ask, self.fetch.as_mut()) {
                            f.release(tk);
                        }
                    }
                    self.per_peer.entry(addr.clone()).or_default().last_close = why.clone();
                    eprintln!(
                        "[peer] - {addr} closed after {}s and {} replies: {why} ({} up)",
                        now.duration_since(slot.up_at).as_secs(),
                        slot.replies,
                        self.peers.len()
                    );
                    self.cool(&addr, now);
                }
            }
            Ev::Data { addr, data } => self.on_data(&addr, data, now),
            Ev::Endpoints { list } => {
                self.t.endpoints_msgs += 1;
                for e in list {
                    if let Some(a) = peer_state::dialable(&e) {
                        if self.known.insert(a.clone()) {
                            self.queue(a);
                        }
                    }
                }
            }
            Ev::Validation { blob } => self.on_validation(&blob),
            Ev::Status { addr, first, last } => {
                if let Some(s) = self.peers.get_mut(&addr) {
                    s.range = Some((first, last));
                }
            }
        }
    }

    fn on_validation(&mut self, blob: &[u8]) {
        if !matches!(self.phase, Phase::Target) {
            return;
        }
        self.t.validations += 1;
        if verify_validation_blob(blob) != SigOutcome::Valid {
            self.t.bad_sigs += 1;
            return;
        }
        let now = now_ms();
        let Some(v) = xrpl_node::consensus_record::parse_validation(blob, now) else { return };
        self.tally.vote(&v.signing_key, v.ledger_seq, v.ledger_hash, v.full, now);
        let (count, q) = (self.tally.count(v.ledger_seq, &v.ledger_hash), self.tally.quorum());
        if q > 0 && count >= q {
            let e = self.validated.entry(v.ledger_seq).or_insert(Some(v.ledger_hash));
            if *e != Some(v.ledger_hash) {
                *e = None;
            }
            self.quorum_seen = (count, q);
        }
    }

    fn on_data(&mut self, addr: &str, d: TmLedgerData, now: Instant) {
        if d.r#type == TmLedgerInfoType::LiBase as i32 {
            self.on_base(addr, &d, now);
        } else if d.r#type == TmLedgerInfoType::LiAsNode as i32 {
            self.on_state(addr, &d, now);
        }
    }

    fn on_base(&mut self, addr: &str, d: &TmLedgerData, now: Instant) {
        let Phase::Header { want, seq } = self.phase else { return };
        if d.ledger_hash != want {
            return;
        }
        self.t.replies_base += 1;
        if let Some(s) = self.peers.get_mut(addr) {
            if matches!(s.asked, Some((Ask::Base(h), _)) if h == want) {
                s.asked = None;
                s.strikes = 0;
                s.replies += 1;
            }
        }
        if d.error.is_some() {
            self.t.peer_errors += 1;
            return;
        }
        let (header, root) = match peer_state::prove_base(&want, d) {
            Ok(x) => x,
            Err(e) => {
                self.t.dropped_bad += 1;
                self.drop_peer(addr, &format!("a liBASE reply that does not prove out: {e}"), true, now);
                return;
            }
        };
        match seq {
            Some(s) if header.sequence > s => {
                self.t.walk_steps += 1;
                if self.t.walk_steps > WALK_MAX {
                    self.phase = Phase::Done(format!("#{s} is more than {WALK_MAX} ledgers back"));
                } else {
                    self.phase = Phase::Header { want: header.parent_hash.0, seq };
                }
            }
            Some(s) if header.sequence < s => {
                self.phase = Phase::Done(format!("{} is #{}, not #{s}", hexu(&want), header.sequence));
            }
            _ => self.start_state(want, header, root, addr, now),
        }
    }

    fn start_state(&mut self, hash: [u8; 32], header: LedgerHeader, root: Option<Vec<u8>>, from: &str, now: Instant) {
        eprintln!(
            "[target] #{} {} proven from its header (from {from}); account_hash {}; close time {}",
            header.sequence,
            hexu(&hash),
            hexu(&header.account_hash.0),
            header.close_time
        );
        let leaves = match LeafFile::open(&self.leaves_path(), 0) {
            Ok(f) => f,
            Err(e) => {
                self.phase = Phase::Done(format!("opening {}: {e}", self.leaves_path().display()));
                return;
            }
        };
        let mut fetch = StateTreeFetch::new(header.account_hash.0);
        if let Some(r) = root {
            let o = fetch.offer([(NodeId::ROOT, r.as_slice())], &mut |_, _| {});
            eprintln!("[target] the state root came with the header: {}", if o.accepted == 1 { "proven" } else { "refused" });
        }
        self.want_validations.store(false, Ordering::Relaxed);
        self.target = Some(hash);
        self.header = Some(header);
        self.fetch = Some(fetch);
        self.leaves = Some(leaves);
        self.state_started = Some(now);
        self.last_accept = now;
        self.phase = Phase::State;
    }

    fn on_state(&mut self, addr: &str, d: &TmLedgerData, now: Instant) {
        let (Some(target), Some(fetch), Some(leaves)) = (self.target, self.fetch.as_mut(), self.leaves.as_mut()) else { return };
        if d.ledger_hash != target {
            return;
        }
        let asked = self.peers.get(addr).and_then(|s| s.asked).map(|(a, _)| a);
        if d.error.is_some() {
            self.t.peer_errors += 1;
            if let Some(Ask::State(tk)) = asked {
                fetch.release(tk);
            }
            self.drop_peer(addr, &format!("answered error {:?} (it lacks the ledger)", d.error), false, now);
            return;
        }
        let k = d.nodes.len();
        let bytes = d.nodes.iter().map(|x| x.nodedata.len() as u64).sum::<u64>();
        self.t.replies_state += 1;
        self.t.state_bytes += bytes;
        let ps = self.per_peer.entry(addr.to_string()).or_default();
        ps.replies += 1;
        ps.nodes += k as u64;
        ps.bytes += bytes;
        ps.max_nodes = ps.max_nodes.max(k);
        ps.max_bytes = ps.max_bytes.max(bytes);
        if let Some((Ask::State(_), at)) = self.peers.get(addr).and_then(|s| s.asked) {
            ps.latency_ms += now.duration_since(at).as_millis() as u64;
        }
        if k >= SOFT_MAX_REPLY_NODES {
            self.t.full_replies += 1;
        }
        let io_err = &mut self.io_err;
        let nodes = d.nodes.iter().filter_map(|x| Some((NodeId::from_raw(x.nodeid.as_deref()?)?, x.nodedata.as_slice())));
        let o = fetch.offer(nodes, &mut |key, item| {
            if let Err(e) = leaves.append(key, item) {
                io_err.get_or_insert_with(|| e.to_string());
            }
        });
        if o.accepted > 0 {
            self.last_accept = now;
            self.used.insert(addr.to_string());
        }
        if let Some(s) = self.peers.get_mut(addr) {
            if let Some(Ask::State(tk)) = asked {
                fetch.settle(tk, k);
                s.asked = None;
                s.strikes = 0;
                s.replies += 1;
            }
        }
        if o.rejected > 0 {
            self.t.dropped_bad += 1;
            self.drop_peer(addr, &format!("{} nodes that are not what their parents name", o.rejected), true, now);
        }
    }

    // -- the clock --

    fn housekeeping(&mut self, now: Instant) {
        self.dial_more(now);
        self.timeouts(now);
        match self.phase {
            Phase::Target => self.choose_target(now),
            Phase::Header { want, seq } => self.ask_header(want, seq, now),
            Phase::State => {
                self.drop_out_of_range(now);
                self.ask_state(now);
                self.check_state_end(now);
            }
            Phase::Done(_) => {}
        }
        if now.duration_since(self.last_progress) >= PROGRESS_EVERY {
            self.progress(now);
            self.last_progress = now;
        }
        if matches!(self.phase, Phase::State) && now.duration_since(self.last_checkpoint) >= CHECKPOINT_EVERY {
            if let Err(e) = self.save_checkpoint() {
                eprintln!("[checkpoint] failed: {e}");
            }
            self.last_checkpoint = now;
        }
        if let Some(m) = self.args.max_run {
            if now.duration_since(self.started) >= m && !matches!(self.phase, Phase::Done(_)) {
                self.phase = Phase::Done(format!("--max-minutes {} reached", m.as_secs() / 60));
            }
        }
    }

    fn timeouts(&mut self, now: Instant) {
        let limit = |ask: Ask| match ask {
            Ask::Base(_) => BASE_TIMEOUT.min(self.args.timeout),
            Ask::State(_) => self.args.timeout,
        };
        let late: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, s)| s.asked.is_some_and(|(ask, at)| now.duration_since(at) >= limit(ask)))
            .map(|(a, _)| a.clone())
            .collect();
        for addr in late {
            let Some(s) = self.peers.get_mut(&addr) else { continue };
            let (ask, _) = s.asked.take().expect("asked");
            s.strikes += 1;
            let strikes = s.strikes;
            self.t.timeouts += 1;
            self.per_peer.entry(addr.clone()).or_default().timeouts += 1;
            if let (Ask::State(tk), Some(f)) = (ask, self.fetch.as_mut()) {
                f.release(tk);
            }
            if strikes >= STRIKES_TO_DROP {
                self.t.dropped_stalled += 1;
                self.drop_peer(&addr, &format!("{strikes} requests in a row unanswered"), false, now);
            }
        }
    }

    fn choose_target(&mut self, now: Instant) {
        let newest = self.validated.iter().rev().find_map(|(s, h)| h.map(|h| (*s, h)));
        let (q_count, q) = self.quorum_seen;
        match (newest, self.args.seq) {
            (Some((s, h)), None) => {
                self.target_how = format!("the newest ledger a quorum of trusted validators validated ({q_count}/{q} when chosen)");
                self.phase = Phase::Header { want: h, seq: Some(s) };
            }
            (Some(_), Some(want)) if matches!(self.validated.get(&want), Some(Some(_))) => {
                let h = self.validated[&want].expect("checked");
                self.target_how = format!("--seq {want}, validated by a quorum of trusted validators ({q_count}/{q})");
                self.phase = Phase::Header { want: h, seq: Some(want) };
            }
            (Some((s, h)), Some(want)) if want < s => {
                self.target_how = format!("--seq {want}, walked back by parent hash from #{s}, which a quorum validated");
                self.phase = Phase::Header { want: h, seq: Some(want) };
            }
            _ => {
                if now.duration_since(self.started) > TARGET_WAIT {
                    self.phase = Phase::Done(format!(
                        "no ledger reached a quorum of trusted validations in {} s ({} validations, {} bad signatures)",
                        TARGET_WAIT.as_secs(),
                        self.t.validations,
                        self.t.bad_sigs
                    ));
                }
            }
        }
        if let Phase::Header { want, seq } = self.phase {
            eprintln!("[target] {}: {} #{}", self.target_how, hexu(&want), seq.map_or("?".into(), |s| s.to_string()));
        }
    }

    /// Peers ready for a request: none out, the gap since the last one passed, and (when known) a validated range
    /// that holds `seq`. Longest rested first.
    fn ready(&self, now: Instant, seq: Option<u32>) -> Vec<String> {
        let mut v: Vec<(&String, &Slot)> = self
            .peers
            .iter()
            .filter(|(_, s)| {
                s.asked.is_none()
                    && s.last_sent.is_none_or(|t| now.duration_since(t) >= self.args.gap)
                    && match (seq, s.range) {
                        (Some(q), Some((lo, hi))) => lo <= q && q <= hi,
                        _ => true,
                    }
            })
            .collect();
        v.sort_by(|a, b| a.1.last_sent.cmp(&b.1.last_sent).then(a.0.cmp(b.0)));
        v.into_iter().map(|(a, _)| a.clone()).collect()
    }

    fn send(&mut self, addr: &str, ask: Ask, f: Vec<u8>, now: Instant) -> bool {
        let Some(s) = self.peers.get_mut(addr) else { return false };
        if s.frames.try_send(f).is_err() {
            return false;
        }
        s.asked = Some((ask, now));
        s.last_sent = Some(now);
        true
    }

    fn ask_header(&mut self, want: [u8; 32], seq: Option<u32>, now: Instant) {
        if self.peers.values().any(|s| matches!(s.asked, Some((Ask::Base(_), _)))) {
            return; // one header request out at a time
        }
        // A peer whose range is known to hold it first; then any.
        let ready = self.ready(now, seq);
        let pick = ready.iter().find(|a| self.peers[*a].range.is_some()).or(ready.first()).cloned();
        if let Some(addr) = pick {
            if self.send(&addr, Ask::Base(want), frame(&peer_state::base_request(&want)), now) {
                self.t.req_base += 1;
            }
        }
    }

    fn ask_state(&mut self, now: Instant) {
        let (Some(target), Some(seq)) = (self.target, self.target_seq()) else { return };
        for addr in self.ready(now, Some(seq)) {
            let Some(b) = self.fetch.as_mut().and_then(StateTreeFetch::next_batch) else { break };
            if self.send(&addr, Ask::State(b.ticket), frame(&b.request(&target)), now) {
                self.t.req_state += 1;
            } else if let Some(f) = self.fetch.as_mut() {
                f.release(b.ticket);
            }
        }
    }

    fn drop_out_of_range(&mut self, now: Instant) {
        let Some(seq) = self.target_seq() else { return };
        let out: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, s)| {
                s.asked.is_none()
                    && now.duration_since(s.up_at) >= OUT_OF_RANGE_GRACE
                    && s.range.is_some_and(|(lo, hi)| seq < lo || seq > hi)
            })
            .map(|(a, _)| a.clone())
            .collect();
        for addr in out {
            let r = self.peers[&addr].range;
            self.t.dropped_range += 1;
            self.drop_peer(&addr, &format!("its validated range {r:?} does not hold #{seq}"), false, now);
        }
    }

    fn check_state_end(&mut self, now: Instant) {
        let Some(f) = self.fetch.as_ref() else { return };
        if let Some(e) = &self.io_err {
            self.phase = Phase::Done(format!("writing leaves.bin: {e}"));
        } else if f.is_complete() {
            self.phase = Phase::Done("complete".into());
        } else if self.args.limit.is_some_and(|l| f.counts.nodes() >= l) {
            self.phase = Phase::Done(format!("--limit {} reached", self.args.limit.unwrap_or(0)));
        } else if now.duration_since(self.last_accept) >= STALL {
            self.phase = Phase::Done(format!("no node accepted for {} s", STALL.as_secs()));
        }
    }

    fn progress(&mut self, now: Instant) {
        let up = self.peers.len();
        let head = format!("[{} {}]", hms(now.duration_since(self.started)), match self.phase {
            Phase::Target => "target",
            Phase::Header { .. } => "header",
            Phase::State => "state",
            Phase::Done(_) => "done",
        });
        match self.fetch.as_ref() {
            Some(f) => {
                let c = f.counts;
                let rate = c.nodes().saturating_sub(self.last_nodes) as f64 / PROGRESS_EVERY.as_secs_f64();
                self.last_nodes = c.nodes();
                eprintln!(
                    "{head} nodes {} (inner {} leaf {}) | wanted {} out {} stubborn {} | {:.0}/s | req {} rep {} full {} t/o {} | peers {up} up {} used {} dialing {} known | in {} | rss {:.2} GB",
                    n(c.nodes()), n(c.inner), n(c.leaves), n(f.wanted() as u64), n(f.in_flight() as u64), f.stubborn(), rate,
                    n(self.t.req_state), n(self.t.replies_state), self.t.full_replies, self.t.timeouts, self.used.len(),
                    self.dialing.len(), self.known.len(), gb(self.bytes_in.load(Ordering::Relaxed)), rss_gb()
                );
            }
            None => eprintln!(
                "{head} peers {up} up {} dialing {} known | validations {} (bad sig {}) | quorum-validated {} | in {}",
                self.dialing.len(),
                self.known.len(),
                self.t.validations,
                self.t.bad_sigs,
                self.validated.keys().next_back().map_or("none yet".into(), |s| format!("#{s}")),
                gb(self.bytes_in.load(Ordering::Relaxed))
            ),
        }
    }

    fn save_checkpoint(&mut self) -> std::io::Result<()> {
        let (Some(f), Some(leaves), Some(h)) = (self.fetch.as_ref(), self.leaves.as_mut(), self.header.as_ref()) else { return Ok(()) };
        let len = leaves.sync()?;
        let path = self.args.out.join("checkpoint.bin");
        Checkpoint { header: h.serialize(), leaves_len: len, counts: f.counts, frontier: f.frontier() }.save(&path)
    }

    async fn run(&mut self, mut rx: mpsc::Receiver<Ev>) {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = rx.recv() => if let Some(ev) = ev { self.on_event(ev, Instant::now()) },
                _ = tick.tick() => {}
                _ = sigint.recv() => self.phase = Phase::Done("interrupted (SIGINT)".into()),
                _ = sigterm.recv() => self.phase = Phase::Done("interrupted (SIGTERM)".into()),
            }
            if matches!(self.phase, Phase::Done(_)) {
                break;
            }
            self.housekeeping(Instant::now());
            if matches!(self.phase, Phase::Done(_)) {
                break;
            }
        }
        self.progress(Instant::now());
    }

    /// Close the sessions, save the checkpoint, recheck the file, ask the witness, print the summary. The exit code.
    async fn finish(&mut self) -> i32 {
        let reason = match &self.phase {
            Phase::Done(r) => r.clone(),
            _ => "ended".into(),
        };
        let connected_at_end = self.peers.len();
        self.peers.clear();
        let complete = self.fetch.as_ref().is_some_and(StateTreeFetch::is_complete);
        if self.fetch.is_some() {
            match self.save_checkpoint() {
                Ok(()) => eprintln!("[checkpoint] saved: {}", self.checkpoint_path().display()),
                Err(e) => eprintln!("[checkpoint] failed: {e}"),
            }
        }
        let counts = self.fetch.as_ref().map(|f| f.counts).unwrap_or_default();
        let file_len = self.leaves.as_ref().map_or(0, LeafFile::len);
        let account_hash = self.header.as_ref().map(|h| h.account_hash.0);

        let recheck = if complete && self.args.recheck {
            eprintln!("[recheck] recomputing the root bottom-up from {} ...", self.leaves_path().display());
            let path = self.leaves_path();
            let want = counts.leaves;
            Some(tokio::task::spawn_blocking(move || peer_state::recheck(&path, want)).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string())))
        } else {
            None
        };

        let witness = match (&self.args.witness, &self.header, self.target) {
            (Some(url), Some(h), Some(hash)) => Some(witness(url, h.sequence, &hash, &h.account_hash.0).await),
            _ => None,
        };

        let mut table: Vec<(&String, &PeerStats)> = self.per_peer.iter().filter(|(_, p)| p.replies + p.timeouts > 0).collect();
        table.sort_by(|a, b| b.1.nodes.cmp(&a.1.nodes).then(a.0.cmp(b.0)));
        eprintln!("[peers] {:<24} {:>4} {:>8} {:>11} {:>9} {:>9} {:>10} {:>8} {:>5}  last close", "address", "sess", "replies", "nodes", "GB", "max nodes", "max MB", "avg ms", "t/o");
        for (a, p) in &table {
            eprintln!(
                "[peers] {:<24} {:>4} {:>8} {:>11} {:>9.3} {:>9} {:>10.2} {:>8} {:>5}  {}",
                a, p.sessions, p.replies, n(p.nodes), p.bytes as f64 / 1e9, p.max_nodes, p.max_bytes as f64 / 1e6,
                p.latency_ms / p.replies.max(1), p.timeouts, p.last_close
            );
        }
        let el = self.started.elapsed();
        let st = self.state_started.map(|t| t.elapsed());
        println!("==== PEER STATE SYNC SUMMARY ====");
        match (&self.header, self.target) {
            (Some(h), Some(hash)) => {
                println!("ledger        #{} {}", h.sequence, hexu(&hash));
                println!("target        {}", self.target_how);
                println!("account_hash  {}", hexu(&h.account_hash.0));
            }
            _ => println!("ledger        none proven"),
        }
        println!(
            "nodes         {} fetched (inner {}, leaf {}); {} rejected, {} unasked, {} misses{}",
            n(counts.nodes()),
            n(counts.inner),
            n(counts.leaves),
            counts.rejected,
            n(counts.unasked),
            n(counts.misses),
            if self.resumed_nodes > 0 { format!("; {} of them before this run (resumed)", n(self.resumed_nodes)) } else { String::new() }
        );
        if let Some(f) = &self.fetch {
            println!("still wanted  {} positions", n(f.wanted() as u64));
        }
        println!("state objects {} ({} of objects; leaves.bin {})", n(counts.leaves), gb(counts.leaf_bytes), gb(file_len));
        println!(
            "bytes         {} received from peers, {} of it state nodes",
            gb(self.bytes_in.load(Ordering::Relaxed)),
            gb(self.t.state_bytes)
        );
        println!(
            "peers         {} used (answered state requests), {} connected in all ({} at the end), {} dialed, {} handshakes failed ({} full/503); {} addresses known from {} TMEndpoints",
            self.used.len(),
            self.t.connected,
            connected_at_end,
            self.t.dialed,
            self.t.handshake_failed,
            self.t.handshake_full,
            self.known.len(),
            self.t.endpoints_msgs
        );
        println!(
            "              dropped: {} stalled, {} sent bad data, {} out of range; {} closed by the peer ({} with a request out)",
            self.t.dropped_stalled, self.t.dropped_bad, self.t.dropped_range, self.t.closed, self.t.closed_while_asked
        );
        println!(
            "requests      {} state + {} header{}; {} state replies ({} at the {}-node cap), {} timeouts, {} peer errors",
            n(self.t.req_state),
            self.t.req_base,
            if self.t.walk_steps > 0 { format!(" ({} walked back)", self.t.walk_steps) } else { String::new() },
            n(self.t.replies_state),
            self.t.full_replies,
            SOFT_MAX_REPLY_NODES,
            self.t.timeouts,
            self.t.peer_errors
        );
        println!("elapsed       {}{}", hms(el), st.map_or(String::new(), |s| format!(" (state {})", hms(s))));
        println!("rss           {:.2} GB at the end", rss_gb());
        let mut proven = complete && account_hash.is_some();
        match &recheck {
            Some(Ok(r)) => {
                let ok = Some(r.root) == account_hash && r.leaves == counts.leaves && r.repeated == 0;
                proven &= ok;
                println!(
                    "recheck       leaves.bin: {} leaves, {} repeated, root {} {}",
                    n(r.leaves),
                    r.repeated,
                    hexu(&r.root),
                    if ok { "= account_hash" } else { "!= account_hash (or counts differ)" }
                );
            }
            Some(Err(e)) => {
                proven = false;
                println!("recheck       failed: {e}");
            }
            None if complete => println!("recheck       skipped (--no-recheck): proven top-down only"),
            None => println!("recheck       not run (the tree is not complete)"),
        }
        match &witness {
            Some(w) => println!("witness .39   {w}"),
            None => println!("witness .39   not asked"),
        }
        println!("stopped       {reason}");
        match (proven, account_hash) {
            (true, Some(a)) => {
                println!("ROOT PROVEN: {}", hexu(&a));
                let h = self.header.as_ref().expect("header");
                let marker = json!({
                    "seq": h.sequence, "ledger_hash": self.target.map(|t| hexu(&t)), "account_hash": hexu(&a),
                    "objects": counts.leaves, "inner": counts.inner, "leaves_bin_bytes": file_len,
                    "elapsed_s": el.as_secs(), "peers_used": self.used.len(),
                });
                let _ = std::fs::write(self.args.out.join("proven.json"), format!("{marker:#}\n"));
                0
            }
            _ => {
                println!("ROOT NOT PROVEN ({})", if complete { "the recheck failed" } else { reason.as_str() });
                i32::from(!reason.starts_with("--limit"))
            }
        }
    }
}

/// .39's copy of the same ledger's hashes, said in words. Read-only, once; never used.
async fn witness(url: &str, seq: u32, hash: &[u8; 32], account_hash: &[u8; 32]) -> String {
    let c = match reqwest::Client::builder().timeout(Duration::from_secs(15)).build() {
        Ok(c) => c,
        Err(e) => return format!("unavailable ({e})"),
    };
    let body = json!({"method": "ledger", "params": [{"ledger_index": seq}]});
    let r: Value = match c.post(url).json(&body).send().await {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => return format!("unavailable ({e})"),
        },
        Err(e) => return format!("unavailable ({e})"),
    };
    let lh = r["result"]["ledger_hash"].as_str().or(r["result"]["ledger"]["ledger_hash"].as_str()).unwrap_or("");
    let ah = r["result"]["ledger"]["account_hash"].as_str().unwrap_or("");
    if lh.is_empty() {
        return format!("unavailable ({})", r["result"]["error"].as_str().unwrap_or("no ledger_hash"));
    }
    let same = |a: &str, b: &[u8; 32]| if a.eq_ignore_ascii_case(&hex::encode(b)) { "SAME" } else { "DIFFERENT" };
    format!("#{seq}: ledger_hash {}, account_hash {} ({url})", same(lh, hash), same(ah, account_hash))
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&args.out) {
        eprintln!("{}: {e}", args.out.display());
        std::process::exit(2);
    }
    // The validator list's last-good record goes beside our output, not under /mnt/xrpl-data; and never an
    // unverified list.
    if std::env::var_os("XRPL_UNL_STATE").is_none() {
        std::env::set_var("XRPL_UNL_STATE", args.out.join("unl-last-good.json"));
    }
    if std::env::var_os("XRPL_UNL_ENFORCE").is_none() {
        std::env::set_var("XRPL_UNL_ENFORCE", "1");
    }
    let identity = match NodeIdentity::generate() {
        Ok(i) => Arc::new(i),
        Err(e) => {
            eprintln!("node identity: {e}");
            std::process::exit(2);
        }
    };
    let (ev_tx, rx) = mpsc::channel::<Ev>(64);
    let now = Instant::now();
    let mut run = Run {
        identity,
        ev_tx,
        bytes_in: Arc::new(AtomicU64::new(0)),
        want_validations: Arc::new(AtomicBool::new(false)),
        peers: HashMap::new(),
        candidates: VecDeque::new(),
        known: HashSet::new(),
        dialing: HashSet::new(),
        cooldown: HashMap::new(),
        banned: HashSet::new(),
        used: HashSet::new(),
        per_peer: HashMap::new(),
        t: Tallies::default(),
        phase: Phase::Target,
        tally: Tally::default(),
        validated: BTreeMap::new(),
        quorum_seen: (0, 0),
        header: None,
        target: None,
        target_how: String::new(),
        fetch: None,
        leaves: None,
        io_err: None,
        started: now,
        state_started: None,
        resumed_nodes: 0,
        last_progress: now,
        last_checkpoint: now,
        last_accept: now,
        last_nodes: 0,
        args,
    };
    for b in run.args.bootstrap.clone() {
        run.known.insert(b.clone());
        run.queue(b);
    }
    eprintln!(
        "[start] out {} | max peers {} | gap {} ms | timeout {} s | limit {} | witness {}",
        run.args.out.display(),
        run.args.max_peers,
        run.args.gap.as_millis(),
        run.args.timeout.as_secs(),
        run.args.limit.map_or("none".into(), n),
        run.args.witness.as_deref().unwrap_or("none")
    );

    // Resume from a checkpoint in --out, unless --fresh.
    let cp_path = run.checkpoint_path();
    if run.args.fresh {
        let _ = std::fs::remove_file(&cp_path);
    }
    match Checkpoint::load(&cp_path) {
        Ok(Some(c)) => {
            let h = match LedgerHeader::deserialize(&c.header) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("{}: a header that does not parse ({e}); pass --fresh", cp_path.display());
                    std::process::exit(2);
                }
            };
            let hash = h.hash().0;
            if run.args.ledger_hash.is_some_and(|x| x != hash) || run.args.seq.is_some_and(|s| s != h.sequence) {
                eprintln!("{} is for #{} {}; pass --fresh to start over", cp_path.display(), h.sequence, hexu(&hash));
                std::process::exit(2);
            }
            let leaves = match LeafFile::open(&run.leaves_path(), c.leaves_len) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("{}: {e}; pass --fresh", run.leaves_path().display());
                    std::process::exit(2);
                }
            };
            eprintln!(
                "[resume] #{} {}: {} nodes done, {} positions wanted, leaves.bin cut to {}",
                h.sequence,
                hexu(&hash),
                n(c.counts.nodes()),
                n(c.frontier.len() as u64),
                gb(c.leaves_len)
            );
            run.resumed_nodes = c.counts.nodes();
            run.last_nodes = c.counts.nodes();
            run.fetch = Some(StateTreeFetch::resume(h.account_hash.0, &c.frontier, c.counts));
            run.leaves = Some(leaves);
            run.target = Some(hash);
            run.header = Some(h);
            run.target_how = "resumed from the checkpoint".into();
            run.state_started = Some(now);
            run.phase = Phase::State;
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("{e}; pass --fresh");
            std::process::exit(2);
        }
    }

    if matches!(run.phase, Phase::Target) {
        if let Some(h) = run.args.ledger_hash {
            run.target_how = "--ledger-hash (as given)".into();
            run.phase = Phase::Header { want: h, seq: run.args.seq };
        } else {
            match xrpl_node::unl_fetch::fetch_default_unl_verified().await {
                Ok(entries) => {
                    let e: Vec<(String, Option<String>)> =
                        entries.iter().map(|e| (e.public_key.clone(), e.signing_key.clone())).collect();
                    run.tally.set_unl(&e);
                    eprintln!("[target] {} trusted validators, quorum {}", run.tally.unl_size(), run.tally.quorum());
                    run.want_validations.store(true, Ordering::Relaxed);
                }
                Err(e) => {
                    eprintln!("validator list: {e} (pass --ledger-hash to name the ledger)");
                    std::process::exit(2);
                }
            }
        }
    }
    run.run(rx).await;
    let code = run.finish().await;
    std::process::exit(code);
}
