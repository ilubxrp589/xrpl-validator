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
//!
//! Stage 4 Phase B, the native writer (`XRPL_NATIVE_WRITER=1`, 2026-10-08):
//! the native overlay becomes the authoritative source for `state.rocks` and
//! the FFI leg a comparison only. ws-sync asks [`NativeShadow::propose`] for a
//! ledger's writes, then reports what it wrote ([`NativeShadow::commit`]) or
//! that nothing landed ([`NativeShadow::abort`]); the mirror follows the bytes
//! that were written and verified, never its own guess. The engine vouches
//! for a ledger only when every result matches the network's metadata and its
//! writes cover exactly the objects the metadata names; otherwise that
//! ledger's bytes come from the network, counted. The state hash stays the
//! final check, and a mismatch on our bytes hands the retry to the network's.
//! Why (2026-10-08 #107525002): libxrpl 3.4.0 refused PermissionDelegationV1_1
//! transactions the network applied, and under Stage 3 its overlay was what
//! got written — the validator halted while this engine had it right.

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
    pub map_op_err: AtomicU64,
    pub key_noop_missing: AtomicU64,
    pub key_noop_extra: AtomicU64,
    pub ledgers: AtomicU64,
    pub full_match: AtomicU64,
    pub overlay_diverged: AtomicU64,
    pub txs_applied: AtomicU64,
    pub ter_matched: AtomicU64,
    pub ter_mismatched: AtomicU64,
    /// Batch INNER transactions whose result disagreed with the ledger's,
    /// counted separately and NEVER folded into `ter_mismatched`: that pair
    /// counts ledger ENTRIES applied (`ter_matched + ter_mismatched ==
    /// txs_applied`, one per outer), and an inner is not an entry this leg
    /// applied on its own. A tick here with `ter_mismatched` at zero means the
    /// outer agreed while something inside it did not.
    pub batch_inner_ter_mm: AtomicU64,
    pub keys_compared: AtomicU64,
    pub key_missing: AtomicU64,
    pub key_extra: AtomicU64,
    pub byte_mismatch: AtomicU64,
    pub skipped_gap: AtomicU64,
    /// Ledgers the FFI leg did not verify (an empty overlay under real
    /// transactions): no compare, the mirror dropped for re-hydration.
    pub skipped_unverified: AtomicU64,
    pub apply_ms_last: AtomicU64,
    /// The receipt canary ([`NativeShadow::arm_canary`]): injections made, and
    /// how many of them the compare flagged. Fired above detected means the
    /// compare has gone blind, and a zero-receipt soak proves nothing.
    pub canary_fired: AtomicU64,
    pub canary_detected: AtomicU64,
    /// Phase B, the native writer. `writer_native`: ledgers whose state.rocks
    /// bytes came from our overlay. The rest name why a ledger's bytes came
    /// from the network instead: no mirror yet or a gap (`writer_unready`), the
    /// engine disagreed with the metadata (`writer_refused`), or the retry of a
    /// ledger our bytes failed the state hash on (`writer_distrusted`, one per
    /// `writer_mismatch`).
    pub writer_enabled: AtomicU64,
    pub writer_native: AtomicU64,
    pub writer_unready: AtomicU64,
    pub writer_refused: AtomicU64,
    pub writer_distrusted: AtomicU64,
    pub writer_mismatch: AtomicU64,
    /// 1 once BREAKER_FAILS hash failures on our bytes landed within
    /// BREAKER_WINDOW ledgers: the writer is off until the next restart.
    pub writer_breaker: AtomicU64,
}

pub fn stats() -> &'static ShadowStats {
    static S: OnceLock<ShadowStats> = OnceLock::new();
    S.get_or_init(ShadowStats::default)
}

/// Count a mirror map-op failure instead of swallowing it. The 2026-08-31
/// depth-64 freeze hid for hours behind `let _ = insert(...)` — silently
/// dropped writes froze book pages while lookups kept serving stale values.
/// With the tree fixed this counter must read 0 forever; any tick is a
/// structural regression.
fn note_map_op(r: Result<bool, xrpl_ledger::error::LedgerError>) {
    if r.is_err() {
        stats().map_op_err.fetch_add(1, Ordering::Relaxed);
    }
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
                    // Align the meta's dialect before comparing: r-addresses
                    // become hex like the mirror's (arrays too — VoteSlots
                    // false-STALEd #106678645 on VoteEntry.Account spelling
                    // with byte-identical values, the 75d553e bug's array
                    // flavor).
                    let mut want_hex = want.clone();
                    crate::native_apply::hexify_addresses(&mut want_hex);
                    let eq = match (have, &want_hex) {
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
    /// A hydration in flight on its own thread (WS-lag fix step 2,
    /// 2026-09-01): the mirror is built from a RocksDB snapshot while the
    /// sync loop keeps flowing; ledgers that arrive meanwhile queue in
    /// `pending` and replay in order the moment the build lands. Before
    /// this the scan ran ON the sync loop — 244 s of stall per launch,
    /// ~65 ledgers of backlog the loop could never drain (memory
    /// project_validator_wssync_lag_2026_09_01).
    hydrating: Option<std::thread::JoinHandle<Option<HydrateOutcome>>>,
    pending: Vec<PendingLedger>,
    /// The receipt canary's trigger file (`XRPL_NATIVE_SHADOW_CANARY`, else
    /// `native_shadow.canary` beside the receipt log).
    canary: Option<std::path::PathBuf>,
    /// Phase B: the proposal ws-sync has not settled yet (commit or abort).
    held: Option<Held>,
    /// Phase B: a ledger our bytes failed the state hash on. Its retry is
    /// written from the network's bytes; the commit clears it.
    distrust: Option<u32>,
    /// Phase B: the writer's own decisions (`XRPL_NATIVE_WRITER_LOG`, else
    /// `native_writer.jsonl` beside the receipt log). Never receipts: those
    /// stay the overlay-vs-overlay compare's.
    writer_log: Option<std::fs::File>,
    /// Phase B on (enable_writer).
    writer: bool,
    /// Keys the mirror may hold wrong: the hydrate could not load them cleanly,
    /// or a reconcile could not make them re-encode to the written bytes. The
    /// writer never vouches for a ledger that writes one.
    bad_keys: HashSet<[u8; 32]>,
    /// More bad keys than BAD_KEYS_CAP: vouch for nothing until a re-hydrate.
    bad_overflow: bool,
    /// Ledgers whose writes from our bytes failed the state hash (the breaker's window).
    writer_fails: Vec<u32>,
    writer_tripped: bool,
}

/// What the writer hands ws-sync for one ledger.
pub struct Proposal {
    /// Our writes in canonical binary, singletons left out: ws-sync fetches
    /// those from the network, as Stage 3 did.
    pub overlay: LedgerOverlay,
    /// Why the engine cannot vouch for this ledger; ws-sync then writes the
    /// network's bytes. None: write `overlay`.
    pub refusal: Option<String>,
}

/// A proposal's pre-images, kept until ws-sync says whether the ledger landed.
struct Held {
    seq: u32,
    undo: HashMap<Hash256, Option<Vec<u8>>>,
}

/// One ledger applied to the mirror: what it touched, the pre-images, and the
/// result codes that disagreed with the ledger's metadata.
struct Applied {
    dirty: HashSet<Hash256>,
    undo: HashMap<Hash256, Option<Vec<u8>>>,
    ter_mm: Vec<String>,
    /// Transactions the engine could not even parse (build_txfields None): applied by nobody.
    skipped: usize,
}

/// The ledger's transactions in apply order (TransactionIndex), as the replay does.
fn apply_order(txs: &[Value]) -> Vec<&Value> {
    let mut ordered: Vec<&Value> = txs.iter().collect();
    ordered.sort_by_key(|t| t["metaData"]["TransactionIndex"].as_u64().unwrap_or(u64::MAX));
    ordered
}

/// Engine JSON -> the canonical binary the network stores.
fn encode_entry(jb: &[u8]) -> Result<Vec<u8>, String> {
    let mut v: Value = serde_json::from_slice(jb).map_err(|e| format!("parse: {e}"))?;
    canon_for_encode(&mut v);
    xrpl_core::codec::encode::encode_transaction_json(&v, false).map_err(|e| format!("encode: {e:?}"))
}

/// What the hydrate thread hands back.
struct HydrateOutcome {
    state: LedgerState,
    at_seq: u32,
    objects: u64,
    undecodable: u64,
    reencode_bad: u64,
    ms: u64,
    /// The keys behind `undecodable` and `reencode_bad` (at most BAD_KEYS_CAP;
    /// `bad_overflow` when there were more).
    bad_keys: HashSet<[u8; 32]>,
    bad_overflow: bool,
}

/// Phase B keeps the keys a hydrate could not load cleanly and never vouches
/// for a ledger that writes one; past this many it vouches for nothing.
const BAD_KEYS_CAP: usize = 4096;

/// Phase B breaker: this many hash failures on our bytes within
/// BREAKER_WINDOW ledgers turns the writer off until the next restart.
const BREAKER_FAILS: usize = 3;
const BREAKER_WINDOW: u32 = 1000;

/// A ledger the sync loop processed while the mirror was still building.
enum PendingLedger {
    /// Phase A: apply it and compare against its FFI overlay.
    Shadow {
        seq: u32,
        parent_hash: [u8; 32],
        parent_close_time: u32,
        total_drops: u64,
        txs: Vec<Value>,
        overlay: LedgerOverlay,
    },
    /// Phase B: the bytes ws-sync wrote for it; the mirror only follows them.
    Written { seq: u32, written: LedgerOverlay },
}

impl PendingLedger {
    fn seq(&self) -> u32 {
        match self {
            PendingLedger::Shadow { seq, .. } | PendingLedger::Written { seq, .. } => *seq,
        }
    }
}

/// Deeper than this and the wait is not a hydration any more — drop the
/// attempt (the caller's cooldown re-arms it) rather than hold ~GBs of
/// buffered ledgers.
const PENDING_CAP: usize = 400;

/// Pseudo-transactions: they touch only the singletons the FFI overlay never
/// carries, so a ledger of nothing else legitimately yields an empty overlay.
const PSEUDO_TX_TYPES: [&str; 3] = ["EnableAmendment", "SetFee", "UNLModify"];

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
        // A build still running is detached: its result is discarded when
        // it lands (the JoinHandle is gone), and the buffer with it.
        self.hydrating = None;
        self.pending.clear();
        self.held = None;
        self.bad_keys.clear();
        self.bad_overflow = false;
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
        let canary = std::env::var("XRPL_NATIVE_SHADOW_CANARY")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| std::path::Path::new(&path).parent().map(|d| d.join("native_shadow.canary")));
        eprintln!(
            "[native-shadow] ENABLED — hydrates on first steady ledger; receipts -> {path}; canary trigger {}",
            canary.as_ref().map_or("off".to_string(), |p| p.display().to_string())
        );
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
        Some(Self {
            state: LedgerState::new_unverified(header),
            at_seq: 0,
            hydrated: false,
            last_hydrate: None,
            log,
            hydrating: None,
            pending: Vec::new(),
            canary,
            held: None,
            distrust: None,
            writer_log: None,
            writer: false,
            bad_keys: HashSet::new(),
            bad_overflow: false,
            writer_fails: Vec::new(),
            writer_tripped: false,
        })
    }

    /// Phase B on: open the writer's log and mark it in the stats.
    pub fn enable_writer(&mut self) {
        self.writer = true;
        stats().writer_enabled.store(1, Ordering::Relaxed);
        let path = std::env::var("XRPL_NATIVE_WRITER_LOG").unwrap_or_else(|_| {
            let receipts = std::env::var("XRPL_NATIVE_SHADOW_LOG")
                .unwrap_or_else(|_| "/mnt/xrpl-data/native_shadow.jsonl".to_string());
            std::path::Path::new(&receipts)
                .parent()
                .map(|d| d.join("native_writer.jsonl").display().to_string())
                .unwrap_or_else(|| "native_writer.jsonl".to_string())
        });
        self.writer_log = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        eprintln!("[native-writer] ENABLED — our overlay writes state.rocks once the mirror is hydrated; decisions -> {path}");
    }

    fn writer_note(&mut self, line: Value) {
        if let Some(f) = &mut self.writer_log {
            let _ = writeln!(f, "{line}");
        }
    }

    /// The receipt canary (2026-09-23). A zero-receipt soak proves nothing
    /// unless the compare can still see a divergence, so while the trigger
    /// file exists the shadow plants one: the lowest-keyed AccountRoot this
    /// ledger wrote that currently encodes EQUAL to the FFI leg's bytes gets
    /// one drop added to its Balance in the mirror — a simulated engine miss,
    /// planted after the apply and before the compare so it travels the real
    /// path. Choosing an agreeing entry means the plant never lands on (or
    /// hides) a real divergence. The compare must flag it; `on_ledger` keeps
    /// it out of every divergence counter, writes it as a `canary` line, and
    /// removes the trigger. The reconcile then restores the entry like any
    /// other native write (the key is dirty, so its pre-image is in `undo`).
    /// Returns the planted key; None when disarmed or when this ledger wrote
    /// no suitable entry (the trigger stays for the next ledger).
    fn arm_canary(&mut self, dirty: &HashSet<Hash256>, ffi_overlay: &LedgerOverlay, seq: u32) -> Option<Hash256> {
        if !self.canary.as_ref()?.exists() {
            return None;
        }
        let mut keys: Vec<Hash256> = dirty
            .iter()
            .filter(|k| !is_singleton_key(k, seq) && matches!(ffi_overlay.get(&k.0), Some(Some(_))))
            .copied()
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        for k in keys {
            let Some(Some(fb)) = ffi_overlay.get(&k.0) else { continue };
            let Some(mut v) = self.state.state_map.lookup(&k).and_then(|b| serde_json::from_slice::<Value>(b).ok())
            else {
                continue;
            };
            if v["LedgerEntryType"].as_str() != Some("AccountRoot") {
                continue;
            }
            let mut canon = v.clone();
            canon_for_encode(&mut canon);
            if xrpl_core::codec::encode::encode_transaction_json(&canon, false).ok().as_ref() != Some(fb) {
                continue;
            }
            let Some(drops) = v["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()) else { continue };
            v["Balance"] = Value::String((drops + 1).to_string());
            let Ok(bytes) = serde_json::to_vec(&v) else { continue };
            note_map_op(self.state.state_map.insert(k, bytes));
            return Some(k);
        }
        None
    }

    /// Full-scan `state.rocks` into the in-RAM mirror, decoding every binary
    /// SLE to the engine's JSON form. Blocks the sync loop once (~minutes);
    /// ws-sync's hold-position machinery absorbs the stall and catches up.
    /// `as_of` is the ledger the DB currently represents (last_synced).
    pub fn hydrate(&mut self, db: &std::sync::Arc<rocksdb::DB>, as_of: u32) {
        if self.hydrating.is_some() {
            return;
        }
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
        self.hydrated = false;
        self.pending.clear();
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
        let audit = std::env::var("XRPL_SHADOW_HYDRATE_AUDIT").map(|v| v != "0").unwrap_or(true);
        let snap = crate::ffi_engine::OwnedSnapshot::new(db.clone());
        let header = self.state.header.clone();
        self.last_hydrate = Some(std::time::Instant::now());
        eprintln!("[native-shadow] hydrating in the background from a snapshot at #{as_of} — the sync loop keeps flowing");
        match std::thread::Builder::new()
            .name("shadow-hydrate".into())
            .spawn(move || build_mirror(snap, header, as_of, audit))
        {
            Ok(h) => self.hydrating = Some(h),
            Err(e) => eprintln!("[native-shadow] hydrate thread failed to spawn: {e}"),
        }
    }

    /// Whether the shadow is live OR building: the sync loop's catch-up
    /// budget must treat both as "keep every ledger's overlay" — a skipped
    /// overlay during the build is a gap the moment the mirror lands.
    pub fn is_active(&self) -> bool {
        self.hydrated || self.hydrating.is_some()
    }

    /// Phase B: the breaker turned the writer off; the mirror has no further use.
    pub fn writer_tripped(&self) -> bool {
        self.writer_tripped
    }

    /// Land a finished build: install the mirror and replay everything the
    /// loop processed meanwhile, in order. Cheap when nothing is in flight.
    fn poll_hydrate(&mut self) {
        let finished = self.hydrating.as_ref().is_some_and(|h| h.is_finished());
        if !finished {
            return;
        }
        let Some(h) = self.hydrating.take() else { return };
        match h.join() {
            Ok(Some(out)) => self.install_hydrate(out),
            Ok(None) => {
                eprintln!("[native-shadow] hydrate build produced no mirror — will retry after the cooldown");
                self.pending.clear();
            }
            Err(_) => {
                eprintln!("[native-shadow] hydrate thread PANICKED — mirror not installed; retry after the cooldown");
                self.pending.clear();
            }
        }
        if !self.hydrated {
            return;
        }
        let mut queued = std::mem::take(&mut self.pending);
        queued.sort_by_key(|p| p.seq());
        let n = queued.len();
        for p in queued {
            if p.seq() <= self.at_seq {
                continue;
            }
            match p {
                PendingLedger::Shadow { seq, parent_hash, parent_close_time, total_drops, txs, overlay } => {
                    self.on_ledger(seq, &parent_hash, parent_close_time, total_drops, &txs, &overlay);
                }
                PendingLedger::Written { seq, written } => {
                    if self.at_seq + 1 != seq {
                        stats().skipped_gap.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[native-shadow] gap in the hydration backlog: mirror at #{}, next #{seq}", self.at_seq);
                        self.drop_mirror("gap");
                    } else {
                        self.reconcile(seq, HashMap::new(), &written, true);
                    }
                }
            }
            if !self.hydrated {
                break; // a gap dropped the mirror mid-replay
            }
        }
        if n > 0 {
            eprintln!("[native-shadow] replayed {n} ledgers buffered during hydration — mirror at #{}", self.at_seq);
        }
    }

    fn install_hydrate(&mut self, out: HydrateOutcome) {
        let HydrateOutcome { state, at_seq, objects: n, undecodable: bad, reencode_bad: bad_rt, ms, bad_keys, bad_overflow } =
            out;
        self.bad_keys = bad_keys;
        self.bad_overflow = bad_overflow;
        let as_of = at_seq;
        self.state = state;
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
        self.poll_hydrate();
        if !self.hydrated {
            if self.hydrating.is_some() {
                if self.pending.len() >= PENDING_CAP {
                    eprintln!("[native-shadow] {PENDING_CAP} ledgers queued behind the hydrate — dropping the attempt");
                    self.drop_mirror("hydrate backlog");
                    return;
                }
                self.pending.push(PendingLedger::Shadow {
                    seq,
                    parent_hash: *parent_hash,
                    parent_close_time,
                    total_drops,
                    txs: txs.to_vec(),
                    overlay: ffi_overlay.clone(),
                });
            }
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
        // An EMPTY FFI overlay under a real transaction is no trajectory: the
        // FFI leg did not verify this ledger (the era sentinel's skip returns
        // an empty overlay; every applied transaction writes at least its fee
        // payer). Compared against, every native write reads as "extra";
        // reconciled to, the ledger vanishes from the mirror and every later
        // ledger diverges (2026-09-29, #107314806, after a false era skip).
        // Treat it as a gap: drop the mirror, the caller re-hydrates from
        // state.rocks, and no receipt is written.
        let real_tx = txs
            .iter()
            .any(|t| !PSEUDO_TX_TYPES.contains(&t["TransactionType"].as_str().unwrap_or("")));
        if ffi_overlay.is_empty() && real_tx {
            st.skipped_unverified.fetch_add(1, Ordering::Relaxed);
            eprintln!("[native-shadow] #{seq}: the FFI leg did not verify this ledger (empty overlay) — will re-hydrate");
            self.drop_mirror("unverified ledger");
            return;
        }
        let t0 = std::time::Instant::now();
        let ordered = apply_order(txs);
        let applied = self.apply_ledger(seq, parent_hash, parent_close_time, total_drops, &ordered);
        self.compare_and_report(seq, &ordered, &applied, ffi_overlay, t0);
        self.reconcile(seq, applied.undo, ffi_overlay, false);
    }

    /// Apply ledger `seq`'s transactions (in apply order) to the mirror.
    fn apply_ledger(
        &mut self,
        seq: u32,
        parent_hash: &[u8; 32],
        parent_close_time: u32,
        total_drops: u64,
        ordered: &[&Value],
    ) -> Applied {
        let st = stats();
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

        // Batch (BatchV1_1): the ledger records every inner transaction as its
        // own entry (own hash, own meta carrying ParentBatchID, the indices
        // right after its outer) but rippled APPLIES it inside the outer's
        // application — and so does BatchTransactor::do_apply. The inner
        // entries are therefore skipped in the replay below and their metadata
        // is folded into the outer's, which is where our overlay reports them.
        let attribution = crate::native_apply::batch_attribution(ordered);
        let by_hash: HashMap<String, &Value> = ordered
            .iter()
            .map(|t| (t["hash"].as_str().unwrap_or("").to_uppercase(), *t))
            .collect();

        // Flag-ledger NegativeUNL rotation (ledger-level, outside tx metas).
        let mut dirty: HashSet<Hash256> = HashSet::new();
        let mut undo: HashMap<Hash256, Option<Vec<u8>>> = HashMap::new();
        if seq % 256 == 0 {
            let nk = keylet::negative_unl_key();
            if let Some(bytes) = self.state.state_map.lookup(&nk).map(|b| b.to_vec()) {
                match xrpl_ledger::tx::pseudo::rotate_negative_unl(&bytes, seq) {
                    Some(Some(nb)) => {
                        undo.entry(nk).or_insert_with(|| Some(bytes.clone()));
                        note_map_op(self.state.state_map.insert(nk, nb));
                        dirty.insert(nk);
                    }
                    Some(None) => {
                        undo.entry(nk).or_insert_with(|| Some(bytes.clone()));
                        note_map_op(self.state.state_map.delete(&nk));
                        dirty.insert(nk);
                    }
                    None => {}
                }
            }
        }

        let mut ter_mm: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        for tx in ordered {
            let this_hash = tx["hash"].as_str().unwrap_or("").to_uppercase();
            if attribution.skip.contains(&this_hash) {
                continue; // applied inside its outer Batch
            }
            let Some(txf) = build_txfields(tx) else {
                skipped += 1;
                continue;
            };
            let expected_ter = tx["metaData"]["TransactionResult"].as_str().unwrap_or("?");
            let tx_hash = tx["hash"].as_str().unwrap_or("").to_string();
            let (our_ter, mut mods) = native_apply_one(&self.state, &txf);
            // Threading stamps (PreviousTxnID/PreviousTxnLgrSeq) — the replay
            // applies them after every tx; first live ledger without them read
            // 175 byte-diffs at zero TER mismatches (#106628655).
            //
            // A Batch is threaded per inner: rippled applies each inner as
            // its own transaction, so the objects an inner touched carry the
            // INNER's hash and only the outer's own changes (its fee and
            // sequence) carry the outer's.
            //
            // The ids are RECOMPUTED from the outer's RawTransactions, not
            // taken from `attribution.inners_of`: ParentBatchID names only the
            // inners the ledger FILED, and an inner that failed, or that a
            // mode never reached, has no entry at all. The engine reports one
            // result and one touched set per inner it ATTEMPTED, so pairing
            // those against the filed subset shifts every later inner onto its
            // neighbour's. An inner with an empty touched set stamps nothing.
            let is_batch = tx["TransactionType"].as_str() == Some("Batch");
            let inner_ids: Vec<String> =
                if is_batch { crate::native_apply::batch_inner_ids(tx) } else { Vec::new() };
            // Drain the touched sets on EVERY Batch outer, for the same
            // staleness reason the per-inner results below are drained
            // unconditionally — the cell is cleared at do_apply entry, so an
            // outer that never got there would otherwise leave the PREVIOUS
            // batch's sets for the next reader.
            let mut inner_touched =
                if is_batch { xrpl_ledger::tx::batch::take_inner_touched() } else { Vec::new() };
            // One set per attempted inner, one id per inner: pad so the two
            // line up. A vector LONGER than the ids means an id could not be
            // recomputed — stamp_batch_threading's own guard then stamps the
            // outer alone, and the TER guard below withholds the verdicts.
            if inner_touched.len() < inner_ids.len() {
                inner_touched.resize(inner_ids.len(), Vec::new());
            }
            if inner_ids.is_empty() {
                xrpl_ledger::ledger::threading::stamp_threading(
                    &mut mods,
                    &|k| self.state.state_map.lookup(k).map(|b| b.to_vec()),
                    &tx_hash,
                    seq,
                );
            } else {
                xrpl_ledger::ledger::threading::stamp_batch_threading(
                    &mut mods,
                    &|k| self.state.state_map.lookup(k).map(|b| b.to_vec()),
                    &tx_hash,
                    seq,
                    &inner_ids,
                    &inner_touched,
                );
            }
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
                let mut node_sources: Vec<&Value> = vec![*tx];
                // The same recomputed id list: the ones the ledger filed have
                // an entry here (in RawTransactions order, which for the filed
                // subset is TransactionIndex order), the rest have none.
                for ih in &inner_ids {
                    if let Some(it) = by_hash.get(ih) {
                        node_sources.push(*it);
                    }
                }
                // A Batch outer's expected mutation set is the union of its own
                // and its inners' AffectedNodes, folded per key with the FFI
                // leg's rules (ffi_engine.rs:2300-2312): Created wins over
                // Modified, Created-then-Deleted is no entry at all, and the
                // LAST occurrence's FinalFields is the post-batch image. For a
                // non-Batch tx node_sources is just [tx] and the fold is the
                // identity over its own AffectedNodes, in meta order.
                let mut node_order: Vec<&str> = Vec::new();
                let mut folded: HashMap<&str, (bool, bool, &Value)> = HashMap::new();
                for src in &node_sources {
                    for node in src["metaData"]["AffectedNodes"].as_array().into_iter().flatten() {
                        let Some((kind, body)) = node.as_object().and_then(|o| o.iter().next()) else { continue };
                        let Some(li) = body["LedgerIndex"].as_str() else { continue };
                        let e = folded.entry(li).or_insert_with(|| {
                            node_order.push(li);
                            (false, false, node)
                        });
                        e.0 |= kind == "CreatedNode";
                        e.1 |= kind == "DeletedNode";
                    }
                }
                for key in &node_order {
                    let Some(&(created, deleted, node)) = folded.get(key) else { continue };
                    // Only the first two rules bite in THIS body: created —
                    // alone, or created-then-deleted inside the batch — means
                    // the entry has no pre-ledger image, so the audit below has
                    // nothing to check, as for a plain deletion. The third
                    // (last-occurrence wins) is about FinalFields; this body
                    // reads PreviousFields, whose pre-LEDGER image is the FIRST
                    // toucher's — the rule pre_stale_audit already states — so
                    // the node retained per key is the first. Keeping the last
                    // would compare a MID-batch image against the mirror and
                    // false-STALE every key an inner touched after its outer.
                    if created || deleted {
                        continue;
                    }
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
                        // Object-valued fields (amounts) compare value+currency —
                        // the mirror dialect spells issuers as hex where the meta
                        // has r-addresses; strict equality branded every such
                        // field STALE with byte-identical values on both sides
                        // (2026-08-31 evening: 4 path ter-mms mislabeled
                        // input-caused when they were engine disagreements).
                        // Arrays get the dialect aligned the same way
                        // (#106678645's VoteSlots).
                        let mut want_hex = want.clone();
                        crate::native_apply::hexify_addresses(&mut want_hex);
                        let eq = match (have, &want_hex) {
                            (Some(h), w) if h.is_object() && w.is_object() => {
                                h.get("value") == w.get("value")
                                    && h.get("currency") == w.get("currency")
                            }
                            (h, w) => h == Some(w),
                        };
                        if !eq {
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
            // The inners ran inside our do_apply; their per-inner results come
            // back through the ledger crate's thread-local. Drain it on EVERY
            // Batch outer — the cell is cleared at do_apply entry, so an outer
            // that never got there (preflight/preclaim failure) would otherwise
            // leave the PREVIOUS batch's results for the next reader.
            let inner_results = if tx["TransactionType"].as_str() == Some("Batch") {
                xrpl_ledger::tx::batch::take_inner_results()
            } else {
                Vec::new()
            };
            if !inner_ids.is_empty() {
                let tripped = crate::native_apply::inner_id_tripwire(
                    &inner_ids,
                    attribution.inners_of.get(&this_hash).map(Vec::as_slice),
                );
                if let Some(why) = tripped {
                    st.batch_inner_ter_mm.fetch_add(1, Ordering::Relaxed);
                    ter_mm.push(format!("{this_hash} BATCH-INNER: {why} — inner verdicts withheld"));
                } else if inner_results.len() > inner_ids.len() {
                    // One result per ATTEMPTED inner can never outnumber the
                    // inners themselves: more results than ids means an id
                    // could not be recomputed and no pairing is trustworthy.
                    // Withhold the verdicts, but say so and count it — a
                    // withheld comparison must not read as agreement.
                    st.batch_inner_ter_mm.fetch_add(1, Ordering::Relaxed);
                    ter_mm.push(format!(
                        "{this_hash} BATCH-INNER: {} inner ids vs {} results — inner verdicts withheld",
                        inner_ids.len(),
                        inner_results.len()
                    ));
                } else {
                    // Paired BY ID against what the ledger FILED: an inner
                    // with no entry was not applied, and any tes or tec of
                    // ours for it is the disagreement. Filed for THIS outer:
                    // its ParentBatchID, never the id alone — several outers
                    // can carry one inner (`filed_inner_verdicts`).
                    let filed = crate::native_apply::filed_inner_verdicts(
                        &inner_ids,
                        attribution.inners_of.get(&this_hash).map(Vec::as_slice),
                        &by_hash,
                    );
                    for (i, ih, want, mismatch) in crate::native_apply::pair_inner_verdicts(
                        &inner_ids,
                        &inner_results,
                        &filed,
                        crate::native_apply::batch_all_or_nothing(tx),
                    ) {
                        if !mismatch {
                            continue;
                        }
                        let got = inner_results
                            .get(i)
                            .map(String::as_str)
                            .unwrap_or(crate::native_apply::INNER_NOT_ATTEMPTED);
                        // Its own counter, never `ter_mismatched`: that pair
                        // counts LEDGER ENTRIES applied (one per outer), so an
                        // inner must not move it.
                        st.batch_inner_ter_mm.fetch_add(1, Ordering::Relaxed);
                        ter_mm.push(format!(
                            "{this_hash} BATCH-INNER[{i}] {ih}: our_ter={got} net_ter={want}"
                        ));
                    }
                }
            }
            for (k, ent) in mods {
                undo.entry(k).or_insert_with(|| self.state.state_map.lookup(&k).map(|b| b.to_vec()));
                match ent {
                    SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                        note_map_op(self.state.state_map.insert(k, b));
                    }
                    SandboxEntry::Deleted => {
                        note_map_op(self.state.state_map.delete(&k));
                    }
                }
                dirty.insert(k);
            }
        }
        // The skip list's pre-images, so an undo restores them too (update_skip_list
        // writes the map and `dirty` but keeps no undo). Phase A's reconcile skips
        // singleton undo entries, so this changes nothing there.
        let mut skip_keys = vec![keylet::skip_list_key()];
        if (seq - 1) & 0xff == 0 {
            skip_keys.push(crate::native_apply::skip_every_key(seq - 1));
        }
        for k in skip_keys {
            undo.entry(k).or_insert_with(|| self.state.state_map.lookup(&k).map(|b| b.to_vec()));
        }
        update_skip_list(&mut self.state, &mut dirty, seq, &parent_hash_hex);
        Applied { dirty, undo, ter_mm, skipped }
    }

    /// Compare the applied ledger against the FFI overlay: receipts, counters
    /// and the canary. The mirror keeps the native writes (plus a planted
    /// canary) until the reconcile.
    fn compare_and_report(
        &mut self,
        seq: u32,
        ordered: &[&Value],
        applied: &Applied,
        ffi_overlay: &LedgerOverlay,
        t0: std::time::Instant,
    ) {
        let st = stats();
        let (dirty, undo, ter_mm) = (&applied.dirty, &applied.undo, &applied.ter_mm);
        let canary = self.arm_canary(dirty, ffi_overlay, seq);

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
                                ordered,
                                &hex::encode_upper(k),
                            );
                            byte_diff.push(format!("{}:@{off} ours-len {} ffi-len {}{audit}", hex::encode_upper(k), ob.len(), fb.len()));
                        }
                        None => {
                            // Self-documenting: the error and OUR stored JSON,
                            // captured before the reconcile erases them —
                            // #106692562 B580FF73 died unexplained for want of
                            // this (bundle replays could not starve the same
                            // walk into the same write).
                            let err = serde_json::from_slice::<Value>(&jb)
                                .ok()
                                .and_then(|mut v| {
                                    canon_for_encode(&mut v);
                                    xrpl_core::codec::encode::encode_transaction_json(&v, false)
                                        .err()
                                        .map(|e| format!("{e:?}"))
                                })
                                .unwrap_or_else(|| "jb-not-json".to_string());
                            let js: String =
                                String::from_utf8_lossy(&jb).chars().take(700).collect();
                            eprintln!(
                                "[native-shadow] ENCODE-ERR #{seq} {} {err} jb={js}",
                                hex::encode_upper(k)
                            );
                            byte_diff.push(format!(
                                "{}:encode-err {err}",
                                hex::encode_upper(k)
                            ));
                        }
                    }
                }
            }
        }
        for k in dirty {
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
        // The canary's own diff leaves the tallies here: `clean` and every
        // counter below describe the engine alone.
        let canary_hit = canary.map(|k| {
            let key = hex::encode_upper(k.0);
            let diff = byte_diff.iter().position(|d| d.starts_with(&key)).map(|i| byte_diff.remove(i));
            (key, diff)
        });

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
            // Finding 241's lesson, made permanent: a ter mismatch on an
            // otherwise clean ledger used to be counted and then thrown away.
            // The state overlay cannot see a wrong result code — a bad tec
            // charges the same fee and writes the same bytes — so 292 CheckCash
            // disagreements rode through cycle 108's gate and only the live
            // ter-miss counter noticed. Emit them, marked so the soak receipt
            // tally can filter them out (`ter_only`).
            if !ter_mm.is_empty() {
                if let Some(f) = &mut self.log {
                    let _ = writeln!(
                        f,
                        "{}",
                        json!({
                            "seq": seq,
                            "ter_only": true,
                            "missing": [],
                            "extra": [],
                            "byte_diff": [],
                            "ter_mismatch": ter_mm,
                            "noop_missing": noop_missing,
                            "noop_extra": noop_extra,
                        })
                    );
                }
            }
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
        // Its own line, never folded into a receipt: every reader of the
        // receipt log skips `canary` lines when it counts receipts.
        if let Some((key, diff)) = canary_hit {
            let detected = diff.is_some();
            st.canary_fired.fetch_add(1, Ordering::Relaxed);
            if detected {
                st.canary_detected.fetch_add(1, Ordering::Relaxed);
            }
            eprintln!("[native-shadow] #{seq} CANARY planted on {key}: detected={detected}");
            let wrote = self.log.as_mut().is_some_and(|f| {
                writeln!(f, "{}", json!({ "seq": seq, "canary": { "key": key, "detected": detected, "diff": diff } }))
                    .is_ok()
            });
            // Disarm only once the line is down; a failed write leaves the
            // trigger armed and the next ledger plants again.
            if wrote {
                if let Some(p) = &self.canary {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        st.apply_ms_last.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Put the mirror back on the canonical trajectory and advance it to `seq`.
    /// Every native write reverts to its pre-ledger value, then `canonical`'s
    /// bytes (decoded to engine JSON) are applied. Phase A (`singletons_too`
    /// false): singleton/skip-list writes stay native — the replay proved them
    /// and the FFI overlay never carries them. Phase B (true): `canonical` is
    /// everything ws-sync wrote, singletons included, so the mirror ends equal
    /// to state.rocks.
    fn reconcile(
        &mut self,
        seq: u32,
        undo: HashMap<Hash256, Option<Vec<u8>>>,
        canonical: &LedgerOverlay,
        singletons_too: bool,
    ) {
        let st = stats();
        for (k, old) in undo {
            if !singletons_too && is_singleton_key(&k, seq) {
                continue;
            }
            match old {
                Some(b) => {
                    note_map_op(self.state.state_map.insert(k, b));
                }
                None => {
                    note_map_op(self.state.state_map.delete(&k));
                }
            }
        }
        for (k, ffi_val) in canonical {
            let kh = Hash256(*k);
            if !singletons_too && is_singleton_key(&kh, seq) {
                continue;
            }
            match ffi_val {
                Some(fb) => match xrpl_core::codec::decode::decode_transaction_binary(fb) {
                    Ok(mut jv) => {
                        crate::native_apply::hexify_addresses(&mut jv);
                        note_map_op(
                            self.state
                                .state_map
                                .insert(kh, serde_json::to_vec(&jv).unwrap_or_default()),
                        );
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
                    note_map_op(self.state.state_map.delete(&kh));
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
        for (k, ffi_val) in canonical {
            let kh = Hash256(*k);
            if !singletons_too && is_singleton_key(&kh, seq) {
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
                        note_map_op(
                            self.state
                                .state_map
                                .insert(kh, serde_json::to_vec(&jv).unwrap_or_default()),
                        );
                    }
                    if let Some(jb2) = self.state.state_map.lookup(&kh).map(|b| b.to_vec()) {
                        retry_fixed = matches!(reencode(&jb2), Ok(b) if &b == fb);
                        if !retry_fixed {
                            jb_retry = Some(String::from_utf8_lossy(&jb2).into_owned());
                        }
                    }
                }
                None => {
                    note_map_op(self.state.state_map.delete(&kh));
                    retry_fixed = self.state.state_map.lookup(&kh).is_none();
                }
            }
            if retry_fixed {
                st.leak_retry_fixed.fetch_add(1, Ordering::Relaxed);
            } else if self.writer {
                // Phase B: the mirror cannot hold this entry right; never vouch for writing it.
                if self.bad_keys.len() < BAD_KEYS_CAP {
                    self.bad_keys.insert(*k);
                } else {
                    self.bad_overflow = true;
                }
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

    // ---- Stage 4 Phase B: the native writer ---------------------------------

    /// Apply ledger `seq` and offer its writes for state.rocks. None: no mirror
    /// to apply on (still hydrating, a gap, or the retry of a ledger our bytes
    /// failed the state hash on); ws-sync writes the network's bytes and the
    /// commit brings the mirror along. `meta_keys` is every object the
    /// metadata names (modified, created or deleted), `meta_deleted` the
    /// deleted ones; `ffi_overlay`, when the FFI leg verified this ledger,
    /// keeps the overlay-vs-overlay compare (receipts, canary) running.
    /// Every Some must be settled with [`commit`](Self::commit) or
    /// [`abort`](Self::abort) before the next call.
    pub fn propose(
        &mut self,
        seq: u32,
        parent_hash: &[u8; 32],
        parent_close_time: u32,
        total_drops: u64,
        txs: &[Value],
        meta_keys: &HashSet<[u8; 32]>,
        meta_deleted: &HashSet<[u8; 32]>,
        ffi_overlay: Option<&LedgerOverlay>,
    ) -> Option<Proposal> {
        let st = stats();
        self.poll_hydrate();
        if let Some(h) = self.held.take() {
            eprintln!("[native-writer] #{}: proposal never settled — undone", h.seq);
            self.revert(h.undo);
        }
        if !self.hydrated {
            st.writer_unready.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if self.at_seq + 1 != seq {
            st.skipped_gap.fetch_add(1, Ordering::Relaxed);
            st.writer_unready.fetch_add(1, Ordering::Relaxed);
            eprintln!("[native-shadow] gap: mirror at #{}, asked for #{seq} — will re-hydrate", self.at_seq);
            self.drop_mirror("gap");
            return None;
        }
        if self.distrust == Some(seq) {
            st.writer_distrusted.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if self.writer_tripped {
            st.writer_unready.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let t0 = std::time::Instant::now();
        let ordered = apply_order(txs);
        let applied = self.apply_ledger(seq, parent_hash, parent_close_time, total_drops, &ordered);
        // Taken BEFORE the compare: a planted canary must never be written.
        let (overlay, refusal) = self.vouch(seq, &applied, meta_keys, meta_deleted);
        let real_tx = txs
            .iter()
            .any(|t| !PSEUDO_TX_TYPES.contains(&t["TransactionType"].as_str().unwrap_or("")));
        match ffi_overlay {
            // An empty overlay under real transactions: the FFI leg did not
            // verify this ledger. Nothing to compare against; the mirror is
            // unaffected, it follows what gets written.
            Some(ffi) if !(ffi.is_empty() && real_tx) => self.compare_and_report(seq, &ordered, &applied, ffi, t0),
            _ => st.apply_ms_last.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed),
        }
        if let Some(why) = &refusal {
            st.writer_refused.fetch_add(1, Ordering::Relaxed);
            self.writer_note(json!({ "seq": seq, "refused": why }));
        }
        self.held = Some(Held { seq, undo: applied.undo });
        Some(Proposal { overlay, refusal })
    }

    /// Our writes for the ledger, and the reason we cannot vouch for them, if
    /// any: a result that disagrees with the metadata, an entry that will not
    /// encode, or a key set that is not the metadata's exactly (a created or
    /// modified object must be present, a deleted one absent).
    fn vouch(
        &self,
        seq: u32,
        applied: &Applied,
        meta_keys: &HashSet<[u8; 32]>,
        meta_deleted: &HashSet<[u8; 32]>,
    ) -> (LedgerOverlay, Option<String>) {
        let mut overlay = LedgerOverlay::new();
        let mut refusal = applied.ter_mm.first().map(|m| format!("result {}", m.chars().take(160).collect::<String>()));
        if applied.skipped > 0 {
            refusal.get_or_insert_with(|| format!("{} transaction(s) the engine could not parse", applied.skipped));
        }
        if self.bad_overflow {
            refusal.get_or_insert_with(|| format!("the mirror holds over {BAD_KEYS_CAP} entries it could not load cleanly"));
        }
        for k in &applied.dirty {
            if is_singleton_key(k, seq) {
                continue;
            }
            let pre = applied.undo.get(k).and_then(|o| o.as_deref());
            let post = self.state.state_map.lookup(k);
            if pre == post {
                continue; // wrote back the pre-state: the network elides it too
            }
            // Different engine JSON, same canonical bytes: still no change.
            if let (Some(a), Some(b)) = (pre, post) {
                if matches!((encode_entry(a), encode_entry(b)), (Ok(x), Ok(y)) if x == y) {
                    continue;
                }
            }
            match post {
                None => {
                    overlay.insert(k.0, None);
                }
                Some(jb) => match encode_entry(jb) {
                    Ok(b) => {
                        overlay.insert(k.0, Some(b));
                    }
                    Err(e) => {
                        refusal.get_or_insert_with(|| format!("encode {}: {e}", hex::encode_upper(k.0)));
                    }
                },
            }
        }
        // Created and deleted inside the ledger: the metadata files it as
        // deleted and our write nets to nothing. Agreed — delete it.
        for k in meta_deleted {
            if !is_singleton_key(&Hash256(*k), seq)
                && !overlay.contains_key(k)
                && self.state.state_map.lookup(&Hash256(*k)).is_none()
            {
                overlay.insert(*k, None);
            }
        }
        if refusal.is_none() {
            if let Some(k) = overlay.keys().chain(meta_keys.iter()).find(|k| self.bad_keys.contains(*k)) {
                refusal = Some(format!("{} is an entry the mirror could not load cleanly", hex::encode_upper(k)));
            }
        }
        if refusal.is_none() {
            // ws-sync names the singletons by its own rule (the group key from seq / 65536); leave
            // out both its set and ours, so the two rules never disagree into a false refusal.
            let ws_singletons: HashSet<[u8; 32]> = crate::stage3::singleton_keys_for(seq)
                .iter()
                .filter_map(|h| hex::decode(h).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()))
                .collect();
            let theirs: HashSet<[u8; 32]> = meta_keys
                .iter()
                .filter(|k| !is_singleton_key(&Hash256(**k), seq) && !ws_singletons.contains(*k))
                .copied()
                .collect();
            let ours: HashSet<[u8; 32]> = overlay.keys().copied().collect();
            let only_ours: Vec<&[u8; 32]> = ours.difference(&theirs).collect();
            let only_theirs: Vec<&[u8; 32]> = theirs.difference(&ours).collect();
            let wrong_kind: Vec<&[u8; 32]> = overlay
                .iter()
                .filter(|(k, v)| meta_deleted.contains(*k) == v.is_some())
                .map(|(k, _)| k)
                .collect();
            if !only_ours.is_empty() || !only_theirs.is_empty() || !wrong_kind.is_empty() {
                let first = only_ours.first().or(only_theirs.first()).or(wrong_kind.first()).map(|k| hex::encode_upper(k));
                refusal = Some(format!(
                    "keys: {} ours only, {} network only, {} present/deleted the wrong way (first {})",
                    only_ours.len(),
                    only_theirs.len(),
                    wrong_kind.len(),
                    first.unwrap_or_default()
                ));
            }
        }
        (overlay, refusal)
    }

    /// The ledger landed: `written` is every key ws-sync put (Some) or deleted
    /// (None), verified by the state hash. The mirror drops its own writes for
    /// these and takes the written bytes, so it stays equal to state.rocks.
    /// `ours`: the bytes were our proposal's.
    pub fn commit(&mut self, seq: u32, written: &LedgerOverlay, ours: bool) {
        let st = stats();
        if ours {
            st.writer_native.fetch_add(1, Ordering::Relaxed);
        }
        self.poll_hydrate();
        if !self.hydrated {
            self.held = None;
            if self.distrust == Some(seq) {
                self.distrust = None;
            }
            if self.hydrating.is_some() {
                if self.pending.len() >= PENDING_CAP {
                    eprintln!("[native-shadow] {PENDING_CAP} ledgers queued behind the hydrate — dropping the attempt");
                    self.drop_mirror("hydrate backlog");
                    return;
                }
                self.pending.push(PendingLedger::Written { seq, written: written.clone() });
            }
            return;
        }
        let undo = match self.held.take() {
            Some(h) if h.seq == seq => h.undo,
            Some(h) => {
                eprintln!("[native-writer] #{seq} committed over an unsettled proposal for #{} — undone", h.seq);
                self.revert(h.undo);
                HashMap::new()
            }
            None => HashMap::new(),
        };
        if self.at_seq + 1 != seq {
            st.skipped_gap.fetch_add(1, Ordering::Relaxed);
            eprintln!("[native-shadow] gap: mirror at #{}, #{seq} written — will re-hydrate", self.at_seq);
            self.drop_mirror("gap");
            return;
        }
        self.reconcile(seq, undo, written, true);
        if self.distrust == Some(seq) {
            self.distrust = None;
        }
    }

    /// Nothing landed for `seq` (the write was held back or rolled back):
    /// undo our writes so the mirror is the parent ledger again.
    /// `mismatch_on_ours`: our bytes were written and failed the state hash.
    /// The retry is written from the network's bytes; the mirror is dropped
    /// (a stale entry the engine only READ would repeat the failure) and
    /// re-hydrates after the cooldown; BREAKER_FAILS of these within
    /// BREAKER_WINDOW ledgers turn the writer off until the next restart.
    pub fn abort(&mut self, seq: u32, mismatch_on_ours: bool) {
        if let Some(h) = self.held.take() {
            self.revert(h.undo);
        }
        if mismatch_on_ours {
            let st = stats();
            st.writer_mismatch.fetch_add(1, Ordering::Relaxed);
            self.distrust = Some(seq);
            self.writer_fails.retain(|s| seq.saturating_sub(*s) < BREAKER_WINDOW);
            self.writer_fails.push(seq);
            let tripped = self.writer_fails.len() >= BREAKER_FAILS && !self.writer_tripped;
            eprintln!("[native-writer] #{seq}: our bytes failed the state hash — rolled back; the retry uses the network's");
            self.writer_note(json!({ "seq": seq, "mismatch": true, "recent": self.writer_fails }));
            if tripped {
                self.writer_tripped = true;
                st.writer_breaker.store(1, Ordering::Relaxed);
                eprintln!(
                    "[native-writer] BREAKER: {} hash failures on our bytes within {BREAKER_WINDOW} ledgers — writer OFF until restart; state comes from the network",
                    self.writer_fails.len()
                );
                self.writer_note(json!({ "seq": seq, "breaker": true, "fails": self.writer_fails }));
            }
            self.drop_mirror("hash failure on our bytes");
        }
    }

    /// Restore pre-images, singletons included.
    fn revert(&mut self, undo: HashMap<Hash256, Option<Vec<u8>>>) {
        for (k, old) in undo {
            match old {
                Some(b) => note_map_op(self.state.state_map.insert(k, b)),
                None => note_map_op(self.state.state_map.delete(&k)),
            }
        }
    }
}

/// The hydrate scan, on its own thread: every binary SLE in the snapshot
/// decoded to the engine's JSON form (and, with the audit on, re-encoded
/// and compared byte-for-byte). Returns None when the snapshot held no
/// state to build from.
fn build_mirror(
    snap: crate::ffi_engine::OwnedSnapshot,
    header: LedgerHeader,
    as_of: u32,
    audit: bool,
) -> Option<HydrateOutcome> {
    let t0 = std::time::Instant::now();
    let mut state = LedgerState::new_unverified(header);
    let mut n = 0u64;
    let mut bad = 0u64;
    // Integrity audit: the scan plants ~20M decoded objects into the
    // mirror with nothing checking them — after the per-ledger reconcile
    // verifier landed, this is the one unguarded corruption channel left.
    // Re-encode each decoded object and demand the raw store bytes back;
    // failures are counted and the first few named. Costs roughly half
    // the scan time again; XRPL_SHADOW_HYDRATE_AUDIT=0 disables.
    let mut bad_rt = 0u64;
    // Phase B: which keys those were (the writer never vouches for writing one).
    let mut bad_keys: HashSet<[u8; 32]> = HashSet::new();
    let mut bad_overflow = false;
    let mut note_bad = |key: [u8; 32]| {
        if bad_keys.len() < BAD_KEYS_CAP {
            bad_keys.insert(key);
        } else {
            bad_overflow = true;
        }
    };
    // Trust the store's own stamp over the caller's belief: the writer
    // stamps last-written-seq inside each ledger's atomic batch, and the
    // sync loop is frozen while we scan, so the stamp IS the scan's
    // as-of. A missing stamp (pre-stamp store) falls back to the caller.
    let as_of = match snap.get(b"meta:last_seq")
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
    let iter = snap.iterator_opt(rocksdb::IteratorMode::Start, ro);
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
                        note_bad(key);
                        if bad_rt <= 5 {
                            eprintln!(
                                "[native-shadow] hydrate REENCODE-BAD {} ({} bytes)",
                                hex::encode_upper(key),
                                v.len()
                            );
                        }
                    }
                }
                note_map_op(
                    state.state_map.insert(Hash256(key), serde_json::to_vec(&jv).unwrap_or_default()),
                );
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
                note_bad(key);
            }
        }
    }
    let ms = t0.elapsed().as_millis() as u64;
    if n == 0 {
        return None;
    }
    Some(HydrateOutcome {
        state,
        at_seq: as_of,
        objects: n,
        undecodable: bad,
        reencode_bad: bad_rt,
        ms,
        bad_keys,
        bad_overflow,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real testnet ledger with one XRP payment (campaign 14, #20939579),
    /// fed to the shadow the way ws-sync feeds it: the mirror hydrated from
    /// the bundle's pre-images, the FFI overlay = the network's post bytes.
    fn payment_ledger() -> (u32, [u8; 32], u32, u64, Value, LedgerState, LedgerOverlay) {
        let b: Value = serde_json::from_str(include_str!(
            "../tests/vectors/deposit_c14_preauthorized_payment_473859FA8AF3.json"
        ))
        .unwrap();
        let seq = b["seq"].as_u64().unwrap() as u32;
        let parent: [u8; 32] = hex::decode(b["parent_hash"].as_str().unwrap()).unwrap().try_into().unwrap();
        let mut state = LedgerState::new_unverified(LedgerHeader {
            sequence: seq - 1,
            total_coins: 0,
            parent_hash: Hash256(parent),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 0,
            close_time_resolution: 10,
            close_flags: 0,
        });
        for (k, v) in b["pre"].as_object().unwrap() {
            let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&hex::decode(v.as_str().unwrap()).unwrap())
                .unwrap();
            crate::native_apply::hexify_addresses(&mut jv);
            let key: [u8; 32] = hex::decode(k).unwrap().try_into().unwrap();
            state.state_map.insert(Hash256(key), serde_json::to_vec(&jv).unwrap()).unwrap();
        }
        let mut tx = b["tx"].clone();
        tx["metaData"] = json!({ "TransactionResult": b["result"], "TransactionIndex": 0, "AffectedNodes": [] });
        let overlay: LedgerOverlay = b["expect"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let key: [u8; 32] = hex::decode(k).unwrap().try_into().unwrap();
                (key, v.as_str().map(|h| hex::decode(h).unwrap()))
            })
            .collect();
        let pct = b["parent_close_time"].as_u64().unwrap() as u32;
        (seq, parent, pct, b["total_coins"].as_u64().unwrap(), tx, state, overlay)
    }

    fn shadow(state: LedgerState, at_seq: u32, log: &std::path::Path, trigger: &std::path::Path) -> NativeShadow {
        NativeShadow {
            state,
            at_seq,
            hydrated: true,
            last_hydrate: None,
            log: Some(std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap()),
            hydrating: None,
            pending: Vec::new(),
            canary: Some(trigger.to_path_buf()),
            held: None,
            distrust: None,
            writer_log: None,
            writer: false,
            bad_keys: HashSet::new(),
            bad_overflow: false,
            writer_fails: Vec::new(),
            writer_tripped: false,
        }
    }

    // ---- Phase B: the native writer ----

    /// The keys the network's metadata names for the payment ledger: exactly
    /// the bundle's post-state keys (its `expect`), singletons aside.
    fn meta_of(overlay: &LedgerOverlay) -> (HashSet<[u8; 32]>, HashSet<[u8; 32]>) {
        let keys = overlay.keys().copied().collect();
        let deleted = overlay.iter().filter(|(_, v)| v.is_none()).map(|(k, _)| *k).collect();
        (keys, deleted)
    }

    fn reencoded(sh: &NativeShadow, k: &[u8; 32]) -> Option<Vec<u8>> {
        sh.state.state_map.lookup(&Hash256(*k)).map(|jb| encode_entry(jb).unwrap())
    }

    #[test]
    fn the_writer_vouches_for_a_ledger_it_matches_and_offers_the_networks_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let p = sh.propose(seq, &parent, pct, drops, &[tx], &keys, &deleted, Some(&overlay)).unwrap();
        assert_eq!(p.refusal, None, "every result and every key agrees");
        assert_eq!(p.overlay, overlay, "our writes are the network's post-state bytes, byte for byte");
        sh.commit(seq, &p.overlay, true);
        assert_eq!(sh.at_seq, seq);
        for (k, v) in &overlay {
            assert_eq!(&reencoded(&sh, k), v, "the mirror holds what was written");
        }
        assert!(lines(&log).is_empty(), "a clean ledger leaves no receipt");
    }

    #[test]
    fn the_writer_refuses_a_key_set_the_metadata_does_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (mut keys, deleted) = meta_of(&overlay);
        let dropped = *keys.iter().min().unwrap();
        keys.remove(&dropped);
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let p = sh.propose(seq, &parent, pct, drops, &[tx], &keys, &deleted, None).unwrap();
        let why = p.refusal.expect("one object the network never touched: no vouch");
        assert!(why.starts_with("keys: 1 ours only"), "{why}");
        // ws-sync writes the network's bytes instead; the mirror follows them.
        sh.commit(seq, &overlay, false);
        assert_eq!(sh.at_seq, seq);
        for (k, v) in &overlay {
            assert_eq!(&reencoded(&sh, k), v);
        }
    }

    #[test]
    fn the_writer_refuses_a_result_the_metadata_disagrees_with() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, mut tx, state, overlay) = payment_ledger();
        tx["metaData"]["TransactionResult"] = json!("tecUNFUNDED_PAYMENT");
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let p = sh.propose(seq, &parent, pct, drops, &[tx], &keys, &deleted, None).unwrap();
        let why = p.refusal.expect("a result that disagrees: no vouch");
        assert!(why.starts_with("result ") && why.contains("tecUNFUNDED_PAYMENT"), "{why}");
    }

    #[test]
    fn an_abort_restores_the_parent_skip_list_included() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let mut watched: Vec<Hash256> = overlay.keys().map(|k| Hash256(*k)).collect();
        watched.push(keylet::skip_list_key());
        let before: Vec<Option<Vec<u8>>> = watched.iter().map(|k| sh.state.state_map.lookup(k).map(|b| b.to_vec())).collect();
        let txs = [tx];
        sh.propose(seq, &parent, pct, drops, &txs, &keys, &deleted, None).unwrap();
        sh.abort(seq, false);
        assert_eq!(sh.at_seq, seq - 1, "nothing landed: still the parent");
        let after: Vec<Option<Vec<u8>>> = watched.iter().map(|k| sh.state.state_map.lookup(k).map(|b| b.to_vec())).collect();
        assert_eq!(after, before, "every pre-image restored, the skip list's too");
        let p = sh.propose(seq, &parent, pct, drops, &txs, &keys, &deleted, None).unwrap();
        assert_eq!(p.refusal, None, "a plain abort leaves the retry to us");
        assert_eq!(p.overlay, overlay, "and the retry offers the same bytes");
    }

    #[test]
    fn a_hash_failure_on_our_bytes_drops_the_mirror_and_the_retry_is_the_networks() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let failed = stats().writer_mismatch.load(Ordering::Relaxed);
        let txs = [tx];
        sh.propose(seq, &parent, pct, drops, &txs, &keys, &deleted, None).unwrap();
        sh.abort(seq, true);
        assert!(stats().writer_mismatch.load(Ordering::Relaxed) > failed);
        assert!(!sh.hydrated, "a mirror that produced wrong bytes is not trusted again: re-hydrate");
        assert_eq!(sh.distrust, Some(seq));
        assert!(sh.propose(seq, &parent, pct, drops, &txs, &keys, &deleted, None).is_none());
        sh.commit(seq, &overlay, false);
        assert_eq!(sh.distrust, None, "the network's write clears it");
    }

    #[test]
    fn the_breaker_turns_the_writer_off_after_repeated_hash_failures() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(LedgerState::new_unverified(state.header.clone()), 0, &log, &trigger);
        let txs = [tx];
        for n in 0..BREAKER_FAILS {
            let (_, _, _, _, _, fresh, _) = payment_ledger();
            sh.state = fresh;
            sh.hydrated = true;
            sh.at_seq = seq - 1;
            sh.distrust = None;
            assert!(!sh.writer_tripped(), "not yet, after {n}");
            sh.propose(seq, &parent, pct, drops, &txs, &keys, &deleted, None).unwrap();
            sh.abort(seq, true);
        }
        assert!(sh.writer_tripped(), "{BREAKER_FAILS} failures inside the window");
        assert_eq!(stats().writer_breaker.load(Ordering::Relaxed), 1);
        sh.state = state;
        sh.hydrated = true;
        sh.at_seq = seq - 1;
        sh.distrust = None;
        assert!(sh.propose(seq, &parent, pct, drops, &txs, &keys, &deleted, None).is_none(), "the writer stays off");
    }

    #[test]
    fn the_writer_never_vouches_for_writing_an_entry_the_mirror_could_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let bad = *overlay.keys().min().unwrap();
        sh.bad_keys.insert(bad);
        let p = sh.propose(seq, &parent, pct, drops, &[tx], &keys, &deleted, None).unwrap();
        let why = p.refusal.expect("a bad entry in the write set: no vouch");
        assert!(why.contains(&hex::encode_upper(bad)) && why.contains("could not load cleanly"), "{why}");
    }

    #[test]
    fn a_planted_canary_is_compared_but_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        std::fs::write(&trigger, b"").unwrap();
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let p = sh.propose(seq, &parent, pct, drops, &[tx], &keys, &deleted, Some(&overlay)).unwrap();
        assert_eq!(p.overlay, overlay, "the offered bytes carry no plant");
        let got = lines(&log);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0]["canary"]["detected"], json!(true), "the compare still sees it");
        sh.commit(seq, &p.overlay, true);
        for (k, v) in &overlay {
            assert_eq!(&reencoded(&sh, k), v, "and the mirror healed");
        }
    }

    #[test]
    fn ledgers_written_while_the_mirror_builds_replay_onto_it_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let (keys, deleted) = meta_of(&overlay);
        let mut sh = shadow(LedgerState::new_unverified(state.header.clone()), 0, &log, &trigger);
        sh.hydrated = false;
        let (go, wait) = std::sync::mpsc::channel::<()>();
        sh.hydrating = Some(std::thread::spawn(move || {
            wait.recv().unwrap();
            Some(HydrateOutcome {
                state,
                at_seq: seq - 1,
                objects: 1,
                undecodable: 0,
                reencode_bad: 0,
                ms: 0,
                bad_keys: HashSet::new(),
                bad_overflow: false,
            })
        }));
        assert!(sh.propose(seq, &parent, pct, drops, &[tx], &keys, &deleted, None).is_none(), "no mirror yet");
        sh.commit(seq, &overlay, false);
        assert_eq!(sh.pending.len(), 1, "queued behind the build");
        go.send(()).unwrap();
        while !sh.hydrating.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        sh.poll_hydrate();
        assert!(sh.hydrated);
        assert_eq!(sh.at_seq, seq, "the written ledger replayed onto the fresh mirror");
        for (k, v) in &overlay {
            assert_eq!(&reencoded(&sh, k), v);
        }
    }

    fn lines(log: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn a_clean_ledger_without_the_trigger_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        sh.on_ledger(seq, &parent, pct, drops, &[tx], &overlay);
        assert_eq!(sh.at_seq, seq, "the ledger was applied");
        assert!(lines(&log).is_empty(), "a clean ledger leaves no receipt: {:?}", lines(&log));
    }

    #[test]
    fn the_canary_plants_one_drop_the_compare_flags_it_and_the_mirror_heals() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        std::fs::write(&trigger, b"").unwrap();
        let (seq, parent, pct, drops, tx, state, overlay) = payment_ledger();
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let fired = stats().canary_fired.load(Ordering::Relaxed);
        sh.on_ledger(seq, &parent, pct, drops, &[tx], &overlay);

        let got = lines(&log);
        assert_eq!(got.len(), 1, "exactly one line, the canary's — no receipt: {got:?}");
        let c = &got[0]["canary"];
        assert_eq!(got[0]["seq"], json!(seq));
        assert_eq!(c["detected"], json!(true), "the compare must flag the planted drop: {c}");
        let key = c["key"].as_str().unwrap();
        let lowest_account_root = overlay
            .keys()
            .filter(|k| {
                let raw = overlay[*k].as_ref().unwrap();
                xrpl_core::codec::decode::decode_transaction_binary(raw).unwrap()["LedgerEntryType"] == "AccountRoot"
            })
            .min()
            .map(hex::encode_upper)
            .unwrap();
        assert_eq!(key, lowest_account_root, "the plant lands on the lowest agreeing AccountRoot");
        assert!(c["diff"].as_str().unwrap().starts_with(key), "the diff names the planted key: {c}");
        assert!(!trigger.exists(), "the trigger is consumed once the line is down");
        assert!(stats().canary_fired.load(Ordering::Relaxed) > fired);

        // The reconcile put the planted entry back on the canonical bytes.
        let k: [u8; 32] = hex::decode(key).unwrap().try_into().unwrap();
        let mut mine: Value = serde_json::from_slice(sh.state.state_map.lookup(&Hash256(k)).unwrap()).unwrap();
        canon_for_encode(&mut mine);
        let enc = xrpl_core::codec::encode::encode_transaction_json(&mine, false).unwrap();
        assert_eq!(Some(&enc), overlay[&k].as_ref(), "the mirror healed to the FFI bytes");
    }

    /// 2026-09-29, #107314806: the FFI leg skipped its verify (a false era
    /// mismatch) and handed over an EMPTY overlay. Compared against, every
    /// native write read as "extra"; reconciled to, the ledger vanished from
    /// the mirror and every later ledger diverged. No compare, no receipt: the
    /// mirror is dropped and the caller re-hydrates from state.rocks.
    #[test]
    fn an_unverified_ledger_drops_the_mirror_and_writes_no_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        let (seq, parent, pct, drops, tx, state, _overlay) = payment_ledger();
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        let before = stats().skipped_unverified.load(Ordering::Relaxed);
        sh.on_ledger(seq, &parent, pct, drops, &[tx], &LedgerOverlay::new());
        assert!(lines(&log).is_empty(), "no compare against a ledger the FFI leg did not verify: {:?}", lines(&log));
        assert!(!sh.hydrated, "the mirror is dropped; the caller re-hydrates from state.rocks");
        assert!(stats().skipped_unverified.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn a_ledger_with_no_account_root_write_keeps_the_trigger_armed() {
        let dir = tempfile::tempdir().unwrap();
        let (log, trigger) = (dir.path().join("receipts.jsonl"), dir.path().join("native_shadow.canary"));
        std::fs::write(&trigger, b"").unwrap();
        let (seq, parent, pct, drops, _tx, state, _overlay) = payment_ledger();
        let mut sh = shadow(state, seq - 1, &log, &trigger);
        sh.on_ledger(seq, &parent, pct, drops, &[], &LedgerOverlay::new());
        assert!(lines(&log).is_empty(), "nothing to plant on, nothing written");
        assert!(trigger.exists(), "the trigger waits for the next ledger");
    }
}
