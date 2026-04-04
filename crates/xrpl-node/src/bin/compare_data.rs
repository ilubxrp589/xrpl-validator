//! Compare RocksDB data against rippled ledger_data to find discrepancies.
fn main() {
    let db_path = std::env::var("XRPL_ROCKS_PATH")
        .unwrap_or_else(|_| "/mnt/xrpl-data/sync/state.rocks".to_string());
    let db = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), &db_path, false).unwrap();
    
    // Sample 10 keys from RocksDB
    let mut samples: Vec<(String, Vec<u8>)> = Vec::new();
    let iter = db.iterator(rocksdb::IteratorMode::Start);
    let mut count = 0u64;
    for item in iter {
        if samples.len() >= 10 { break; }
        if let Ok((key, value)) = item {
            if key.len() == 32 {
                count += 1;
                // Sample every 1M-th key
                if count == 1 || count % 1_000_000 == 0 || samples.is_empty() {
                    samples.push((hex::encode(&key), value.to_vec()));
                }
            }
        }
    }
    
    eprintln!("Sampled {} keys from RocksDB ({count} total)", samples.len());
    
    // Fetch the same keys from rippled at validated ledger
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build().unwrap();
    
    for (key_hex, rocks_data) in &samples {
        let resp: serde_json::Value = client
            .post("http://localhost:5005")
            .json(&serde_json::json!({
                "method": "ledger_entry",
                "params": [{"index": key_hex, "binary": true, "ledger_index": "validated"}]
            }))
            .send().unwrap()
            .json().unwrap();
        
        if let Some(data_hex) = resp["result"]["node_binary"].as_str() {
            let rippled_data = hex::decode(data_hex).unwrap_or_default();
            let match_status = if rocks_data == &rippled_data { "MATCH" } else { "DIFF" };
            println!("[{match_status}] key={}... rocks={} rippled={}", 
                &key_hex[..16], rocks_data.len(), rippled_data.len());
            if rocks_data != &rippled_data {
                println!("  rocks:  {}", hex::encode(&rocks_data[..32.min(rocks_data.len())]));
                println!("  rippled: {}", hex::encode(&rippled_data[..32.min(rippled_data.len())]));
            }
        } else {
            let err = resp["result"]["error"].as_str().unwrap_or("unknown");
            println!("[ERR] key={}... error={err}", &key_hex[..16]);
        }
    }
}
