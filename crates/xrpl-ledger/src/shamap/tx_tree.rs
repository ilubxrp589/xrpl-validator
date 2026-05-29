//! Transaction-tree (SHAMap) root computation.
//!
//! Recomputes a ledger's transaction tree root (the header `transaction_hash`)
//! from the raw transaction + metadata blobs, so ws-sync can verify that a pulled
//! transaction set is COMPLETE before applying it. Mirrors rippled's transaction
//! SHAMap exactly:
//!
//! ```text
//! txid       = SHA512Half("TXN\0" || tx_blob)                  (the SHAMap key)
//! leaf_data  = VL(tx_blob) || VL(meta_blob)                    (the SHAMapItem slice)
//! leaf_hash  = SHA512Half("SND\0" || leaf_data || txid)        (SHAMapTxPlusMetaLeafNode)
//! inner_hash = SHA512Half("MIN\0" || child[0..15])             (each 32B, zero if empty)
//! ```
//!
//! Source: vendored rippled `Ledger.cpp:581-586`, `SHAMapTxPlusMetaLeafNode.h:68-72`.

use super::hash::{
    sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_TRANSACTION_ID, HASH_PREFIX_TX_NODE,
};
use super::node::ZERO_HASH;
use xrpl_core::types::Hash256;

/// Encode an XRPL variable-length (VL) length prefix.
/// Mirrors rippled `Serializer::addEncoded` exactly. tx/meta blobs never approach
/// the 3-byte ceiling; the final branch clamps defensively (a wrong prefix can only
/// cause a completeness mismatch, never silent corruption — the gate would re-pull).
fn vl_prefix(len: usize) -> Vec<u8> {
    if len <= 192 {
        vec![len as u8]
    } else if len <= 12480 {
        let l = len - 193;
        vec![(193 + (l >> 8)) as u8, (l & 0xff) as u8]
    } else {
        let l = len.min(918_744) - 12481;
        vec![
            (241 + (l >> 16)) as u8,
            ((l >> 8) & 0xff) as u8,
            (l & 0xff) as u8,
        ]
    }
}

/// The transaction ID (the transaction tree's SHAMap key) for a raw tx blob:
/// `SHA512Half("TXN\0" || tx_blob)`.
pub fn tx_id(tx_blob: &[u8]) -> Hash256 {
    sha512_half_prefixed(&HASH_PREFIX_TRANSACTION_ID, tx_blob)
}

/// Build one transaction-tree leaf: returns `(key = txid, leaf_hash)`.
fn make_leaf(tx_blob: &[u8], meta_blob: &[u8]) -> ([u8; 32], [u8; 32]) {
    let txid = tx_id(tx_blob).0;
    // item->slice() = VL(tx) || VL(meta); leaf hash appends item->key() (the txid).
    let mut data = Vec::with_capacity(tx_blob.len() + meta_blob.len() + 40);
    data.extend_from_slice(&vl_prefix(tx_blob.len()));
    data.extend_from_slice(tx_blob);
    data.extend_from_slice(&vl_prefix(meta_blob.len()));
    data.extend_from_slice(meta_blob);
    data.extend_from_slice(&txid);
    let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_TX_NODE, &data).0;
    (txid, leaf_hash)
}

/// SHAMap branch for `key` at `depth`: high nibble of byte `depth/2` when depth is
/// even, low nibble when odd (rippled `SHAMapNodeID::selectBranch`).
fn nibble(key: &[u8; 32], depth: usize) -> usize {
    let byte = key[depth / 2];
    (if depth & 1 == 0 { byte >> 4 } else { byte & 0x0f }) as usize
}

/// Hash of the SHAMap node covering `items` at `depth`. The root (depth 0) is always
/// an inner node; a single item at depth > 0 collapses to its leaf hash.
fn node_hash(items: &[([u8; 32], [u8; 32])], depth: usize) -> [u8; 32] {
    if items.len() == 1 && depth > 0 {
        return items[0].1;
    }
    let mut buckets: Vec<Vec<([u8; 32], [u8; 32])>> = vec![Vec::new(); 16];
    for it in items {
        buckets[nibble(&it.0, depth)].push(*it);
    }
    let mut data = [0u8; 16 * 32];
    for (i, bucket) in buckets.iter().enumerate() {
        if !bucket.is_empty() {
            let h = node_hash(bucket, depth + 1);
            data[i * 32..(i + 1) * 32].copy_from_slice(&h);
        }
    }
    sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data).0
}

/// Compute the transaction-tree root from `(tx_blob, meta_blob)` pairs (raw bytes,
/// as returned by rippled's `ledger` RPC with `binary: true`). An empty set hashes
/// to zero, matching rippled's empty SHAMap.
pub fn compute_tx_tree_root(txs: &[(Vec<u8>, Vec<u8>)]) -> Hash256 {
    if txs.is_empty() {
        return ZERO_HASH;
    }
    let items: Vec<([u8; 32], [u8; 32])> =
        txs.iter().map(|(tx, meta)| make_leaf(tx, meta)).collect();
    Hash256(node_hash(&items, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/txtree")
    }

    fn load_fixture(path: &Path) -> (String, Vec<(Vec<u8>, Vec<u8>)>) {
        let content = fs::read_to_string(path).expect("read fixture");
        let mut lines = content.lines();
        let expected = lines.next().expect("expected hash line").trim().to_string();
        let mut txs = Vec::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (tb, mt) = line.split_once(',').expect("txblob,meta");
            txs.push((
                hex::decode(tb).expect("decode tx_blob"),
                hex::decode(mt).expect("decode meta"),
            ));
        }
        (expected, txs)
    }

    #[test]
    fn tx_tree_root_matches_mainnet_transaction_hash() {
        let dir = fixture_dir();
        let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("read fixture dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "txt"))
            .collect();
        paths.sort();
        let mut checked = 0usize;
        for path in &paths {
            let (expected, txs) = load_fixture(path);
            let got = hex::encode_upper(compute_tx_tree_root(&txs).0);
            assert_eq!(
                got,
                expected,
                "tx-tree root mismatch for {:?} ({} txs)",
                path.file_name().unwrap(),
                txs.len()
            );
            checked += 1;
        }
        assert!(checked >= 5, "expected >= 5 fixtures, found {checked}");
        eprintln!("tx-tree root verified against {checked} mainnet ledgers");
    }

    #[test]
    fn empty_tx_tree_is_zero() {
        assert_eq!(compute_tx_tree_root(&[]).0, ZERO_HASH.0);
    }

    /// An incomplete tx set must NOT reproduce the header root — both a missing
    /// middle tx and a truncated tail (which a contiguity check would miss).
    #[test]
    fn incomplete_tx_set_is_detected() {
        let path = fixture_dir().join("104552077.txt");
        let (expected, full) = load_fixture(&path);
        assert!(full.len() > 4, "need a multi-tx fixture");
        assert_eq!(hex::encode_upper(compute_tx_tree_root(&full).0), expected);

        // drop a middle tx
        let mut middle = full.clone();
        middle.remove(full.len() / 2);
        assert_ne!(
            hex::encode_upper(compute_tx_tree_root(&middle).0),
            expected,
            "missing middle tx must change the root"
        );

        // truncate the tail
        let tail = full[..full.len() - 1].to_vec();
        assert_ne!(
            hex::encode_upper(compute_tx_tree_root(&tail).0),
            expected,
            "truncated tail must change the root"
        );
    }
}
