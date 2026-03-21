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
        let (new_root, is_new) = insert_into(self.root.clone(), key, data, 0)?;
        self.root = new_root;
        if is_new {
            self.size += 1;
        }
        Ok(is_new)
    }

    /// Delete a key. Returns true if the key was found and removed.
    pub fn delete(&mut self, key: &Hash256) -> Result<bool, LedgerError> {
        let (new_root, found) = delete_from(self.root.clone(), key, 0)?;
        if found {
            self.root = new_root;
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

fn insert_into(
    node: SHAMapNode,
    key: Hash256,
    data: Vec<u8>,
    depth: usize,
) -> Result<(SHAMapNode, bool), LedgerError> {
    if depth >= 64 {
        return Err(LedgerError::InvalidTreeType(
            "SHAMap depth exceeded 64".to_string(),
        ));
    }

    match node {
        SHAMapNode::Inner(mut inner) => {
            let nibble = nibble_at(&key, depth);

            if let Some(existing_child) = inner.take_child_node(nibble) {
                // Recurse into the existing child
                let (new_child, is_new) = insert_into(existing_child, key, data, depth + 1)?;
                inner.set_child_node(nibble, new_child);
                Ok((SHAMapNode::Inner(inner), is_new))
            } else {
                // Empty slot — place the leaf directly
                let leaf = SHAMapNode::Leaf(LeafNode::new(key, data));
                inner.set_child_node(nibble, leaf);
                Ok((SHAMapNode::Inner(inner), true))
            }
        }
        SHAMapNode::Leaf(existing) => {
            if existing.key() == &key {
                // Overwrite existing leaf
                Ok((SHAMapNode::Leaf(LeafNode::new(key, data)), false))
            } else {
                // Collision — split into inner node with both leaves
                let mut new_inner = Box::<InnerNode>::default();
                let existing_nibble = nibble_at(existing.key(), depth);
                let new_nibble = nibble_at(&key, depth);

                if existing_nibble == new_nibble {
                    // Same nibble at this depth — recurse deeper
                    let (child, _) = insert_into(
                        SHAMapNode::Leaf(existing),
                        key,
                        data,
                        depth + 1,
                    )?;
                    new_inner.set_child_node(existing_nibble, child);
                } else {
                    // Different nibbles — place both as children
                    new_inner.set_child_node(existing_nibble, SHAMapNode::Leaf(existing));
                    new_inner.set_child_node(new_nibble, SHAMapNode::Leaf(LeafNode::new(key, data)));
                }
                Ok((SHAMapNode::Inner(new_inner), true))
            }
        }
    }
}

fn delete_from(
    node: SHAMapNode,
    key: &Hash256,
    depth: usize,
) -> Result<(SHAMapNode, bool), LedgerError> {
    match node {
        SHAMapNode::Leaf(leaf) => {
            if leaf.key() == key {
                // Replace with empty inner (parent will handle collapsing)
                Ok((SHAMapNode::Inner(Box::<InnerNode>::default()), true))
            } else {
                Ok((SHAMapNode::Leaf(leaf), false))
            }
        }
        SHAMapNode::Inner(mut inner) => {
            let nibble = nibble_at(key, depth);

            if let Some(child) = inner.take_child_node(nibble) {
                let (new_child, found) = delete_from(child, key, depth + 1)?;
                if found {
                    // Check if the new child is an empty inner node
                    match &new_child {
                        SHAMapNode::Inner(i) if i.is_empty() => {
                            // Don't put the empty inner back — slot becomes None
                        }
                        _ => {
                            inner.set_child_node(nibble, new_child);
                        }
                    }

                    // Collapse: if only one child remains, and it's a leaf, promote it
                    if inner.child_count() == 1 {
                        if let Some(only_idx) = inner.only_child_index() {
                            if let Some(SHAMapNode::Leaf(_)) = inner.get_child_node(only_idx) {
                                if let Some(promoted) = inner.take_child_node(only_idx) {
                                    return Ok((promoted, true));
                                }
                            }
                        }
                    }

                    Ok((SHAMapNode::Inner(inner), true))
                } else {
                    // Key not found deeper — put child back
                    inner.set_child_node(nibble, new_child);
                    Ok((SHAMapNode::Inner(inner), false))
                }
            } else {
                Ok((SHAMapNode::Inner(inner), false))
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
