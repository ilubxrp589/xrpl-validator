//! UNL — Unique Node List management.
//!
//! The UNL is the set of validators this node trusts for consensus.
//! Only proposals from UNL members influence consensus decisions.

use std::collections::{HashMap, HashSet};

/// A validator's identity manifest — binds master key to ephemeral signing key.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub master_key: Vec<u8>,
    pub signing_key: Vec<u8>,
    pub sequence: u32,
}

/// Manages the Unique Node List of trusted validators.
pub struct UNL {
    /// Trusted master public keys (33 bytes each, hex-encoded for fast lookup).
    trusted: HashSet<String>,
    /// Manifests: master key hex → latest manifest.
    manifests: HashMap<String, Manifest>,
}

impl UNL {
    /// Create a UNL from a list of hex-encoded public keys.
    pub fn from_keys(keys: &[String]) -> Self {
        let trusted: HashSet<String> = keys.iter().map(|k| k.to_uppercase()).collect();
        Self {
            trusted,
            manifests: HashMap::new(),
        }
    }

    /// Create an empty UNL (trusts no one).
    pub fn empty() -> Self {
        Self {
            trusted: HashSet::new(),
            manifests: HashMap::new(),
        }
    }

    /// Check if a validator public key is trusted.
    pub fn is_trusted(&self, pubkey_hex: &str) -> bool {
        self.trusted.contains(&pubkey_hex.to_uppercase())
    }

    /// Add a trusted key.
    pub fn add_trusted(&mut self, pubkey_hex: String) {
        self.trusted.insert(pubkey_hex.to_uppercase());
    }

    /// Number of trusted validators.
    pub fn trusted_count(&self) -> usize {
        self.trusted.len()
    }

    /// Get all trusted keys.
    pub fn trusted_keys(&self) -> &HashSet<String> {
        &self.trusted
    }

    /// Process a manifest — update the master→signing key mapping.
    /// Only accepts if sequence > existing sequence for this master key.
    pub fn process_manifest(&mut self, manifest: Manifest) {
        let key = hex::encode_upper(&manifest.master_key);
        let dominated = self
            .manifests
            .get(&key)
            .is_some_and(|existing| existing.sequence >= manifest.sequence);

        if !dominated {
            self.manifests.insert(key, manifest);
        }
    }

    /// Resolve a master key to the current ephemeral signing key.
    pub fn resolve_signing_key(&self, master_key_hex: &str) -> Option<&[u8]> {
        self.manifests
            .get(&master_key_hex.to_uppercase())
            .map(|m| m.signing_key.as_slice())
    }

    /// Get all manifests.
    pub fn manifests(&self) -> &HashMap<String, Manifest> {
        &self.manifests
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_keys() {
        let unl = UNL::from_keys(&["AABB".into(), "CCDD".into()]);
        assert_eq!(unl.trusted_count(), 2);
        assert!(unl.is_trusted("AABB"));
        assert!(unl.is_trusted("aabb")); // case insensitive
        assert!(!unl.is_trusted("EEFF"));
    }

    #[test]
    fn empty_trusts_nobody() {
        let unl = UNL::empty();
        assert_eq!(unl.trusted_count(), 0);
        assert!(!unl.is_trusted("anything"));
    }

    #[test]
    fn process_manifest() {
        let mut unl = UNL::from_keys(&["AABB".into()]);

        unl.process_manifest(Manifest {
            master_key: vec![0xAA, 0xBB],
            signing_key: vec![0x11, 0x22],
            sequence: 1,
        });

        assert_eq!(unl.resolve_signing_key("AABB"), Some(&[0x11, 0x22][..]));
    }

    #[test]
    fn manifest_higher_sequence_wins() {
        let mut unl = UNL::from_keys(&["AABB".into()]);

        unl.process_manifest(Manifest {
            master_key: vec![0xAA, 0xBB],
            signing_key: vec![0x11],
            sequence: 1,
        });

        unl.process_manifest(Manifest {
            master_key: vec![0xAA, 0xBB],
            signing_key: vec![0x22],
            sequence: 5,
        });

        assert_eq!(unl.resolve_signing_key("AABB"), Some(&[0x22][..]));
    }

    #[test]
    fn manifest_lower_sequence_rejected() {
        let mut unl = UNL::from_keys(&["AABB".into()]);

        unl.process_manifest(Manifest {
            master_key: vec![0xAA, 0xBB],
            signing_key: vec![0x11],
            sequence: 5,
        });

        unl.process_manifest(Manifest {
            master_key: vec![0xAA, 0xBB],
            signing_key: vec![0x22],
            sequence: 3,
        });

        // Old key stays
        assert_eq!(unl.resolve_signing_key("AABB"), Some(&[0x11][..]));
    }
}
