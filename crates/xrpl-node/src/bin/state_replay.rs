//! state_replay — full-ledger native replay against mainnet account_hash.
//!
//! Phase 4's capstone: load the ENTIRE verified state snapshot (19.4M
//! objects at the pinned seq), decode every blob to JSON through the
//! canonical codec, apply the next ledger's transaction set through the
//! native engine with NO truth-overlay and NO hydration crutches, write the
//! skip list, re-encode every touched node, update the state SHAMap
//! incrementally, and compare the computed root against the real ledger's
//! account_hash. A MATCH means the native engine reproduced the whole
//! ledger byte-for-byte.
//!
//! The load pass doubles as a full-state codec census: decode(blob) →
//! canon → encode must reproduce the original blob for every object in
//! the ledger (--no-census to skip).
//!
//! Usage: state_replay <snapshot_dir> <base_seq> [--count N] [--rpc URL]
//!        [--no-census] [--keep-going]
//! Exit: 0 all replayed ledgers MATCH, 1 mismatch, 2 setup error.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;
use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::{Sandbox, SandboxEntry};
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::ledger::transactor::{apply_common, TxFields, TxResult};
use xrpl_ledger::shamap::hash::{
    sha512_half, sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE,
};
use xrpl_ledger::shamap::node::{nibble_at, ZERO_HASH};
use xrpl_ledger::tx::dispatch::get_transactor;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const ACCOUNT_FIELDS: &[&str] = &["Destination", "Owner", "Authorize", "Unauthorize", "RegularKey"];

fn decode_address(addr: &str) -> Option<[u8; 20]> {
    const ALPHABET: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let mut n: Vec<u8> = vec![0];
    for ch in addr.bytes() {
        let carry = ALPHABET.iter().position(|&c| c == ch)?;
        let mut c = carry;
        for byte in n.iter_mut().rev() {
            c += (*byte as usize) * 58;
            *byte = (c & 0xFF) as u8;
            c >>= 8;
        }
        while c > 0 {
            n.insert(0, (c & 0xFF) as u8);
            c >>= 8;
        }
    }
    let leading = addr.bytes().take_while(|&b| b == b'r').count();
    let mut result = vec![0u8; leading];
    result.extend_from_slice(&n);
    if result.len() < 25 {
        return None;
    }
    let mut id = [0u8; 20];
    id.copy_from_slice(&result[1..21]);
    Some(id)
}

fn build_txfields(txjson: &Value) -> Option<TxFields> {
    // Pseudo-transactions carry Account: "" and Fee: "0" — the zero account.
    let account = match txjson["Account"].as_str()? {
        "" => [0u8; 20],
        a => decode_address(a)?,
    };
    let tx_type = txjson["TransactionType"].as_str()?.to_string();
    let fee = txjson["Fee"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let sequence = txjson["Sequence"].as_u64().unwrap_or(0) as u32;
    let ticket_seq = txjson.get("TicketSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
    let last_ledger_seq = txjson.get("LastLedgerSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
    let mut fields = txjson.clone();
    for k in ACCOUNT_FIELDS {
        if let Some(a) = fields.get(*k).and_then(|v| v.as_str()) {
            if a.starts_with('r') {
                if let Some(id) = decode_address(a) {
                    fields[*k] = json!(hex::encode(id));
                }
            }
        }
    }
    Some(TxFields { account, tx_type, fee, sequence, ticket_seq, last_ledger_seq, fields })
}

/// Native per-tx apply — identical branching to differential_probe's copy
/// (which mirrors apply.rs::apply_transaction_set). Returns (ter, mods).
fn native_apply_one(state: &LedgerState, tx: &TxFields) -> (String, HashMap<Hash256, SandboxEntry>) {
    let transactor = match get_transactor(&tx.tx_type) {
        Some(t) => t,
        None => {
            let mut sb = Sandbox::new(state);
            let r = apply_common(tx, &mut sb);
            if r.is_success() {
                return (TxResult::Unsupported.code_str().to_string(), sb.into_modifications());
            }
            return (r.code_str().to_string(), HashMap::new());
        }
    };
    if xrpl_ledger::tx::dispatch::is_pseudo(&tx.tx_type) {
        let pf = transactor.preflight(tx);
        if !pf.is_success() {
            return (pf.code_str().to_string(), HashMap::new());
        }
        let mut sb = Sandbox::new(state);
        let applied = transactor.do_apply(tx, &mut sb);
        if applied.is_success() {
            return (TxResult::Success.code_str().to_string(), sb.into_modifications());
        }
        return (applied.code_str().to_string(), HashMap::new());
    }
    let preflight = transactor.preflight(tx);
    if !preflight.is_success() {
        if preflight.is_claimed() {
            let mut sb = Sandbox::new(state);
            let common = apply_common(tx, &mut sb);
            if common.is_success() {
                return (preflight.code_str().to_string(), sb.into_modifications());
            }
            return (common.code_str().to_string(), HashMap::new());
        }
        return (preflight.code_str().to_string(), HashMap::new());
    }
    let mut sb = Sandbox::new(state);
    let preclaim = transactor.preclaim(tx, &sb);
    if !preclaim.is_success() && !preclaim.is_claimed() {
        return (preclaim.code_str().to_string(), HashMap::new());
    }
    if !preclaim.is_success() {
        let common = apply_common(tx, &mut sb);
        if common.is_success() {
            return (preclaim.code_str().to_string(), sb.into_modifications());
        }
        return (common.code_str().to_string(), HashMap::new());
    }
    let common = apply_common(tx, &mut sb);
    if !common.is_success() {
        return (common.code_str().to_string(), HashMap::new());
    }
    let snap = sb.snapshot();
    let applied = transactor.do_apply(tx, &mut sb);
    if applied.is_success() {
        // Success-only (Transactor.cpp:660; tec rolls the stamp back).
        xrpl_ledger::ledger::transactor::stamp_account_txn_id(tx, &mut sb);
        (TxResult::Success.code_str().to_string(), sb.into_modifications())
    } else if applied.is_claimed() {
        if applied != TxResult::Killed {
            sb.restore_snapshot(snap);
        }
        (applied.code_str().to_string(), sb.into_modifications())
    } else {
        (applied.code_str().to_string(), HashMap::new())
    }
}

/// Re-spell engine-internal JSON into the canonical forms the binary codec
/// demands (same table as differential_probe's byte census).
fn canon_for_encode(v: &mut Value) {
    const U64_HEX: &[&str] = &[
        "OwnerNode", "BookNode", "LowNode", "HighNode", "DestinationNode",
        "IndexNext", "IndexPrevious", "XChainClaimID", "XChainAccountCreateCount",
        "XChainAccountClaimCount", "ReferenceCount", "NFTokenOfferNode", "IssuerNode",
        "AssetPrice",
    ];
    const U64_DEC: &[&str] = &["MaximumAmount", "OutstandingAmount", "MPTAmount", "LockedAmount"];
    const ACCTS: &[&str] = &[
        "Account", "Owner", "Destination", "Issuer", "RegularKey", "Authorize",
        "Unauthorize", "NFTokenMinter", "Holder", "OtherChainSource",
        "AttestationSignerAccount", "AttestationRewardAccount", "LockingChainDoor",
        "IssuingChainDoor", "issuer",
    ];
    match v {
        Value::Array(a) => {
            for e in a {
                canon_for_encode(e);
            }
        }
        Value::Object(o) => {
            for (name, val) in o.iter_mut() {
                if U64_HEX.contains(&name.as_str()) {
                    let n = val.as_u64().or_else(|| {
                        val.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())
                    });
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if U64_DEC.contains(&name.as_str()) {
                    let n = val
                        .as_u64()
                        .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()));
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if ACCTS.contains(&name.as_str()) {
                    if let Some(s) = val.as_str() {
                        if s.len() == 40 {
                            if let Ok(b) = hex::decode(s) {
                                if let Ok(arr) = <[u8; 20]>::try_from(b.as_slice()) {
                                    *val = Value::String(
                                        xrpl_core::AccountId::from_bytes(arr).to_address(),
                                    );
                                }
                            }
                        }
                    }
                } else {
                    canon_for_encode(val);
                }
            }
        }
        _ => {}
    }
}

/// Recursively rewrite any base58 classic-address string (`r…`) to 20-byte
/// hex — the native engine's account-field convention (same pass the probe
/// applies to every hydrated object).
fn hexify_addresses(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.starts_with('r') && s.len() >= 25 && s.len() <= 40 {
                if let Some(id) = decode_address(s) {
                    *v = json!(hex::encode(id));
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(hexify_addresses),
        Value::Object(m) => m.values_mut().for_each(hexify_addresses),
        _ => {}
    }
}

fn leaf_hash(key: &Hash256, blob: &[u8]) -> Hash256 {
    let mut buf = Vec::with_capacity(blob.len() + 32);
    buf.extend_from_slice(blob);
    buf.extend_from_slice(&key.0);
    sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
}

/// Bottom-up SHAMap root from key-sorted (key, leaf_hash) entries — the
/// proven fold from verify_bottomup (verified against insert_hash_only).
fn compute_subtree_hash(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
    if entries.is_empty() {
        return ZERO_HASH;
    }
    if entries.len() == 1 {
        return entries[0].1;
    }
    let mut child_hashes = [ZERO_HASH; 16];
    let mut start = 0;
    for nibble in 0..16u8 {
        let end =
            entries[start..].partition_point(|&(key, _)| nibble_at(&key, depth) <= nibble) + start;
        let bucket_start =
            entries[start..end].partition_point(|&(key, _)| nibble_at(&key, depth) < nibble) + start;
        if bucket_start < end {
            child_hashes[nibble as usize] =
                compute_subtree_hash(&entries[bucket_start..end], depth + 1);
        }
        start = end;
    }
    let mut data = [0u8; 16 * 32];
    for (i, h) in child_hashes.iter().enumerate() {
        data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
    }
    sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
}

/// Root hash: parallel fold over the 16 depth-0 buckets.
fn fold_root(entries: &[(Hash256, Hash256)]) -> Hash256 {
    if entries.is_empty() {
        return ZERO_HASH;
    }
    let mut bounds = [0usize; 17];
    for nibble in 0..16u8 {
        bounds[nibble as usize + 1] =
            entries.partition_point(|&(key, _)| nibble_at(&key, 0) <= nibble);
    }
    let child_hashes: Vec<Hash256> = (0..16usize)
        .into_par_iter()
        .map(|n| compute_subtree_hash(&entries[bounds[n]..bounds[n + 1]], 1))
        .collect();
    let mut data = [0u8; 16 * 32];
    for (i, h) in child_hashes.iter().enumerate() {
        data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
    }
    sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
}

/// keylet::skip(seq): the every-65536-block LedgerHashes entry —
/// SHA512Half(0x0073 ‖ u32be(seq >> 16)) (rippled Indexes.cpp).
fn skip_every_key(seq: u32) -> Hash256 {
    let mut buf = Vec::with_capacity(6);
    buf.extend_from_slice(&[0x00, 0x73]);
    buf.extend_from_slice(&(seq >> 16).to_be_bytes());
    sha512_half(&buf)
}

/// Ledger::updateSkipList, on the JSON state: at close of ledger `target`,
/// push hash(target-1) into the rolling 256-entry list (trim front at 256),
/// and — when (target-1) & 0xff == 0 — append it to the every-256th entry
/// for the 65536-block too (no trim; it holds exactly 256 when full).
fn update_skip_list(
    state: &mut LedgerState,
    dirty: &mut HashSet<Hash256>,
    target: u32,
    parent_hash_hex: &str,
) {
    let prev = target - 1;
    let mut write = |key: Hash256, trim: bool| {
        let mut obj = state
            .state_map
            .lookup(&key)
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
            .unwrap_or_else(|| {
                json!({
                    "LedgerEntryType": "LedgerHashes",
                    "Flags": 0,
                    "Hashes": [],
                    "index": hex::encode_upper(key.0),
                })
            });
        let hashes = obj["Hashes"].as_array().cloned().unwrap_or_default();
        let mut hashes = hashes;
        if trim && hashes.len() == 256 {
            hashes.remove(0);
        }
        hashes.push(json!(parent_hash_hex.to_uppercase()));
        obj["Hashes"] = json!(hashes);
        obj["LastLedgerSequence"] = json!(prev);
        let _ = state.state_map.insert(key, serde_json::to_vec(&obj).unwrap_or_default());
        dirty.insert(key);
    };
    if prev & 0xff == 0 {
        write(skip_every_key(prev), false);
    }
    write(keylet::skip_list_key(), true);
}

fn rpc_call(client: &reqwest::blocking::Client, rpc: &str, body: Value) -> anyhow::Result<Value> {
    Ok(client.post(rpc).json(&body).send()?.json()?)
}

fn fetch_ledger(
    client: &reqwest::blocking::Client,
    rpc: &str,
    seq: u32,
    with_txs: bool,
) -> anyhow::Result<Value> {
    let mut params = json!({"ledger_index": seq});
    if with_txs {
        params["transactions"] = json!(true);
        params["expand"] = json!(true);
    }
    let resp = rpc_call(client, rpc, json!({"method": "ledger", "params": [params]}))?;
    let lgr = resp["result"]["ledger"].clone();
    if !lgr.is_object() {
        anyhow::bail!("no ledger {seq} from {rpc}: {}", resp["result"]["error"].as_str().unwrap_or("?"));
    }
    Ok(lgr)
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: state_replay <snapshot_dir> <base_seq> [--count N] [--rpc URL] [--no-census] [--keep-going]");
        return 2;
    }
    let dir = args[1].clone();
    let base: u32 = match args[2].parse() {
        Ok(s) => s,
        Err(_) => {
            eprintln!("bad base_seq {}", args[2]);
            return 2;
        }
    };
    let getopt = |name: &str| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let count: u32 = getopt("--count").and_then(|s| s.parse().ok()).unwrap_or(1);
    let rpc = getopt("--rpc").unwrap_or_else(|| "http://localhost:5005".to_string());
    let census = !args.iter().any(|a| a == "--no-census");
    let keep_going = args.iter().any(|a| a == "--keep-going");
    let diff = args.iter().any(|a| a == "--diff");

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("http client: {e}");
            return 2;
        }
    };

    // ---- expected root at base (load sanity) ----
    let base_lgr = match fetch_ledger(&client, &rpc, base, false) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fetch base ledger: {e}");
            return 2;
        }
    };
    let base_ah = base_lgr["account_hash"].as_str().unwrap_or("").to_uppercase();
    eprintln!("Base #{base} account_hash {base_ah}");

    // ---- load snapshot: binary tree (hashing) + JSON state (engine) ----
    let mut files: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect(),
        Err(e) => {
            eprintln!("read_dir {dir}: {e}");
            return 2;
        }
    };
    files.sort();
    eprintln!("{} batch files in {dir}; loading (census={census}) …", files.len());

    let start = std::time::Instant::now();
    // Sorted (key, leaf_hash) entries — the hashing substrate. A full SHAMap
    // of 19.4M leaves costs ~15GB of inner-node overhead; the bottom-up fold
    // over this 1.2GB vec computes the identical root (verify_bottomup).
    let mut entries: Vec<(Hash256, Hash256)> = Vec::with_capacity(20_000_000);
    let header0 = LedgerHeader {
        sequence: base,
        total_coins: 0,
        parent_hash: Hash256([0; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: 0,
        close_time: 0,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header0);
    let mut n_objects: u64 = 0;
    let mut n_decode_err: u64 = 0;
    let mut n_census_bad: u64 = 0;
    let mut census_samples: Vec<String> = Vec::new();
    let mut decode_samples: Vec<String> = Vec::new();

    for chunk in files.chunks(64) {
        // (key, leaf, json_bytes, decode_err, census_bad_desc)
        type Loaded = (Hash256, Hash256, Option<Vec<u8>>, bool, Option<String>);
        let batch: Vec<Vec<Loaded>> = chunk
            .par_iter()
            .map(|path| {
                let mut out: Vec<Loaded> = Vec::new();
                let Ok(text) = std::fs::read_to_string(path) else { return out };
                let Ok(arr) = serde_json::from_str::<Vec<Value>>(&text) else { return out };
                for obj in &arr {
                    let (Some(idx), Some(dh)) = (obj["index"].as_str(), obj["data"].as_str())
                    else {
                        continue;
                    };
                    let (Ok(kb), Ok(blob)) = (hex::decode(idx), hex::decode(dh)) else { continue };
                    let Ok(karr) = <[u8; 32]>::try_from(kb.as_slice()) else { continue };
                    let key = Hash256(karr);
                    let leaf = leaf_hash(&key, &blob);
                    match xrpl_core::codec::decode::decode_transaction_binary(&blob) {
                        Ok(mut j) => {
                            let mut bad = None;
                            if census {
                                let mut c = j.clone();
                                canon_for_encode(&mut c);
                                match xrpl_core::codec::encode::encode_transaction_json(&c, false) {
                                    Ok(re) if re == blob => {}
                                    Ok(_) => {
                                        bad = Some(format!(
                                            "{} {} re-encode differs",
                                            &idx[..16.min(idx.len())],
                                            j["LedgerEntryType"].as_str().unwrap_or("?")
                                        ))
                                    }
                                    Err(e) => {
                                        bad = Some(format!(
                                            "{} {} encode err: {e}",
                                            &idx[..16.min(idx.len())],
                                            j["LedgerEntryType"].as_str().unwrap_or("?")
                                        ))
                                    }
                                }
                            }
                            hexify_addresses(&mut j);
                            j["index"] = json!(idx.to_uppercase());
                            out.push((key, leaf, serde_json::to_vec(&j).ok(), false, bad));
                        }
                        Err(e) => out.push((
                            key,
                            leaf,
                            None,
                            true,
                            Some(format!("{idx} tybyte {} len {}: {e}", &dh[..8.min(dh.len())], blob.len())),
                        )),
                    }
                }
                out
            })
            .collect();
        for filevec in batch {
            for (key, leaf, jb, derr, bad) in filevec {
                entries.push((key, leaf));
                n_objects += 1;
                if derr {
                    n_decode_err += 1;
                    if let Some(b) = bad {
                        if decode_samples.len() < 20 {
                            decode_samples.push(b);
                        }
                    }
                } else if let Some(b) = bad {
                    n_census_bad += 1;
                    if census_samples.len() < 10 {
                        census_samples.push(b);
                    }
                }
                if let Some(jb) = jb {
                    let _ = state.state_map.insert(key, jb);
                }
            }
        }
        eprint!("\r  {} objects, {:.0}s", n_objects, start.elapsed().as_secs_f64());
    }
    eprintln!();
    eprintln!(
        "Loaded {} objects in {:.0}s; decode-errors {}, census-mismatches {}",
        n_objects,
        start.elapsed().as_secs_f64(),
        n_decode_err,
        n_census_bad
    );
    for s in &census_samples {
        eprintln!("  CENSUS {s}");
    }
    for s in &decode_samples {
        eprintln!("  DECODE-ERR {s}");
    }
    if n_decode_err > 0 {
        eprintln!("aborting: {} blobs failed to decode — engine state would be incomplete", n_decode_err);
        return 2;
    }
    if census && n_census_bad > 0 {
        eprintln!("aborting: {} census mismatches — re-encode would corrupt untouched nodes", n_census_bad);
        return 2;
    }
    entries.par_sort_unstable_by(|a, b| a.0 .0.cmp(&b.0 .0));
    let root0 = hex::encode_upper(fold_root(&entries).0);
    if root0 != base_ah {
        eprintln!("BASE ROOT MISMATCH: computed {root0} expected {base_ah}");
        return 2;
    }
    eprintln!("BASE ROOT MATCH {root0}  ({:.0}s total)", start.elapsed().as_secs_f64());

    // ---- replay ledgers base+1 ..= base+count ----
    let mut any_mismatch = false;
    for target in base + 1..=base + count {
        let lgr = match fetch_ledger(&client, &rpc, target, true) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("fetch ledger {target}: {e}");
                return 2;
            }
        };
        let expected_ah = lgr["account_hash"].as_str().unwrap_or("").to_uppercase();
        let parent_hash_hex = lgr["parent_hash"].as_str().unwrap_or("").to_uppercase();
        let parent_hash = hex::decode(&parent_hash_hex)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .map(Hash256)
            .unwrap_or(Hash256([0; 32]));
        let parent_close = lgr["parent_close_time"].as_u64().unwrap_or(0) as u32;
        let total_coins = lgr["total_coins"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000_000_000_000_000);
        state.header = LedgerHeader {
            sequence: target - 1,
            total_coins,
            parent_hash,
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: parent_close,
            close_time: parent_close,
            close_time_resolution: 10,
            close_flags: 0,
        };

        let mut dirty: HashSet<Hash256> = HashSet::new();

        // Flag-ledger open: rotate NegativeUNL pending fields (ledger-level,
        // outside every tx meta).
        if target % 256 == 0 {
            let nk = keylet::negative_unl_key();
            if let Some(bytes) = state.state_map.lookup(&nk).map(|b| b.to_vec()) {
                match xrpl_ledger::tx::pseudo::rotate_negative_unl(&bytes, target) {
                    Some(Some(nb)) => {
                        let _ = state.state_map.insert(nk, nb);
                        dirty.insert(nk);
                    }
                    Some(None) => {
                        let _ = state.state_map.delete(&nk);
                        dirty.insert(nk);
                    }
                    None => {}
                }
            }
        }

        let mut txs: Vec<Value> = lgr["transactions"].as_array().cloned().unwrap_or_default();
        txs.sort_by_key(|t| t["metaData"]["TransactionIndex"].as_u64().unwrap_or(u64::MAX));
        let n_txs = txs.len();
        let mut ter_mismatch = 0usize;
        let mut fatal_skip = 0usize;

        for tx in &txs {
            let h = tx["hash"].as_str().unwrap_or("").to_string();
            let expected_ter =
                tx["metaData"]["TransactionResult"].as_str().unwrap_or("?").to_string();
            let tx_type = tx["TransactionType"].as_str().unwrap_or("?").to_string();
            if std::env::var("DX_PAY").is_ok() || std::env::var("DX_AMM").is_ok() {
                eprintln!("DX_TX {h} {tx_type}");
            }
            let Some(txf) = build_txfields(tx) else {
                eprintln!("  FATAL-SKIP {h} {tx_type}: build_txfields failed");
                fatal_skip += 1;
                continue;
            };
            // DX_REPLAY_TX=<hash prefix> + DX_REPLAY_SET=<comma list of DX
            // vars>: arm the listed receipt envs for exactly the matching
            // tx — the only way to get engine receipts out of a 200-ledger
            // replay without drowning in every other tx's output. The apply
            // loop is single-threaded, so set/remove around the one call is
            // sound. (#106455229 7D1380A7: a replay-only ulp overdrain no
            // per-tx leg reproduces.)
            let dx_armed = std::env::var("DX_REPLAY_TX")
                .map(|p| !p.is_empty() && h.starts_with(&p.to_uppercase()))
                .unwrap_or(false);
            if dx_armed {
                eprintln!("DX_REPLAY armed for {h} {tx_type}");
                if let Ok(list) = std::env::var("DX_REPLAY_SET") {
                    for v in list.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                        std::env::set_var(v, "1");
                    }
                }
            }
            let (our_ter, mut mods) = native_apply_one(&state, &txf);
            if dx_armed {
                if let Ok(list) = std::env::var("DX_REPLAY_SET") {
                    for v in list.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                        std::env::remove_var(v);
                    }
                }
            }
            xrpl_ledger::ledger::threading::stamp_threading(
                &mut mods,
                &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
                &h,
                target,
            );
            if our_ter != expected_ter {
                eprintln!("  TER {h} {tx_type}: ours {our_ter} mainnet {expected_ter}");
                ter_mismatch += 1;
            }
            for (k, ent) in mods {
                // DX_WATCH=<hex key prefix>: print the node's Balance after
                // every tx that writes it (uppercase hex prefix match).
                if let Ok(w) = std::env::var("DX_WATCH") {
                    if !w.is_empty() && hex::encode_upper(k.0).starts_with(&w.to_uppercase()) {
                        let bal = match &ent {
                            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                                serde_json::from_slice::<Value>(b).ok().map(|v| {
                                    // Balance for lines/roots; the two amount
                                    // sides for offers.
                                    if v["Balance"].is_null() {
                                        serde_json::json!({
                                            "TakerGets": v["TakerGets"].clone(),
                                            "TakerPays": v["TakerPays"].clone(),
                                        })
                                    } else {
                                        v["Balance"].clone()
                                    }
                                })
                            }
                            SandboxEntry::Deleted => None,
                        };
                        eprintln!("DX_WATCH {h} {tx_type} -> {:?}", bal);
                    }
                }
                match ent {
                    SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                        let _ = state.state_map.insert(k, b);
                    }
                    SandboxEntry::Deleted => {
                        let _ = state.state_map.delete(&k);
                    }
                }
                dirty.insert(k);
            }
        }

        update_skip_list(&mut state, &mut dirty, target, &parent_hash_hex);

        // Re-encode every dirty node and patch the sorted entries vec.
        let mut encode_err = 0usize;
        for k in &dirty {
            let pos = entries.binary_search_by(|e| e.0 .0.cmp(&k.0));
            match state.state_map.lookup(k).map(|b| b.to_vec()) {
                Some(jb) => match serde_json::from_slice::<Value>(&jb) {
                    Ok(mut v) => {
                        canon_for_encode(&mut v);
                        match xrpl_core::codec::encode::encode_transaction_json(&v, false) {
                            Ok(blob) => {
                                let leaf = leaf_hash(k, &blob);
                                match pos {
                                    Ok(i) => entries[i].1 = leaf,
                                    Err(i) => entries.insert(i, (*k, leaf)),
                                }
                                // Re-store the node as decode(encode(json)) —
                                // the same pipeline hydration uses. rippled's
                                // persisted state is CANONICAL: an STAmount
                                // never carries more than 16 digits into the
                                // next ledger, while our sandbox JSON can (the
                                // serialized bytes round; the JSON keeps the
                                // tail, and it compounds across ledgers).
                                // #106455229 7D1380A7: a full-balance drain
                                // against a carried wide balance left sub-ulp
                                // dust where mainnet writes canonical zero —
                                // per-tx probes hydrate canonical values and
                                // were structurally blind to it.
                                if let Ok(mut cj) =
                                    xrpl_core::codec::decode::decode_transaction_binary(&blob)
                                {
                                    hexify_addresses(&mut cj);
                                    if let Ok(cb) = serde_json::to_vec(&cj) {
                                        let _ = state.state_map.insert(*k, cb);
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "  ENCODE-ERR {} {}: {e}",
                                    hex::encode_upper(&k.0[..8]),
                                    v["LedgerEntryType"].as_str().unwrap_or("?")
                                );
                                encode_err += 1;
                            }
                        }
                    }
                    Err(_) => encode_err += 1,
                },
                None => {
                    if let Ok(i) = pos {
                        entries.remove(i);
                    }
                }
            }
        }

        let root = hex::encode_upper(fold_root(&entries).0);
        let ok = root == expected_ah && fatal_skip == 0 && encode_err == 0;
        if diff && root != expected_ah {
            // Two-sided node diff against the true post-state.
            let mut meta_keys: HashSet<String> = HashSet::new();
            for tx in &txs {
                for node in tx["metaData"]["AffectedNodes"].as_array().into_iter().flatten() {
                    for kind in ["CreatedNode", "ModifiedNode", "DeletedNode"] {
                        if let Some(li) = node[kind]["LedgerIndex"].as_str() {
                            meta_keys.insert(li.to_uppercase());
                        }
                    }
                }
            }
            for kh in &meta_keys {
                if let Ok(kb) = hex::decode(kh) {
                    if let Ok(karr) = <[u8; 32]>::try_from(kb.as_slice()) {
                        if !dirty.contains(&Hash256(karr)) {
                            eprintln!("  DIFF MISSING-WRITE {kh} (in meta, not dirty)");
                        }
                    }
                }
            }
            for k in &dirty {
                let kh = hex::encode_upper(k.0);
                let truth = rpc_call(
                    &client,
                    &rpc,
                    json!({"method":"ledger_entry","params":[{"index": kh, "ledger_index": target, "binary": true}]}),
                )
                .ok()
                .and_then(|r| r["result"]["node_binary"].as_str().map(|s| s.to_string()));
                let ours = state.state_map.lookup(k).map(|b| b.to_vec()).and_then(|jb| {
                    let mut v = serde_json::from_slice::<Value>(&jb).ok()?;
                    canon_for_encode(&mut v);
                    xrpl_core::codec::encode::encode_transaction_json(&v, false).ok()
                });
                match (ours, truth) {
                    (Some(o), Some(t)) => {
                        let th = t.to_uppercase();
                        let oh = hex::encode_upper(&o);
                        if oh != th {
                            let ty = state
                                .state_map
                                .lookup(k)
                                .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                                .and_then(|v| v["LedgerEntryType"].as_str().map(|s| s.to_string()))
                                .unwrap_or_default();
                            let off = oh
                                .as_bytes()
                                .iter()
                                .zip(th.as_bytes())
                                .position(|(a, b)| a != b)
                                .unwrap_or(oh.len().min(th.len()));
                            eprintln!("  DIFF BYTES {kh} {ty} first-diff@{off} ours-len {} true-len {}", oh.len(), th.len());
                            let lo = off.saturating_sub(24);
                            eprintln!("    ours …{}…", &oh[lo..(off + 40).min(oh.len())]);
                            eprintln!("    true …{}…", &th[lo..(off + 40).min(th.len())]);
                        }
                    }
                    (Some(_), None) => eprintln!("  DIFF PHANTOM {kh} (ours exists, truth absent)"),
                    (None, Some(_)) => eprintln!("  DIFF MISSING {kh} (truth exists, ours deleted)"),
                    (None, None) => {}
                }
            }
        }
        println!(
            "REPLAY #{target} {} ({} txs, {} dirty, ter-mismatch {}, fatal-skip {}, encode-err {})",
            if ok { "MATCH" } else { "MISMATCH" },
            n_txs,
            dirty.len(),
            ter_mismatch,
            fatal_skip,
            encode_err
        );
        if !ok {
            println!("  computed {root}");
            println!("  expected {expected_ah}");
            any_mismatch = true;
            if !keep_going {
                break;
            }
        }
    }
    if any_mismatch {
        1
    } else {
        0
    }
}
