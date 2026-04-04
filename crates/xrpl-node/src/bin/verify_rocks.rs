//! Verify that RocksDB roundtrip preserves hash correctness.
//! Downloads ALL state → writes to temp RocksDB → reads back → compares hashes.
//!
//! Run: cargo run --release -p xrpl-node --bin verify_rocks

use serde_json::{json, Value};
use std::time::Instant;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

const RPC: &str = "http://localhost:5005";

fn main() -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    // Get validated ledger
    let resp: Value = client.post(RPC)
        .json(&json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send()?.json()?;

    let ledger = &resp["result"]["ledger"];
    let seq: u32 = ledger["ledger_index"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected_hash = ledger["account_hash"].as_str().unwrap_or("???");
    eprintln!("Ledger #{seq}");
    eprintln!("Expected: {expected_hash}");

    // Create temp RocksDB
    let tmp_path = "/tmp/verify_rocks_db";
    let _ = std::fs::remove_dir_all(tmp_path);
    let db = rocksdb::DB::open_default(tmp_path)?;

    // Download ALL via ledger_data, write to RocksDB AND keep in memory
    let start = Instant::now();
    let mut mem_objects: Vec<(Hash256, Vec<u8>)> = Vec::new();
    let mut marker: Option<String> = None;
    let mut pages = 0u32;

    loop {
        pages += 1;
        let mut params = json!({"ledger_index": seq, "limit": 2048, "binary": true});
        if let Some(ref m) = marker {
            params["marker"] = Value::String(m.clone());
        }

        let resp: Value = client.post(RPC)
            .json(&json!({"method": "ledger_data", "params": [params]}))
            .send()?.json()?;

        if let Some(state) = resp["result"]["state"].as_array() {
            for obj in state {
                let index = obj["index"].as_str().unwrap_or("");
                let data_hex = obj["data"].as_str().unwrap_or("");
                if let (Ok(key_bytes), Ok(data)) = (hex::decode(index), hex::decode(data_hex)) {
                    if key_bytes.len() == 32 {
                        // Write to RocksDB
                        db.put(&key_bytes, &data)?;
                        // Keep in memory
                        let mut key = [0u8; 32];
                        key.copy_from_slice(&key_bytes);
                        mem_objects.push((Hash256(key), data));
                    }
                }
            }
        }

        marker = resp["result"]["marker"].as_str().map(String::from);
        if marker.is_none() { break; }
        if pages % 100 == 0 {
            eprintln!("  page {pages}: {} objects...", mem_objects.len());
        }
    }

    let dl_time = start.elapsed();
    eprintln!("Downloaded {} objects in {:.1}s", mem_objects.len(), dl_time.as_secs_f64());

    // Hash 1: From memory (same as verify_hash Method 1)
    eprintln!("\nHash from MEMORY:");
    let build_start = Instant::now();
    let mut map_mem = SHAMap::new(TreeType::State);
    for (key, data) in &mem_objects {
        let mut buf = Vec::with_capacity(data.len() + 32);
        buf.extend_from_slice(data);
        buf.extend_from_slice(&key.0);
        let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
        let _ = map_mem.insert_hash_only(*key, leaf_hash);
    }
    let hash_mem = hex::encode(map_mem.root_hash().0);
    eprintln!("  Hash: {hash_mem}");
    eprintln!("  Match: {}", hash_mem.to_uppercase() == expected_hash.to_uppercase());
    eprintln!("  Time: {:.1}s", build_start.elapsed().as_secs_f64());

    // Hash 2: From RocksDB (same as live rebuild)
    eprintln!("\nHash from ROCKSDB:");
    let build_start = Instant::now();
    let mut map_rocks = SHAMap::new(TreeType::State);
    let mut rocks_count = 0u64;
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
                let _ = map_rocks.insert_hash_only(Hash256(key_arr), leaf_hash);
                rocks_count += 1;
            }
        }
    }
    let hash_rocks = hex::encode(map_rocks.root_hash().0);
    eprintln!("  Hash: {hash_rocks}");
    eprintln!("  Match: {}", hash_rocks.to_uppercase() == expected_hash.to_uppercase());
    eprintln!("  Entries: {rocks_count}");
    eprintln!("  Time: {:.1}s", build_start.elapsed().as_secs_f64());

    // Summary
    eprintln!("\n=== SUMMARY ===");
    eprintln!("Expected:  {expected_hash}");
    eprintln!("Memory:    {hash_mem}");
    eprintln!("RocksDB:   {hash_rocks}");
    eprintln!("Mem==Exp:  {}", hash_mem.to_uppercase() == expected_hash.to_uppercase());
    eprintln!("Rocks==Exp: {}", hash_rocks.to_uppercase() == expected_hash.to_uppercase());
    eprintln!("Mem==Rocks: {}", hash_mem == hash_rocks);

    // Cleanup
    drop(db);
    let _ = std::fs::remove_dir_all(tmp_path);

    Ok(())
}
