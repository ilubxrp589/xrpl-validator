//! In-memory NodeStore backed by a HashMap.

use std::collections::HashMap;

use parking_lot::RwLock;
use xrpl_core::types::Hash256;

use super::NodeStore;
use crate::LedgerError;

/// In-memory storage backend using a HashMap behind a RwLock.
pub struct InMemoryStore {
    data: RwLock<HashMap<Hash256, Vec<u8>>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }

    /// Number of entries stored.
    pub fn len(&self) -> usize {
        self.data.read().len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.data.read().is_empty()
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeStore for InMemoryStore {
    fn fetch(&self, hash: &Hash256) -> Result<Option<Vec<u8>>, LedgerError> {
        Ok(self.data.read().get(hash).cloned())
    }

    fn store(&self, hash: &Hash256, data: &[u8]) -> Result<(), LedgerError> {
        self.data.write().insert(*hash, data.to_vec());
        Ok(())
    }

    fn exists(&self, hash: &Hash256) -> Result<bool, LedgerError> {
        Ok(self.data.read().contains_key(hash))
    }

    fn batch_store(&self, items: &[(Hash256, Vec<u8>)]) -> Result<(), LedgerError> {
        let mut store = self.data.write();
        for (hash, data) in items {
            store.insert(*hash, data.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_fetch() {
        let store = InMemoryStore::new();
        let hash = Hash256([0xAA; 32]);
        let data = vec![1, 2, 3, 4];

        store.store(&hash, &data).unwrap();
        let fetched = store.fetch(&hash).unwrap();
        assert_eq!(fetched, Some(data));
    }

    #[test]
    fn fetch_nonexistent() {
        let store = InMemoryStore::new();
        let hash = Hash256([0xBB; 32]);
        assert_eq!(store.fetch(&hash).unwrap(), None);
    }

    #[test]
    fn exists() {
        let store = InMemoryStore::new();
        let hash = Hash256([0xCC; 32]);
        assert!(!store.exists(&hash).unwrap());

        store.store(&hash, &[1]).unwrap();
        assert!(store.exists(&hash).unwrap());
    }

    #[test]
    fn batch_store() {
        let store = InMemoryStore::new();
        let items = vec![
            (Hash256([0x01; 32]), vec![1]),
            (Hash256([0x02; 32]), vec![2]),
            (Hash256([0x03; 32]), vec![3]),
        ];

        store.batch_store(&items).unwrap();
        assert_eq!(store.len(), 3);
        assert_eq!(store.fetch(&Hash256([0x02; 32])).unwrap(), Some(vec![2]));
    }

    #[test]
    fn overwrite() {
        let store = InMemoryStore::new();
        let hash = Hash256([0xDD; 32]);

        store.store(&hash, &[1]).unwrap();
        store.store(&hash, &[2]).unwrap();
        assert_eq!(store.fetch(&hash).unwrap(), Some(vec![2]));
        assert_eq!(store.len(), 1);
    }
}
