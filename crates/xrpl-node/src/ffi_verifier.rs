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
//! - Reads each ledger's amendment set from its parent's Amendments object
//!   (`ledger_amendments`; the set loaded at startup is only the fallback) and
//!   holds a shared `FfiStats` snapshot exposed on `/api/status`.
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
//! - `verify_ledger(seq, tx_blobs, parent_hash, parent_close_time, close_resolution, total_drops)`
//! - `stats()` → `FfiStats`

use std::any::Any;
use std::sync::Arc;

use crate::ffi_engine::{
    amendments_in_object, amendments_object_key, apply_ledger_in_order_with_net, fetch_amendments_at, fetch_mainnet_amendments, new_stats,
    DivergenceLog, FfiStats, LedgerOverlay, NetParams, OwnedSnapshot, SharedFfiStats,
};

/// FFI-based per-ledger tx verifier. Send + Sync, cheap to `Arc`-clone.
pub struct FfiVerifier {
    stats: SharedFfiStats,
    rpc_urls: Vec<String>,
    /// Independence Step 4: with peers as the ledger source, the amendments come from our own state, not RPC.
    state_db: Option<Arc<rocksdb::DB>>,
    /// The amendment set last read from a parent ledger's Amendments object (at start: the one loaded then). Each
    /// ledger reads its own (`ledger_amendments`); this is what a ledger falls back to when its read yields nothing.
    amendments: parking_lot::RwLock<Vec<[u8; 32]>>,
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
        Self::with_amendments(rpc_urls, amendments, None)
    }

    /// Independence Step 4 (`XRPL_LEDGER_SOURCE=peers`): the active amendments from our own state.rocks, now and for
    /// every ledger. No RPC.
    pub fn from_state(rpc_urls: Vec<String>, db: Arc<rocksdb::DB>) -> Self {
        let amendments = crate::ffi_engine::amendments_from_state(&db);
        tracing::info!(count = amendments.len(), "ffi_verifier: loaded active amendments from our own state");
        Self::with_amendments(rpc_urls, amendments, Some(db))
    }

    fn with_amendments(rpc_urls: Vec<String>, amendments: Vec<[u8; 32]>, state_db: Option<Arc<rocksdb::DB>>) -> Self {
        let silent_path = std::env::var("XRPL_FFI_SILENT_DIVERGENCE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/silent_divergences.jsonl"));
        let mutation_path = std::env::var("XRPL_FFI_MUTATION_DIVERGENCE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/mutation_divergences.jsonl"));
        Self {
            stats: new_stats(),
            rpc_urls,
            state_db,
            amendments: parking_lot::RwLock::new(amendments),
            divergence_log: Arc::new(DivergenceLog::new()),
            silent_divergence_log: Arc::new(DivergenceLog::with_path(silent_path)),
            mutation_divergence_log: Arc::new(DivergenceLog::with_path(mutation_path)),
        }
    }

    /// The amendment set ledger `ledger_seq` is applied under: the enabled list of its PARENT ledger's Amendments
    /// object, read for every ledger. rippled builds a ledger under its parent's rules (`rules_(prevLedger.rules_)`,
    /// Ledger.cpp:259), so the ledger that carries an EnableAmendment runs under the old set throughout and the
    /// next ledger under the new one; the shim turns this list into libxrpl's `Rules` for the whole ledger.
    ///
    /// Where the parent's object is read from:
    /// - `snapshot`, when the caller verifies on one: it is the pre-ledger state, so its object is the parent's;
    /// - otherwise our own state as it stands, when peers are the ledger source (no RPC in that mode; the state
    ///   is not pinned to the parent there, as the rest of that path's reads are not);
    /// - otherwise `ledger_entry` at `ledger_seq - 1` on the first RPC endpoint (the same ledger the RPC path
    ///   reads every other object at), three tries.
    ///
    /// A source that yields no enabled amendment (object absent or unreadable, RPC failed) leaves the last set
    /// known in force: an empty Rules would turn every amendment-gated transaction into a divergence.
    ///
    /// SCOPING: this covers amendments the linked libxrpl already implements. One introduced by a newer rippled
    /// still needs the rebuild: an id cannot supply engine semantics.
    pub fn ledger_amendments(&self, ledger_seq: u32, snapshot: Option<&OwnedSnapshot>) -> Vec<[u8; 32]> {
        let read = match (snapshot, &self.state_db) {
            (Some(snap), _) => snap
                .get(&amendments_object_key())
                .ok()
                .flatten()
                .map(|bytes| amendments_in_object(&bytes))
                .unwrap_or_default(),
            (None, Some(db)) => crate::ffi_engine::amendments_from_state(db),
            (None, None) => fetch_amendments_at(&self.rpc_urls[0], &ledger_seq.saturating_sub(1).to_string(), 3),
        };
        if read.is_empty() {
            eprintln!("[ffi-amendments] #{ledger_seq}: no enabled amendments readable from the parent state — keeping the last known set");
            return self.amendments.read().clone();
        }
        let delta = crate::amendments::diff_amendment_sets(&self.amendments.read(), &read);
        if !delta.is_empty() {
            eprintln!(
                "[ffi-amendments] #{ledger_seq}: the parent's enabled set differs from the last one used — {} added, {} removed",
                delta.added.len(),
                delta.removed.len(),
            );
            for a in &delta.added {
                eprintln!("[ffi-amendments]   + {}", hex::encode_upper(a));
            }
            for a in &delta.removed {
                eprintln!("[ffi-amendments]   - {}", hex::encode_upper(a));
            }
            *self.amendments.write() = read.clone();
        }
        read
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
        close_resolution: u8,
        total_drops: u64,
        expected_outcomes: Option<&std::collections::HashMap<String, String>>,
        expected_mutations: Option<&std::collections::HashMap<String, Vec<(String, u8)>>>,
        batch_parents: Option<&std::collections::HashMap<String, String>>,
    ) -> LedgerOverlay {
        let amendments = self.ledger_amendments(ledger_seq, None);
        apply_ledger_in_order_with_net(
            &self.stats,
            sorted_tx_blobs,
            ledger_seq,
            &self.rpc_urls,
            &amendments,
            parent_hash,
            parent_close_time,
            close_resolution,
            total_drops,
            Some(self.divergence_log.as_ref()),
            None,
            Some(self.silent_divergence_log.as_ref()),
            expected_outcomes,
            Some(self.mutation_divergence_log.as_ref()),
            expected_mutations,
            batch_parents,
            None,
            &NetParams::default(),
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
        close_resolution: u8,
        total_drops: u64,
        snapshot: Option<&OwnedSnapshot>,
        expected_outcomes: Option<&std::collections::HashMap<String, String>>,
        expected_mutations: Option<&std::collections::HashMap<String, Vec<(String, u8)>>>,
        batch_parents: Option<&std::collections::HashMap<String, String>>,
    ) -> LedgerOverlay {
        let amendments = self.ledger_amendments(ledger_seq, snapshot);
        apply_ledger_in_order_with_net(
            &self.stats,
            sorted_tx_blobs,
            ledger_seq,
            &self.rpc_urls,
            &amendments,
            parent_hash,
            parent_close_time,
            close_resolution,
            total_drops,
            Some(self.divergence_log.as_ref()),
            snapshot,
            Some(self.silent_divergence_log.as_ref()),
            expected_outcomes,
            Some(self.mutation_divergence_log.as_ref()),
            expected_mutations,
            batch_parents,
            None,
            &NetParams::default(),
        )
    }

    /// Compute shadow state hash using a PRE-CAPTURED hasher snapshot.
    /// The snapshot must be taken BEFORE process_ledger writes to the real hasher.
    /// Returns our computed state root (`Some([u8; 32])`), or `None` if the hasher
    /// isn't built yet. Match/mismatch vs `network_hash` is still tracked in stats;
    /// the returned root lets the lockstep shadow recompute the full ledger_hash.
    pub fn check_shadow_hash(
        &self,
        hasher_snapshot: Box<dyn std::any::Any + Send>,
        overlay: &LedgerOverlay,
        network_hash: &str,
    ) -> Option<[u8; 32]> {
        let shadow = match crate::state_hash::StateHashComputer::shadow_hash_from_snapshot(
            hasher_snapshot, overlay,
        ) {
            Some(h) => h,
            None => return None,
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
        Some(shadow.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    const AMENDMENTS_KEY: &str = "7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4";

    fn id(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    /// Write an Amendments object with this enabled list into the state, as ws-sync does with the network's bytes.
    fn put_amendments(db: &rocksdb::DB, enabled: &[[u8; 32]]) {
        let ids: Vec<String> = enabled.iter().map(hex::encode_upper).collect();
        let sle = serde_json::json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": ids});
        db.put(hex::decode(AMENDMENTS_KEY).unwrap(), crate::meta_check::encode(&sle).unwrap()).unwrap();
    }

    /// A verifier that started with `startup` as its amendment set. Its RPC endpoint answers nothing.
    fn verifier(db: &Arc<rocksdb::DB>, startup: &[[u8; 32]]) -> FfiVerifier {
        FfiVerifier::with_amendments(vec!["http://127.0.0.1:1".into()], startup.to_vec(), Some(db.clone()))
    }

    /// rippled builds a ledger under its PARENT's rules (`rules_(prevLedger.rules_)`, Ledger.cpp:259): the ledger
    /// that carries the EnableAmendment pseudo-transaction runs under the old set to its last transaction, and
    /// the very next ledger under the new one. The native leg freezes its rules that way (ba74cb9); the libxrpl
    /// leg gets its Rules from the list the verifier hands the shim, one list per ledger, so the verifier must
    /// read that list from the pre-ledger state it verifies on, for every ledger and not only for flag ledgers.
    #[test]
    fn a_ledger_is_verified_under_its_parents_amendments() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(rocksdb::DB::open_default(dir.path()).unwrap());
        let (old, new) = (vec![id(0xAA), id(0xBB)], vec![id(0xAA), id(0xBB), id(0xCC)]);
        // An amendment is enabled in the ledger after a flag ledger.
        let activation = 256 * 400_000 + 1;

        put_amendments(&db, &old);
        let v = verifier(&db, &old);
        let before_activation = OwnedSnapshot::new(db.clone());
        // The activation ledger's writes land (its EnableAmendment rewrote the Amendments object).
        put_amendments(&db, &new);
        let after_activation = OwnedSnapshot::new(db.clone());

        assert_eq!(
            v.ledger_amendments(activation, Some(&before_activation)),
            old,
            "the activation ledger itself runs under its parent's set, whatever the state holds by now"
        );
        assert_eq!(
            v.ledger_amendments(activation + 1, Some(&after_activation)),
            new,
            "the ledger after the activation runs under the new set (it is not a flag ledger: the next one is 255 away)"
        );
        assert_eq!(
            v.ledger_amendments(activation, Some(&before_activation)),
            old,
            "a ledger re-driven on its own pre-state still gets its parent's set, not the last one seen"
        );
    }

    /// A pre-state with no readable Amendments object must not empty the rules (every amendment-gated
    /// transaction would diverge): the last set known stays in force.
    #[test]
    fn a_pre_state_without_an_amendments_object_keeps_the_last_known_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(rocksdb::DB::open_default(dir.path()).unwrap());
        let known = vec![id(0xAA), id(0xBB)];
        let v = verifier(&db, &known);
        let empty_state = OwnedSnapshot::new(db.clone());
        assert_eq!(v.ledger_amendments(256 * 400_000 + 2, Some(&empty_state)), known);
        db.put(hex::decode(AMENDMENTS_KEY).unwrap(), b"not an object").unwrap();
        let garbled = OwnedSnapshot::new(db.clone());
        assert_eq!(v.ledger_amendments(256 * 400_000 + 3, Some(&garbled)), known);
    }
}
