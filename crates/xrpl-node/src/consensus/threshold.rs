//! Threshold convergence — dynamic voting thresholds per round.
//!
//! During Establish, validators adjust their proposed transaction sets.
//! The inclusion threshold increases with each round.

use std::collections::{HashMap, HashSet};

use xrpl_core::types::Hash256;

use super::unl::UNL;

/// Get the inclusion threshold for a given round.
///
/// Round 1: 50%, Round 2: 60%, Round 3: 70%, Round 4-8: 80%, Round 9+: 95%
pub fn threshold_for_round(round: u32) -> f64 {
    match round {
        1 => 0.50,
        2 => 0.60,
        3 => 0.70,
        4..=8 => 0.80,
        _ => 0.95,
    }
}

/// Update our position based on peer proposals and the current threshold.
///
/// For each disputed transaction, include it if the percentage of trusted
/// validators that include it >= threshold, exclude otherwise.
///
/// Returns true if our set changed.
pub fn update_position(
    our_set: &mut HashSet<Hash256>,
    peer_proposals: &HashMap<String, HashSet<Hash256>>,
    unl: &UNL,
    round: u32,
) -> bool {
    let threshold = threshold_for_round(round);
    let trusted_count = unl.trusted_count();
    if trusted_count == 0 {
        return false;
    }

    // Collect all unique tx hashes across all trusted proposals
    let mut all_txs: HashSet<Hash256> = HashSet::new();
    for (key, set) in peer_proposals {
        if unl.is_trusted(key) {
            all_txs.extend(set);
        }
    }
    all_txs.extend(our_set.iter());

    let mut changed = false;

    for tx in &all_txs {
        // Count trusted validators that include this tx
        let support = peer_proposals
            .iter()
            .filter(|(key, set)| unl.is_trusted(key) && set.contains(tx))
            .count();

        let pct = support as f64 / trusted_count as f64;
        let should_include = pct >= threshold;

        if should_include && !our_set.contains(tx) {
            our_set.insert(*tx);
            changed = true;
        } else if !should_include && our_set.contains(tx) {
            our_set.remove(tx);
            changed = true;
        }
    }

    changed
}

/// Check if consensus is reached: >=80% of trusted validators have
/// the same set hash as ours.
pub fn is_consensus_reached(
    our_set: &HashSet<Hash256>,
    peer_proposals: &HashMap<String, HashSet<Hash256>>,
    unl: &UNL,
) -> bool {
    let trusted_count = unl.trusted_count();
    if trusted_count == 0 {
        return false;
    }

    let our_hash = compute_set_hash(our_set);

    let agree = peer_proposals
        .iter()
        .filter(|(key, set)| unl.is_trusted(key) && compute_set_hash(set) == our_hash)
        .count();

    let pct = agree as f64 / trusted_count as f64;
    pct >= 0.80
}

// Re-export from proposal module to avoid duplication.
pub use super::proposal::compute_tx_set_hash as compute_set_hash;

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash256 {
        Hash256([b; 32])
    }

    #[test]
    fn thresholds() {
        assert_eq!(threshold_for_round(1), 0.50);
        assert_eq!(threshold_for_round(2), 0.60);
        assert_eq!(threshold_for_round(3), 0.70);
        assert_eq!(threshold_for_round(5), 0.80);
        assert_eq!(threshold_for_round(10), 0.95);
    }

    #[test]
    fn update_includes_majority() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into(), "V4".into()]);
        let mut our_set = HashSet::new();

        // 3 of 4 validators include tx AA → 75% → above round 1 threshold (50%)
        let mut peers = HashMap::new();
        peers.insert("V1".into(), [h(0xAA)].into());
        peers.insert("V2".into(), [h(0xAA)].into());
        peers.insert("V3".into(), [h(0xAA)].into());
        peers.insert("V4".into(), HashSet::new());

        let changed = update_position(&mut our_set, &peers, &unl, 1);
        assert!(changed);
        assert!(our_set.contains(&h(0xAA)));
    }

    #[test]
    fn update_excludes_minority() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into(), "V4".into()]);
        let mut our_set = HashSet::from([h(0xBB)]);

        // Only 1 of 4 includes BB → 25% → below round 3 threshold (70%)
        let mut peers = HashMap::new();
        peers.insert("V1".into(), [h(0xBB)].into());
        peers.insert("V2".into(), HashSet::new());
        peers.insert("V3".into(), HashSet::new());
        peers.insert("V4".into(), HashSet::new());

        let changed = update_position(&mut our_set, &peers, &unl, 3);
        assert!(changed);
        assert!(!our_set.contains(&h(0xBB)));
    }

    #[test]
    fn consensus_reached_when_supermajority() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into(), "V4".into(), "V5".into()]);
        let our_set: HashSet<Hash256> = [h(0xAA), h(0xBB)].into();

        // 4 of 5 agree (80%)
        let mut peers = HashMap::new();
        peers.insert("V1".into(), our_set.clone());
        peers.insert("V2".into(), our_set.clone());
        peers.insert("V3".into(), our_set.clone());
        peers.insert("V4".into(), our_set.clone());
        peers.insert("V5".into(), [h(0xCC)].into()); // disagrees

        assert!(is_consensus_reached(&our_set, &peers, &unl));
    }

    #[test]
    fn no_consensus_without_supermajority() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into(), "V3".into(), "V4".into(), "V5".into()]);
        let our_set: HashSet<Hash256> = [h(0xAA)].into();

        // Only 2 of 5 agree (40%)
        let mut peers = HashMap::new();
        peers.insert("V1".into(), our_set.clone());
        peers.insert("V2".into(), our_set.clone());
        peers.insert("V3".into(), [h(0xBB)].into());
        peers.insert("V4".into(), [h(0xCC)].into());
        peers.insert("V5".into(), [h(0xDD)].into());

        assert!(!is_consensus_reached(&our_set, &peers, &unl));
    }

    #[test]
    fn set_hash_deterministic() {
        let s1: HashSet<Hash256> = [h(0xAA), h(0xBB)].into();
        let s2: HashSet<Hash256> = [h(0xBB), h(0xAA)].into(); // same set, different order
        assert_eq!(compute_set_hash(&s1), compute_set_hash(&s2));
    }

    #[test]
    fn set_hash_differs() {
        let s1: HashSet<Hash256> = [h(0xAA)].into();
        let s2: HashSet<Hash256> = [h(0xBB)].into();
        assert_ne!(compute_set_hash(&s1), compute_set_hash(&s2));
    }

    #[test]
    fn ignores_untrusted_proposals() {
        let unl = UNL::from_keys(&["V1".into(), "V2".into()]);
        let mut our_set = HashSet::new();

        let mut peers = HashMap::new();
        peers.insert("V1".into(), [h(0xAA)].into());
        peers.insert("UNTRUSTED".into(), [h(0xAA)].into());
        // Only 1 of 2 trusted includes AA → 50% → passes round 1

        let changed = update_position(&mut our_set, &peers, &unl, 1);
        assert!(changed);
        assert!(our_set.contains(&h(0xAA)));
    }
}
