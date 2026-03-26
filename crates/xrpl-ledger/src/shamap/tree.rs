//! SHAMap trie — insert, lookup, delete, and root hash computation.
//!
//! Inner nodes store actual child node references (not just hashes),
//! enabling full traversal for lookup and deletion.

use xrpl_core::types::Hash256;

use super::node::{nibble_at, InnerNode, LeafNode, SHAMapNode, ZERO_HASH};
use crate::LedgerError;

/// The type of SHAMap tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeType {
    /// Account state tree (ledger objects).
    State,
    /// Transaction tree.
    Transaction,
}

/// A SHAMap — Merkle-Patricia trie with branching factor 16.
///
/// Keys are 256-bit hashes. Navigation uses nibbles (4-bit segments) of the key.
/// Maximum depth: 64 (one nibble per level, 32 bytes = 64 nibbles).
///
/// Inner nodes store actual child references, enabling full traversal.
/// The root hash is computed lazily from the tree structure.
#[derive(Debug, Clone)]
pub struct SHAMap {
    /// Root node — always starts as an empty InnerNode.
    root: SHAMapNode,
    tree_type: TreeType,
    size: usize,
}

impl SHAMap {
    /// Create a new empty SHAMap.
    pub fn new(tree_type: TreeType) -> Self {
        Self {
            root: SHAMapNode::Inner(Box::<InnerNode>::default()),
            tree_type,
            size: 0,
        }
    }

    /// Create a SHAMap from a pre-built root node (bulk construction).
    pub fn from_root(root: SHAMapNode, tree_type: TreeType, size: usize) -> Self {
        Self { root, tree_type, size }
    }

    pub fn tree_type(&self) -> TreeType {
        self.tree_type
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Compute the root hash of the tree.
    pub fn root_hash(&self) -> Hash256 {
        if self.is_empty() {
            return ZERO_HASH;
        }
        self.root.hash()
    }

    /// Access the root node (for traversal, e.g., diff).
    pub fn root_node(&self) -> &SHAMapNode {
        &self.root
    }

    /// Look up a value by key.
    pub fn lookup(&self, key: &Hash256) -> Option<&[u8]> {
        lookup_in(&self.root, key, 0)
    }

    /// Insert a key-value pair. Returns true if the key was new (not an overwrite).
    pub fn insert(&mut self, key: Hash256, data: Vec<u8>) -> Result<bool, LedgerError> {
        let is_new = insert_into_mut(&mut self.root, key, data, 0)?;
        if is_new {
            self.size += 1;
        }
        Ok(is_new)
    }

    /// Insert a hash-only leaf (no data stored). Used for memory-efficient
    /// state hash computation where leaf data is never read back.
    pub fn insert_hash_only(&mut self, key: Hash256, leaf_hash: Hash256) -> Result<bool, LedgerError> {
        let is_new = insert_hash_only_mut(&mut self.root, key, leaf_hash, 0)?;
        if is_new {
            self.size += 1;
        }
        Ok(is_new)
    }

    /// Delete a key. Returns true if the key was found and removed.
    pub fn delete(&mut self, key: &Hash256) -> Result<bool, LedgerError> {
        let found = delete_from_mut(&mut self.root, key, 0)?;
        if found {
            self.size -= 1;
        }
        Ok(found)
    }
}

// ---- Recursive helpers ----

fn lookup_in<'a>(node: &'a SHAMapNode, key: &Hash256, depth: usize) -> Option<&'a [u8]> {
    match node {
        SHAMapNode::Leaf(leaf) => {
            if leaf.key() == key {
                Some(leaf.data())
            } else {
                None
            }
        }
        SHAMapNode::Inner(inner) => {
            let nibble = nibble_at(key, depth);
            inner.get_child_node(nibble).and_then(|child| lookup_in(child, key, depth + 1))
        }
    }
}

/// In-place insert — modifies the tree without cloning.
fn insert_into_mut(
    node: &mut SHAMapNode,
    key: Hash256,
    data: Vec<u8>,
    depth: usize,
) -> Result<bool, LedgerError> {
    if depth >= 64 {
        return Err(LedgerError::InvalidTreeType(
            "SHAMap depth exceeded 64".to_string(),
        ));
    }

    match node {
        SHAMapNode::Inner(ref mut inner) => {
            let nibble = nibble_at(&key, depth);

            if inner.has_child(nibble) {
                // Recurse into the existing child in-place
                let child = inner.get_child_node_mut(nibble).unwrap();
                insert_into_mut(child, key, data, depth + 1)
            } else {
                // Empty slot — place the leaf directly
                inner.set_child_node(nibble, SHAMapNode::Leaf(LeafNode::new(key, data)));
                Ok(true)
            }
        }
        SHAMapNode::Leaf(ref existing) => {
            if existing.key() == &key {
                // Overwrite existing leaf in place
                *node = SHAMapNode::Leaf(LeafNode::new(key, data));
                Ok(false)
            } else {
                // Collision — replace this leaf with an inner node containing both
                let existing_key = *existing.key();
                let existing_nibble = nibble_at(&existing_key, depth);
                let new_nibble = nibble_at(&key, depth);

                // Take the existing leaf out, replace with inner
                let old_node = std::mem::replace(
                    node,
                    SHAMapNode::Inner(Box::<InnerNode>::default()),
                );

                if let SHAMapNode::Inner(ref mut inner) = node {
                    if existing_nibble == new_nibble {
                        // Same nibble — place existing leaf, then recurse to split deeper
                        inner.set_child_node(existing_nibble, old_node);
                        let child = inner.get_child_node_mut(existing_nibble).unwrap();
                        insert_into_mut(child, key, data, depth + 1)?;
                    } else {
                        // Different nibbles — place both as direct children
                        inner.set_child_node(existing_nibble, old_node);
                        inner.set_child_node(new_nibble, SHAMapNode::Leaf(LeafNode::new(key, data)));
                    }
                }
                Ok(true)
            }
        }
    }
}

/// In-place delete — modifies the tree without cloning.
fn delete_from_mut(
    node: &mut SHAMapNode,
    key: &Hash256,
    depth: usize,
) -> Result<bool, LedgerError> {
    match node {
        SHAMapNode::Leaf(ref leaf) => {
            if leaf.key() == key {
                // Replace leaf with empty inner (parent handles collapsing)
                *node = SHAMapNode::Inner(Box::<InnerNode>::default());
                Ok(true)
            } else {
                Ok(false)
            }
        }
        SHAMapNode::Inner(ref mut inner) => {
            let nibble = nibble_at(key, depth);

            if !inner.has_child(nibble) {
                return Ok(false);
            }

            let child = inner.get_child_node_mut(nibble).unwrap();
            let found = delete_from_mut(child, key, depth + 1)?;

            if !found {
                return Ok(false);
            }

            // If child became an empty inner node, remove it
            if let SHAMapNode::Inner(ref i) = inner.get_child_node(nibble).unwrap() {
                if i.is_empty() {
                    inner.remove_child(nibble);
                }
            }

            // Collapse: if only one child remains and it's a leaf, promote it
            if inner.child_count() == 1 {
                if let Some(only_idx) = inner.only_child_index() {
                    if matches!(inner.get_child_node(only_idx), Some(SHAMapNode::Leaf(_))) {
                        let promoted = inner.take_child_node(only_idx).unwrap();
                        *node = promoted;
                    }
                }
            }

            Ok(true)
        }
    }
}

/// In-place insert of a hash-only leaf — no data stored, only the pre-computed hash.
fn insert_hash_only_mut(
    node: &mut SHAMapNode,
    key: Hash256,
    leaf_hash: Hash256,
    depth: usize,
) -> Result<bool, LedgerError> {
    if depth >= 64 {
        return Err(LedgerError::InvalidTreeType(
            "SHAMap depth exceeded 64".to_string(),
        ));
    }

    match node {
        SHAMapNode::Inner(ref mut inner) => {
            let nibble = nibble_at(&key, depth);

            if inner.has_child(nibble) {
                let child = inner.get_child_node_mut(nibble).unwrap();
                insert_hash_only_mut(child, key, leaf_hash, depth + 1)
            } else {
                inner.set_child_node(nibble, SHAMapNode::Leaf(LeafNode::new_hash_only(key, leaf_hash)));
                Ok(true)
            }
        }
        SHAMapNode::Leaf(ref existing) => {
            if existing.key() == &key {
                *node = SHAMapNode::Leaf(LeafNode::new_hash_only(key, leaf_hash));
                Ok(false)
            } else {
                let existing_key = *existing.key();
                let existing_nibble = nibble_at(&existing_key, depth);
                let new_nibble = nibble_at(&key, depth);

                let old_node = std::mem::replace(
                    node,
                    SHAMapNode::Inner(Box::<InnerNode>::default()),
                );

                if let SHAMapNode::Inner(ref mut inner) = node {
                    if existing_nibble == new_nibble {
                        inner.set_child_node(existing_nibble, old_node);
                        let child = inner.get_child_node_mut(existing_nibble).unwrap();
                        insert_hash_only_mut(child, key, leaf_hash, depth + 1)?;
                    } else {
                        inner.set_child_node(existing_nibble, old_node);
                        inner.set_child_node(new_nibble, SHAMapNode::Leaf(LeafNode::new_hash_only(key, leaf_hash)));
                    }
                }
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(byte: u8) -> Hash256 {
        Hash256([byte; 32])
    }

    #[test]
    fn empty_tree() {
        let tree = SHAMap::new(TreeType::State);
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.root_hash(), ZERO_HASH);
    }

    #[test]
    fn insert_and_lookup() {
        let mut tree = SHAMap::new(TreeType::State);
        let key = make_key(0xAA);
        tree.insert(key, vec![1, 2, 3]).unwrap();

        assert_eq!(tree.len(), 1);
        assert_eq!(tree.lookup(&key), Some(&[1, 2, 3][..]));
        assert_eq!(tree.lookup(&make_key(0xBB)), None);
    }

    #[test]
    fn insert_overwrite() {
        let mut tree = SHAMap::new(TreeType::State);
        let key = make_key(0xAA);

        let is_new = tree.insert(key, vec![1]).unwrap();
        assert!(is_new);
        assert_eq!(tree.len(), 1);

        let is_new = tree.insert(key, vec![2]).unwrap();
        assert!(!is_new);
        assert_eq!(tree.len(), 1); // size unchanged
        assert_eq!(tree.lookup(&key), Some(&[2][..]));
    }

    #[test]
    fn insert_multiple_different_first_nibble() {
        let mut tree = SHAMap::new(TreeType::State);

        let mut k1 = [0u8; 32]; k1[0] = 0x10;
        let mut k2 = [0u8; 32]; k2[0] = 0x20;
        let mut k3 = [0u8; 32]; k3[0] = 0x30;

        tree.insert(Hash256(k1), vec![1]).unwrap();
        tree.insert(Hash256(k2), vec![2]).unwrap();
        tree.insert(Hash256(k3), vec![3]).unwrap();

        assert_eq!(tree.len(), 3);
        assert_eq!(tree.lookup(&Hash256(k1)), Some(&[1][..]));
        assert_eq!(tree.lookup(&Hash256(k2)), Some(&[2][..]));
        assert_eq!(tree.lookup(&Hash256(k3)), Some(&[3][..]));
    }

    #[test]
    fn insert_collision_same_first_nibble() {
        let mut tree = SHAMap::new(TreeType::State);

        let mut k1 = [0u8; 32]; k1[0] = 0xA1;
        let mut k2 = [0u8; 32]; k2[0] = 0xA2;

        tree.insert(Hash256(k1), vec![1]).unwrap();
        tree.insert(Hash256(k2), vec![2]).unwrap();

        assert_eq!(tree.len(), 2);
        assert_eq!(tree.lookup(&Hash256(k1)), Some(&[1][..]));
        assert_eq!(tree.lookup(&Hash256(k2)), Some(&[2][..]));
    }

    #[test]
    fn insert_deep_collision() {
        let mut tree = SHAMap::new(TreeType::State);

        // Keys share the first 4 nibbles (2 bytes) but differ at nibble 4
        let mut k1 = [0u8; 32]; k1[0] = 0xAB; k1[1] = 0xC1;
        let mut k2 = [0u8; 32]; k2[0] = 0xAB; k2[1] = 0xC2;

        tree.insert(Hash256(k1), vec![1]).unwrap();
        tree.insert(Hash256(k2), vec![2]).unwrap();

        assert_eq!(tree.len(), 2);
        assert_eq!(tree.lookup(&Hash256(k1)), Some(&[1][..]));
        assert_eq!(tree.lookup(&Hash256(k2)), Some(&[2][..]));
    }

    #[test]
    fn delete_existing() {
        let mut tree = SHAMap::new(TreeType::State);
        let key = make_key(0xBB);

        tree.insert(key, vec![1, 2, 3]).unwrap();
        assert_eq!(tree.len(), 1);

        let found = tree.delete(&key).unwrap();
        assert!(found);
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.lookup(&key), None);
    }

    #[test]
    fn delete_nonexistent() {
        let mut tree = SHAMap::new(TreeType::State);
        tree.insert(make_key(0xAA), vec![1]).unwrap();

        let found = tree.delete(&make_key(0xBB)).unwrap();
        assert!(!found);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn delete_preserves_siblings() {
        let mut tree = SHAMap::new(TreeType::State);

        let mut k1 = [0u8; 32]; k1[0] = 0x10;
        let mut k2 = [0u8; 32]; k2[0] = 0x20;

        tree.insert(Hash256(k1), vec![1]).unwrap();
        tree.insert(Hash256(k2), vec![2]).unwrap();
        assert_eq!(tree.len(), 2);

        tree.delete(&Hash256(k1)).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.lookup(&Hash256(k1)), None);
        assert_eq!(tree.lookup(&Hash256(k2)), Some(&[2][..]));
    }

    #[test]
    fn root_hash_deterministic() {
        let mut t1 = SHAMap::new(TreeType::State);
        let mut t2 = SHAMap::new(TreeType::State);

        t1.insert(make_key(0xAA), vec![1, 2, 3]).unwrap();
        t2.insert(make_key(0xAA), vec![1, 2, 3]).unwrap();

        assert_eq!(t1.root_hash(), t2.root_hash());
    }

    #[test]
    fn different_data_different_hash() {
        let mut t1 = SHAMap::new(TreeType::State);
        let mut t2 = SHAMap::new(TreeType::State);

        t1.insert(make_key(0xAA), vec![1, 2, 3]).unwrap();
        t2.insert(make_key(0xAA), vec![4, 5, 6]).unwrap();

        assert_ne!(t1.root_hash(), t2.root_hash());
    }

    #[test]
    fn order_independence() {
        let mut t1 = SHAMap::new(TreeType::State);
        let mut t2 = SHAMap::new(TreeType::State);

        let keys: Vec<(Hash256, Vec<u8>)> = (0..10u8).map(|i| {
            let mut k = [0u8; 32];
            k[0] = i * 17;
            (Hash256(k), vec![i])
        }).collect();

        // Forward order
        for (k, v) in &keys {
            t1.insert(*k, v.clone()).unwrap();
        }

        // Reverse order
        for (k, v) in keys.iter().rev() {
            t2.insert(*k, v.clone()).unwrap();
        }

        assert_eq!(t1.root_hash(), t2.root_hash());

        // Verify all lookups work in both
        for (k, v) in &keys {
            assert_eq!(t1.lookup(k), Some(v.as_slice()));
            assert_eq!(t2.lookup(k), Some(v.as_slice()));
        }
    }

    #[test]
    fn insert_many_and_lookup_all() {
        let mut tree = SHAMap::new(TreeType::State);
        for i in 0..=255u8 {
            tree.insert(make_key(i), vec![i]).unwrap();
        }
        assert_eq!(tree.len(), 256);

        for i in 0..=255u8 {
            assert_eq!(tree.lookup(&make_key(i)), Some(&[i][..]),
                "lookup failed for key 0x{i:02x}");
        }
    }
}
