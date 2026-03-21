//! SHAMap diff — compute changes between two tree versions.

use xrpl_core::types::Hash256;

use super::node::SHAMapNode;
use super::tree::SHAMap;

/// Differences between two SHAMap trees.
#[derive(Debug, Default)]
pub struct SHAMapDiff {
    /// Keys of entries added in the new tree.
    pub added: Vec<Hash256>,
    /// Keys of entries removed from the old tree.
    pub removed: Vec<Hash256>,
    /// Keys of entries present in both but with different data.
    pub modified: Vec<Hash256>,
}

impl SHAMapDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    pub fn total_changes(&self) -> usize {
        self.added.len() + self.removed.len() + self.modified.len()
    }
}

/// Compute the diff between two SHAMap trees by structural traversal.
///
/// Skips subtrees with matching hashes (the Merkle optimization).
pub fn diff(old: &SHAMap, new: &SHAMap) -> SHAMapDiff {
    let mut result = SHAMapDiff::default();

    if old.root_hash() == new.root_hash() {
        return result;
    }

    diff_nodes(old.root_node(), new.root_node(), &mut result);
    result
}

fn diff_nodes(old: &SHAMapNode, new: &SHAMapNode, result: &mut SHAMapDiff) {
    // If hashes match, subtrees are identical — skip
    if old.hash() == new.hash() {
        return;
    }

    match (old, new) {
        // Both inner — recurse into children
        (SHAMapNode::Inner(old_inner), SHAMapNode::Inner(new_inner)) => {
            for i in 0..16u8 {
                match (old_inner.get_child_node(i), new_inner.get_child_node(i)) {
                    (Some(old_child), Some(new_child)) => {
                        diff_nodes(old_child, new_child, result);
                    }
                    (Some(old_child), None) => {
                        collect_leaves(old_child, &mut result.removed);
                    }
                    (None, Some(new_child)) => {
                        collect_leaves(new_child, &mut result.added);
                    }
                    (None, None) => {}
                }
            }
        }
        // Old is leaf, new is inner — old leaf removed, new subtree added
        (SHAMapNode::Leaf(old_leaf), SHAMapNode::Inner(_)) => {
            result.removed.push(*old_leaf.key());
            collect_leaves(new, &mut result.added);
        }
        // Old is inner, new is leaf — old subtree removed, new leaf added
        (SHAMapNode::Inner(_), SHAMapNode::Leaf(new_leaf)) => {
            collect_leaves(old, &mut result.removed);
            result.added.push(*new_leaf.key());
        }
        // Both leaves
        (SHAMapNode::Leaf(old_leaf), SHAMapNode::Leaf(new_leaf)) => {
            if old_leaf.key() == new_leaf.key() {
                // Same key, different data → modified
                result.modified.push(*old_leaf.key());
            } else {
                // Different keys → one removed, one added
                result.removed.push(*old_leaf.key());
                result.added.push(*new_leaf.key());
            }
        }
    }
}

/// Collect all leaf keys from a subtree.
fn collect_leaves(node: &SHAMapNode, keys: &mut Vec<Hash256>) {
    match node {
        SHAMapNode::Leaf(leaf) => keys.push(*leaf.key()),
        SHAMapNode::Inner(inner) => {
            for i in 0..16u8 {
                if let Some(child) = inner.get_child_node(i) {
                    collect_leaves(child, keys);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shamap::tree::TreeType;

    fn make_key(byte: u8) -> Hash256 {
        Hash256([byte; 32])
    }

    #[test]
    fn identical_trees_no_diff() {
        let t1 = SHAMap::new(TreeType::State);
        let t2 = SHAMap::new(TreeType::State);
        let d = diff(&t1, &t2);
        assert!(d.is_empty());
    }

    #[test]
    fn detect_addition() {
        let t1 = SHAMap::new(TreeType::State);
        let mut t2 = SHAMap::new(TreeType::State);
        t2.insert(make_key(0xAA), vec![1]).unwrap();

        let d = diff(&t1, &t2);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0], make_key(0xAA));
        assert!(d.removed.is_empty());
        assert!(d.modified.is_empty());
    }

    #[test]
    fn detect_removal() {
        let mut t1 = SHAMap::new(TreeType::State);
        t1.insert(make_key(0xBB), vec![1]).unwrap();
        let t2 = SHAMap::new(TreeType::State);

        let d = diff(&t1, &t2);
        assert!(d.added.is_empty());
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0], make_key(0xBB));
    }

    #[test]
    fn detect_modification() {
        let mut t1 = SHAMap::new(TreeType::State);
        let mut t2 = SHAMap::new(TreeType::State);

        t1.insert(make_key(0xCC), vec![1]).unwrap();
        t2.insert(make_key(0xCC), vec![2]).unwrap();

        let d = diff(&t1, &t2);
        assert!(d.added.is_empty());
        assert!(d.removed.is_empty());
        assert_eq!(d.modified.len(), 1);
        assert_eq!(d.modified[0], make_key(0xCC));
    }

    #[test]
    fn mixed_changes() {
        let mut t1 = SHAMap::new(TreeType::State);
        let mut t2 = SHAMap::new(TreeType::State);

        // In t1: keys AA, BB
        t1.insert(make_key(0xAA), vec![1]).unwrap();
        t1.insert(make_key(0xBB), vec![2]).unwrap();

        // In t2: keys BB (modified), CC (added)
        t2.insert(make_key(0xBB), vec![99]).unwrap();
        t2.insert(make_key(0xCC), vec![3]).unwrap();

        let d = diff(&t1, &t2);
        assert!(d.added.contains(&make_key(0xCC)));
        assert!(d.removed.contains(&make_key(0xAA)));
        assert!(d.modified.contains(&make_key(0xBB)));
    }

    #[test]
    fn skip_unchanged_subtrees() {
        let mut t1 = SHAMap::new(TreeType::State);
        let mut t2 = SHAMap::new(TreeType::State);

        // Many shared keys
        for i in 0..20u8 {
            let mut k = [0u8; 32]; k[0] = i;
            t1.insert(Hash256(k), vec![i]).unwrap();
            t2.insert(Hash256(k), vec![i]).unwrap();
        }

        // One difference
        let mut k_diff = [0u8; 32]; k_diff[0] = 0xF0;
        t1.insert(Hash256(k_diff), vec![1]).unwrap();
        t2.insert(Hash256(k_diff), vec![2]).unwrap();

        let d = diff(&t1, &t2);
        assert_eq!(d.modified.len(), 1);
        assert!(d.added.is_empty());
        assert!(d.removed.is_empty());
    }
}
