//! Bulk state sync — s1 does the FULL ledger (fast, no markers needed),
//! other servers help in parallel with their nibble ranges.
//! s1 keeps going through ALL data while others fill in gaps.
//! Whoever writes an object first wins (RocksDB overwrites are fine).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
        let synced = self.objects_synced.clone();
        synced.store(0, Ordering::Relaxed);
        { let mut s = status.lock(); *s = BulkSyncStatus { running: true, workers_total: 7, ..Default::default() }; }

        let workers_done = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let start_time = std::time::Instant::now();

        // Worker 0: s1 — pages through the ENTIRE ledger (no marker, ~6K/s)
        // This alone gets all 18.7M objects. The others just speed up the process.
        {
            let db = db.clone(); let synced = synced.clone(); let status = status.clone();
            let running = running.clone(); let workers_done = workers_done.clone();
            std::thread::spawn(move || {
                Self::run_full_stream("https://s1.ripple.com:51234", &db, &synced, &status, &running, &workers_done, 7, estimated_total, start_time);
            });
        }

        // Workers 1-6: other servers, each with a 2-nibble range via marker
        let helpers: Vec<(&str, u8, u8)> = vec![
            ("https://xrplcluster.com", 0x40, 0x70),
            ("https://xrplcluster.com", 0x70, 0xA0),
            ("https://xrpl.ws",         0xA0, 0xC0),
            ("https://xrpl.ws",         0xC0, 0xE0),
            ("https://xrpl.link",       0xE0, 0xFF),
            ("https://xrpl.link",       0x20, 0x40),
        ];

        for (server, start_byte, end_byte) in helpers {
            let db = db.clone(); let synced = synced.clone(); let status = status.clone();
            let running = running.clone(); let workers_done = workers_done.clone();
            let server = server.to_string();
            std::thread::spawn(move || {
                Self::run_range(&server, start_byte, end_byte, &db, &synced, &status, &running, &workers_done, 7, estimated_total, start_time);
            });
        }
    }

    /// Full stream: page through entire ledger from beginning, no marker.
    fn run_full_stream(
        rpc: &str, db: &Arc<rocksdb::DB>, synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>, running: &Arc<AtomicBool>,
        workers_done: &Arc<std::sync::atomic::AtomicU32>, total_workers: u32,
        estimated: u64, start: std::time::Instant,
    ) {
        let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();
        let mut marker: Option<serde_json::Value> = None;
        let mut total: u64 = 0;
        let mut pages: u64 = 0;
        let mut errors: u64 = 0;
        let mut batch = rocksdb::WriteBatch::default();
        let mut bs = 0u32;

        loop {
            let mut params = serde_json::json!({"ledger_index":"validated","binary":true,"limit":2048});
            if let Some(ref m) = marker { params["marker"] = m.clone(); }

            let body: serde_json::Value = match client.post(rpc)
                .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
                .send().and_then(|r| r.json())
            { Ok(b) => b, Err(_) => { errors += 1; std::thread::sleep(std::time::Duration::from_secs(2)); if errors > 100 { break; } continue; } };

            if body.get("result").is_none() { errors += 1; std::thread::sleep(std::time::Duration::from_secs(5)); if errors > 100 { break; } continue; }

            if let Some(objs) = body["result"]["state"].as_array() {
                for obj in objs {
                    if let (Some(i), Some(d)) = (obj["index"].as_str(), obj["data"].as_str()) {
                        if i.len() == 64 { if let (Ok(k), Ok(v)) = (hex::decode(i), hex::decode(d)) { if k.len() == 32 { batch.put(&k, &v); bs += 1; total += 1; } } }
                    }
                }
            }
            if bs >= 20_000 { let _ = db.write(batch); batch = rocksdb::WriteBatch::default(); bs = 0; }

            pages += 1;
            synced.fetch_add(2048.min(total), Ordering::Relaxed);
            // Only s1 updates the global counter directly
            synced.store(synced.load(Ordering::Relaxed).max(total), Ordering::Relaxed);

            if pages % 100 == 0 || pages <= 3 {
                let elapsed = start.elapsed().as_secs_f64();
                let gtotal = synced.load(Ordering::Relaxed);
                let rate = gtotal as f64 / elapsed;
                let remaining = (estimated.saturating_sub(gtotal)) as f64 / rate.max(1.0);
                eprintln!("[s1] {total} ({:.1}%) — {rate:.0}/s — ~{remaining:.0}s", total as f64 / estimated as f64 * 100.0);
                let mut s = status.lock(); s.objects_synced = gtotal; s.pages_fetched = pages; s.errors = errors;
                s.elapsed_secs = elapsed; s.rate = rate; s.estimated_remaining_secs = remaining;
            }

            marker = body.get("result").and_then(|r| r.get("marker")).cloned();
            if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) { break; }
        }
        if bs > 0 { let _ = db.write(batch); }

        let done = workers_done.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[s1] DONE: {total} objects, {pages} pages. Workers: {done}/{total_workers}");
        Self::check_all_done(done, total_workers, synced, status, running, start);
    }

    /// Range stream: start from a marker, stop when past end_byte.
    fn run_range(
        rpc: &str, start_byte: u8, end_byte: u8,
        db: &Arc<rocksdb::DB>, synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>, running: &Arc<AtomicBool>,
        workers_done: &Arc<std::sync::atomic::AtomicU32>, total_workers: u32,
        estimated: u64, start: std::time::Instant,
    ) {
        let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();
        let mut key = [0u8; 32]; key[0] = start_byte;
        let mut marker: Option<serde_json::Value> = Some(serde_json::Value::String(hex::encode(key)));
        let mut total: u64 = 0;
        let mut errors: u64 = 0;
        let mut batch = rocksdb::WriteBatch::default();
        let mut bs = 0u32;

        loop {
            let mut params = serde_json::json!({"ledger_index":"validated","binary":true,"limit":2048});
            if let Some(ref m) = marker { params["marker"] = m.clone(); }

            let body: serde_json::Value = match client.post(rpc)
                .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
                .send().and_then(|r| r.json())
            { Ok(b) => b, Err(_) => { errors += 1; std::thread::sleep(std::time::Duration::from_secs(2)); if errors > 50 { break; } continue; } };

            if body.get("result").is_none() { errors += 1; std::thread::sleep(std::time::Duration::from_secs(5)); if errors > 50 { break; } continue; }
            if body["result"].get("error").is_some() { errors += 1; std::thread::sleep(std::time::Duration::from_secs(2)); if errors > 50 { break; } continue; }

            let mut past = false;
            if let Some(objs) = body["result"]["state"].as_array() {
                for obj in objs {
                    if let (Some(i), Some(d)) = (obj["index"].as_str(), obj["data"].as_str()) {
                        if i.len() >= 2 {
                            let fb = u8::from_str_radix(&i[0..2], 16).unwrap_or(0);
                            if fb >= end_byte { past = true; break; }
                        }
                        if i.len() == 64 { if let (Ok(k), Ok(v)) = (hex::decode(i), hex::decode(d)) { if k.len() == 32 { batch.put(&k, &v); bs += 1; total += 1; } } }
                    }
                }
            }
            if bs >= 20_000 { let _ = db.write(batch); batch = rocksdb::WriteBatch::default(); bs = 0; }
            synced.fetch_add(total.min(2048), Ordering::Relaxed);

            if past { break; }
            marker = body.get("result").and_then(|r| r.get("marker")).cloned();
            if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) { break; }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if bs > 0 { let _ = db.write(batch); }

        let done = workers_done.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[helper-{start_byte:02X}] DONE: {total} objects. Workers: {done}/{total_workers}");
        Self::check_all_done(done, total_workers, synced, status, running, start);
    }

    fn check_all_done(
        done: u32, total_workers: u32, synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>, running: &Arc<AtomicBool>,
        start: std::time::Instant,
    ) {
        if done >= total_workers {
            let total = synced.load(Ordering::Relaxed);
            let elapsed = start.elapsed().as_secs_f64();
            eprintln!("[sync] ALL DONE: {total} objects in {elapsed:.0}s");
            let mut s = status.lock();
            s.running = false; s.objects_synced = total; s.elapsed_secs = elapsed;
            s.rate = total as f64 / elapsed; s.estimated_remaining_secs = 0.0; s.workers_done = done;
            running.store(false, Ordering::SeqCst);
        } else {
            status.lock().workers_done = done;
        }
    }
}
