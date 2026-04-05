//! FFI demo + HTTP stats server.
//!
//! Runs libxrpl health checks continuously and exposes stats via HTTP at
//! /api/ffi-status (port 3778 by default). watch_engine.py polls this
//! alongside the main validator to show FFI status.
//!
//! Usage: cargo run --release -p xrpl-node --features ffi --bin ffi_test

#[cfg(feature = "ffi")]
#[tokio::main]
async fn main() {
    use std::sync::Arc;
    use std::time::Duration;

    println!("=== xrpl-node FFI stats server ===\n");

    let stats = xrpl_node::ffi_engine::new_stats();
    println!("libxrpl version: {}", stats.lock().libxrpl_version);

    // Initial health check
    let passed = xrpl_node::ffi_engine::health_check(&stats);
    println!("Initial health check: {}", if passed { "PASSED ✓" } else { "FAILED ✗" });

    // Periodic health checks
    let hc_stats = stats.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            xrpl_node::ffi_engine::health_check(&hc_stats);
        }
    });

    // Live mainnet tx feeder — fetch ledgers from rippled, push every tx through FFI
    let live_stats = stats.clone();
    tokio::spawn(async move {
        let rpc_url = std::env::var("XRPL_RPC_URL")
            .unwrap_or_else(|_| "http://10.0.0.39:5005".to_string());

        // Fetch active mainnet amendments at startup (kills most tefEXCEPTION)
        let rpc_url_for_amend = rpc_url.clone();
        let amendments = tokio::task::spawn_blocking(move || {
            xrpl_node::ffi_engine::fetch_mainnet_amendments(&rpc_url_for_amend)
        })
        .await
        .unwrap_or_default();
        println!("[ffi-live] Loaded {} active mainnet amendments", amendments.len());

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        let mut last_processed: u32 = 0;
        println!("[ffi-live] Fetching live mainnet txs from {rpc_url}");
        loop {
            // Get validated ledger index
            let resp = client
                .post(&rpc_url)
                .json(&serde_json::json!({
                    "method": "ledger",
                    "params": [{"ledger_index": "validated"}]
                }))
                .send()
                .await;
            let Ok(r) = resp else { tokio::time::sleep(Duration::from_secs(2)).await; continue; };
            let Ok(body) = r.json::<serde_json::Value>().await else { continue; };
            let seq: u32 = body["result"]["ledger"]["ledger_index"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if seq == 0 || seq == last_processed {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            // Fetch ledger with full binary txs
            let resp = client
                .post(&rpc_url)
                .json(&serde_json::json!({
                    "method": "ledger",
                    "params": [{"ledger_index": seq, "transactions": true, "expand": true, "binary": true}]
                }))
                .send()
                .await;
            let Ok(r) = resp else { tokio::time::sleep(Duration::from_secs(2)).await; continue; };
            let Ok(body) = r.json::<serde_json::Value>().await else { continue; };
            let empty = Vec::new();
            let txs = body["result"]["ledger"]["transactions"].as_array().unwrap_or(&empty);
            // Parse each tx + extract TransactionIndex from its meta blob.
            // Meta blob starts with sfTransactionIndex (type=2 UInt32 field=28)
            // encoded as 0x20 0x1C followed by 4 bytes big-endian.
            let mut ordered: Vec<(u32, Vec<u8>)> = Vec::with_capacity(txs.len());
            for tx in txs.iter() {
                let tx_blob_hex = tx["tx_blob"].as_str().or_else(|| tx.as_str());
                let Some(hex_str) = tx_blob_hex else { continue; };
                let Ok(bytes) = hex::decode(hex_str) else { continue; };
                xrpl_node::ffi_engine::process_live_tx(&live_stats, &bytes, seq);

                // Extract TransactionIndex from meta blob
                let meta_hex = tx["meta"].as_str().unwrap_or("");
                let idx = if meta_hex.len() >= 12 {
                    let meta_bytes = hex::decode(&meta_hex[..12]).unwrap_or_default();
                    if meta_bytes.len() >= 6 && meta_bytes[0] == 0x20 && meta_bytes[1] == 0x1C {
                        u32::from_be_bytes([meta_bytes[2], meta_bytes[3], meta_bytes[4], meta_bytes[5]])
                    } else { u32::MAX }
                } else { u32::MAX };
                ordered.push((idx, bytes));
            }
            last_processed = seq;

            // Apply every 4th ledger's txs IN TransactionIndex ORDER.
            if seq % 4 == 0 && !ordered.is_empty() {
                ordered.sort_by_key(|(i, _)| *i);
                let sorted_blobs: Vec<Vec<u8>> = ordered.into_iter().map(|(_, b)| b).collect();

                // Fetch ledger header in JSON mode (binary mode doesn't break out fields)
                let hdr_resp = client.post(&rpc_url)
                    .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":seq}]}))
                    .send().await;
                let (parent_hash, parent_close_time, total_drops) = if let Ok(r) = hdr_resp {
                    if let Ok(body) = r.json::<serde_json::Value>().await {
                        let lg = &body["result"]["ledger"];
                        let ph = lg["parent_hash"].as_str()
                            .and_then(|s| hex::decode(s).ok())
                            .and_then(|b| if b.len()==32 { let mut a=[0u8;32]; a.copy_from_slice(&b); Some(a)} else {None})
                            .unwrap_or([0u8; 32]);
                        let pct = lg["parent_close_time"].as_u64().unwrap_or(0) as u32;
                        let td = lg["total_coins"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0u64);
                        (ph, pct, td)
                    } else { ([0u8;32], 0, 0) }
                } else { ([0u8;32], 0, 0) };
                eprintln!("[ffi-live] Applying {} txs from ledger #{seq} (drops={total_drops}) in TransactionIndex order...", sorted_blobs.len());

                let apply_stats = live_stats.clone();
                let rpc_url_owned = rpc_url.clone();
                let amendments_owned = amendments.clone();
                tokio::task::spawn_blocking(move || {
                    xrpl_node::ffi_engine::apply_ledger_in_order(
                        &apply_stats,
                        &sorted_blobs,
                        seq,
                        &rpc_url_owned,
                        &amendments_owned,
                        parent_hash,
                        parent_close_time,
                        total_drops,
                    );
                });
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // HTTP server for watch_engine.py integration
    let port = std::env::var("FFI_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(3778);

    use axum::{routing::get, Json, Router};
    let api_stats = Arc::clone(&stats);
    let app = Router::new().route(
        "/api/ffi-status",
        get(move || {
            let stats = api_stats.clone();
            async move {
                let snapshot = stats.lock().clone();
                Json(serde_json::to_value(&snapshot).unwrap_or_default())
            }
        }),
    );

    let addr = format!("0.0.0.0:{port}");
    println!("\n[ffi] HTTP stats server listening on http://{addr}/api/ffi-status");
    println!("[ffi] Running health checks every 500ms...");
    println!("\nPress Ctrl-C to stop.\n");

    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server error: {e}");
    }
}

#[cfg(not(feature = "ffi"))]
fn main() {
    eprintln!("Build with --features ffi to enable this test");
    std::process::exit(1);
}
