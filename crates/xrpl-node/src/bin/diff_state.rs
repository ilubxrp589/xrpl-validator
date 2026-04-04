//! Find discrepancies between live RocksDB and rippled's current state.
//! Samples keys throughout the keyspace and checks for data mismatches.

fn main() {
    let db_path = std::env::var("XRPL_ROCKS_PATH")
        .unwrap_or_else(|_| "/mnt/xrpl-data/sync/state.rocks".to_string());
    let db = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), &db_path, false)
        .expect("Failed to open RocksDB");

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build().unwrap();

    // Get current validated ledger
    let resp: serde_json::Value = client
        .post("http://localhost:5005")
        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
        .send().unwrap().json().unwrap();
    let seq: u32 = resp["result"]["ledger"]["ledger_index"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let expected_hash = resp["result"]["ledger"]["account_hash"].as_str().unwrap_or("?");
    eprintln!("Validated: #{seq}  hash: {expected_hash}");

    // Iterate RocksDB, sample every 100k-th key, fetch from rippled and compare
    let mut total = 0u64;
    let mut sampled = 0u64;
    let mut matches = 0u64;
    let mut diffs = 0u64;
    let mut missing_in_rippled = 0u64;
    let mut diff_keys: Vec<String> = Vec::new();

    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if let Ok((key, value)) = item {
            if key.len() != 32 { continue; }
            total += 1;
            
            // Sample every 100k-th key
            if total % 100_000 != 1 { continue; }
            sampled += 1;
            
            let key_hex = hex::encode(&key);
            let resp: serde_json::Value = client
                .post("http://localhost:5005")
                .json(&serde_json::json!({
                    "method": "ledger_entry",
                    "params": [{"index": &key_hex, "binary": true, "ledger_index": "validated"}]
                }))
                .send().unwrap().json().unwrap();
            
            if let Some(data_hex) = resp["result"]["node_binary"].as_str() {
                let rippled_data = hex::decode(data_hex).unwrap_or_default();
                if value.to_vec() == rippled_data {
                    matches += 1;
                } else {
                    diffs += 1;
                    diff_keys.push(key_hex.clone());
                    if diffs <= 5 {
                        eprintln!("[DIFF] key={} rocks={}B rippled={}B", 
                            &key_hex[..16], value.len(), rippled_data.len());
                        eprintln!("  rocks:   {}", hex::encode(&value[..32.min(value.len())]));
                        eprintln!("  rippled: {}", hex::encode(&rippled_data[..32.min(rippled_data.len())]));
                    }
                }
            } else {
                missing_in_rippled += 1;
                if missing_in_rippled <= 3 {
                    let err = resp["result"]["error"].as_str().unwrap_or("?");
                    eprintln!("[MISSING] key={} err={err}", &key_hex[..16]);
                }
            }
            
            if sampled % 50 == 0 {
                eprintln!("Progress: {total} scanned, {sampled} sampled, {matches} match, {diffs} diff, {missing_in_rippled} missing");
            }
        }
    }

    eprintln!("\n=== RESULT ===");
    eprintln!("Total keys in RocksDB: {total}");
    eprintln!("Sampled: {sampled}");
    eprintln!("Matches: {matches}");
    eprintln!("Diffs: {diffs}");
    eprintln!("Missing in rippled: {missing_in_rippled}");
    if !diff_keys.is_empty() {
        eprintln!("First diff keys: {:?}", &diff_keys[..5.min(diff_keys.len())]);
    }
}
