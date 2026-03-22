//! JSON-RPC and WebSocket server.
//!
//! Exposes the node's functionality to external clients:
//! submit transactions, query ledger state, monitor health.

pub mod handlers;
pub mod metrics;
pub mod server;

pub use handlers::AppState;
pub use metrics::NodeMetrics;
pub use server::create_router;
