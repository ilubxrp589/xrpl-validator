//! Candidate transaction-SET (SHAMap) root computation.
//!
//! The hash a consensus proposal commits to. This is NOT the ledger's
//! `transaction_hash` — that tree carries metadata and lives in
//! [`super::tx_tree`]. A proposal's set is the `tnTRANSACTION_NM` map,
//! "transaction, no metadata", and its leaves hash differently:
//!
//! ```text
//! txid       = SHA512Half("TXN\0" || tx_blob)     (the SHAMap key)
//! leaf_hash  = SHA512Half("TXN\0" || tx_blob)     (SHAMapTxLeafNode) == txid
//! inner_hash = SHA512Half("MIN\0" || child[0..15])
//! ```
//!
//! The leaf hash EQUALS the transaction id, because the item's slice is the raw
//! blob and the prefix is the same one that defines the id. Verified in
//! rippled's source rather than inferred — `SHAMapTxLeafNode.h`:
//!
//! ```cpp
//! updateHash() final
//! {
//!     hash_ = SHAMapHash{sha512Half(HashPrefix::TransactionId, item_->slice())};
//! }
//! ```
//!
//! Contrast `SHAMapTxPlusMetaLeafNode`, which prefixes "SND\0" over
//! `VL(tx) || VL(meta) || key`. Same tree, different leaves.
//!
//! ⚠ VERIFICATION STATUS. The tree machinery (`node_hash`, `nibble`) and the id
//! function (`tx_id`) are shared with [`super::tx_tree`], which IS checked
//! against a mainnet ledger's `transaction_hash` — so both halves of this
//! computation are independently verified. The composed NM root itself has NO
//! offline oracle: no RPC publishes a candidate-set hash. Direct confirmation
//! needs a live peer `TMProposeSet` alongside the set it names, which is what
//! M2a's acquisition path will make available. Do not describe this as
//! mainnet-verified until that capture exists.

use super::node::ZERO_HASH;
use super::tx_tree::{node_hash, tx_id};
use xrpl_core::types::Hash256;

/// Root of the candidate transaction set built from raw transaction blobs.
///
/// An empty set hashes to zero, matching rippled's empty SHAMap.
pub fn compute_tx_set_root_from_blobs(tx_blobs: &[Vec<u8>]) -> Hash256 {
    let ids: Vec<Hash256> = tx_blobs.iter().map(|b| tx_id(b)).collect();
    compute_tx_set_root(&ids)
}

/// Root of the candidate transaction set built from transaction IDs.
///
/// Both the SHAMap key and the leaf hash are the id itself, so the ids alone
/// determine the root — the blobs are not needed once they are known.
/// Order-independent: the tree is keyed, not sequential.
pub fn compute_tx_set_root(ids: &[Hash256]) -> Hash256 {
    if ids.is_empty() {
        return ZERO_HASH;
    }
    let items: Vec<([u8; 32], [u8; 32])> = ids.iter().map(|h| (h.0, h.0)).collect();
    Hash256(node_hash(&items, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash256 {
        Hash256([b; 32])
    }

    #[test]
    fn empty_set_is_zero() {
        assert_eq!(compute_tx_set_root(&[]), ZERO_HASH);
    }

    #[test]
    fn order_does_not_matter() {
        let a = compute_tx_set_root(&[h(1), h(2), h(3)]);
        let b = compute_tx_set_root(&[h(3), h(1), h(2)]);
        assert_eq!(a, b, "a SHAMap is keyed, so insertion order cannot matter");
    }

    #[test]
    fn membership_changes_the_root() {
        assert_ne!(compute_tx_set_root(&[h(1), h(2)]), compute_tx_set_root(&[h(1), h(3)]));
        assert_ne!(compute_tx_set_root(&[h(1)]), compute_tx_set_root(&[h(1), h(2)]));
    }

    #[test]
    fn root_is_not_the_bare_id_for_a_single_tx() {
        // The root is always an INNER node, even with one leaf — `node_hash`
        // collapses to a leaf only at depth > 0. A single-transaction set whose
        // root equalled the txid would mean the tree was skipped entirely.
        let only = h(7);
        assert_ne!(compute_tx_set_root(&[only]), only);
    }

    #[test]
    fn blobs_and_ids_agree() {
        let blobs = vec![b"\x12\x00\x00deadbeef".to_vec(), b"\x12\x00\x01cafebabe".to_vec()];
        let ids: Vec<Hash256> = blobs.iter().map(|b| tx_id(b)).collect();
        assert_eq!(compute_tx_set_root_from_blobs(&blobs), compute_tx_set_root(&ids));
    }

    #[test]
    fn leaf_hash_is_the_transaction_id() {
        // The property this module rests on (SHAMapTxLeafNode::updateHash):
        // the leaf hash and the SHAMap key are the same value.
        let blob = b"\x12\x00\x00some-transaction-blob".to_vec();
        let id = tx_id(&blob);
        let items = [(id.0, id.0)];
        assert_eq!(compute_tx_set_root(&[id]).0, super::node_hash(&items, 0));
    }
}
