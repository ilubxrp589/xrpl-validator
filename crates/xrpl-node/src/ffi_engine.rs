//! FFI integration — delegates tx application to libxrpl (rippled's C++ engine).
//!
//! Only compiled with `--features ffi`. Provides:
//! - `RocksDbProvider`: implements xrpl_ffi::SleProvider over a RocksDB snapshot.
//! - `FfiStats`: counters + last-result tracking for the dashboard.
//! - `health_check`: runs a self-test on the hardcoded mainnet tx to verify FFI
//!   is working end-to-end. Returns a summary for the /api/ffi-status endpoint.

use std::sync::Arc;

use parking_lot::Mutex;
use xrpl_ffi::{
    apply_with_mutations, libxrpl_version, parse_tx, preflight, LedgerInfo,
    MemoryProvider, SleProvider,
};

/// Implements SleProvider over a RocksDB snapshot (for future production use).
pub struct RocksDbProvider<'a> {
    snapshot: rocksdb::Snapshot<'a>,
    /// Arena keeps bytes alive for the duration of apply().
    arena: parking_lot::Mutex<Vec<Vec<u8>>>,
}

impl<'a> RocksDbProvider<'a> {
    pub fn new(db: &'a rocksdb::DB) -> Self {
        Self {
            snapshot: db.snapshot(),
            arena: parking_lot::Mutex::new(Vec::with_capacity(16)),
        }
    }
}

impl<'a> SleProvider for RocksDbProvider<'a> {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]> {
        let data = self.snapshot.get(key).ok().flatten()?;
        let mut arena = self.arena.lock();
        arena.push(data);
        // SAFETY: the arena keeps the buffer alive; we just need to extend the
        // lifetime from the MutexGuard to &self. The arena is only appended to
        // during apply() (single-threaded from C++'s perspective).
        unsafe {
            let last = arena.last().unwrap();
            Some(std::slice::from_raw_parts(last.as_ptr(), last.len()))
        }
    }
}

/// Fetch currently-active mainnet amendments from rippled.
///
/// Returns a list of 32-byte feature IDs. Used to construct Rules for
/// preflight/apply — without these, amendment-gated txs fail with tefEXCEPTION.
///
/// Retries up to 10 times with exponential backoff to handle rippled overload
/// at startup — a ledger with 0 amendments would cause massive divergence.
pub fn fetch_mainnet_amendments(rpc_url: &str) -> Vec<[u8; 32]> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut backoff_ms: u64 = 250;
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            backoff_ms = (backoff_ms * 2).min(8000);
        }
        // Amendments singleton keylet
        let resp = match client
            .post(rpc_url)
            .json(&serde_json::json!({
                "method": "ledger_entry",
                "params": [{
                    "index": "7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4",
                    "ledger_index": "validated",
                    "binary": false
                }]
            }))
            .send()
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        let body_text = match resp.text() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let tt = body_text.trim();
        if tt.starts_with("Server is overloaded") || tt.starts_with("Server too busy") {
            continue;
        }
        let body: serde_json::Value = match serde_json::from_str(&body_text) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let empty = Vec::new();
        let amendments = body["result"]["node"]["Amendments"]
            .as_array()
            .unwrap_or(&empty);
        if amendments.is_empty() {
            // Most likely rippled returned an error — retry
            continue;
        }
        let mut out = Vec::with_capacity(amendments.len());
        for a in amendments {
            if let Some(hex_str) = a.as_str() {
                if let Ok(bytes) = hex::decode(hex_str) {
                    if bytes.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&bytes);
                        out.push(arr);
                    }
                }
            }
        }
        return out;
    }
    Vec::new()
}

/// RPC-backed SleProvider: fetches SLEs synchronously from rippled via
/// `ledger_entry` RPC at a fixed pre-ledger index. Slow but works from anywhere.
///
/// Supports a list of RPC endpoints for failover. On "Server is overloaded"
/// or network errors, rotates to the next endpoint before retrying.
pub struct RpcProvider {
    client: reqwest::blocking::Client,
    rpc_urls: Vec<String>,
    ledger_index: u32,
    arena: parking_lot::Mutex<Vec<Vec<u8>>>,
    pub hits: std::sync::atomic::AtomicU64,
    pub misses: std::sync::atomic::AtomicU64,
    pub miss_keys: parking_lot::Mutex<Vec<String>>,
    /// Next endpoint index to try first (round-robin on failover).
    next_url_idx: std::sync::atomic::AtomicUsize,
}

impl RpcProvider {
    /// Single-endpoint constructor (backwards compatible).
    pub fn new(rpc_url: String, ledger_index: u32) -> Self {
        Self::with_endpoints(vec![rpc_url], ledger_index)
    }

    /// Multi-endpoint constructor. Endpoints are tried in order, starting
    /// from the last-successful one (sticky), failing over on overload/error.
    pub fn with_endpoints(rpc_urls: Vec<String>, ledger_index: u32) -> Self {
        assert!(!rpc_urls.is_empty(), "at least one RPC endpoint required");
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest blocking client"),
            rpc_urls,
            ledger_index,
            arena: parking_lot::Mutex::new(Vec::with_capacity(16)),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            miss_keys: parking_lot::Mutex::new(Vec::new()),
            next_url_idx: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl SleProvider for RpcProvider {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]> {
        use std::sync::atomic::Ordering;
        let key_hex = hex::encode_upper(key);
        // Retry loop for transient "server overloaded" and network errors.
        // Up to 6 attempts, rotating endpoints after each failure with
        // exponential backoff (10ms, 20, 40, 80, 160, 320ms).
        let mut backoff_ms: u64 = 10;
        let mut last_err = String::from("no attempts");
        let mut url_idx = self.next_url_idx.load(Ordering::Relaxed) % self.rpc_urls.len();
        for attempt in 0..6 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms *= 2;
                url_idx = (url_idx + 1) % self.rpc_urls.len();
            }
            let rpc_url = &self.rpc_urls[url_idx];
            let resp = match self
                .client
                .post(rpc_url)
                .json(&serde_json::json!({
                    "method": "ledger_entry",
                    "params": [{
                        "index": key_hex,
                        "ledger_index": self.ledger_index,
                        "binary": true
                    }]
                }))
                .send()
            {
                Ok(r) => r,
                Err(e) => { last_err = format!("send_err: {e}"); continue; }
            };
            // Read raw text first so we can detect non-JSON overload responses
            let body_text = match resp.text() {
                Ok(t) => t,
                Err(e) => { last_err = format!("body_err: {e}"); continue; }
            };
            let tt = body_text.trim();
            if tt.starts_with("Server is overloaded")
                || tt.starts_with("Server too busy")
                || tt.contains("service unavailable")
            {
                last_err = "rippled_overloaded".into();
                continue;
            }
            let body: serde_json::Value = match serde_json::from_str(&body_text) {
                Ok(b) => b,
                Err(_) => { last_err = format!("non_json: {}", &tt[..tt.len().min(40)]); continue; }
            };
            // Check for error responses that mean we should retry
            if let Some(err_str) = body["result"]["error"].as_str() {
                // entryNotFound is a legit "this SLE doesn't exist" — don't retry.
                if err_str == "entryNotFound" {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    let mut mk = self.miss_keys.lock();
                    if mk.len() < 20 {
                        mk.push(format!("{}@{}: entryNotFound", &key_hex[..16], self.ledger_index));
                    }
                    return None;
                }
                // Other errors (overloaded, invalidParams, etc.) — retry.
                last_err = err_str.to_string();
                continue;
            }
            let data_hex = match body["result"]["node_binary"].as_str() {
                Some(s) => s,
                None => {
                    last_err = "no_node_binary".into();
                    continue;
                }
            };
            let bytes = match hex::decode(data_hex) {
                Ok(b) => b,
                Err(_) => { last_err = "bad_hex".into(); continue; }
            };
            self.hits.fetch_add(1, Ordering::Relaxed);
            // Remember which endpoint worked (sticky) so the next call
            // doesn't pay the cost of re-discovering a healthy endpoint.
            self.next_url_idx.store(url_idx, Ordering::Relaxed);
            let mut arena = self.arena.lock();
            arena.push(bytes);
            unsafe {
                let last = arena.last().unwrap();
                return Some(std::slice::from_raw_parts(last.as_ptr(), last.len()));
            }
        }
        // All retries exhausted
        self.misses.fetch_add(1, Ordering::Relaxed);
        let mut mk = self.miss_keys.lock();
        if mk.len() < 20 {
            mk.push(format!("{}@{}: EXHAUSTED {}", &key_hex[..16], self.ledger_index, last_err));
        }
        None
    }
}

/// A rocksdb snapshot bundled with an Arc<DB> that keeps it alive. The
/// snapshot borrows from the DB, but we own the Arc, so the borrow is
/// safe for the lifetime of this struct. This makes the snapshot Send +
/// 'static so it can cross thread/task boundaries (e.g. into
/// spawn_blocking for FFI verify).
///
/// SAFETY invariant: `snapshot` is dropped before `_db`. Rust's struct
/// field drop order (declaration order) guarantees this. The 'static
/// lifetime on the snapshot is a managed lie — the snapshot is only valid
/// for as long as this struct (and its inner Arc<DB>) is alive.
pub struct OwnedSnapshot {
    // Field order matters: snapshot must drop before _db.
    snapshot: rocksdb::Snapshot<'static>,
    _db: std::sync::Arc<rocksdb::DB>,
}

// SAFETY: rocksdb::Snapshot is inherently Send once we control the DB
// lifetime ourselves (via the Arc).
unsafe impl Send for OwnedSnapshot {}
unsafe impl Sync for OwnedSnapshot {}

impl OwnedSnapshot {
    pub fn new(db: std::sync::Arc<rocksdb::DB>) -> Self {
        // Take snapshot against &DB dereferenced from Arc.
        let snapshot: rocksdb::Snapshot<'_> = db.snapshot();
        // SAFETY: we're about to store `db` inside the same struct as this
        // snapshot. The snapshot borrows from `*db`. Drop order ensures
        // snapshot is dropped before db. The 'static lifetime here is
        // upheld by struct invariant, not by the compiler.
        let snapshot: rocksdb::Snapshot<'static> =
            unsafe { std::mem::transmute(snapshot) };
        Self { snapshot, _db: db }
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, rocksdb::Error> {
        self.snapshot.get(key)
    }
}

/// Three-tier SLE provider: overlay (in-ledger mutations) → OwnedSnapshot
/// of the validator's state at the PRE-ledger boundary → RpcProvider
/// fallback. Cuts RPC load by 90%+ once the DB is populated.
///
/// The snapshot must be taken BEFORE any concurrent writes to the DB that
/// would reflect post-ledger state. See ws_sync.rs for the ordering
/// contract.
pub struct OverlayedDbProvider<'a> {
    overlay: &'a parking_lot::Mutex<std::collections::HashMap<[u8; 32], Option<Vec<u8>>>>,
    snapshot: &'a OwnedSnapshot,
    rpc_fallback: &'a RpcProvider,
    arena: parking_lot::Mutex<Vec<Vec<u8>>>,
    pub db_hits: std::sync::atomic::AtomicU64,
    pub rpc_fallbacks: std::sync::atomic::AtomicU64,
}

impl<'a> OverlayedDbProvider<'a> {
    pub fn new(
        overlay: &'a parking_lot::Mutex<std::collections::HashMap<[u8; 32], Option<Vec<u8>>>>,
        snapshot: &'a OwnedSnapshot,
        rpc_fallback: &'a RpcProvider,
    ) -> Self {
        Self {
            overlay,
            snapshot,
            rpc_fallback,
            arena: parking_lot::Mutex::new(Vec::with_capacity(16)),
            db_hits: std::sync::atomic::AtomicU64::new(0),
            rpc_fallbacks: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl<'a> SleProvider for OverlayedDbProvider<'a> {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]> {
        use std::sync::atomic::Ordering;
        // 1. Overlay (mutations from prior txs in this ledger)
        {
            let ov = self.overlay.lock();
            if let Some(entry) = ov.get(key) {
                match entry {
                    None => return None, // tombstone
                    Some(bytes) => {
                        let mut arena = self.arena.lock();
                        arena.push(bytes.clone());
                        unsafe {
                            let last = arena.last().unwrap();
                            return Some(std::slice::from_raw_parts(last.as_ptr(), last.len()));
                        }
                    }
                }
            }
        }
        // 2. Owned snapshot (validator's own state, pre-ledger)
        if let Ok(Some(data)) = self.snapshot.get(key) {
            self.db_hits.fetch_add(1, Ordering::Relaxed);
            let mut arena = self.arena.lock();
            arena.push(data);
            unsafe {
                let last = arena.last().unwrap();
                return Some(std::slice::from_raw_parts(last.as_ptr(), last.len()));
            }
        }
        // 3. RPC fallback (unsynced SLE, or DB was never populated with it)
        self.rpc_fallbacks.fetch_add(1, Ordering::Relaxed);
        self.rpc_fallback.read(key)
    }
}

/// Provider that checks an in-memory overlay first (for cross-tx state within
/// a single ledger), then falls back to RPC. Used by apply_ledger_in_order.
///
/// Overlay values: `Some(bytes)` = modified SLE, `None` = tombstone (deleted
/// this ledger — RPC must NOT be queried for it, since rippled still has the
/// pre-ledger version which is now stale).
pub struct LayeredProvider<'a> {
    overlay: &'a parking_lot::Mutex<std::collections::HashMap<[u8; 32], Option<Vec<u8>>>>,
    fallback: &'a RpcProvider,
    arena: parking_lot::Mutex<Vec<Vec<u8>>>,
}

impl<'a> LayeredProvider<'a> {
    pub fn new(
        overlay: &'a parking_lot::Mutex<std::collections::HashMap<[u8; 32], Option<Vec<u8>>>>,
        fallback: &'a RpcProvider,
    ) -> Self {
        Self {
            overlay,
            fallback,
            arena: parking_lot::Mutex::new(Vec::with_capacity(16)),
        }
    }
}

impl<'a> SleProvider for LayeredProvider<'a> {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]> {
        // Check overlay first (mutations from prior txs in this ledger)
        {
            let overlay = self.overlay.lock();
            if let Some(entry) = overlay.get(key) {
                match entry {
                    None => return None, // tombstone — do NOT fall back to RPC
                    Some(bytes) => {
                        let mut arena = self.arena.lock();
                        arena.push(bytes.clone());
                        unsafe {
                            let last = arena.last().unwrap();
                            return Some(std::slice::from_raw_parts(last.as_ptr(), last.len()));
                        }
                    }
                }
            }
        }
        // Fallback to RPC
        self.fallback.read(key)
    }
}

/// Live FFI stats for the dashboard.
#[derive(Clone, Default, serde::Serialize)]
pub struct FfiStats {
    pub libxrpl_version: String,
    // Self-test (same hardcoded tx in a loop)
    pub health_checks: u64,
    pub health_passed: u64,
    pub last_ter: i32,
    pub last_ter_name: String,
    pub last_applied: bool,
    pub last_mutations: usize,
    pub last_check_ms: u64,
    // Live mainnet tx processing (parse + preflight)
    pub live_txs_parsed: u64,
    pub live_txs_preflight_ok: u64,
    pub live_txs_preflight_fail: u64,
    pub live_last_type: String,
    pub live_last_hash: String,
    pub live_last_ter: String,
    pub live_ledger_seq: u32,
    pub live_types_seen: std::collections::BTreeMap<String, u64>,
    // Full apply on live mainnet tx (with RPC-fetched state)
    pub live_apply_attempted: u64,
    pub live_apply_ok: u64,        // tesSUCCESS
    pub live_apply_claimed: u64,   // tec* (legit claimed failures — rippled also returned these)
    pub live_apply_diverged: u64,  // terPRE_SEQ / tef* / tem* / tel* (our processing issues)
    pub live_apply_failed: u64,    // deprecated — use diverged
    pub live_apply_last_ter: String,
    pub live_apply_last_mutations: usize,
    pub live_apply_last_ms: u64,
    pub live_apply_ter_counts: std::collections::BTreeMap<String, u64>,
    /// Divergence breakdown: "{tx_type}/{ter_name}" → count
    pub live_diverged_by_type: std::collections::BTreeMap<String, u64>,
    /// Total RPC SLE lookups (hit = found in rippled, miss = not found/error)
    pub rpc_sle_hits: u64,
    pub rpc_sle_misses: u64,
    /// RocksDB-backed SLE lookups (validator's own state, when configured).
    /// db_hits = served from local DB; db_rpc_fallbacks = fell through to RPC.
    pub db_hits: u64,
    pub db_rpc_fallbacks: u64,
    /// Sample of up to 20 miss keys from the last ledger applied
    pub rpc_sle_miss_samples: Vec<String>,
    /// Sample of up to 10 diverged tx hashes (tx_type/TER/hash) for mainnet lookup
    pub diverged_tx_samples: Vec<String>,
    /// Rolling buffer of the last 50 txs applied by the FFI engine, regardless
    /// of outcome. Format: "L{seq} {tx_type}/{ter_name} {short_hash} {ms}ms mut={N}"
    /// where short_hash is the first 16 chars. Lets dashboards + watch_engine.py
    /// show live per-tx activity without polling a separate endpoint.
    pub recent_tx_samples: Vec<String>,
    /// True when `XRPL_FFI_STAGE3=1` was read at `start_ws_sync` entry. Exposed
    /// via `/api/engine` so dashboards and watch_engine.py can show a big
    /// "STAGE 3: ACTIVE" banner without having to grep the log.
    pub stage3_enabled: bool,
    /// Apply-latency histogram buckets (milliseconds). Cumulative counts
    /// (Prometheus-style: bucket[i] = count of observations ≤ bucket_bound[i]).
    /// Bounds: 1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, +Inf
    pub apply_duration_buckets_ms: [u64; 11],
    pub apply_duration_count: u64,
    pub apply_duration_sum_ms: u64,
    /// Per-tx-type apply counts (independent of TER)
    pub apply_by_type: std::collections::BTreeMap<String, u64>,
    /// Number of ledgers fully applied (for throughput metrics)
    pub ledgers_applied: u64,
    /// Per-round (current ledger) tx type counts — resets each ledger.
    pub round_tx_types: std::collections::BTreeMap<String, u64>,
    pub round_tx_count: u64,
    pub round_ledger_seq: u32,
    /// Sum of fees (in drops) burned across all txs in the current ledger.
    /// Resets each ledger alongside `round_tx_count`. Cumulative across the
    /// process lifetime is in `total_fees_burned_drops`.
    pub round_fees_drops: u64,
    /// Lifetime sum of fees burned by every tx the FFI engine has processed
    /// in this process. Authoritative counterpart to the gossip-inflated
    /// `round_fees` exposed by the peer-relay tracker at the engine level.
    pub total_fees_burned_drops: u64,
    /// Shadow state hash — FFI-derived state vs rippled's account_hash.
    pub shadow_hash_attempted: u64,
    pub shadow_hash_matched: u64,
    pub shadow_hash_mismatched: u64,
    pub shadow_hash_last: String,
    pub shadow_hash_last_network: String,
    pub shadow_hash_last_matched: bool,
}

/// Histogram bucket bounds (in milliseconds) for apply latency.
pub const APPLY_LATENCY_BUCKETS_MS: [u64; 10] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000];

/// Render FfiStats as a Prometheus OpenMetrics exposition.
///
/// Exposes counters, gauges and one histogram. Labels follow Prometheus
/// conventions (snake_case metric names, value-typed labels).
pub fn render_prometheus(s: &FfiStats) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(4096);

    // libxrpl build info (gauge-as-label trick)
    let _ = writeln!(out, "# HELP xrpl_ffi_build_info libxrpl version info");
    let _ = writeln!(out, "# TYPE xrpl_ffi_build_info gauge");
    let _ = writeln!(
        out,
        "xrpl_ffi_build_info{{libxrpl_version=\"{}\"}} 1",
        escape_label(&s.libxrpl_version)
    );

    // Self-test health (gauge)
    let _ = writeln!(out, "# HELP xrpl_ffi_health_checks_total Self-test runs");
    let _ = writeln!(out, "# TYPE xrpl_ffi_health_checks_total counter");
    let _ = writeln!(out, "xrpl_ffi_health_checks_total {}", s.health_checks);
    let _ = writeln!(out, "# HELP xrpl_ffi_health_passed_total Self-test runs that passed");
    let _ = writeln!(out, "# TYPE xrpl_ffi_health_passed_total counter");
    let _ = writeln!(out, "xrpl_ffi_health_passed_total {}", s.health_passed);

    // Live feed — preflight
    let _ = writeln!(out, "# HELP xrpl_ffi_preflight_total Live mainnet txs run through parse+preflight");
    let _ = writeln!(out, "# TYPE xrpl_ffi_preflight_total counter");
    let _ = writeln!(out, "xrpl_ffi_preflight_total{{result=\"ok\"}} {}", s.live_txs_preflight_ok);
    let _ = writeln!(out, "xrpl_ffi_preflight_total{{result=\"fail\"}} {}", s.live_txs_preflight_fail);
    let _ = writeln!(out, "# HELP xrpl_ffi_parsed_total Live mainnet txs successfully parsed");
    let _ = writeln!(out, "# TYPE xrpl_ffi_parsed_total counter");
    let _ = writeln!(out, "xrpl_ffi_parsed_total {}", s.live_txs_parsed);

    // Live feed — full apply
    let _ = writeln!(out, "# HELP xrpl_ffi_apply_total Apply() calls by result class");
    let _ = writeln!(out, "# TYPE xrpl_ffi_apply_total counter");
    let _ = writeln!(out, "xrpl_ffi_apply_total{{result=\"tesSUCCESS\"}} {}", s.live_apply_ok);
    let _ = writeln!(out, "xrpl_ffi_apply_total{{result=\"tec_claimed\"}} {}", s.live_apply_claimed);
    let _ = writeln!(out, "xrpl_ffi_apply_total{{result=\"diverged\"}} {}", s.live_apply_diverged);
    let _ = writeln!(out, "xrpl_ffi_apply_attempted_total {}", s.live_apply_attempted);

    // Mainnet agreement ratio (gauge)
    let agreed = s.live_apply_ok + s.live_apply_claimed;
    let ratio: f64 = if s.live_apply_attempted > 0 {
        agreed as f64 / s.live_apply_attempted as f64
    } else {
        1.0
    };
    let _ = writeln!(out, "# HELP xrpl_ffi_mainnet_agreement_ratio Fraction of apply() results that matched mainnet (tesSUCCESS + tec*)");
    let _ = writeln!(out, "# TYPE xrpl_ffi_mainnet_agreement_ratio gauge");
    let _ = writeln!(out, "xrpl_ffi_mainnet_agreement_ratio {ratio:.6}");

    // Per-TER counts
    let _ = writeln!(out, "# HELP xrpl_ffi_apply_ter_total Apply() calls by TER code");
    let _ = writeln!(out, "# TYPE xrpl_ffi_apply_ter_total counter");
    for (ter, count) in &s.live_apply_ter_counts {
        let _ = writeln!(out, "xrpl_ffi_apply_ter_total{{ter=\"{}\"}} {}", escape_label(ter), count);
    }

    // Per-tx-type counts
    let _ = writeln!(out, "# HELP xrpl_ffi_apply_by_type_total Apply() calls by tx_type");
    let _ = writeln!(out, "# TYPE xrpl_ffi_apply_by_type_total counter");
    for (ty, count) in &s.apply_by_type {
        let _ = writeln!(out, "xrpl_ffi_apply_by_type_total{{tx_type=\"{}\"}} {}", escape_label(ty), count);
    }

    // Divergence breakdown (tx_type × TER)
    let _ = writeln!(out, "# HELP xrpl_ffi_diverged_total Diverged apply results by tx_type and TER");
    let _ = writeln!(out, "# TYPE xrpl_ffi_diverged_total counter");
    for (combo, count) in &s.live_diverged_by_type {
        if let Some((ty, ter)) = combo.split_once('/') {
            let _ = writeln!(
                out,
                "xrpl_ffi_diverged_total{{tx_type=\"{}\",ter=\"{}\"}} {}",
                escape_label(ty), escape_label(ter), count
            );
        }
    }

    // Apply-latency histogram (convert ms → seconds for Prometheus convention)
    let _ = writeln!(out, "# HELP xrpl_ffi_apply_duration_seconds Apply() latency in seconds");
    let _ = writeln!(out, "# TYPE xrpl_ffi_apply_duration_seconds histogram");
    for (i, bound_ms) in APPLY_LATENCY_BUCKETS_MS.iter().enumerate() {
        let le = *bound_ms as f64 / 1000.0;
        let _ = writeln!(
            out,
            "xrpl_ffi_apply_duration_seconds_bucket{{le=\"{le}\"}} {}",
            s.apply_duration_buckets_ms[i]
        );
    }
    let _ = writeln!(
        out,
        "xrpl_ffi_apply_duration_seconds_bucket{{le=\"+Inf\"}} {}",
        s.apply_duration_buckets_ms[10]
    );
    let _ = writeln!(
        out,
        "xrpl_ffi_apply_duration_seconds_sum {:.3}",
        s.apply_duration_sum_ms as f64 / 1000.0
    );
    let _ = writeln!(out, "xrpl_ffi_apply_duration_seconds_count {}", s.apply_duration_count);

    // RPC provider
    let _ = writeln!(out, "# HELP xrpl_ffi_rpc_sle_lookups_total SLE lookups via rippled RPC");
    let _ = writeln!(out, "# TYPE xrpl_ffi_rpc_sle_lookups_total counter");
    let _ = writeln!(out, "xrpl_ffi_rpc_sle_lookups_total{{result=\"hit\"}} {}", s.rpc_sle_hits);
    let _ = writeln!(out, "xrpl_ffi_rpc_sle_lookups_total{{result=\"miss\"}} {}", s.rpc_sle_misses);

    // RocksDB provider (if validator has local state available)
    let _ = writeln!(out, "# HELP xrpl_ffi_db_sle_lookups_total SLE lookups via local RocksDB snapshot");
    let _ = writeln!(out, "# TYPE xrpl_ffi_db_sle_lookups_total counter");
    let _ = writeln!(out, "xrpl_ffi_db_sle_lookups_total{{result=\"hit\"}} {}", s.db_hits);
    let _ = writeln!(out, "xrpl_ffi_db_sle_lookups_total{{result=\"rpc_fallback\"}} {}", s.db_rpc_fallbacks);

    // Ledger progress
    let _ = writeln!(out, "# HELP xrpl_ffi_ledgers_applied_total Ledgers fully applied via FFI");
    let _ = writeln!(out, "# TYPE xrpl_ffi_ledgers_applied_total counter");
    let _ = writeln!(out, "xrpl_ffi_ledgers_applied_total {}", s.ledgers_applied);
    let _ = writeln!(out, "# HELP xrpl_ffi_live_ledger_seq Most recent ledger sequence fed through FFI");
    let _ = writeln!(out, "# TYPE xrpl_ffi_live_ledger_seq gauge");
    let _ = writeln!(out, "xrpl_ffi_live_ledger_seq {}", s.live_ledger_seq);

    out
}

/// Escape a Prometheus label value per OpenMetrics rules.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Append-only JSONL log for every diverged tx. Survives restarts.
///
/// One record per divergence. Opening the file is lazy + per-line flushed
/// so readers can tail it live. Path defaults to logs/divergences.jsonl
/// under the cwd; override via XRPL_FFI_DIVERGENCE_LOG env var.
pub struct DivergenceLog {
    path: std::path::PathBuf,
    file: parking_lot::Mutex<Option<std::fs::File>>,
}

impl DivergenceLog {
    pub fn new() -> Self {
        let path = std::env::var("XRPL_FFI_DIVERGENCE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/divergences.jsonl"));
        Self { path, file: parking_lot::Mutex::new(None) }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Record one divergence. Swallows I/O errors (divergence recording must
    /// never crash the apply loop).
    pub fn record(
        &self,
        ledger_seq: u32,
        tx_hash: &str,
        tx_type: &str,
        our_ter: &str,
        duration_ms: u64,
        fatal: &str,
    ) {
        use std::io::Write;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = serde_json::json!({
            "ts": ts,
            "ledger_seq": ledger_seq,
            "tx_hash": tx_hash,
            "tx_type": tx_type,
            "our_ter": our_ter,
            "duration_ms": duration_ms,
            "fatal": fatal,
        });
        let mut guard = self.file.lock();
        if guard.is_none() {
            // Ensure parent directory exists
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let opened = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path);
            if let Ok(f) = opened {
                *guard = Some(f);
            } else {
                return;
            }
        }
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

impl Default for DivergenceLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the fee (in drops) from a serialized XRPL transaction.
///
/// In canonical XRPL binary form, sfFee (type 6, field 8) is encoded as
/// field code 0x68 followed by an 8-byte STAmount in native-XRP form. The
/// high byte of that 8-byte block has the layout:
///
///     bit 7 (0x80) = 0 → native XRP (clear) / 1 → IOU
///     bit 6 (0x40) = 1 → positive       / 0 → negative
///     bits 5..0    = top 6 bits of the 62-bit drops value
///
/// Fees are always native and positive, so the high byte is `0x40 | (top6)`.
/// The 2-byte pattern `[0x68, 0x4_]` (where the second byte's top two bits
/// are exactly `01`) is far more selective than scanning for `0x68` alone —
/// random `0x68` bytes inside SigningPubKey or TxnSignature blobs almost
/// never have the matching native-positive flag in the next byte.
///
/// Canonical field order also puts sfFee BEFORE sfSigningPubKey and
/// sfTxnSignature in the binary tx, so the first hit when scanning forward
/// is overwhelmingly the real fee. Returns the fee in drops, or `None` if
/// no plausible match is found.
/// Scan a serialized AccountRoot SLE for the `Sequence` field. UInt32 field 4
/// is encoded as byte `0x24` followed by 4 big-endian bytes. Used by debug
/// tooling and tests to pull the Sequence out of raw overlay bytes without a
/// full SLE parse.
pub fn scan_sequence_in_account_root(data: &[u8]) -> Option<u32> {
    for i in 0..data.len().saturating_sub(5) {
        if data[i] == 0x24 {
            return Some(u32::from_be_bytes(data[i + 1..i + 5].try_into().ok()?));
        }
    }
    None
}

pub fn extract_fee_drops(tx_bytes: &[u8]) -> Option<u64> {
    if tx_bytes.len() < 9 {
        return None;
    }
    let end = tx_bytes.len() - 9;
    for i in 0..=end {
        if tx_bytes[i] == 0x68 && (tx_bytes[i + 1] & 0xC0) == 0x40 {
            let mut val: u64 = ((tx_bytes[i + 1] & 0x3F) as u64) << 56;
            val |= (tx_bytes[i + 2] as u64) << 48;
            val |= (tx_bytes[i + 3] as u64) << 40;
            val |= (tx_bytes[i + 4] as u64) << 32;
            val |= (tx_bytes[i + 5] as u64) << 24;
            val |= (tx_bytes[i + 6] as u64) << 16;
            val |= (tx_bytes[i + 7] as u64) << 8;
            val |= tx_bytes[i + 8] as u64;
            return Some(val);
        }
    }
    None
}

/// Extract the sender's AccountRoot keylet from a serialized XRPL transaction.
///
/// In canonical XRPL binary form, sfAccount is encoded as:
///     [0x81] [0x14] [20 bytes of AccountID]
///
/// where 0x81 is the field code (type 8 / field 1) and 0x14 (20) is the
/// VL-encoded length. We scan for this 2-byte pattern rather than just 0x81
/// because random 0x81 bytes commonly appear inside SigningPubKey (33 bytes)
/// and TxnSignature (~70 bytes), which canonically come BEFORE sfAccount in
/// the serialized tx.
///
/// Returns the AccountRoot SLE keylet (`sha512_half(0x0061 || account_id)[..32]`),
/// ready for direct use with the SLE provider.
pub fn extract_sender_account_root_key(tx_bytes: &[u8]) -> Option<[u8; 32]> {
    if tx_bytes.len() < 22 {
        return None;
    }
    let end = tx_bytes.len() - 22;
    for i in 0..=end {
        if tx_bytes[i] == 0x81 && tx_bytes[i + 1] == 0x14 {
            use sha2::{Digest, Sha512};
            let mut h = Sha512::new();
            h.update([0x00u8, 0x61]); // account root keylet prefix
            h.update(&tx_bytes[i + 2..i + 22]);
            let hash = h.finalize();
            let mut key = [0u8; 32];
            key.copy_from_slice(&hash[..32]);
            return Some(key);
        }
    }
    None
}

pub type SharedFfiStats = Arc<Mutex<FfiStats>>;

pub fn new_stats() -> SharedFfiStats {
    Arc::new(Mutex::new(FfiStats {
        libxrpl_version: libxrpl_version(),
        ..Default::default()
    }))
}

/// Hardcoded mainnet Payment tx for self-test (matches Rust integration test).
/// Hash: 39077702C3FCE0DDC5693065FC0DA35576E4D0112FDEA08D6CAD099074033ABA
const HEALTH_TX_HEX: &str = concat!(
    "12000022800000002404E59B6E61400000000000000A68400000000000000F",
    "732103ECD7DE6564273713EE4EA28A1F4522B3B480F66DC903EB4E5309D32F",
    "633A6DAC74463044022074B318BE47C213C5B9A57341A454033D3CDF97FBB6",
    "998FDA654B4F879A9C1C6502204F520B978C98C8F857B8111FD5E00E94EE16",
    "A2C503EF522FDFA7B0131201051A8114E5A5902FEBDA49C3BDE5B7E4522C62",
    "F3D49E4666831492BD9E89265D9F5853BF1AAB400766CDDBDAEC3CF9EA7D1D",
    "526566657272616C207265776172642066726F6D204D61676E65746963E1F1"
);
const HEALTH_SENDER_KEY: &str =
    "CED60F22A245F8DE393F2351C5097A81836153584DC3C24B803FA1B9906A506A";
const HEALTH_SENDER_SLE: &str = concat!(
    "11006122000000002404E59B6E25062910882D0000000055B8E0C782ADB99C",
    "79445A6C72A3553C6FFD6E7BC529A906DF6FEEE09F439EADFA62400000000776FE378",
    "114E5A5902FEBDA49C3BDE5B7E4522C62F3D49E4666"
);
const HEALTH_DEST_KEY: &str =
    "5180078F1F6E062E4F01B17D6D05E734DF5976F0EDB187ED9EBF652A6F47D28D";
const HEALTH_DEST_SLE: &str = concat!(
    "1100612200000000240432660C2506290E532D00000012553059070AA6AB0E",
    "4DEAC9CEE66914270E0FAD957168D465B657C359132498A644",
    "62400000000C234F9F8",
    "11492BD9E89265D9F5853BF1AAB400766CDDBDAEC3C"
);

fn hex_to_array32(s: &str) -> [u8; 32] {
    let v = hex::decode(s).unwrap();
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    a
}

/// Run a full FFI self-test: parse → preflight → apply with mutations.
/// Returns true if all 3 stages succeed with the expected results.
pub fn health_check(stats: &SharedFfiStats) -> bool {
    let t0 = std::time::Instant::now();

    let tx = match hex::decode(HEALTH_TX_HEX) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Parse
    let parsed = match parse_tx(&tx) {
        Some(p) => p,
        None => {
            let mut s = stats.lock();
            s.health_checks += 1;
            s.last_ter_name = "parse_failed".into();
            return false;
        }
    };
    if parsed.tx_type != "Payment" {
        return false;
    }

    // Preflight
    let pf = preflight(&tx, &[], 0, 0);
    if !pf.is_success() {
        let mut s = stats.lock();
        s.health_checks += 1;
        s.last_ter = pf.ter;
        s.last_ter_name = pf.ter_name;
        return false;
    }

    // Apply with in-memory provider (production would use RocksDbProvider)
    let mut provider = MemoryProvider::new();
    provider.insert(
        hex_to_array32(HEALTH_SENDER_KEY),
        hex::decode(HEALTH_SENDER_SLE).unwrap(),
    );
    provider.insert(
        hex_to_array32(HEALTH_DEST_KEY),
        hex::decode(HEALTH_DEST_SLE).unwrap(),
    );

    let ledger = LedgerInfo {
        seq: 103354511,
        parent_close_time: 797193960,
        total_drops: 99985687626634189,
        parent_hash: [0u8; 32],
        base_fee_drops: 10,
        reserve_drops: 10_000_000,
        increment_drops: 2_000_000,
    };

    let outcome = match apply_with_mutations(&tx, &[], &ledger, &provider, 0, 0) {
        Some(o) => o,
        None => return false,
    };

    let elapsed_ms = t0.elapsed().as_micros() as u64 / 1000;
    let passed = outcome.is_success() && outcome.applied && outcome.mutations.len() == 2;

    let mut s = stats.lock();
    s.health_checks += 1;
    if passed {
        s.health_passed += 1;
    }
    s.last_ter = outcome.ter;
    s.last_ter_name = outcome.ter_name;
    s.last_applied = outcome.applied;
    s.last_mutations = outcome.mutations.len();
    s.last_check_ms = elapsed_ms;
    passed
}

/// Process a single live mainnet tx: parse + preflight via FFI, update stats.
/// Does NOT call apply (no state access). Just validates signature + format
/// against libxrpl's canonical rules.
pub fn process_live_tx(stats: &SharedFfiStats, tx_bytes: &[u8], ledger_seq: u32) {
    let parsed = match parse_tx(tx_bytes) {
        Some(p) => p,
        None => return,
    };
    let pf = preflight(tx_bytes, &[], 0, 0);

    let mut s = stats.lock();
    s.live_txs_parsed += 1;
    if pf.is_success() {
        s.live_txs_preflight_ok += 1;
    } else {
        s.live_txs_preflight_fail += 1;
    }
    s.live_last_type = parsed.tx_type.clone();
    s.live_last_hash = hex::encode_upper(parsed.hash);
    s.live_last_ter = pf.ter_name.clone();
    s.live_ledger_seq = ledger_seq;
    *s.live_types_seen.entry(parsed.tx_type).or_insert(0) += 1;
}

/// Apply an entire ledger's worth of txs IN ORDER through libxrpl via FFI.
///
/// Each tx sees the state left by prior txs in the same ledger (sender's
/// sequence is bumped between applications, etc). This eliminates spurious
/// terPRE_SEQ failures that occur when applying txs in isolation.
///
/// - `txs_in_order`: binary tx blobs sorted by TransactionIndex
/// - `ledger_seq`: sequence of the ledger these txs close
/// - `rpc_url`: rippled RPC for pre-state lookups
/// - `amendments`: active amendment feature IDs
/// Accumulated mutations from a full ledger apply. Keys map to `Some(data)`
/// for created/modified SLEs, `None` for deleted (tombstoned) SLEs.
pub type LedgerOverlay = std::collections::HashMap<[u8; 32], Option<Vec<u8>>>;


pub fn apply_ledger_in_order(
    stats: &SharedFfiStats,
    txs_in_order: &[Vec<u8>],
    ledger_seq: u32,
    rpc_urls: &[String],
    amendments: &[[u8; 32]],
    parent_hash: [u8; 32],
    parent_close_time: u32,
    total_drops: u64,
    divergence_log: Option<&DivergenceLog>,
    db_snapshot: Option<&OwnedSnapshot>,
) -> LedgerOverlay {
    let fallback = RpcProvider::with_endpoints(rpc_urls.to_vec(), ledger_seq.saturating_sub(1));
    let overlay: parking_lot::Mutex<std::collections::HashMap<[u8; 32], Option<Vec<u8>>>> =
        parking_lot::Mutex::new(std::collections::HashMap::new());
    let ledger = LedgerInfo {
        seq: ledger_seq,
        parent_close_time,
        total_drops,
        parent_hash,
        base_fee_drops: 10,
        reserve_drops: 1_000_000,   // Mainnet: 1 XRP
        increment_drops: 200_000,   // Mainnet: 0.2 XRP
    };

    // Reset per-round stats for this ledger
    {
        let mut s = stats.lock();
        s.round_tx_types.clear();
        s.round_tx_count = 0;
        s.round_fees_drops = 0;
        s.round_ledger_seq = ledger_seq;
    }
    let mut tx_num = 0u32;
    let mut overlay_hits = 0u32;
    for tx_bytes in txs_in_order {
        tx_num += 1;
        // Parse to get tx_type + hash (for diagnostic buckets)
        let (tx_type, tx_hash) = xrpl_ffi::parse_tx(tx_bytes)
            .map(|p| (p.tx_type, hex::encode_upper(p.hash)))
            .unwrap_or_else(|| ("Unknown".to_string(), String::new()));
        let t0 = std::time::Instant::now();
        // Use DB-first provider when a pre-ledger OwnedSnapshot is supplied.
        // The snapshot is stable (rocksdb MVCC) — safe against concurrent writes.
        let outcome_opt = if let Some(snap) = db_snapshot {
            let provider = OverlayedDbProvider::new(&overlay, snap, &fallback);
            let r = xrpl_ffi::apply_with_mutations(tx_bytes, amendments, &ledger, &provider, 0, 0);
            // Accumulate provider counters into stats
            use std::sync::atomic::Ordering;
            let h = provider.db_hits.load(Ordering::Relaxed);
            let f = provider.rpc_fallbacks.load(Ordering::Relaxed);
            if h > 0 || f > 0 {
                let mut s = stats.lock();
                s.db_hits += h;
                s.db_rpc_fallbacks += f;
            }
            r
        } else {
            let provider = LayeredProvider::new(&overlay, &fallback);
            xrpl_ffi::apply_with_mutations(tx_bytes, amendments, &ledger, &provider, 0, 0)
        };
        let outcome = match outcome_opt {
            Some(o) => o,
            None => {
                let mut s = stats.lock();
                s.live_apply_attempted += 1;
                s.live_apply_diverged += 1;
                s.live_apply_failed += 1;
                *s.live_apply_ter_counts.entry("FFI_NULL".into()).or_insert(0) += 1;
                *s.live_diverged_by_type.entry(format!("{tx_type}/FFI_NULL")).or_insert(0) += 1;
                continue;
            }
        };
        let elapsed_ms = t0.elapsed().as_micros() as u64 / 1000;

        // Thread mutations into overlay for subsequent txs — ONLY for tesSUCCESS
        // or tec* (claimed). ter*/tef*/tem*/tel* return no state changes in
        // rippled, so threading partial/empty mutations corrupts the overlay.
        let should_thread = outcome.ter_name == "tesSUCCESS"
            || outcome.ter_name.starts_with("tec");
        let prior_overlay_size;
        let post_overlay_size;
        {
            let mut ov = overlay.lock();
            prior_overlay_size = ov.len();
            if should_thread {
                for m in &outcome.mutations {
                    match m.kind {
                        xrpl_ffi::MutationKind::Deleted => {
                            // Insert tombstone so fallback RPC won't serve stale pre-state
                            ov.insert(m.key, None);
                        }
                        _ => {
                            ov.insert(m.key, Some(m.data.clone()));
                        }
                    }
                }
            }
            post_overlay_size = ov.len();
        }
        // terPRE_SEQ recovery: sender's sequence in overlay is stale because
        // a prior tx from the same sender had a failure path that didn't
        // thread mutations correctly. Fetch their AccountRoot at the CURRENT
        // ledger (post-state) from a fresh RPC call and inject into overlay.
        // This prevents cascading terPRE_SEQ for all subsequent txs from
        // this sender in this ledger.
        if outcome.ter_name == "terPRE_SEQ" {
            let sender_key = extract_sender_account_root_key(tx_bytes);
            if let Some(key) = sender_key {
                // Try current ledger first, then ledger-1
                let mut recovered = false;
                let repair = RpcProvider::with_endpoints(rpc_urls.to_vec(), ledger_seq);
                if let Some(data) = repair.read(&key) {
                    overlay.lock().insert(key, Some(data.to_vec()));
                    eprintln!("[ffi] terPRE_SEQ recovery: injected sender AccountRoot ({} bytes) at #{ledger_seq}", data.len());
                    recovered = true;
                }
                if !recovered {
                    let repair2 = RpcProvider::with_endpoints(rpc_urls.to_vec(), ledger_seq.saturating_sub(1));
                    if let Some(data) = repair2.read(&key) {
                        overlay.lock().insert(key, Some(data.to_vec()));
                        eprintln!("[ffi] terPRE_SEQ recovery: injected sender AccountRoot ({} bytes) at #{} (fallback)", data.len(), ledger_seq - 1);
                        recovered = true;
                    }
                }
                if !recovered {
                    eprintln!("[ffi] terPRE_SEQ recovery FAILED at #{ledger_seq}");
                }
            }
        }
        // Trace-level structured event for non-success — so we can correlate
        // with divergence log entries without spamming at info level.
        if !outcome.is_success() {
            tracing::trace!(
                tx_num,
                ter = %outcome.ter_name,
                mutations = outcome.mutations.len(),
                overlay_size = post_overlay_size,
                "apply non-success"
            );
        }
        let _ = (overlay_hits, prior_overlay_size);

        let mut s = stats.lock();
        s.live_apply_attempted += 1;
        // Categorize: tesSUCCESS → ok, tec* → claimed (rippled agreed), else → diverged
        if outcome.ter_name == "tesSUCCESS" {
            s.live_apply_ok += 1;
        } else if outcome.ter_name.starts_with("tec") {
            s.live_apply_claimed += 1;
        } else {
            s.live_apply_diverged += 1;
            s.live_apply_failed += 1; // keep deprecated field in sync
            *s.live_diverged_by_type.entry(format!("{}/{}", tx_type, outcome.ter_name)).or_insert(0) += 1;
            if !tx_hash.is_empty() {
                let tail = if outcome.last_fatal.is_empty() {
                    String::new()
                } else {
                    let f = &outcome.last_fatal;
                    let trimmed = if f.len() > 140 { &f[..140] } else { f.as_str() };
                    format!(" :: {trimmed}")
                };
                s.diverged_tx_samples.push(format!("L{} {}/{} {}{}", ledger_seq, tx_type, outcome.ter_name, tx_hash, tail));
                if s.diverged_tx_samples.len() > 50 {
                    s.diverged_tx_samples.remove(0);
                }
            }
            drop(s);
            if let Some(log) = divergence_log {
                log.record(ledger_seq, &tx_hash, &tx_type, &outcome.ter_name, elapsed_ms, &outcome.last_fatal);
            }
            // Re-acquire — remaining fields updated below
            s = stats.lock();
        }
        s.live_apply_last_ter = outcome.ter_name.clone();
        s.live_apply_last_mutations = outcome.mutations.len();
        s.live_apply_last_ms = elapsed_ms;
        // Rolling buffer of last 50 txs — powers live per-tx visibility on the
        // dashboard + watch_engine.py via /api/engine. Regardless of outcome.
        {
            let short_hash = if tx_hash.len() >= 16 { &tx_hash[..16] } else { tx_hash.as_str() };
            s.recent_tx_samples.push(format!(
                "L{} {}/{} {} {}ms mut={}",
                ledger_seq, tx_type, outcome.ter_name, short_hash, elapsed_ms, outcome.mutations.len()
            ));
            if s.recent_tx_samples.len() > 50 {
                s.recent_tx_samples.remove(0);
            }
        }
        *s.live_apply_ter_counts.entry(outcome.ter_name).or_insert(0) += 1;
        *s.apply_by_type.entry(tx_type.clone()).or_insert(0) += 1;
        *s.round_tx_types.entry(tx_type.clone()).or_insert(0) += 1;
        s.round_tx_count += 1;
        // Per-tx fee burn from the FFI result (drops_destroyed). This is
        // authoritative — the byte-pattern scanner (extract_fee_drops) was
        // hitting false positives inside tx signatures, producing absurd values.
        if outcome.drops_destroyed > 0 {
            let fee = outcome.drops_destroyed as u64;
            s.round_fees_drops = s.round_fees_drops.saturating_add(fee);
            s.total_fees_burned_drops = s.total_fees_burned_drops.saturating_add(fee);
        }
        // Histogram: find the first bucket bound >= elapsed_ms, increment it
        // and every bucket after (Prometheus cumulative convention).
        s.apply_duration_count += 1;
        s.apply_duration_sum_ms += elapsed_ms;
        let mut found = false;
        for (i, bound) in APPLY_LATENCY_BUCKETS_MS.iter().enumerate() {
            if elapsed_ms <= *bound {
                for j in i..11 {
                    s.apply_duration_buckets_ms[j] += 1;
                }
                found = true;
                break;
            }
        }
        if !found {
            // Overflowed all finite bounds → only +Inf bucket
            s.apply_duration_buckets_ms[10] += 1;
        }
    }
    stats.lock().ledgers_applied += 1;

    // Export cumulative RPC provider counters (per-ledger)
    use std::sync::atomic::Ordering;
    let h = fallback.hits.load(Ordering::Relaxed);
    let m = fallback.misses.load(Ordering::Relaxed);
    let miss_samples = fallback.miss_keys.lock().clone();
    let mut s = stats.lock();
    s.rpc_sle_hits += h;
    s.rpc_sle_misses += m;
    s.rpc_sle_miss_samples = miss_samples;
    let _ = overlay_hits;

    // Return accumulated mutations for shadow state hash computation
    overlay.into_inner()
}

/// Apply a live mainnet tx via libxrpl's full tx engine.
///
/// State lookups go through an RPC-backed provider that fetches SLEs from
/// rippled at `ledger_seq - 1` (pre-tx state).
pub fn apply_live_tx(
    stats: &SharedFfiStats,
    tx_bytes: &[u8],
    ledger_seq: u32,
    rpc_url: &str,
) {
    let t0 = std::time::Instant::now();
    let provider = RpcProvider::new(rpc_url.to_string(), ledger_seq.saturating_sub(1));
    let ledger = LedgerInfo {
        seq: ledger_seq,
        parent_close_time: 0,
        total_drops: 99_985_687_626_634_189,
        parent_hash: [0u8; 32],
        base_fee_drops: 10,
        reserve_drops: 10_000_000,
        increment_drops: 2_000_000,
    };
    let outcome = match xrpl_ffi::apply_with_mutations(tx_bytes, &[], &ledger, &provider, 0, 0) {
        Some(o) => o,
        None => {
            let mut s = stats.lock();
            s.live_apply_attempted += 1;
            s.live_apply_failed += 1;
            s.live_apply_last_ter = "FFI_NULL".into();
            return;
        }
    };
    let elapsed_ms = t0.elapsed().as_micros() as u64 / 1000;
    let mut s = stats.lock();
    s.live_apply_attempted += 1;
    if outcome.is_success() && outcome.applied {
        s.live_apply_ok += 1;
    } else {
        s.live_apply_failed += 1;
    }
    s.live_apply_last_ter = outcome.ter_name.clone();
    s.live_apply_last_mutations = outcome.mutations.len();
    s.live_apply_last_ms = elapsed_ms;
    *s.live_apply_ter_counts.entry(outcome.ter_name).or_insert(0) += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OwnedSnapshot must isolate reads from concurrent writes to the DB.
    /// This is the exact invariant ws_sync depends on when FFI verify runs
    /// concurrently with process_ledger's writes.
    #[test]
    fn owned_snapshot_isolates_from_concurrent_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(rocksdb::DB::open_default(tmp.path()).unwrap());

        // Populate pre-snapshot state
        db.put(b"key_pre", b"value_pre").unwrap();
        db.put(b"key_mutated", b"pre_value").unwrap();

        // Take the snapshot — this is "pre-ledger" state
        let snap = OwnedSnapshot::new(db.clone());

        // Simulate process_ledger writing POST-ledger state CONCURRENTLY
        db.put(b"key_mutated", b"post_value").unwrap();
        db.put(b"key_new", b"value_new").unwrap();
        db.delete(b"key_pre").unwrap();

        // Snapshot must still see pre-ledger state exactly
        assert_eq!(snap.get(b"key_pre").unwrap(), Some(b"value_pre".to_vec()));
        assert_eq!(snap.get(b"key_mutated").unwrap(), Some(b"pre_value".to_vec()));
        assert_eq!(snap.get(b"key_new").unwrap(), None);

        // Direct DB reads see post-ledger state (control group)
        assert_eq!(db.get(b"key_pre").unwrap(), None);
        assert_eq!(db.get(b"key_mutated").unwrap(), Some(b"post_value".to_vec()));
        assert_eq!(db.get(b"key_new").unwrap(), Some(b"value_new".to_vec()));
    }

    /// OwnedSnapshot must be Send — it crosses into spawn_blocking tasks.
    #[test]
    fn owned_snapshot_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<OwnedSnapshot>();
    }

    /// Real-world OfferCancel blob from mainnet ledger 103446686, tx
    /// 0564259CCBFCD6F8B4132416ED1ECFEE5FF613183030F18DAF1AB0ABBC2259E2.
    /// Sender: rMWVf1qJsgHgEd1Tuy378Zs53noKh4BujK
    /// (AccountID hex E0F60CFE821A65D1DB15EB041359ECB0CA16F47A).
    /// AccountRoot keylet: 91A0E3FA537D68D286B4452BD890BA6D2051800AE3EDED1797418D657B9D8537
    ///
    /// This is the exact tx type that bit us at ledger 103446686 — 75
    /// terPRE_SEQ failures from this sender in one ledger because the old
    /// scanner extracted the wrong account.
    #[test]
    fn extract_sender_real_offer_cancel_mainnet() {
        // Canonical full blob fetched from mainnet via xrplcluster.
        let blob = hex::decode(
            "12000824061B23BC2019061B238D68400000000000000A7321ED8EDE2A52E0BB5D6AE861D\
             87B08D80D01369944B703095A439515C704B75D0AE674401A24DA6F655EE58419034B7358\
             494AD3E37EF43232052E4C92ABA9A6A03A62B7993867848BCA8E0D039028CA2B51EED5F2F\
             079B6B08437EC3B0AEE6FE20CA1068114E0F60CFE821A65D1DB15EB041359ECB0CA16F47A"
                .replace([' ', '\n', '\r', '\t'], "")
                .as_str(),
        )
        .unwrap();
        assert_eq!(blob.len(), 146, "real OfferCancel blob is 146 bytes");

        let key = extract_sender_account_root_key(&blob).expect("must extract");
        let expected = hex::decode(
            "91A0E3FA537D68D286B4452BD890BA6D2051800AE3EDED1797418D657B9D8537",
        )
        .unwrap();
        assert_eq!(&key[..], &expected[..], "AccountRoot keylet must match the rMWVf1qJ... account");
    }

    /// Regression test for the off-by-one bug. The OLD scanner did
    /// `hash(tx_bytes[i+1..i+21])` which included the VL length byte (0x14)
    /// and dropped the last account byte. The buggy keylet for the real
    /// rMWVf1qJ... blob is `DA85A0C9200C6CF3...` — make sure we never
    /// produce that hash again.
    #[test]
    fn extract_sender_off_by_one_regression() {
        let blob = hex::decode(
            "12000824061B23BC2019061B238D68400000000000000A7321ED8EDE2A52E0BB5D6AE861D\
             87B08D80D01369944B703095A439515C704B75D0AE674401A24DA6F655EE58419034B7358\
             494AD3E37EF43232052E4C92ABA9A6A03A62B7993867848BCA8E0D039028CA2B51EED5F2F\
             079B6B08437EC3B0AEE6FE20CA1068114E0F60CFE821A65D1DB15EB041359ECB0CA16F47A"
                .replace([' ', '\n', '\r', '\t'], "")
                .as_str(),
        )
        .unwrap();
        let key = extract_sender_account_root_key(&blob).unwrap();
        let buggy = hex::decode(
            "DA85A0C9200C6CF3C8F65A02EBBDDEF656CC86B80791F83EC0276FFEEAA247B8",
        )
        .unwrap();
        assert_ne!(&key[..], &buggy[..], "must NOT reproduce the off-by-one keylet");
    }

    /// Synthetic test: a stray 0x81 byte (not followed by 0x14) appears in
    /// the SigningPubKey. The OLD scanner would have grabbed the next 20
    /// bytes after that stray and built a totally wrong keylet. The fixed
    /// scanner requires the 2-byte pattern `[0x81, 0x14]` and skips past
    /// the false match.
    #[test]
    fn extract_sender_skips_false_0x81_match() {
        // Construct: [stray 0x81, 0xFF, 30 random bytes, real 0x81, 0x14, 20 zero bytes]
        let mut blob = Vec::new();
        blob.push(0x81);
        blob.push(0xFF);
        blob.extend_from_slice(&[0xAB; 30]);
        blob.push(0x81);
        blob.push(0x14);
        blob.extend_from_slice(&[0u8; 20]);

        let key = extract_sender_account_root_key(&blob).unwrap();

        // Expected keylet for the all-zero AccountID
        use sha2::{Digest, Sha512};
        let mut h = Sha512::new();
        h.update([0x00u8, 0x61]);
        h.update([0u8; 20]);
        let expected = h.finalize();
        assert_eq!(&key[..], &expected[..32]);
    }

    /// Real-world OfferCancel blob from mainnet has sfFee = 10 drops
    /// (`6840000000000000000A` in canonical form). Verify the parser
    /// extracts the right value.
    #[test]
    fn extract_fee_real_offer_cancel_mainnet() {
        let blob = hex::decode(
            "12000824061B23BC2019061B238D68400000000000000A7321ED8EDE2A52E0BB5D6AE861D\
             87B08D80D01369944B703095A439515C704B75D0AE674401A24DA6F655EE58419034B7358\
             494AD3E37EF43232052E4C92ABA9A6A03A62B7993867848BCA8E0D039028CA2B51EED5F2F\
             079B6B08437EC3B0AEE6FE20CA1068114E0F60CFE821A65D1DB15EB041359ECB0CA16F47A"
                .replace([' ', '\n', '\r', '\t'], "")
                .as_str(),
        )
        .unwrap();
        assert_eq!(extract_fee_drops(&blob), Some(10));
    }

    /// Synthetic small fee (typical 12 drops on mainnet).
    #[test]
    fn extract_fee_small_value() {
        // [0x68, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0C] = 12 drops
        let blob = vec![
            0x12, 0x00, 0x00, // TransactionType
            0x68, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, // sfFee = 12
            0x73, 0x21, // sfSigningPubKey marker (rest doesn't matter for the test)
        ];
        assert_eq!(extract_fee_drops(&blob), Some(12));
    }

    /// Larger fee that uses bits in higher bytes of the 8-byte amount.
    #[test]
    fn extract_fee_multibyte() {
        // 0x000000000001E240 = 123456 drops
        let blob = vec![
            0x12, 0x00, 0x00,
            0x68, 0x40, 0x00, 0x00, 0x00, 0x00, 0x01, 0xE2, 0x40,
            0x73, 0x21,
        ];
        assert_eq!(extract_fee_drops(&blob), Some(123456));
    }

    /// Fee whose value uses the top 6 bits of the high byte.
    /// 0x?? = 0x40 | 0x05 = 0x45, value bits 0b000101_<56 zeros> = 5 << 56
    #[test]
    fn extract_fee_uses_top_value_bits() {
        let blob = vec![
            0x12, 0x00, 0x00,
            0x68, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x73, 0x21,
        ];
        assert_eq!(extract_fee_drops(&blob), Some(5u64 << 56));
    }

    /// A stray 0x68 byte preceded by a non-matching second byte (0xAA, top
    /// bits = 10, IOU not native) must be skipped, and the real sfFee must
    /// still be found further along.
    #[test]
    fn extract_fee_skips_false_0x68_match() {
        let blob = vec![
            0x68, 0xAA, // false: high bits = 10, not 01
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0x68, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, // real, 7 drops
        ];
        assert_eq!(extract_fee_drops(&blob), Some(7));
    }

    /// Buffers that can't possibly contain a full sfFee return None.
    #[test]
    fn extract_fee_returns_none_when_too_short() {
        assert!(extract_fee_drops(&[]).is_none());
        assert!(extract_fee_drops(&[0x68; 8]).is_none()); // 8 bytes — not enough
        // 9-byte buffer with a good marker → should succeed (boundary case)
        let ok = vec![0x68, 0x40, 0, 0, 0, 0, 0, 0, 0x2A];
        assert_eq!(extract_fee_drops(&ok), Some(42));
    }

    /// No sfFee at all → None.
    #[test]
    fn extract_fee_returns_none_when_no_fee_field() {
        let blob = vec![0x12, 0x00, 0x00, 0x73, 0x21, 0xAA, 0xBB, 0xCC, 0xDD];
        assert!(extract_fee_drops(&blob).is_none());
    }

    /// Buffer too short to contain even an sfAccount field.
    #[test]
    fn extract_sender_returns_none_when_too_short() {
        assert!(extract_sender_account_root_key(&[]).is_none());
        assert!(extract_sender_account_root_key(&[0x81; 21]).is_none());
        assert!(extract_sender_account_root_key(&[0x81; 22]).is_none()); // no 0x14 after
    }

    /// OwnedSnapshot survives the DB's original borrow lifetime going out of
    /// scope — only the Arc inside matters.
    #[test]
    fn owned_snapshot_outlives_original_arc_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = {
            let db = std::sync::Arc::new(rocksdb::DB::open_default(tmp.path()).unwrap());
            db.put(b"scoped_key", b"scoped_val").unwrap();
            OwnedSnapshot::new(db)
            // `db` Arc drops here but OwnedSnapshot internally holds another
            // Arc clone, so the DB stays alive.
        };
        assert_eq!(snap.get(b"scoped_key").unwrap(), Some(b"scoped_val".to_vec()));
    }
}
