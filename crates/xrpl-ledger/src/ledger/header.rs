//! XRPL Ledger Header — 118-byte fixed-size structure.
//!
//! The ledger hash is `SHA512Half(HASH_PREFIX_LEDGER_MASTER || serialized_header)`.
//! This hash is what the entire network agrees on for each ledger.

use xrpl_core::types::Hash256;

use crate::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEDGER_MASTER};
use crate::LedgerError;

/// Ripple epoch offset: seconds between Unix epoch (1970-01-01) and
/// Ripple epoch (2000-01-01 00:00:00 UTC).
pub const RIPPLE_EPOCH_OFFSET: u64 = 946684800;

/// Size of the serialized ledger header in bytes.
pub const HEADER_SIZE: usize = 118;

/// XRPL Ledger Header.
///
/// Fields are serialized in this exact order as big-endian values.
/// Total: 118 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerHeader {
    /// Ledger sequence number (monotonically increasing).
    pub sequence: u32,
    /// Total XRP in existence (in drops).
    pub total_coins: u64,
    /// Hash of the parent (previous) ledger.
    pub parent_hash: Hash256,
    /// Root hash of the transaction SHAMap.
    pub transaction_hash: Hash256,
    /// Root hash of the account state SHAMap.
    pub account_hash: Hash256,
    /// Close time of the parent ledger (Ripple epoch seconds).
    pub parent_close_time: u32,
    /// Close time of this ledger (Ripple epoch seconds).
    pub close_time: u32,
    /// Close time resolution in seconds.
    pub close_time_resolution: u8,
    /// Close flags (bit 0 = sLCF_NoConsensusTime).
    pub close_flags: u8,
}

impl LedgerHeader {
    /// Serialize the header to exactly 118 bytes (big-endian).
    pub fn serialize(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        let mut pos = 0;

        // sequence: u32 (4 bytes)
        buf[pos..pos + 4].copy_from_slice(&self.sequence.to_be_bytes());
        pos += 4;

        // total_coins: u64 (8 bytes)
        buf[pos..pos + 8].copy_from_slice(&self.total_coins.to_be_bytes());
        pos += 8;

        // parent_hash: Hash256 (32 bytes)
        buf[pos..pos + 32].copy_from_slice(&self.parent_hash.0);
        pos += 32;

        // transaction_hash: Hash256 (32 bytes)
        buf[pos..pos + 32].copy_from_slice(&self.transaction_hash.0);
        pos += 32;

        // account_hash: Hash256 (32 bytes)
        buf[pos..pos + 32].copy_from_slice(&self.account_hash.0);
        pos += 32;

        // parent_close_time: u32 (4 bytes)
        buf[pos..pos + 4].copy_from_slice(&self.parent_close_time.to_be_bytes());
        pos += 4;

        // close_time: u32 (4 bytes)
        buf[pos..pos + 4].copy_from_slice(&self.close_time.to_be_bytes());
        pos += 4;

        // close_time_resolution: u8 (1 byte)
        buf[pos] = self.close_time_resolution;
        pos += 1;

        // close_flags: u8 (1 byte)
        buf[pos] = self.close_flags;

        buf
    }

    /// Deserialize a header from 118 bytes.
    pub fn deserialize(data: &[u8]) -> Result<Self, LedgerError> {
        if data.len() < HEADER_SIZE {
            return Err(LedgerError::InvalidHeader(format!(
                "need {} bytes, got {}",
                HEADER_SIZE,
                data.len()
            )));
        }

        let mut pos = 0;

        let sequence = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;

        let total_coins = u64::from_be_bytes([
            data[pos], data[pos + 1], data[pos + 2], data[pos + 3],
            data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7],
        ]);
        pos += 8;

        let mut parent_hash = [0u8; 32];
        parent_hash.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;

        let mut transaction_hash = [0u8; 32];
        transaction_hash.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;

        let mut account_hash = [0u8; 32];
        account_hash.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;

        let parent_close_time = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;

        let close_time = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;

        let close_time_resolution = data[pos];
        pos += 1;

        let close_flags = data[pos];

        Ok(Self {
            sequence,
            total_coins,
            parent_hash: Hash256(parent_hash),
            transaction_hash: Hash256(transaction_hash),
            account_hash: Hash256(account_hash),
            parent_close_time,
            close_time,
            close_time_resolution,
            close_flags,
        })
    }

    /// Compute the ledger hash.
    ///
    /// `SHA512Half(HASH_PREFIX_LEDGER_MASTER || serialized_118_bytes)`
    pub fn hash(&self) -> Hash256 {
        let serialized = self.serialize();
        sha512_half_prefixed(&HASH_PREFIX_LEDGER_MASTER, &serialized)
    }

    /// Convert a Ripple epoch timestamp to Unix epoch.
    pub fn ripple_to_unix(ripple_time: u32) -> u64 {
        ripple_time as u64 + RIPPLE_EPOCH_OFFSET
    }

    /// Convert a Unix epoch timestamp to Ripple epoch.
    pub fn unix_to_ripple(unix_time: u64) -> u32 {
        (unix_time - RIPPLE_EPOCH_OFFSET) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> LedgerHeader {
        LedgerHeader {
            sequence: 1000,
            total_coins: 99_999_999_999_000_000,
            parent_hash: Hash256([0xAA; 32]),
            transaction_hash: Hash256([0xBB; 32]),
            account_hash: Hash256([0xCC; 32]),
            parent_close_time: 800000000,
            close_time: 800000010,
            close_time_resolution: 10,
            close_flags: 0,
        }
    }

    #[test]
    fn serialize_size() {
        let header = sample_header();
        let bytes = header.serialize();
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(bytes.len(), 118);
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let header = sample_header();
        let bytes = header.serialize();
        let restored = LedgerHeader::deserialize(&bytes).unwrap();
        assert_eq!(header, restored);
    }

    #[test]
    fn hash_deterministic() {
        let header = sample_header();
        let h1 = header.hash();
        let h2 = header.hash();
        assert_eq!(h1, h2);
        assert_ne!(h1.0, [0u8; 32]);
    }

    #[test]
    fn different_sequence_different_hash() {
        let mut h1 = sample_header();
        let mut h2 = sample_header();
        h2.sequence = 1001;
        assert_ne!(h1.hash(), h2.hash());
    }

    #[test]
    fn deserialize_too_short() {
        let result = LedgerHeader::deserialize(&[0u8; 100]);
        assert!(result.is_err());
    }

    #[test]
    fn epoch_conversion() {
        // 2000-01-01 00:00:00 UTC = Ripple epoch 0 = Unix 946684800
        assert_eq!(LedgerHeader::ripple_to_unix(0), RIPPLE_EPOCH_OFFSET);
        assert_eq!(LedgerHeader::unix_to_ripple(RIPPLE_EPOCH_OFFSET), 0);

        // Round trip
        let ripple_time = 827441633u32;
        let unix = LedgerHeader::ripple_to_unix(ripple_time);
        assert_eq!(LedgerHeader::unix_to_ripple(unix), ripple_time);
    }

    #[test]
    fn field_order_in_serialization() {
        let header = LedgerHeader {
            sequence: 0x01020304,
            total_coins: 0,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 0,
            close_time_resolution: 0,
            close_flags: 0,
        };
        let bytes = header.serialize();
        // First 4 bytes should be the sequence in big-endian
        assert_eq!(&bytes[0..4], &[0x01, 0x02, 0x03, 0x04]);
    }
}
