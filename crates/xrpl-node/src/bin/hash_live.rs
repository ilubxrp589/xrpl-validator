//! Compute hash from live RocksDB and compare against current validated ledger.
//! Opens DB read-only — safe to run while validator is running.

use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

fn main() {
    let db_path = "/mnt/xrpl-data/sync/state.rocks";
    
    // Get current validated ledger hash FIRST (before the read takes time)
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build().unwrap();
    let resp: serde_json::Value = client
        .post("http://localhost:5005")
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");
    eprintln!("Before scan — Validated #{seq}: {expected}");

    // Open RocksDB read-only with snapshot
    let db = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), db_path, false)
        .expect("open failed");
    
    let start = std::time::Instant::now();
    let mut map = SHAMap::new(TreeType::State);
    let mut count = 0u64;
    
    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                let mut key_arr = [0u8; 32];
                key_arr.copy_from_slice(&key);
                let mut buf = Vec::with_capacity(value.len() + 32);
                buf.extend_from_slice(&value);
                buf.extend_from_slice(&key_arr);
                let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                let _ = map.insert_hash_only(Hash256(key_arr), leaf_hash);
                count += 1;
            }
        }
    }
    
    let our_hash = hex::encode(map.root_hash().0);
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!("Computed {count} entries in {elapsed:.1}s");
    eprintln!("Our hash: {our_hash}");
    
    // Get hash AFTER scan too (to see if ledger moved)
    let resp2: serde_json::Value = client
        .post("http://localhost:5005")
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq2: u32 = resp2["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected2 = resp2["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");
    eprintln!("After scan — Validated #{seq2}: {expected2}");
    
    // Try matching against the specific ledger our data might be at
    // Check the last few ledgers
    eprintln!("\nChecking recent ledgers:");
    for check_seq in (seq2.saturating_sub(15))..=seq2 {
        let resp: serde_json::Value = client
            .post("http://localhost:5005")
            .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":check_seq}]}))
            .send().unwrap().json().unwrap();
        if let Some(hash) = resp["result"]["ledger"]["account_hash"].as_str() {
            let m = if our_hash.to_uppercase() == hash.to_uppercase() { "<<< MATCH!" } else { "" };
            eprintln!("  #{check_seq}: {hash} {m}");
        }
    }
}
