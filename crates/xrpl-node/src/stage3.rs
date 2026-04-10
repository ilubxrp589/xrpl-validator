//! Stage 3 — FFI as the source of truth for `state.rocks`.
//!
//! ## Roadmap context
//!
//! The validator has been validating its FFI-derived state in stages:
//!
//! | Stage | Source of `state.rocks` bytes | Purpose                                       |
//! |-------|-------------------------------|-----------------------------------------------|
//! |  1    | RPC `node_binary` from rippled| Bootstrap the canonical state                 |
//! |  2    | RPC `node_binary` (no change) | Run FFI shadow hash to prove key-set parity   |
//! |  3    | **FFI mutation overlay**      | FFI bytes become the on-disk source of truth  |
//!
//! Stage 3 means: when the FFI engine reports a tx mutated key K with bytes B,
//! we write B to RocksDB instead of round-tripping the key through rippled's
//! `ledger_entry binary=true`. The validator stops needing rippled to derive
//! its own state — rippled remains in the loop only as a tx feeder and as a
//! comparison oracle for the state hash.
//!
//! ## Hybrid model (read this before flipping the toggle)
//!
//! Even with Stage 3 enabled, **the 5 ledger-level singletons must still come
//! from RPC**:
//!
//! - LedgerHashes      `B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B`
//! - NegativeUNL       `2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244`
//! - Amendments        `7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4`
//! - FeeSettings       `4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651`
//! - Skip-list (rotates per `seq / 65536` group)
//!
//! These are mutated by **pseudo-transactions** (`EnableAmendment`, `SetFee`,
//! `UNLModify`) which `apply_ledger_in_order` filters out before calling
//! libxrpl. The FFI overlay therefore never carries them. Stage 3 must
//! explicitly RPC-fetch the singletons and merge them into the same write
//! batch as the FFI mutations.
//!
//! ## Safety gate
//!
//! Stage 3 must NOT be enabled until the shadow hash has been at 100% over a
//! large window. The shadow check (see `ffi_verifier::check_shadow_hash`) is
//! the proof that FFI's overlay covers exactly the same key set as rippled's
//! diff and that the bytes hash to the same root. If shadow mismatches are
//! non-zero, flipping Stage 3 will silently corrupt `state.rocks` for every
//! divergent ledger, requiring a full wipe + resync to recover.
//!
//! Phase A (the 100K → 200K → 300K shadow-only run) is the gate. After Phase
//! A is clean, set `XRPL_FFI_STAGE3=1`, wipe state, and start Phase B.
//!
//! ## Wiring (deferred until Phase A clears)
//!
//! `process_ledger` in `ws_sync.rs` currently:
//! 1. Spawns FFI verify in a fire-and-forget `spawn_blocking` task.
//! 2. Concurrently fetches RPC `node_binary` for every modified key.
//! 3. Writes the RPC bytes to RocksDB and computes the state hash.
//!
//! The Stage 3 wiring will:
//! 1. Run FFI verify **synchronously** (await the `spawn_blocking`) and take
//!    its returned `LedgerOverlay`.
//! 2. RPC-fetch only the 5 singletons (small, fast, ~5 calls).
//! 3. Build the write batch with [`write_overlay_to_batch`] for the FFI
//!    overlay, then [`merge_singletons_into_batch`] for the singleton bytes.
//! 4. Compute the state hash from the resulting RocksDB state, same as today.
//!
//! No code path in this module is wired into the live loop yet — every helper
//! here is called only by tests. The deliberate gap is so a Stage 3 bug
//! cannot leak into the running shadow-only test on m3060.

use crate::ffi_engine::LedgerOverlay;

/// Per-process Stage 3 configuration. Read once at startup so a stale env var
/// or a mid-run flip cannot change behavior unpredictably.
#[derive(Debug, Clone, Copy)]
pub struct Stage3Config {
    /// When true, FFI mutation bytes are written to `state.rocks` instead of
    /// RPC `node_binary`. Default: false.
    pub enabled: bool,
}

impl Stage3Config {
    /// Read configuration from process environment.
    ///
    /// `XRPL_FFI_STAGE3` accepts `1`, `true`, `yes`, `on` (case-insensitive)
    /// as enable. Anything else (including unset) is disabled.
    pub fn from_env() -> Self {
        let enabled = std::env::var("XRPL_FFI_STAGE3")
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        Self { enabled }
    }
}

/// The 5 protocol-level singleton SLE keys that pseudo-transactions own.
/// These bypass libxrpl's normal apply path and must be sourced from RPC even
/// when Stage 3 is enabled. The 5th entry — the skip-list — rotates per
/// `seq / 65536` group; use [`skip_list_key`] to compute it.
pub const STATIC_SINGLETON_KEYS: [&str; 4] = [
    "B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B", // LedgerHashes
    "2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244", // NegativeUNL
    "7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4", // Amendments
    "4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651", // FeeSettings
];

/// Compute the rotating skip-list singleton key for a given ledger sequence.
/// Mirrors the inline calculation in `ws_sync::start_ws_sync` so both paths
/// agree on the same byte string.
pub fn skip_list_key(ledger_seq: u32) -> String {
    use sha2::{Digest, Sha512};
    let group = ledger_seq / 65536;
    let mut data = vec![0x00u8, 0x73];
    data.extend_from_slice(&group.to_be_bytes());
    let hash = Sha512::digest(&data);
    hex::encode(&hash[..32]).to_uppercase()
}

/// All 5 singleton keys for a given ledger, in canonical order.
pub fn singleton_keys_for(ledger_seq: u32) -> Vec<String> {
    let mut v: Vec<String> = STATIC_SINGLETON_KEYS.iter().map(|s| s.to_string()).collect();
    v.push(skip_list_key(ledger_seq));
    v
}

/// Write every entry in an FFI [`LedgerOverlay`] into a RocksDB write batch.
/// `Some(bytes)` becomes a `put`, `None` becomes a `delete`. Returns the number
/// of operations queued so the caller can sanity-check it against the expected
/// per-ledger mutation count.
pub fn write_overlay_to_batch(overlay: &LedgerOverlay, batch: &mut rocksdb::WriteBatch) -> usize {
    let mut count = 0usize;
    for (key, value) in overlay {
        match value {
            Some(data) => batch.put(key, data),
            None => batch.delete(key),
        }
        count += 1;
    }
    count
}

/// Merge a list of `(key_hex, Option<data_hex>)` singleton entries (as
/// returned from `ledger_entry binary=true`) into the same write batch.
///
/// `None` for the data means rippled reported `entryNotFound` — that singleton
/// doesn't exist at this ledger and must be deleted from the local DB to keep
/// it in sync. (LedgerHashes/Amendments/FeeSettings always exist on mainnet,
/// but NegativeUNL and the skip-list do come and go.)
///
/// Malformed hex is silently skipped: it's the caller's responsibility to
/// ensure singleton data was fetched correctly. Returns the number of valid
/// operations queued.
pub fn merge_singletons_into_batch(
    singletons: &[(String, Option<String>)],
    batch: &mut rocksdb::WriteBatch,
) -> usize {
    let mut count = 0usize;
    for (key_hex, data_opt) in singletons {
        let Ok(key_bytes) = hex::decode(key_hex) else { continue; };
        if key_bytes.len() != 32 {
            continue;
        }
        match data_opt {
            Some(data_hex) if !data_hex.is_empty() => {
                if let Ok(data) = hex::decode(data_hex) {
                    batch.put(&key_bytes, &data);
                    count += 1;
                }
            }
            _ => {
                batch.delete(&key_bytes);
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Cargo runs tests in parallel by default and `XRPL_FFI_STAGE3` is
    /// process-global state. Any test that touches it must hold this mutex
    /// for the duration of its read so siblings don't race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn config_default_is_disabled() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("XRPL_FFI_STAGE3").ok();
        std::env::remove_var("XRPL_FFI_STAGE3");
        let cfg = Stage3Config::from_env();
        assert!(!cfg.enabled);
        if let Some(v) = prev {
            std::env::set_var("XRPL_FFI_STAGE3", v);
        }
    }

    #[test]
    fn config_accepts_truthy_values() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for v in ["1", "true", "TRUE", "yes", "Yes", "on", "ON"] {
            std::env::set_var("XRPL_FFI_STAGE3", v);
            assert!(Stage3Config::from_env().enabled, "value `{v}` should enable");
        }
        std::env::remove_var("XRPL_FFI_STAGE3");
    }

    #[test]
    fn config_rejects_falsy_values() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for v in ["0", "false", "no", "off", "", "garbage"] {
            std::env::set_var("XRPL_FFI_STAGE3", v);
            assert!(!Stage3Config::from_env().enabled, "value `{v}` should NOT enable");
        }
        std::env::remove_var("XRPL_FFI_STAGE3");
    }

    #[test]
    fn skip_list_key_is_deterministic() {
        let a = skip_list_key(103_475_018);
        let b = skip_list_key(103_475_018);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // 32 bytes hex
    }

    #[test]
    fn skip_list_key_changes_per_group() {
        // group = seq / 65536; ledgers in different groups must produce
        // different keys, ledgers in the same group must produce the same key.
        let same_group_a = skip_list_key(103_424_730);
        let same_group_b = skip_list_key(103_424_731);
        assert_eq!(
            same_group_a, same_group_b,
            "consecutive ledgers in the same 65536-group share a skip-list"
        );

        let group_n = skip_list_key(65_536);
        let group_n_plus_one = skip_list_key(131_072);
        assert_ne!(
            group_n, group_n_plus_one,
            "different 65536-groups must rotate the skip-list"
        );
    }

    #[test]
    fn singleton_keys_for_returns_five() {
        let keys = singleton_keys_for(103_475_018);
        assert_eq!(keys.len(), 5);
        for k in &keys[..4] {
            assert!(STATIC_SINGLETON_KEYS.contains(&k.as_str()));
        }
        // The 5th is the skip-list
        assert_eq!(keys[4], skip_list_key(103_475_018));
    }

    fn open_temp_db() -> (tempfile::TempDir, std::sync::Arc<rocksdb::DB>) {
        let tmp = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(rocksdb::DB::open_default(tmp.path()).unwrap());
        (tmp, db)
    }

    #[test]
    fn write_overlay_round_trips_create_modify_delete() {
        let (_tmp, db) = open_temp_db();

        // Pre-seed an existing key so we can prove `Some(_)` overwrites and
        // `None` deletes the existing entry.
        let pre_key = [0xAA; 32];
        db.put(pre_key, b"old").unwrap();

        let mut overlay: LedgerOverlay = HashMap::new();
        overlay.insert(pre_key, Some(b"new".to_vec())); // modify
        overlay.insert([0xBB; 32], Some(b"created".to_vec())); // create
        overlay.insert([0xCC; 32], None); // tombstone for non-existent (no-op delete)
        // Also test deleting a key that DOES exist:
        let to_delete = [0xDD; 32];
        db.put(to_delete, b"will_die").unwrap();
        overlay.insert(to_delete, None);

        let mut batch = rocksdb::WriteBatch::default();
        let n = write_overlay_to_batch(&overlay, &mut batch);
        assert_eq!(n, 4);
        db.write(batch).unwrap();

        assert_eq!(db.get(pre_key).unwrap().as_deref(), Some(b"new".as_slice()));
        assert_eq!(db.get([0xBB; 32]).unwrap().as_deref(), Some(b"created".as_slice()));
        assert_eq!(db.get([0xCC; 32]).unwrap(), None);
        assert_eq!(db.get(to_delete).unwrap(), None);
    }

    #[test]
    fn merge_singletons_writes_present_and_deletes_missing() {
        let (_tmp, db) = open_temp_db();

        let key_present = STATIC_SINGLETON_KEYS[0].to_string();
        let key_missing = STATIC_SINGLETON_KEYS[1].to_string();
        let key_present_bytes: [u8; 32] = hex::decode(&key_present).unwrap().try_into().unwrap();
        let key_missing_bytes: [u8; 32] = hex::decode(&key_missing).unwrap().try_into().unwrap();

        // Seed: missing key currently exists in DB; we expect it to be removed.
        db.put(key_missing_bytes, b"stale").unwrap();

        let singletons = vec![
            (key_present.clone(), Some("DEADBEEF".to_string())),
            (key_missing.clone(), None),
        ];
        let mut batch = rocksdb::WriteBatch::default();
        let n = merge_singletons_into_batch(&singletons, &mut batch);
        assert_eq!(n, 2);
        db.write(batch).unwrap();

        assert_eq!(
            db.get(key_present_bytes).unwrap().as_deref(),
            Some(hex::decode("DEADBEEF").unwrap().as_slice())
        );
        assert_eq!(db.get(key_missing_bytes).unwrap(), None);
    }

    #[test]
    fn merge_singletons_skips_malformed_hex() {
        let (_tmp, _db) = open_temp_db();

        let singletons = vec![
            ("not-hex".to_string(), Some("AABB".to_string())),
            ("AA".to_string(), Some("AABB".to_string())), // wrong length
            (STATIC_SINGLETON_KEYS[0].to_string(), Some("AABB".to_string())), // good
        ];
        let mut batch = rocksdb::WriteBatch::default();
        let n = merge_singletons_into_batch(&singletons, &mut batch);
        assert_eq!(n, 1, "only the well-formed singleton survives");
    }
}
