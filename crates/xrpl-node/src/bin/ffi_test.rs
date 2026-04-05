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
            // Sample: apply ~3 txs per ledger via full FFI apply (RPC-fetched state).
            // Doing every tx would swamp rippled with ledger_entry calls.
            let apply_every_n = (txs.len() / 3).max(1);
            for (i, tx) in txs.iter().enumerate() {
                let tx_blob_hex = tx["tx_blob"].as_str().or_else(|| tx.as_str());
                let Some(hex_str) = tx_blob_hex else { continue; };
                let Ok(bytes) = hex::decode(hex_str) else { continue; };
                xrpl_node::ffi_engine::process_live_tx(&live_stats, &bytes, seq);
                // Full apply on sampled txs (blocking RPC calls per SLE fetch)
                if i % apply_every_n == 0 {
                    let apply_stats = live_stats.clone();
                    let tx_bytes_owned = bytes.clone();
                    let rpc_url_owned = rpc_url.clone();
                    tokio::task::spawn_blocking(move || {
                        xrpl_node::ffi_engine::apply_live_tx(
                            &apply_stats, &tx_bytes_owned, seq, &rpc_url_owned,
                        );
                    });
                }
            }
            last_processed = seq;
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
