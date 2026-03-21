//! NodeStore — Abstract storage backend for SHAMap nodes.
//!
//! Provides a trait for key-value storage of SHAMap nodes,
//! with in-memory and optional sled-based persistent backends.

use crate::LedgerError;
use xrpl_core::types::Hash256;

/// Abstract storage backend for SHAMap nodes and ledger data.
pub trait NodeStore: Send + Sync {
    /// Fetch a node by its hash. Returns None if not found.
    fn fetch(&self, hash: &Hash256) -> Result<Option<Vec<u8>>, LedgerError>;

    /// Store a node with its hash as the key.
    fn store(&self, hash: &Hash256, data: &[u8]) -> Result<(), LedgerError>;

    /// Check if a node exists without fetching its data.
    fn exists(&self, hash: &Hash256) -> Result<bool, LedgerError>;

    /// Store multiple nodes atomically.
    fn batch_store(&self, items: &[(Hash256, Vec<u8>)]) -> Result<(), LedgerError>;
}
