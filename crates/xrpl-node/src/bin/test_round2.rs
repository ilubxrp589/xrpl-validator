//! Targeted test: download at ledger X, apply X+1 (MATCH), apply X+2, compare.
use std::collections::HashSet;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
const RPC: &str = "http://10.0.0.39:5005";

fn leaf_hash(key: &[u8; 32], data: &[u8]) -> Hash256 {
    let mut buf = Vec::with_capacity(data.len() + 32);
    buf.extend_from_slice(data);
    buf.extend_from_slice(key);
    sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
}

fn build_fresh(db: &rocksdb::DB) -> (String, u64) {
    let mut map = SHAMap::new(TreeType::State);
    let mut count = 0u64;
    for item in db.iterator(rocksdb::IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32]; k.copy_from_slice(&key);
                let _ = map.insert_hash_only(Hash256(k), leaf_hash(&k, &value));
                count += 1;
            }
        }
    }
    (hex::encode(map.root_hash().0), count)
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
                if let Some(c) = node.get("CreatedNode") { if let Some(i) = c["LedgerIndex"].as_str() { modified.insert(i.to_string()); } }
                if let Some(m) = node.get("ModifiedNode") { if let Some(i) = m["LedgerIndex"].as_str() { modified.insert(i.to_string()); } }
                if let Some(d) = node.get("DeletedNode") { if let Some(i) = d["LedgerIndex"].as_str() { deleted.insert(i.to_string()); modified.remove(i); } }
            }
        }
    }
    modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string());
    modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string());
    modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string());

    let mut fetched = 0u32;
    let mut failed = 0u32;
    for idx in &modified {
        let r: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger_entry","params":[{"index":idx,"binary":true,"ledger_index":seq}]}))
            .send().unwrap().json().unwrap();
        if let Some(d) = r["result"]["node_binary"].as_str() {
            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(d)) {
                if k.len() == 32 {
                    db.put(&k, &v).unwrap();
                    let mut ka = [0u8; 32]; ka.copy_from_slice(&k);
                    let _ = live_map.insert_hash_only(Hash256(ka), leaf_hash(&ka, &v));
                    fetched += 1;
                }
            }
        } else { failed += 1; }
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

fn main() {
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(60)).build().unwrap();
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let exp0 = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?").to_string();

    eprintln!("Downloading at #{seq}...");
    let tmp = "/tmp/test_round2_db";
    let _ = std::fs::remove_dir_all(tmp);
    let db = rocksdb::DB::open_default(tmp).unwrap();
    let mut marker: Option<String> = None;
    let mut count = 0u64;
    loop {
        let mut p = serde_json::json!({"ledger_index":seq,"binary":true,"limit":2048});
        if let Some(ref m) = marker { p["marker"] = serde_json::Value::String(m.clone()); }
        let r: serde_json::Value = client.post(RPC).json(&serde_json::json!({"method":"ledger_data","params":[p]})).send().unwrap().json().unwrap();
        if let Some(st) = r["result"]["state"].as_array() {
            for o in st { if let (Some(i),Some(d)) = (o["index"].as_str(),o["data"].as_str()) {
                if let (Ok(k),Ok(v)) = (hex::decode(i),hex::decode(d)) { if k.len()==32 { db.put(&k,&v).unwrap(); count+=1; }}
            }}
        }
        marker = r["result"]["marker"].as_str().map(String::from);
        if marker.is_none() { break; }
        if count % 4_000_000 < 2048 { eprintln!("  {count}..."); }
    }
    eprintln!("Downloaded {count}");

    // Build live SHAMap
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
    eprintln!("Initial: {} ({})", if h0.to_uppercase()==exp0.to_uppercase() {"MATCH"} else {"MISMATCH"}, &h0[..16]);

    // Apply 5 rounds, checking incremental vs rebuild each time
    for round in 1..=5 {
        let next = seq + round;
        loop { std::thread::sleep(std::time::Duration::from_secs(1));
            let r: serde_json::Value = client.post(RPC).json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]})).send().unwrap().json().unwrap();
            if r["result"]["ledger"]["account_hash"].is_string() { break; }
        }
        let r: serde_json::Value = client.post(RPC).json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":next}]})).send().unwrap().json().unwrap();
        let exp = r["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");

        let (f, d, e) = apply_round(&client, next, &db, &mut live_map);
        let inc_hash = hex::encode(live_map.root_hash().0);
        let (reb_hash, _) = build_fresh(&db);
        let inc_m = if inc_hash.to_uppercase()==exp.to_uppercase() {"MATCH"} else {"MISMATCH"};
        let reb_m = if reb_hash.to_uppercase()==exp.to_uppercase() {"MATCH"} else {"MISMATCH"};
        eprintln!("Round {round} (#{next}): inc={inc_m} reb={reb_m} — {f}f {d}d {e}e  inc==reb:{}", inc_hash==reb_hash);
        if inc_m == "MISMATCH" || reb_m == "MISMATCH" {
            eprintln!("  Expected: {}", &exp[..32]);
            eprintln!("  Inc:      {}", &inc_hash[..32]);
            eprintln!("  Rebuild:  {}", &reb_hash[..32]);
        }
    }
    drop(db); let _ = std::fs::remove_dir_all(tmp);
}
