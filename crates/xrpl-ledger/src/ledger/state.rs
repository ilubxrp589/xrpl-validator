//! Verified ledger state — combines header, state SHAMap, and transaction SHAMap.

use xrpl_core::types::Hash256;

use super::header::LedgerHeader;
use crate::shamap::node::ZERO_HASH;
use crate::shamap::tree::{SHAMap, TreeType};
use crate::LedgerError;

/// A complete ledger snapshot with verified root hashes.
#[derive(Debug, Clone)]
pub struct LedgerState {
    /// The ledger header.
    pub header: LedgerHeader,
    /// Account state SHAMap (root hash should match header.account_hash).
    pub state_map: SHAMap,
    /// Transaction SHAMap (root hash should match header.transaction_hash).
    pub tx_map: SHAMap,
    /// Whether this ledger has been validated by the network.
    pub validated: bool,
}

impl LedgerState {
    /// Create a new LedgerState and verify root hashes match the header.
    pub fn from_parts(
        header: LedgerHeader,
        state_map: SHAMap,
        tx_map: SHAMap,
    ) -> Result<Self, LedgerError> {
        let state = Self {
            header,
            state_map,
            tx_map,
            validated: false,
        };

        state.verify()?;
        Ok(state)
    }

    /// Create an unverified LedgerState (for building incrementally).
    pub fn new_unverified(header: LedgerHeader) -> Self {
        Self {
            header,
            state_map: SHAMap::new(TreeType::State),
            tx_map: SHAMap::new(TreeType::Transaction),
            validated: false,
        }
    }

    /// Verify that the SHAMap root hashes match the header.
    pub fn verify(&self) -> Result<(), LedgerError> {
        let state_root = self.state_map.root_hash();
        let tx_root = self.tx_map.root_hash();

        // Empty trees produce ZERO_HASH, which is valid for empty ledgers
        if state_root != self.header.account_hash && self.header.account_hash != ZERO_HASH {
            return Err(LedgerError::HashMismatch {
                expected: hex::encode(self.header.account_hash.0),
                actual: hex::encode(state_root.0),
            });
        }

        if tx_root != self.header.transaction_hash && self.header.transaction_hash != ZERO_HASH {
            return Err(LedgerError::HashMismatch {
                expected: hex::encode(self.header.transaction_hash.0),
                actual: hex::encode(tx_root.0),
            });
        }

        Ok(())
    }

    /// The ledger hash (from the header).
    pub fn ledger_hash(&self) -> Hash256 {
        self.header.hash()
    }

    /// The ledger sequence number.
    pub fn sequence(&self) -> u32 {
        self.header.sequence
    }

    /// The ledger close time (Ripple epoch).
    pub fn close_time(&self) -> u32 {
        self.header.close_time
    }

    /// Number of entries in the state tree.
    pub fn state_count(&self) -> usize {
        self.state_map.len()
    }

    /// Number of transactions in this ledger.
    pub fn tx_count(&self) -> usize {
        self.tx_map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_header() -> LedgerHeader {
        LedgerHeader {
            sequence: 1,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]), // empty tree
            account_hash: Hash256([0; 32]),     // empty tree
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        }
    }

    #[test]
    fn new_unverified() {
        let state = LedgerState::new_unverified(empty_header());
        assert_eq!(state.sequence(), 1);
        assert_eq!(state.state_count(), 0);
        assert_eq!(state.tx_count(), 0);
    }

    #[test]
    fn verify_empty_ledger() {
        let header = empty_header();
        let state_map = SHAMap::new(TreeType::State);
        let tx_map = SHAMap::new(TreeType::Transaction);

        let state = LedgerState::from_parts(header, state_map, tx_map);
        assert!(state.is_ok());
    }

    #[test]
    fn verify_mismatched_hash_fails() {
        let mut header = empty_header();
        header.account_hash = Hash256([0xFF; 32]); // non-zero, won't match empty tree

        let state_map = SHAMap::new(TreeType::State);
        let tx_map = SHAMap::new(TreeType::Transaction);

        let result = LedgerState::from_parts(header, state_map, tx_map);
        assert!(result.is_err());
    }

    #[test]
    fn ledger_hash_from_state() {
        let state = LedgerState::new_unverified(empty_header());
        let hash = state.ledger_hash();
        assert_ne!(hash.0, [0u8; 32]);
    }
}
