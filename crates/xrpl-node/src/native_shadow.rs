//! native_shadow — Stage 4 Phase A: the native Rust transaction engine runs
//! beside the FFI (libxrpl C++) leg on every live ledger, applying the same
//! transactions against an in-RAM mirror of `state.rocks` and comparing its
//! mutation overlay against the FFI overlay byte for byte.
//!
//! The engine code here is EXACTLY what `state_replay` proved offline
//! (three consecutive fully-clean 280-ledger windows) — `native_apply` is
//! shared verbatim. What this module adds is the harness around it:
//!
//!   * an in-RAM `LedgerState` mirror, hydrated once from `state.rocks`
//!     (the engine's `Sandbox` needs ordered prefix scans for the book
//!     walks, which the concrete in-memory map provides),
//!   * per-ledger apply of the SAME parsed tx JSON ws-sync already fetched,
//!   * an overlay-vs-overlay compare against the FFI leg (key sets and
//!     encoded bytes), TER-vs-mainnet counting on the side,
//!   * canonical reconciliation: after the compare, the mirror is put back
//!     on the CANONICAL trajectory (the FFI overlay under Stage 3), so one
//!     native divergence can never compound into the next ledger. The skip
//!     list and pseudo-transaction singletons — which the FFI overlay never
//!     carries — are maintained natively, exactly as the replay proved.
//!
//! Enabled by `XRPL_NATIVE_SHADOW=1`. Compare receipts append to
//! `XRPL_NATIVE_SHADOW_LOG` (default `/mnt/xrpl-data/native_shadow.jsonl`).
//! Counters are exposed process-wide via [`stats`] for `/api/engine`.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;

use crate::ffi_engine::LedgerOverlay;
use crate::native_apply::{build_txfields, canon_for_encode, native_apply_one, update_skip_list};

/// Process-wide counters, readable from the `/api/engine` handler without
/// threading the shadow itself out of ws-sync.
#[derive(Default)]
pub struct ShadowStats {
    pub enabled: AtomicU64,
    pub hydrated: AtomicU64,
    pub hydrate_objects: AtomicU64,
    pub hydrate_decode_err: AtomicU64,
    pub hydrate_ms: AtomicU64,
    pub ledgers: AtomicU64,
    pub full_match: AtomicU64,
    pub overlay_diverged: AtomicU64,
    pub txs_applied: AtomicU64,
    pub ter_matched: AtomicU64,
    pub ter_mismatched: AtomicU64,
    pub keys_compared: AtomicU64,
    pub key_missing: AtomicU64,
    pub key_extra: AtomicU64,
    pub byte_mismatch: AtomicU64,
    pub skipped_gap: AtomicU64,
    pub apply_ms_last: AtomicU64,
}

pub fn stats() -> &'static ShadowStats {
    static S: OnceLock<ShadowStats> = OnceLock::new();
    S.get_or_init(ShadowStats::default)
}

/// The 4 static protocol singletons plus the rolling skip list: keys the FFI
/// overlay never carries (pseudo-tx territory) and therefore excluded from
/// the overlay compare. The mirror maintains them natively.
fn is_singleton_key(key: &Hash256, seq: u32) -> bool {
    if *key == keylet::skip_list_key() {
        return true;
    }
    for hex_key in crate::stage3::STATIC_SINGLETON_KEYS {
        if hex::encode_upper(key.0).eq_ignore_ascii_case(hex_key) {
            return true;
        }
    }
    // The every-65536-block LedgerHashes entry rotates with the seq group.
    let mut buf = Vec::with_capacity(6);
    buf.extend_from_slice(&[0x00, 0x73]);
    buf.extend_from_slice(&((seq.saturating_sub(1)) >> 16).to_be_bytes());
    *key == xrpl_ledger::shamap::hash::sha512_half(&buf)
}

pub struct NativeShadow {
    state: LedgerState,
    /// The mirror represents post-state of `at_seq`; `on_ledger(at_seq + 1)`
    /// is the only sequence it will apply. Anything else marks a gap.
    at_seq: u32,
    pub hydrated: bool,
    log: Option<std::fs::File>,
}

impl NativeShadow {
    /// Build the (unhydrated) shadow if `XRPL_NATIVE_SHADOW=1`.
    pub fn maybe_new() -> Option<Self> {
        let on = std::env::var("XRPL_NATIVE_SHADOW")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        if !on {
            return None;
        }
        stats().enabled.store(1, Ordering::Relaxed);
        let path = std::env::var("XRPL_NATIVE_SHADOW_LOG")
            .unwrap_or_else(|_| "/mnt/xrpl-data/native_shadow.jsonl".to_string());
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        eprintln!("[native-shadow] ENABLED — hydrates on first steady ledger; receipts -> {path}");
        // Placeholder header — on_ledger installs the real per-ledger header
        // (the replay's exact recipe) before every apply.
        let header = LedgerHeader {
            sequence: 0,
            total_coins: 0,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 0,
            close_time_resolution: 10,
            close_flags: 0,
        };
        Some(Self { state: LedgerState::new_unverified(header), at_seq: 0, hydrated: false, log })
    }

    /// Full-scan `state.rocks` into the in-RAM mirror, decoding every binary
    /// SLE to the engine's JSON form. Blocks the sync loop once (~minutes);
    /// ws-sync's hold-position machinery absorbs the stall and catches up.
    /// `as_of` is the ledger the DB currently represents (last_synced).
    pub fn hydrate(&mut self, db: &rocksdb::DB, as_of: u32) {
        let t0 = std::time::Instant::now();
        let mut n = 0u64;
        let mut bad = 0u64;
        let iter = db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let Ok((k, v)) = item else { continue };
            if k.len() != 32 {
                continue;
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&k);
            match xrpl_core::codec::decode::decode_transaction_binary(&v) {
                Ok(jv) => {
                    let _ = self
                        .state
                        .state_map
                        .insert(Hash256(key), serde_json::to_vec(&jv).unwrap_or_default());
                    n += 1;
                }
                Err(_) => {
                    bad += 1;
                }
            }
        }
        let ms = t0.elapsed().as_millis() as u64;
        self.at_seq = as_of;
        self.hydrated = true;
        stats().hydrated.store(1, Ordering::Relaxed);
        stats().hydrate_objects.store(n, Ordering::Relaxed);
        stats().hydrate_decode_err.store(bad, Ordering::Relaxed);
        stats().hydrate_ms.store(ms, Ordering::Relaxed);
        eprintln!(
            "[native-shadow] hydrated {n} objects from state.rocks in {}s ({bad} undecodable) — mirror at #{as_of}",
            ms / 1000
        );
    }

    /// Apply ledger `seq` natively and compare against the FFI overlay.
    /// `txs` are the parsed `metaData`-bearing tx JSONs, `parent_hash_hex`
    /// the header's parent hash (skip-list input), `close_time` the header
    /// close time (the engine reads it off the mirror's base header).
    pub fn on_ledger(
        &mut self,
        seq: u32,
        parent_hash: &[u8; 32],
        parent_close_time: u32,
        total_drops: u64,
        txs: &[Value],
        ffi_overlay: &LedgerOverlay,
    ) {
        let st = stats();
        if !self.hydrated {
            return;
        }
        if self.at_seq + 1 != seq {
            // Gap (batch skip-ahead, resync): the mirror no longer represents
            // the parent. Mark unhydrated; the caller re-hydrates.
            st.skipped_gap.fetch_add(1, Ordering::Relaxed);
            self.hydrated = false;
            eprintln!("[native-shadow] gap: mirror at #{}, asked for #{seq} — will re-hydrate", self.at_seq);
            return;
        }
        let t0 = std::time::Instant::now();
        // The engine reads the BASE header: sequence/close_time of the PARENT
        // ledger (BookStep streams price expiry off sb.parentCloseTime) —
        // byte-identical recipe to state_replay's per-ledger header.
        self.state.header = LedgerHeader {
            sequence: seq - 1,
            total_coins: total_drops,
            parent_hash: Hash256(*parent_hash),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time,
            close_time: parent_close_time,
            close_time_resolution: 10,
            close_flags: 0,
        };
        let parent_hash_hex = hex::encode_upper(parent_hash);

        // Sort by TransactionIndex, exactly as the replay does.
        let mut ordered: Vec<&Value> = txs.iter().collect();
        ordered.sort_by_key(|t| t["metaData"]["TransactionIndex"].as_u64().unwrap_or(u64::MAX));

        // Flag-ledger NegativeUNL rotation (ledger-level, outside tx metas).
        let mut dirty: HashSet<Hash256> = HashSet::new();
        let mut undo: HashMap<Hash256, Option<Vec<u8>>> = HashMap::new();
        if seq % 256 == 0 {
            let nk = keylet::negative_unl_key();
            if let Some(bytes) = self.state.state_map.lookup(&nk).map(|b| b.to_vec()) {
                match xrpl_ledger::tx::pseudo::rotate_negative_unl(&bytes, seq) {
                    Some(Some(nb)) => {
                        undo.entry(nk).or_insert_with(|| Some(bytes.clone()));
                        let _ = self.state.state_map.insert(nk, nb);
                        dirty.insert(nk);
                    }
                    Some(None) => {
                        undo.entry(nk).or_insert_with(|| Some(bytes.clone()));
                        let _ = self.state.state_map.delete(&nk);
                        dirty.insert(nk);
                    }
                    None => {}
                }
            }
        }

        let mut ter_mm: Vec<String> = Vec::new();
        for tx in &ordered {
            let Some(txf) = build_txfields(tx) else { continue };
            let expected_ter = tx["metaData"]["TransactionResult"].as_str().unwrap_or("?");
            let tx_hash = tx["hash"].as_str().unwrap_or("").to_string();
            let (our_ter, mut mods) = native_apply_one(&self.state, &txf);
            // Threading stamps (PreviousTxnID/PreviousTxnLgrSeq) — the replay
            // applies them after every tx; first live ledger without them read
            // 175 byte-diffs at zero TER mismatches (#106628655).
            xrpl_ledger::ledger::threading::stamp_threading(
                &mut mods,
                &|k| self.state.state_map.lookup(k).map(|b| b.to_vec()),
                &tx_hash,
                seq,
            );
            st.txs_applied.fetch_add(1, Ordering::Relaxed);
            if our_ter == expected_ter {
                st.ter_matched.fetch_add(1, Ordering::Relaxed);
            } else {
                st.ter_mismatched.fetch_add(1, Ordering::Relaxed);
                ter_mm.push(format!(
                    "{}:{our_ter} vs {expected_ter}",
                    tx["hash"].as_str().unwrap_or("?").chars().take(12).collect::<String>()
                ));
            }
            for (k, ent) in mods {
                undo.entry(k).or_insert_with(|| self.state.state_map.lookup(&k).map(|b| b.to_vec()));
                match ent {
                    SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                        let _ = self.state.state_map.insert(k, b);
                    }
                    SandboxEntry::Deleted => {
                        let _ = self.state.state_map.delete(&k);
                    }
                }
                dirty.insert(k);
            }
        }
        update_skip_list(&mut self.state, &mut dirty, seq, &parent_hash_hex);

        // ---- Compare native overlay vs FFI overlay (singletons excluded) ----
        let mut missing: Vec<String> = Vec::new(); // FFI wrote, we didn't
        let mut extra: Vec<String> = Vec::new(); // we wrote, FFI didn't
        let mut byte_diff: Vec<String> = Vec::new();
        let mut compared = 0u64;
        for (k, ffi_val) in ffi_overlay {
            let kh = Hash256(*k);
            if is_singleton_key(&kh, seq) {
                continue;
            }
            compared += 1;
            if !dirty.contains(&kh) {
                missing.push(hex::encode_upper(k));
                continue;
            }
            let ours = self.state.state_map.lookup(&kh).map(|b| b.to_vec());
            match (ours, ffi_val) {
                (None, None) => {}
                (None, Some(_)) => byte_diff.push(format!("{}:deleted-vs-present", hex::encode_upper(k))),
                (Some(_), None) => byte_diff.push(format!("{}:present-vs-deleted", hex::encode_upper(k))),
                (Some(jb), Some(fb)) => {
                    let enc = serde_json::from_slice::<Value>(&jb).ok().and_then(|mut v| {
                        canon_for_encode(&mut v);
                        xrpl_core::codec::encode::encode_transaction_json(&v, false).ok()
                    });
                    match enc {
                        Some(ob) if &ob == fb => {}
                        Some(ob) => {
                            let off = ob.iter().zip(fb.iter()).position(|(a, b)| a != b).unwrap_or(ob.len().min(fb.len()));
                            byte_diff.push(format!("{}:@{off} ours-len {} ffi-len {}", hex::encode_upper(k), ob.len(), fb.len()));
                        }
                        None => byte_diff.push(format!("{}:encode-err", hex::encode_upper(k))),
                    }
                }
            }
        }
        for k in &dirty {
            if is_singleton_key(k, seq) {
                continue;
            }
            if !ffi_overlay.contains_key(&k.0) {
                extra.push(hex::encode_upper(k.0));
            }
        }

        let clean = missing.is_empty() && extra.is_empty() && byte_diff.is_empty();
        st.ledgers.fetch_add(1, Ordering::Relaxed);
        st.keys_compared.fetch_add(compared, Ordering::Relaxed);
        st.key_missing.fetch_add(missing.len() as u64, Ordering::Relaxed);
        st.key_extra.fetch_add(extra.len() as u64, Ordering::Relaxed);
        st.byte_mismatch.fetch_add(byte_diff.len() as u64, Ordering::Relaxed);
        if clean {
            st.full_match.fetch_add(1, Ordering::Relaxed);
        } else {
            st.overlay_diverged.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[native-shadow] #{seq} DIVERGED: missing={} extra={} bytes={} (ter-mm={})",
                missing.len(),
                extra.len(),
                byte_diff.len(),
                ter_mm.len()
            );
            if let Some(f) = &mut self.log {
                let _ = writeln!(
                    f,
                    "{}",
                    json!({
                        "seq": seq,
                        "missing": missing,
                        "extra": extra,
                        "byte_diff": byte_diff,
                        "ter_mismatch": ter_mm,
                    })
                );
            }
        }
        st.apply_ms_last.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);

        // ---- Reconcile the mirror onto the canonical trajectory ----
        // Every non-singleton native write reverts to its pre-ledger value,
        // then the FFI overlay's bytes (decoded to engine JSON) are applied.
        // Singleton/skip-list writes stay native — the replay proved them and
        // the FFI overlay never carries them.
        for (k, old) in undo {
            if is_singleton_key(&k, seq) {
                continue;
            }
            match old {
                Some(b) => {
                    let _ = self.state.state_map.insert(k, b);
                }
                None => {
                    let _ = self.state.state_map.delete(&k);
                }
            }
        }
        for (k, ffi_val) in ffi_overlay {
            let kh = Hash256(*k);
            if is_singleton_key(&kh, seq) {
                continue;
            }
            match ffi_val {
                Some(fb) => match xrpl_core::codec::decode::decode_transaction_binary(fb) {
                    Ok(jv) => {
                        let _ = self
                            .state
                            .state_map
                            .insert(kh, serde_json::to_vec(&jv).unwrap_or_default());
                    }
                    Err(_) => {
                        // A canonical byte we cannot decode leaves the mirror
                        // stale on this key: safer to drop the mirror than
                        // silently drift. Re-hydrate.
                        eprintln!("[native-shadow] #{seq}: undecodable canonical byte — re-hydrating");
                        self.hydrated = false;
                        return;
                    }
                },
                None => {
                    let _ = self.state.state_map.delete(&kh);
                }
            }
        }
        self.at_seq = seq;
    }
}
