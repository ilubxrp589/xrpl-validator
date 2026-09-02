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
//! Leaves hold RAW blobs (decoded lazily on read through
//! `LedgerState::leaf_decoder`), so a full mainnet state fits in ~10 GB.
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
use xrpl_node::native_apply::{
    build_txfields, canon_for_encode, decode_address, hexify_addresses, native_apply_one,
    update_skip_list,
};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;






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
/// Lazy leaf decoder installed on the replay state: raw blob → engine JSON
/// (hex account ids, `index` = the key) exactly as the eager loader used to
/// store. A decode failure is loud — it would otherwise read as "no object".
fn decode_leaf(key: &Hash256, blob: &[u8]) -> Option<Vec<u8>> {
    match xrpl_core::codec::decode::decode_transaction_binary(blob) {
        Ok(mut j) => {
            hexify_addresses(&mut j);
            j["index"] = json!(hex::encode_upper(key.0));
            serde_json::to_vec(&j).ok()
        }
        Err(e) => {
            eprintln!("LAZY DECODE FAIL {} len {}: {e}", hex::encode_upper(key.0), blob.len());
            None
        }
    }
}

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
    state.leaf_decoder = Some(decode_leaf);
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
                    // Leaves hold the RAW blob; the engine decodes on read via
                    // `LedgerState::leaf_decoder` (decode_leaf). Only the census
                    // decodes at load, and drops the JSON immediately.
                    if !census {
                        out.push((key, leaf, Some(blob), false, None));
                        continue;
                    }
                    match xrpl_core::codec::decode::decode_transaction_binary(&blob) {
                        Ok(j) => {
                            let mut bad = None;
                            {
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
                            out.push((key, leaf, Some(blob), false, bad));
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
            if let Some(bytes) = state.read_json(&nk) {
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

        // XRPL_REPLAY_TIMING=1: per-ledger phase timing on its own line (the
        // Stage 4 perf gate — raw native-apply cost with no RPC in the path).
        // Separate line, not REPLAY-line fields: the gates grep those lines.
        let timing_on = std::env::var("XRPL_REPLAY_TIMING").is_ok();
        let t_apply0 = std::time::Instant::now();

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
                &|k| state.read_json(k),
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
                    // Comma list of uppercase hex key prefixes.
                    if w.split(',').any(|p| {
                        let p = p.trim();
                        !p.is_empty() && hex::encode_upper(k.0).starts_with(&p.to_uppercase())
                    }) {
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

        let apply_ms = t_apply0.elapsed().as_secs_f64() * 1000.0;
        update_skip_list(&mut state, &mut dirty, target, &parent_hash_hex);
        let t_hash0 = std::time::Instant::now();

        // Re-encode every dirty node and patch the sorted entries vec.
        let mut encode_err = 0usize;
        for k in &dirty {
            let pos = entries.binary_search_by(|e| e.0 .0.cmp(&k.0));
            match state.read_json(k) {
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
                                // Re-store the node as its CANONICAL bytes;
                                // reads decode them lazily (decode_leaf), the
                                // same pipeline hydration uses. rippled's
                                // persisted state is canonical: an STAmount
                                // never carries more than 16 digits into the
                                // next ledger, while our sandbox JSON can (the
                                // serialized bytes round; the JSON keeps the
                                // tail, and it compounds across ledgers).
                                // #106455229 7D1380A7: a full-balance drain
                                // against a carried wide balance left sub-ulp
                                // dust where mainnet writes canonical zero —
                                // per-tx probes hydrate canonical values and
                                // were structurally blind to it.
                                let _ = state.state_map.insert(*k, blob);
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
        if timing_on {
            let hash_ms = t_hash0.elapsed().as_secs_f64() * 1000.0;
            println!(
                "TIMING #{target} txs={n_txs} apply={apply_ms:.1}ms encode+fold={hash_ms:.1}ms total={:.1}ms",
                apply_ms + hash_ms
            );
        }
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
                let ours = state.read_json(k).and_then(|jb| {
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
                                .read_json(k)
                                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
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
