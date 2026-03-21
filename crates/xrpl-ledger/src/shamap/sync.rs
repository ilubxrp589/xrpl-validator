//! SHAMap sync — tracks which nodes are needed and processes fetched data.

use std::collections::VecDeque;

use xrpl_core::types::Hash256;

use super::node::ZERO_HASH;
use crate::nodestore::NodeStore;
use crate::LedgerError;

/// Tracks the progress of syncing a SHAMap from the network.
pub struct SHAMapSync {
    /// Nodes we need to fetch (depth, hash).
    needed: VecDeque<(u32, Hash256)>,
    /// Number of nodes successfully fetched.
    fetched: usize,
    /// Expected root hash.
    root_hash: Hash256,
}

impl SHAMapSync {
    /// Start syncing a SHAMap with a known root hash.
    pub fn new(root_hash: Hash256) -> Self {
        let mut needed = VecDeque::new();
        if root_hash != ZERO_HASH {
            needed.push_back((0, root_hash));
        }
        Self {
            needed,
            fetched: 0,
            root_hash,
        }
    }

    /// Get a batch of node hashes to request from a peer.
    pub fn request_batch(&self, max: usize) -> Vec<Hash256> {
        self.needed
            .iter()
            .take(max)
            .map(|(_, hash)| *hash)
            .collect()
    }

    /// Process a fetched node: verify its hash and store it.
    ///
    /// If it's an inner node, adds its children to the needed queue.
    /// Returns the number of new children added.
    pub fn process_node<S: NodeStore>(
        &mut self,
        expected_hash: &Hash256,
        data: &[u8],
        is_inner: bool,
        store: &S,
    ) -> Result<usize, LedgerError> {
        // Verify the hash matches what we expected.
        // Inner nodes: SHA512Half(HASH_PREFIX_INNER_NODE || data)
        // Leaf nodes:  SHA512Half(HASH_PREFIX_LEAF_NODE || data)
        let prefix = if is_inner {
            &super::hash::HASH_PREFIX_INNER_NODE
        } else {
            &super::hash::HASH_PREFIX_LEAF_NODE
        };
        let actual_hash = super::hash::sha512_half_prefixed(prefix, data);

        if actual_hash != *expected_hash {
            return Err(LedgerError::HashMismatch {
                expected: hex::encode(expected_hash.0),
                actual: hex::encode(actual_hash.0),
            });
        }

        store.store(expected_hash, data)?;

        // Remove from needed
        self.needed.retain(|(_, h)| h != expected_hash);
        self.fetched += 1;

        // If inner node, extract 16 child hashes and queue missing ones
        let mut new_children = 0;
        if is_inner && data.len() == 512 {
            for i in 0..16 {
                let offset = i * 32;
                let mut child_hash = [0u8; 32];
                child_hash.copy_from_slice(&data[offset..offset + 32]);
                let child = Hash256(child_hash);

                if child != ZERO_HASH && !store.exists(&child)? {
                    self.needed.push_back((0, child));
                    new_children += 1;
                }
            }
        }

        Ok(new_children)
    }

    /// Check if sync is complete (no more nodes needed).
    pub fn is_complete(&self) -> bool {
        self.needed.is_empty()
    }

    /// Get progress: (fetched, remaining).
    pub fn progress(&self) -> (usize, usize) {
        (self.fetched, self.needed.len())
    }

    /// The expected root hash.
    pub fn root_hash(&self) -> &Hash256 {
        &self.root_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodestore::InMemoryStore;

    #[test]
    fn new_with_zero_hash() {
        let sync = SHAMapSync::new(ZERO_HASH);
        assert!(sync.is_complete());
        assert_eq!(sync.progress(), (0, 0));
    }

    #[test]
    fn new_with_real_hash() {
        let hash = Hash256([0xAA; 32]);
        let sync = SHAMapSync::new(hash);
        assert!(!sync.is_complete());
        assert_eq!(sync.progress(), (0, 1));
        assert_eq!(sync.request_batch(10), vec![hash]);
    }

    #[test]
    fn process_leaf_node_valid_hash() {
        let data = vec![1, 2, 3, 4, 5];
        let hash = super::super::hash::sha512_half_prefixed(
            &super::super::hash::HASH_PREFIX_LEAF_NODE,
            &data,
        );
        let mut sync = SHAMapSync::new(hash);
        let store = InMemoryStore::new();

        let new_children = sync.process_node(&hash, &data, false, &store).unwrap();

        assert_eq!(new_children, 0);
        assert!(sync.is_complete());
        assert_eq!(sync.progress(), (1, 0));
        assert!(store.exists(&hash).unwrap());
    }

    #[test]
    fn process_node_rejects_bad_hash() {
        let bad_hash = Hash256([0xFF; 32]);
        let mut sync = SHAMapSync::new(bad_hash);
        let store = InMemoryStore::new();

        let data = vec![1, 2, 3];
        let result = sync.process_node(&bad_hash, &data, false, &store);
        assert!(result.is_err());
    }

    #[test]
    fn process_inner_node_adds_children() {
        // Build inner node data (512 bytes = 16 child hashes)
        let mut inner_data = vec![0u8; 512];
        inner_data[0..32].copy_from_slice(&[0xDD; 32]); // child 0
        inner_data[32..64].copy_from_slice(&[0xEE; 32]); // child 1
        // rest are zero = empty

        let root = super::super::hash::sha512_half_prefixed(
            &super::super::hash::HASH_PREFIX_INNER_NODE,
            &inner_data,
        );

        let mut sync = SHAMapSync::new(root);
        let store = InMemoryStore::new();

        let new_children = sync.process_node(&root, &inner_data, true, &store).unwrap();
        assert_eq!(new_children, 2);
        assert!(!sync.is_complete());
        assert_eq!(sync.progress(), (1, 2));
    }
}
