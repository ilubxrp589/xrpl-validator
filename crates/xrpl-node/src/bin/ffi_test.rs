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
    use tracing::{info, warn};

    // Structured JSON logs to stderr. Override format with LOG_FORMAT=text.
    let log_format = std::env::var("LOG_FORMAT").unwrap_or_else(|_| "json".into());
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if log_format == "text" {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    }

    let stats = xrpl_node::ffi_engine::new_stats();
    let version = stats.lock().libxrpl_version.clone();
    info!(component = "ffi_test", libxrpl_version = %version, "starting");

    // Initial health check
    let passed = xrpl_node::ffi_engine::health_check(&stats);
    if passed {
        info!(check = "health", result = "pass", "initial self-test passed");
    } else {
        warn!(check = "health", result = "fail", "initial self-test failed");
    }

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
    let divergence_log = Arc::new(xrpl_node::ffi_engine::DivergenceLog::new());
    let live_dlog = divergence_log.clone();
    info!(path = %divergence_log.path().display(), "divergence log opened");
    tokio::spawn(async move {
        use tracing::info;
        // Comma-separated list of RPC endpoints for failover. First URL is primary.
        let rpc_urls: Vec<String> = std::env::var("XRPL_RPC_URLS")
            .or_else(|_| std::env::var("XRPL_RPC_URL"))
            .unwrap_or_else(|_| "http://10.0.0.39:5005".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let rpc_url = rpc_urls[0].clone();
        info!(endpoints = ?rpc_urls, "rpc endpoints configured");

        // Fetch active mainnet amendments at startup (kills most tefEXCEPTION)
        let rpc_url_for_amend = rpc_url.clone();
        let amendments = tokio::task::spawn_blocking(move || {
            xrpl_node::ffi_engine::fetch_mainnet_amendments(&rpc_url_for_amend)
        })
        .await
        .unwrap_or_default();
        info!(count = amendments.len(), "loaded active mainnet amendments");

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        let mut last_processed: u32 = 0;
        info!(rpc_url = %rpc_url, "fetching live mainnet txs");
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
                info!(ledger_seq = seq, txs = sorted_blobs.len(), total_drops, "applying ledger in transaction-index order");

                let apply_stats = live_stats.clone();
                let rpc_urls_owned = rpc_urls.clone();
                let amendments_owned = amendments.clone();
                let dlog = live_dlog.clone();
                tokio::task::spawn_blocking(move || {
                    xrpl_node::ffi_engine::apply_ledger_in_order(
                        &apply_stats,
                        &sorted_blobs,
                        seq,
                        &rpc_urls_owned,
                        &amendments_owned,
                        parent_hash,
                        parent_close_time,
                        total_drops,
                        Some(dlog.as_ref()),
                        None, // no snapshot — sidecar uses pure RPC path
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

    use axum::{http::header, response::IntoResponse, routing::get, Json, Router};
    let api_stats = Arc::clone(&stats);
    let metrics_stats = Arc::clone(&stats);
    let app = Router::new()
        .route(
            "/api/ffi-status",
            get(move || {
                let stats = api_stats.clone();
                async move {
                    let snapshot = stats.lock().clone();
                    Json(serde_json::to_value(&snapshot).unwrap_or_default())
                }
            }),
        )
        .route(
            "/metrics",
            get(move || {
                let stats = metrics_stats.clone();
                async move {
                    let snapshot = stats.lock().clone();
                    let body = xrpl_node::ffi_engine::render_prometheus(&snapshot);
                    (
                        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                        body,
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/health",
            get(move || async move {
                (axum::http::StatusCode::OK, "ok")
            }),
        );

    let addr = format!("0.0.0.0:{port}");
    info!(bind = %addr, routes = "/api/ffi-status,/metrics,/health", "http server listening");

    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "server error");
    }
}

#[cfg(not(feature = "ffi"))]
fn main() {
    eprintln!("Build with --features ffi to enable this test");
    std::process::exit(1);
}
