//! Bulk state sync via WebSocket — bypasses Cloudflare HTTP rate limiting.
//!
//! Strategy:
//! - 4 servers, 4 workers each = 16 WS connections
//! - s1: 4 workers (1 full stream + 3 range via fast-forward)
//! - xrplcluster/xrpl.ws/xrpl.link: 4 range workers each
//! - WS connections stay open — no per-request rate limiting.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
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

// 3 HTTP endpoints, 1 worker each, non-overlapping keyspace ranges.
// s1/s2 don't support ledger_data over HTTP — only these 3 work.
// 1 page per second per worker to avoid bans.
const HTTP_ENDPOINTS: &[(&str, u8, u8)] = &[
    ("https://xrplcluster.com", 0x00, 0x55),
    ("https://xrpl.ws",         0x55, 0xAA),
    ("https://xrpl.link",       0xAA, 0xFF),
];
const PAGE_DELAY_MS: u64 = 1000;

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

        // Fetch the current validated ledger sequence ONCE — pin all workers to it.
        // This ensures every object comes from the same ledger = consistent hash.
        let pinned_seq: u32 = {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build().unwrap();
            let resp = client.post("https://xrpl.ws")
                .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
                .send();
            match resp {
                Ok(r) => {
                    if let Ok(body) = r.json::<serde_json::Value>() {
                        body["result"]["ledger_index"].as_u64()
                            .or_else(|| body["result"]["ledger"]["ledger_index"].as_u64())
                            .unwrap_or(0) as u32
                    } else { 0 }
                }
                Err(_) => 0,
            }
        };
        if pinned_seq == 0 {
            eprintln!("[sync] FATAL: could not fetch validated ledger seq — aborting bulk sync");
            self.running.store(false, Ordering::SeqCst);
            return;
        }
        eprintln!("[sync] Pinning ALL bulk sync to ledger #{pinned_seq}");
        let pinned = Arc::new(AtomicU32::new(pinned_seq));

        let total_workers = HTTP_ENDPOINTS.len() as u32;
        { let mut s = status.lock(); *s = BulkSyncStatus { running: true, workers_total: total_workers, ..Default::default() }; }

        let workers_done = Arc::new(AtomicU32::new(0));
        let start_time = std::time::Instant::now();

        // 5 HTTP workers, each covers its own keyspace range
        for (i, &(url, start_byte, end_byte)) in HTTP_ENDPOINTS.iter().enumerate() {
            let url = url.to_string();
            let label = format!("http{i}-{start_byte:02X}");
            let db = db.clone(); let synced = synced.clone(); let status = status.clone();
            let running = running.clone(); let wd = workers_done.clone(); let pin = pinned.clone();
            std::thread::spawn(move || {
                Self::run_throttled_http(
                    &url, start_byte, end_byte, &label,
                    &db, &synced, &status, &running, &wd,
                    total_workers, estimated_total, start_time, pin.load(Ordering::Relaxed),
                );
            });
        }
    }

    /// Single WS worker. If start_marker is None, does full stream (s1 style).
    fn run_ws_worker(
        url: &str, start_byte: Option<u8>, end_byte: u8, label: &str,
        db: &Arc<rocksdb::DB>, synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>, running: &Arc<AtomicBool>,
        workers_done: &Arc<AtomicU32>, total_workers: u32,
        estimated: u64, start: std::time::Instant, pinned_seq: u32,
    ) {
        let is_last = end_byte == 0xFF && start_byte.is_some();
        let is_full = start_byte.is_none();

        // Build initial marker
        let mut marker: Option<serde_json::Value> = start_byte.map(|sb| {
            let mut key = [0u8; 32];
            key[0] = sb;
            serde_json::Value::String(hex::encode(key))
        });

        let mut local_count = 0u64;
        let mut local_pages = 0u64;
        let mut errors = 0u64;
        let mut batch = rocksdb::WriteBatch::default();
        let mut batch_size = 0u32;

        // Connect with retry
        let mut ws = match Self::ws_connect(url, label) {
            Some(ws) => ws,
            None => {
                eprintln!("[{label}] Failed to connect after retries, falling back to HTTP");
                Self::run_http_fallback(
                    url, start_byte, end_byte, label,
                    db, synced, status, running, workers_done,
                    total_workers, estimated, start, pinned_seq,
                );
                return;
            }
        };

        loop {
            if !running.load(Ordering::Relaxed) { break; }

            // Build WS command (flat format, not JSON-RPC wrapped)
            let mut cmd = serde_json::json!({
                "command": "ledger_data",
                "ledger_index": pinned_seq,
                "binary": true,
                "limit": 2048
            });
            if let Some(ref m) = marker { cmd["marker"] = m.clone(); }

            // Send
            if let Err(e) = ws.send(tungstenite::Message::Text(cmd.to_string())) {
                errors += 1;
                eprintln!("[{label}] WS send error: {e}");
                if let Some(new_ws) = Self::ws_connect(url, label) {
                    ws = new_ws; continue;
                } else { break; }
            }

            // Read response
            let msg = match ws.read() {
                Ok(m) => m,
                Err(e) => {
                    errors += 1;
                    eprintln!("[{label}] WS read error: {e}");
                    if let Some(new_ws) = Self::ws_connect(url, label) {
                        ws = new_ws; continue;
                    } else { break; }
                }
            };

            let text = match msg {
                tungstenite::Message::Text(t) => t,
                tungstenite::Message::Close(_) => {
                    eprintln!("[{label}] WS closed by server");
                    if let Some(new_ws) = Self::ws_connect(url, label) {
                        ws = new_ws; continue;
                    } else { break; }
                }
                _ => continue, // ping/pong/binary — skip
            };

            let body: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => { errors += 1; continue; }
            };

            // Check for error responses
            if body.get("error").is_some() || body.get("status").and_then(|s| s.as_str()) == Some("error") {
                let err = body.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                if err == "slowDown" || err == "tooBusy" {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    continue;
                }
                errors += 1;
                if errors <= 5 || errors % 20 == 0 {
                    eprintln!("[{label}] Error: {err} ({errors} total)");
                }
                if errors > 200 { break; } continue;
            }

            // Result is in body["result"] (WS wraps it)
            let result = body.get("result").unwrap_or(&body);

            let mut past_end = false;
            let mut page_objs = 0u64;
            if let Some(objs) = result["state"].as_array() {
                for obj in objs {
                    if let (Some(idx), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                        // Range check for non-full-stream workers
                        if !is_full && !is_last && idx.len() >= 2 {
                            let fb = u8::from_str_radix(&idx[0..2], 16).unwrap_or(0);
                            if fb > end_byte { past_end = true; break; }
                        }
                        if idx.len() == 64 {
                            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(data)) {
                                if k.len() == 32 {
                                    batch.put(&k, &v);
                                    batch_size += 1;
                                    local_count += 1;
                                    page_objs += 1;
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

            synced.fetch_add(page_objs, Ordering::Relaxed);
            local_pages += 1;

            // Log progress
            if local_pages % 20 == 0 || local_pages == 1 {
                let gtotal = synced.load(Ordering::Relaxed);
                let elapsed = start.elapsed().as_secs_f64();
                let rate = gtotal as f64 / elapsed.max(0.1);
                let remaining = (estimated.saturating_sub(gtotal)) as f64 / rate.max(1.0);
                eprintln!("[{label}] pg {local_pages}: {local_count} local | {gtotal} global ({:.1}%) | {rate:.0}/s | ~{remaining:.0}s",
                    gtotal as f64 / estimated as f64 * 100.0);
                let mut s = status.lock();
                s.objects_synced = gtotal; s.elapsed_secs = elapsed; s.rate = rate;
                s.estimated_remaining_secs = remaining; s.errors = errors;
                s.pages_fetched = local_pages;
            }

            if past_end { break; }
            marker = result.get("marker").cloned();
            if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) { break; }

            // Throttle: 1 page per second to avoid bans
            std::thread::sleep(std::time::Duration::from_millis(PAGE_DELAY_MS));
        }
        if batch_size > 0 { let _ = db.write(batch); }

        let done = workers_done.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[{label}] DONE: {local_count} objs, {local_pages} pgs, {errors} errs. Workers: {done}/{total_workers}");
        Self::check_all_done(done, total_workers, synced, status, running, start);
    }

    /// Connect to WS with retries (up to 5 attempts).
    fn ws_connect(url: &str, label: &str) -> Option<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>> {
        for attempt in 1..=5 {
            match tungstenite::connect(url) {
                Ok((ws, _)) => {
                    eprintln!("[{label}] Connected to {url}");
                    return Some(ws);
                }
                Err(e) => {
                    eprintln!("[{label}] WS connect attempt {attempt}/5 failed: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
        None
    }

    /// HTTP fallback if WS fails entirely (uses the old approach).
    fn run_http_fallback(
        ws_url: &str, start_byte: Option<u8>, end_byte: u8, label: &str,
        db: &Arc<rocksdb::DB>, synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>, running: &Arc<AtomicBool>,
        workers_done: &Arc<AtomicU32>, total_workers: u32,
        estimated: u64, start: std::time::Instant, pinned_seq: u32,
    ) {
        // Convert wss:// URL to https:// for JSON-RPC
        let http_url = if ws_url.contains("s1.ripple.com") {
            "https://s1.ripple.com:51234".to_string()
        } else {
            ws_url.replace("wss://", "https://")
        };

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build().unwrap();

        let mut marker: Option<serde_json::Value> = start_byte.map(|sb| {
            let mut key = [0u8; 32]; key[0] = sb;
            serde_json::Value::String(hex::encode(key))
        });
        let mut local_count = 0u64;
        let mut local_pages = 0u64;
        let mut errors = 0u64;
        let mut batch = rocksdb::WriteBatch::default();
        let mut batch_size = 0u32;
        let is_full = start_byte.is_none();
        let is_last = end_byte == 0xFF && start_byte.is_some();

        eprintln!("[{label}] HTTP fallback → {http_url}");

        loop {
            if !running.load(Ordering::Relaxed) { break; }

            let mut params = serde_json::json!({"ledger_index":pinned_seq,"binary":true,"limit":2048});
            if let Some(ref m) = marker { params["marker"] = m.clone(); }

            let resp = match client.post(&http_url)
                .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
                .send()
            {
                Ok(r) => r,
                Err(_) => {
                    errors += 1;
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if errors > 100 { break; } continue;
                }
            };

            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let ra = resp.headers().get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(30);
                std::thread::sleep(std::time::Duration::from_secs(ra.min(60)));
                errors += 1; if errors > 100 { break; } continue;
            }

            let body: serde_json::Value = match resp.json() {
                Ok(b) => b,
                Err(_) => { errors += 1; std::thread::sleep(std::time::Duration::from_secs(2)); if errors > 100 { break; } continue; }
            };

            if body.get("result").is_none() {
                errors += 1;
                std::thread::sleep(std::time::Duration::from_secs(5));
                if errors > 100 { break; } continue;
            }

            let mut past_end = false;
            let mut page_objs = 0u64;
            if let Some(objs) = body["result"]["state"].as_array() {
                for obj in objs {
                    if let (Some(idx), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                        if !is_full && !is_last && idx.len() >= 2 {
                            let fb = u8::from_str_radix(&idx[0..2], 16).unwrap_or(0);
                            if fb > end_byte { past_end = true; break; }
                        }
                        if idx.len() == 64 {
                            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(data)) {
                                if k.len() == 32 { batch.put(&k, &v); batch_size += 1; local_count += 1; page_objs += 1; }
                            }
                        }
                    }
                }
            }
            if batch_size >= 10_000 { let _ = db.write(batch); batch = rocksdb::WriteBatch::default(); batch_size = 0; }
            synced.fetch_add(page_objs, Ordering::Relaxed);
            local_pages += 1;

            if local_pages % 20 == 0 || local_pages == 1 {
                let gtotal = synced.load(Ordering::Relaxed);
                let elapsed = start.elapsed().as_secs_f64();
                let rate = gtotal as f64 / elapsed.max(0.1);
                eprintln!("[{label}] HTTP pg {local_pages}: {local_count} | {gtotal} ({:.1}%) | {rate:.0}/s",
                    gtotal as f64 / estimated as f64 * 100.0);
                let remaining = (estimated.saturating_sub(gtotal)) as f64 / rate.max(1.0);
                let mut s = status.lock(); s.objects_synced = gtotal; s.elapsed_secs = elapsed;
                s.rate = rate; s.estimated_remaining_secs = remaining;
            }

            if past_end { break; }
            marker = body.get("result").and_then(|r| r.get("marker")).cloned();
            if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) { break; }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if batch_size > 0 { let _ = db.write(batch); }

        let done = workers_done.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[{label}] HTTP DONE: {local_count} objs. Workers: {done}/{total_workers}");
        Self::check_all_done(done, total_workers, synced, status, running, start);
    }

    /// Throttled HTTP worker — 1 page per PAGE_DELAY_MS, no rate limiting.
    fn run_throttled_http(
        url: &str, start_byte: u8, end_byte: u8, label: &str,
        db: &Arc<rocksdb::DB>, synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>, running: &Arc<AtomicBool>,
        workers_done: &Arc<AtomicU32>, total_workers: u32,
        estimated: u64, start: std::time::Instant, pinned_seq: u32,
    ) {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build().unwrap();

        let mut marker: Option<serde_json::Value> = {
            let mut key = [0u8; 32]; key[0] = start_byte;
            Some(serde_json::Value::String(hex::encode(key)))
        };
        let mut local_count = 0u64;
        let mut local_pages = 0u64;
        let mut errors = 0u64;
        let mut batch = rocksdb::WriteBatch::default();
        let mut batch_size = 0u32;
        let is_last = end_byte == 0xFF;

        eprintln!("[{label}] Throttled HTTP → {url} (0x{start_byte:02X}-0x{end_byte:02X})");

        loop {
            if !running.load(Ordering::Relaxed) { break; }

            let mut params = serde_json::json!({"ledger_index":pinned_seq,"binary":true,"limit":2048});
            if let Some(ref m) = marker { params["marker"] = m.clone(); }

            let resp = match client.post(url)
                .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
                .send()
            {
                Ok(r) => r,
                Err(_) => {
                    errors += 1;
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    if errors > 50 { break; } continue;
                }
            };

            let body: serde_json::Value = match resp.json() {
                Ok(b) => b,
                Err(_) => { errors += 1; std::thread::sleep(std::time::Duration::from_secs(5)); if errors > 50 { break; } continue; }
            };

            if body.get("result").is_none() {
                errors += 1;
                std::thread::sleep(std::time::Duration::from_secs(5));
                if errors > 50 { break; } continue;
            }

            let mut past_end = false;
            let mut page_objs = 0u64;
            if let Some(objs) = body["result"]["state"].as_array() {
                for obj in objs {
                    if let (Some(idx), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                        if !is_last && idx.len() >= 2 {
                            let fb = u8::from_str_radix(&idx[0..2], 16).unwrap_or(0);
                            if fb > end_byte { past_end = true; break; }
                        }
                        if idx.len() == 64 {
                            if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(data)) {
                                if k.len() == 32 { batch.put(&k, &v); batch_size += 1; local_count += 1; page_objs += 1; }
                            }
                        }
                    }
                }
            }
            if batch_size >= 10_000 { let _ = db.write(batch); batch = rocksdb::WriteBatch::default(); batch_size = 0; }
            synced.fetch_add(page_objs, Ordering::Relaxed);
            local_pages += 1;

            if local_pages % 20 == 0 || local_pages == 1 {
                let gtotal = synced.load(Ordering::Relaxed);
                let elapsed = start.elapsed().as_secs_f64();
                let rate = gtotal as f64 / elapsed.max(0.1);
                let remaining = (estimated.saturating_sub(gtotal)) as f64 / rate.max(1.0);
                eprintln!("[{label}] pg {local_pages}: {local_count} local | {gtotal} global ({:.1}%) | {rate:.0}/s | ~{remaining:.0}s",
                    gtotal as f64 / estimated as f64 * 100.0);
                let mut s = status.lock(); s.objects_synced = gtotal; s.elapsed_secs = elapsed;
                s.rate = rate; s.estimated_remaining_secs = remaining;
            }

            if past_end { break; }
            marker = body.get("result").and_then(|r| r.get("marker")).cloned();
            if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) { break; }

            // Steady throttle — no rate limiting
            std::thread::sleep(std::time::Duration::from_millis(PAGE_DELAY_MS));
        }
        if batch_size > 0 { let _ = db.write(batch); }

        let done = workers_done.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[{label}] DONE: {local_count} objs, {errors} errs. Workers: {done}/{total_workers}");
        Self::check_all_done(done, total_workers, synced, status, running, start);
    }

    fn check_all_done(
        done: u32, total_workers: u32, synced: &Arc<AtomicU64>,
        status: &Arc<Mutex<BulkSyncStatus>>, running: &Arc<AtomicBool>,
        start: std::time::Instant,
    ) {
        let total = synced.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        if done >= total_workers {
            eprintln!("[sync] ALL DONE: {total} objects in {elapsed:.0}s ({:.0}/s)", total as f64 / elapsed);
            let mut s = status.lock();
            s.running = false; s.objects_synced = total; s.elapsed_secs = elapsed;
            s.rate = total as f64 / elapsed; s.estimated_remaining_secs = 0.0; s.workers_done = done;
            running.store(false, Ordering::SeqCst);
        } else {
            let mut s = status.lock();
            s.workers_done = done; s.objects_synced = total; s.elapsed_secs = elapsed;
            s.rate = total as f64 / elapsed.max(0.1);
        }
    }
}
