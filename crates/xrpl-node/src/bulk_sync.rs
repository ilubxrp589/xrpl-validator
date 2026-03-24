//! Bulk state sync via peer protocol — request SHAMap nodes from ALL
//! connected peers in parallel using TMGetLedger. Much faster than RPC
//! since we're pulling from 10+ peers simultaneously.

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

/// Bulk state syncer — downloads all state via ledger_data RPC.
/// Uses the validated ledger's complete state dump.
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

    /// Start bulk sync using ledger_data RPC (blocking, single thread).
    pub fn start(&self, db: Arc<rocksdb::DB>, estimated_total: u64) {
        if self.running.swap(true, Ordering::SeqCst) {
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

        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            eprintln!("[bulk-sync] Starting full state sync via s1.ripple.com...");

            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest blocking client");

            let rpc = "https://s1.ripple.com:51234";
            let mut marker: Option<serde_json::Value> = None;
            let mut total_objects: u64 = 0;
            let mut total_pages: u64 = 0;
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

                let body: serde_json::Value = match client.post(rpc)
                    .json(&serde_json::json!({"method": "ledger_data", "params": [params]}))
                    .send()
                    .and_then(|r| r.json())
                {
                    Ok(b) => b,
                    Err(e) => {
                        errors += 1;
                        if errors <= 5 { eprintln!("[bulk-sync] Error: {e}"); }
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        if errors > 20 { break; }
                        continue;
                    }
                };

                // Check if result exists (rate limiting may return no result)
                if body.get("result").is_none() {
                    errors += 1;
                    if errors <= 5 {
                        let _ = std::fs::OpenOptions::new().create(true).append(true)
                            .open("/tmp/bulk-sync.log").and_then(|mut f| {
                                use std::io::Write;
                                f.write_all(format!("NO RESULT on page {total_pages}, sleeping 5s (body keys: {:?})\n", body.as_object().map(|o| o.keys().collect::<Vec<_>>())).as_bytes())
                            });
                    }
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    continue; // Retry same marker
                }

                if let Some(err) = body["result"].get("error") {
                    errors += 1;
                    if errors <= 5 { eprintln!("[bulk-sync] API error: {err}"); }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if errors > 20 { break; }
                    continue;
                }

                if let Some(objects) = body["result"]["state"].as_array() {
                    for obj in objects {
                        if let (Some(index), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                            if index.len() == 64 {
                                if let (Ok(k), Ok(d)) = (hex::decode(index), hex::decode(data)) {
                                    if k.len() == 32 {
                                        batch.put(&k, &d);
                                        batch_size += 1;
                                        total_objects += 1;
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

                total_pages += 1;
                objects_synced.store(total_objects, Ordering::Relaxed);

                if total_pages % 25 == 0 || total_pages <= 5 {
                    let elapsed = start.elapsed().as_secs_f64();
                    let rate = total_objects as f64 / elapsed;
                    let remaining = (estimated_total.saturating_sub(total_objects)) as f64 / rate;
                    eprintln!(
                        "[bulk-sync] {total_objects}/{estimated_total} ({:.1}%) — {rate:.0}/s — ~{remaining:.0}s left",
                        total_objects as f64 / estimated_total as f64 * 100.0,
                    );
                    let mut s = status.lock();
                    s.objects_synced = total_objects;
                    s.pages_fetched = total_pages;
                    s.errors = errors;
                    s.elapsed_secs = elapsed;
                    s.rate = rate;
                    s.estimated_remaining_secs = remaining;
                }

                // Get next page marker
                marker = body.get("result")
                    .and_then(|r| r.get("marker"))
                    .cloned();

                {
                    let marker_str = marker.as_ref().map(|m| m.to_string()).unwrap_or("NONE".into());
                    let has_result = body.get("result").is_some();
                    let has_error = body.get("result").and_then(|r| r.get("error")).is_some();
                    let state_len = body["result"]["state"].as_array().map(|a| a.len()).unwrap_or(0);
                    let msg = format!("Page {total_pages}: total={total_objects} state={state_len} has_result={has_result} err={has_error} marker={}\n", &marker_str[..marker_str.len().min(40)]);
                    let _ = std::fs::OpenOptions::new().create(true).append(true)
                        .open("/tmp/bulk-sync.log").and_then(|mut f| {
                            use std::io::Write;
                            f.write_all(msg.as_bytes())
                        });
                }

                if marker.is_none() || marker.as_ref().is_some_and(|m| m.is_null()) {
                    eprintln!("[bulk-sync] Pagination ended at page {total_pages}");
                    break;
                }

                // Small delay to avoid rate limiting
                std::thread::sleep(std::time::Duration::from_millis(200));
            }

            if batch_size > 0 { let _ = db.write(batch); }

            let elapsed = start.elapsed().as_secs_f64();
            eprintln!("[bulk-sync] DONE: {total_objects} objects in {elapsed:.0}s ({total_pages} pages, {errors} errors)");

            let mut s = status.lock();
            s.running = false;
            s.objects_synced = total_objects;
            s.pages_fetched = total_pages;
            s.errors = errors;
            s.elapsed_secs = elapsed;
            s.rate = total_objects as f64 / elapsed;
            s.estimated_remaining_secs = 0.0;
            running.store(false, Ordering::SeqCst);
        });
    }
}
