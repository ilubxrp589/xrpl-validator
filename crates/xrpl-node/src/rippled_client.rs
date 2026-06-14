//! Resilient RPC client for rippled — tries local node first, falls back to public endpoints.

use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;

/// RPC endpoints — override primary with XRPL_RPC_URL env var.
const ENDPOINTS: &[&str] = &[
    "http://localhost:5005",        // local rippled RPC
    "https://xrplcluster.com",     // public cluster (full history)
    "https://s1.ripple.com:51234", // Ripple's public server
];

/// WebSocket endpoints — override primary with XRPL_WS_URL env var.
const WS_ENDPOINTS: &[&str] = &[
    "ws://localhost:6006",          // local rippled WS
    "wss://xrplcluster.com",       // public WS
    "wss://s1.ripple.com:51233",   // Ripple's public WS
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
    /// Optional env-var override for primary RPC URL.
    rpc_override: Option<String>,
    /// Optional env-var override for primary WS URL.
    ws_override: Option<String>,
}

impl RippledClient {
    pub fn new() -> Self {
        // SECURITY(10.1): Read env var overrides for primary endpoints
        let rpc_override = std::env::var("XRPL_RPC_URL").ok();
        let ws_override = std::env::var("XRPL_WS_URL").ok();
        if let Some(ref url) = rpc_override {
            eprintln!("[rpc] Using XRPL_RPC_URL override: {url}");
        }
        if let Some(ref url) = ws_override {
            eprintln!("[rpc] Using XRPL_WS_URL override: {url}");
        }
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
            rpc_override,
            ws_override,
        }
    }

    /// Get the current RPC endpoint URL.
    /// Returns XRPL_RPC_URL env override for primary (index 0), otherwise hardcoded fallback.
    pub fn rpc_url(&self) -> &str {
        let idx = self.active_rpc.load(Ordering::Relaxed) as usize;
        if idx == 0 {
            if let Some(ref url) = self.rpc_override {
                return url.as_str();
            }
        }
        ENDPOINTS[idx.min(ENDPOINTS.len() - 1)]
    }

    /// Get the current WebSocket endpoint URL.
    /// Returns XRPL_WS_URL env override for primary (index 0), otherwise hardcoded fallback.
    pub fn ws_url(&self) -> &str {
        let idx = self.active_ws.load(Ordering::Relaxed) as usize;
        if idx == 0 {
            if let Some(ref url) = self.ws_override {
                return url.as_str();
            }
        }
        WS_ENDPOINTS[idx.min(WS_ENDPOINTS.len() - 1)]
    }

    /// Make an RPC request with automatic failover.
    /// Tries the current endpoint first, then falls back to alternatives.
    pub async fn rpc_request(&self, body: &serde_json::Value) -> Result<serde_json::Value, String> {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        let start_idx = self.active_rpc.load(Ordering::Relaxed) as usize;

        for attempt in 0..ENDPOINTS.len() {
            let idx = (start_idx + attempt) % ENDPOINTS.len();
            let url = if idx == 0 {
                if let Some(ref o) = self.rpc_override { o.as_str() } else { ENDPOINTS[0] }
            } else {
                ENDPOINTS[idx]
            };

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
    pub fn next_ws_endpoint(&self) -> &str {
        let old = self.active_ws.fetch_add(1, Ordering::Relaxed) as usize;
        let new_idx = (old + 1) % WS_ENDPOINTS.len();
        self.active_ws.store(new_idx as u64, Ordering::Relaxed);
        self.on_fallback.store(new_idx > 0, Ordering::Relaxed);
        if new_idx == 0 {
            if let Some(ref url) = self.ws_override {
                return url.as_str();
            }
        }
        WS_ENDPOINTS[new_idx]
    }

    /// Reset WS endpoint to primary.
    pub fn reset_ws(&self) {
        self.active_ws.store(0, Ordering::Relaxed);
    }
}

/// Reaudit 2026-06-10 finding F7 — guard against silently verifying mainnet state
/// against the PUBLIC fallback clusters.
///
/// A signing validator must source ledger state from a trusted (local/private)
/// rippled. On a host with no local node (e.g. m3060) an unset primary override
/// lets the failover chain reach `xrplcluster.com` / `s1.ripple.com` — a
/// consensus-divergence source AND a violation of the never-hammer-public-RPCs
/// rule. Returns `Err` (caller refuses to start) when this node signs, neither
/// override is set, and the escape hatch is off.
///
/// `allow_default_endpoints` is the `XRPL_ALLOW_DEFAULT_ENDPOINTS=1` opt-out for
/// legitimate localhost runs (e.g. dev on .39 where localhost:5005 is a real node).
pub fn check_signing_endpoints(
    signing_enabled: bool,
    rpc_override: Option<&str>,
    ws_override: Option<&str>,
    allow_default_endpoints: bool,
) -> Result<(), String> {
    if !signing_enabled || allow_default_endpoints {
        return Ok(());
    }
    let mut missing: Vec<&str> = Vec::new();
    if rpc_override.is_none() {
        missing.push("XRPL_RPC_URL");
    }
    if ws_override.is_none() {
        missing.push("XRPL_WS_URL");
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "signing validator started without {missing} set — the endpoint failover \
         chain would reach PUBLIC clusters ({public}), a consensus-divergence source \
         that also violates the never-hammer-public-RPCs rule. Set {missing} to a \
         trusted rippled, or set XRPL_ALLOW_DEFAULT_ENDPOINTS=1 to allow defaults \
         (dev/localhost only).",
        missing = missing.join(" and "),
        public = ENDPOINTS[1],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_signing_node_allows_default_endpoints() {
        // Observer / dev nodes don't sign, so falling back to public clusters is fine.
        assert!(check_signing_endpoints(false, None, None, false).is_ok());
    }

    #[test]
    fn signing_with_both_overrides_is_ok() {
        assert!(check_signing_endpoints(
            true,
            Some("http://10.0.0.39:5005"),
            Some("ws://10.0.0.39:6006"),
            false,
        )
        .is_ok());
    }

    #[test]
    fn signing_without_rpc_override_is_refused() {
        let err = check_signing_endpoints(true, None, Some("ws://10.0.0.39:6006"), false)
            .unwrap_err();
        assert!(err.contains("XRPL_RPC_URL"), "should name the missing RPC var: {err}");
        assert!(!err.contains("XRPL_WS_URL"), "must not flag WS when it is set: {err}");
    }

    #[test]
    fn signing_without_ws_override_is_refused() {
        let err = check_signing_endpoints(true, Some("http://10.0.0.39:5005"), None, false)
            .unwrap_err();
        assert!(err.contains("XRPL_WS_URL"), "should name the missing WS var: {err}");
        assert!(!err.contains("XRPL_RPC_URL"), "must not flag RPC when it is set: {err}");
    }

    #[test]
    fn signing_without_either_override_names_both() {
        let err = check_signing_endpoints(true, None, None, false).unwrap_err();
        assert!(err.contains("XRPL_RPC_URL") && err.contains("XRPL_WS_URL"), "{err}");
    }

    #[test]
    fn explicit_opt_in_allows_default_endpoints_even_when_signing() {
        // XRPL_ALLOW_DEFAULT_ENDPOINTS=1 escape hatch for dev/localhost runs.
        assert!(check_signing_endpoints(true, None, None, true).is_ok());
    }
}
