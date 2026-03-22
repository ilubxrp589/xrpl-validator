//! Sled persistent storage backend.

#[cfg(feature = "sled-backend")]
use std::path::Path;

#[cfg(feature = "sled-backend")]
use xrpl_core::types::Hash256;

#[cfg(feature = "sled-backend")]
use super::NodeStore;
#[cfg(feature = "sled-backend")]
use crate::LedgerError;

/// Persistent storage backend using sled embedded database.
#[cfg(feature = "sled-backend")]
pub struct SledStore {
    db: sled::Db,
}

#[cfg(feature = "sled-backend")]
impl SledStore {
    /// Open or create a sled database at the given path.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let db = sled::open(path).map_err(|e| LedgerError::StorageError(format!("sled open: {e}")))?;
        Ok(Self { db })
    }

    /// Number of entries in the store.
    pub fn len(&self) -> usize {
        self.db.len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.db.is_empty()
    }

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<(), LedgerError> {
        self.db
            .flush()
            .map_err(|e| LedgerError::StorageError(format!("sled flush: {e}")))?;
        Ok(())
    }
}

#[cfg(feature = "sled-backend")]
impl NodeStore for SledStore {
    fn fetch(&self, hash: &Hash256) -> Result<Option<Vec<u8>>, LedgerError> {
        self.db
            .get(hash.0)
            .map(|opt| opt.map(|v| v.to_vec()))
            .map_err(|e| LedgerError::StorageError(format!("sled get: {e}")))
    }

    fn store(&self, hash: &Hash256, data: &[u8]) -> Result<(), LedgerError> {
        self.db
            .insert(hash.0, data)
            .map_err(|e| LedgerError::StorageError(format!("sled insert: {e}")))?;
        Ok(())
    }

    fn exists(&self, hash: &Hash256) -> Result<bool, LedgerError> {
        self.db
            .contains_key(hash.0)
            .map_err(|e| LedgerError::StorageError(format!("sled contains: {e}")))
    }

    fn batch_store(&self, items: &[(Hash256, Vec<u8>)]) -> Result<(), LedgerError> {
        let mut batch = sled::Batch::default();
        for (hash, data) in items {
            batch.insert(&hash.0, data.as_slice());
        }
        self.db
            .apply_batch(batch)
            .map_err(|e| LedgerError::StorageError(format!("sled batch: {e}")))?;
        Ok(())
    }
}

#[cfg(all(test, feature = "sled-backend"))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn store_and_fetch() {
        let dir = tempdir().unwrap();
        let store = SledStore::open(dir.path()).unwrap();
        let hash = Hash256([0xAA; 32]);
        let data = vec![1, 2, 3, 4];

        store.store(&hash, &data).unwrap();
        assert_eq!(store.fetch(&hash).unwrap(), Some(data));
    }

    #[test]
    fn fetch_nonexistent() {
        let dir = tempdir().unwrap();
        let store = SledStore::open(dir.path()).unwrap();
        assert_eq!(store.fetch(&Hash256([0xBB; 32])).unwrap(), None);
    }

    #[test]
    fn exists() {
        let dir = tempdir().unwrap();
        let store = SledStore::open(dir.path()).unwrap();
        let hash = Hash256([0xCC; 32]);
        assert!(!store.exists(&hash).unwrap());
        store.store(&hash, &[1]).unwrap();
        assert!(store.exists(&hash).unwrap());
    }

    #[test]
    fn batch_store() {
        let dir = tempdir().unwrap();
        let store = SledStore::open(dir.path()).unwrap();
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
    fn persistence_across_reopen() {
        let dir = tempdir().unwrap();
        let hash = Hash256([0xDD; 32]);
        let data = vec![42, 43, 44];

        {
            let store = SledStore::open(dir.path()).unwrap();
            store.store(&hash, &data).unwrap();
            store.flush().unwrap();
        }

        // Reopen
        let store = SledStore::open(dir.path()).unwrap();
        assert_eq!(store.fetch(&hash).unwrap(), Some(data));
    }
}
