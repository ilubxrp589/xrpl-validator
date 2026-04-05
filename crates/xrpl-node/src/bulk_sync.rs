//! Bulk state sync with inline SHAMap build.
//!
//! Workers download in parallel → send objects through unbounded channel.
//! Builder thread receives objects, writes to RocksDB AND builds SHAMap.
//! When download finishes, SHAMap is ready within seconds — zero gap.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use parking_lot::Mutex;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

const RPC_ENDPOINTS: &[&str] = &[
    "http://localhost:5005",        // local rippled RPC
    "https://xrplcluster.com",     // public fallback
    "https://s1.ripple.com:51234", // Ripple public
];

/// SECURITY(10.1): Read XRPL_RPC_URL to override the primary RPC endpoint.
fn primary_rpc_endpoint() -> String {
    std::env::var("XRPL_RPC_URL").unwrap_or_else(|_| RPC_ENDPOINTS[0].to_string())
}
const NUM_WORKERS: u32 = 4;

/// Try RPC request against each endpoint until one succeeds.
/// Uses XRPL_RPC_URL env override as the first endpoint if set.
fn rpc_failover(client: &reqwest::blocking::Client, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let primary = primary_rpc_endpoint();
    let endpoints: Vec<String> = std::iter::once(primary)
        .chain(RPC_ENDPOINTS.iter().map(|s| s.to_string()))
        .collect();
    for (i, url) in endpoints.iter().enumerate() {
        match client.post(url.as_str()).json(body).send() {
            Ok(resp) => match resp.json::<serde_json::Value>() {
                Ok(json) => {
                    if json["result"]["error"].as_str() == Some("noNetwork") { continue; }
                    if i > 0 { eprintln!("[sync] Failover to {url}"); }
                    return Ok(json);
                }
                Err(_) => continue,
            },
            Err(_) => continue,
        }
    }
    Err("all endpoints failed".into())
}

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
    pub shamap: Arc<Mutex<Option<SHAMap>>>,
    pub pinned_seq: Arc<AtomicU32>,
}

impl BulkSyncer {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(BulkSyncStatus::default())),
            running: Arc::new(AtomicBool::new(false)),
            objects_synced: Arc::new(AtomicU64::new(0)),
            shamap: Arc::new(Mutex::new(None)),
            pinned_seq: Arc::new(AtomicU32::new(0)),
        }
    }
    pub fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }
    pub fn objects_synced(&self) -> u64 { self.objects_synced.load(Ordering::Relaxed) }

    pub fn start(&self, db: Arc<rocksdb::DB>, _estimated_total: u64) {
        if self.running.swap(true, Ordering::SeqCst) { return; }

        let status = self.status.clone();
        let running = self.running.clone();
        let synced = self.objects_synced.clone();
        let shamap_out = self.shamap.clone();
        let pinned_out = self.pinned_seq.clone();
        synced.store(0, Ordering::Relaxed);

        let pinned_seq: u32 = {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build().unwrap();
            let body = serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]});
            match rpc_failover(&client, &body) {
                Ok(json) => {
                    json["result"]["ledger_index"].as_u64()
                        .or_else(|| json["result"]["ledger"]["ledger_index"]
                            .as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or(0) as u32
                }
                Err(_) => 0,
            }
        };
        if pinned_seq == 0 {
            eprintln!("[sync] FATAL: could not fetch validated ledger seq");
            self.running.store(false, Ordering::SeqCst);
            return;
        }
        pinned_out.store(pinned_seq, Ordering::SeqCst);
        eprintln!("[sync] Pinning to #{pinned_seq} ({NUM_WORKERS} workers + inline SHAMap)");

        { let mut s = status.lock(); *s = BulkSyncStatus { running: true, workers_total: NUM_WORKERS, ..Default::default() }; }

        // SECURITY(4.2): Bounded channel — workers block when builder falls behind
        // instead of accumulating unbounded memory. 32 batches ≈ ~64K objects max queued.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<([u8; 32], Vec<u8>)>>(32);
        let start_time = std::time::Instant::now();
        let range_size = 256u32 / NUM_WORKERS;

        // Spawn download workers — each sends batches of (key, data) through channel
        for i in 0..NUM_WORKERS {
            let start_byte = (i * range_size) as u8;
            let end_byte = if i == NUM_WORKERS - 1 { 0xFF } else { ((i + 1) * range_size - 1) as u8 };
            let is_last = i == NUM_WORKERS - 1;
            let tx = tx.clone();
            let synced = synced.clone();
            let running = running.clone();
            let status = status.clone();

            std::thread::spawn(move || {
                let label = format!("w{i}:{start_byte:02X}-{end_byte:02X}");
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(60)).build().unwrap();
                let mut marker: Option<String> = if i == 0 { None } else {
                    let mut k = [0u8; 32]; k[0] = start_byte; Some(hex::encode(k))
                };
                let mut local_count = 0u64;
                let mut local_pages = 0u64;
                let mut errors = 0u64;

                loop {
                    if !running.load(Ordering::Relaxed) { break; }
                    let mut params = serde_json::json!({"ledger_index":pinned_seq,"binary":true,"limit":2048});
                    if let Some(ref m) = marker { params["marker"] = serde_json::Value::String(m.clone()); }

                    // CRITICAL: use ONLY the first endpoint for pagination.
                    // rpc_failover can switch servers between pages, corrupting markers.
                    let primary = primary_rpc_endpoint();
                    let resp = match client.post(&primary)
                        .json(&serde_json::json!({"method":"ledger_data","params":[params]}))
                        .send() {
                        Ok(r) => r,
                        Err(_) => { errors += 1; std::thread::sleep(std::time::Duration::from_secs(2));
                            if errors > 50 { break; } continue; }
                    };
                    let body: serde_json::Value = match resp.json() {
                        Ok(b) => b,
                        Err(_) => { errors += 1; std::thread::sleep(std::time::Duration::from_secs(2));
                            if errors > 50 { break; } continue; }
                    };

                    let mut page_batch: Vec<([u8; 32], Vec<u8>)> = Vec::with_capacity(2048);
                    let mut past_end = false;
                    if let Some(objs) = body["result"]["state"].as_array() {
                        for obj in objs {
                            if let (Some(idx), Some(data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                                if !is_last && idx.len() >= 2 {
                                    let fb = u8::from_str_radix(&idx[0..2], 16).unwrap_or(0);
                                    if fb > end_byte { past_end = true; break; }
                                }
                                if idx.len() == 64 {
                                    if let (Ok(k), Ok(v)) = (hex::decode(idx), hex::decode(data)) {
                                        if k.len() == 32 {
                                            let mut key = [0u8; 32]; key.copy_from_slice(&k);
                                            page_batch.push((key, v));
                                            local_count += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let page_len = page_batch.len() as u64;
                    if !page_batch.is_empty() {
                        let _ = tx.send(page_batch);
                    }
                    synced.fetch_add(page_len, Ordering::Relaxed);
                    local_pages += 1;

                    if local_pages % 100 == 0 || local_pages == 1 {
                        let gt = synced.load(Ordering::Relaxed);
                        let el = start_time.elapsed().as_secs_f64();
                        let rate = gt as f64 / el.max(0.1);
                        eprintln!("[{label}] pg {local_pages}: {local_count} | {gt} ({:.1}%) | {rate:.0}/s",
                            gt as f64 / 19_000_000.0 * 100.0);
                        let mut s = status.lock(); s.objects_synced = gt; s.elapsed_secs = el; s.rate = rate;
                    }

                    if past_end { break; }
                    marker = body["result"]["marker"].as_str().map(String::from);
                    if marker.is_none() { break; }
                }
                eprintln!("[{label}] DONE: {local_count} objects, {errors} errors");
            });
        }
        // Drop our sender so channel closes when all workers finish
        drop(tx);

        // Builder thread: receives batches, writes to RocksDB, builds SHAMap inline
        std::thread::spawn(move || {
            let mut shamap = SHAMap::new(TreeType::State);
            let mut count = 0u64;
            let mut batch = rocksdb::WriteBatch::default();
            let mut batch_size = 0u32;

            while let Ok(page_batch) = rx.recv() {
                for (key, data) in &page_batch {
                    // Write to RocksDB
                    batch.put(key, data);
                    batch_size += 1;

                    // Compute leaf hash + insert into SHAMap
                    let mut buf = Vec::with_capacity(data.len() + 32);
                    buf.extend_from_slice(data);
                    buf.extend_from_slice(key);
                    let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                    let _ = shamap.insert_hash_only(Hash256(*key), lh);
                    count += 1;
                }

                if batch_size >= 10_000 {
                    if let Err(e) = db.write(batch) {
                        eprintln!("[sync] RocksDB write failed: {e}");
                    }
                    batch = rocksdb::WriteBatch::default();
                    batch_size = 0;
                }
            }

            if batch_size > 0 {
                if let Err(e) = db.write(batch) {
                    eprintln!("[sync] Final RocksDB write failed: {e}");
                }
            }

            let root = shamap.root_hash();
            let elapsed = start_time.elapsed().as_secs_f64();
            eprintln!("[sync] DONE: {count} objects in {elapsed:.0}s ({:.0}/s) — SHAMap hash={}",
                count as f64 / elapsed, hex::encode(&root.0[..8]));

            let data_dir = std::env::var("XRPL_DATA_DIR")
                .unwrap_or_else(|_| "/mnt/xrpl-data/sync".to_string());
            let _ = std::fs::write(
                format!("{data_dir}/dl_done.txt"),
                format!("ledger #{pinned_seq} — {count} objects"),
            );

            *shamap_out.lock() = Some(shamap);
            let mut s = status.lock();
            s.running = false;
            s.objects_synced = count;
            s.elapsed_secs = elapsed;
            s.rate = count as f64 / elapsed;
            s.estimated_remaining_secs = 0.0;
            s.workers_done = NUM_WORKERS;
            running.store(false, Ordering::SeqCst);
        });
    }
}
