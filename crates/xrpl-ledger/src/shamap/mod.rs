//! SHAMap — Merkle-Patricia trie with branching factor 16.
//!
//! The SHAMap is the fundamental data structure of the XRP Ledger.
//! Every ledger contains two SHAMaps:
//! - **State tree** — all current ledger objects (accounts, offers, trust lines)
//! - **Transaction tree** — all transactions in this ledger
//!
//! Nodes are identified by 256-bit hashes. Navigation through the trie
//! uses nibbles (4-bit segments) of the key.

pub mod diff;
pub mod hash;
pub mod node;
pub mod sync;
pub mod tree;

pub use tree::{SHAMap, TreeType};
