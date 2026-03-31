//! Resilient RPC client for rippled — tries local node first, falls back to public endpoints.

use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;

/// Ordered list of RPC endpoints to try.
const ENDPOINTS: &[&str] = &[
    "http://10.0.0.97:5005",       // local rippled on m3060
    "https://xrplcluster.com",     // public cluster (full history)
    "https://s1.ripple.com:51234", // Ripple's public server
];

/// WebSocket endpoints for subscription.
const WS_ENDPOINTS: &[&str] = &[
    "ws://10.0.0.97:6006",        // local rippled WS
    "wss://xrplcluster.com",      // public WS
    "wss://s1.ripple.com:51233",  // Ripple's public WS
];

/// A resilient RPC client that automatically fails over between endpoints.
#[derive(Clone)]
pub struct RippledClient {
    pub client: reqwest::Client,
    /// Index of the currently active RPC endpoint.
    active_rpc: Arc<AtomicU64>,
    /// Index of the currently active WS endpoint.
    active_ws: Arc<AtomicU64>,
    /// Total requests made.
    pub total_requests: Arc<AtomicU64>,
    /// Total failovers triggered.
    pub total_failovers: Arc<AtomicU64>,
    /// Whether we're currently on a fallback endpoint.
    pub on_fallback: Arc<AtomicBool>,
    /// Consecutive successes on current endpoint (used to try switching back to primary).
    consecutive_ok: Arc<AtomicU64>,
}

impl RippledClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            active_rpc: Arc::new(AtomicU64::new(0)),
            active_ws: Arc::new(AtomicU64::new(0)),
            total_requests: Arc::new(AtomicU64::new(0)),
            total_failovers: Arc::new(AtomicU64::new(0)),
            on_fallback: Arc::new(AtomicBool::new(false)),
            consecutive_ok: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the current RPC endpoint URL.
    pub fn rpc_url(&self) -> &'static str {
        let idx = self.active_rpc.load(Ordering::Relaxed) as usize;
        ENDPOINTS[idx.min(ENDPOINTS.len() - 1)]
    }

    /// Get the current WebSocket endpoint URL.
    pub fn ws_url(&self) -> &'static str {
        let idx = self.active_ws.load(Ordering::Relaxed) as usize;
        WS_ENDPOINTS[idx.min(WS_ENDPOINTS.len() - 1)]
    }

    /// Make an RPC request with automatic failover.
    /// Tries the current endpoint first, then falls back to alternatives.
    pub async fn rpc_request(&self, body: &serde_json::Value) -> Result<serde_json::Value, String> {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        let start_idx = self.active_rpc.load(Ordering::Relaxed) as usize;

        for attempt in 0..ENDPOINTS.len() {
            let idx = (start_idx + attempt) % ENDPOINTS.len();
            let url = ENDPOINTS[idx];

            match self.client.post(url).json(body).send().await {
                Ok(resp) => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(json) => {
                            // Check for server overload
                            if json["result"]["error"].as_str() == Some("noNetwork")
                                || json["result"]["error"].as_str() == Some("lgrNotFound")
                            {
                                continue; // try next endpoint
                            }

                            // Success — update active endpoint if we failed over
                            if attempt > 0 {
                                self.active_rpc.store(idx as u64, Ordering::Relaxed);
                                self.on_fallback.store(idx > 0, Ordering::Relaxed);
                                self.total_failovers.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[rpc] Failover to {} (attempt {})", url, attempt + 1);
                            }

                            // Periodically try to switch back to primary
                            let ok = self.consecutive_ok.fetch_add(1, Ordering::Relaxed) + 1;
                            if idx > 0 && ok % 100 == 0 {
                                // Try primary again
                                self.active_rpc.store(0, Ordering::Relaxed);
                                self.on_fallback.store(false, Ordering::Relaxed);
                                eprintln!("[rpc] Trying primary endpoint again after {ok} successes on fallback");
                            }

                            return Ok(json);
                        }
                        Err(e) => {
                            if attempt + 1 < ENDPOINTS.len() { continue; }
                            return Err(format!("json parse: {e}"));
                        }
                    }
                }
                Err(e) => {
                    if attempt + 1 < ENDPOINTS.len() { continue; }
                    return Err(format!("request failed on all endpoints: {e}"));
                }
            }
        }
        Err("all endpoints failed".into())
    }

    /// Convenience: make an RPC method call.
    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({"method": method, "params": [params]});
        self.rpc_request(&body).await
    }

    /// Switch to the next WS endpoint (called when WS disconnects).
    pub fn next_ws_endpoint(&self) -> &'static str {
        let old = self.active_ws.fetch_add(1, Ordering::Relaxed) as usize;
        let new_idx = (old + 1) % WS_ENDPOINTS.len();
        self.active_ws.store(new_idx as u64, Ordering::Relaxed);
        self.on_fallback.store(new_idx > 0, Ordering::Relaxed);
        WS_ENDPOINTS[new_idx]
    }

    /// Reset WS endpoint to primary.
    pub fn reset_ws(&self) {
        self.active_ws.store(0, Ordering::Relaxed);
    }
}
