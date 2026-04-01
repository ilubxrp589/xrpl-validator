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

#[test]
fn flat_vs_tree_hash() {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
    use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE};
    use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};
    use xrpl_core::types::Hash256;

    fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
        if entries.is_empty() { return ZERO_HASH; }
        if entries.len() == 1 { return entries[0].1; }
        let mut child_hashes = [ZERO_HASH; 16];
        let mut pos = 0;
        for nibble in 0..16u8 {
            let end = entries[pos..].partition_point(|&(key, _)| nibble_at(&key, depth) <= nibble) + pos;
            let bucket_start = entries[pos..end].partition_point(|&(key, _)| nibble_at(&key, depth) < nibble) + pos;
            if bucket_start < end {
                child_hashes[nibble as usize] = compute_subtree(&entries[bucket_start..end], depth + 1);
            }
            pos = end;
        }
        let mut data = [0u8; 16 * 32];
        for (i, h) in child_hashes.iter().enumerate() {
            data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }
        sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
    }

    let mut entries: Vec<(Hash256, Hash256)> = Vec::new();
    for i in 0u32..100 {
        let mut key = [0u8; 32];
        key[0..4].copy_from_slice(&i.to_be_bytes());
        let mut data = vec![0u8; 50];
        data[0..4].copy_from_slice(&(i * 7 + 13).to_be_bytes());
        data.extend_from_slice(&key);
        let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data);
        entries.push((Hash256(key), lh));
    }
    entries.sort_by(|a, b| a.0.0.cmp(&b.0.0));

    let flat = compute_subtree(&entries, 0);

    let mut tree = SHAMap::new(TreeType::State);
    for (k, h) in &entries {
        tree.insert_hash_only(*k, *h).unwrap();
    }
    let tree_hash = tree.root_hash();

    eprintln!("Flat:  {}", hex::encode(&flat.0[..16]));
    eprintln!("Tree:  {}", hex::encode(&tree_hash.0[..16]));

    // Test 1 entry
    let one = &entries[..1];
    let flat1 = compute_subtree(one, 0);
    let mut t1 = SHAMap::new(TreeType::State);
    t1.insert_hash_only(one[0].0, one[0].1).unwrap();
    eprintln!("1 entry — flat: {} tree: {}", hex::encode(&flat1.0[..8]), hex::encode(&t1.root_hash().0[..8]));

    // Test 2 entries
    let two = &entries[..2];
    let flat2 = compute_subtree(two, 0);
    let mut t2 = SHAMap::new(TreeType::State);
    for (k, h) in two { t2.insert_hash_only(*k, *h).unwrap(); }
    eprintln!("2 entries — flat: {} tree: {}", hex::encode(&flat2.0[..8]), hex::encode(&t2.root_hash().0[..8]));

    assert_eq!(flat, tree_hash, "100 entries: flat vs tree mismatch");
}

#[test]
fn flat_vs_tree_hash_sha_keys() {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
    use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE};
    use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};
    use xrpl_core::types::Hash256;
    use sha2::{Sha256, Digest};

    fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
        if entries.is_empty() { return ZERO_HASH; }
        if entries.len() == 1 { return entries[0].1; }
        let mut child_hashes = [ZERO_HASH; 16];
        let mut pos = 0;
        for nibble in 0..16u8 {
            let end = entries[pos..].partition_point(|&(key, _)| nibble_at(&key, depth) <= nibble) + pos;
            let bucket_start = entries[pos..end].partition_point(|&(key, _)| nibble_at(&key, depth) < nibble) + pos;
            if bucket_start < end {
                child_hashes[nibble as usize] = compute_subtree(&entries[bucket_start..end], depth + 1);
            }
            pos = end;
        }
        let mut data = [0u8; 16 * 32];
        for (i, h) in child_hashes.iter().enumerate() {
            data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }
        sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
    }

    // Use SHA256 hashed keys (realistic distribution like XRPL)
    let mut entries: Vec<(Hash256, Hash256)> = Vec::new();
    for i in 0u32..10000 {
        let h = Sha256::digest(i.to_be_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&h);
        let mut data = vec![0u8; 100];
        data[0..4].copy_from_slice(&i.to_be_bytes());
        data.extend_from_slice(&key);
        let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data);
        entries.push((Hash256(key), lh));
    }
    entries.sort_by(|a, b| a.0.0.cmp(&b.0.0));

    let flat = compute_subtree(&entries, 0);

    let mut tree = SHAMap::new(TreeType::State);
    for (k, h) in &entries {
        tree.insert_hash_only(*k, *h).unwrap();
    }
    let tree_hash = tree.root_hash();

    eprintln!("10k SHA keys — flat: {} tree: {} match={}", 
        hex::encode(&flat.0[..8]), hex::encode(&tree_hash.0[..8]), flat == tree_hash);

    assert_eq!(flat, tree_hash, "10k SHA keys: flat vs tree mismatch");
}

#[test]
fn flat_vs_tree_incremental_update() {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
    use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE};
    use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};
    use xrpl_core::types::Hash256;
    use sha2::{Sha256, Digest};

    fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
        if entries.is_empty() { return ZERO_HASH; }
        if entries.len() == 1 { return entries[0].1; }
        let mut child_hashes = [ZERO_HASH; 16];
        let mut pos = 0;
        for nibble in 0..16u8 {
            let end = entries[pos..].partition_point(|&(key, _)| nibble_at(&key, depth) <= nibble) + pos;
            let bucket_start = entries[pos..end].partition_point(|&(key, _)| nibble_at(&key, depth) < nibble) + pos;
            if bucket_start < end {
                child_hashes[nibble as usize] = compute_subtree(&entries[bucket_start..end], depth + 1);
            }
            pos = end;
        }
        let mut data = [0u8; 16 * 32];
        for (i, h) in child_hashes.iter().enumerate() {
            data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }
        sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
    }

    fn make_entry(i: u32) -> (Hash256, Hash256) {
        let h = Sha256::digest(i.to_be_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&h);
        let mut data = vec![0u8; 100];
        data[0..4].copy_from_slice(&i.to_be_bytes());
        data.extend_from_slice(&key);
        let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data);
        (Hash256(key), lh)
    }

    // Build initial set
    let mut entries: Vec<(Hash256, Hash256)> = (0..1000).map(make_entry).collect();
    entries.sort_by(|a, b| a.0.0.cmp(&b.0.0));

    let mut tree = SHAMap::new(TreeType::State);
    for (k, h) in &entries { tree.insert_hash_only(*k, *h).unwrap(); }

    let flat1 = compute_subtree(&entries, 0);
    let tree1 = tree.root_hash();
    eprintln!("Initial 1000: flat={} tree={} match={}", hex::encode(&flat1.0[..8]), hex::encode(&tree1.0[..8]), flat1 == tree1);
    assert_eq!(flat1, tree1, "Initial build mismatch");

    // Simulate a ledger round: update 50 existing keys with new hashes
    for i in 0..50u32 {
        let (key, _) = make_entry(i);
        let new_data = vec![i as u8; 200]; // different data
        let mut buf = Vec::new();
        buf.extend_from_slice(&new_data);
        buf.extend_from_slice(&key.0);
        let new_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
        
        // Update tree
        tree.insert_hash_only(key, new_hash).unwrap();
        
        // Update flat array
        if let Ok(idx) = entries.binary_search_by(|e| e.0.0.cmp(&key.0)) {
            entries[idx].1 = new_hash;
        }
    }

    // Add 10 new entries
    for i in 1000..1010u32 {
        let (key, hash) = make_entry(i);
        tree.insert_hash_only(key, hash).unwrap();
        let pos = entries.binary_search_by(|e| e.0.0.cmp(&key.0)).unwrap_err();
        entries.insert(pos, (key, hash));
    }

    // Delete 5 entries
    for i in 50..55u32 {
        let (key, _) = make_entry(i);
        tree.delete(&key).unwrap();
        if let Ok(idx) = entries.binary_search_by(|e| e.0.0.cmp(&key.0)) {
            entries.remove(idx);
        }
    }

    let flat2 = compute_subtree(&entries, 0);
    let tree2 = tree.root_hash();
    eprintln!("After updates: flat={} tree={} match={}", hex::encode(&flat2.0[..8]), hex::encode(&tree2.0[..8]), flat2 == tree2);
    assert_eq!(flat2, tree2, "Incremental update mismatch");
}

#[test]
fn tree_update_vs_rebuild() {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
    use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
    use xrpl_core::types::Hash256;
    use sha2::{Sha256, Digest};

    fn make_entry(i: u32) -> (Hash256, Hash256) {
        let h = Sha256::digest(i.to_be_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&h);
        let mut data = vec![0u8; 100];
        data[0..4].copy_from_slice(&i.to_be_bytes());
        data.extend_from_slice(&key);
        (Hash256(key), sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data))
    }

    fn make_entry_v2(i: u32) -> (Hash256, Hash256) {
        let h = Sha256::digest(i.to_be_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&h);
        let mut data = vec![1u8; 200]; // different data
        data[0..4].copy_from_slice(&i.to_be_bytes());
        data.extend_from_slice(&key);
        (Hash256(key), sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data))
    }

    // Build tree with 50000 entries
    let mut tree = SHAMap::new(TreeType::State);
    for i in 0..50000u32 {
        let (k, h) = make_entry(i);
        tree.insert_hash_only(k, h).unwrap();
    }
    let hash1 = tree.root_hash();
    eprintln!("Initial 50k: {}", hex::encode(&hash1.0[..8]));

    // Update 200 entries (simulate a ledger round)
    for i in 0..200u32 {
        let (k, h) = make_entry_v2(i);
        tree.insert_hash_only(k, h).unwrap();
    }
    let hash_updated = tree.root_hash();
    eprintln!("After update: {}", hex::encode(&hash_updated.0[..8]));

    // Build fresh tree with the same final state
    let mut fresh = SHAMap::new(TreeType::State);
    for i in 200..50000u32 {
        let (k, h) = make_entry(i); // unchanged
        fresh.insert_hash_only(k, h).unwrap();
    }
    for i in 0..200u32 {
        let (k, h) = make_entry_v2(i); // updated
        fresh.insert_hash_only(k, h).unwrap();
    }
    let hash_fresh = fresh.root_hash();
    eprintln!("Fresh build:  {}", hex::encode(&hash_fresh.0[..8]));

    assert_ne!(hash1, hash_updated, "update should change hash");
    assert_eq!(hash_updated, hash_fresh, "incremental update should match fresh build");
}

#[test]
fn tree_vs_rebuild_after_update_500k() {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
    use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
    use xrpl_core::types::Hash256;
    use sha2::{Sha256, Digest};

    fn make(i: u32, version: u8) -> (Hash256, Hash256) {
        let h = Sha256::digest(i.to_be_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&h);
        let mut data = vec![version; 100 + (i % 200) as usize];
        data[0..4].copy_from_slice(&i.to_be_bytes());
        data.extend_from_slice(&key);
        (Hash256(key), sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data))
    }

    // Build tree with 500k entries
    let mut tree = SHAMap::new(TreeType::State);
    for i in 0..500_000u32 {
        let (k, h) = make(i, 0);
        tree.insert_hash_only(k, h).unwrap();
    }
    let h1 = tree.root_hash();
    eprintln!("500k initial: {}", hex::encode(&h1.0[..8]));

    // Update 200 keys with new data
    for i in 0..200u32 {
        let (k, h) = make(i, 1);
        tree.insert_hash_only(k, h).unwrap();
    }
    // Add 5 new keys
    for i in 500_000..500_005u32 {
        let (k, h) = make(i, 1);
        tree.insert_hash_only(k, h).unwrap();
    }
    // Delete 3 keys
    for i in 200..203u32 {
        let (k, _) = make(i, 0);
        tree.delete(&k).unwrap();
    }
    let h2 = tree.root_hash();

    // Build fresh tree with same final state
    let mut fresh = SHAMap::new(TreeType::State);
    for i in 0..200u32 {
        let (k, h) = make(i, 1); // updated
        fresh.insert_hash_only(k, h).unwrap();
    }
    for i in 203..500_000u32 { // skip deleted 200-202
        let (k, h) = make(i, 0); // original
        fresh.insert_hash_only(k, h).unwrap();
    }
    for i in 500_000..500_005u32 {
        let (k, h) = make(i, 1); // new
        fresh.insert_hash_only(k, h).unwrap();
    }
    let h3 = fresh.root_hash();

    eprintln!("500k updated:  {} entries={}", hex::encode(&h2.0[..8]), tree.len());
    eprintln!("500k fresh:    {} entries={}", hex::encode(&h3.0[..8]), fresh.len());
    assert_eq!(h2, h3, "500k incremental vs fresh mismatch");
}

#[test]
fn insertion_order_independence() {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
    use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
    use xrpl_core::types::Hash256;
    use sha2::{Sha256, Digest};

    fn make(i: u32) -> (Hash256, Hash256) {
        let h = Sha256::digest(i.to_be_bytes());
        let mut key = [0u8; 32]; key.copy_from_slice(&h);
        let mut data = vec![0u8; 100]; data[0..4].copy_from_slice(&i.to_be_bytes());
        data.extend_from_slice(&key);
        (Hash256(key), sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data))
    }

    // Build tree in order 0..1000
    let mut t1 = SHAMap::new(TreeType::State);
    for i in 0..1000u32 { let (k,h) = make(i); t1.insert_hash_only(k,h).unwrap(); }

    // Build tree in reverse order
    let mut t2 = SHAMap::new(TreeType::State);
    for i in (0..1000u32).rev() { let (k,h) = make(i); t2.insert_hash_only(k,h).unwrap(); }

    // Build tree in random-ish order
    let mut t3 = SHAMap::new(TreeType::State);
    let order: Vec<u32> = (0..1000).map(|i| (i * 7 + 13) % 1000).collect();
    for i in order { let (k,h) = make(i); t3.insert_hash_only(k,h).unwrap(); }

    let h1 = t1.root_hash();
    let h2 = t2.root_hash();
    let h3 = t3.root_hash();
    eprintln!("Forward:  {}", hex::encode(&h1.0[..8]));
    eprintln!("Reverse:  {}", hex::encode(&h2.0[..8]));
    eprintln!("Shuffled: {}", hex::encode(&h3.0[..8]));
    assert_eq!(h1, h2, "forward vs reverse");
    assert_eq!(h1, h3, "forward vs shuffled");
}

#[test]
fn tree_30_sequential_updates() {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};
    use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};
    use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};
    use xrpl_core::types::Hash256;
    use sha2::{Sha256, Digest};

    fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
        if entries.is_empty() { return ZERO_HASH; }
        if entries.len() == 1 { return entries[0].1; }
        let mut child_hashes = [ZERO_HASH; 16];
        let mut pos = 0;
        for nibble in 0..16u8 {
            let end = entries[pos..].partition_point(|&(key, _)| nibble_at(&key, depth) <= nibble) + pos;
            let bucket_start = entries[pos..end].partition_point(|&(key, _)| nibble_at(&key, depth) < nibble) + pos;
            if bucket_start < end { child_hashes[nibble as usize] = compute_subtree(&entries[bucket_start..end], depth + 1); }
            pos = end;
        }
        let mut data = [0u8; 16 * 32];
        for (i, h) in child_hashes.iter().enumerate() { data[i*32..(i+1)*32].copy_from_slice(&h.0); }
        sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
    }

    fn make(i: u32, v: u32) -> (Hash256, Hash256) {
        let h = Sha256::digest(i.to_be_bytes());
        let mut key = [0u8; 32]; key.copy_from_slice(&h);
        let mut data = vec![v as u8; 50 + (i % 150) as usize];
        data[0..4].copy_from_slice(&v.to_be_bytes());
        data.extend_from_slice(&key);
        (Hash256(key), sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &data))
    }

    // Build with 100k entries (larger than previous tests)
    let n = 100_000u32;
    let mut tree = SHAMap::new(TreeType::State);
    let mut flat: Vec<(Hash256, Hash256)> = Vec::new();
    for i in 0..n {
        let (k, h) = make(i, 0);
        tree.insert_hash_only(k, h).unwrap();
        flat.push((k, h));
    }
    flat.sort_by(|a, b| a.0.0.cmp(&b.0.0));

    // Verify initial
    assert_eq!(compute_subtree(&flat, 0), tree.root_hash(), "initial mismatch");

    // Apply 30 "rounds" of updates — like 30 ledgers
    for round in 1..=30u32 {
        let base = round * 200; // different keys each round
        let mut updates = Vec::new();

        // Update 100 existing keys
        for i in 0..100u32 {
            let idx = (base + i) % n;
            let (k, h) = make(idx, round);
            tree.insert_hash_only(k, h).unwrap();
            updates.push((k, Some(h)));
        }
        // Insert 20 new keys
        for i in 0..20u32 {
            let (k, h) = make(n + round * 20 + i, round);
            tree.insert_hash_only(k, h).unwrap();
            updates.push((k, Some(h)));
        }
        // Delete 10 keys
        for i in 0..10u32 {
            let idx = (base + 100 + i) % n;
            let (k, _) = make(idx, 0);
            let _ = tree.delete(&k);
            updates.push((k, None));
        }

        // Apply same updates to flat array
        updates.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let mut new_flat = Vec::with_capacity(flat.len() + updates.len());
        let (mut ci, mut ui) = (0, 0);
        while ci < flat.len() || ui < updates.len() {
            if ui >= updates.len() { new_flat.extend_from_slice(&flat[ci..]); break; }
            else if ci >= flat.len() { for u in &updates[ui..] { if let Some(h) = u.1 { new_flat.push((u.0, h)); } } break; }
            else if flat[ci].0.0 < updates[ui].0.0 { new_flat.push(flat[ci]); ci += 1; }
            else if flat[ci].0.0 > updates[ui].0.0 { if let Some(h) = updates[ui].1 { new_flat.push((updates[ui].0, h)); } ui += 1; }
            else { if let Some(h) = updates[ui].1 { new_flat.push((updates[ui].0, h)); } ci += 1; ui += 1; }
        }
        flat = new_flat;

        let fh = compute_subtree(&flat, 0);
        let th = tree.root_hash();
        if fh != th {
            eprintln!("DIVERGENCE at round {round}: flat={} tree={} flat_len={} tree_len={}",
                hex::encode(&fh.0[..8]), hex::encode(&th.0[..8]), flat.len(), tree.len());
            assert_eq!(fh, th, "round {round} mismatch");
        }
    }
    eprintln!("All 30 rounds matched! Final: {} entries", flat.len());
}
