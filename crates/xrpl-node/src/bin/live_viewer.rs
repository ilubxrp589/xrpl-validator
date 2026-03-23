//! Live XRPL peer message viewer — connects to testnet and streams
//! decoded messages to a web browser via Server-Sent Events.
//!
//! Run: cargo run -p xrpl-node --bin live_viewer
//! Open: http://localhost:3777

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use bytes::BytesMut;
use futures::stream::Stream;
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_util::codec::Decoder;

use xrpl_node::peer::codec::MessageCodec;
use xrpl_node::peer::handshake;
use xrpl_node::peer::identity::NodeIdentity;
use xrpl_node::peer::message::PeerMessage;

const MAINNET_PEERS: &[&str] = &[
    "s1.ripple.com:51235",
    "s2.ripple.com:51235",
];

#[derive(Clone, serde::Serialize)]
struct MessageEvent {
    seq: u64,
    time: f64,
    msg_type: String,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ledger_seq: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peers: Option<Vec<PeerEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_info: Option<TxInfo>,
}

#[derive(Clone, serde::Serialize)]
struct TxInfo {
    account: String,
    fee: u64,
    sequence: u32,
    hash: String,
    sig_verified: bool,
    tx_type: String,
}

#[derive(Clone, serde::Serialize)]
struct PeerEntry {
    endpoint: String,
    hops: u32,
}

/// Original XRPL supply: 100 billion XRP in drops.
const ORIGINAL_SUPPLY_DROPS: u64 = 100_000_000_000_000_000;

/// Path for persisted engine stats.
const ENGINE_STATE_PATH: &str = "/mnt/xrpl-data/engine_state.json";

/// Persistent stats saved to disk.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct PersistedStats {
    total_txs: u64,
    total_fees_burned: u64,
    ledgers_processed: u32,
    last_ledger_seq: u32,
}

impl PersistedStats {
    fn load() -> Self {
        std::fs::read_to_string(ENGINE_STATE_PATH)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(ENGINE_STATE_PATH, json);
        }
    }
}

/// Consensus engine state — tracks per-round transaction stats.
#[derive(Clone, Default, serde::Serialize)]
struct EngineState {
    /// Current ledger sequence being tracked.
    ledger_seq: u32,
    /// Transactions seen in the current round.
    round_tx_count: u32,
    /// Total fees accumulated in the current round (drops).
    round_fees: u64,
    /// Transaction type counts for the current round.
    round_tx_types: std::collections::HashMap<String, u32>,
    /// Total transactions processed since startup.
    total_txs: u64,
    /// Total fees burned since startup (drops).
    total_fees_burned: u64,
    /// Ledgers processed since startup.
    ledgers_processed: u32,
    /// Supported tx type count in current round.
    round_supported: u32,
    /// Unsupported tx type count in current round.
    round_unsupported: u32,
    /// Current total_coins from the network (drops). 0 if not yet fetched.
    network_total_coins: u64,
    /// All-time XRP burned (original supply - current total_coins) in drops.
    lifetime_burned: u64,
    /// Total messages received since startup.
    total_messages: u64,
    /// Total validations received since startup.
    total_validations: u64,
    /// Total proposals received since startup.
    total_proposals: u64,
    /// Validations in current round.
    round_validations: u32,
    /// Proposals in current round.
    round_proposals: u32,
    /// Signature verified count.
    sig_ok: u64,
    /// Signature failed count.
    sig_fail: u64,
    /// Uptime start (unix millis).
    start_time_ms: u64,
}

#[tokio::main]
async fn main() {
    eprintln!("Starting XRPL Live Viewer...");

    // Init rustls crypto provider
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (tx, _) = broadcast::channel::<MessageEvent>(1000);
    let tx2 = tx.clone();

    // Message dedup: hash of recent messages to avoid duplicates from multiple peers
    let seen_msgs: Arc<dashmap::DashMap<u64, ()>> = Arc::new(dashmap::DashMap::new());

    // Periodic cleanup of seen messages (every 30s, clear all)
    let seen_cleanup = seen_msgs.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            seen_cleanup.clear();
        }
    });

    // Shared set of peers we've already connected/tried
    let known_peers: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let connected_count: Arc<std::sync::atomic::AtomicUsize> = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Track which peers are currently connected (for the peers page)
    let connected_peers: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let max_peers: usize = 1000;

    // Seed with initial peers
    for peer in MAINNET_PEERS {
        known_peers.lock().insert(peer.to_string());
    }

    // Spawn connections to seed peers
    for peer in MAINNET_PEERS {
        spawn_peer_connection(
            peer.to_string(),
            tx2.clone(),
            known_peers.clone(),
            connected_count.clone(),
            max_peers,
            seen_msgs.clone(),
            connected_peers.clone(),
        );
    }

    // Live engine — opens sled DB and applies transactions to real state
    // Disabled while sync is running (sled lock conflict)
    let sync_running = std::fs::metadata("/mnt/xrpl-data/sync/state.sled/db")
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e.as_secs() < 60).unwrap_or(false))
        .unwrap_or(false);

    let live_engine: Arc<Mutex<Option<xrpl_node::engine::LiveEngine>>> = if std::env::var("XRPL_NO_ENGINE").is_ok() {
        eprintln!("[live-engine] Disabled via XRPL_NO_ENGINE env var");
        Arc::new(Mutex::new(None))
    } else {
        Arc::new(Mutex::new(
            match xrpl_node::engine::LiveEngine::open(std::path::Path::new("/mnt/xrpl-data/sync/state.sled")) {
                Ok(e) => {
                    eprintln!("[live-engine] Opened sled with {} entries", e.entry_count());
                    Some(e)
                }
                Err(e) => {
                    eprintln!("[live-engine] Failed to open sled: {e} (sync running?)");
                    None
                }
            }
        ))
    };

    // Engine state — load persisted stats, then track consensus round stats
    let persisted = PersistedStats::load();
    eprintln!("[engine] Loaded persisted stats: {} txs, {} ledgers, {} fees burned",
        persisted.total_txs, persisted.ledgers_processed, persisted.total_fees_burned);
    let mut initial_state = EngineState::default();
    initial_state.total_txs = persisted.total_txs;
    initial_state.total_fees_burned = persisted.total_fees_burned;
    initial_state.ledgers_processed = persisted.ledgers_processed;
    initial_state.ledger_seq = persisted.last_ledger_seq;
    initial_state.start_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let engine_state: Arc<Mutex<EngineState>> = Arc::new(Mutex::new(initial_state));

    // Background task: poll total_coins from network every 30s
    let coins_engine = engine_state.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        loop {
            if let Ok(resp) = client
                .post("https://xrplcluster.com")
                .json(&serde_json::json!({
                    "method": "ledger",
                    "params": [{"ledger_index": "validated"}]
                }))
                .send()
                .await
            {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(coins_str) = body["result"]["ledger"]["total_coins"].as_str() {
                        if let Ok(coins) = coins_str.parse::<u64>() {
                            let mut state = coins_engine.lock();
                            state.network_total_coins = coins;
                            state.lifetime_burned = ORIGINAL_SUPPLY_DROPS.saturating_sub(coins);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    // Persist engine stats to disk every 60s
    let persist_engine = engine_state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let state = persist_engine.lock().clone();
            let stats = PersistedStats {
                total_txs: state.total_txs + state.round_tx_count as u64,
                total_fees_burned: state.total_fees_burned + state.round_fees,
                ledgers_processed: state.ledgers_processed,
                last_ledger_seq: state.ledger_seq,
            };
            stats.save();
        }
    });

    // Subscribe to message events for engine tracking
    let engine_rx = tx2.subscribe();
    let engine = engine_state.clone();
    let le = live_engine.clone();
    tokio::spawn(async move {
        let mut rx = engine_rx;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let mut state = engine.lock();

                    // Track ledger sequence changes (new round)
                    if let Some(seq) = event.ledger_seq {
                        if seq > state.ledger_seq {
                            // New ledger — finalize previous round
                            if state.ledger_seq > 0 {
                                state.total_txs += state.round_tx_count as u64;
                                state.total_fees_burned += state.round_fees;
                                state.ledgers_processed += 1;
                            }
                            // Reset round stats
                            state.ledger_seq = seq;
                            state.round_tx_count = 0;
                            state.round_fees = 0;
                            state.round_tx_types.clear();
                            state.round_supported = 0;
                            state.round_unsupported = 0;
                            state.round_validations = 0;
                            state.round_proposals = 0;
                        }
                    }

                    // Count all messages
                    state.total_messages += 1;

                    // Track by type
                    if event.msg_type == "Validation" {
                        state.total_validations += 1;
                        state.round_validations += 1;
                    } else if event.msg_type == "Proposal" {
                        state.total_proposals += 1;
                        state.round_proposals += 1;
                    } else if event.msg_type == "Transaction" {
                        if let Some(ref info) = event.tx_info {
                            state.round_tx_count += 1;
                            state.round_fees += info.fee;
                            *state.round_tx_types.entry(info.tx_type.clone()).or_insert(0) += 1;

                            if info.sig_verified { state.sig_ok += 1; } else { state.sig_fail += 1; }

                            if xrpl_ledger::tx::dispatch::is_supported(&info.tx_type) {
                                state.round_supported += 1;
                            } else {
                                state.round_unsupported += 1;
                            }

                            // Apply to live state via sled
                            if let Some(ref mut eng) = *le.lock() {
                                if let Some(id) = xrpl_node::engine::decode_address(&info.account) {
                                    eng.apply_transaction(
                                        &info.tx_type, &id, info.fee,
                                        &serde_json::Value::Null,
                                    );
                                }
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    // Peer info cache — crawled versions/names
    let peer_info_cache: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Background: crawl connected peers for version info
    let crawl_cache = peer_info_cache.clone();
    let crawl_active = connected_peers.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap_or_default();
        loop {
            let peers: Vec<String> = crawl_active.lock().iter().cloned().collect();
            for addr in peers {
                if crawl_cache.lock().contains_key(&addr) {
                    continue;
                }
                let ip = addr.split(':').next().unwrap_or(&addr);
                let port = addr.split(':').nth(1).unwrap_or("51235");
                let url = format!("https://{}:{}/crawl", ip, port);
                if let Ok(resp) = client.get(&url).send().await {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        let version = body.get("server")
                            .and_then(|s| s.get("build_version"))
                            .and_then(|v| v.as_str())
                            .map(|v| format!("rippled-{}", v))
                            .or_else(|| {
                                body.get("overlay")
                                    .and_then(|o| o.get("active"))
                                    .and_then(|a| a.as_array())
                                    .and_then(|a| a.first())
                                    .and_then(|p| p.get("version"))
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                            });
                        if let Some(v) = version {
                            crawl_cache.lock().insert(addr.clone(), v);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    // Share peer state with web handlers
    let web_known = known_peers.clone();
    let web_connected = connected_count.clone();
    let web_active = connected_peers.clone();
    let web_peer_info = peer_info_cache.clone();

    // Web server
    let app = Router::new()
        .route("/", get(index_page))
        .route("/consensus", get(consensus_page))
        .route("/metrics", get(metrics_page))
        .route("/peers", get(peers_page))
        .route("/events", get(move || sse_handler(tx.clone())))
        .route("/api/sync-status", get(sync_status))
        .route("/api/engine", get({
            let engine_web = engine_state.clone();
            let le_web = live_engine.clone();
            move || {
                let engine_web = engine_web.clone();
                let le_web = le_web.clone();
                async move {
                    let state = engine_web.lock().clone();
                    let mut json = serde_json::to_value(&state).unwrap_or_default();
                    // Add live engine stats
                    if let Some(ref eng) = *le_web.lock() {
                        json["live_engine"] = serde_json::json!({
                            "active": true,
                            "sled_entries": eng.entry_count(),
                            "round_applied": eng.round_applied,
                            "round_failed": eng.round_failed,
                            "total_applied": eng.total_applied,
                            "total_coins": eng.total_coins,
                        });
                    } else {
                        json["live_engine"] = serde_json::json!({"active": false});
                    }
                    axum::Json(json)
                }
            }
        }))
        .route("/api/peers", get(move || async move {
            let connected_count = web_connected.load(std::sync::atomic::Ordering::Relaxed);
            let active: Vec<String> = web_active.lock().iter().cloned().collect();
            let guard = web_known.lock();
            let known = guard.len();
            let all_known: Vec<String> = guard.iter().cloned().collect();
            drop(guard);
            let active_set: std::collections::HashSet<&str> = active.iter().map(|s| s.as_str()).collect();
            let discovered: Vec<&str> = all_known.iter()
                .filter(|p| !active_set.contains(p.as_str()))
                .map(|s| s.as_str())
                .collect();
            let info = web_peer_info.lock();
            let peer_details: Vec<serde_json::Value> = active.iter().map(|addr| {
                let version = info.get(addr).cloned().unwrap_or_default();
                serde_json::json!({"addr": addr, "version": version})
            }).collect();
            drop(info);
            axum::Json(serde_json::json!({
                "connected": connected_count,
                "known": known,
                "max": max_peers,
                "peers": active,
                "peer_details": peer_details,
                "discovered": discovered,
            }))
        }));

    let addr = "0.0.0.0:3777";
    eprintln!("[web] Listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("failed to bind port 3777");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("[web] Server error: {e}");
    }
}

fn spawn_peer_connection(
    peer: String,
    tx: broadcast::Sender<MessageEvent>,
    known_peers: Arc<Mutex<HashSet<String>>>,
    connected_count: Arc<std::sync::atomic::AtomicUsize>,
    max_peers: usize,
    seen_msgs: Arc<dashmap::DashMap<u64, ()>>,
    active_peers: Arc<Mutex<HashSet<String>>>,
) {
    tokio::spawn(async move {
        loop {
            let current = connected_count.load(std::sync::atomic::Ordering::Relaxed);
            if current >= max_peers {
                return;
            }
            connected_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            active_peers.lock().insert(peer.clone());
            eprintln!("[peer] Connecting to {peer}...");
            match run_peer_connection(&peer, &tx, &known_peers, &connected_count, max_peers, &seen_msgs, &active_peers).await {
                Ok(()) => eprintln!("[peer] {peer} disconnected"),
                Err(e) => eprintln!("[peer] {peer}: {e}"),
            }
            active_peers.lock().remove(&peer);
            connected_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

async fn run_peer_connection(
    peer_addr: &str,
    tx: &broadcast::Sender<MessageEvent>,
    known_peers: &Arc<Mutex<HashSet<String>>>,
    connected_count: &Arc<std::sync::atomic::AtomicUsize>,
    max_peers: usize,
    seen_msgs: &Arc<dashmap::DashMap<u64, ()>>,
    active_peers: &Arc<Mutex<HashSet<String>>>,
) -> Result<(), String> {
    let identity = NodeIdentity::generate().map_err(|e| format!("identity: {e}"))?;

    let mut hs = timeout(
        Duration::from_secs(15),
        handshake::outbound_handshake(peer_addr, &identity, handshake::NETWORK_ID_MAINNET),
    )
    .await
    .map_err(|_| "handshake timeout".to_string())?
    .map_err(|e| format!("handshake: {e}"))?;

    eprintln!("[peer] Handshake with {peer_addr} successful!");

    let _ = tx.send(MessageEvent {
        seq: 0,
        time: 0.0,
        msg_type: "CONNECTED".into(),
        detail: format!("Handshake with {peer_addr}"),
        ledger_seq: None,
        peers: None,
        validator: None,
        tx_info: None,
    });

    let mut codec = MessageCodec;
    let mut buf = BytesMut::with_capacity(65536);
    buf.extend_from_slice(&hs.remaining_bytes);

    let mut msg_seq: u64 = 0;
    let start = Instant::now();

    loop {
        // Decode from buffer
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(msg)) => {
                    msg_seq += 1;
                    let elapsed = start.elapsed().as_secs_f64();

                    // Auto-discover peers from Endpoints messages
                    if let PeerMessage::Endpoints(ref ep) = msg {
                        let current = connected_count.load(std::sync::atomic::Ordering::Relaxed);
                        if current < max_peers {
                            for peer_ep in &ep.endpoints_v2 {
                                if peer_ep.hops <= 3 {
                                    let addr = peer_ep.endpoint.clone();
                                    // Skip invalid/local addresses
                                    if addr.starts_with("[::") || addr.starts_with("0.") || addr.starts_with("127.") || addr.starts_with("10.") || addr.starts_with("192.168.") {
                                        continue;
                                    }
                                    let mut known = known_peers.lock();
                                    if !known.contains(&addr) {
                                        known.insert(addr.clone());
                                        drop(known);
                                        eprintln!("[discovery] Found new peer: {addr}");
                                        spawn_peer_connection(
                                            addr,
                                            tx.clone(),
                                            known_peers.clone(),
                                            connected_count.clone(),
                                            max_peers,
                                            seen_msgs.clone(),
                                            active_peers.clone(),
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Dedup: hash the raw message bytes to skip duplicates from multiple peers
                    let msg_hash = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        // Hash the protobuf bytes for uniqueness
                        msg.encode_to_vec().hash(&mut h);
                        h.finish()
                    };
                    if seen_msgs.contains_key(&msg_hash) {
                        continue; // duplicate from another peer, skip
                    }
                    seen_msgs.insert(msg_hash, ());

                    let (msg_type, detail, ledger_seq, peers, validator, tx_info) = format_message(&msg);

                    let event = MessageEvent {
                        seq: msg_seq,
                        time: elapsed,
                        msg_type,
                        detail,
                        ledger_seq,
                        peers,
                        validator,
                        tx_info,
                    };

                    let _ = tx.send(event);
                    continue;
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[peer] Codec error: {e}");
                    return Err(format!("codec: {e}"));
                }
            }
        }

        // Read more from stream
        let mut chunk = vec![0u8; 32768];
        match timeout(Duration::from_secs(30), hs.stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => return Err(format!("read: {e}")),
            Err(_) => continue,
        }
    }
}

/// Returns (msg_type, detail, ledger_seq, peers, validator_key, tx_info)
#[allow(clippy::type_complexity)]
fn format_message(msg: &PeerMessage) -> (String, String, Option<u32>, Option<Vec<PeerEntry>>, Option<String>, Option<TxInfo>) {
    match msg {
        PeerMessage::StatusChange(sc) => {
            let status = sc.new_status.map(|s| match s {
                1 => "CONNECTING",
                2 => "CONNECTED",
                3 => "MONITORING",
                4 => "VALIDATING",
                5 => "SHUTTING",
                _ => "?",
            }).unwrap_or("?");
            let event = sc.new_event.map(|e| match e {
                1 => "CLOSING",
                2 => "ACCEPTED",
                3 => "SWITCHED",
                4 => "LOST_SYNC",
                _ => "?",
            }).unwrap_or("?");
            (
                "StatusChange".into(),
                format!("status={status} event={event}"),
                sc.ledger_seq,
                None,
                None,
                None,
            )
        }
        PeerMessage::Validation(v) => {
            // Extract validator key from serialized validation blob.
            // The blob is an XRPL serialized object. The SigningPubKey field
            // (field code 0x73) is typically at a known offset. We scan for
            // the 33-byte key prefix (0x02/0x03 for secp256k1, 0xED for ed25519).
            let vkey = extract_key_from_validation(&v.validation);
            let detail = format!("{}B", v.validation.len());
            ("Validation".into(), detail, None, None, vkey, None)
        }
        PeerMessage::Manifests(m) => {
            ("Manifests".into(), format!("{} manifests", m.list.len()), None, None, None, None)
        }
        PeerMessage::ValidatorListCollection(_) => {
            ("ValidatorList".into(), "UNL bootstrap".into(), None, None, None, None)
        }
        PeerMessage::Transaction(t) => {
            use xrpl_node::mempool::{validate_transaction, ValidationResult};

            let (result, tx_hash, fields) = validate_transaction(&t.raw_transaction);
            let sig_ok = matches!(result, ValidationResult::Valid);

            let tx_info = fields.map(|f| {
                // Try to get TransactionType from decoded blob
                let tx_type = xrpl_core::codec::decode_transaction_binary(&t.raw_transaction)
                    .ok()
                    .and_then(|v| v.get("TransactionType").and_then(|t| t.as_str()).map(String::from))
                    .unwrap_or_else(|| "?".into());

                TxInfo {
                    account: f.account.clone(),
                    fee: f.fee,
                    sequence: f.sequence,
                    hash: hex::encode(&tx_hash.0[..8]),
                    sig_verified: sig_ok,
                    tx_type,
                }
            });

            let detail = if let Some(ref info) = tx_info {
                let sig_icon = if info.sig_verified { "OK" } else { "FAIL" };
                format!("{} from {} fee={} sig={sig_icon}", info.tx_type, &info.account[..8], info.fee)
            } else {
                format!("{}B undecoded", t.raw_transaction.len())
            };

            ("Transaction".into(), detail, None, None, None, tx_info)
        }
        PeerMessage::ProposeSet(p) => {
            let hash = hex::encode(&p.current_tx_hash[..std::cmp::min(8, p.current_tx_hash.len())]);
            let vkey = if !p.node_pub_key.is_empty() {
                Some(hex::encode(&p.node_pub_key))
            } else {
                None
            };
            ("Proposal".into(), format!("seq={} hash={hash}...", p.propose_seq), None, None, vkey, None)
        }
        PeerMessage::HaveTransactionSet(h) => {
            let hash = hex::encode(&h.hash[..std::cmp::min(8, h.hash.len())]);
            ("HaveTxSet".into(), format!("hash={hash}..."), None, None, None, None)
        }
        PeerMessage::Endpoints(e) => {
            let peers: Vec<PeerEntry> = e.endpoints_v2.iter()
                .map(|ep| PeerEntry {
                    endpoint: ep.endpoint.clone(),
                    hops: ep.hops,
                })
                .collect();
            let detail = format!("{} peers", peers.len());
            ("Endpoints".into(), detail, None, Some(peers), None, None)
        }
        PeerMessage::GetLedger(g) => {
            ("GetLedger".into(), format!("seq={:?}", g.ledger_seq), None, None, None, None)
        }
        PeerMessage::LedgerData(d) => {
            ("LedgerData".into(), format!("{} nodes seq={}", d.nodes.len(), d.ledger_seq), None, None, None, None)
        }
        PeerMessage::Ping(p) => {
            let kind = if p.r#type == 0 { "ping" } else { "pong" };
            ("Ping".into(), format!("{kind} seq={:?}", p.seq), None, None, None, None)
        }
        other => (other.name().into(), String::new(), None, None, None, None),
    }
}

/// Try to extract a public key from a serialized XRPL validation object.
/// Scans for the SigningPubKey field: type code 0x73 followed by a
/// length byte (0x21 = 33) and a 33-byte compressed public key.
fn extract_key_from_validation(blob: &[u8]) -> Option<String> {
    // Scan for 0x73 0x21 (SigningPubKey, length 33)
    for i in 0..blob.len().saturating_sub(35) {
        if blob[i] == 0x73 && blob[i + 1] == 0x21 {
            let key_start = i + 2;
            let key_end = key_start + 33;
            if key_end <= blob.len() {
                let first_byte = blob[key_start];
                // Valid compressed key prefix: 0x02, 0x03 (secp256k1) or 0xED (ed25519)
                if first_byte == 0x02 || first_byte == 0x03 || first_byte == 0xED {
                    return Some(hex::encode(&blob[key_start..key_end]));
                }
            }
        }
    }
    None
}

async fn sync_status() -> axum::Json<serde_json::Value> {
    let sync_file = "/mnt/xrpl-data/sync/objects.jsonl";
    let estimated_total: u64 = 30_000_000; // ~30M objects on mainnet

    let (objects, size_bytes) = match std::fs::metadata(sync_file) {
        Ok(meta) => {
            let size = meta.len();
            // Estimate line count from file size (avg ~430 bytes per line)
            let lines = size / 430;
            (lines, size)
        }
        Err(_) => (0, 0),
    };

    let pct = if estimated_total > 0 {
        (objects as f64 / estimated_total as f64 * 100.0).min(100.0)
    } else {
        0.0
    };

    let syncing = std::fs::metadata(sync_file)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e.as_secs() < 30).unwrap_or(false))
        .unwrap_or(false);

    axum::Json(serde_json::json!({
        "objects": objects,
        "estimated_total": estimated_total,
        "percent": (pct * 10.0).round() / 10.0,
        "size_mb": size_bytes / 1_048_576,
        "syncing": syncing,
    }))
}

async fn index_page() -> Html<&'static str> {
    Html(include_str!("../../static/viewer.html"))
}

async fn consensus_page() -> Html<&'static str> {
    Html(include_str!("../../static/consensus.html"))
}

async fn metrics_page() -> Html<&'static str> {
    Html(include_str!("../../static/metrics.html"))
}

async fn peers_page() -> Html<&'static str> {
    Html(include_str!("../../static/peers.html"))
}

async fn sse_handler(
    tx: broadcast::Sender<MessageEvent>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = tx.subscribe();
    let stream = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    yield Ok(Event::default().data(
                        format!("{{\"msg_type\":\"LAGGED\",\"detail\":\"skipped {n} messages\",\"seq\":0,\"time\":0}}")
                    ));
                }
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
