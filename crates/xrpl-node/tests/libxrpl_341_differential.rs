//! The mainnet-arm differential: libxrpl (rippled 3.4.1, through the shim) against our engine, on real devnet
//! pre-states whose rules are rewritten to what a 3.4.1 network runs.
//!
//! Why: every network replay of the vault and lending transactors ran on devnet (rippled 3.5.0-rc5), most of it
//! with `fixCleanup3_5_0` enabled. Mainnet validators run 3.4.1, which does not know that amendment: when
//! SingleAssetVault / LendingProtocol activate there, the code path taken is the fixCleanup3_5_0-OFF arm. This
//! target gives that arm an oracle that is not our own reading of the source.
//!
//! For every bundle (the `vault_vector.rs` / `probe_bundle.rs` format):
//! 1. the pre-state objects are taken as they are, except the Amendments object, from which the amendments in
//!    [`REMOVED`] are deleted (what is left is mainnet's enabled set plus SingleAssetVault, LendingProtocol and
//!    LendingProtocolV1_1, every one of them `Supported::Yes` or retired in 3.4.1's `features.macro`);
//! 2. libxrpl applies the transaction (really signed: `tapNONE`, so preflight's signature check, preclaim's
//!    sequence / LastLedgerSequence / signer checks and the invariant checkers all run) on that state, the
//!    remaining amendments as its Rules;
//! 3. our engine applies it on the same bytes;
//! 4. result code, the set of created / modified / deleted objects and every object's bytes are compared.
//!
//! There is no network truth for the rewritten state: libxrpl is the oracle. A difference is an engine finding on
//! the mainnet path or a harness problem (an object libxrpl reads that a bundle hydrated for the 3.5.0 path
//! lacks); the keys each leg asked for and did not find are printed with every difference.
//!
//! Classes (one `D341` line per bundle):
//! - `AGREE`  same result, same object set, same bytes;
//! - `NOOP`   as AGREE but for threading-only writes (an object `update()`d and not changed);
//! - `TER` / `MUT` / `BYTE`  result / object set / bytes differ: the test fails;
//! - `SPONSOR`  the transaction carries `sfSponsor`: featureSponsor is not ported and is removed from the rules,
//!   reported and not counted;
//! - `HARNESS`  the bundle could not be run (said why): the test fails.
//!
//! libxrpl's invariant checkers read objects the transaction itself does not; `vectors/diff341_extra_pre.json`
//! supplies the ones the pinned loan vectors lack (see [`extra_pre`]).
//!
//! `vs-devnet=same|differs` says whether libxrpl 3.4.1 on the rewritten rules reproduced what devnet recorded:
//! `differs` marks the transactions where the two arms part.
//!
//! Input: `DIFF341_BUNDLES` = `:`-separated files or directories (searched recursively for `*.json`); default the
//! pinned devnet vectors `tests/vectors/{vault,loan,c27}_*.json`. `DIFF341_VERBOSE=1` prints libxrpl's objects
//! for every bundle where it parts from devnet. Needs `--features ffi`; one test, sequential (libxrpl's Number
//! scale and current rules are process state).
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use xrpl_core::types::Hash256;
use xrpl_ffi::{MutationKind, SleProvider};
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::shamap::hash::{sha512_half, sha512_half_prefixed, HASH_PREFIX_TRANSACTION_ID};
use xrpl_node::native_apply::{build_txfields, canon_for_encode, decode_address, hexify_addresses, native_apply_one};

/// Amendments devnet has enabled that are deleted from the Amendments object: `fixCleanup3_5_0` is unknown to
/// rippled 3.4.1; the others are known to it but not enabled on mainnet (read from a mainnet node's `feature`,
/// 2026-11-21), and Sponsor / DynamicMPT are not ported. An amendment's ID is sha512Half(its name).
const REMOVED: &[&str] =
    &["fixCleanup3_5_0", "Sponsor", "DynamicMPT", "ConfidentialTransfer", "XChainBridge", "fixXChainRewardRounding"];

/// Devnet's network id (below 1025: transactions carry no NetworkID field).
const NETWORK_ID: u32 = 2;

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

/// The pre-state as libxrpl sees it: the bundle's objects, byte for byte. Records the keys asked for and absent.
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

/// The ledger's fees from its FeeSettings object: (base, reserve, increment) in drops.
fn fees_of(pre: &BTreeMap<Key, Vec<u8>>) -> Result<(u64, u64, u64), String> {
    let jv = decode(pre.get(&keylet::fee_settings_key().0).ok_or("the bundle has no FeeSettings object")?);
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    let field = |new: &str, old: &str, old_is_hex: bool| -> Result<u64, String> {
        if let Some(n) = jv.get(new).and_then(num) {
            return Ok(n);
        }
        let v = jv.get(old).ok_or(format!("FeeSettings has neither {new} nor {old}"))?;
        match v.as_str() {
            Some(s) if old_is_hex => u64::from_str_radix(s, 16).map_err(|e| e.to_string()),
            _ => num(v).ok_or(format!("FeeSettings {old} unreadable")),
        }
    };
    Ok((
        field("BaseFeeDrops", "BaseFee", true)?,
        field("ReserveBaseDrops", "ReserveBase", false)?,
        field("ReserveIncrementDrops", "ReserveIncrement", false)?,
    ))
}

/// The enabled amendments of the (already rewritten) pre-state.
fn amendments_of(pre: &BTreeMap<Key, Vec<u8>>) -> Result<Vec<Key>, String> {
    let jv = decode(pre.get(&keylet::amendments_key().0).ok_or("the bundle has no Amendments object")?);
    Ok(jv["Amendments"].as_array().ok_or("Amendments unreadable")?.iter().filter_map(Value::as_str).map(key32).collect())
}

/// Delete [`REMOVED`] from the Amendments object. Returns the names actually removed.
fn rewrite_amendments(pre: &mut BTreeMap<Key, Vec<u8>>) -> Result<Vec<&'static str>, String> {
    let key = keylet::amendments_key().0;
    let mut jv = decode(pre.get(&key).ok_or("the bundle has no Amendments object")?);
    let list = jv["Amendments"].as_array_mut().ok_or("Amendments unreadable")?;
    let mut removed = Vec::new();
    for name in REMOVED {
        let id = hex::encode_upper(sha512_half(name.as_bytes()).0);
        let before = list.len();
        list.retain(|a| !a.as_str().is_some_and(|s| s.eq_ignore_ascii_case(&id)));
        if list.len() != before {
            removed.push(*name);
        }
    }
    pre.insert(key, encode(&jv)?);
    Ok(removed)
}

/// The signed transaction as the network carried it; its id must come out as the bundle records it.
fn tx_bytes_of(bundle: &Value) -> Result<Vec<u8>, String> {
    let mut tx = bundle["tx"].clone();
    let o = tx.as_object_mut().ok_or("no tx")?;
    for k in ["hash", "ctid", "date", "inLedger", "ledger_index", "validated", "meta", "metaData"] {
        o.remove(k);
    }
    let bytes = encode(&tx).map_err(|e| format!("the transaction does not encode: {e}"))?;
    let id = hex::encode_upper(sha512_half_prefixed(&HASH_PREFIX_TRANSACTION_ID, &bytes).0);
    let want = bundle["tx"]["hash"].as_str().unwrap_or_default().to_uppercase();
    if id != want {
        return Err(format!("the re-encoded transaction hashes to {id}, the bundle says {want}"));
    }
    Ok(bytes)
}

fn libxrpl_leg(ledger: &Ledger, pre: &BTreeMap<Key, Vec<u8>>, tx_bytes: &[u8]) -> Result<(String, String, MutMap, Vec<Key>), String> {
    let (base, reserve, increment) = fees_of(pre)?;
    let amendments = amendments_of(pre)?;
    let info = xrpl_ffi::LedgerInfo {
        seq: ledger.seq,
        parent_close_time: ledger.pct,
        total_drops: ledger.total_coins,
        parent_hash: ledger.parent_hash,
        base_fee_drops: base,
        reserve_drops: reserve,
        increment_drops: increment,
    };
    let provider = BundleProvider { pre, misses: RefCell::new(BTreeSet::new()) };
    let out = xrpl_ffi::apply_with_mutations(tx_bytes, &amendments, &info, &provider, 0, NETWORK_ID)
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

fn both_legs(bundle: &Value, pre: &BTreeMap<Key, Vec<u8>>, tx_bytes: &[u8]) -> Result<Legs, String> {
    let ledger = ledger_of(bundle);
    let (lib_ter, lib_fatal, lib, lib_misses) = libxrpl_leg(&ledger, pre, tx_bytes)?;
    let (our_ter, ours, our_misses) = our_leg(&ledger, pre, &bundle["tx"])?;
    Ok(Legs { lib_ter, lib_fatal, lib, lib_misses, our_ter, ours, our_misses })
}

/// The object with its threading stamps set aside.
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

/// AGREE / NOOP / TER / MUT / BYTE, as `tx_fuzz` classes a pair of outcomes.
fn classify(legs: &Legs, pre: &BTreeMap<Key, Vec<u8>>) -> &'static str {
    let (lib, ours) = (&legs.lib, &legs.ours);
    // A Modified entry equal to its pre-image once the threading stamps are set aside: rippled keeps an object a
    // transactor `update()`d without changing; we drop it (and the other way round).
    let threading_only =
        |k: &Key, kind: u8, b: &[u8]| kind == 1 && pre.get(k).is_some_and(|p| strip_threading(p) == strip_threading(b));
    if legs.lib_ter != legs.our_ter {
        return "TER";
    }
    let kinds_agree = lib.iter().all(|(k, (kind, _))| ours.get(k).is_none_or(|(ok, _)| ok == kind));
    let same_keys = lib.len() == ours.len() && lib.keys().all(|k| ours.contains_key(k));
    let bytes_agree = lib.iter().all(|(k, (_, b))| ours.get(k).is_none_or(|(_, ob)| ob == b));
    if !same_keys || !kinds_agree {
        let only_noop = kinds_agree
            && lib.iter().filter(|(k, _)| !ours.contains_key(*k)).all(|(k, (kind, b))| threading_only(k, *kind, b))
            && ours.iter().filter(|(k, _)| !lib.contains_key(*k)).all(|(k, (kind, b))| threading_only(k, *kind, b));
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

/// Every object the two legs disagree on, one line each.
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
        lines.push(format!("libxrpl log: {}", legs.lib_fatal.chars().take(1500).collect::<String>()));
    }
    let keys = |v: &[Key]| v.iter().map(hex::encode_upper).collect::<Vec<_>>().join(" ");
    lines.push(format!("keys libxrpl asked for and the bundle lacks: [{}]", keys(&legs.lib_misses)));
    lines.push(format!("keys our engine asked for and the bundle lacks: [{}]", keys(&legs.our_misses)));
    lines
}

/// Whether libxrpl on the rewritten rules reproduced what devnet recorded for this transaction.
fn same_as_devnet(bundle: &Value, legs: &Legs, pre: &BTreeMap<Key, Vec<u8>>) -> bool {
    if bundle["result"].as_str() != Some(legs.lib_ter.as_str()) {
        return false;
    }
    let expect = bundle["expect"].as_object().unwrap();
    let want = |k: &Key| expect.get(&hex::encode_upper(k)).and_then(Value::as_str).map(|s| s.trim().to_uppercase());
    let wrote = legs.lib.iter().all(|(k, (kind, b))| match want(k) {
        Some(w) => w == hex::encode_upper(b),
        // Not named by devnet's metadata: fine only for an unchanged object.
        None => *kind == 1 && pre.get(k).is_some_and(|p| strip_threading(p) == strip_threading(b)),
    });
    let named = expect.iter().all(|(k, w)| {
        let (k, w) = (key32(k), w.as_str().unwrap_or_default().trim().to_uppercase());
        legs.lib.contains_key(&k) || pre.get(&k).is_some_and(|p| hex::encode_upper(p) == w)
    });
    wrote && named
}

/// The shim hands libxrpl a ledger header whose `closeTime` is the PARENT's close time (xrpl_shim.cpp: closed-ledger
/// view, `header.closeTime = parent_close_time`). On the network the ledger being built carries the parent's close
/// time plus the resolution, and `LoanSet` dates the loan from exactly that (`view.header().closeTime`,
/// LoanSet.cpp:228; nothing else in the vault or lending transactors reads it). So libxrpl through the shim
/// writes `StartDate` and `NextPaymentDueDate` one resolution early. When that is the whole difference on the
/// created Loan, libxrpl's two fields are moved forward so that the rest of the object still compares byte for
/// byte. Returns whether it did.
fn adjust_loan_start(legs: &mut Legs, resolution: u64) -> bool {
    let mut adjusted = false;
    for (k, (kind, lib_bytes)) in legs.lib.iter_mut() {
        let Some((_, our_bytes)) = legs.ours.get(k) else { continue };
        if *kind != 0 || entry_type(lib_bytes) != "Loan" {
            continue;
        }
        let (mut lib, ours) = (decode(lib_bytes), decode(our_bytes));
        let early = |f: &str| match (lib[f].as_u64(), ours[f].as_u64()) {
            (Some(l), Some(o)) => l + resolution == o,
            (None, None) => true,
            _ => false,
        };
        if lib["StartDate"].as_u64().is_some() && early("StartDate") && early("NextPaymentDueDate") {
            for f in ["StartDate", "NextPaymentDueDate"] {
                if let Some(v) = lib[f].as_u64() {
                    lib[f] = Value::from(v + resolution);
                }
            }
            if let Ok(b) = encode(&lib) {
                *lib_bytes = b;
                adjusted = true;
            }
        }
    }
    adjusted
}

struct Row {
    class: &'static str,
    tx_type: String,
    devnet: String,
    lib_ter: String,
    our_ter: String,
    vs_devnet: bool,
    detail: Vec<String>,
}

fn pre_of(bundle: &Value) -> BTreeMap<Key, Vec<u8>> {
    bundle["pre"].as_object().unwrap().iter().map(|(k, v)| (key32(k), hex::decode(v.as_str().unwrap().trim()).unwrap())).collect()
}

/// Objects libxrpl's invariant checkers read and the transaction does not (so the bundle, hydrated from the
/// transaction's own read-set, lacks them): the vault's share MPTokenIssuance for the loan transactions, which
/// ValidVault looks up for every vault a transaction updates ("updated vault must have shares"). Fetched from
/// devnet at each bundle's parent ledger and kept beside the vectors, keyed by vector name.
fn extra_pre(name: &str) -> BTreeMap<Key, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut take = |text: &str| {
        let all: Value = serde_json::from_str(text).unwrap();
        for (k, v) in all.get(name).and_then(Value::as_object).into_iter().flatten() {
            out.insert(key32(k), hex::decode(v.as_str().unwrap()).unwrap());
        }
    };
    take(include_str!("vectors/diff341_extra_pre.json"));
    // `DIFF341_EXTRA`: a file of the same shape, for bundles that are not pinned vectors.
    if let Ok(path) = std::env::var("DIFF341_EXTRA") {
        take(&std::fs::read_to_string(path).unwrap());
    }
    out
}

fn run_bundle(name: &str, bundle: &Value) -> Result<Row, String> {
    let tx_type = bundle["tx"]["TransactionType"].as_str().unwrap_or("?").to_string();
    let devnet = bundle["result"].as_str().unwrap_or("?").to_string();
    let mut pre = pre_of(bundle);
    for (k, v) in extra_pre(name) {
        pre.entry(k).or_insert(v);
    }
    rewrite_amendments(&mut pre)?;
    let tx_bytes = tx_bytes_of(bundle)?;
    let mut legs = both_legs(bundle, &pre, &tx_bytes)?;
    let shim_close_time = tx_type == "LoanSet" && adjust_loan_start(&mut legs, ledger_of(bundle).resolution as u64);
    let mut class = classify(&legs, &pre);
    let vs_devnet = same_as_devnet(bundle, &legs, &pre);
    let mut detail = if class == "AGREE" { Vec::new() } else { describe(&legs) };
    if shim_close_time {
        detail.push("shim: libxrpl's Loan dated from the parent close time; StartDate and NextPaymentDueDate moved by the resolution before comparing".into());
    }
    if bundle["tx"].get("Sponsor").is_some() && !matches!(class, "AGREE" | "NOOP") {
        class = "SPONSOR";
    }
    if !vs_devnet && std::env::var("DIFF341_VERBOSE").is_ok() {
        for (k, (kind, b)) in &legs.lib {
            detail.push(format!("libxrpl wrote {} kind {kind}: {}", hex::encode_upper(k), decode(b)));
        }
    }
    Ok(Row { class, tx_type, devnet, lib_ter: legs.lib_ter, our_ter: legs.our_ter, vs_devnet, detail })
}

fn collect(path: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path).unwrap().flatten().map(|e| e.path()).collect();
        entries.sort();
        entries.iter().for_each(|p| collect(p, out));
    } else if path.extension().is_some_and(|e| e == "json") {
        out.push(path.to_path_buf());
    }
}

fn bundle_files() -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    match std::env::var("DIFF341_BUNDLES") {
        Ok(list) => list.split(':').filter(|s| !s.is_empty()).for_each(|p| collect(std::path::Path::new(p), &mut files)),
        Err(_) => {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
            let mut all = Vec::new();
            collect(&dir, &mut all);
            files.extend(all.into_iter().filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                n.contains("_devnet_") && ["vault_", "loan_", "c27_"].iter().any(|pre| n.starts_with(pre))
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

fn edit_object(pre: &mut BTreeMap<Key, Vec<u8>>, key: &Key, edit: impl FnOnce(&mut Value)) -> Result<(), String> {
    let mut jv = decode(pre.get(key).ok_or("control: object not in the bundle")?);
    edit(&mut jv);
    pre.insert(*key, encode(&jv)?);
    Ok(())
}

/// What libxrpl's apply checks, shown on a transaction it accepts: a broken signature, a consumed sequence and a
/// state that breaks a vault invariant must each be refused.
fn controls(bundle: &Value) -> Result<Vec<String>, String> {
    let ledger = ledger_of(bundle);
    let mut pre = pre_of(bundle);
    rewrite_amendments(&mut pre)?;
    let tx_bytes = tx_bytes_of(bundle)?;
    let mut lines = Vec::new();
    let (ter, ..) = libxrpl_leg(&ledger, &pre, &tx_bytes)?;
    lines.push(format!("untouched: libxrpl {ter}"));

    // 1. One bit of the signature flipped.
    let sig = hex::decode(bundle["tx"]["TxnSignature"].as_str().ok_or("control: no TxnSignature")?).unwrap();
    let at = tx_bytes.windows(sig.len()).position(|w| w == sig.as_slice()).ok_or("control: signature not found")?;
    let mut bad = tx_bytes.clone();
    bad[at + sig.len() / 2] ^= 0x01;
    let (ter, ..) = libxrpl_leg(&ledger, &pre, &bad)?;
    lines.push(format!("signature bit flipped: libxrpl {ter}"));

    // 2. The account's Sequence already past the transaction's.
    let account = decode_address(bundle["tx"]["Account"].as_str().unwrap()).ok_or("control: account")?;
    let mut past = pre.clone();
    edit_object(&mut past, &keylet::account_root_key(&account).0, |jv| {
        jv["Sequence"] = Value::from(jv["Sequence"].as_u64().unwrap_or(0) + 1);
    })?;
    let (ter, ..) = libxrpl_leg(&ledger, &past, &tx_bytes)?;
    lines.push(format!("account Sequence + 1: libxrpl {ter}"));

    // 3. LastLedgerSequence already passed.
    if let Some(last) = bundle["tx"]["LastLedgerSequence"].as_u64() {
        let late = Ledger { seq: last as u32 + 1, ..ledger_of(bundle) };
        let (ter, ..) = libxrpl_leg(&late, &pre, &tx_bytes)?;
        lines.push(format!("ledger past LastLedgerSequence: libxrpl {ter}"));
    }

    // 4. A vault whose AssetsAvailable exceeds its AssetsTotal: ValidVault must refuse whatever touches it.
    if let Some(vault) = bundle["tx"]["VaultID"].as_str() {
        let mut broken = pre.clone();
        edit_object(&mut broken, &key32(vault), |jv| {
            let total: u64 = jv["AssetsTotal"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
            jv["AssetsAvailable"] = Value::String((total + 1_000_000).to_string());
        })?;
        let (ter, fatal, ..) = libxrpl_leg(&ledger, &broken, &tx_bytes)?;
        let (our_ter, ..) = our_leg(&ledger, &broken, &bundle["tx"])?;
        lines.push(format!(
            "vault AssetsAvailable > AssetsTotal: libxrpl {ter}, ours {our_ter} ({})",
            fatal.chars().take(300).collect::<String>()
        ));
    }
    Ok(lines)
}

#[test]
fn libxrpl_341_against_our_engine_on_the_mainnet_arm() {
    let version = xrpl_ffi::libxrpl_version();
    println!("D341 libxrpl version linked: {version}");
    assert!(
        version.contains("3.4.1") || std::env::var("DIFF341_ANY_VERSION").is_ok(),
        "this differential is about rippled 3.4.1; the shim links {version}"
    );
    let ids: Vec<String> = REMOVED.iter().map(|n| format!("{n}={}", &hex::encode_upper(sha512_half(n.as_bytes()).0)[..8])).collect();
    println!("D341 amendments removed from every bundle's Amendments object: {}", ids.join(" "));

    let files = bundle_files();
    assert!(!files.is_empty(), "no bundles");
    let mut tally: BTreeMap<(String, &'static str), u32> = BTreeMap::new();
    let mut arm: BTreeMap<(String, String, String), u32> = BTreeMap::new();
    let mut amendment_sets: HashMap<Vec<&'static str>, u32> = HashMap::new();
    let mut failing = Vec::new();
    let mut control_done = false;
    for path in &files {
        let name = name_of(path);
        let bundle: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        if bundle.get("pre").is_none() || bundle.get("tx").is_none() {
            continue;
        }
        if let Ok(removed) = rewrite_amendments(&mut pre_of(&bundle)) {
            *amendment_sets.entry(removed).or_default() += 1;
        }
        let row = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_bundle(&name, &bundle)))
            .unwrap_or_else(|p| Err(format!("panic: {}", p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())));
        match row {
            Ok(r) => {
                println!(
                    "D341 {} {name} {} devnet={} libxrpl={} ours={} vs-devnet={}",
                    r.class,
                    r.tx_type,
                    r.devnet,
                    r.lib_ter,
                    r.our_ter,
                    if r.vs_devnet { "same" } else { "differs" }
                );
                r.detail.iter().for_each(|l| println!("D341   {l}"));
                *tally.entry((r.tx_type.clone(), r.class)).or_default() += 1;
                if !r.vs_devnet {
                    *arm.entry((r.tx_type.clone(), r.devnet.clone(), r.lib_ter.clone())).or_default() += 1;
                }
                if matches!(r.class, "TER" | "MUT" | "BYTE") {
                    failing.push(name.clone());
                }
                if !control_done && r.class == "AGREE" && r.tx_type == "VaultDeposit" && r.lib_ter == "tesSUCCESS" {
                    control_done = true;
                    match controls(&bundle) {
                        Ok(lines) => lines.iter().for_each(|l| println!("D341 CONTROL {name}: {l}")),
                        Err(e) => println!("D341 CONTROL {name}: could not run: {e}"),
                    }
                }
            }
            Err(e) => {
                // A sponsored transaction that cannot even be set up (the codec does not carry the sponsor
                // fields) is the same known gap as one that runs and differs.
                let sponsored = bundle["tx"].get("Sponsor").is_some();
                let class = if sponsored { "SPONSOR" } else { "HARNESS" };
                let tx_type = bundle["tx"]["TransactionType"].as_str().unwrap_or("?").to_string();
                println!("D341 {class} {name} {tx_type}: could not run: {e}");
                *tally.entry((tx_type, class)).or_default() += 1;
                if !sponsored {
                    failing.push(name.clone());
                }
            }
        }
    }
    println!("D341 SUMMARY {} bundles", files.len());
    for (removed, n) in &amendment_sets {
        println!("D341 SUMMARY removed {removed:?} from {n} bundles");
    }
    for ((t, c), n) in &tally {
        println!("D341 SUMMARY {t} {c} {n}");
    }
    for ((t, d, l), n) in &arm {
        println!("D341 SUMMARY libxrpl-3.4.1-parts-from-devnet {t} devnet={d} libxrpl={l} {n}");
    }
    assert!(failing.is_empty(), "{} bundle(s) differ or could not run: {}", failing.len(), failing.join(" "));
}
