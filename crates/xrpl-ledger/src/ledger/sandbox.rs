//! Sandbox — transactional overlay view over a LedgerState.
//!
//! Allows tentative modifications without committing to the real state.
//! If a transaction succeeds, call `commit()` to apply changes.
//! If it fails, call `discard()` or just drop the sandbox.

use std::collections::HashMap;
use xrpl_core::types::Hash256;

use super::state::LedgerState;
use crate::LedgerError;

/// What happened to a ledger object in this sandbox.
#[derive(Debug, Clone)]
pub enum SandboxEntry {
    /// New object created in this sandbox.
    Created(Vec<u8>),
    /// Existing object modified.
    Modified(Vec<u8>),
    /// Existing object deleted.
    Deleted,
}

/// A transactional overlay over a LedgerState.
///
/// Reads fall through to the base state if not modified.
/// Writes are buffered until `commit()` is called.
pub struct Sandbox<'a> {
    base: &'a LedgerState,
    modifications: HashMap<Hash256, SandboxEntry>,
}

impl<'a> Sandbox<'a> {
    /// Create a new sandbox over the given ledger state.
    pub fn new(base: &'a LedgerState) -> Self {
        Self {
            base,
            modifications: HashMap::new(),
        }
    }

    /// Read an object by key. Checks modifications first, then falls through to base.
    /// Returns `None` if the object doesn't exist or was deleted.
    pub fn read(&self, key: &Hash256) -> Option<Vec<u8>> {
        match self.modifications.get(key) {
            Some(SandboxEntry::Created(data)) | Some(SandboxEntry::Modified(data)) => {
                Some(data.clone())
            }
            Some(SandboxEntry::Deleted) => None,
            None => self.base.state_map.lookup(key).map(|d| d.to_vec()),
        }
    }

    /// Write an object. If it exists in base, records as Modified; otherwise Created.
    pub fn write(&mut self, key: Hash256, data: Vec<u8>) {
        let entry = if self.base.state_map.lookup(&key).is_some() {
            // Check if we already created it in this sandbox
            match self.modifications.get(&key) {
                Some(SandboxEntry::Created(_)) => SandboxEntry::Created(data),
                _ => SandboxEntry::Modified(data),
            }
        } else {
            // Not in base — check if we previously created it in this sandbox
            match self.modifications.get(&key) {
                Some(SandboxEntry::Created(_)) => SandboxEntry::Created(data),
                Some(SandboxEntry::Modified(_)) => SandboxEntry::Modified(data),
                _ => SandboxEntry::Created(data),
            }
        };
        self.modifications.insert(key, entry);
    }

    /// Mark an object as deleted.
    pub fn delete(&mut self, key: Hash256) {
        self.modifications.insert(key, SandboxEntry::Deleted);
    }

    /// Check if a key exists (in modifications or base, and not deleted).
    pub fn exists(&self, key: &Hash256) -> bool {
        match self.modifications.get(key) {
            Some(SandboxEntry::Created(_)) | Some(SandboxEntry::Modified(_)) => true,
            Some(SandboxEntry::Deleted) => false,
            None => self.base.state_map.lookup(key).is_some(),
        }
    }

    /// Take a snapshot of the current modifications (for partial rollback).
    ///
    /// Returns a clone of the modifications map at this point in time.
    /// Use `restore_snapshot` to revert to this state.
    pub fn snapshot(&self) -> HashMap<Hash256, SandboxEntry> {
        self.modifications.clone()
    }

    /// Restore modifications to a previously-saved snapshot.
    ///
    /// Any modifications made after the snapshot was taken are discarded.
    pub fn restore_snapshot(&mut self, snapshot: HashMap<Hash256, SandboxEntry>) {
        self.modifications = snapshot;
    }

    /// Extract all modifications, consuming the sandbox.
    /// This drops the borrow on base so the caller can then mutate the state.
    pub fn into_modifications(self) -> HashMap<Hash256, SandboxEntry> {
        self.modifications
    }

    /// Discard all modifications (no-op, just consumes self).
    pub fn discard(self) {
        // Modifications are dropped
    }

    /// Get an iterator over all modifications (for metadata generation).
    pub fn modifications(&self) -> &HashMap<Hash256, SandboxEntry> {
        &self.modifications
    }

    /// Number of modifications in this sandbox.
    pub fn modification_count(&self) -> usize {
        self.modifications.len()
    }

    /// Access the base ledger state (read-only).
    pub fn base(&self) -> &LedgerState {
        self.base
    }
}

/// Apply sandbox modifications to a LedgerState's SHAMap.
/// Call this after `sandbox.into_modifications()` to commit changes.
pub fn apply_modifications(
    state: &mut LedgerState,
    mods: HashMap<Hash256, SandboxEntry>,
) -> Result<(), LedgerError> {
    for (key, entry) in mods {
        match entry {
            SandboxEntry::Created(data) | SandboxEntry::Modified(data) => {
                state.state_map.insert(key, data)?;
            }
            SandboxEntry::Deleted => {
                state.state_map.delete(&key)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;

    fn test_state() -> LedgerState {
        let header = LedgerHeader {
            sequence: 1,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        LedgerState::new_unverified(header)
    }

    fn key(n: u8) -> Hash256 {
        let mut h = [0u8; 32];
        h[0] = n;
        Hash256(h)
    }

    #[test]
    fn read_through_to_base() {
        let mut state = test_state();
        let k = key(1);
        state.state_map.insert(k, vec![0xAA, 0xBB]).unwrap();

        let sandbox = Sandbox::new(&state);
        assert_eq!(sandbox.read(&k), Some(vec![0xAA, 0xBB]));
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let state = test_state();
        let sandbox = Sandbox::new(&state);
        assert_eq!(sandbox.read(&key(99)), None);
    }

    #[test]
    fn write_overrides_base() {
        let mut state = test_state();
        let k = key(1);
        state.state_map.insert(k, vec![0xAA]).unwrap();

        let mut sandbox = Sandbox::new(&state);
        sandbox.write(k, vec![0xBB]);
        assert_eq!(sandbox.read(&k), Some(vec![0xBB]));

        // Base is untouched
        assert_eq!(state.state_map.lookup(&k), Some([0xAA].as_slice()));
    }

    #[test]
    fn delete_hides_base_entry() {
        let mut state = test_state();
        let k = key(1);
        state.state_map.insert(k, vec![0xAA]).unwrap();

        let mut sandbox = Sandbox::new(&state);
        sandbox.delete(k);
        assert_eq!(sandbox.read(&k), None);
        assert!(!sandbox.exists(&k));

        // Base still has it
        assert!(state.state_map.lookup(&k).is_some());
    }

    #[test]
    fn commit_applies_changes() {
        let mut state = test_state();
        let k1 = key(1);
        let k2 = key(2);
        let k3 = key(3);
        state.state_map.insert(k1, vec![0x01]).unwrap();
        state.state_map.insert(k3, vec![0x03]).unwrap();

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            sandbox.write(k1, vec![0xFF]); // modify
            sandbox.write(k2, vec![0x02]); // create
            sandbox.delete(k3);            // delete
            sandbox.into_modifications()
        };
        // Sandbox dropped — state is no longer borrowed
        apply_modifications(&mut state, mods).unwrap();

        assert_eq!(state.state_map.lookup(&k1), Some([0xFF].as_slice()));
        assert_eq!(state.state_map.lookup(&k2), Some([0x02].as_slice()));
        assert_eq!(state.state_map.lookup(&k3), None);
    }

    #[test]
    fn discard_loses_changes() {
        let mut state = test_state();
        let k = key(1);
        state.state_map.insert(k, vec![0xAA]).unwrap();

        let mut sandbox = Sandbox::new(&state);
        sandbox.write(k, vec![0xFF]);
        sandbox.discard();

        // State unchanged
        assert_eq!(state.state_map.lookup(&k), Some([0xAA].as_slice()));
    }

    #[test]
    fn exists_checks_correctly() {
        let mut state = test_state();
        let k1 = key(1);
        state.state_map.insert(k1, vec![0x01]).unwrap();

        let mut sandbox = Sandbox::new(&state);
        assert!(sandbox.exists(&k1));
        assert!(!sandbox.exists(&key(99)));

        // Create new
        let k2 = key(2);
        sandbox.write(k2, vec![0x02]);
        assert!(sandbox.exists(&k2));

        // Delete existing
        sandbox.delete(k1);
        assert!(!sandbox.exists(&k1));
    }

    #[test]
    fn modification_count() {
        let state = test_state();
        let mut sandbox = Sandbox::new(&state);
        assert_eq!(sandbox.modification_count(), 0);

        sandbox.write(key(1), vec![0x01]);
        sandbox.write(key(2), vec![0x02]);
        sandbox.delete(key(3));
        assert_eq!(sandbox.modification_count(), 3);
    }
}
