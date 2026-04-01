//! SHAMap node types — inner nodes (branching factor 16) and leaf nodes.

use std::cell::Cell;
use xrpl_core::types::Hash256;

use super::hash::{sha512_half_prefixed, HASH_PREFIX_INNER_NODE, HASH_PREFIX_LEAF_NODE};

/// The zero hash — represents an empty child slot in an inner node.
pub const ZERO_HASH: Hash256 = Hash256([0u8; 32]);

/// A node in the SHAMap trie.
#[derive(Debug, Clone)]
pub enum SHAMapNode {
    Inner(Box<InnerNode>),
    Leaf(LeafNode),
}

impl SHAMapNode {
    /// Compute the hash of this node.
    pub fn hash(&self) -> Hash256 {
        match self {
            Self::Inner(inner) => inner.hash(),
            Self::Leaf(leaf) => leaf.hash(),
        }
    }

    /// Recursively clear all cached inner node hashes.
    /// After this, the next `hash()` call recomputes everything from scratch.
    /// Leaves are unaffected (their hash is stored, not cached).
    /// Cost: O(inner_nodes) — pure memory traversal, no I/O. ~2-3 seconds for 18.7M leaves.
    pub fn invalidate_all_caches(&mut self) {
        if let SHAMapNode::Inner(inner) = self {
            inner.cached_hash.set(None);
            for slot in inner.children.iter_mut().flatten() {
                slot.invalidate_all_caches();
            }
        }
    }
}

/// Inner node in the SHAMap — has up to 16 children (branching factor 16).
///
/// Hash computation:
/// `SHA512Half(HASH_PREFIX_INNER_NODE || child_0_hash || child_1_hash || ... || child_15_hash)`
///
/// Empty children contribute 32 zero bytes to the hash input.
#[derive(Debug, Clone)]
pub struct InnerNode {
    /// Child nodes — fixed 16 slots (branching factor 16). `None` = empty slot.
    children: [Option<SHAMapNode>; 16],
    /// Cached hash — invalidated on mutation, recomputed and cached lazily.
    /// Uses Cell for interior mutability so hash() can cache from &self.
    cached_hash: Cell<Option<Hash256>>,
}

impl InnerNode {
    /// Create a new empty inner node.
    pub fn new() -> Self {
        Self {
            children: std::array::from_fn(|_| None),
            cached_hash: Cell::new(None),
        }
    }

    /// Set a child node at the given index (0-15).
    pub fn set_child_node(&mut self, index: u8, node: SHAMapNode) {
        debug_assert!(index < 16, "child index must be 0-15");
        self.children[index as usize] = Some(node);
        self.cached_hash.set(None);
    }

    /// Take a child node out (replacing with None).
    pub fn take_child_node(&mut self, index: u8) -> Option<SHAMapNode> {
        debug_assert!(index < 16);
        self.cached_hash.set(None);
        self.children[index as usize].take()
    }

    /// Get a reference to a child node.
    pub fn get_child_node(&self, index: u8) -> Option<&SHAMapNode> {
        self.children[index as usize].as_ref()
    }

    /// Get a mutable reference to a child node.
    pub fn get_child_node_mut(&mut self, index: u8) -> Option<&mut SHAMapNode> {
        self.cached_hash.set(None);
        self.children[index as usize].as_mut()
    }

    /// Remove a child at the given index.
    pub fn remove_child(&mut self, index: u8) {
        debug_assert!(index < 16);
        self.children[index as usize] = None;
        self.cached_hash.set(None);
    }

    /// Get the hash of a child (returns zero hash for empty slots).
    pub fn child_hash(&self, index: u8) -> Hash256 {
        match &self.children[index as usize] {
            Some(node) => node.hash(),
            None => ZERO_HASH,
        }
    }

    /// Check if a child slot is occupied.
    pub fn has_child(&self, index: u8) -> bool {
        self.children[index as usize].is_some()
    }

    /// Count the number of non-empty children.
    pub fn child_count(&self) -> usize {
        self.children.iter().filter(|c| c.is_some()).count()
    }

    /// Check if all child slots are empty.
    pub fn is_empty(&self) -> bool {
        self.children.iter().all(|c| c.is_none())
    }

    /// Get the index of the single child, if exactly one exists.
    pub fn only_child_index(&self) -> Option<u8> {
        if self.child_count() != 1 {
            return None;
        }
        self.children
            .iter()
            .position(|c| c.is_some())
            .map(|i| i as u8)
    }

    /// Set the cached hash (used by bulk_build to avoid recomputation).
    pub fn set_cached_hash(&mut self, hash: Hash256) {
        self.cached_hash.set(Some(hash));
    }

    /// Compute the hash of this inner node.
    /// Caches the result via interior mutability for O(1) subsequent calls.
    ///
    /// Special case: if this node has exactly one child and it's a leaf,
    /// return the leaf hash directly (no inner node wrapping). This matches
    /// rippled's SHAMap behavior where single-leaf subtrees are collapsed.
    pub fn hash(&self) -> Hash256 {
        if let Some(h) = self.cached_hash.get() {
            return h;
        }

        // Single-leaf collapse: if exactly 1 child and it's a leaf, return leaf hash.
        if self.child_count() == 1 {
            if let Some(idx) = self.only_child_index() {
                if let Some(SHAMapNode::Leaf(ref leaf)) = self.children[idx as usize] {
                    let h = leaf.hash();
                    self.cached_hash.set(Some(h));
                    return h;
                }
            }
        }

        let mut data = [0u8; 16 * 32];
        for i in 0..16 {
            let h = self.child_hash(i as u8);
            data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }

        let hash = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data);
        self.cached_hash.set(Some(hash));
        hash
    }
}

impl Default for InnerNode {
    fn default() -> Self {
        Self::new()
    }
}

/// Leaf node in the SHAMap — holds a key and associated data.
///
/// Hash computation:
/// `SHA512Half(HASH_PREFIX_LEAF_NODE || key || data)`
///
/// Supports a "hash-only" mode where data is empty and the pre-computed
/// hash is cached. Used by StateHashComputer where leaf data is never read back.
#[derive(Debug, Clone)]
pub struct LeafNode {
    /// The 256-bit key (e.g., account state key or transaction hash).
    key: Hash256,
    /// The serialized data (e.g., encoded ledger object or transaction).
    /// Empty in hash-only mode.
    data: Vec<u8>,
    /// Pre-computed hash — avoids recomputation when set.
    cached_hash: Option<Hash256>,
}

impl LeafNode {
    /// Create a new leaf node with data.
    pub fn new(key: Hash256, data: Vec<u8>) -> Self {
        Self { key, data, cached_hash: None }
    }

    /// Create a hash-only leaf node (no data stored, only the pre-computed hash).
    /// Used for memory-efficient state hash computation.
    pub fn new_hash_only(key: Hash256, leaf_hash: Hash256) -> Self {
        Self { key, data: Vec::new(), cached_hash: Some(leaf_hash) }
    }

    /// The leaf's key.
    pub fn key(&self) -> &Hash256 {
        &self.key
    }

    /// The leaf's data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Compute the hash of this leaf node.
    ///
    /// `SHA512Half(HASH_PREFIX_LEAF_NODE || key || data)`
    pub fn hash(&self) -> Hash256 {
        if let Some(h) = self.cached_hash {
            return h;
        }
        // Rippled order: prefix || data || key
        let mut buf = Vec::with_capacity(self.data.len() + 32);
        buf.extend_from_slice(&self.data);
        buf.extend_from_slice(&self.key.0);
        sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
    }
}

/// Extract the nibble (4-bit value, 0-15) at a given depth from a 256-bit key.
///
/// Depth 0 is the most significant nibble of byte 0.
/// Depth 1 is the least significant nibble of byte 0.
/// Depth 2 is the most significant nibble of byte 1.
/// Maximum depth: 63 (64 nibbles in 32 bytes).
#[allow(clippy::manual_is_multiple_of)]
pub fn nibble_at(key: &Hash256, depth: usize) -> u8 {
    debug_assert!(depth < 64, "depth must be 0-63");
    let byte = key.0[depth / 2];
    if depth % 2 == 0 {
        (byte >> 4) & 0x0F // high nibble
    } else {
        byte & 0x0F // low nibble
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_inner_node() {
        let node = InnerNode::new();
        assert!(node.is_empty());
        assert_eq!(node.child_count(), 0);
        // Hash should be deterministic
        let h1 = node.hash();
        let h2 = node.hash();
        assert_eq!(h1, h2);
        assert_ne!(h1, ZERO_HASH); // not zero — it's SHA512Half of prefix + 512 zero bytes
    }

    #[test]
    fn inner_node_with_one_child() {
        let mut node = InnerNode::new();
        let child = LeafNode::new(Hash256([0xAB; 32]), vec![1, 2, 3]);
        let child_hash = SHAMapNode::Leaf(child.clone()).hash();
        node.set_child_node(5, SHAMapNode::Leaf(child));

        assert!(!node.is_empty());
        assert_eq!(node.child_count(), 1);
        assert!(node.has_child(5));
        assert!(!node.has_child(0));
        assert_eq!(node.child_hash(5), child_hash);
        assert_eq!(node.child_hash(0), ZERO_HASH);
        assert_eq!(node.only_child_index(), Some(5));
    }

    #[test]
    fn inner_node_hash_changes_with_children() {
        let mut node = InnerNode::new();
        let h_empty = node.hash();

        let leaf1 = SHAMapNode::Leaf(LeafNode::new(Hash256([0x01; 32]), vec![1]));
        node.set_child_node(0, leaf1);
        let h_one = node.hash();
        assert_ne!(h_empty, h_one);

        let leaf2 = SHAMapNode::Leaf(LeafNode::new(Hash256([0x02; 32]), vec![2]));
        node.set_child_node(1, leaf2);
        let h_two = node.hash();
        assert_ne!(h_one, h_two);

        node.remove_child(1);
        let h_back = node.hash();
        assert_eq!(h_one, h_back);
    }

    #[test]
    fn leaf_node_hash() {
        let key = Hash256([0xAA; 32]);
        let data = vec![1, 2, 3, 4, 5];
        let leaf = LeafNode::new(key, data.clone());

        assert_eq!(leaf.key(), &key);
        assert_eq!(leaf.data(), &data);

        let h1 = leaf.hash();
        let h2 = leaf.hash();
        assert_eq!(h1, h2);
        assert_ne!(h1, ZERO_HASH);
    }

    #[test]
    fn different_leaves_different_hashes() {
        let leaf1 = LeafNode::new(Hash256([0xAA; 32]), vec![1, 2, 3]);
        let leaf2 = LeafNode::new(Hash256([0xBB; 32]), vec![1, 2, 3]);
        let leaf3 = LeafNode::new(Hash256([0xAA; 32]), vec![4, 5, 6]);
        assert_ne!(leaf1.hash(), leaf2.hash()); // different key
        assert_ne!(leaf1.hash(), leaf3.hash()); // different data
    }

    #[test]
    fn shamap_node_enum_dispatch() {
        let inner = SHAMapNode::Inner(Box::new(InnerNode::new()));
        let leaf = SHAMapNode::Leaf(LeafNode::new(Hash256([0; 32]), vec![]));
        // Both should produce valid hashes
        assert_ne!(inner.hash(), ZERO_HASH);
        assert_ne!(leaf.hash(), ZERO_HASH);
        assert_ne!(inner.hash(), leaf.hash());
    }

    #[test]
    fn nibble_extraction() {
        // Key: 0xAB_CD_EF_... → nibbles: A, B, C, D, E, F, ...
        let mut key_bytes = [0u8; 32];
        key_bytes[0] = 0xAB;
        key_bytes[1] = 0xCD;
        key_bytes[2] = 0xEF;
        let key = Hash256(key_bytes);

        assert_eq!(nibble_at(&key, 0), 0x0A); // high nibble of byte 0
        assert_eq!(nibble_at(&key, 1), 0x0B); // low nibble of byte 0
        assert_eq!(nibble_at(&key, 2), 0x0C); // high nibble of byte 1
        assert_eq!(nibble_at(&key, 3), 0x0D); // low nibble of byte 1
        assert_eq!(nibble_at(&key, 4), 0x0E); // high nibble of byte 2
        assert_eq!(nibble_at(&key, 5), 0x0F); // low nibble of byte 2
    }

    #[test]
    fn nibble_all_zeros() {
        let key = Hash256([0; 32]);
        for d in 0..64 {
            assert_eq!(nibble_at(&key, d), 0);
        }
    }

    #[test]
    fn nibble_all_ones() {
        let key = Hash256([0xFF; 32]);
        for d in 0..64 {
            assert_eq!(nibble_at(&key, d), 0x0F);
        }
    }
}
