//! # xrpl-ledger
//!
//! SHAMap, ledger storage, and state management for the XRP Ledger.
//!
//! This crate provides the core data structures for representing and
//! verifying XRPL ledger state:
//!
//! - **SHAMap** — Merkle-Patricia trie (branching factor 16) used by the
//!   XRP Ledger for both the state tree and transaction tree
//! - **NodeStore** — Abstract storage backend (in-memory or sled-based)
//! - **Ledger** — Ledger header, entry types, and verified ledger state

pub mod error;
pub mod ledger;
pub mod nodestore;
pub mod shamap;

pub use error::LedgerError;
