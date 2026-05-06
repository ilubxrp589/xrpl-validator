//! Prometheus-compatible metrics endpoint for XRPL Rust Validator.
//!
//! Renders metrics in Prometheus text exposition format (text/plain).
//! No external crate dependency — hand-rendered for minimal footprint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Node-level metrics counters.
pub struct NodeMetrics {
    pub start_time: Instant,
    pub messages_received: AtomicU64,
    pub messages_sent: AtomicU64,
}

impl NodeMetrics {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            messages_received: AtomicU64::new(0),
            messages_sent: AtomicU64::new(0),
        }
    }

    pub fn inc_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_sent(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for NodeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of all validator metrics for Prometheus rendering.
pub struct ValidatorMetricsSnapshot {
    pub uptime_secs: u64,
    pub peer_count: usize,
    pub ledger_seq: u32,
    pub messages_received: u64,
    pub messages_sent: u64,
    pub consecutive_matches: u32,
    pub total_matches: u64,
    pub total_mismatches: u64,
    pub round_time_secs: f64,
    pub compute_time_secs: f64,
    pub state_objects: u64,
    pub ready_to_sign: bool,
    pub total_txs: u64,
    /// VALAUDIT Phase 3 (va-03) signing-gate skip counters.
    pub validations_skipped_not_ready: u64,
    pub validations_skipped_zero_hash: u64,
}

/// Render full validator metrics in Prometheus text exposition format.
pub fn render_prometheus(snap: &ValidatorMetricsSnapshot) -> String {
    let ready = if snap.ready_to_sign { 1 } else { 0 };
    // SECURITY(10.2): Read domain from env var instead of hardcoding
    let domain = std::env::var("XRPL_DOMAIN").unwrap_or_else(|_| "halcyon-names.io".to_string());

    format!(
        "# HELP xrpl_validator_uptime_seconds Seconds since validator process started\n\
         # TYPE xrpl_validator_uptime_seconds gauge\n\
         xrpl_validator_uptime_seconds {}\n\
         \n\
         # HELP xrpl_validator_ledger_sequence Latest validated ledger sequence\n\
         # TYPE xrpl_validator_ledger_sequence gauge\n\
         xrpl_validator_ledger_sequence {}\n\
         \n\
         # HELP xrpl_validator_consecutive_matches Current consecutive state hash match streak\n\
         # TYPE xrpl_validator_consecutive_matches gauge\n\
         xrpl_validator_consecutive_matches {}\n\
         \n\
         # HELP xrpl_validator_total_matches Total state hash matches since startup\n\
         # TYPE xrpl_validator_total_matches counter\n\
         xrpl_validator_total_matches {}\n\
         \n\
         # HELP xrpl_validator_total_mismatches Total state hash mismatches since startup\n\
         # TYPE xrpl_validator_total_mismatches counter\n\
         xrpl_validator_total_mismatches {}\n\
         \n\
         # HELP xrpl_validator_round_time_seconds Time for most recent ledger round\n\
         # TYPE xrpl_validator_round_time_seconds gauge\n\
         xrpl_validator_round_time_seconds {:.6}\n\
         \n\
         # HELP xrpl_validator_compute_time_seconds Hash computation time for most recent round\n\
         # TYPE xrpl_validator_compute_time_seconds gauge\n\
         xrpl_validator_compute_time_seconds {:.6}\n\
         \n\
         # HELP xrpl_validator_peers_connected Currently connected XRPL peers\n\
         # TYPE xrpl_validator_peers_connected gauge\n\
         xrpl_validator_peers_connected {}\n\
         \n\
         # HELP xrpl_validator_state_objects Total state objects in SHAMap\n\
         # TYPE xrpl_validator_state_objects gauge\n\
         xrpl_validator_state_objects {}\n\
         \n\
         # HELP xrpl_validator_ready_to_sign Whether validator is signing with own hash\n\
         # TYPE xrpl_validator_ready_to_sign gauge\n\
         xrpl_validator_ready_to_sign {}\n\
         \n\
         # HELP xrpl_validator_messages_received_total Total peer messages received\n\
         # TYPE xrpl_validator_messages_received_total counter\n\
         xrpl_validator_messages_received_total {}\n\
         \n\
         # HELP xrpl_validator_messages_sent_total Total peer messages sent\n\
         # TYPE xrpl_validator_messages_sent_total counter\n\
         xrpl_validator_messages_sent_total {}\n\
         \n\
         # HELP xrpl_validator_transactions_total Total transactions processed\n\
         # TYPE xrpl_validator_transactions_total counter\n\
         xrpl_validator_transactions_total {}\n\
         \n\
         # HELP xrpl_validator_validations_skipped_total Validations the va-03 signing gate refused to sign, by reason\n\
         # TYPE xrpl_validator_validations_skipped_total counter\n\
         xrpl_validator_validations_skipped_total{{reason=\"not_ready\"}} {}\n\
         xrpl_validator_validations_skipped_total{{reason=\"zero_hash\"}} {}\n\
         \n\
         # HELP xrpl_validator_info Validator identity and version info\n\
         # TYPE xrpl_validator_info gauge\n\
         xrpl_validator_info{{version=\"0.1.0\",implementation=\"rust\",domain=\"{domain}\"}} 1\n",
        snap.uptime_secs,
        snap.ledger_seq,
        snap.consecutive_matches,
        snap.total_matches,
        snap.total_mismatches,
        snap.round_time_secs,
        snap.compute_time_secs,
        snap.peer_count,
        snap.state_objects,
        ready,
        snap.messages_received,
        snap.messages_sent,
        snap.total_txs,
        snap.validations_skipped_not_ready,
        snap.validations_skipped_zero_hash,
        domain = domain,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_prometheus_format() {
        let snap = ValidatorMetricsSnapshot {
            uptime_secs: 3600,
            peer_count: 10,
            ledger_seq: 103000000,
            messages_received: 50000,
            messages_sent: 30000,
            consecutive_matches: 500,
            total_matches: 500,
            total_mismatches: 0,
            round_time_secs: 0.065,
            compute_time_secs: 0.015,
            state_objects: 18750000,
            ready_to_sign: true,
            total_txs: 100000,
            validations_skipped_not_ready: 3,
            validations_skipped_zero_hash: 0,
        };
        let output = render_prometheus(&snap);
        assert!(output.contains("xrpl_validator_uptime_seconds 3600"));
        assert!(output.contains("xrpl_validator_consecutive_matches 500"));
        assert!(output.contains("xrpl_validator_validations_skipped_total{reason=\"not_ready\"} 3"));
        assert!(output.contains("xrpl_validator_validations_skipped_total{reason=\"zero_hash\"} 0"));
        assert!(output.contains("xrpl_validator_state_objects 18750000"));
        assert!(output.contains("xrpl_validator_ready_to_sign 1"));
        assert!(output.contains("xrpl_validator_info{"));
        assert!(output.contains("# TYPE xrpl_validator_total_matches counter"));
    }
}
