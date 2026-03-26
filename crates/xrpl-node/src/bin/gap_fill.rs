//! Gap-fill: 3 parallel workers scanning the full 0x00-0xFF keyspace,
//! one endpoint each, checking each key against RocksDB and filling missing entries.
//! Usage: cargo run --release --bin gap_fill -- <ledger_index>

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ledger_index: u32 = args.get(1)
        .and_then(|s| s.parse().ok())
        .expect("Usage: gap_fill <ledger_index>");

    let rocks_path = std::env::var("XRPL_ROCKS_PATH")
        .unwrap_or_else(|_| "/mnt/xrpl-data/sync/state.rocks".to_string());

    eprintln!("[gap-fill] Opening RocksDB at {rocks_path}");
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);
    opts.optimize_for_point_lookup(64);
    let db = Arc::new(rocksdb::DB::open(&opts, &rocks_path)
        .expect("Failed to open RocksDB"));

    // Count existing
    let mut existing = 0u64;
    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if item.is_ok() { existing += 1; }
    }
    eprintln!("[gap-fill] Existing entries: {existing}");

    let total_filled = Arc::new(AtomicU64::new(0));
    let total_checked = Arc::new(AtomicU64::new(0));
    let start = std::time::Instant::now();

    let workers: Vec<(&str, u8, u8)> = vec![
        ("https://xrplcluster.com", 0x00, 0x55),
        ("https://xrplcluster.com", 0x55, 0xAA),
        ("https://xrplcluster.com", 0xAA, 0xFF),
    ];

    eprintln!("[gap-fill] Scanning ledger #{ledger_index} with 3 workers...");

    let mut handles = vec![];
    for (endpoint, start_byte, end_byte) in workers {
        let db = db.clone();
        let filled = total_filled.clone();
        let checked = total_checked.clone();
        let ep = endpoint.to_string();
        let is_last = end_byte == 0xFF;

        handles.push(std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client");

            let mut marker: Option<serde_json::Value> = if start_byte == 0x00 {
                None
            } else {
                let mut key = [0u8; 32];
                key[0] = start_byte;
                Some(serde_json::Value::String(hex::encode(key)))
            };

            let label = format!("{:02X}-{:02X}", start_byte, end_byte);
            let mut pages = 0u64;
            let mut local_filled = 0u64;
            let mut local_checked = 0u64;
            let mut errors = 0u64;

            eprintln!("[{label}] Worker → {ep}");

            loop {
                let mut params = serde_json::json!({
                    "ledger_index": ledger_index,
                    "binary": true,
                    "limit": 2048
                });
                if let Some(ref m) = marker {
                    params["marker"] = m.clone();
                }

                let resp = match client.post(&ep)
                    .json(&serde_json::json!({"method": "ledger_data", "params": [params]}))
                    .send()
                {
                    Ok(r) => r,
                    Err(e) => {
                        errors += 1;
                        eprintln!("[{label}] Error: {e}");
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        if errors > 50 { break; }
                        continue;
                    }
                };

                let body: serde_json::Value = match resp.json() {
                    Ok(b) => b,
                    Err(e) => {
                        errors += 1;
                        eprintln!("[{label}] Parse error: {e}");
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        if errors > 50 { break; }
                        continue;
                    }
                };

                if body.get("result").is_none() {
                    errors += 1;
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if errors > 50 { break; }
                    continue;
                }

                let mut past_end = false;
                let mut page_filled = 0u64;
                if let Some(objs) = body["result"]["state"].as_array() {
                    for obj in objs {
                        if let (Some(idx), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                            if !is_last && idx.len() >= 2 {
                                if let Ok(fb) = u8::from_str_radix(&idx[0..2], 16) {
                                    if fb >= end_byte { past_end = true; break; }
                                }
                            }
                            if idx.len() == 64 {
                                if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(data)) {
                                    if k.len() == 32 {
                                        local_checked += 1;
                                        match db.get(&k) {
                                            Ok(Some(_)) => {}
                                            _ => {
                                                let _ = db.put(&k, &v);
                                                local_filled += 1;
                                                page_filled += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                checked.fetch_add(local_checked, Ordering::Relaxed);
                local_checked = 0;
                filled.fetch_add(page_filled, Ordering::Relaxed);

                pages += 1;
                if pages % 100 == 0 || page_filled > 0 {
                    let gc = checked.load(Ordering::Relaxed);
                    let gf = filled.load(Ordering::Relaxed);
                    eprintln!("[{label}] pg {pages}: local_filled={local_filled} | global: checked={gc} filled={gf}");
                }

                if past_end { break; }
                marker = body.get("result").and_then(|r| r.get("marker")).cloned();
                if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) { break; }

                // Throttle: ~3 req/sec per worker (9 total = 540/min, under 1000 limit)
                std::thread::sleep(std::time::Duration::from_millis(333));
            }

            eprintln!("[{label}] DONE: {local_filled} filled, {pages} pages, {errors} errors");
        }));
    }

    for h in handles {
        let _ = h.join();
    }

    let elapsed = start.elapsed().as_secs_f64();
    let gf = total_filled.load(Ordering::Relaxed);
    eprintln!("[gap-fill] ALL DONE: filled {gf} gaps in {elapsed:.0}s");

    // Recount
    let mut final_count = 0u64;
    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if item.is_ok() { final_count += 1; }
    }
    eprintln!("[gap-fill] Final entry count: {final_count} (was {existing}, added {gf})");
}
