//! Full state diff: download ALL state from rippled, compare every entry against live RocksDB.
//! Finds missing, extra, and data-mismatched entries.

use std::collections::HashMap;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};

fn main() {
    let db_path = "/mnt/xrpl-data/sync/state.rocks";
    let db = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), db_path, false)
        .expect("open failed");

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build().unwrap();

    // Get validated ledger
    let resp: serde_json::Value = client
        .post("http://10.0.0.39:5005")
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");
    eprintln!("Validated: #{seq}  hash: {expected}");

    // Download ALL state, store leaf hashes only (32+32 bytes per entry = ~1.2GB)
    eprintln!("Downloading all state from rippled...");
    let start = std::time::Instant::now();
    let mut rippled_hashes: HashMap<[u8; 32], [u8; 32]> = HashMap::with_capacity(19_000_000);
    let mut marker: Option<String> = None;
    let mut pages = 0u32;
    let mut dl_count = 0u64;

    loop {
        pages += 1;
        let mut params = serde_json::json!({"ledger_index": seq, "binary": true, "limit": 2048});
        if let Some(ref m) = marker {
            params["marker"] = serde_json::Value::String(m.clone());
        }

        let resp: serde_json::Value = match client.post("http://10.0.0.39:5005")
            .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
            .send()
        {
            Ok(r) => r.json().unwrap(),
            Err(e) => { eprintln!("HTTP error: {e}"); std::thread::sleep(std::time::Duration::from_secs(2)); continue; }
        };

        if let Some(state) = resp["result"]["state"].as_array() {
            for obj in state {
                let idx = obj["index"].as_str().unwrap_or("");
                let data_hex = obj["data"].as_str().unwrap_or("");
                if let (Ok(key_bytes), Ok(data)) = (hex::decode(idx), hex::decode(data_hex)) {
                    if key_bytes.len() == 32 {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(&key_bytes);
                        // Compute leaf hash
                        let mut buf = Vec::with_capacity(data.len() + 32);
                        buf.extend_from_slice(&data);
                        buf.extend_from_slice(&key);
                        let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                        rippled_hashes.insert(key, leaf_hash.0);
                        dl_count += 1;
                    }
                }
            }
        }

        marker = resp["result"]["marker"].as_str().map(String::from);
        if marker.is_none() { break; }
        if pages % 200 == 0 {
            let elapsed = start.elapsed().as_secs_f64();
            eprintln!("  pg {pages}: {} objects ({:.0}/s)", dl_count, dl_count as f64 / elapsed);
        }
    }
    let dl_time = start.elapsed().as_secs_f64();
    eprintln!("Downloaded {dl_count} objects in {dl_time:.0}s");

    // Now iterate RocksDB and compare
    eprintln!("\nComparing against RocksDB...");
    let mut rocks_count = 0u64;
    let mut match_count = 0u64;
    let mut data_diff = 0u64;
    let mut extra_in_rocks = 0u64;
    let mut diff_examples: Vec<String> = Vec::new();
    let mut extra_examples: Vec<String> = Vec::new();

    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if let Ok((key, value)) = item {
            if key.len() != 32 { continue; }
            rocks_count += 1;

            let mut key_arr = [0u8; 32];
            key_arr.copy_from_slice(&key);

            // Compute leaf hash from RocksDB data
            let mut buf = Vec::with_capacity(value.len() + 32);
            buf.extend_from_slice(&value);
            buf.extend_from_slice(&key_arr);
            let rocks_leaf = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);

            if let Some(rippled_leaf) = rippled_hashes.remove(&key_arr) {
                if rocks_leaf.0 == rippled_leaf {
                    match_count += 1;
                } else {
                    data_diff += 1;
                    if diff_examples.len() < 10 {
                        diff_examples.push(format!("{} rocks_leaf={} rippled_leaf={}",
                            hex::encode(&key_arr[..8]),
                            hex::encode(&rocks_leaf.0[..8]),
                            hex::encode(&rippled_leaf[..8])));
                    }
                }
            } else {
                extra_in_rocks += 1;
                if extra_examples.len() < 10 {
                    extra_examples.push(hex::encode(&key_arr[..8]));
                }
            }
        }
    }

    let missing_from_rocks = rippled_hashes.len() as u64;

    eprintln!("\n=== FULL DIFF RESULT ===");
    eprintln!("Rippled entries: {dl_count}");
    eprintln!("RocksDB entries: {rocks_count}");
    eprintln!("Matching: {match_count}");
    eprintln!("Data differs: {data_diff}");
    eprintln!("Extra in RocksDB (not in rippled): {extra_in_rocks}");
    eprintln!("Missing from RocksDB (in rippled): {missing_from_rocks}");

    if !diff_examples.is_empty() {
        eprintln!("\nData diff examples:");
        for ex in &diff_examples { eprintln!("  {ex}"); }
    }
    if !extra_examples.is_empty() {
        eprintln!("\nExtra in RocksDB:");
        for ex in &extra_examples { eprintln!("  {ex}"); }
    }
    if missing_from_rocks > 0 {
        eprintln!("\nMissing from RocksDB (first 10):");
        for (i, (key, _)) in rippled_hashes.iter().enumerate() {
            if i >= 10 { break; }
            eprintln!("  {}", hex::encode(&key[..8]));
        }
    }
}
