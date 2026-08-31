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
    pub hydrate_skipped_lowmem: AtomicU64,
    pub hydrate_reencode_bad: AtomicU64,
    pub reconcile_leaks: AtomicU64,
    pub leak_retry_fixed: AtomicU64,
    pub key_noop_missing: AtomicU64,
    pub key_noop_extra: AtomicU64,
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

/// Audit OUR pre-value of a byte-diffed key against the ledger metadata's
/// PreviousFields: the first tx (apply order) to touch a key records the
/// entry's state BEFORE the ledger — exactly what the mirror should have
/// held going in. " STALE[..]" = the mirror's input was wrong (instrument);
/// " PRE-OK" = the input was right and the divergence arose inside the
/// apply (engine). Meta uses r-addresses inside amount objects where the
/// mirror dialect is hex, so object-valued fields compare value+currency.
/// Each verdict names the first-toucher tx (hash#index): a mid-ledger STALE
/// on a key whose first toucher is EARLIER than the diverging tx is an
/// intra-ledger cascade, not pre-ledger staleness (#106670827 read that way).
fn pre_stale_audit(undo_pre: Option<&[u8]>, ordered: &[&Value], key_hex: &str) -> String {
    let mine: Option<Value> = undo_pre.and_then(|b| serde_json::from_slice(b).ok());
    for tx in ordered {
        for n in tx["metaData"]["AffectedNodes"].as_array().into_iter().flatten() {
            for node in n.as_object().into_iter().flat_map(|o| o.values()) {
                if node["LedgerIndex"].as_str() != Some(key_hex) {
                    continue;
                }
                let toucher = format!(
                    " tx={}#{}",
                    tx["hash"].as_str().unwrap_or("?").chars().take(12).collect::<String>(),
                    tx["metaData"]["TransactionIndex"].as_u64().unwrap_or(u64::MAX)
                );
                let Some(pf) = node["PreviousFields"].as_object() else {
                    return format!(" PRE-UNKNOWN(no-prev){toucher}");
                };
                let mut stale = Vec::new();
                for (f, want) in pf {
                    if f == "PreviousTxnID" || f == "PreviousTxnLgrSeq" {
                        continue;
                    }
                    let have = mine.as_ref().and_then(|m| m.get(f));
                    let eq = match (have, want) {
                        (Some(h), w) if h.is_object() && w.is_object() => {
                            h.get("value") == w.get("value") && h.get("currency") == w.get("currency")
                        }
                        (h, w) => h == Some(w),
                    };
                    if !eq {
                        stale.push(format!("{f} mirror={} meta={}", disp(have), disp(Some(want))));
                    }
                }
                return if stale.is_empty() {
                    format!(" PRE-OK{toucher}")
                } else {
                    format!(" STALE[{}]{toucher}", stale.join(" | "))
                };
            }
        }
    }
    " PRE-UNKNOWN(no-meta)".into()
}

/// Compact value display for audit receipts: an object's `value` (amounts)
/// beats its currency-first prefix — a 28-char whole-object truncation hid
/// every number in the #106670827 receipt.
fn disp(v: Option<&Value>) -> String {
    match v {
        None => "ABSENT".into(),
        Some(x) => {
            let s = x.get("value").map(|w| w.to_string()).unwrap_or_else(|| x.to_string());
            s.chars().take(64).collect()
        }
    }
}

/// MemAvailable from /proc/meminfo, in GB — the kernel's honest "how much
/// can you allocate before we start reclaiming/swapping" figure.
fn mem_available_gb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1_048_576)
}

/// Our own resident set, in GB (VmRSS via /proc/self/statm page count).
fn rss_gb() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: f64 = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096.0 / 1_073_741_824.0)
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
    /// Monotonic instant of the last hydration — the caller gates re-entry
    /// (v3's death loop: hydrate → 2-min stall → lag → overlay gap →
    /// re-hydrate). At most one hydration per cooldown window.
    pub last_hydrate: Option<std::time::Instant>,
    log: Option<std::fs::File>,
}

impl NativeShadow {
    /// Free the mirror NOW. A stale 19.8M-object map is ~14GB of dead weight,
    /// and keeping it while waiting to re-hydrate deadlocks the budget gate
    /// against itself: the gate refuses to hydrate because the corpse of the
    /// LAST mirror is still holding the RAM (2026-08-30: one orphaned mirror,
    /// 108 refusals overnight, MemAvailable pinned at 11GB on a box whose
    /// steady baseline affords 25+).
    fn drop_mirror(&mut self, why: &str) {
        self.state = LedgerState::new_unverified(self.state.header.clone());
        self.hydrated = false;
        // Keep the dashboard honest: the stats twin of `hydrated` stayed 1
        // through drops until 2026-08-31 (API said True over a dropped mirror).
        stats().hydrated.store(0, Ordering::Relaxed);
        eprintln!("[native-shadow] mirror dropped ({why}) — memory returns as jemalloc purges");
    }

    /// May the caller hydrate now? Only near the live edge (a mid-catch-up
    /// hydration guarantees the next overlay ledger gaps past the mirror)
    /// and not more than once per 10 minutes.
    pub fn can_hydrate(&self, lag: u32) -> bool {
        lag <= 2
            && self
                .last_hydrate
                .map(|t| t.elapsed() > std::time::Duration::from_secs(600))
                .unwrap_or(true)
    }
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
        Some(Self { state: LedgerState::new_unverified(header), at_seq: 0, hydrated: false, last_hydrate: None, log })
    }

    /// Full-scan `state.rocks` into the in-RAM mirror, decoding every binary
    /// SLE to the engine's JSON form. Blocks the sync loop once (~minutes);
    /// ws-sync's hold-position machinery absorbs the stall and catches up.
    /// `as_of` is the ledger the DB currently represents (last_synced).
    pub fn hydrate(&mut self, db: &rocksdb::DB, as_of: u32) {
        // v5 memory diet. v4's single greedy pass spiked live_viewer to
        // 58.6GB on the 62GB box — swap filled and the machine thrashed
        // (the mirror itself fits fine; the SPIKE is the killer). Three
        // levers, receipts to prove them:
        //   1. Budget gate: refuse to start unless MemAvailable covers the
        //      mirror plus slack — a skipped hydrate is a receipt, a
        //      thrashing validator is an outage.
        //   2. fill_cache(false): the full scan otherwise pumps the whole
        //      store through the 4GB block cache (engine.rs:274) for blocks
        //      we will never read again.
        //   3. Progress receipts with live RSS every 2M objects, so the
        //      console shows WHERE the memory goes instead of a silent
        //      climb ending in SIGKILL. (Pair with MALLOC_CONF
        //      background_thread:true,dirty_decay_ms:1000 at launch — the
        //      20M dropped decode temporaries then return to the OS on a
        //      purger thread instead of lingering in the arenas.)
        // v3 died here: re-hydration INSERTED into the existing 19.8M-object
        // map without clearing — two passes ~doubled RSS and the third met
        // the OOM killer (silent SIGKILL, no panic line). Fresh map FIRST —
        // and BEFORE the budget gate below, or a stale mirror deadlocks the
        // gate against itself (the corpse holds the very RAM the gate is
        // waiting to see free). The gap/undecodable paths drop_mirror()
        // eagerly, so this is usually a no-op; it stays for any re-hydrate
        // path that didn't.
        self.state = LedgerState::new_unverified(self.state.header.clone());
        let min_gb: u64 = std::env::var("XRPL_SHADOW_HYDRATE_MIN_GB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);
        if let Some(avail) = mem_available_gb() {
            if avail < min_gb {
                stats().hydrate_skipped_lowmem.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[native-shadow] hydrate SKIPPED — MemAvailable {avail}GB < {min_gb}GB budget (XRPL_SHADOW_HYDRATE_MIN_GB); retry after cooldown"
                );
                // Arm the cooldown so the next attempt is 600s out, not
                // every ledger.
                self.last_hydrate = Some(std::time::Instant::now());
                return;
            }
        }
        let t0 = std::time::Instant::now();
        let mut n = 0u64;
        let mut bad = 0u64;
        // Integrity audit: the scan plants ~20M decoded objects into the
        // mirror with nothing checking them — after the per-ledger reconcile
        // verifier landed, this is the one unguarded corruption channel left.
        // Re-encode each decoded object and demand the raw store bytes back;
        // failures are counted and the first few named. Costs roughly half
        // the scan time again; XRPL_SHADOW_HYDRATE_AUDIT=0 disables.
        let audit = std::env::var("XRPL_SHADOW_HYDRATE_AUDIT").map(|v| v != "0").unwrap_or(true);
        let mut bad_rt = 0u64;
        // Trust the store's own stamp over the caller's belief: the writer
        // stamps last-written-seq inside each ledger's atomic batch, and the
        // sync loop is frozen while we scan, so the stamp IS the scan's
        // as-of. A missing stamp (pre-stamp store) falls back to the caller.
        let as_of = match db
            .get(b"meta:last_seq")
            .ok()
            .flatten()
            .and_then(|v| <[u8; 4]>::try_from(&v[..]).ok())
            .map(u32::from_le_bytes)
        {
            Some(stamped) => {
                if stamped != as_of {
                    eprintln!(
                        "[native-shadow] hydrate as_of: caller believed #{as_of}, store stamp says #{stamped} — trusting the stamp"
                    );
                }
                stamped
            }
            None => as_of,
        };
        let mut ro = rocksdb::ReadOptions::default();
        ro.fill_cache(false);
        let iter = db.iterator_opt(rocksdb::IteratorMode::Start, ro);
        for item in iter {
            let Ok((k, v)) = item else { continue };
            if k.len() != 32 {
                continue;
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&k);
            match xrpl_core::codec::decode::decode_transaction_binary(&v) {
                Ok(mut jv) => {
                    crate::native_apply::hexify_addresses(&mut jv);
                    if audit {
                        let mut cv = jv.clone();
                        crate::native_apply::canon_for_encode(&mut cv);
                        let ok = xrpl_core::codec::encode::encode_transaction_json(&cv, false)
                            .map(|rb| rb.as_slice() == &v[..])
                            .unwrap_or(false);
                        if !ok {
                            bad_rt += 1;
                            if bad_rt <= 5 {
                                eprintln!(
                                    "[native-shadow] hydrate REENCODE-BAD {} ({} bytes)",
                                    hex::encode_upper(key),
                                    v.len()
                                );
                            }
                        }
                    }
                    let _ = self
                        .state
                        .state_map
                        .insert(Hash256(key), serde_json::to_vec(&jv).unwrap_or_default());
                    n += 1;
                    if n % 2_000_000 == 0 {
                        eprintln!(
                            "[native-shadow] hydrating… {}M objects, RSS {:.1}GB, {}s",
                            n / 1_000_000,
                            rss_gb().unwrap_or(0.0),
                            t0.elapsed().as_secs()
                        );
                    }
                }
                Err(_) => {
                    bad += 1;
                }
            }
        }
        let ms = t0.elapsed().as_millis() as u64;
        self.at_seq = as_of;
        self.hydrated = true;
        self.last_hydrate = Some(std::time::Instant::now());
        stats().hydrated.store(1, Ordering::Relaxed);
        stats().hydrate_objects.store(n, Ordering::Relaxed);
        stats().hydrate_decode_err.store(bad, Ordering::Relaxed);
        stats().hydrate_reencode_bad.store(bad_rt, Ordering::Relaxed);
        stats().hydrate_ms.store(ms, Ordering::Relaxed);
        eprintln!(
            "[native-shadow] hydrated {n} objects from state.rocks in {}s ({bad} undecodable, {bad_rt} reencode-bad) — mirror at #{as_of}",
            ms / 1000
        );
        if bad_rt > 0 {
            if let Some(f) = &mut self.log {
                let _ = writeln!(
                    f,
                    "{}",
                    json!({ "hydrate_audit": { "as_of": as_of, "objects": n, "reencode_bad": bad_rt } })
                );
            }
        }
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
            // the parent. Drop it NOW; the caller re-hydrates.
            st.skipped_gap.fetch_add(1, Ordering::Relaxed);
            eprintln!("[native-shadow] gap: mirror at #{}, asked for #{seq} — will re-hydrate", self.at_seq);
            self.drop_mirror("gap");
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
                // Stale-mirror audit: for each key this tx's TRUE meta touched,
                // does the mirror's CURRENT value match the meta's
                // PreviousFields where given? A named mismatch here = the key
                // went stale in the mirror before this ledger — the class name
                // tells us which reconcile lane is leaking.
                let mut stale: Vec<String> = Vec::new();
                for node in tx["metaData"]["AffectedNodes"].as_array().into_iter().flatten() {
                    let n = &node["ModifiedNode"];
                    let (Some(li), Some(pf)) = (n["LedgerIndex"].as_str(), n["PreviousFields"].as_object()) else { continue };
                    let Ok(kb) = hex::decode(li) else { continue };
                    let Ok(karr) = <[u8; 32]>::try_from(kb.as_slice()) else { continue };
                    let mine = self
                        .state
                        .state_map
                        .lookup(&Hash256(karr))
                        .and_then(|b| serde_json::from_slice::<Value>(b).ok());
                    for (f, want) in pf {
                        if f == "PreviousTxnID" || f == "PreviousTxnLgrSeq" {
                            continue;
                        }
                        let have = mine.as_ref().and_then(|m| m.get(f));
                        if have != Some(want) {
                            let ty = n["LedgerEntryType"].as_str().unwrap_or("?");
                            stale.push(format!(
                                "{}:{ty}.{f} mirror={} meta={}",
                                &li[..12.min(li.len())],
                                disp(have),
                                disp(Some(want))
                            ));
                        }
                    }
                }
                // Defect B instrument: the first mismatch per ledger dumps the
                // ENGINE'S-EYE tx — the parsed fields exactly as build_txfields
                // delivered them. Offline replays of the same ledgers read
                // clean, so if the live inputs differ in any byte, this names
                // it; if they match, the divergence is environmental to the
                // process and the dump proves that too.
                let dump = if ter_mm.is_empty() {
                    let fields = serde_json::to_string(&txf.fields).unwrap_or_default();
                    format!(" FIELDS[{}]", fields.chars().take(700).collect::<String>())
                } else {
                    String::new()
                };
                ter_mm.push(format!(
                    "{}:{} {our_ter} vs {expected_ter}{}{}",
                    tx["hash"].as_str().unwrap_or("?").chars().take(12).collect::<String>(),
                    txf.tx_type,
                    if stale.is_empty() { String::new() } else { format!(" STALE[{}]", stale.join(" | ")) },
                    dump
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
        let mut noop_missing = 0u64; // FFI wrote a value the mirror already held
        let mut noop_extra = 0u64; // we wrote back exactly the pre-state
        let mut byte_diff: Vec<String> = Vec::new();
        let mut compared = 0u64;
        for (k, ffi_val) in ffi_overlay {
            let kh = Hash256(*k);
            if is_singleton_key(&kh, seq) {
                continue;
            }
            compared += 1;
            if !dirty.contains(&kh) {
                // Value-aware judge: the FFI leg re-writes objects it merely
                // READ (on-demand hydration), so its overlay carries keys no
                // transaction changed. If the overlay bytes equal what the
                // mirror already holds, that is bookkeeping, not divergence —
                // count it and stay quiet. (2026-08-31 soak: 1-3 phantom
                // AccountRoots per ledger, one proven absent from every tx's
                // metadata in its ledger.)
                let ours_bin = self
                    .state
                    .state_map
                    .lookup(&kh)
                    .and_then(|jb| serde_json::from_slice::<Value>(jb).ok())
                    .and_then(|mut v| {
                        canon_for_encode(&mut v);
                        xrpl_core::codec::encode::encode_transaction_json(&v, false).ok()
                    });
                if ours_bin.as_deref() == ffi_val.as_deref() {
                    noop_missing += 1;
                    continue;
                }
                // Name the class: decode the FFI bytes for LedgerEntryType so
                // the receipt histogram says WHAT we fail to touch.
                let ty = ffi_val
                    .as_ref()
                    .and_then(|b| xrpl_core::codec::decode::decode_transaction_binary(b).ok())
                    .and_then(|v| v["LedgerEntryType"].as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "deleted".to_string());
                missing.push(format!("{}:{ty}", hex::encode_upper(k)));
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
                            // The decisive discriminator (2026-08-31): audit
                            // OUR pre-value (undo map) against the ledger
                            // meta's PreviousFields BEFORE the reconcile
                            // erases the evidence. STALE[..] = the input was
                            // wrong (mirror problem); PRE-OK = the input was
                            // right and the divergence arose inside the apply
                            // (engine problem). Both specimens of finding-38
                            // died unclassifiable for want of this line.
                            let audit = pre_stale_audit(
                                undo.get(&kh).and_then(|o| o.as_deref()),
                                &ordered,
                                &hex::encode_upper(k),
                            );
                            byte_diff.push(format!("{}:@{off} ours-len {} ffi-len {}{audit}", hex::encode_upper(k), ob.len(), fb.len()));
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
                // Our no-op twin: we wrote back exactly what was there
                // (rippled elides unchanged writes from its overlay).
                let pre = undo.get(k).and_then(|o| o.as_deref());
                let post = self.state.state_map.lookup(k).map(|b| &b[..]);
                if pre == post {
                    noop_extra += 1;
                    continue;
                }
                extra.push(hex::encode_upper(k.0));
            }
        }

        let clean = missing.is_empty() && extra.is_empty() && byte_diff.is_empty();
        st.ledgers.fetch_add(1, Ordering::Relaxed);
        st.keys_compared.fetch_add(compared, Ordering::Relaxed);
        st.key_missing.fetch_add(missing.len() as u64, Ordering::Relaxed);
        st.key_extra.fetch_add(extra.len() as u64, Ordering::Relaxed);
        st.byte_mismatch.fetch_add(byte_diff.len() as u64, Ordering::Relaxed);
        st.key_noop_missing.fetch_add(noop_missing, Ordering::Relaxed);
        st.key_noop_extra.fetch_add(noop_extra, Ordering::Relaxed);
        if clean {
            st.full_match.fetch_add(1, Ordering::Relaxed);
        } else {
            st.overlay_diverged.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[native-shadow] #{seq} DIVERGED: missing={} extra={} bytes={} (ter-mm={}, noop {}+{})",
                missing.len(),
                extra.len(),
                byte_diff.len(),
                ter_mm.len(),
                noop_missing,
                noop_extra
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
                        "noop_missing": noop_missing,
                        "noop_extra": noop_extra,
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
                    Ok(mut jv) => {
                        crate::native_apply::hexify_addresses(&mut jv);
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
                        self.drop_mirror("undecodable canonical byte");
                        return;
                    }
                },
                None => {
                    let _ = self.state.state_map.delete(&kh);
                }
            }
        }
        // Post-reconcile verifier: every overlay key re-read from the mirror
        // and byte-checked. A failure here is the leak AT BIRTH — the stale
        // audits above only see it ledgers later. Each failure dumps full
        // forensics to the receipt log (canonical bytes, mirror JSON, the
        // re-encode result — an encode ERROR is a named verdict, previously
        // indistinguishable from a byte diff) and then re-asserts the
        // canonical value: `retry_fixed` splits a transient write fault from
        // a deterministic in-process encode fault, and the re-assert repairs
        // the mirror whenever it can (2026-08-31: 5 leaks/day, all created-
        // then-deleted book pages, byte-identical bytes EXACT in every
        // offline process — only live forensics can name the mechanism).
        let reencode = |raw: &[u8]| -> Result<Vec<u8>, String> {
            let mut v: Value = serde_json::from_slice(raw).map_err(|e| format!("parse: {e}"))?;
            canon_for_encode(&mut v);
            xrpl_core::codec::encode::encode_transaction_json(&v, false)
                .map_err(|e| format!("encode: {e:?}"))
        };
        let mut leak = 0u32;
        for (k, ffi_val) in ffi_overlay {
            let kh = Hash256(*k);
            if is_singleton_key(&kh, seq) {
                continue;
            }
            let mine = self.state.state_map.lookup(&kh).map(|b| b.to_vec());
            let (kind, jb_str, re_hex, err, off) = match (mine, ffi_val) {
                (None, None) => continue,
                (Some(jb), Some(fb)) => match reencode(&jb) {
                    Ok(b) if &b == fb => continue,
                    Ok(b) => {
                        let off = b
                            .iter()
                            .zip(fb.iter())
                            .position(|(a, c)| a != c)
                            .unwrap_or(b.len().min(fb.len()));
                        (
                            "reencode",
                            Some(String::from_utf8_lossy(&jb).into_owned()),
                            Some(hex::encode_upper(&b)),
                            None,
                            Some(off),
                        )
                    }
                    Err(e) => {
                        ("reencode", Some(String::from_utf8_lossy(&jb).into_owned()), None, Some(e), None)
                    }
                },
                (Some(jb), None) => {
                    ("undead", Some(String::from_utf8_lossy(&jb).into_owned()), None, None, None)
                }
                (None, Some(_)) => ("vanished", None, None, None, None),
            };
            leak += 1;
            st.reconcile_leaks.fetch_add(1, Ordering::Relaxed);
            // Re-assert the canonical trajectory for this key, then re-audit.
            let mut retry_fixed = false;
            let mut jb_retry: Option<String> = None;
            match ffi_val {
                Some(fb) => {
                    if let Ok(mut jv) = xrpl_core::codec::decode::decode_transaction_binary(fb) {
                        crate::native_apply::hexify_addresses(&mut jv);
                        let _ = self
                            .state
                            .state_map
                            .insert(kh, serde_json::to_vec(&jv).unwrap_or_default());
                    }
                    if let Some(jb2) = self.state.state_map.lookup(&kh).map(|b| b.to_vec()) {
                        retry_fixed = matches!(reencode(&jb2), Ok(b) if &b == fb);
                        if !retry_fixed {
                            jb_retry = Some(String::from_utf8_lossy(&jb2).into_owned());
                        }
                    }
                }
                None => {
                    let _ = self.state.state_map.delete(&kh);
                    retry_fixed = self.state.state_map.lookup(&kh).is_none();
                }
            }
            if retry_fixed {
                st.leak_retry_fixed.fetch_add(1, Ordering::Relaxed);
            }
            if leak <= 3 {
                eprintln!(
                    "[native-shadow] #{seq} RECONCILE-LEAK {kind} {} ({}; retry_fixed={retry_fixed})",
                    hex::encode_upper(k),
                    err.clone().unwrap_or_else(|| match off {
                        Some(o) => format!("diff @{o}"),
                        None => "state".into(),
                    })
                );
            }
            // Full forensics for the first few leaks per ledger; a systemic
            // event (say, a whole bad hydration) still counts every leak but
            // must not write hundreds of multi-KB dumps per ledger.
            if leak <= 4 {
                if let Some(f) = &mut self.log {
                    let _ = writeln!(
                        f,
                        "{}",
                        json!({
                            "seq": seq,
                            "leak": {
                                "kind": kind,
                                "key": hex::encode_upper(k),
                                "fb": ffi_val.as_ref().map(hex::encode_upper),
                                "jb": jb_str,
                                "re": re_hex,
                                "err": err,
                                "off": off,
                                "retry_fixed": retry_fixed,
                                "jb_retry": jb_retry,
                            }
                        })
                    );
                }
            }
        }
        if leak > 0 {
            eprintln!("[native-shadow] #{seq} RECONCILE-LEAK total={leak}");
        }
        self.at_seq = seq;
    }
}
