//! # xrpl-node
//!
//! XRPL validator node — peer protocol, consensus, and RPC server.
//!
//! This crate implements a full XRPL network participant capable of:
//! - Connecting to peers via the XRPL/2.0 protocol
//! - Following ledger progression (Observer/Follower mode)
//! - Participating in consensus (Validator mode)
//! - Serving JSON-RPC queries
//!
//! ## Modes
//!
//! - **Observer** — connect and log, no state storage
//! - **Follower** — sync ledger state, serve RPC, no consensus
//! - **Validator** — full consensus participation with validation signing

pub mod bulk_sync;
pub mod config;
pub mod consensus;
pub mod consensus_engine;
pub mod engine;
pub mod ledger_close;
pub mod error;
pub mod mempool;
pub mod node;
pub mod overlay;
pub mod peer;
pub mod rpc;
pub mod state_hash;
pub mod validation;

pub use config::{NodeConfig, NodeMode};
pub use error::NodeError;
