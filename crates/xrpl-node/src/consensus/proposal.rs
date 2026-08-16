//! Proposal generation and exchange via TMProposeSet.

use std::collections::HashSet;

use xrpl_core::types::Hash256;

use xrpl_ledger::shamap::tx_set::compute_tx_set_root;

/// The hash a proposal commits its transaction set to.
///
/// This is the root of the `tnTRANSACTION_NM` SHAMap — see
/// [`xrpl_ledger::shamap::tx_set`] for the leaf rule and its source.
///
/// ⚠ It was a PLACEHOLDER until 2026-08-16: sort the ids, concatenate, hash
/// once. That is not a SHAMap and shares none of its structure, so every
/// proposal we generated named a set hash no peer could agree with — harmless
/// only because nothing consumed it yet. The three properties its tests
/// asserted (empty is zero, order-independent, membership matters) are all
/// TRUE of the real function too, which is exactly why passing tests did not
/// reveal that the value was wrong. Properties constrain; they do not identify.
pub fn compute_tx_set_hash(txs: &HashSet<Hash256>) -> Hash256 {
    let ids: Vec<Hash256> = txs.iter().copied().collect();
    compute_tx_set_root(&ids)
}

/// A consensus proposal.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub tx_set_hash: Hash256,
    pub close_time: u32,
    pub ledger_sequence: u32,
    pub previous_ledger: Hash256,
    pub propose_seq: u32,
    pub node_pubkey: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash256 {
        Hash256([b; 32])
    }

    #[test]
    fn empty_set_hash() {
        let set: HashSet<Hash256> = HashSet::new();
        assert_eq!(compute_tx_set_hash(&set), Hash256([0; 32]));
    }

    #[test]
    fn deterministic_hash() {
        let s1: HashSet<Hash256> = [h(0xAA), h(0xBB), h(0xCC)].into();
        let s2: HashSet<Hash256> = [h(0xCC), h(0xAA), h(0xBB)].into();
        assert_eq!(compute_tx_set_hash(&s1), compute_tx_set_hash(&s2));
    }

    #[test]
    fn different_sets_different_hash() {
        let s1: HashSet<Hash256> = [h(0xAA)].into();
        let s2: HashSet<Hash256> = [h(0xBB)].into();
        assert_ne!(compute_tx_set_hash(&s1), compute_tx_set_hash(&s2));
    }
}
