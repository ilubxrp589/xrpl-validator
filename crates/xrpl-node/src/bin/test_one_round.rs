//! Test: download fresh state, verify hash, then apply ONE ledger of changes and verify again.
//! This isolates whether the incremental sync introduces errors.

use std::collections::HashSet;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

const RPC: &str = "http://10.0.0.39:5005";

fn build_hash(db: &rocksdb::DB) -> (String, u64) {
    let mut map = SHAMap::new(TreeType::State);
    let mut count = 0u64;
    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&key);
                let mut buf = Vec::with_capacity(value.len() + 32);
                buf.extend_from_slice(&value);
                buf.extend_from_slice(&k);
                let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                let _ = map.insert_hash_only(Hash256(k), lh);
                count += 1;
            }
        }
    }
    (hex::encode(map.root_hash().0), count)
}

fn get_network_hash(client: &reqwest::blocking::Client, seq: u32) -> Option<String> {
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":seq}]}))
        .send().ok()?.json().ok()?;
    resp["result"]["ledger"]["account_hash"].as_str().map(|s| s.to_string())
}

fn main() {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build().unwrap();

    // Step 1: Get validated ledger
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?").to_string();
    eprintln!("=== Step 1: Download ALL state at ledger #{seq} ===");

    // Download to temp RocksDB
    let tmp_path = "/tmp/test_one_round_db";
    let _ = std::fs::remove_dir_all(tmp_path);
    let db = rocksdb::DB::open_default(tmp_path).unwrap();

    let mut marker: Option<String> = None;
    let mut count = 0u64;
    let mut pages = 0u32;
    loop {
        pages += 1;
        let mut params = serde_json::json!({"ledger_index": seq, "binary": true, "limit": 2048});
        if let Some(ref m) = marker { params["marker"] = serde_json::Value::String(m.clone()); }
        let resp: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
            .send().unwrap().json().unwrap();
        if let Some(state) = resp["result"]["state"].as_array() {
            for obj in state {
                if let (Some(idx), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                    if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(data)) {
                        if k.len() == 32 { db.put(&k, &v).unwrap(); count += 1; }
                    }
                }
            }
        }
        marker = resp["result"]["marker"].as_str().map(String::from);
        if marker.is_none() { break; }
        if pages % 200 == 0 { eprintln!("  pg {pages}: {count} objects"); }
    }
    eprintln!("Downloaded {count} objects");

    // Verify initial hash
    let (hash0, n0) = build_hash(&db);
    let m = if hash0.to_uppercase() == expected.to_uppercase() { "MATCH" } else { "MISMATCH" };
    eprintln!("\n=== Step 2: Verify initial hash ===");
    eprintln!("  Entries: {n0}");
    eprintln!("  Ours:     {hash0}");
    eprintln!("  Expected: {expected}");
    eprintln!("  Result:   {m}");

    if m == "MISMATCH" {
        eprintln!("Initial hash doesn't match — aborting");
        drop(db);
        let _ = std::fs::remove_dir_all(tmp_path);
        return;
    }

    // Step 3: Wait for next ledger and apply changes
    eprintln!("\n=== Step 3: Apply ONE ledger of changes ===");
    let next_seq = seq + 1;
    eprintln!("Waiting for ledger #{next_seq}...");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if let Some(_) = get_network_hash(&client, next_seq) { break; }
    }
    let next_expected = get_network_hash(&client, next_seq).unwrap_or_default();
    eprintln!("Ledger #{next_seq} available. Expected hash: {next_expected}");

    // Fetch ledger with transactions
    let resp: serde_json::Value = client.post(RPC)
        .json(&serde_json::json!({
            "method": "ledger",
            "params": [{"ledger_index": next_seq, "transactions": true, "expand": true, "binary": false}]
        }))
        .send().unwrap().json().unwrap();

    let txs = resp["result"]["ledger"]["transactions"].as_array().unwrap();
    let mut modified_indices: HashSet<String> = HashSet::new();
    let mut deleted_indices: HashSet<String> = HashSet::new();

    for tx in txs {
        let meta = if tx["metaData"].is_object() { &tx["metaData"] }
            else if tx["meta"].is_object() { &tx["meta"] }
            else { continue; };
        if let Some(nodes) = meta["AffectedNodes"].as_array() {
            for node in nodes {
                if let Some(created) = node.get("CreatedNode") {
                    if let Some(idx) = created["LedgerIndex"].as_str() {
                        modified_indices.insert(idx.to_string());
                    }
                }
                if let Some(modified) = node.get("ModifiedNode") {
                    if let Some(idx) = modified["LedgerIndex"].as_str() {
                        modified_indices.insert(idx.to_string());
                    }
                }
                if let Some(deleted) = node.get("DeletedNode") {
                    if let Some(idx) = deleted["LedgerIndex"].as_str() {
                        deleted_indices.insert(idx.to_string());
                        modified_indices.remove(idx);
                    }
                }
            }
        }
    }

    // Add LedgerHashes — changes every ledger without appearing in AffectedNodes
    modified_indices.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string());

    eprintln!("  Transactions: {}", txs.len());
    eprintln!("  Modified indices: {} (includes LedgerHashes)", modified_indices.len());
    eprintln!("  Deleted indices: {}", deleted_indices.len());

    // Fetch each modified object and write to DB
    let mut fetched = 0u32;
    let mut failed = 0u32;
    for idx in &modified_indices {
        let resp: serde_json::Value = client.post(RPC)
            .json(&serde_json::json!({
                "method": "ledger_entry",
                "params": [{"index": idx, "binary": true, "ledger_index": next_seq}]
            }))
            .send().unwrap().json().unwrap();
        if let Some(data_hex) = resp["result"]["node_binary"].as_str() {
            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(data_hex)) {
                if k.len() == 32 { db.put(&k, &v).unwrap(); fetched += 1; }
            }
        } else {
            let err = resp["result"]["error"].as_str().unwrap_or("?");
            eprintln!("  FETCH FAIL: {}: {err}", &idx[..16]);
            failed += 1;
        }
    }

    // Delete removed objects
    let mut deleted = 0u32;
    for idx in &deleted_indices {
        if let Ok(k) = hex::decode(idx) {
            if k.len() == 32 { db.delete(&k).unwrap(); deleted += 1; }
        }
    }

    eprintln!("  Fetched: {fetched}, Deleted: {deleted}, Failed: {failed}");

    // Rebuild and verify
    let (hash1, n1) = build_hash(&db);
    let m = if hash1.to_uppercase() == next_expected.to_uppercase() { "MATCH" } else { "MISMATCH" };
    eprintln!("\n=== Step 4: Verify after ONE round ===");
    eprintln!("  Entries: {n1}");
    eprintln!("  Ours:     {hash1}");
    eprintln!("  Expected: {next_expected}");
    eprintln!("  Result:   {m}");

    // Cleanup
    drop(db);
    let _ = std::fs::remove_dir_all(tmp_path);
}
