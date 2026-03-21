//! Top-level Node orchestrator.
//!
//! Ties all subsystems together: peer manager, overlay router,
//! mempool, consensus engine, ledger store, and RPC server.

use crate::config::NodeConfig;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize the tracing subscriber for structured logging.
///
/// Reads the log level from `config.log_level`, which supports:
/// - Simple levels: `"info"`, `"debug"`, `"trace"`
/// - Per-module overrides: `"info,peer=debug,consensus=trace"`
pub fn init_tracing(config: &NodeConfig) {
    let filter = EnvFilter::try_new(&config.log_level)
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_names(true)
        .with_timer(fmt::time::uptime())
        .init();
}
