//! FFI-based independent tx verifier for the live validator.
//!
//! Replaces the deprecated pure-Rust `tx_engine` module. Every ledger that the
//! validator syncs gets its tx set replayed through libxrpl (via `ffi_engine`)
//! and tracked against mainnet for any divergence. 100% agreement is the
//! correctness SLO — see `docs/SLO.md`.
//!
//! Feature-gated behind `--features ffi`.
//!
//! ## Architecture
//!
//! - One `FfiVerifier` instance per validator process, cloned via `Arc` into
//!   each sync task.
//! - Holds the active mainnet amendments (fetched once at startup) and a
//!   shared `FfiStats` snapshot exposed on `/api/status`.
//! - `verify_ledger(...)` takes a pre-ordered list of tx blobs plus the
//!   ledger header fields and delegates to `ffi_engine::apply_ledger_in_order`.
//! - Every diverged result is recorded to `logs/divergences.jsonl` via
//!   `DivergenceLog` (append-only, survives restarts).
//!
//! ## Current state provider
//!
//! Uses `RpcProvider` with multi-endpoint failover and retry-on-overload.
//! A future revision will swap to `RocksDbProvider` (validator's own DB
//! snapshot) so we stop hammering rippled — but the RPC path is the
//! reference implementation we verified to 100% agreement on live mainnet.
//!
//! ## API (same shape as old TxEngine)
//!
//! - `FfiVerifier::new(rpc_urls)` — constructor
//! - `verify_ledger(seq, tx_blobs, parent_hash, parent_close_time, total_drops)`
//! - `stats()` → `FfiStats`

use std::any::Any;
use std::sync::Arc;

use crate::ffi_engine::{
    apply_ledger_in_order, fetch_mainnet_amendments, new_stats,
    DivergenceLog, FfiStats, LedgerOverlay, OwnedSnapshot, SharedFfiStats,
};

/// FFI-based per-ledger tx verifier. Send + Sync, cheap to `Arc`-clone.
pub struct FfiVerifier {
    stats: SharedFfiStats,
    rpc_urls: Vec<String>,
    amendments: Vec<[u8; 32]>,
    divergence_log: Arc<DivergenceLog>,
    /// Separate log for `tec*`/`tesSUCCESS` divergences — cases the main
    /// `divergence_log` skips because libxrpl returned a "claimed" or
    /// "success" outcome that our threading still acts on, but which
    /// disagrees with the network's recorded TransactionResult.
    silent_divergence_log: Arc<DivergenceLog>,
    /// Same-TER, different-mutation-set divergences (BF6C928F class).
    mutation_divergence_log: Arc<DivergenceLog>,
}

impl FfiVerifier {
    /// Construct a verifier and load the active mainnet amendments from the
    /// first RPC endpoint. Blocks while amendments are fetched (up to ~40s
    /// on rippled overload — see `fetch_mainnet_amendments`).
    pub fn new(rpc_urls: Vec<String>) -> Self {
        assert!(!rpc_urls.is_empty(), "at least one RPC endpoint required");
        let amendments = fetch_mainnet_amendments(&rpc_urls[0]);
        tracing::info!(
            count = amendments.len(),
            "ffi_verifier: loaded active mainnet amendments"
        );
        let silent_path = std::env::var("XRPL_FFI_SILENT_DIVERGENCE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/silent_divergences.jsonl"));
        let mutation_path = std::env::var("XRPL_FFI_MUTATION_DIVERGENCE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/mutation_divergences.jsonl"));
        Self {
            stats: new_stats(),
            rpc_urls,
            amendments,
            divergence_log: Arc::new(DivergenceLog::new()),
            silent_divergence_log: Arc::new(DivergenceLog::with_path(silent_path)),
            mutation_divergence_log: Arc::new(DivergenceLog::with_path(mutation_path)),
        }
    }

    /// Apply and verify every tx in a ledger, in `TransactionIndex` order,
    /// against mainnet's recorded TER via libxrpl. Blocking — call from a
    /// spawn_blocking task or a dedicated thread. Each diverged result is
    /// logged to `logs/divergences.jsonl` and counted in `stats()`.
    ///
    /// Pure RPC path — used by the sidecar `ffi_test` binary that has no
    /// local state DB.
    pub fn verify_ledger(
        &self,
        ledger_seq: u32,
        sorted_tx_blobs: &[Vec<u8>],
        parent_hash: [u8; 32],
        parent_close_time: u32,
        total_drops: u64,
        expected_outcomes: Option<&std::collections::HashMap<String, String>>,
        expected_mutations: Option<&std::collections::HashMap<String, Vec<(String, u8)>>>,
    ) -> LedgerOverlay {
        apply_ledger_in_order(
            &self.stats,
            sorted_tx_blobs,
            ledger_seq,
            &self.rpc_urls,
            &self.amendments,
            parent_hash,
            parent_close_time,
            total_drops,
            Some(self.divergence_log.as_ref()),
            None,
            Some(self.silent_divergence_log.as_ref()),
            expected_outcomes,
            Some(self.mutation_divergence_log.as_ref()),
            expected_mutations,
        )
    }

    /// Apply + verify using a pre-captured `OwnedSnapshot` as the SLE
    /// source (with RPC fallback on miss). The snapshot MUST be taken at
    /// the PRE-ledger boundary — i.e., before ws_sync's process_ledger()
    /// writes post-state to the DB. Owner of this invariant is the caller
    /// (see ws_sync.rs call site).
    pub fn verify_ledger_with_snapshot(
        &self,
        ledger_seq: u32,
        sorted_tx_blobs: &[Vec<u8>],
        parent_hash: [u8; 32],
        parent_close_time: u32,
        total_drops: u64,
        snapshot: Option<&OwnedSnapshot>,
        expected_outcomes: Option<&std::collections::HashMap<String, String>>,
        expected_mutations: Option<&std::collections::HashMap<String, Vec<(String, u8)>>>,
    ) -> LedgerOverlay {
        apply_ledger_in_order(
            &self.stats,
            sorted_tx_blobs,
            ledger_seq,
            &self.rpc_urls,
            &self.amendments,
            parent_hash,
            parent_close_time,
            total_drops,
            Some(self.divergence_log.as_ref()),
            snapshot,
            Some(self.silent_divergence_log.as_ref()),
            expected_outcomes,
            Some(self.mutation_divergence_log.as_ref()),
            expected_mutations,
        )
    }

    /// Compute shadow state hash using a PRE-CAPTURED hasher snapshot.
    /// The snapshot must be taken BEFORE process_ledger writes to the real hasher.
    pub fn check_shadow_hash(
        &self,
        hasher_snapshot: Box<dyn std::any::Any + Send>,
        overlay: &LedgerOverlay,
        network_hash: &str,
    ) -> bool {
        let shadow = match crate::state_hash::StateHashComputer::shadow_hash_from_snapshot(
            hasher_snapshot, overlay,
        ) {
            Some(h) => h,
            None => return false,
        };
        let ours = hex::encode(shadow.0).to_uppercase();
        let net = network_hash.to_uppercase();
        let matched = ours == net;

        if !matched {
            let seq = self.stats.lock().round_ledger_seq;
            eprintln!("[ffi-shadow] MISMATCH ledger #{seq}: overlay keys: {}  ours: {}  network: {}", overlay.len(), &ours[..16], &net[..16]);
            // Diagnostic: dump first 3 overlay keys + data length for investigation
            let mut count = 0;
            for (key, val) in overlay.iter() {
                if count >= 3 { break; }
                let key_hex = hex::encode_upper(key);
                match val {
                    Some(data) => eprintln!("[ffi-shadow]   key={} len={} first8={}", &key_hex[..16], data.len(), hex::encode_upper(&data[..data.len().min(8)])),
                    None => eprintln!("[ffi-shadow]   key={} DELETED", &key_hex[..16]),
                }
                count += 1;
            }
            // Persist to shadow mismatch log
            self.log_shadow_mismatch(seq, &ours, &net, overlay.len());
        }
        let mut s = self.stats.lock();
        s.shadow_hash_attempted += 1;
        if matched {
            s.shadow_hash_matched += 1;
        } else {
            s.shadow_hash_mismatched += 1;
        }
        s.shadow_hash_last = ours;
        s.shadow_hash_last_network = net;
        s.shadow_hash_last_matched = matched;
        matched
    }

    fn log_shadow_mismatch(&self, seq: u32, ours: &str, network: &str, overlay_keys: usize) {
        use std::io::Write;
        let path = std::env::var("XRPL_FFI_SHADOW_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/shadow_mismatches.jsonl"));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = serde_json::json!({
            "ts": ts,
            "ledger_seq": seq,
            "our_hash": ours,
            "network_hash": network,
            "overlay_keys": overlay_keys,
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }

    /// Snapshot the shared `FfiStats` (cheap clone, holds the mutex briefly).
    pub fn stats(&self) -> FfiStats {
        self.stats.lock().clone()
    }

    /// Direct access to the shared stats handle (for consumers that want to
    /// avoid cloning each time, e.g. Prometheus render).
    pub fn shared_stats(&self) -> SharedFfiStats {
        Arc::clone(&self.stats)
    }
}
