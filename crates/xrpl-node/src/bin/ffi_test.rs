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
