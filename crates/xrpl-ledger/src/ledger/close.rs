//! Canonical transaction ordering for ledger application.
//!
//! When applying a transaction set to produce a new ledger, transactions
//! MUST be applied in rippled's canonical order. Different ordering
//! produces a different resulting state hash, causing consensus failure.

use xrpl_core::types::Hash256;

/// A transaction entry with the fields needed for canonical ordering.
#[derive(Debug, Clone)]
pub struct TransactionEntry {
    /// Transaction hash (SHA512Half of TXN\0 || blob).
    pub hash: Hash256,
    /// The account that submitted this transaction (20-byte account ID).
    pub account: [u8; 20],
    /// Account sequence number.
    pub sequence: u32,
    /// Raw transaction blob.
    pub blob: Vec<u8>,
}

/// Sort transactions in rippled's canonical order.
///
/// Ordering:
/// 1. Primary: account bytes (lexicographic, ascending)
/// 2. Secondary: sequence number (ascending)
/// 3. Tertiary: transaction hash (lexicographic, ascending)
pub fn canonical_order(txs: &mut [TransactionEntry]) {
    txs.sort_by(|a, b| {
        a.account
            .cmp(&b.account)
            .then(a.sequence.cmp(&b.sequence))
            .then(a.hash.0.cmp(&b.hash.0))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(account_byte: u8, seq: u32, hash_byte: u8) -> TransactionEntry {
        TransactionEntry {
            hash: Hash256([hash_byte; 32]),
            account: [account_byte; 20],
            sequence: seq,
            blob: vec![],
        }
    }

    #[test]
    fn sort_by_account() {
        let mut txs = vec![
            tx(0x30, 1, 0x01),
            tx(0x10, 1, 0x02),
            tx(0x20, 1, 0x03),
        ];
        canonical_order(&mut txs);
        assert_eq!(txs[0].account[0], 0x10);
        assert_eq!(txs[1].account[0], 0x20);
        assert_eq!(txs[2].account[0], 0x30);
    }

    #[test]
    fn sort_by_sequence_within_account() {
        let mut txs = vec![
            tx(0xAA, 5, 0x01),
            tx(0xAA, 1, 0x02),
            tx(0xAA, 3, 0x03),
        ];
        canonical_order(&mut txs);
        assert_eq!(txs[0].sequence, 1);
        assert_eq!(txs[1].sequence, 3);
        assert_eq!(txs[2].sequence, 5);
    }

    #[test]
    fn sort_by_hash_as_tiebreaker() {
        let mut txs = vec![
            tx(0xBB, 1, 0x30),
            tx(0xBB, 1, 0x10),
            tx(0xBB, 1, 0x20),
        ];
        canonical_order(&mut txs);
        assert_eq!(txs[0].hash.0[0], 0x10);
        assert_eq!(txs[1].hash.0[0], 0x20);
        assert_eq!(txs[2].hash.0[0], 0x30);
    }

    #[test]
    fn mixed_accounts_and_sequences() {
        let mut txs = vec![
            tx(0x20, 3, 0x01),
            tx(0x10, 1, 0x02),
            tx(0x20, 1, 0x03),
            tx(0x10, 2, 0x04),
        ];
        canonical_order(&mut txs);

        // Account 0x10 first, then 0x20
        assert_eq!(txs[0].account[0], 0x10);
        assert_eq!(txs[0].sequence, 1);
        assert_eq!(txs[1].account[0], 0x10);
        assert_eq!(txs[1].sequence, 2);
        assert_eq!(txs[2].account[0], 0x20);
        assert_eq!(txs[2].sequence, 1);
        assert_eq!(txs[3].account[0], 0x20);
        assert_eq!(txs[3].sequence, 3);
    }

    #[test]
    fn empty_is_noop() {
        let mut txs: Vec<TransactionEntry> = vec![];
        canonical_order(&mut txs);
        assert!(txs.is_empty());
    }

    #[test]
    fn single_element() {
        let mut txs = vec![tx(0x01, 1, 0xFF)];
        canonical_order(&mut txs);
        assert_eq!(txs.len(), 1);
    }
}
