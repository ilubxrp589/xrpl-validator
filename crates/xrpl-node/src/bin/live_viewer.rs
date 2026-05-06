//! Live XRPL peer message viewer — connects to testnet and streams
//! decoded messages to a web browser via Server-Sent Events.
//!
//! Run: cargo run -p xrpl-node --bin live_viewer
//! Open: http://localhost:3777

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::collections::HashSet;

// Limit rayon to 8 threads — leave cores for OS, rippled, and peer handling
static _RAYON_INIT: std::sync::LazyLock<()> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new().num_threads(8).build_global().ok();
});
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
    ledger_hash: Option<String>,
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
    #[serde(skip)]
    raw_tx: Vec<u8>,
}

#[derive(Clone, serde::Serialize)]
struct PeerEntry {
    endpoint: String,
    hops: u32,
}

/// Tracks tx_hash agreement across UNL validators.
///
/// For each trusted validator, stores their latest proposed tx_hash.
/// Groups by hash to detect supermajority agreement (threshold for consensus).
#[derive(Default, serde::Serialize, Clone)]
struct ConsensusMonitor {
    /// validator_key_hex → (tx_hash_hex, propose_seq, close_time, last_seen_millis)
    #[serde(skip)]
    positions: std::collections::HashMap<String, (String, u32, u32, u64)>,
    /// Last observed agreement (cached).
    pub last_agreement: Option<Agreement>,
    /// Total proposals received since startup.
    pub total_proposals: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
struct Agreement {
    pub tx_hash: String,
    pub count: usize,
    pub pct: f64,
    pub propose_seq_max: u32,
}

impl ConsensusMonitor {
    fn record(&mut self, validator: String, tx_hash: String, propose_seq: u32, close_time: u32) {
        self.total_proposals += 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        // Keep the HIGHER propose_seq (latest position this round)
        let entry = self.positions.entry(validator).or_insert((tx_hash.clone(), propose_seq, close_time, now));
        if propose_seq >= entry.1 {
            *entry = (tx_hash, propose_seq, close_time, now);
        }
    }

    fn expire_old(&mut self, _current_ledger: u32) {
        // Drop positions older than 15s (network has moved on to next ledger)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.positions.retain(|_, (_, _, _, seen)| now.saturating_sub(*seen) < 15_000);
    }

    fn check_agreement(&mut self, unl_size: usize) -> Option<Agreement> {
        if unl_size == 0 || self.positions.is_empty() {
            self.last_agreement = None;
            return None;
        }
        // Group by tx_hash
        let mut counts: std::collections::HashMap<String, (usize, u32)> = std::collections::HashMap::new();
        for (_, (tx_hash, seq, _, _)) in &self.positions {
            let entry = counts.entry(tx_hash.clone()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = entry.1.max(*seq);
        }
        // Find the highest-count hash
        let best = counts.into_iter().max_by_key(|(_, (c, _))| *c);
        if let Some((hash, (count, seq))) = best {
            let pct = count as f64 / unl_size as f64;
            let agreement = Agreement {
                tx_hash: hash,
                count,
                pct,
                propose_seq_max: seq,
            };
            self.last_agreement = Some(agreement.clone());
            Some(agreement)
        } else {
            self.last_agreement = None;
            None
        }
    }

    fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "tracked_validators": self.positions.len(),
            "total_proposals": self.total_proposals,
            "agreement": self.last_agreement,
        })
    }
}

/// Raw peer proposal data for the consensus engine.
/// Decoded from TmProposeSet. Since added/removed are empty for large sets,
/// we primarily track validators' current_tx_hash for agreement detection.
#[derive(Clone, Debug)]
struct PeerProposal {
    /// Validator's public key (node_pub_key from TmProposeSet).
    validator_key: Vec<u8>,
    /// Proposal round number.
    propose_seq: u32,
    /// Hash identifying the full tx set this validator is proposing.
    current_tx_hash: Vec<u8>,
    /// Close time (ripple epoch seconds).
    close_time: u32,
    /// Hash of the previous ledger this proposal builds on.
    previous_ledger: Vec<u8>,
}

/// Original XRPL supply: 100 billion XRP in drops.
const ORIGINAL_SUPPLY_DROPS: u64 = 100_000_000_000_000_000;

/// Persisted engine stats path. Delegates to the central `paths` helper so
/// the writer (here) and any reader stay in agreement under `XRPL_DATA_DIR`.
fn engine_state_path() -> String {
    xrpl_node::paths::engine_state_path()
}

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
        std::fs::read_to_string(engine_state_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(engine_state_path(), json);
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
    let start_time = std::time::Instant::now();

    // Init rustls crypto provider
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (tx, _) = broadcast::channel::<MessageEvent>(10000);
    let tx2 = tx.clone();

    // Peer proposal channel — full TmProposeSet data for consensus engine
    let (proposal_tx, _) = broadcast::channel::<PeerProposal>(4000);
    let proposal_tx_for_handler = proposal_tx.clone();

    // Outbound message channel — validation + transaction relay to broadcast to all peers
    let (outbound_tx, _) = broadcast::channel::<Vec<u8>>(2000);
    let outbound_tx2 = outbound_tx.clone();

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
    let max_peers: usize = 125;

    // Seed with initial peers
    for peer in MAINNET_PEERS {
        known_peers.lock().insert(peer.to_string());
    }

    // Persistent validator identity — same key across restarts
    let validator_identity = Arc::new({
        let seed_path = xrpl_node::paths::seed_path();

        // VALAUDIT Phase 4 (va-04) — seed hygiene. Fatal on bad mode, atomic
        // 0600 create, fatal on malformed seed (no silent identity rotation).
        // See xrpl_node::paths::load_or_create_seed for full audit rationale.
        let (seed, was_created) = match xrpl_node::paths::load_or_create_seed(&seed_path) {
            Ok(pair) => pair,
            Err(msg) => {
                eprintln!("{msg}");
                std::process::exit(1);
            }
        };
        if was_created {
            eprintln!("[validator] Generated NEW seed (saved to {seed_path}, mode 0600)");
        } else {
            eprintln!("[validator] Loaded persistent seed from {seed_path}");
        }
        let identity = NodeIdentity::from_seed(&seed).expect("identity from seed failed");
        eprintln!("[validator] Signing key (Secp256k1): {}", identity.public_key_hex());
        eprintln!("[validator] Master key (Ed25519):    {}", hex::encode(identity.master_public_key()));
        identity
    });

    // Build manifest once at startup — pre-encoded as a framed TMManifests message
    let manifest_bytes = xrpl_node::validation::build_manifest(&validator_identity);
    let manifest_frame: Arc<Vec<u8>> = Arc::new({
        use prost::Message;
        let tm_manifest = xrpl_node::peer::protocol::TmManifests {
            list: vec![xrpl_node::peer::protocol::TmManifest {
                stobject: manifest_bytes.clone(),
            }],
            history: None,
        };
        let payload = tm_manifest.encode_to_vec();
        let type_code: u16 = 2; // mtMANIFESTS
        let len = payload.len() as u32;
        let mut frame = Vec::with_capacity(6 + payload.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&type_code.to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    });
    eprintln!("[validator] Manifest built ({} bytes)", manifest_bytes.len());

    // Dedicated validation senders — multiple peers for redundancy
    for relay_peer in &[
        "s1.ripple.com:51235",
        "s2.ripple.com:51235",
        "xrplcluster.com:51235",
        "r.ripple.com:51235",
    ] {
        let peer = relay_peer.to_string();
        let val_send_outbound = outbound_tx.clone();
        let val_send_manifest = manifest_frame.clone();
        tokio::spawn(async move {
        loop {
            eprintln!("[val-sender] Connecting to {peer} for validation relay...");
            let id = match NodeIdentity::generate() {
                Ok(i) => i,
                Err(_) => { tokio::time::sleep(Duration::from_secs(10)).await; continue; }
            };

            let hs = match timeout(
                Duration::from_secs(15),
                handshake::outbound_handshake(&peer, &id, handshake::NETWORK_ID_MAINNET),
            ).await {
                Ok(Ok(h)) => h,
                _ => { tokio::time::sleep(Duration::from_secs(10)).await; continue; }
            };

            eprintln!("[val-sender] Connected! Sending manifest + validations...");
            let mut stream = hs.stream;

            // Send manifest so peers know our validator identity
            {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stream.write_all(&val_send_manifest).await {
                    eprintln!("[val-sender] Manifest write failed: {e}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
                eprintln!("[val-sender] Manifest sent ({} bytes)", val_send_manifest.len());
            }
            // Split into read (drain) + write (validations)
            let (mut reader, mut writer) = tokio::io::split(stream);

            // Drain incoming messages so the connection doesn't stall
            let drain = tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {} // discard
                    }
                }
            });

            let mut rx = val_send_outbound.subscribe();
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        use tokio::io::AsyncWriteExt;
                        if let Err(e) = writer.write_all(&frame).await {
                            eprintln!("[val-sender] Write failed: {e}, reconnecting...");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            drain.abort();
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    } // end for relay_peer

    // Periodically broadcast manifest so all peers get it
    let manifest_broadcast = manifest_frame.clone();
    let manifest_outbound = outbound_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = manifest_outbound.send(manifest_broadcast.to_vec());
        }
    });

    // Live engine — opens RocksDB and applies transactions to real state
    // (created early so peer connections can serve ledger data)
    let rocks_db_path = xrpl_node::paths::state_rocks_path();
    let live_engine: Arc<Mutex<Option<xrpl_node::engine::LiveEngine>>> = Arc::new(Mutex::new(
        match xrpl_node::engine::LiveEngine::open(std::path::Path::new(&rocks_db_path)) {
            Ok(e) => {
                eprintln!("[live-engine] Opened RocksDB with {} entries", e.entry_count());
                Some(e)
            }
            Err(e) => {
                eprintln!("[live-engine] Failed to open RocksDB: {e}");
                None
            }
        }
    ));

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
            outbound_tx.clone(),
            manifest_frame.clone(),
            live_engine.clone(),
            proposal_tx_for_handler.clone(),
        );
    }


    // Inbound peer listener on port 51235
    // Accepts TLS connections, does XRPL handshake, processes messages
    let inbound_tx = tx2.clone();
    let inbound_known = known_peers.clone();
    let inbound_count = connected_count.clone();
    let inbound_seen = seen_msgs.clone();
    let inbound_active = connected_peers.clone();
    let inbound_identity = NodeIdentity::generate().unwrap_or_else(|_| {
        eprintln!("[inbound] Failed to generate identity");
        std::process::exit(1);
    });
    let node_pubkey_web = hex::encode(inbound_identity.public_key());
    let inbound_connected_web = connected_count.clone();

    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind("0.0.0.0:51235").await {
            Ok(l) => {
                eprintln!("[inbound] Listening for peer connections on port 51235");
                l
            }
            Err(e) => {
                eprintln!("[inbound] Failed to bind port 51235: {e}");
                return;
            }
        };

        loop {
            let (stream, addr) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[inbound] Accept error: {e}");
                    continue;
                }
            };

            let peer_addr = addr.to_string();
            eprintln!("[inbound] Connection from {peer_addr}");

            let tx = inbound_tx.clone();
            let known = inbound_known.clone();
            let count = inbound_count.clone();
            let seen = inbound_seen.clone();
            let active = inbound_active.clone();
            let max = max_peers;

            tokio::spawn(async move {
                let current = count.load(std::sync::atomic::Ordering::Relaxed);
                if current >= max {
                    eprintln!("[inbound] At max peers, rejecting {peer_addr}");
                    return;
                }
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                active.lock().insert(peer_addr.clone());

                // For inbound, we just read messages (simplified — no full handshake for now)
                // The connection helps us appear in the network topology
                eprintln!("[inbound] Accepted {peer_addr}");

                // Hold connection open for a while so crawlers see us
                tokio::time::sleep(Duration::from_secs(300)).await;

                active.lock().remove(&peer_addr);
                count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!("[inbound] {peer_addr} disconnected");
            });
        }
    });

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

    // State hash computation — build SHAMap from RocksDB in background
    // State hash: first do a targeted re-sync of corrupted entries, then build SHAMap
    let bulk_syncer = Arc::new(xrpl_node::bulk_sync::BulkSyncer::new());
    let state_hash_computer = Arc::new(xrpl_node::state_hash::StateHashComputer::new());
    // Set cache path next to the RocksDB for instant restarts
    {
        let cache_dir = std::path::Path::new(&rocks_db_path).parent().unwrap_or(std::path::Path::new("."));
        state_hash_computer.set_cache_path(cache_dir.join("leaf_cache.bin"));
    }
    // Historical data — SQLite time-series for uptime tracking
    let history_store: Arc<parking_lot::Mutex<xrpl_node::history::HistoryStore>> = {
        let history_path = std::path::Path::new(&rocks_db_path)
            .parent().unwrap_or(std::path::Path::new("."))
            .join("history.sqlite");
        let store = xrpl_node::history::HistoryStore::open(history_path.to_str().unwrap_or("history.sqlite"))
            .expect("failed to open history database");
        Arc::new(parking_lot::Mutex::new(store))
    };

    // Save cache on SIGTERM for instant restart (works with nohup/background)
    {
        let shc = state_hash_computer.clone();
        tokio::spawn(async move {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate()
            ).expect("failed to register SIGTERM handler");
            sigterm.recv().await;
            eprintln!("[shutdown] SIGTERM received — saving leaf cache...");
            shc.save_cache();
            eprintln!("[shutdown] Cache saved. Exiting.");
            std::process::exit(0);
        });
    }
    // FFI verifier — replays every ledger's txs through libxrpl (rippled's
    // C++ engine) and checks 100% TER agreement with mainnet. Feature-gated.
    #[cfg(feature = "ffi")]
    let ffi_verifier = {
        let rpc_urls: Vec<String> = std::env::var("XRPL_RPC_URLS")
            .or_else(|_| std::env::var("XRPL_RPC_URL"))
            .unwrap_or_else(|_| "http://10.0.0.39:5005".to_string())
            .split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        eprintln!("[ffi] initializing FfiVerifier with {} RPC endpoint(s)", rpc_urls.len());
        Arc::new(xrpl_node::ffi_verifier::FfiVerifier::new(rpc_urls))
    };
    #[cfg(not(feature = "ffi"))]
    let ffi_verifier: Option<Arc<()>> = None;
    #[cfg(feature = "ffi")]
    eprintln!("[ffi] verifier ready");

    let incremental_syncer = Arc::new(xrpl_node::incremental_sync::IncrementalSyncer::new());
    {
        let engine_guard = live_engine.lock();
        if let Some(ref eng) = *engine_guard {
            let db = eng.db_arc();
            let estimated = eng.entry_count() as u64;

            // Always do a full bulk sync to get clean, consistent state.
            // After completion: build SHAMap, then incremental sync takes over.
            // This will NOT re-trigger on restart — see the completion marker below.
            let marker_path = xrpl_node::paths::sync_complete_marker_path();
            let sync_done = std::path::Path::new(&marker_path).exists();

            if sync_done {
                eprintln!("[startup] Sync already completed — building SHAMap directly (~{estimated} entries)");

                // Suppress inc-sync IMMEDIATELY — before SHAMap build starts.
                // Inc-sync must not touch the tree until backfill catches up.
                incremental_syncer.backfilling.store(true, std::sync::atomic::Ordering::SeqCst);

                state_hash_computer.start_computation(db.clone(), estimated);

                // Scan wallet count in background
                {
                    let wc_db = db.clone();
                    let wc_hash = state_hash_computer.clone();
                    tokio::task::spawn_blocking(move || {
                        wc_hash.scan_wallet_count(wc_db.as_ref());
                    });
                }

                // Backfill gap: read pinned ledger from download marker, catch up to current
                let backfill_db = db.clone();
                let backfill_hash = state_hash_computer.clone();
                let backfill_syncer = incremental_syncer.clone();
                let backfill_hist = history_store.clone();
                #[cfg(feature = "ffi")]
                let backfill_ffi = Some(ffi_verifier.clone());
                #[cfg(not(feature = "ffi"))]
                let backfill_ffi: Option<Arc<()>> = None;
                tokio::spawn(async move {
                    let history_store = backfill_hist;
                    // Wait for SHAMap to finish building
                    loop {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if backfill_hash.is_ready() {
                            break;
                        }
                    }

                    // Read pinned ledger from download marker
                    let pinned_seq: u32 = std::fs::read_to_string(xrpl_node::paths::dl_done_path())
                        .ok()
                        .and_then(|s| {
                            // Format: "ledger #103122702 — ..."
                            s.split('#').nth(1)
                                .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                                .and_then(|n| n.parse().ok())
                        })
                        .unwrap_or(0);

                    if pinned_seq == 0 {
                        eprintln!("[backfill] Could not read pinned ledger from dl_done.txt — skipping backfill");
                        return;
                    }

                    // Get current validated ledger from rippled
                    let client = reqwest::Client::builder()
                        .timeout(Duration::from_secs(10))
                        .build()
                        .unwrap_or_default();
                    let current_seq: u32 = match client
                        .post("http://10.0.0.39:5005")
                        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
                        .send()
                        .await
                    {
                        Ok(resp) => {
                            resp.json::<serde_json::Value>().await.ok()
                                .and_then(|v| v["result"]["ledger"]["ledger_index"].as_str()
                                    .and_then(|s| s.parse().ok()))
                                .unwrap_or(0)
                        }
                        Err(_) => 0,
                    };

                    if current_seq > pinned_seq {
                        let gap = current_seq - pinned_seq;
                        eprintln!("[backfill] Gap: {gap} ledgers (#{pinned_seq} → #{current_seq})");
                        backfill_syncer.backfill_range(
                            pinned_seq,
                            current_seq,
                            backfill_db.clone(),
                            backfill_hash.clone(),
                        );

                        // Wait for backfill to finish before starting WS sync
                        // (avoids race: leaf cache built from partially-backfilled DB)
                        loop {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            if !backfill_syncer.backfilling.load(std::sync::atomic::Ordering::SeqCst) {
                                break;
                            }
                        }
                    } else {
                        eprintln!("[backfill] No gap — already current");
                    }

                    // Get latest validated ledger (backfill may have advanced past current_seq)
                    let ws_start: u32 = {
                        let c = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_default();
                        match c.post("http://10.0.0.39:5005")
                            .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
                            .send().await {
                            Ok(r) => r.json::<serde_json::Value>().await.ok()
                                .and_then(|v| v["result"]["ledger"]["ledger_index"].as_str()
                                    .and_then(|s| s.parse().ok()))
                                .unwrap_or(current_seq),
                            Err(_) => current_seq,
                        }
                    };

                    let ws_last = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(ws_start));
                    eprintln!("[startup] Starting WS sync from #{ws_start} after backfill");
                    xrpl_node::ws_sync::start_ws_sync(backfill_db, backfill_hash, ws_last, Some(history_store.clone()), backfill_ffi).await;
                });
            } else {
                eprintln!("[startup] Running full state sync — this is the final one");
                // Suppress inc-sync during download to prevent race conditions.
                // Bulk download writes objects at the pinned ledger; inc-sync writes
                // newer versions. Without suppression, the download can overwrite
                // newer data with stale data from the pinned ledger.
                incremental_syncer.backfilling.store(true, std::sync::atomic::Ordering::SeqCst);
                bulk_syncer.start(db.clone(), 30_000_000);

                let syncer = bulk_syncer.clone();
                let hash_comp = state_hash_computer.clone();
                let backfill_syncer = incremental_syncer.clone();
                let startup_db = db.clone();
                let startup_hist = history_store.clone();
                #[cfg(feature = "ffi")]
                let startup_ffi = ffi_verifier.clone();
                #[cfg(not(feature = "ffi"))]
                let startup_ffi: () = ();
                tokio::spawn(async move {
                    let history_store = startup_hist;
                    loop {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if !syncer.is_running() {
                            let synced = syncer.objects_synced();
                            let pinned = syncer.pinned_seq.load(std::sync::atomic::Ordering::SeqCst);
                            let _ = std::fs::write(marker_path, format!("{synced}"));

                            // Take the SHAMap built by the syncer
                            let built_map = syncer.shamap.lock().take();
                            if let Some(map) = built_map {
                                hash_comp.set_shamap(map);
                                eprintln!("[startup] SHAMap received ({synced} entries at #{pinned})");
                                // Scan for wallet count in background
                                let wc_db = startup_db.clone();
                                let wc_hash = hash_comp.clone();
                                tokio::task::spawn_blocking(move || {
                                    wc_hash.scan_wallet_count(wc_db.as_ref());
                                });
                            } else {
                                eprintln!("[startup] ERROR: no SHAMap from syncer");
                            }

                            // Skip second download — start WS sync directly from pinned ledger
                            // The first download data is at the pinned ledger; ws_sync catches up the gap
                            let ws_db = startup_db.clone();
                            let ws_hash = hash_comp.clone();
                            let ws_last = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(pinned));
                            // Save pinned ledger for restart backfill
                            let _ = std::fs::write(xrpl_node::paths::dl_done_path(),
                                format!("ledger #{pinned} — {synced} objects"));
                            let ws_hist = history_store.clone();
                            #[cfg(feature = "ffi")]
                            let ws_ffi = Some(startup_ffi.clone());
                            #[cfg(not(feature = "ffi"))]
                            let ws_ffi: Option<Arc<()>> = None;
                            tokio::spawn(async move {
                                xrpl_node::ws_sync::start_ws_sync(ws_db, ws_hash, ws_last, Some(ws_hist), ws_ffi).await;
                            });
                            eprintln!("[startup] WS sync from #{pinned} (no second download, inc-sync DISABLED)");
                            // Keep backfilling=true
                            break;
                        }
                    }
                });
            }
        }
    }

    // Background task: poll total_coins + account_hash from local rippled every 5s
    // (using local node — no rate limits, faster feedback on hash matching)
    let coins_engine = engine_state.clone();
    let hash_computer = state_hash_computer.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builder failed");
        loop {
            if let Ok(resp) = client
                .post("http://10.0.0.39:5005")
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
                    // Hash verification is done inline by inc-sync after each rebuild.
                    // Don't compare here — the polling interval doesn't align with
                    // the rebuild cycle and would produce false mismatches.
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
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

    // RPCA consensus engine
    let consensus = Arc::new(Mutex::new(xrpl_node::consensus_engine::ConsensusEngine::new()));

    // Fetch UNL (trusted validators) from published sources — required for consensus threshold math
    {
        let consensus_for_unl = consensus.clone();
        tokio::spawn(async move {
            match xrpl_node::unl_fetch::fetch_default_unl().await {
                Ok(entries) => {
                    let mut eng = consensus_for_unl.lock();
                    // TMProposeSet messages are signed with EPHEMERAL signing keys, not master keys.
                    // So we trust the signing keys in our UNL (extracted from each validator's manifest).
                    // Fall back to master key if no signing key available (manifest rotation not tracked yet).
                    let mut with_signing = 0;
                    for e in &entries {
                        if let Some(ref sk) = e.signing_key {
                            eng.unl.add_trusted(sk.clone());
                            with_signing += 1;
                        } else {
                            eng.unl.add_trusted(e.public_key.clone());
                        }
                    }
                    eprintln!(
                        "[consensus] UNL loaded: {} validators, {} with signing keys, trust_count={}",
                        entries.len(), with_signing, eng.unl.trusted_count()
                    );
                }
                Err(e) => {
                    eprintln!("[consensus] UNL fetch failed: {e} — consensus disabled");
                }
            }
        });
    }

    // Consensus monitor: tracks tx_hash agreement across trusted validators.
    //
    // XRPL proposals only include added/removed transactions when the set is small.
    // For typical ledger closes (>8 txs), only current_tx_hash identifies the set,
    // and validators fetch contents via TMGetObjectByHash. So we track hash-level
    // agreement: "which tx_set hash do most validators propose?".
    let consensus_monitor: Arc<Mutex<ConsensusMonitor>> = Arc::new(Mutex::new(ConsensusMonitor::default()));

    // FFI health check — runs periodically to verify libxrpl integration stays healthy
    #[cfg(feature = "ffi")]
    let ffi_stats = {
        let stats = xrpl_node::ffi_engine::new_stats();
        eprintln!("[ffi] Linked against libxrpl {}", stats.lock().libxrpl_version);
        // Run initial health check
        let passed = xrpl_node::ffi_engine::health_check(&stats);
        eprintln!("[ffi] Initial health check: {}", if passed { "PASSED" } else { "FAILED" });
        // Periodic health checks (every 10s)
        let hc_stats = stats.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                xrpl_node::ffi_engine::health_check(&hc_stats);
            }
        });
        stats
    };
    {
        let monitor = consensus_monitor.clone();
        let consensus_for_monitor = consensus.clone();
        let mut prop_rx = proposal_tx.subscribe();
        tokio::spawn(async move {
            while let Ok(p) = prop_rx.recv().await {
                let validator_key_hex = hex::encode_upper(&p.validator_key);
                let tx_hash_hex = hex::encode_upper(&p.current_tx_hash);

                // Only track proposals from trusted UNL validators
                let is_trusted = consensus_for_monitor.lock().unl.is_trusted(&validator_key_hex);
                if !is_trusted {
                    continue;
                }

                let mut m = monitor.lock();
                m.record(validator_key_hex, tx_hash_hex, p.propose_seq, p.close_time);
            }
        });
    }

    // Consensus monitor driver: every 1s, check current agreement level
    {
        let monitor = consensus_monitor.clone();
        let consensus_for_check = consensus.clone();
        tokio::spawn(async move {
            let mut last_reported_hash: Option<String> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let unl_size = consensus_for_check.lock().unl.trusted_count();
                let current_ledger = consensus_for_check.lock().ledger_seq;
                let mut m = monitor.lock();
                m.expire_old(current_ledger);
                let agreement = m.check_agreement(unl_size);
                if let Some(ref a) = agreement {
                    if last_reported_hash.as_ref() != Some(&a.tx_hash) {
                        eprintln!(
                            "[consensus] Agreement: {}/{} validators on tx_hash {}... ({:.0}%)",
                            a.count, unl_size, &a.tx_hash[..16.min(a.tx_hash.len())], a.pct * 100.0
                        );
                        last_reported_hash = Some(a.tx_hash.clone());
                    }
                }
            }
        });
    }

    // Subscribe to message events for engine tracking + validation signing + consensus
    let engine_rx = tx2.subscribe();
    let engine = engine_state.clone();
    let le = live_engine.clone();
    let val_identity = validator_identity.clone();
    let val_outbound = outbound_tx2.clone();
    let val_sse = tx2.clone();
    let consensus_loop = consensus.clone();
    let state_hash_for_close = state_hash_computer.clone();
    let inc_syncer = incremental_syncer.clone();
    let inc_hash_comp = state_hash_computer.clone();
    let mut last_validated_seq: u32 = 0;
    // Buffer raw transactions per round for full ledger close
    let mut round_raw_txs: Vec<(xrpl_core::types::Hash256, Vec<u8>)> = Vec::new();
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

                            // Collect verification data + modified keylets (separate lock)
                            let (prev_modified, _modified_keys) = {
                                if let Some(ref mut eng) = *le.lock() {
                                    eng.new_round(seq)
                                } else {
                                    (Vec::new(), Vec::new())
                                }
                            };

                            // --- Full ledger close: apply all buffered txs ---
                            let prev_round_txs = std::mem::take(&mut round_raw_txs);
                            if !prev_round_txs.is_empty() {
                                let db_for_close = {
                                    le.lock().as_ref().map(|e| e.db_arc())
                                };
                                if let Some(db) = db_for_close {
                                    let prev_seq = seq.saturating_sub(1);
                                    let ledger_hash = event.ledger_hash.as_ref()
                                        .and_then(|h| hex::decode(h).ok())
                                        .and_then(|b| if b.len() == 32 {
                                            let mut arr = [0u8; 32];
                                            arr.copy_from_slice(&b);
                                            Some(arr)
                                        } else { None })
                                        .unwrap_or([0u8; 32]);

                                    let now_ripple = (std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        .saturating_sub(946684800)) as u32;

                                    let total_coins = state.network_total_coins;

                                    // DISABLED: close_ledger reads from state_db which may interfere
                                    // with WS sync writes. Disable to test if this is the cause.
                                    if false {
                                    tokio::task::spawn_blocking(move || {
                                        let _ = xrpl_node::ledger_close::close_ledger(
                                            &db,
                                            &prev_round_txs,
                                            prev_seq,
                                            ledger_hash,
                                            total_coins,
                                            now_ripple,
                                            now_ripple.saturating_sub(4),
                                        );
                                    });
                                    } // end if false — close_ledger disabled

                                    // Incremental sync: fetch ALL changed state objects
                                    // (not just accounts — offers, trust lines, directories too)
                                    // Incremental sync — pointed at local rippled (localhost:5005)
                                    {
                                        let inc_db = le.lock().as_ref().map(|e| e.db_arc());
                                        if let Some(idb) = inc_db {
                                            inc_syncer.sync_ledger(
                                                prev_seq,
                                                idb,
                                                inc_hash_comp.clone(),
                                            );
                                        }
                                    }
                                }
                            }

                            eprintln!("[verify] Round ended, {prev_modified_count} accounts to verify, {key_count} keylets modified",
                                prev_modified_count = prev_modified.len(),
                                key_count = _modified_keys.len());
                            // Balance sampling removed — was fundamentally flawed
                            // (didn't account for incoming payments, only fee deductions).
                            // Real verification is the SHAMap root hash comparison
                            // in state_hash::set_network_hash().
                            let _ = prev_modified;
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
                        // Feed proposals to consensus engine
                        if let Some(ref vkey) = event.validator {
                            // For now, we follow the network's consensus rather than
                            // independently proposing. We track proposals for convergence.
                            // TODO: parse the actual tx set from the proposal
                        }
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

                            // Feed transaction to consensus mempool
                            {
                                let tx_hash_bytes = hex::decode(&info.hash).unwrap_or_default();
                                if tx_hash_bytes.len() >= 8 {
                                    let mut hash = [0u8; 32];
                                    hash[..tx_hash_bytes.len().min(32)].copy_from_slice(
                                        &tx_hash_bytes[..tx_hash_bytes.len().min(32)]
                                    );
                                    consensus_loop.lock().on_transaction(
                                        xrpl_core::types::Hash256(hash),
                                        info.raw_tx.clone(),
                                    );
                                }
                            }

                            // Buffer raw tx for full ledger close
                            if !info.raw_tx.is_empty() {
                                let mut hash = [0u8; 32];
                                let tx_hash_bytes = hex::decode(&info.hash).unwrap_or_default();
                                hash[..tx_hash_bytes.len().min(32)].copy_from_slice(
                                    &tx_hash_bytes[..tx_hash_bytes.len().min(32)]
                                );
                                round_raw_txs.push((xrpl_core::types::Hash256(hash), info.raw_tx.clone()));
                            }

                            // DISABLED: engine apply_transaction reads from state_db
                            // which may interfere with WS sync writes
                            // if let Some(ref mut eng) = *le.lock() { ... }
                        }
                    }

                    // Trigger consensus on CLOSING events
                    if event.msg_type == "StatusChange" && event.detail.contains("CLOSING") {
                        if let Some(seq) = event.ledger_seq {
                            consensus_loop.lock().on_close_trigger(seq);
                        }
                    }

                    // Sign and broadcast validation on ACCEPTED
                    if event.msg_type == "StatusChange"
                        && event.detail.contains("ACCEPTED")
                    {
                        if let Some(seq) = event.ledger_seq {
                            if seq > last_validated_seq {
                                last_validated_seq = seq;

                                // Get network's ledger hash from StatusChange
                                let network_hash_bytes: [u8; 32] = event.ledger_hash.as_ref()
                                    .and_then(|h| hex::decode(h).ok())
                                    .and_then(|b| if b.len() == 32 {
                                        let mut arr = [0u8; 32];
                                        arr.copy_from_slice(&b);
                                        Some(arr)
                                    } else { None })
                                    .unwrap_or([0u8; 32]);

                                // VALAUDIT Phase 3 (va-03) — Signing Gate, simple form.
                                // Refuse to sign until our independently-computed
                                // account_hash has matched the network's for at least
                                // 3 consecutive ledgers (StateHashComputer::is_ready_to_sign).
                                //
                                // Pre-Phase-3 behavior: hash_source was just a log label;
                                // the validator signed network_hash_bytes regardless of
                                // whether we'd verified anything. Per the audit, that gate
                                // was tautological and effectively a no-op security control.
                                //
                                // Post-Phase-3: skip categorically (no sign, no broadcast)
                                // for either of two reasons, both counted for /metrics +
                                // /api/state-hash visibility:
                                //   - not_ready: consecutive_matches < 3
                                //   - zero_hash: StatusChange arrived without ledger_hash
                                //
                                // Operational expectation: ~3 not_ready skips at warmup
                                // after every restart, then a clean stream of VERIFIED
                                // signs forever. Mainnet 7-day watch confirms this.
                                use std::sync::atomic::Ordering;
                                if network_hash_bytes == [0u8; 32] {
                                    state_hash_for_close.validations_skipped_zero_hash
                                        .fetch_add(1, Ordering::Relaxed);
                                    eprintln!("[validator] Declining to sign #{seq}: zero ledger_hash from StatusChange");
                                    let _ = val_sse.send(MessageEvent {
                                        seq: 0, time: 0.0,
                                        msg_type: "ValidationSkipped".into(),
                                        detail: format!("zero ledger_hash from StatusChange"),
                                        ledger_seq: Some(seq),
                                        ledger_hash: None, peers: None, validator: None, tx_info: None,
                                    });
                                    continue;
                                }
                                if !state_hash_for_close.is_ready_to_sign() {
                                    let n = state_hash_for_close.consecutive_matches.load(Ordering::Acquire);
                                    state_hash_for_close.validations_skipped_not_ready
                                        .fetch_add(1, Ordering::Relaxed);
                                    eprintln!("[validator] Declining to sign #{seq}: only {n} consecutive matches (need 3)");
                                    let _ = val_sse.send(MessageEvent {
                                        seq: 0, time: 0.0,
                                        msg_type: "ValidationSkipped".into(),
                                        detail: format!("only {n} consecutive matches (need 3)"),
                                        ledger_seq: Some(seq),
                                        ledger_hash: None, peers: None, validator: None, tx_info: None,
                                    });
                                    continue;
                                }
                                let hash_source = "VERIFIED";
                                let ledger_hash_bytes = network_hash_bytes;

                                // Gate passed (ready_to_sign && non-zero hash) — proceed.
                                {

                                // On flag ledgers (every 256th), include amendment votes
                                let amendments = if xrpl_node::consensus::amendment_vote::is_flag_ledger(seq) {
                                    let a = xrpl_node::validation::supported_amendments();
                                    eprintln!("[validator] Flag ledger #{seq} — voting on {} amendments", a.len());
                                    Some(a)
                                } else {
                                    None
                                };

                                let validation_bytes = xrpl_node::validation::sign_validation(
                                    &val_identity, seq, &ledger_hash_bytes,
                                    amendments.as_deref(),
                                );

                                if !validation_bytes.is_empty() {
                                    // Wrap in TMValidation protobuf
                                    use prost::Message;
                                    let tm_val = xrpl_node::peer::protocol::TmValidation {
                                        validation: validation_bytes,
                                        checked_signature: None,
                                        hops: Some(0),
                                    };
                                    // Encode as framed peer message: 4-byte len + 2-byte type + payload
                                    let payload = tm_val.encode_to_vec();
                                    let type_code: u16 = 41; // mtVALIDATION
                                    let len = payload.len() as u32;
                                    let mut frame = Vec::with_capacity(6 + payload.len());
                                    frame.extend_from_slice(&len.to_be_bytes());
                                    frame.extend_from_slice(&type_code.to_be_bytes());
                                    frame.extend_from_slice(&payload);

                                    // Broadcast to all connected peers via outbound channel
                                    let _ = val_outbound.send(frame);

                                    // Notify the frontend
                                    let hash_short = hex::encode(&ledger_hash_bytes[..8]);
                                    let _ = val_sse.send(MessageEvent {
                                        seq: 0, time: 0.0,
                                        msg_type: "OurValidation".into(),
                                        detail: format!("Validated #{seq} hash={hash_short}..."),
                                        ledger_seq: Some(seq),
                                        ledger_hash: None, peers: None, validator: None, tx_info: None,
                                    });
                                    eprintln!("[validator] Signed validation for ledger #{seq} hash={hash_short}... (src={hash_source})");
                                }
                                } // gate-passed scope
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
            .expect("reqwest client builder failed");
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
        .route("/", get(validator_page))
        .route("/feed", get(index_page))
        .route("/consensus", get(consensus_page))
        .route("/validator", get(validator_page))
        .route("/incidents", get(incidents_page))
        .route("/historical", get(historical_page))
        .route("/network", get(network_page))
        .route("/page/{name}", get(catch_all_page))
        .route("/dashboard", get(metrics_page))
        .route("/metrics", get({
            let prom_hash = state_hash_computer.clone();
            let prom_engine = engine_state.clone();
            let prom_connected = connected_count.clone();
            let prom_le = live_engine.clone();
            let prom_start = start_time;
            move || {
                let prom_hash = prom_hash.clone();
                let prom_engine = prom_engine.clone();
                let prom_connected = prom_connected.clone();
                let prom_le = prom_le.clone();
                async move {
                    let hash_status = prom_hash.status.lock().clone();
                    let engine = prom_engine.lock().clone();
                    let peers = prom_connected.load(std::sync::atomic::Ordering::Relaxed);
                    let db_entries = prom_le.lock().as_ref().map(|e| e.entry_count() as u64).unwrap_or(0);
                    let round_time = hash_status.sync_log.first().map(|e| e.time_secs).unwrap_or(0.0);

                    let snap = xrpl_node::rpc::metrics::ValidatorMetricsSnapshot {
                        uptime_secs: prom_start.elapsed().as_secs(),
                        peer_count: peers,
                        ledger_seq: hash_status.ledger_seq,
                        messages_received: 0, // TODO: wire node_metrics
                        messages_sent: 0,
                        consecutive_matches: hash_status.consecutive_matches,
                        total_matches: hash_status.total_matches,
                        total_mismatches: hash_status.total_mismatches,
                        round_time_secs: round_time,
                        compute_time_secs: hash_status.compute_time_secs,
                        state_objects: db_entries,
                        ready_to_sign: hash_status.ready_to_sign,
                        total_txs: engine.total_txs,
                        validations_skipped_not_ready: prom_hash.validations_skipped_not_ready
                            .load(std::sync::atomic::Ordering::Relaxed),
                        validations_skipped_zero_hash: prom_hash.validations_skipped_zero_hash
                            .load(std::sync::atomic::Ordering::Relaxed),
                    };
                    let body = xrpl_node::rpc::metrics::render_prometheus(&snap);
                    (
                        axum::http::StatusCode::OK,
                        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
                        body,
                    )
                }
            }
        }))
        .route("/peers", get(peers_page))
        .route("/state", get(state_page))
        .route("/events", get(move || sse_handler(tx.clone())))
        .route("/crawl", get({
            let crawl_pubkey = node_pubkey_web.clone();
            let crawl_connected = inbound_connected_web.clone();
            move || {
                let pk = crawl_pubkey.clone();
                let cc = crawl_connected.clone();
                async move {
                    let connected = cc.load(std::sync::atomic::Ordering::Relaxed);
                    axum::Json(serde_json::json!({
                        "server": {
                            "build_version": "xrpl-rust-validator/0.1.0",
                            "server_state": "full",
                            "pubkey_node": pk,
                        },
                        "overlay": {
                            "active": [],
                        },
                        "counts": {
                            "connected": connected,
                        }
                    }))
                }
            }
        }))
        .route("/api/sync-status", get({
            let sync_le = live_engine.clone();
            let sync_bulk = bulk_syncer.clone();
            move || {
                let sync_le = sync_le.clone();
                let sync_bulk = sync_bulk.clone();
                async move {
                    // Check completion marker first — if it exists, sync is done
                    let marker_count: u64 = std::fs::read_to_string(xrpl_node::paths::sync_complete_marker_path())
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);

                    let rocks_path = xrpl_node::paths::state_rocks_path();
                    let db_size_mb = std::fs::read_dir(&rocks_path)
                        .map(|es| es.filter_map(|e| e.ok())
                            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                            .sum::<u64>())
                        .unwrap_or(0) / 1_048_576;

                    if marker_count > 0 {
                        // Sync completed — show 100%
                        axum::Json(serde_json::json!({
                            "objects": marker_count,
                            "estimated_total": marker_count,
                            "percent": 100.0,
                            "size_mb": db_size_mb,
                            "syncing": false,
                            "sync_rate": 0,
                            "sync_objects": marker_count,
                            "sync_eta_secs": 0,
                        }))
                    } else {
                        // Still syncing — show bulk sync progress
                        let bulk = sync_bulk.status.lock().clone();
                        let entries = if let Some(ref eng) = *sync_le.lock() {
                            eng.entry_count() as u64
                        } else { 0 };
                        let estimated_total: u64 = 30_000_000;
                        let pct = (entries as f64 / estimated_total as f64 * 100.0).min(100.0);
                        axum::Json(serde_json::json!({
                            "objects": entries,
                            "estimated_total": estimated_total,
                            "percent": (pct * 10.0).round() / 10.0,
                            "size_mb": db_size_mb,
                            "syncing": bulk.running,
                            "sync_rate": (bulk.rate * 10.0).round() / 10.0,
                            "sync_objects": bulk.objects_synced,
                            "sync_eta_secs": bulk.estimated_remaining_secs.round() as u64,
                        }))
                    }
                }
            }
        }))
        .route("/api/engine", get({
            let engine_web = engine_state.clone();
            let le_web = live_engine.clone();
            #[cfg(feature = "ffi")]
            let ffi_web = ffi_verifier.clone();
            move || {
                let engine_web = engine_web.clone();
                let le_web = le_web.clone();
                #[cfg(feature = "ffi")]
                let ffi_web = ffi_web.clone();
                async move {
                    let state = engine_web.lock().clone();
                    let mut json = serde_json::to_value(&state).unwrap_or_default();
                    // Add live engine stats
                    if let Some(ref eng) = *le_web.lock() {
                        let verify_pct = if eng.verified_total > 0 {
                            eng.verified_match as f64 / eng.verified_total as f64 * 100.0
                        } else { 0.0 };
                        json["live_engine"] = serde_json::json!({
                            "active": true,
                            "entries": eng.entry_count(),
                            "round_applied": eng.round_applied,
                            "round_failed": eng.round_failed,
                            "total_applied": eng.total_applied,
                            "total_failed": eng.total_failed,
                            "total_coins": eng.total_coins,
                            "verified_match": eng.verified_match,
                            "verified_mismatch": eng.verified_mismatch,
                            "verified_total": eng.verified_total,
                            "verify_pct": (verify_pct * 10.0).round() / 10.0,
                        });
                    } else {
                        json["live_engine"] = serde_json::json!({"active": false});
                    }
                    // FFI verification stats (replaces deprecated tx_engine)
                    #[cfg(feature = "ffi")]
                    {
                        let ffi_stats = ffi_web.stats();
                        json["ffi_verifier"] = serde_json::to_value(&ffi_stats).unwrap_or_default();
                    }
                    #[cfg(not(feature = "ffi"))]
                    {
                        json["ffi_verifier"] = serde_json::json!({"enabled": false, "note": "build with --features ffi"});
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
        }))
        .route("/api/state-hash", get({
            let hash_status = state_hash_computer.clone();
            let sync_status = bulk_syncer.clone();
            let hash_le = live_engine.clone();
            let inc_status = incremental_syncer.clone();
            move || {
                let hash_status = hash_status.clone();
                let sync_status = sync_status.clone();
                let hash_le = hash_le.clone();
                let inc_status = inc_status.clone();
                async move {
                    let mut status = serde_json::to_value(&hash_status.status.lock().clone()).unwrap_or_default();
                    status["bulk_sync"] = serde_json::to_value(&sync_status.status.lock().clone()).unwrap_or_default();
                    status["incremental_sync"] = serde_json::to_value(&inc_status.stats.lock().clone()).unwrap_or_default();
                    // Use marker file count (accurate) instead of estimate-num-keys (broken)
                    let marker_count: u64 = std::fs::read_to_string(xrpl_node::paths::sync_complete_marker_path())
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or_else(|| {
                            hash_le.lock().as_ref().map(|e| e.entry_count() as u64).unwrap_or(0)
                        });
                    status["db_entries"] = serde_json::json!(marker_count);
                    status["wallet_count"] = serde_json::json!(
                        hash_status.wallet_count.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    status["wallet_history"] = serde_json::json!(
                        hash_status.wallet_history.lock().clone()
                    );
                    // VALAUDIT Phase 3 (va-03) signing-gate skip counters.
                    status["validations_skipped_not_ready"] = serde_json::json!(
                        hash_status.validations_skipped_not_ready
                            .load(std::sync::atomic::Ordering::Relaxed)
                    );
                    status["validations_skipped_zero_hash"] = serde_json::json!(
                        hash_status.validations_skipped_zero_hash
                            .load(std::sync::atomic::Ordering::Relaxed)
                    );
                    axum::Json(status)
                }
            }
        }))
        .route("/api/history", get({
            let hist = history_store.clone();
            move || {
                let hist = hist.clone();
                async move {
                    // Default: 24h summary. Query params parsed from request if needed later.
                    let h = hist.lock();
                    let s1h = h.summary(3600);
                    let s24h = h.summary(86400);
                    let s7d = h.summary(604800);
                    let s30d = h.summary(2592000);
                    let recent = h.recent(50);
                    let total = h.total_rounds();
                    drop(h);
                    axum::Json(serde_json::json!({
                        "total_recorded": total,
                        "1h": s1h,
                        "24h": s24h,
                        "7d": s7d,
                        "30d": s30d,
                        "recent": recent,
                    }))
                }
            }
        }))
        .route("/api/consensus", get({
            let consensus_web = consensus.clone();
            let monitor_web = consensus_monitor.clone();
            move || {
                let consensus_web = consensus_web.clone();
                let monitor_web = monitor_web.clone();
                async move {
                    let status = consensus_web.lock().status();
                    let mut json = serde_json::to_value(&status).unwrap_or_default();
                    json["monitor"] = monitor_web.lock().snapshot();
                    axum::Json(json)
                }
            }
        }));

    #[cfg(feature = "ffi")]
    let app = app.route("/api/ffi-status", get({
        let ffi_stats_web = ffi_stats.clone();
        move || {
            let ffi_stats_web = ffi_stats_web.clone();
            async move {
                let snapshot = ffi_stats_web.lock().clone();
                axum::Json(serde_json::to_value(&snapshot).unwrap_or_default())
            }
        }
    }));
    #[cfg(not(feature = "ffi"))]
    let app = app.route("/api/ffi-status", get(|| async {
        axum::Json(serde_json::json!({
            "enabled": false,
            "note": "Build with --features ffi to enable libxrpl integration"
        }))
    }));

    // SECURITY(10.3): All /api/* endpoints are read-only (sync-status, engine, peers,
    // state-hash, history, consensus). There is no submit/transaction endpoint.
    // If a submit endpoint is ever added, it MUST require authentication (API key or mTLS).
    // Binding to 0.0.0.0 is acceptable for read-only status since Caddy fronts this
    // on the public side with its own access controls.
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
    outbound_rx: broadcast::Sender<Vec<u8>>,
    manifest: Arc<Vec<u8>>,
    rocks_db: Arc<Mutex<Option<xrpl_node::engine::LiveEngine>>>,
    proposal_tx: broadcast::Sender<PeerProposal>,
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
            match run_peer_connection(&peer, &tx, &known_peers, &connected_count, max_peers, &seen_msgs, &active_peers, &outbound_rx, &manifest, &rocks_db, &proposal_tx).await {
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
    outbound: &broadcast::Sender<Vec<u8>>,
    manifest: &Arc<Vec<u8>>,
    rocks_db: &Arc<Mutex<Option<xrpl_node::engine::LiveEngine>>>,
    proposal_tx: &broadcast::Sender<PeerProposal>,
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
        ledger_hash: None,
        peers: None,
        validator: None,
        tx_info: None,
    });

    // Split stream for read + write (needed to respond to GetLedger/GetObjects)
    let (mut reader, writer) = tokio::io::split(hs.stream);
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

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
                                    if addr.starts_with("[::") || addr.starts_with("0.") || addr.starts_with("127.") || addr.starts_with("10.") || addr.starts_with("192.168.") || addr.starts_with("172.") || addr.starts_with("169.") || addr.starts_with("100.64") {
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
                                            outbound.clone(),
                                            manifest.clone(),
                                            rocks_db.clone(),
                                            proposal_tx.clone(),
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

                    // --- Transaction relay: forward to all relay peers ---
                    if let PeerMessage::Transaction(_) = &msg {
                        let payload = msg.encode_to_vec();
                        let type_code: u16 = 30; // mtTRANSACTION
                        let len = payload.len() as u32;
                        let mut frame = Vec::with_capacity(6 + payload.len());
                        frame.extend_from_slice(&len.to_be_bytes());
                        frame.extend_from_slice(&type_code.to_be_bytes());
                        frame.extend_from_slice(&payload);
                        let _ = outbound.send(frame);
                    }

                    // --- Serve ledger data: respond to GetLedger / GetObjects ---
                    if let PeerMessage::GetLedger(ref req) = msg {
                        // Respond with objects from RocksDB if we have them
                        let writer_c = writer.clone();
                        let db_c = rocks_db.clone();
                        let itype = req.itype;
                        let node_ids = req.node_i_ds.clone();
                        let ledger_hash_req = req.ledger_hash.clone();
                        let ledger_seq_req = req.ledger_seq;
                        let cookie = req.request_cookie;
                        tokio::spawn(async move {
                            serve_get_ledger(
                                &writer_c, &db_c, itype, &node_ids,
                                ledger_hash_req.as_deref(), ledger_seq_req, cookie,
                            ).await;
                        });
                    }

                    if let PeerMessage::GetObjects(ref req) = msg {
                        if req.query {
                            let writer_c = writer.clone();
                            let db_c = rocks_db.clone();
                            let objects = req.objects.clone();
                            let obj_type = req.r#type;
                            let ledger_hash_req = req.ledger_hash.clone();
                            let seq = req.seq;
                            tokio::spawn(async move {
                                serve_get_objects(
                                    &writer_c, &db_c, obj_type, &objects,
                                    ledger_hash_req.as_deref(), seq,
                                ).await;
                            });
                        }
                    }

                    // Publish peer proposals to consensus engine channel
                    if let PeerMessage::ProposeSet(p) = &msg {
                        if !p.node_pub_key.is_empty() {
                            let _ = proposal_tx.send(PeerProposal {
                                validator_key: p.node_pub_key.clone(),
                                propose_seq: p.propose_seq,
                                current_tx_hash: p.current_tx_hash.clone(),
                                close_time: p.close_time,
                                previous_ledger: p.previousledger.clone(),
                            });
                        }
                    }

                    let (msg_type, detail, ledger_seq, ledger_hash, peers, validator, tx_info) = format_message(&msg);

                    let event = MessageEvent {
                        seq: msg_seq,
                        time: elapsed,
                        msg_type,
                        detail,
                        ledger_seq,
                        ledger_hash,
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
        match timeout(Duration::from_secs(30), reader.read(&mut chunk)).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => return Err(format!("read: {e}")),
            Err(_) => continue,
        }
    }
}

/// Write a framed peer protocol message to a stream.
async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &tokio::sync::Mutex<W>,
    type_code: u16,
    payload: &[u8],
) {
    use tokio::io::AsyncWriteExt;
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(6 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&type_code.to_be_bytes());
    frame.extend_from_slice(payload);
    let mut w = writer.lock().await;
    let _ = w.write_all(&frame).await;
}

/// Serve a TMGetLedger request — look up SHAMap nodes from RocksDB.
async fn serve_get_ledger<W: tokio::io::AsyncWrite + Unpin + Send>(
    writer: &tokio::sync::Mutex<W>,
    rocks_db: &Arc<Mutex<Option<xrpl_node::engine::LiveEngine>>>,
    itype: i32,
    node_ids: &[Vec<u8>],
    ledger_hash: Option<&[u8]>,
    ledger_seq: Option<u32>,
    cookie: Option<u64>,
) {
    use prost::Message;

    // Only serve account state nodes (liAS_NODE = 2) and tx nodes (1)
    if itype != 2 && itype != 1 {
        return;
    }

    // Look up nodes under the lock, then drop it before awaiting
    let (nodes, hash_vec, seq_val) = {
        let engine_guard = rocks_db.lock();
        let engine = match engine_guard.as_ref() {
            Some(e) => e,
            None => return,
        };

        let mut nodes = Vec::new();
        for node_id in node_ids {
            if node_id.len() != 32 { continue; }
            if let Some(data) = engine.get(node_id) {
                nodes.push(xrpl_node::peer::protocol::TmLedgerNode {
                    nodedata: data,
                    nodeid: Some(node_id.clone()),
                });
            }
        }
        (nodes, ledger_hash.map(|h| h.to_vec()).unwrap_or_default(), ledger_seq.unwrap_or(0))
    }; // engine_guard dropped here

    if nodes.is_empty() {
        return;
    }

    let resp = xrpl_node::peer::protocol::TmLedgerData {
        ledger_hash: hash_vec,
        ledger_seq: seq_val,
        r#type: itype,
        nodes,
        request_cookie: cookie.map(|c| c as u32),
        error: None,
    };

    let payload = resp.encode_to_vec();
    write_frame(writer, 32, &payload).await; // mtLEDGER_DATA = 32
}

/// Serve a TMGetObjectByHash request — look up objects from RocksDB.
async fn serve_get_objects<W: tokio::io::AsyncWrite + Unpin + Send>(
    writer: &tokio::sync::Mutex<W>,
    rocks_db: &Arc<Mutex<Option<xrpl_node::engine::LiveEngine>>>,
    obj_type: i32,
    objects: &[xrpl_node::peer::protocol::TmIndexedObject],
    ledger_hash: Option<&[u8]>,
    seq: Option<u32>,
) {
    use prost::Message;

    // Look up objects under the lock, then drop it before awaiting
    let (found, hash_vec) = {
        let engine_guard = rocks_db.lock();
        let engine = match engine_guard.as_ref() {
            Some(e) => e,
            None => return,
        };

        let mut found = Vec::new();
        for obj in objects {
            let key = obj.hash.as_deref()
                .or(obj.index.as_deref());
            if let Some(k) = key {
                if k.len() == 32 {
                    if let Some(data) = engine.get(k) {
                        found.push(xrpl_node::peer::protocol::TmIndexedObject {
                            hash: obj.hash.clone(),
                            node_id: obj.node_id.clone(),
                            index: obj.index.clone(),
                            data: Some(data),
                            ledger_seq: obj.ledger_seq,
                        });
                    }
                }
            }
        }
        (found, ledger_hash.map(|h| h.to_vec()))
    }; // engine_guard dropped here

    if found.is_empty() {
        return;
    }

    let resp = xrpl_node::peer::protocol::TmGetObjectByHash {
        r#type: obj_type,
        query: false, // This is a reply
        seq,
        ledger_hash: hash_vec,
        fat: None,
        objects: found,
    };

    let payload = resp.encode_to_vec();
    write_frame(writer, 42, &payload).await; // mtGET_OBJECTS = 42
}

/// Returns (msg_type, detail, ledger_seq, ledger_hash, peers, validator_key, tx_info)
#[allow(clippy::type_complexity)]
fn format_message(msg: &PeerMessage) -> (String, String, Option<u32>, Option<String>, Option<Vec<PeerEntry>>, Option<String>, Option<TxInfo>) {
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
            let hash = sc.ledger_hash.as_ref().map(|h| hex::encode(h));
            (
                "StatusChange".into(),
                format!("status={status} event={event}"),
                sc.ledger_seq,
                hash,
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
            ("Validation".into(), detail, None, None, None, vkey, None)
        }
        PeerMessage::Manifests(m) => {
            // Dump first manifest for debugging
            if !m.list.is_empty() {
                static DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let raw = &m.list[0].stobject;
                    eprintln!("[manifest-debug] Real manifest: {} bytes: {}", raw.len(), hex::encode(raw));
                    // Parse fields
                    let mut i = 0;
                    while i < raw.len() {
                        let b = raw[i];
                        let (tc, fc) = if b & 0xF0 != 0 && b & 0x0F != 0 {
                            (b >> 4, b & 0x0F)
                        } else if b & 0xF0 == 0 {
                            i += 1; if i >= raw.len() { break; }
                            (raw[i], b & 0x0F)
                        } else {
                            i += 1; if i >= raw.len() { break; }
                            (b >> 4, raw[i])
                        };
                        eprintln!("[manifest-debug]   offset={i} byte=0x{b:02X} type={tc} field={fc}");
                        i += 1;
                    }
                }
            }
            ("Manifests".into(), format!("{} manifests", m.list.len()), None, None, None, None, None)
        }
        PeerMessage::ValidatorListCollection(_) => {
            ("ValidatorList".into(), "UNL bootstrap".into(), None, None, None, None, None)
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
                    raw_tx: t.raw_transaction.clone(),
                }
            });

            let detail = if let Some(ref info) = tx_info {
                let sig_icon = if info.sig_verified { "OK" } else { "FAIL" };
                format!("{} from {} fee={} sig={sig_icon}", info.tx_type, &info.account[..8], info.fee)
            } else {
                format!("{}B undecoded", t.raw_transaction.len())
            };

            ("Transaction".into(), detail, None, None, None, None, tx_info)
        }
        PeerMessage::ProposeSet(p) => {
            let hash = hex::encode(&p.current_tx_hash[..std::cmp::min(8, p.current_tx_hash.len())]);
            let vkey = if !p.node_pub_key.is_empty() {
                Some(hex::encode(&p.node_pub_key))
            } else {
                None
            };
            ("Proposal".into(), format!("seq={} hash={hash}...", p.propose_seq), None, None, None, vkey, None)
        }
        PeerMessage::HaveTransactionSet(h) => {
            let hash = hex::encode(&h.hash[..std::cmp::min(8, h.hash.len())]);
            ("HaveTxSet".into(), format!("hash={hash}..."), None, None, None, None, None)
        }
        PeerMessage::Endpoints(e) => {
            let peers: Vec<PeerEntry> = e.endpoints_v2.iter()
                .map(|ep| PeerEntry {
                    endpoint: ep.endpoint.clone(),
                    hops: ep.hops,
                })
                .collect();
            let detail = format!("{} peers", peers.len());
            ("Endpoints".into(), detail, None, None, Some(peers), None, None)
        }
        PeerMessage::GetLedger(g) => {
            ("GetLedger".into(), format!("seq={:?}", g.ledger_seq), None, None, None, None, None)
        }
        PeerMessage::LedgerData(d) => {
            ("LedgerData".into(), format!("{} nodes seq={}", d.nodes.len(), d.ledger_seq), None, None, None, None, None)
        }
        PeerMessage::Ping(p) => {
            let kind = if p.r#type == 0 { "ping" } else { "pong" };
            ("Ping".into(), format!("{kind} seq={:?}", p.seq), None, None, None, None, None)
        }
        other => (other.name().into(), String::new(), None, None, None, None, None),
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

/// Serve a static HTML file from the static dir (disk, not compiled in).
/// Falls back to compiled-in validator.html for "/" if disk file missing.
async fn serve_static(filename: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Try disk paths in order: ./static/, crate static dir
    let paths = [
        std::path::PathBuf::from(format!("static/{filename}")),
        std::path::PathBuf::from(format!("crates/xrpl-node/static/{filename}")),
    ];
    for path in &paths {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            return axum::response::Html(content).into_response();
        }
    }
    // Fallback: compiled-in validator.html for index
    axum::response::Html("Page not found").into_response()
}

async fn index_page() -> axum::response::Response { serve_static("viewer.html").await }
async fn consensus_page() -> axum::response::Response { serve_static("consensus.html").await }
async fn metrics_page() -> axum::response::Response { serve_static("metrics.html").await }
async fn peers_page() -> axum::response::Response { serve_static("peers.html").await }
async fn state_page() -> axum::response::Response { serve_static("state.html").await }
async fn validator_page() -> axum::response::Response { serve_static("validator.html").await }
async fn incidents_page() -> axum::response::Response { serve_static("incidents.html").await }
async fn historical_page() -> axum::response::Response { serve_static("historical.html").await }
async fn network_page() -> axum::response::Response { serve_static("network.html").await }

/// Catch-all: serve any page from static/ by name (e.g., /network → network.html)
async fn catch_all_page(axum::extract::Path(page): axum::extract::Path<String>) -> axum::response::Response {
    // Sanitize: only allow alphanumeric + hyphens + underscores
    let clean: String = page.chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
    if clean.is_empty() { return serve_static("validator.html").await; }
    serve_static(&format!("{clean}.html")).await
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
