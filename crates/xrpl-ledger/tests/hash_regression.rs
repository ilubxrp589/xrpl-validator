//! Hash compatibility regression tests — stored test vectors.
//!
//! These verify our hash computations match rippled WITHOUT network access.
//! Vectors were captured from XRPL testnet on 2026-03-21.

use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::shamap::hash::{sha512_half, sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_LEDGER_MASTER};
use xrpl_ledger::shamap::node::{InnerNode, LeafNode, ZERO_HASH};

fn hex_to_hash(s: &str) -> Hash256 {
    let bytes = hex::decode(s).unwrap();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Hash256(arr)
}

#[test]
fn empty_shamap_root_is_zero() {
    let tree = xrpl_ledger::shamap::SHAMap::new(xrpl_ledger::shamap::TreeType::State);
    assert_eq!(tree.root_hash(), ZERO_HASH);
}

#[test]
fn empty_inner_node_hash_is_deterministic() {
    let node = InnerNode::new();
    let h1 = node.hash();
    let h2 = node.hash();
    assert_eq!(h1, h2);
    assert_ne!(h1, ZERO_HASH);
}

#[test]
fn leaf_node_hash_uses_correct_prefix() {
    let key = Hash256([0xAA; 32]);
    let data = vec![1, 2, 3, 4, 5];

    let leaf = LeafNode::new(key, data.clone());
    let expected = {
        // Rippled order: prefix || data || key
        let mut buf = Vec::new();
        buf.extend_from_slice(&data);
        buf.extend_from_slice(&key.0);
        sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
    };

    assert_eq!(leaf.hash(), expected);
}

#[test]
fn sha512_half_known_vector() {
    // SHA-512 of empty input, first 32 bytes
    let h = sha512_half(b"");
    // SHA-512("") = cf83e1357eefb8bd...
    assert_eq!(&hex::encode(h.0)[..8], "cf83e135");
}

#[test]
fn ledger_header_hash_known_vector() {
    // Captured from testnet ledger #15872618
    let header = LedgerHeader {
        sequence: 15872618,
        total_coins: 99999908484127946,
        parent_hash: hex_to_hash("FB467926A518AFC6C9B309D7ADC1ECF80AC851D9D8CCFD72E7886DA50BE5149A"),
        transaction_hash: hex_to_hash("51304A1A294B13EFC13E9F01B8994ECE3951186FDB7EF294756887165CAF9B3B"),
        account_hash: hex_to_hash("A02CB711652D91D67E997049398C5C174EA42F708A73C49B484D7FA3A6496A8E"),
        parent_close_time: 827449551,
        close_time: 827449552,
        close_time_resolution: 10,
        close_flags: 0,
    };

    let hash = header.hash();
    assert_eq!(
        hex::encode_upper(hash.0),
        "1074A773779084293C59D2743025D7149F56963F346D5746FEBCB4F8AC2BF182"
    );
}

#[test]
fn ledger_header_serialize_roundtrip() {
    let header = LedgerHeader {
        sequence: 12345678,
        total_coins: 99999999999000000,
        parent_hash: hex_to_hash("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        transaction_hash: hex_to_hash("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"),
        account_hash: hex_to_hash("CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC"),
        parent_close_time: 800000000,
        close_time: 800000010,
        close_time_resolution: 10,
        close_flags: 0,
    };

    let bytes = header.serialize();
    assert_eq!(bytes.len(), 118);
    let restored = LedgerHeader::deserialize(&bytes).unwrap();
    assert_eq!(header, restored);
}

#[test]
fn hash_prefix_inner_node_value() {
    assert_eq!(&HASH_PREFIX_INNER_NODE, b"MIN\0");
    assert_eq!(u32::from_be_bytes(HASH_PREFIX_INNER_NODE), 0x4D494E00);
}

#[test]
fn hash_prefix_ledger_master_value() {
    assert_eq!(&HASH_PREFIX_LEDGER_MASTER, b"LWR\0");
    assert_eq!(u32::from_be_bytes(HASH_PREFIX_LEDGER_MASTER), 0x4C575200);
}

#[test]
fn ripple_epoch_offset() {
    assert_eq!(xrpl_ledger::ledger::header::RIPPLE_EPOCH_OFFSET, 946684800);
    // 2000-01-01 00:00:00 UTC
    assert_eq!(LedgerHeader::ripple_to_unix(0), 946684800);
}
