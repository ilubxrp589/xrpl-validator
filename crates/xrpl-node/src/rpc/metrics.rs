//! Prometheus-compatible metrics endpoint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Node metrics for monitoring.
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

/// Render metrics in Prometheus text exposition format.
pub fn render_metrics(
    metrics: &NodeMetrics,
    peer_count: usize,
    mempool_size: usize,
    latest_ledger: u32,
) -> String {
    let uptime = metrics.start_time.elapsed().as_secs();
    let received = metrics.messages_received.load(Ordering::Relaxed);
    let sent = metrics.messages_sent.load(Ordering::Relaxed);

    format!(
        "# HELP xrpl_node_uptime_seconds Node uptime in seconds\n\
         # TYPE xrpl_node_uptime_seconds gauge\n\
         xrpl_node_uptime_seconds {uptime}\n\
         \n\
         # HELP xrpl_node_peers_connected Current number of connected peers\n\
         # TYPE xrpl_node_peers_connected gauge\n\
         xrpl_node_peers_connected {peer_count}\n\
         \n\
         # HELP xrpl_node_mempool_size Transactions in mempool\n\
         # TYPE xrpl_node_mempool_size gauge\n\
         xrpl_node_mempool_size {mempool_size}\n\
         \n\
         # HELP xrpl_node_ledger_sequence Latest validated ledger sequence\n\
         # TYPE xrpl_node_ledger_sequence gauge\n\
         xrpl_node_ledger_sequence {latest_ledger}\n\
         \n\
         # HELP xrpl_node_messages_received_total Total messages received from peers\n\
         # TYPE xrpl_node_messages_received_total counter\n\
         xrpl_node_messages_received_total {received}\n\
         \n\
         # HELP xrpl_node_messages_sent_total Total messages sent to peers\n\
         # TYPE xrpl_node_messages_sent_total counter\n\
         xrpl_node_messages_sent_total {sent}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_all_metrics() {
        let m = NodeMetrics::new();
        m.inc_received();
        m.inc_received();
        m.inc_sent();

        let output = render_metrics(&m, 5, 10, 12345);
        assert!(output.contains("xrpl_node_uptime_seconds"));
        assert!(output.contains("xrpl_node_peers_connected 5"));
        assert!(output.contains("xrpl_node_mempool_size 10"));
        assert!(output.contains("xrpl_node_ledger_sequence 12345"));
        assert!(output.contains("xrpl_node_messages_received_total 2"));
        assert!(output.contains("xrpl_node_messages_sent_total 1"));
    }
}
