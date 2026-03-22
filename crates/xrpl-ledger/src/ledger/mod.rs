//! Ledger — Header, entry types, and verified ledger state.
//!
//! A validated XRPL ledger consists of:
//! - A fixed-size header (118 bytes) containing metadata and root hashes
//! - A state SHAMap containing all current ledger objects
//! - A transaction SHAMap containing all transactions in this ledger

pub mod apply;
pub mod close;
pub mod header;
pub mod keylet;
pub mod objects;
pub mod sandbox;
pub mod state;
pub mod transactor;

pub use header::LedgerHeader;
pub use objects::{LedgerEntryType, LedgerObject};
pub use state::LedgerState;
