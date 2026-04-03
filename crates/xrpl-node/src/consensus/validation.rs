//! Validation messages — sign after ledger close, verify from peers.

use std::collections::HashMap;

use xrpl_core::types::Hash256;

use super::unl::UNL;

/// A validation vote for a specific ledger.
#[derive(Debug, Clone)]
pub struct Validation {
    pub ledger_hash: Hash256,
    pub ledger_sequence: u32,
    pub close_time: u32,
    pub signing_time: u32,
    pub node_pubkey: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Tracks validations per ledger to determine "fully validated" status.
pub struct ValidationCollector {
    /// Ledger hash → set of validator keys that validated it.
    per_ledger: HashMap<Hash256, Vec<String>>,
}

impl ValidationCollector {
    pub fn new() -> Self {
        Self {
            per_ledger: HashMap::new(),
        }
    }

    /// Record a validation for a ledger.
    /// SECURITY(4.6): Auto-prunes when per_ledger exceeds 1000 entries.
    pub fn track(&mut self, ledger_hash: Hash256, validator_key_hex: String) {
        self.per_ledger
            .entry(ledger_hash)
            .or_default()
            .push(validator_key_hex);

        // Auto-prune: keep at most 1000 ledger entries
        if self.per_ledger.len() > 1000 {
            self.prune(500);
        }
    }

    /// Check if a ledger is fully validated (>80% of UNL).
    pub fn is_fully_validated(&self, ledger_hash: &Hash256, unl: &UNL) -> bool {
        let trusted_count = unl.trusted_count();
        if trusted_count == 0 {
            return false;
        }

        let validators = match self.per_ledger.get(ledger_hash) {
            Some(v) => v,
            None => return false,
        };

        let trusted_validations = validators
            .iter()
            .filter(|key| unl.is_trusted(key))
            .count();

        let pct = trusted_validations as f64 / trusted_count as f64;
        pct >= 0.80
    }

    /// Count validations for a ledger.
    pub fn validation_count(&self, ledger_hash: &Hash256) -> usize {
        self.per_ledger.get(ledger_hash).map_or(0, |v| v.len())
    }

    /// Clean up old ledger entries (keep only recent N).
    /// Since HashMap has no ordering, we sort by validation count
    /// (ascending) and remove the least-validated first.
    pub fn prune(&mut self, keep: usize) {
        if self.per_ledger.len() <= keep {
            return;
        }
        let excess = self.per_ledger.len() - keep;
        let mut entries: Vec<(Hash256, usize)> = self
            .per_ledger
            .iter()
            .map(|(&h, v)| (h, v.len()))
            .collect();
        entries.sort_by_key(|(_, count)| *count);
        for (hash, _) in entries.into_iter().take(excess) {
            self.per_ledger.remove(&hash);
        }
    }
}

impl Default for ValidationCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash256 {
        Hash256([b; 32])
    }

    #[test]
    fn track_and_count() {
        let mut vc = ValidationCollector::new();
        vc.track(h(0xAA), "V1".into());
        vc.track(h(0xAA), "V2".into());
        vc.track(h(0xBB), "V1".into());

        assert_eq!(vc.validation_count(&h(0xAA)), 2);
        assert_eq!(vc.validation_count(&h(0xBB)), 1);
        assert_eq!(vc.validation_count(&h(0xCC)), 0);
    }

    #[test]
    fn fully_validated() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into(), "V4".into(), "V5".into()]);
        let mut vc = ValidationCollector::new();

        // 5 of 5 validate (100%)
        for i in 1..=5 {
            vc.track(h(0xAA), format!("V{i}"));
        }
        assert!(vc.is_fully_validated(&h(0xAA), &unl));
    }

    #[test]
    fn not_fully_validated() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into(), "V4".into(), "V5".into()]);
        let mut vc = ValidationCollector::new();

        // 2 of 5 validate (40%)
        vc.track(h(0xBB), "V1".into());
        vc.track(h(0xBB), "V2".into());
        assert!(!vc.is_fully_validated(&h(0xBB), &unl));
    }

    #[test]
    fn prune() {
        let mut vc = ValidationCollector::new();
        for i in 0..20u8 {
            vc.track(h(i), "V1".into());
        }
        assert_eq!(vc.per_ledger.len(), 20);
        vc.prune(10);
        assert_eq!(vc.per_ledger.len(), 10);
    }
}
