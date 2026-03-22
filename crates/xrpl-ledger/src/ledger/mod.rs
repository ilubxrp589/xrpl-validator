//! Ledger — Header, entry types, and verified ledger state.
//!
//! A validated XRPL ledger consists of:
//! - A fixed-size header (118 bytes) containing metadata and root hashes
//! - A state SHAMap containing all current ledger objects
//! - A transaction SHAMap containing all transactions in this ledger

pub mod header;
pub mod objects;
pub mod state;

pub use header::LedgerHeader;
pub use objects::{LedgerEntryType, LedgerObject};
pub use state::LedgerState;

pub mod close;
