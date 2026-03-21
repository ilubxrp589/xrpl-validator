//! Hash prefix constants for the XRPL protocol.
//!
//! rippled prepends a 4-byte prefix to data before hashing with SHA-512 Half.
//! Each prefix is 3 ASCII characters followed by a null byte.
//! Source: rippled `include/xrpl/protocol/HashPrefix.h`

use sha2::{Digest, Sha512};
use xrpl_core::types::Hash256;

// ---- Hash Prefix Constants ----
// Exact values from rippled HashPrefix.h. DO NOT MODIFY.

/// Hash prefix for transaction IDs: "TXN\0"
pub const HASH_PREFIX_TRANSACTION_ID: [u8; 4] = [0x54, 0x58, 0x4E, 0x00];

/// Hash prefix for transaction tree nodes (SHAMap leaf in tx tree): "SND\0"
pub const HASH_PREFIX_TX_NODE: [u8; 4] = [0x53, 0x4E, 0x44, 0x00];

/// Hash prefix for leaf nodes in a SHAMap: "MLN\0"
pub const HASH_PREFIX_LEAF_NODE: [u8; 4] = [0x4D, 0x4C, 0x4E, 0x00];

/// Hash prefix for inner nodes in a SHAMap: "MIN\0"
pub const HASH_PREFIX_INNER_NODE: [u8; 4] = [0x4D, 0x49, 0x4E, 0x00];

/// Hash prefix for ledger header (master): "LWR\0"
pub const HASH_PREFIX_LEDGER_MASTER: [u8; 4] = [0x4C, 0x57, 0x52, 0x00];

/// Hash prefix for transaction signing: "STX\0"
pub const HASH_PREFIX_TX_SIGN: [u8; 4] = [0x53, 0x54, 0x58, 0x00];

/// Hash prefix for multi-signing: "SMT\0"
pub const HASH_PREFIX_TX_MULTI_SIGN: [u8; 4] = [0x53, 0x4D, 0x54, 0x00];

/// Hash prefix for validation messages: "VAL\0"
pub const HASH_PREFIX_VALIDATION: [u8; 4] = [0x56, 0x41, 0x4C, 0x00];

/// Hash prefix for consensus proposals: "PRP\0"
pub const HASH_PREFIX_PROPOSAL: [u8; 4] = [0x50, 0x52, 0x50, 0x00];

/// Hash prefix for validator manifests: "MAN\0"
pub const HASH_PREFIX_MANIFEST: [u8; 4] = [0x4D, 0x41, 0x4E, 0x00];

/// Hash prefix for payment channel claims: "CLM\0"
pub const HASH_PREFIX_PAYMENT_CHANNEL_CLAIM: [u8; 4] = [0x43, 0x4C, 0x4D, 0x00];

/// Hash prefix for batch transactions: "BCH\0"
pub const HASH_PREFIX_BATCH: [u8; 4] = [0x42, 0x43, 0x48, 0x00];

/// Compute SHA-512 Half (first 32 bytes of SHA-512) with a 4-byte prefix.
///
/// This is the fundamental hashing primitive of the XRPL protocol.
/// `result = SHA512(prefix || data)[0..32]`
pub fn sha512_half_prefixed(prefix: &[u8; 4], data: &[u8]) -> Hash256 {
    let mut hasher = Sha512::new();
    hasher.update(prefix);
    hasher.update(data);
    let full = hasher.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&full[..32]);
    Hash256(result)
}

/// Compute SHA-512 Half without a prefix.
pub fn sha512_half(data: &[u8]) -> Hash256 {
    let mut hasher = Sha512::new();
    hasher.update(data);
    let full = hasher.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&full[..32]);
    Hash256(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_ascii_values() {
        // Verify each prefix matches its ASCII representation
        assert_eq!(&HASH_PREFIX_TRANSACTION_ID, b"TXN\0");
        assert_eq!(&HASH_PREFIX_TX_NODE, b"SND\0");
        assert_eq!(&HASH_PREFIX_LEAF_NODE, b"MLN\0");
        assert_eq!(&HASH_PREFIX_INNER_NODE, b"MIN\0");
        assert_eq!(&HASH_PREFIX_LEDGER_MASTER, b"LWR\0");
        assert_eq!(&HASH_PREFIX_TX_SIGN, b"STX\0");
        assert_eq!(&HASH_PREFIX_TX_MULTI_SIGN, b"SMT\0");
        assert_eq!(&HASH_PREFIX_VALIDATION, b"VAL\0");
        assert_eq!(&HASH_PREFIX_PROPOSAL, b"PRP\0");
        assert_eq!(&HASH_PREFIX_MANIFEST, b"MAN\0");
        assert_eq!(&HASH_PREFIX_PAYMENT_CHANNEL_CLAIM, b"CLM\0");
        assert_eq!(&HASH_PREFIX_BATCH, b"BCH\0");
    }

    #[test]
    fn prefix_hex_values() {
        // Verify hex values match rippled HashPrefix.h
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_TRANSACTION_ID), 0x54584E00);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_TX_NODE), 0x534E4400);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_LEAF_NODE), 0x4D4C4E00);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_INNER_NODE), 0x4D494E00);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_LEDGER_MASTER), 0x4C575200);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_TX_SIGN), 0x53545800);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_TX_MULTI_SIGN), 0x534D5400);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_VALIDATION), 0x56414C00);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_PROPOSAL), 0x50525000);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_MANIFEST), 0x4D414E00);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_PAYMENT_CHANNEL_CLAIM), 0x434C4D00);
        assert_eq!(u32::from_be_bytes(HASH_PREFIX_BATCH), 0x42434800);
    }

    #[test]
    fn sha512_half_prefixed_deterministic() {
        let h1 = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, b"test data");
        let h2 = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, b"test data");
        assert_eq!(h1, h2);

        // Different prefix → different hash
        let h3 = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, b"test data");
        assert_ne!(h1, h3);

        // Different data → different hash
        let h4 = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, b"other data");
        assert_ne!(h1, h4);
    }

    #[test]
    fn sha512_half_is_32_bytes() {
        let h = sha512_half(b"anything");
        assert_eq!(h.0.len(), 32);
    }

    #[test]
    fn empty_inner_node_hash() {
        // An empty inner node hashes 4-byte prefix + 512 zero bytes (16 * 32-byte empty children)
        let mut data = vec![0u8; 16 * 32];
        let hash = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data);
        // This should be a deterministic, non-zero hash
        assert_ne!(hash.0, [0u8; 32]);
    }
}
