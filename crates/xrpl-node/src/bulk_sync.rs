//! Bulk state sync — 8 parallel workers across 4 XRPL servers.
//! Work-stealing queue: fast workers grab more nibble ranges.
//! s1 workers start from beginning (no marker), others use markers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;

#[derive(Clone, Default, serde::Serialize)]
pub struct BulkSyncStatus {
    pub running: bool,
    pub objects_synced: u64,
    pub pages_fetched: u64,
    pub errors: u64,
    pub elapsed_secs: f64,
    pub rate: f64,
    pub estimated_remaining_secs: f64,
    pub workers_done: u32,
    pub workers_total: u32,
}

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

    pub fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }
    pub fn objects_synced(&self) -> u64 { self.objects_synced.load(Ordering::Relaxed) }

    pub fn start(&self, db: Arc<rocksdb::DB>, estimated_total: u64) {
        if self.running.swap(true, Ordering::SeqCst) { return; }

        let status = self.status.clone();
        let running = self.running.clone();
        let objects_synced = self.objects_synced.clone();
        objects_synced.store(0, Ordering::Relaxed);
        { let mut s = status.lock(); *s = BulkSyncStatus { running: true, workers_total: 8, ..Default::default() }; }

        // Work queue: nibble ranges 1-F for marker-capable servers.
        // Nibble 0 is reserved for s1 (which can't use markers).
        let work_queue: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(
            (1..16u8).rev().collect() // [F, E, D, ..., 2, 1]
        ));
        // s1 gets nibble 0 directly (only one s1 worker needs it)
        let s1_nibble_taken = Arc::new(AtomicBool::new(false));

        let workers_done = Arc::new(AtomicU32::new(0));
        let start_time = std::time::Instant::now();

        // 2x s1 (can only do nibble 0, but blazing fast at ~6K/s)
        // 2x xrplcluster, 2x xrpl.ws, 2x xrpl.link (accept markers, ~1.1K/s each)
        let servers = vec![
            ("https://s1.ripple.com:51234", true),  // s1 — no marker support
            ("https://s1.ripple.com:51234", true),  // s1 — no marker support
            ("https://xrplcluster.com", false),
            ("https://xrplcluster.com", false),
            ("https://xrpl.ws", false),
            ("https://xrpl.ws", false),
            ("https://xrpl.link", false),
            ("https://xrpl.link", false),
        ];

        for (i, (server, no_marker)) in servers.into_iter().enumerate() {
            let db = db.clone();
            let objects_synced = objects_synced.clone();
            let status = status.clone();
            let running = running.clone();
            let workers_done = workers_done.clone();
            let work_queue = work_queue.clone();
            let s1_nibble_taken = s1_nibble_taken.clone();
            let server = server.to_string();

            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build().unwrap();

                let log = |msg: &str| {
                    let _ = std::fs::OpenOptions::new().create(true).append(true)
                        .open("/tmp/bulk-sync.log").and_then(|mut f| {
                            use std::io::Write; writeln!(f, "[w{i}] {msg}")
                        });
                };

                let mut my_objects: u64 = 0;
                let mut my_pages: u64 = 0;

                loop {
                    // Grab next nibble
                    let nibble = if no_marker {
                        // s1: grab nibble 0 (reserved, no marker needed)
                        if !s1_nibble_taken.swap(true, Ordering::SeqCst) {
                            Some(0u8)
                        } else {
                            None // other s1 worker — nothing to do
                        }
                    } else {
                        work_queue.lock().pop() // other servers: grab from queue
                    };
                    let nibble = match nibble {
                        Some(n) => n,
                        None => break,
                    };

                    log(&format!("Nibble {nibble:X} via {server}"));

                    let range_end = nibble + 1;
                    let mut marker: Option<serde_json::Value> = if nibble == 0 {
                        None
                    } else {
                        let mut k = [0u8; 32];
                        k[0] = nibble << 4;
                        Some(serde_json::Value::String(hex::encode(k)))
                    };

                    let mut batch = rocksdb::WriteBatch::default();
                    let mut batch_size = 0u32;
                    let mut range_objects: u64 = 0;
                    let mut errors = 0u32;

                    loop {
                        let mut params = serde_json::json!({
                            "ledger_index": "validated", "binary": true, "limit": 2048
                        });
                        if let Some(ref m) = marker { params["marker"] = m.clone(); }

                        let body: serde_json::Value = match client.post(&server)
                            .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
                            .send().and_then(|r| r.json())
                        {
                            Ok(b) => b,
                            Err(_) => {
                                errors += 1;
                                std::thread::sleep(std::time::Duration::from_secs(2));
                                if errors > 30 { break; } continue;
                            }
                        };

                        if body.get("result").is_none() {
                            errors += 1;
                            std::thread::sleep(std::time::Duration::from_secs(5));
                            if errors > 30 { break; } continue;
                        }
                        if body["result"].get("error").is_some() {
                            errors += 1;
                            if errors <= 3 { log(&format!("Error on nibble {nibble:X}: {}", body["result"]["error"])); }
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            if errors > 30 { break; } continue;
                        }

                        let mut past_range = false;
                        if let Some(objs) = body["result"]["state"].as_array() {
                            for obj in objs {
                                if let (Some(idx), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                                    if idx.len() >= 1 {
                                        let n = u8::from_str_radix(&idx[0..1], 16).unwrap_or(0);
                                        if n >= range_end { past_range = true; break; }
                                        if n < nibble { continue; }
                                    }
                                    if idx.len() == 64 {
                                        if let (Ok(k), Ok(d)) = (hex::decode(idx), hex::decode(data)) {
                                            if k.len() == 32 {
                                                batch.put(&k, &d);
                                                batch_size += 1;
                                                range_objects += 1;
                                            }
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

                        if past_range { break; }
                        marker = body.get("result").and_then(|r| r.get("marker")).cloned();
                        if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) { break; }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }

                    if batch_size > 0 { let _ = db.write(batch); }
                    my_objects += range_objects;
                    objects_synced.fetch_add(range_objects, Ordering::Relaxed);

                    log(&format!("Nibble {nibble:X} done: {range_objects} objects ({errors} err)"));

                    let elapsed = start_time.elapsed().as_secs_f64();
                    let total = objects_synced.load(Ordering::Relaxed);
                    let rate = total as f64 / elapsed;
                    let remaining = (estimated_total.saturating_sub(total)) as f64 / rate.max(1.0);
                    { let mut s = status.lock(); s.objects_synced = total; s.elapsed_secs = elapsed; s.rate = rate; s.estimated_remaining_secs = remaining; }
                }

                let done = workers_done.fetch_add(1, Ordering::SeqCst) + 1;
                log(&format!("Done: {my_objects} objects, {my_pages} pages. Workers: {done}/8"));

                if done >= 8 {
                    let total = objects_synced.load(Ordering::Relaxed);
                    let elapsed = start_time.elapsed().as_secs_f64();
                    log(&format!("ALL DONE: {total} objects in {elapsed:.0}s ({:.0}/s)", total as f64 / elapsed));
                    let mut s = status.lock();
                    s.running = false; s.objects_synced = total; s.elapsed_secs = elapsed;
                    s.rate = total as f64 / elapsed; s.estimated_remaining_secs = 0.0; s.workers_done = done;
                    running.store(false, Ordering::SeqCst);
                } else { status.lock().workers_done = done; }
            });
        }
    }
}
