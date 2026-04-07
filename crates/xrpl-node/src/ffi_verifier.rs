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

use std::sync::Arc;

use crate::ffi_engine::{
    apply_ledger_in_order, compute_shadow_hash, fetch_mainnet_amendments, new_stats,
    DivergenceLog, FfiStats, LedgerOverlay, OwnedSnapshot, SharedFfiStats,
};

/// FFI-based per-ledger tx verifier. Send + Sync, cheap to `Arc`-clone.
pub struct FfiVerifier {
    stats: SharedFfiStats,
    rpc_urls: Vec<String>,
    amendments: Vec<[u8; 32]>,
    divergence_log: Arc<DivergenceLog>,
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
        Self {
            stats: new_stats(),
            rpc_urls,
            amendments,
            divergence_log: Arc::new(DivergenceLog::new()),
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
        )
    }

    /// Compute shadow state hash from FFI mutations + existing state.
    /// Compare against rippled's account_hash and update stats.
    pub fn check_shadow_hash(
        &self,
        hash_comp: &crate::state_hash::StateHashComputer,
        overlay: &LedgerOverlay,
        network_hash: &str,
    ) -> bool {
        let shadow = match compute_shadow_hash(hash_comp, overlay) {
            Some(h) => h,
            None => return false,
        };
        let ours = hex::encode(shadow.0).to_uppercase();
        let net = network_hash.to_uppercase();
        let matched = ours == net;

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
