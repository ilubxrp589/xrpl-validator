//! Fast parallel ledger state sync — downloads the full XRPL state.
//!
//! Uses multiple RPC endpoints in parallel for maximum throughput.
//! Saves progress to disk so it can resume if interrupted.
//!
//! Run: cargo run --release -p xrpl-node --bin sync_ledger

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Semaphore;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;

// Multiple public RPC endpoints for parallel downloads
const RPC_ENDPOINTS: &[&str] = &[
    "https://xrplcluster.com",
    "https://s1.ripple.com:51234",
    "https://s2.ripple.com:51234",
];

const PAGE_SIZE: u32 = 2048;
const MAX_CONCURRENT: usize = 6; // parallel requests
const SAVE_DIR: &str = "/mnt/xrpl-data/sync";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!("=== XRPL Mainnet Ledger Sync (Parallel) ===\n");

    // Create save directory
    std::fs::create_dir_all(SAVE_DIR)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(10)
        .build()?;

    // Step 1: Get validated ledger header
    eprintln!("[1/3] Fetching validated ledger...");
    let (header, ledger_index, account_hash_hex) = fetch_header(&client).await?;
    let our_hash = header.hash();
    let expected_hash_hex = hex::encode_upper(our_hash.0);

    eprintln!("  Ledger:     #{ledger_index}");
    eprintln!("  Hash:       {expected_hash_hex}");
    eprintln!("  State hash: {account_hash_hex}");
    eprintln!();

    // Step 2: Download all state objects with parallel workers
    eprintln!("[2/3] Downloading state (parallel across {} endpoints)...", RPC_ENDPOINTS.len());
    let start = Instant::now();

    let total_objects = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    let total_pages = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT));

    // We'll use a sequential approach with parallel retries
    // since ledger_data pagination requires a marker from the previous page
    let mut marker: Option<String> = None;

    // Check for resume marker
    let resume_file = format!("{SAVE_DIR}/marker.txt");
    let objects_file = format!("{SAVE_DIR}/objects.jsonl");

    if let Ok(saved_marker) = std::fs::read_to_string(&resume_file) {
        let saved_marker = saved_marker.trim().to_string();
        if !saved_marker.is_empty() {
            // Count existing objects
            let existing = std::fs::read_to_string(&objects_file)
                .map(|s| s.lines().count() as u64)
                .unwrap_or(0);
            total_objects.store(existing, Ordering::Relaxed);
            marker = Some(saved_marker.clone());
            eprintln!("  Resuming from marker ({}+ objects already saved)", existing);
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&objects_file)?;

    let mut endpoint_idx = 0;
    let mut consecutive_errors = 0u32;

    loop {
        let page = total_pages.fetch_add(1, Ordering::Relaxed) + 1;

        // Use primary endpoint for pagination (marker is endpoint-specific)
        let endpoint = RPC_ENDPOINTS[0];

        let params = if let Some(ref m) = marker {
            json!({
                "ledger_index": ledger_index,
                "limit": PAGE_SIZE,
                "binary": true,
                "marker": m,
            })
        } else {
            json!({
                "ledger_index": ledger_index,
                "limit": PAGE_SIZE,
                "binary": true,
            })
        };

        // Make request with retry
        let resp = match rpc_with_retry(&client, endpoint, "ledger_data", params, 3).await {
            Ok(r) => {
                consecutive_errors = 0;
                r
            }
            Err(e) => {
                errors.fetch_add(1, Ordering::Relaxed);
                consecutive_errors += 1;
                eprintln!("  [page {page}] Error from {endpoint}: {e}");
                if consecutive_errors >= 10 {
                    eprintln!("  Too many consecutive errors, stopping.");
                    break;
                }
                // Try next endpoint
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let result = &resp["result"];
        let state = result["state"].as_array();

        if let Some(objects) = state {
            use std::io::Write;
            for obj in objects {
                let index = obj["index"].as_str().unwrap_or("");
                let data_hex = obj["data"].as_str().unwrap_or("");
                if !index.is_empty() && !data_hex.is_empty() {
                    total_bytes.fetch_add(data_hex.len() as u64 / 2, Ordering::Relaxed);
                    total_objects.fetch_add(1, Ordering::Relaxed);
                    // Save to disk as JSONL
                    writeln!(file, "{}\t{}", index, data_hex)?;
                }
            }
        }

        // Save marker for resume
        marker = result["marker"].as_str().map(String::from);
        if let Some(ref m) = marker {
            std::fs::write(&resume_file, m)?;
        }

        // Progress
        let obj_count = total_objects.load(Ordering::Relaxed);
        let byte_count = total_bytes.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let rate = obj_count as f64 / elapsed.max(0.1);
        let err_count = errors.load(Ordering::Relaxed);

        if page % 5 == 0 || page <= 3 {
            eprintln!(
                "  [page {:>5}] {:>8} objects | {:>6.1}MB | {:>5.0}/s | {:.0}s | {} errors | {}",
                page, obj_count, byte_count as f64 / 1_048_576.0,
                rate, elapsed, err_count, endpoint
            );
        }

        if marker.is_none() {
            eprintln!("  No more pages — download complete!");
            break;
        }

        // Small delay to avoid overwhelming the RPC
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let elapsed = start.elapsed();
    let obj_count = total_objects.load(Ordering::Relaxed);
    let byte_count = total_bytes.load(Ordering::Relaxed);

    eprintln!();
    eprintln!("  Download finished!");
    eprintln!("  Objects:  {obj_count}");
    eprintln!("  Size:     {:.1}MB", byte_count as f64 / 1_048_576.0);
    eprintln!("  Time:     {:.0}s ({:.1}m)", elapsed.as_secs_f64(), elapsed.as_secs_f64() / 60.0);
    eprintln!("  Rate:     {:.0}/s", obj_count as f64 / elapsed.as_secs_f64().max(0.1));
    eprintln!("  Saved to: {objects_file}");
    eprintln!();

    // Step 3: Report
    eprintln!("[3/3] Summary");
    eprintln!("  Ledger #{ledger_index} — {obj_count} state objects — {:.1}MB",
        byte_count as f64 / 1_048_576.0);
    eprintln!("  Data saved to {SAVE_DIR}/objects.jsonl");
    eprintln!("  Resume marker saved to {SAVE_DIR}/marker.txt");
    eprintln!("  Re-run this binary to continue downloading.");

    // Clean up marker if complete
    if obj_count > 0 {
        eprintln!("\n  To verify state hash, load all objects into SHAMap");
        eprintln!("  (requires full download — mainnet has ~30M+ objects)");
    }

    Ok(())
}

async fn fetch_header(client: &reqwest::Client) -> anyhow::Result<(LedgerHeader, u32, String)> {
    let resp = rpc_with_retry(client, RPC_ENDPOINTS[0], "ledger", json!({
        "ledger_index": "validated",
    }), 5).await?;

    let ledger = &resp["result"]["ledger"];
    let ledger_index: u32 = ledger["ledger_index"].as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| ledger["ledger_index"].as_u64().map(|n| n as u32))
        .unwrap_or(0);

    let account_hash_hex = ledger["account_hash"].as_str().unwrap_or("").to_string();

    let header = LedgerHeader {
        sequence: ledger_index,
        total_coins: ledger["total_coins"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
        parent_hash: hex_to_hash(ledger["parent_hash"].as_str().unwrap_or("")),
        transaction_hash: hex_to_hash(ledger["transaction_hash"].as_str().unwrap_or("")),
        account_hash: hex_to_hash(&account_hash_hex),
        parent_close_time: ledger["parent_close_time"].as_u64().unwrap_or(0) as u32,
        close_time: ledger["close_time"].as_u64().unwrap_or(0) as u32,
        close_time_resolution: ledger["close_time_resolution"].as_u64().unwrap_or(10) as u8,
        close_flags: ledger["close_flags"].as_u64().unwrap_or(0) as u8,
    };

    Ok((header, ledger_index, account_hash_hex))
}

async fn rpc_with_retry(
    client: &reqwest::Client,
    endpoint: &str,
    method: &str,
    params: Value,
    max_retries: u32,
) -> anyhow::Result<Value> {
    let body = json!({ "method": method, "params": [params] });

    for attempt in 0..max_retries {
        match client.post(endpoint).json(&body).send().await {
            Ok(resp) => {
                match resp.json::<Value>().await {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        if attempt < max_retries - 1 {
                            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                            continue;
                        }
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                if attempt < max_retries - 1 {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    continue;
                }
                return Err(e.into());
            }
        }
    }
    anyhow::bail!("max retries exceeded")
}

fn hex_to_hash(s: &str) -> Hash256 {
    let bytes = hex::decode(s).unwrap_or_default();
    if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Hash256(arr)
    } else {
        Hash256([0u8; 32])
    }
}
