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
pub fn fetch_mainnet_amendments(rpc_url: &str) -> Vec<[u8; 32]> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
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
        Err(_) => return Vec::new(),
    };
    let body: serde_json::Value = match resp.json() {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let empty = Vec::new();
    let amendments = body["result"]["node"]["Amendments"]
        .as_array()
        .unwrap_or(&empty);
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
    out
}

/// RPC-backed SleProvider: fetches SLEs synchronously from rippled via
/// `ledger_entry` RPC at a fixed pre-ledger index. Slow but works from anywhere.
pub struct RpcProvider {
    client: reqwest::blocking::Client,
    rpc_url: String,
    ledger_index: u32,
    arena: parking_lot::Mutex<Vec<Vec<u8>>>,
    pub hits: std::sync::atomic::AtomicU64,
    pub misses: std::sync::atomic::AtomicU64,
    pub miss_keys: parking_lot::Mutex<Vec<String>>,
}

impl RpcProvider {
    pub fn new(rpc_url: String, ledger_index: u32) -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest blocking client"),
            rpc_url,
            ledger_index,
            arena: parking_lot::Mutex::new(Vec::with_capacity(16)),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            miss_keys: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

impl SleProvider for RpcProvider {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]> {
        use std::sync::atomic::Ordering;
        let key_hex = hex::encode_upper(key);
        let resp = match self
            .client
            .post(&self.rpc_url)
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
            Err(_) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let body: serde_json::Value = match resp.json() {
            Ok(b) => b,
            Err(_) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let data_hex = match body["result"]["node_binary"].as_str() {
            Some(s) => s,
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                let mut mk = self.miss_keys.lock();
                if mk.len() < 20 {
                    // Also capture rippled's error string if present
                    let err = body["result"]["error"].as_str().unwrap_or("no_node_binary");
                    mk.push(format!("{}@{}: {}", &key_hex[..16], self.ledger_index, err));
                }
                return None;
            }
        };
        let bytes = match hex::decode(data_hex) {
            Ok(b) => b,
            Err(_) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        self.hits.fetch_add(1, Ordering::Relaxed);
        let mut arena = self.arena.lock();
        arena.push(bytes);
        unsafe {
            let last = arena.last().unwrap();
            Some(std::slice::from_raw_parts(last.as_ptr(), last.len()))
        }
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
    /// Sample of up to 20 miss keys from the last ledger applied
    pub rpc_sle_miss_samples: Vec<String>,
    /// Sample of up to 10 diverged tx hashes (tx_type/TER/hash) for mainnet lookup
    pub diverged_tx_samples: Vec<String>,
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
pub fn apply_ledger_in_order(
    stats: &SharedFfiStats,
    txs_in_order: &[Vec<u8>],
    ledger_seq: u32,
    rpc_url: &str,
    amendments: &[[u8; 32]],
    parent_hash: [u8; 32],
    parent_close_time: u32,
    total_drops: u64,
) {
    let fallback = RpcProvider::new(rpc_url.to_string(), ledger_seq.saturating_sub(1));
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

    let mut tx_num = 0u32;
    let mut overlay_hits = 0u32;
    for tx_bytes in txs_in_order {
        tx_num += 1;
        // Parse to get tx_type + hash (for diagnostic buckets)
        let (tx_type, tx_hash) = xrpl_ffi::parse_tx(tx_bytes)
            .map(|p| (p.tx_type, hex::encode_upper(p.hash)))
            .unwrap_or_else(|| ("Unknown".to_string(), String::new()));
        let t0 = std::time::Instant::now();
        let provider = LayeredProvider::new(&overlay, &fallback);
        let outcome = match xrpl_ffi::apply_with_mutations(
            tx_bytes,
            amendments,
            &ledger,
            &provider,
            0,
            0,
        ) {
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
        // Only log non-success so we see the failure patterns
        if !outcome.is_success() && tx_num % 10 == 1 {
            eprintln!(
                "  [tx #{tx_num}] {} mutations={} overlay={}",
                outcome.ter_name, outcome.mutations.len(), post_overlay_size
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
            if s.diverged_tx_samples.len() < 10 && !tx_hash.is_empty() {
                let tail = if outcome.last_fatal.is_empty() {
                    String::new()
                } else {
                    let f = &outcome.last_fatal;
                    let trimmed = if f.len() > 140 { &f[..140] } else { f.as_str() };
                    format!(" :: {trimmed}")
                };
                s.diverged_tx_samples.push(format!("{}/{} {}{}", tx_type, outcome.ter_name, tx_hash, tail));
            }
        }
        s.live_apply_last_ter = outcome.ter_name.clone();
        s.live_apply_last_mutations = outcome.mutations.len();
        s.live_apply_last_ms = elapsed_ms;
        *s.live_apply_ter_counts.entry(outcome.ter_name).or_insert(0) += 1;
    }

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
