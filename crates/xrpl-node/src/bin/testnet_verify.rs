//! Full pipeline test on TESTNET — download all state, verify hash,
//! apply rounds, check incremental vs rebuild.
//! Testnet has ~200k objects (vs 18.7M mainnet) so this runs in seconds.

use std::collections::HashSet;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

const RPC: &str = "https://s.altnet.rippletest.net:51234"; // Public testnet — might rate limit but OK for testing

fn leaf_hash(key: &[u8; 32], data: &[u8]) -> Hash256 {
    let mut buf = Vec::with_capacity(data.len() + 32);
    buf.extend_from_slice(data);
    buf.extend_from_slice(key);
    sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
}

fn apply_round(client: &reqwest::blocking::Client, seq: u32, db: &rocksdb::DB, live_map: &mut SHAMap) -> (u32, u32, u32) {
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":seq,"transactions":true,"expand":true,"binary":false}]}))
        .send().unwrap().json().unwrap();
    let txs = resp["result"]["ledger"]["transactions"].as_array().unwrap();
    let mut modified: HashSet<String> = HashSet::new();
    let mut deleted: HashSet<String> = HashSet::new();
    for tx in txs {
        let meta = if tx["metaData"].is_object() { &tx["metaData"] } else if tx["meta"].is_object() { &tx["meta"] } else { continue; };
        if let Some(nodes) = meta["AffectedNodes"].as_array() {
            for node in nodes {
                if let Some(c) = node.get("CreatedNode") { if let Some(i) = c["LedgerIndex"].as_str() { modified.insert(i.to_string()); deleted.remove(i); } }
                if let Some(m) = node.get("ModifiedNode") { if let Some(i) = m["LedgerIndex"].as_str() { if !deleted.contains(i) { modified.insert(i.to_string()); } } }
                if let Some(d) = node.get("DeletedNode") { if let Some(i) = d["LedgerIndex"].as_str() { deleted.insert(i.to_string()); modified.remove(i); } }
            }
        }
    }
    // Add singletons
    modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string());
    modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string());
    modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string());

    let mut fetched = 0u32;
    let mut failed = 0u32;
    for idx in &modified {
        let r: serde_json::Value = loop {
            match client.post(RPC)
                .json(&serde_json::json!({"method":"ledger_entry","params":[{"index":idx,"binary":true,"ledger_index":seq}]}))
                .send() {
                Ok(resp) => match resp.json() { Ok(v) => break v, Err(_) => { std::thread::sleep(std::time::Duration::from_secs(1)); continue; } },
                Err(_) => { std::thread::sleep(std::time::Duration::from_secs(1)); continue; }
            }
        };
        if let Some(d) = r["result"]["node_binary"].as_str() {
            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(d)) {
                if k.len() == 32 {
                    db.put(&k, &v).unwrap();
                    let mut ka = [0u8; 32]; ka.copy_from_slice(&k);
                    let _ = live_map.insert_hash_only(Hash256(ka), leaf_hash(&ka, &v));
                    fetched += 1;
                }
            }
        } else {
            let err = r["result"]["error"].as_str().unwrap_or("?");
            if err == "entryNotFound" {
                // Object doesn't exist on testnet — skip
            } else {
                eprintln!("  FETCH FAIL: {}... err={err}", &idx[..16]);
                failed += 1;
            }
        }
    }
    for idx in &deleted {
        if let Ok(k) = hex::decode(idx) {
            if k.len() == 32 {
                db.delete(&k).unwrap();
                let mut ka = [0u8; 32]; ka.copy_from_slice(&k);
                let _ = live_map.delete(&Hash256(ka));
            }
        }
    }
    (fetched, deleted.len() as u32, failed)
}

fn build_fresh(db: &rocksdb::DB) -> String {
    let mut map = SHAMap::new(TreeType::State);
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32]; k.copy_from_slice(&key);
                let _ = map.insert_hash_only(Hash256(k), leaf_hash(&k, &value));
            }
        }
    }
    hex::encode(map.root_hash().0)
}

fn main() {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30)).build().unwrap();

    // Get validated ledger
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let exp0 = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?").to_string();
    eprintln!("=== TESTNET #{seq} ===");
    eprintln!("Expected hash: {}", &exp0[..16]);

    // Download ALL state
    let tmp = "/tmp/testnet_verify_db";
    let _ = std::fs::remove_dir_all(tmp);
    let db = rocksdb::DB::open_default(tmp).unwrap();
    let mut marker: Option<String> = None;
    let mut count = 0u64;
    let start = std::time::Instant::now();
    loop {
        let mut p = serde_json::json!({"ledger_index":seq,"binary":true,"limit":2048});
        if let Some(ref m) = marker { p["marker"] = serde_json::Value::String(m.clone()); }
        let r: serde_json::Value = loop {
            match client.post(RPC)
                .json(&serde_json::json!({"method":"ledger_data","params":[p]}))
                .send()
            {
                Ok(resp) => {
                    if resp.status() == 429 {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        continue;
                    }
                    match resp.json() {
                        Ok(v) => break v,
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            continue;
                        }
                    }
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    continue;
                }
            }
        };
        if let Some(st) = r["result"]["state"].as_array() {
            for o in st {
                if let (Some(i), Some(d)) = (o["index"].as_str(), o["data"].as_str()) {
                    if let (Ok(k), Ok(v)) = (hex::decode(i), hex::decode(d)) {
                        if k.len() == 32 { db.put(&k, &v).unwrap(); count += 1; }
                    }
                }
            }
        }
        marker = r["result"]["marker"].as_str().map(String::from);
        if marker.is_none() { break; }
        if count % 50_000 < 2048 { eprintln!("  {count} objects..."); }
    }
    let dl_time = start.elapsed().as_secs_f64();
    eprintln!("Downloaded {count} objects in {dl_time:.1}s");

    // Build SHAMap + verify
    let mut live_map = SHAMap::new(TreeType::State);
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32]; k.copy_from_slice(&key);
                let _ = live_map.insert_hash_only(Hash256(k), leaf_hash(&k, &value));
            }
        }
    }
    let h0 = hex::encode(live_map.root_hash().0);
    let m0 = if h0.to_uppercase() == exp0.to_uppercase() { "MATCH" } else { "MISMATCH" };
    eprintln!("Initial: {m0} ({count} entries, {:.1}s)", start.elapsed().as_secs_f64());

    // Apply 20 rounds
    for round in 1..=20 {
        let next = seq + round;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let r: serde_json::Value = client.post(RPC)
                .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]}))
                .send().unwrap().json().unwrap();
            if r["result"]["ledger"]["account_hash"].is_string() { break; }
        }
        let r: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]}))
            .send().unwrap().json().unwrap();
        let exp = r["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");

        let (f, d, e) = apply_round(&client, next, &db, &mut live_map);
        let inc = hex::encode(live_map.root_hash().0);
        let reb = build_fresh(&db);
        let inc_m = if inc.to_uppercase() == exp.to_uppercase() { "MATCH" } else { "MISMATCH" };
        let reb_m = if reb.to_uppercase() == exp.to_uppercase() { "MATCH" } else { "MISMATCH" };

        eprintln!("Round {round:2} (#{next}): inc={inc_m} reb={reb_m} — {f}f {d}d {e}e  inc==reb:{}", inc == reb);
        if inc_m == "MISMATCH" && reb_m == "MATCH" {
            eprintln!("  >>> SHAMap incremental bug detected <<<");
            eprintln!("  Expected: {}", &exp[..32]);
            eprintln!("  Inc:      {}", &inc[..32]);
            eprintln!("  Rebuild:  {}", &reb[..32]);
        }
        if reb_m == "MISMATCH" {
            eprintln!("  >>> DATA bug — rebuild doesn't match <<<");
            eprintln!("  Expected: {}", &exp[..32]);
            eprintln!("  Rebuild:  {}", &reb[..32]);
        }
    }

    drop(db);
    let _ = std::fs::remove_dir_all(tmp);
    eprintln!("\n=== DONE ===");
}
