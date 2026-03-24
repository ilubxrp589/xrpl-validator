//! Bulk state sync — re-download all ledger objects from the network.
//!
//! Uses the `ledger_data` RPC to paginate through ALL state objects
//! and write fresh binary data to RocksDB. This ensures our local
//! state matches the network's exactly (byte-for-byte), so the
//! SHAMap root hash will match.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

/// Status of the bulk sync operation.
#[derive(Clone, Default, serde::Serialize)]
pub struct BulkSyncStatus {
    pub running: bool,
    pub objects_synced: u64,
    pub pages_fetched: u64,
    pub errors: u64,
    pub elapsed_secs: f64,
    pub rate: f64,
    pub estimated_remaining_secs: f64,
}

/// Bulk state syncer.
pub struct BulkSyncer {
    pub status: Arc<Mutex<BulkSyncStatus>>,
    running: Arc<AtomicBool>,
    objects_synced: Arc<AtomicU64>,
}

impl BulkSyncer {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(BulkSyncStatus::default())),
            running: Arc::new(AtomicBool::new(false)),
            objects_synced: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn objects_synced(&self) -> u64 {
        self.objects_synced.load(Ordering::Relaxed)
    }

    /// Start bulk sync with parallel workers fetching from different key ranges.
    pub fn start(&self, db: Arc<rocksdb::DB>, estimated_total: u64) {
        if self.running.swap(true, Ordering::SeqCst) {
            eprintln!("[bulk-sync] Already running");
            return;
        }

        let status = self.status.clone();
        let running = self.running.clone();
        let objects_synced = self.objects_synced.clone();

        objects_synced.store(0, Ordering::Relaxed);
        {
            let mut s = status.lock();
            *s = BulkSyncStatus { running: true, ..Default::default() };
        }

        // Spawn 4 parallel workers, each covering 1/4 of the key space
        // Worker 0: 00... - 3F...
        // Worker 1: 40... - 7F...
        // Worker 2: 80... - BF...
        // Worker 3: C0... - FF...
        let workers = 4;
        let worker_done = Arc::new(std::sync::atomic::AtomicU32::new(0));

        for w in 0..workers {
            let db = db.clone();
            let objects_synced = objects_synced.clone();
            let status = status.clone();
            let running = running.clone();
            let worker_done = worker_done.clone();
            let estimated_per_worker = estimated_total / workers as u64;

            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let start_nibble = w * 4; // 0, 4, 8, C
                let rpc = match w {
                    0 | 2 => "https://s1.ripple.com:51234",
                    _ => "https://s2.ripple.com:51234",
                };

                // Start marker: first key in our range
                let mut start_key = [0u8; 32];
                start_key[0] = (start_nibble as u8) << 4;
                let mut marker: Option<String> = Some(hex::encode(start_key));
                let end_nibble = start_nibble + 4;

                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .expect("reqwest client");

                let mut my_objects: u64 = 0;
                let mut my_pages: u64 = 0;
                let mut errors: u64 = 0;
                let mut batch = rocksdb::WriteBatch::default();
                let mut batch_size = 0;

                eprintln!("[bulk-sync-{w}] Starting from key {start_nibble:X}0... to {end_nibble:X}0... via {rpc}");

                loop {
                    let mut params = serde_json::json!({
                        "ledger_index": "validated",
                        "binary": true,
                        "limit": 2048
                    });
                    if let Some(ref m) = marker {
                        params["marker"] = serde_json::Value::String(m.clone());
                    }

                    let resp = client.post(rpc)
                        .json(&serde_json::json!({
                            "method": "ledger_data",
                            "params": [params]
                        }))
                        .send();

                    let body = match resp {
                        Ok(r) => match r.json::<serde_json::Value>() {
                            Ok(b) => b,
                            Err(e) => {
                                errors += 1;
                                if errors <= 3 { eprintln!("[bulk-sync-{w}] JSON: {e}"); }
                                std::thread::sleep(std::time::Duration::from_secs(2));
                                continue;
                            }
                        },
                        Err(e) => {
                            errors += 1;
                            if errors <= 3 { eprintln!("[bulk-sync-{w}] RPC: {e}"); }
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            continue;
                        }
                    };

                    let result = &body["result"];
                    if result.get("error").is_some() {
                        errors += 1;
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        continue;
                    }

                    let mut past_range = false;
                    if let Some(objects) = result["state"].as_array() {
                        for obj in objects {
                            let index = obj["index"].as_str().unwrap_or("");
                            let data = obj["data"].as_str().unwrap_or("");
                            if index.len() == 64 {
                                // Check if we've passed our range
                                let first_nibble = u8::from_str_radix(&index[0..1], 16).unwrap_or(0);
                                if first_nibble >= end_nibble as u8 {
                                    past_range = true;
                                    break;
                                }
                                if let (Ok(key_bytes), Ok(data_bytes)) = (hex::decode(index), hex::decode(data)) {
                                    if key_bytes.len() == 32 {
                                        batch.put(&key_bytes, &data_bytes);
                                        batch_size += 1;
                                        my_objects += 1;
                                    }
                                }
                            }
                        }
                    }

                    if batch_size >= 10_000 {
                        let _ = db.write(batch);
                        batch = rocksdb::WriteBatch::default();
                        batch_size = 0;
                    }

                    my_pages += 1;
                    objects_synced.fetch_add(
                        my_objects.saturating_sub(objects_synced.load(Ordering::Relaxed) / workers as u64),
                        Ordering::Relaxed
                    );

                    if my_pages % 100 == 0 {
                        let elapsed = start.elapsed().as_secs_f64();
                        let rate = my_objects as f64 / elapsed;
                        eprintln!(
                            "[bulk-sync-{w}] {my_objects} objects, {my_pages} pages, {rate:.0}/s, {errors} errors",
                        );
                        let mut s = status.lock();
                        s.objects_synced = objects_synced.load(Ordering::Relaxed);
                        s.pages_fetched += my_pages;
                        s.errors += errors;
                        s.elapsed_secs = elapsed;
                        s.rate = s.objects_synced as f64 / elapsed;
                    }

                    if past_range {
                        eprintln!("[bulk-sync-{w}] Reached end of range at {end_nibble:X}0");
                        break;
                    }

                    marker = result.get("marker")
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    if marker.is_none() {
                        break;
                    }
                }

                if batch_size > 0 {
                    let _ = db.write(batch);
                }

                let elapsed = start.elapsed().as_secs_f64();
                eprintln!("[bulk-sync-{w}] DONE: {my_objects} objects in {elapsed:.0}s");

                let done = worker_done.fetch_add(1, Ordering::SeqCst) + 1;
                if done >= workers as u32 {
                    let total = objects_synced.load(Ordering::Relaxed);
                    eprintln!("[bulk-sync] ALL WORKERS DONE: ~{total} total objects synced");
                    let mut s = status.lock();
                    s.running = false;
                    s.objects_synced = total;
                    s.elapsed_secs = elapsed;
                    s.rate = total as f64 / elapsed;
                    s.estimated_remaining_secs = 0.0;
                    running.store(false, Ordering::SeqCst);
                }
            });
        }
    }
}
