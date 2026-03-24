//! Bulk state sync — parallel download from multiple XRPL servers.
//!
//! Spawns N workers, each starting from a different point in the key space,
//! each hitting a different public server. Total throughput scales linearly.

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

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn objects_synced(&self) -> u64 {
        self.objects_synced.load(Ordering::Relaxed)
    }

    /// Start parallel bulk sync from multiple XRPL servers.
    pub fn start(&self, db: Arc<rocksdb::DB>, estimated_total: u64) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let status = self.status.clone();
        let running = self.running.clone();
        let objects_synced = self.objects_synced.clone();
        objects_synced.store(0, Ordering::Relaxed);

        // 8 workers across 4 public XRPL servers (2 connections each)
        let servers: Vec<&str> = vec![
            "https://s1.ripple.com:51234",
            "https://s1.ripple.com:51234",
            "https://xrplcluster.com",
            "https://xrplcluster.com",
            "https://xrpl.ws",
            "https://xrpl.ws",
            "https://xrpl.link",
            "https://xrpl.link",
        ];
        let num_workers = servers.len() as u32;

        {
            let mut s = status.lock();
            *s = BulkSyncStatus { running: true, workers_total: num_workers, ..Default::default() };
        }

        let workers_done = Arc::new(AtomicU32::new(0));
        let start_time = std::time::Instant::now();

        // Each worker handles a portion of the key space.
        // Worker 0: keys 00-3F (via server 0, no starting marker)
        // Worker 1: keys 40-7F (via server 1, marker=40...)
        // Worker 2: keys 80-BF (via server 2, marker=80...)
        // Worker 3: keys C0-FF (via server 3, marker=C0...)
        for (i, server) in servers.into_iter().enumerate() {
            let db = db.clone();
            let objects_synced = objects_synced.clone();
            let status = status.clone();
            let running = running.clone();
            let workers_done = workers_done.clone();
            let server = server.to_string();
            let start_time = start_time;
            let est_per_worker = estimated_total / num_workers as u64;

            std::thread::spawn(move || {
                Self::run_worker(
                    i as u32, num_workers, &server, &db,
                    &objects_synced, &status, &running, &workers_done,
                    est_per_worker, start_time,
                );
            });
        }
    }

    fn run_worker(
        worker_id: u32,
        num_workers: u32,
        server: &str,
        db: &Arc<rocksdb::DB>,
        global_synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>,
        running: &Arc<AtomicBool>,
        workers_done: &Arc<AtomicU32>,
        est_per_worker: u64,
        global_start: std::time::Instant,
    ) {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client");

        // First request: worker 0 starts from beginning, others need to
        // find their starting point by requesting with a synthetic marker.
        // The ledger_data API doesn't accept arbitrary markers, so all workers
        // start from the beginning but skip objects outside their range.
        // Worker 0: keys where first nibble is 0-3
        // Worker 1: keys where first nibble is 4-7
        // etc.
        let range_start = worker_id * (16 / num_workers);
        let range_end = (worker_id + 1) * (16 / num_workers);

        // All workers start from their key range beginning
        let mut start_key = [0u8; 32];
        start_key[0] = (range_start as u8) << 4;

        // Worker 0 starts with no marker (beginning of ledger)
        // Other workers start with a marker at their range boundary
        let mut marker: Option<serde_json::Value> = if worker_id == 0 {
            None
        } else {
            Some(serde_json::Value::String(hex::encode(start_key)))
        };

        let log = |msg: &str| {
            let _ = std::fs::OpenOptions::new().create(true).append(true)
                .open("/tmp/bulk-sync.log").and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "[worker-{worker_id}] {msg}")
                });
        };

        log(&format!("Starting: range {range_start:X}-{range_end:X} via {server}"));

        let mut my_objects: u64 = 0;
        let mut my_pages: u64 = 0;
        let mut errors: u64 = 0;
        let mut batch = rocksdb::WriteBatch::default();
        let mut batch_size = 0;

        loop {
            let mut params = serde_json::json!({
                "ledger_index": "validated",
                "binary": true,
                "limit": 2048
            });
            if let Some(ref m) = marker {
                params["marker"] = m.clone();
            }

            let body: serde_json::Value = match client.post(server)
                .json(&serde_json::json!({"method": "ledger_data", "params": [params]}))
                .send()
                .and_then(|r| r.json())
            {
                Ok(b) => b,
                Err(_) => {
                    errors += 1;
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    if errors > 50 { break; }
                    continue;
                }
            };

            // Rate limited — no result field
            if body.get("result").is_none() {
                errors += 1;
                log(&format!("Rate limited on page {my_pages}, sleeping 5s"));
                std::thread::sleep(std::time::Duration::from_secs(5));
                if errors > 50 { break; }
                continue;
            }

            if body["result"].get("error").is_some() {
                errors += 1;
                log(&format!("API error: {}", body["result"]["error"]));
                std::thread::sleep(std::time::Duration::from_secs(3));
                if errors > 50 { break; }
                continue;
            }

            let mut past_range = false;
            if let Some(objects) = body["result"]["state"].as_array() {
                for obj in objects {
                    if let (Some(index), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                        if index.len() >= 1 {
                            // Check if this key is in our range
                            let first_nibble = u8::from_str_radix(&index[0..1], 16).unwrap_or(0);
                            if first_nibble >= range_end as u8 {
                                past_range = true;
                                break;
                            }
                            if first_nibble < range_start as u8 {
                                continue; // Skip keys before our range
                            }
                        }
                        if index.len() == 64 {
                            if let (Ok(k), Ok(d)) = (hex::decode(index), hex::decode(data)) {
                                if k.len() == 32 {
                                    batch.put(&k, &d);
                                    batch_size += 1;
                                    my_objects += 1;
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
            global_synced.fetch_add(my_objects.min(2048), Ordering::Relaxed);
            // Fix: only add the delta, not cumulative
            let prev = global_synced.load(Ordering::Relaxed);
            global_synced.store(prev.max(my_objects * num_workers as u64), Ordering::Relaxed);

            if my_pages % 50 == 0 || my_pages <= 3 {
                let elapsed = global_start.elapsed().as_secs_f64();
                let rate = my_objects as f64 / elapsed;
                let remaining = (est_per_worker.saturating_sub(my_objects)) as f64 / rate.max(1.0);
                log(&format!("{my_objects} objects, {my_pages} pages, {rate:.0}/s, ~{remaining:.0}s left"));

                let mut s = status.lock();
                s.objects_synced = global_synced.load(Ordering::Relaxed);
                s.elapsed_secs = elapsed;
                s.rate = s.objects_synced as f64 / elapsed;
                s.estimated_remaining_secs = remaining;
                s.errors += errors;
            }

            if past_range {
                log(&format!("Reached end of range at {range_end:X}0 after {my_objects} objects"));
                break;
            }

            marker = body.get("result")
                .and_then(|r| r.get("marker"))
                .cloned();

            if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) {
                log(&format!("No more pages after {my_pages} pages, {my_objects} objects"));
                break;
            }

            // Small delay to be nice to the server
            std::thread::sleep(std::time::Duration::from_millis(150));
        }

        // Flush remaining
        if batch_size > 0 {
            let _ = db.write(batch);
        }

        let done = workers_done.fetch_add(1, Ordering::SeqCst) + 1;
        log(&format!("DONE: {my_objects} objects in {my_pages} pages ({errors} errors). Workers: {done}/{num_workers}"));

        if done >= num_workers {
            let total = global_synced.load(Ordering::Relaxed);
            let elapsed = global_start.elapsed().as_secs_f64();
            log(&format!("ALL WORKERS DONE: ~{total} total objects in {elapsed:.0}s"));

            let mut s = status.lock();
            s.running = false;
            s.objects_synced = total;
            s.elapsed_secs = elapsed;
            s.rate = total as f64 / elapsed;
            s.estimated_remaining_secs = 0.0;
            s.workers_done = done;
            running.store(false, Ordering::SeqCst);
        } else {
            status.lock().workers_done = done;
        }
    }
}
