//! Proposal generation and exchange via TMProposeSet.

use std::collections::HashSet;

use xrpl_core::crypto::signing::sha512_half;
use xrpl_core::types::Hash256;

/// Compute the hash of a transaction set for a proposal.
///
/// Sort tx hashes lexicographically, concatenate, SHA512-half.
pub fn compute_tx_set_hash(txs: &HashSet<Hash256>) -> Hash256 {
    if txs.is_empty() {
        return Hash256([0; 32]);
    }

    let mut sorted: Vec<&Hash256> = txs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut data = Vec::with_capacity(sorted.len() * 32);
    for h in sorted {
        data.extend_from_slice(&h.0);
    }

    Hash256(sha512_half(&data))
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
