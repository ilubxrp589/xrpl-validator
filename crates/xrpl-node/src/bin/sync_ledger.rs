//! Fast full-state download — downloads the entire XRPL state tree at a
//! pinned ledger sequence directly into RocksDB.
//!
//! Uses a local rippled node (no rate limits) for maximum throughput.
//! Saves progress via marker so it can resume if interrupted.
//!
//! Run: cargo run --release -p xrpl-node --bin sync_ledger

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const RPC_ENDPOINT: &str = "http://10.0.0.97:5005";
const PAGE_SIZE: u32 = 2048;
const ROCKS_PATH: &str = "/mnt/xrpl-data/sync/state.rocks";
const MARKER_FILE: &str = "/mnt/xrpl-data/sync/dl_marker.txt";
const DONE_FILE: &str = "/mnt/xrpl-data/sync/dl_done.txt";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!("=== XRPL Full State Download (local rippled, no rate limits) ===\n");
    eprintln!("  RPC: {RPC_ENDPOINT}");
    eprintln!("  DB:  {ROCKS_PATH}\n");

    // Check if already completed
    if std::path::Path::new(DONE_FILE).exists() {
        let info = std::fs::read_to_string(DONE_FILE).unwrap_or_default();
        eprintln!("  Already completed! {info}");
        eprintln!("  Delete {DONE_FILE} to re-run.");
        return Ok(());
    }

    // Open RocksDB
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);
    opts.set_write_buffer_size(256 * 1024 * 1024); // 256MB write buffer
    opts.set_max_write_buffer_number(4);
    opts.set_target_file_size_base(128 * 1024 * 1024);
    opts.set_compression_type(rocksdb::DBCompressionType::None);
    let db = rocksdb::DB::open(&opts, ROCKS_PATH)?;
    let db = Arc::new(db);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(4)
        .build()?;

    // Step 1: Get validated ledger and pin to it
    eprintln!("[1/2] Fetching validated ledger...");
    let resp = rpc(&client, "ledger", json!({"ledger_index": "validated"})).await?;
    let ledger = &resp["result"]["ledger"];
    let ledger_seq: u32 = ledger["ledger_index"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| ledger["ledger_index"].as_u64().map(|n| n as u32))
        .unwrap_or(0);
    let account_hash = ledger["account_hash"].as_str().unwrap_or("???");

    if ledger_seq == 0 {
        anyhow::bail!("Could not get validated ledger from {RPC_ENDPOINT}");
    }

    eprintln!("  Ledger:       #{ledger_seq}");
    eprintln!("  account_hash: {account_hash}");
    eprintln!();

    // Step 2: Paginate through all state objects
    eprintln!("[2/2] Downloading all state objects...");
    let start = Instant::now();
    let total_objects = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));

    // Check for resume
    let mut marker: Option<String> = None;
    if let Ok(saved) = std::fs::read_to_string(MARKER_FILE) {
        let saved = saved.trim().to_string();
        if !saved.is_empty() {
            marker = Some(saved);
            // Estimate how many we already have
            let est = db
                .property_value("rocksdb.estimate-num-keys")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            total_objects.store(est, Ordering::Relaxed);
            eprintln!("  Resuming from marker (~{est} objects already in DB)");
        }
    }

    let mut page: u64 = 0;
    let mut consecutive_errors: u32 = 0;

    loop {
        page += 1;

        let params = if let Some(ref m) = marker {
            json!({
                "ledger_index": ledger_seq,
                "limit": PAGE_SIZE,
                "binary": true,
                "marker": m,
            })
        } else {
            json!({
                "ledger_index": ledger_seq,
                "limit": PAGE_SIZE,
                "binary": true,
            })
        };

        let resp = match rpc(&client, "ledger_data", params).await {
            Ok(r) => {
                consecutive_errors = 0;
                r
            }
            Err(e) => {
                consecutive_errors += 1;
                eprintln!("  [page {page}] Error: {e}");
                if consecutive_errors >= 20 {
                    eprintln!("  Too many errors, stopping. Re-run to resume.");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        let result = &resp["result"];
        if let Some(objects) = result["state"].as_array() {
            let mut batch = rocksdb::WriteBatch::default();
            let mut page_bytes: u64 = 0;

            for obj in objects {
                let index = obj["index"].as_str().unwrap_or("");
                let data_hex = obj["data"].as_str().unwrap_or("");
                if index.is_empty() || data_hex.is_empty() {
                    continue;
                }
                if let (Ok(key), Ok(data)) = (hex::decode(index), hex::decode(data_hex)) {
                    if key.len() == 32 {
                        page_bytes += data.len() as u64;
                        batch.put(&key, &data);
                        total_objects.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            total_bytes.fetch_add(page_bytes, Ordering::Relaxed);
            db.write(batch)?;
        }

        // Save marker for resume
        marker = result["marker"].as_str().map(String::from);
        if let Some(ref m) = marker {
            let _ = std::fs::write(MARKER_FILE, m);
        }

        // Progress every 10 pages
        if page % 10 == 0 || page <= 3 {
            let obj_count = total_objects.load(Ordering::Relaxed);
            let byte_count = total_bytes.load(Ordering::Relaxed);
            let elapsed = start.elapsed().as_secs_f64();
            let rate = obj_count as f64 / elapsed.max(0.1);
            let est_total = 30_000_000u64;
            let eta_min = if rate > 0.0 {
                (est_total.saturating_sub(obj_count)) as f64 / rate / 60.0
            } else {
                0.0
            };
            eprintln!(
                "  [page {:>6}] {:>10} objects | {:>7.1} MB | {:>6.0}/s | ETA ~{:.0}min",
                page,
                obj_count,
                byte_count as f64 / 1_048_576.0,
                rate,
                eta_min,
            );
        }

        if marker.is_none() {
            eprintln!("\n  Download complete!");
            break;
        }

        // No delay — local rippled, no rate limits
    }

    let elapsed = start.elapsed();
    let obj_count = total_objects.load(Ordering::Relaxed);
    let byte_count = total_bytes.load(Ordering::Relaxed);

    eprintln!();
    eprintln!("  Objects:  {obj_count}");
    eprintln!("  Size:     {:.1} GB", byte_count as f64 / 1_073_741_824.0);
    eprintln!("  Time:     {:.1} min", elapsed.as_secs_f64() / 60.0);
    eprintln!("  Rate:     {:.0}/s", obj_count as f64 / elapsed.as_secs_f64().max(0.1));
    eprintln!("  Pinned:   ledger #{ledger_seq}");
    eprintln!("  DB:       {ROCKS_PATH}");

    // Mark complete
    let info = format!(
        "ledger #{ledger_seq} — {obj_count} objects — {:.1}GB — {:.1}min",
        byte_count as f64 / 1_073_741_824.0,
        elapsed.as_secs_f64() / 60.0,
    );
    std::fs::write(DONE_FILE, &info)?;
    let _ = std::fs::remove_file(MARKER_FILE);

    eprintln!("\n  Saved to {DONE_FILE}");
    eprintln!("  Now restart live_viewer — it will rebuild the SHAMap from this consistent state.");

    Ok(())
}

async fn rpc(client: &reqwest::Client, method: &str, params: Value) -> anyhow::Result<Value> {
    let body = json!({ "method": method, "params": [params] });
    for attempt in 0..5u32 {
        match client.post(RPC_ENDPOINT).json(&body).send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if attempt < 4 {
                        tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
                        continue;
                    }
                    return Err(e.into());
                }
            },
            Err(e) => {
                if attempt < 4 {
                    tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
                    continue;
                }
                return Err(e.into());
            }
        }
    }
    anyhow::bail!("max retries exceeded")
}
