//! The 19-digit differential: libxrpl (rippled 3.4.1, through the shim) against our engine on real pre-states,
//! each run twice, under mainnet's rules as they are today (Number at the 16-digit Small scale) and under the same
//! rules plus SingleAssetVault, LendingProtocol and LendingProtocolV1_1 (Number at the 19-digit Large330 scale).
//!
//! Why: `setCurrentTransactionRules` (Rules.cpp:35-74) switches Number's mantissa for EVERY transaction the day
//! either vault amendment activates, so every IOU, AMM, offer and payment calculation on mainnet changes its
//! rounding that day. The Number core is fuzzed against libxrpl at both scales (`arith_fuzz.rs`); this target gives
//! the FORMULAS built on it (AMM deposit / withdraw / swap / bid / vote, offer crossing, the payment engine,
//! transfer fees) an oracle that is not our own reading of the source.
//!
//! For every bundle (the `probe_bundle.rs` format), and for each of the two rule sets:
//! 1. the pre-state objects are taken as they are, except the Amendments object, whose list is replaced by
//!    `vectors/large330_rules.json` (mainnet's enabled set), with [`LARGE_ON`] added for the Large run. A bundle
//!    without an Amendments object gets one;
//! 2. libxrpl applies the transaction on that state (`tapNONE` and the real signature; a bundle marked
//!    `"unsigned": true`, a mutant whose transaction was edited, runs under `tapDRY_RUN` with its signature fields
//!    removed; so does a `_fuzz_` vector, whose transaction was built by the fuzzer and never signed);
//! 3. our engine applies it on the same bytes;
//! 4. result code, the set of created / modified / deleted objects and every object's bytes are compared.
//!
//! The network's recorded outcome is NOT the reference here: the rules are rewritten, libxrpl is the oracle. The
//! Small run is the control: it is mainnet today, and a difference there is a finding on the live path or a bundle
//! that lacks an object libxrpl reads (the keys each leg asked for and did not find are printed).
//!
//! One `D330` line per bundle and scale, classes as `libxrpl_341_differential.rs` (AGREE / NOOP / TER / MUT / BYTE /
//! HARNESS). `sens=1` on a Large line: libxrpl's own answer at Large differs from its answer at Small, i.e. the
//! bundle is one whose bytes the 19-digit scale changes (an AGREE on a `sens=0` bundle says little about rounding).
//!
//! Input: `L330_BUNDLES` = `:`-separated files or directories (searched recursively for `*.json`); default every
//! `tests/vectors/*.json` bundle that is not a vault / lending one (those are `libxrpl_341_differential.rs`'s).
//! `L330_TYPES` = comma-separated transaction types to keep. `L330_PIN_DIR` = a directory to write each Large
//! AGREE / NOOP bundle to, in the form `large330_vector.rs` replays (rewritten pre-state, libxrpl's result and
//! objects as the expectation). `L330_REPORT_ONLY=1`: print, do not fail. Needs `--features ffi`; one test,
//! sequential (libxrpl's Number scale and current rules are process state).
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use xrpl_core::types::Hash256;
use xrpl_ffi::{MutationKind, SleProvider};
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::shamap::hash::{sha512_half, sha512_half_prefixed, HASH_PREFIX_TRANSACTION_ID};
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

/// Added to mainnet's rules for the Large run. Either of the first two puts Number at Large330.
const LARGE_ON: &[&str] = &["SingleAssetVault", "LendingProtocol", "LendingProtocolV1_1"];

/// rippled's `tapDRY_RUN` (ApplyView.h): an unsigned transaction skips the signature checks and nothing else.
const TAP_DRY_RUN: u32 = 0x1000;

/// Mainnet's and testnet's fees, for a bundle without a FeeSettings object (our engine's defaults are the same).
const DEFAULT_FEES: (u64, u64, u64) = (10, 1_000_000, 200_000);

type Key = [u8; 32];
/// key → (0 created / 1 modified / 2 deleted, object bytes)
type MutMap = BTreeMap<Key, (u8, Vec<u8>)>;

fn key32(hex_key: &str) -> Key {
    <[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap()
}

fn decode(bytes: &[u8]) -> Value {
    xrpl_core::codec::decode::decode_transaction_binary(bytes).unwrap_or(Value::Null)
}

fn encode(jv: &Value) -> Result<Vec<u8>, String> {
    let mut jv = jv.clone();
    canon_for_encode(&mut jv);
    xrpl_core::codec::encode::encode_transaction_json(&jv, false).map_err(|e| format!("{e:?}"))
}

fn encode_engine_json(json_bytes: &[u8]) -> Vec<u8> {
    serde_json::from_slice::<Value>(json_bytes).ok().and_then(|jv| encode(&jv).ok()).unwrap_or_default()
}

struct BundleProvider<'a> {
    pre: &'a BTreeMap<Key, Vec<u8>>,
    misses: RefCell<BTreeSet<Key>>,
}

impl SleProvider for BundleProvider<'_> {
    fn read(&self, key: &Key) -> Option<&[u8]> {
        let hit = self.pre.get(key).map(|v| v.as_slice());
        if hit.is_none() {
            self.misses.borrow_mut().insert(*key);
        }
        hit
    }
    fn succ(&self, key: &Key, last: Option<&Key>) -> Option<Key> {
        use std::ops::Bound::{Excluded, Unbounded};
        let (k, _) = self.pre.range((Excluded(*key), Unbounded)).next()?;
        last.is_none_or(|l| k < l).then_some(*k)
    }
}

struct Legs {
    lib_ter: String,
    lib_fatal: String,
    lib: MutMap,
    lib_misses: Vec<Key>,
    our_ter: String,
    ours: MutMap,
    our_misses: Vec<Key>,
}

struct Ledger {
    seq: u32,
    pct: u32,
    resolution: u8,
    total_coins: u64,
    parent_hash: Key,
}

fn ledger_of(bundle: &Value) -> Ledger {
    Ledger {
        seq: bundle["seq"].as_u64().unwrap() as u32,
        pct: bundle["parent_close_time"].as_u64().unwrap() as u32,
        resolution: bundle["close_time_resolution"].as_u64().unwrap_or(10) as u8,
        total_coins: bundle["total_coins"].as_u64().unwrap(),
        parent_hash: key32(bundle["parent_hash"].as_str().unwrap()),
    }
}

/// The ledger's fees from its FeeSettings object, [`DEFAULT_FEES`] without one.
fn fees_of(pre: &BTreeMap<Key, Vec<u8>>) -> (u64, u64, u64) {
    let Some(bytes) = pre.get(&keylet::fee_settings_key().0) else { return DEFAULT_FEES };
    let jv = decode(bytes);
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    let field = |new: &str, old: &str, old_is_hex: bool, default: u64| -> u64 {
        if let Some(n) = jv.get(new).and_then(num) {
            return n;
        }
        match jv.get(old) {
            Some(v) if old_is_hex => v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()).unwrap_or(default),
            Some(v) => num(v).unwrap_or(default),
            None => default,
        }
    };
    (
        field("BaseFeeDrops", "BaseFee", true, DEFAULT_FEES.0),
        field("ReserveBaseDrops", "ReserveBase", false, DEFAULT_FEES.1),
        field("ReserveIncrementDrops", "ReserveIncrement", false, DEFAULT_FEES.2),
    )
}

fn amendment_id(name: &str) -> String {
    hex::encode_upper(sha512_half(name.as_bytes()).0)
}

/// Mainnet's enabled amendments (`vectors/large330_rules.json`), [`LARGE_ON`] added when `large`.
fn rules(large: bool) -> Vec<String> {
    let file: Value = serde_json::from_str(include_str!("vectors/large330_rules.json")).unwrap();
    let mut list: Vec<String> = file["amendments"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_uppercase()).collect();
    let on: Vec<String> = LARGE_ON.iter().map(|n| amendment_id(n)).collect();
    list.retain(|a| !on.contains(a));
    if large {
        list.extend(on);
    }
    list
}

/// Replace the Amendments object's list (make the object when the bundle has none).
fn set_rules(pre: &mut BTreeMap<Key, Vec<u8>>, large: bool) -> Result<(), String> {
    let key = keylet::amendments_key().0;
    let mut jv = match pre.get(&key) {
        Some(b) => decode(b),
        None => json!({"LedgerEntryType": "Amendments", "Flags": 0}),
    };
    jv["Amendments"] = Value::Array(rules(large).into_iter().map(Value::String).collect());
    pre.insert(key, encode(&jv)?);
    Ok(())
}

/// The transaction's bytes and the ApplyFlags it runs under. A signed transaction must hash to the id the bundle
/// records; an `"unsigned": true` bundle loses its signature fields, takes the id of what is left and runs dry.
fn tx_of(name: &str, bundle: &mut Value) -> Result<(Vec<u8>, u32), String> {
    let unsigned = bundle["unsigned"].as_bool().unwrap_or(false) || name.contains("_fuzz_");
    let want = bundle["tx"]["hash"].as_str().unwrap_or_default().to_uppercase();
    let tx = bundle["tx"].as_object_mut().ok_or("no tx")?;
    for k in ["hash", "ctid", "date", "inLedger", "ledger_index", "validated", "meta", "metaData"] {
        tx.remove(k);
    }
    if unsigned {
        tx.remove("TxnSignature");
        tx.remove("Signers");
        tx.insert("SigningPubKey".into(), Value::String(String::new()));
    }
    let bytes = encode(&bundle["tx"]).map_err(|e| format!("the transaction does not encode: {e}"))?;
    let id = hex::encode_upper(sha512_half_prefixed(&HASH_PREFIX_TRANSACTION_ID, &bytes).0);
    if !unsigned && id != want {
        return Err(format!("the re-encoded transaction hashes to {id}, the bundle says {want}"));
    }
    bundle["tx"]["hash"] = Value::String(id);
    Ok((bytes, if unsigned { TAP_DRY_RUN } else { 0 }))
}

fn libxrpl_leg(
    ledger: &Ledger,
    pre: &BTreeMap<Key, Vec<u8>>,
    tx_bytes: &[u8],
    flags: u32,
    network_id: u32,
) -> Result<(String, String, MutMap, Vec<Key>), String> {
    let jv = decode(pre.get(&keylet::amendments_key().0).ok_or("no Amendments object")?);
    let amendments: Vec<Key> = jv["Amendments"].as_array().ok_or("Amendments unreadable")?.iter().filter_map(Value::as_str).map(key32).collect();
    let (base, reserve, increment) = fees_of(pre);
    let info = xrpl_ffi::LedgerInfo {
        seq: ledger.seq,
        parent_close_time: ledger.pct,
        close_time_resolution: ledger.resolution as u32,
        total_drops: ledger.total_coins,
        parent_hash: ledger.parent_hash,
        base_fee_drops: base,
        reserve_drops: reserve,
        increment_drops: increment,
    };
    let provider = BundleProvider { pre, misses: RefCell::new(BTreeSet::new()) };
    let out = xrpl_ffi::apply_with_mutations(tx_bytes, &amendments, &info, &provider, flags, network_id)
        .ok_or("the shim returned no result")?;
    let muts: MutMap = out
        .mutations
        .iter()
        .map(|m| match m.kind {
            MutationKind::Created => (m.key, (0, m.data.clone())),
            MutationKind::Modified => (m.key, (1, m.data.clone())),
            MutationKind::Deleted => (m.key, (2, Vec::new())),
        })
        .collect();
    Ok((out.ter_name, out.last_fatal, muts, provider.misses.into_inner().into_iter().collect()))
}

fn our_leg(ledger: &Ledger, pre: &BTreeMap<Key, Vec<u8>>, tx: &Value) -> Result<(String, MutMap, Vec<Key>), String> {
    let header = LedgerHeader {
        sequence: ledger.seq - 1,
        total_coins: ledger.total_coins,
        parent_hash: Hash256(ledger.parent_hash),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: ledger.pct,
        close_time: ledger.pct,
        close_time_resolution: ledger.resolution,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    for (k, bytes) in pre {
        let mut jv = xrpl_core::codec::decode::decode_transaction_binary(bytes)
            .map_err(|e| format!("pre-image {} does not decode: {e:?}", hex::encode_upper(k)))?;
        hexify_addresses(&mut jv);
        state.state_map.insert(Hash256(*k), serde_json::to_vec(&jv).unwrap()).map_err(|e| format!("{e:?}"))?;
    }
    let txf = build_txfields(tx).ok_or("build_txfields refused the transaction")?;
    xrpl_ledger::ledger::sandbox::read_log_begin();
    let (ter, mut mods) = native_apply_one(&state, &txf);
    let reads = xrpl_ledger::ledger::sandbox::read_log_take();
    let misses = reads.iter().filter(|k| state.state_map.lookup(k).is_none()).map(|k| k.0).collect();
    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx["hash"].as_str().unwrap_or_default(),
        ledger.seq,
    );
    let muts: MutMap = mods
        .iter()
        .map(|(k, e)| match e {
            SandboxEntry::Created(b) => (k.0, (0, encode_engine_json(b))),
            SandboxEntry::Modified(b) => (k.0, (1, encode_engine_json(b))),
            SandboxEntry::Deleted => (k.0, (2, Vec::new())),
        })
        .collect();
    Ok((ter, muts, misses))
}

fn strip_threading(sle: &[u8]) -> String {
    let mut v = decode(sle);
    match v.as_object_mut() {
        Some(o) => {
            o.remove("PreviousTxnID");
            o.remove("PreviousTxnLgrSeq");
            v.to_string()
        }
        None => hex::encode(sle),
    }
}

fn entry_type(b: &[u8]) -> String {
    decode(b).get("LedgerEntryType").and_then(Value::as_str).unwrap_or("?").to_string()
}

fn field_diff(ours: &[u8], lib: &[u8]) -> String {
    let (a, b) = (decode(ours), decode(lib));
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else { return "undecodable".into() };
    let names: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let show = |v: Option<&Value>| v.map(Value::to_string).unwrap_or_else(|| "(absent)".into());
    names
        .into_iter()
        .filter(|n| a.get(*n) != b.get(*n))
        .map(|n| format!("{n}: ours {} / libxrpl {}", show(a.get(n)), show(b.get(n))))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A Modified entry equal to its pre-image once the threading stamps are set aside.
fn threading_only(pre: &BTreeMap<Key, Vec<u8>>, k: &Key, kind: u8, b: &[u8]) -> bool {
    kind == 1 && pre.get(k).is_some_and(|p| strip_threading(p) == strip_threading(b))
}

fn classify(legs: &Legs, pre: &BTreeMap<Key, Vec<u8>>) -> &'static str {
    let (lib, ours) = (&legs.lib, &legs.ours);
    if legs.lib_ter != legs.our_ter {
        return "TER";
    }
    let kinds_agree = lib.iter().all(|(k, (kind, _))| ours.get(k).is_none_or(|(ok, _)| ok == kind));
    let same_keys = lib.len() == ours.len() && lib.keys().all(|k| ours.contains_key(k));
    let bytes_agree = lib.iter().all(|(k, (_, b))| ours.get(k).is_none_or(|(_, ob)| ob == b));
    if !same_keys || !kinds_agree {
        let only_noop = kinds_agree
            && lib.iter().filter(|(k, _)| !ours.contains_key(*k)).all(|(k, (kind, b))| threading_only(pre, k, *kind, b))
            && ours.iter().filter(|(k, _)| !lib.contains_key(*k)).all(|(k, (kind, b))| threading_only(pre, k, *kind, b));
        return if !only_noop {
            "MUT"
        } else if bytes_agree {
            "NOOP"
        } else {
            "BYTE"
        };
    }
    if bytes_agree {
        "AGREE"
    } else {
        "BYTE"
    }
}

fn describe(legs: &Legs) -> Vec<String> {
    const KIND: [&str; 3] = ["created", "modified", "deleted"];
    let mut lines = Vec::new();
    let keys: BTreeSet<&Key> = legs.lib.keys().chain(legs.ours.keys()).collect();
    for k in keys {
        let kh = hex::encode_upper(k);
        match (legs.ours.get(k), legs.lib.get(k)) {
            (Some((ok, ob)), Some((lk, lb))) if ok != lk => {
                let t = if lb.is_empty() { entry_type(ob) } else { entry_type(lb) };
                lines.push(format!("{kh} {t}: ours {} / libxrpl {}", KIND[*ok as usize], KIND[*lk as usize]));
            }
            (Some((_, ob)), Some((_, lb))) if ob != lb => {
                lines.push(format!("{kh} {}: {}", entry_type(lb), field_diff(ob, lb)));
            }
            (Some((ok, ob)), None) => {
                lines.push(format!("{kh} {}: only ours ({}) {}", entry_type(ob), KIND[*ok as usize], decode(ob)));
            }
            (None, Some((lk, lb))) => {
                lines.push(format!("{kh} {}: only libxrpl ({}) {}", entry_type(lb), KIND[*lk as usize], decode(lb)));
            }
            _ => {}
        }
    }
    if !legs.lib_fatal.is_empty() {
        // `L330_LOG_CHARS`: how much of libxrpl's log to print (with XRPL_FFI_TRACE=1 it is the flow engine's trace).
        let n = std::env::var("L330_LOG_CHARS").ok().and_then(|s| s.parse().ok()).unwrap_or(1500);
        lines.push(format!("libxrpl log: {}", legs.lib_fatal.chars().take(n).collect::<String>()));
    }
    let keys = |v: &[Key]| v.iter().map(hex::encode_upper).collect::<Vec<_>>().join(" ");
    lines.push(format!("keys libxrpl asked for and the bundle lacks: [{}]", keys(&legs.lib_misses)));
    lines.push(format!("keys our engine asked for and the bundle lacks: [{}]", keys(&legs.our_misses)));
    lines
}

fn pre_of(bundle: &Value) -> BTreeMap<Key, Vec<u8>> {
    bundle["pre"].as_object().unwrap().iter().map(|(k, v)| (key32(k), hex::decode(v.as_str().unwrap().trim()).unwrap())).collect()
}

struct Row {
    class: &'static str,
    lib_ter: String,
    our_ter: String,
    detail: Vec<String>,
    lib: MutMap,
    pre: BTreeMap<Key, Vec<u8>>,
    tx: Value,
}

fn network_id(name: &str, bundle: &Value) -> u32 {
    if let Some(n) = bundle["network_id"].as_u64() {
        return n as u32;
    }
    if name.contains("_devnet") {
        2
    } else if name.contains("_testnet") {
        1
    } else {
        0
    }
}

fn run_scale(name: &str, bundle: &Value, large: bool) -> Result<Row, String> {
    let mut bundle = bundle.clone();
    let mut pre = pre_of(&bundle);
    set_rules(&mut pre, large)?;
    let (tx_bytes, flags) = tx_of(name, &mut bundle)?;
    let ledger = ledger_of(&bundle);
    let (lib_ter, lib_fatal, lib, lib_misses) = libxrpl_leg(&ledger, &pre, &tx_bytes, flags, network_id(name, &bundle))?;
    let (our_ter, ours, our_misses) = our_leg(&ledger, &pre, &bundle["tx"])?;
    let legs = Legs { lib_ter, lib_fatal, lib, lib_misses, our_ter, ours, our_misses };
    let class = classify(&legs, &pre);
    let detail = if class == "AGREE" { Vec::new() } else { describe(&legs) };
    Ok(Row { class, lib_ter: legs.lib_ter, our_ter: legs.our_ter, detail, lib: legs.lib, pre, tx: bundle["tx"].clone() })
}

/// The bundle as `large330_vector.rs` replays it: the rewritten pre-state, libxrpl's result, and as the
/// expectation every object libxrpl created, changed or deleted (its unchanged `update()`s left out).
fn pin(dir: &str, name: &str, bundle: &Value, row: &Row, sensitive: bool) {
    let hexmap = |m: &mut serde_json::Map<String, Value>, k: &Key, b: &[u8]| {
        m.insert(hex::encode_upper(k), Value::String(hex::encode_upper(b)));
    };
    let (mut pre, mut expect) = (serde_json::Map::new(), serde_json::Map::new());
    row.pre.iter().for_each(|(k, b)| hexmap(&mut pre, k, b));
    for (k, (kind, b)) in &row.lib {
        if !threading_only(&row.pre, k, *kind, b) {
            hexmap(&mut expect, k, b);
        }
    }
    let out = json!({
        "source": name,
        "oracle": format!("libxrpl {} at Number's Large330 scale ({} added to mainnet's rules)", xrpl_ffi::libxrpl_version(), LARGE_ON.join(", ")),
        "scale_sensitive": sensitive,
        "tx": row.tx,
        "result": row.lib_ter,
        "seq": bundle["seq"],
        "parent_hash": bundle["parent_hash"],
        "parent_close_time": bundle["parent_close_time"],
        "total_coins": bundle["total_coins"],
        "close_time_resolution": bundle["close_time_resolution"].as_u64().unwrap_or(10),
        "pre": pre,
        "expect": expect,
    });
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(std::path::Path::new(dir).join(format!("{name}.json")), serde_json::to_string(&out).unwrap()).unwrap();
}

fn collect(path: &std::path::Path, recurse: bool, out: &mut Vec<std::path::PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path).unwrap().flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in &entries {
            if recurse || !p.is_dir() {
                collect(p, recurse, out);
            }
        }
    } else if path.extension().is_some_and(|e| e == "json") {
        out.push(path.to_path_buf());
    }
}

fn bundle_files() -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    match std::env::var("L330_BUNDLES") {
        Ok(list) => list.split(':').filter(|s| !s.is_empty()).for_each(|p| collect(std::path::Path::new(p), true, &mut files)),
        Err(_) => {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
            let mut all = Vec::new();
            collect(&dir, false, &mut all);
            files.extend(all.into_iter().filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                !["vault_", "loan_", "c27_", "c28_"].iter().any(|pre| n.starts_with(pre))
            }));
        }
    }
    files
}

fn name_of(path: &std::path::Path) -> String {
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    if stem == "bundle" {
        path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or(stem)
    } else {
        stem
    }
}

fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
}

#[test]
fn libxrpl_against_our_engine_at_both_number_scales() {
    let version = xrpl_ffi::libxrpl_version();
    println!("D330 libxrpl version linked: {version}");
    assert!(
        version.contains("3.4.1") || std::env::var("L330_ANY_VERSION").is_ok(),
        "this differential is about rippled 3.4.1; the shim links {version}"
    );
    println!("D330 rules: Small = {} amendments, Large = those + {}", rules(false).len(), LARGE_ON.join(" + "));
    let types: Option<Vec<String>> = std::env::var("L330_TYPES").ok().map(|s| s.split(',').map(str::to_string).collect());
    let pin_dir = std::env::var("L330_PIN_DIR").ok();
    let report_only = std::env::var("L330_REPORT_ONLY").is_ok();

    let files = bundle_files();
    assert!(!files.is_empty(), "no bundles");
    let mut tally: BTreeMap<(&'static str, String, &'static str), u32> = BTreeMap::new();
    let mut sens_tally: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut failing = Vec::new();
    let mut ran = 0;
    for path in &files {
        let name = name_of(path);
        let Ok(bundle) = serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap()) else { continue };
        if bundle.get("pre").is_none() || bundle.get("tx").is_none() || bundle.get("seq").is_none() {
            continue;
        }
        let tx_type = bundle["tx"]["TransactionType"].as_str().unwrap_or("?").to_string();
        if types.as_ref().is_some_and(|t| !t.contains(&tx_type)) {
            continue;
        }
        ran += 1;
        let recorded = bundle["result"].as_str().unwrap_or("?").to_string();
        let mut small_lib: Option<(String, MutMap)> = None;
        for (scale, large) in [("Small", false), ("Large", true)] {
            let row = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_scale(&name, &bundle, large)))
                .unwrap_or_else(|p| Err(format!("panic: {}", panic_text(p))));
            match row {
                Ok(r) => {
                    // Threading stamps carry the transaction's id and ledger, the same at both scales.
                    let sensitive = large && small_lib.as_ref().is_some_and(|(t, m)| *t != r.lib_ter || *m != r.lib);
                    println!(
                        "D330 {} {scale} {name} {tx_type} recorded={recorded} libxrpl={} ours={}{}",
                        r.class,
                        r.lib_ter,
                        r.our_ter,
                        if large { format!(" sens={}", sensitive as u8) } else { String::new() }
                    );
                    let cap = std::env::var("L330_LOG_CHARS").ok().and_then(|s| s.parse().ok()).unwrap_or(3000);
                    r.detail.iter().for_each(|l| println!("D330   {}", l.chars().take(cap).collect::<String>()));
                    *tally.entry((scale, tx_type.clone(), r.class)).or_default() += 1;
                    if matches!(r.class, "TER" | "MUT" | "BYTE") {
                        failing.push(format!("{name}@{scale}"));
                    }
                    if large {
                        let e = sens_tally.entry(tx_type.clone()).or_default();
                        e.0 += 1;
                        e.1 += sensitive as u32;
                        if let (Some(dir), "AGREE" | "NOOP") = (&pin_dir, r.class) {
                            pin(dir, &name, &bundle, &r, sensitive);
                        }
                    } else {
                        small_lib = Some((r.lib_ter.clone(), r.lib.clone()));
                    }
                }
                Err(e) => {
                    println!("D330 HARNESS {scale} {name} {tx_type}: could not run: {e}");
                    *tally.entry((scale, tx_type.clone(), "HARNESS")).or_default() += 1;
                    failing.push(format!("{name}@{scale}"));
                }
            }
        }
    }
    println!("D330 SUMMARY {ran} bundles");
    for ((scale, t, c), n) in &tally {
        println!("D330 SUMMARY {scale} {t} {c} {n}");
    }
    for (t, (n, s)) in &sens_tally {
        println!("D330 SUMMARY scale-sensitive {t} {s} of {n}");
    }
    assert!(report_only || failing.is_empty(), "{} run(s) differ or could not run: {}", failing.len(), failing.join(" "));
}
