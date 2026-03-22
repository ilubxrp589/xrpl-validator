//! RPCA state machine — Open → Establish → Accept.

use std::collections::{HashMap, HashSet};

use xrpl_core::types::Hash256;

use super::threshold;
use super::unl::UNL;

/// Consensus phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Accumulating transactions from the mempool.
    Open,
    /// Exchanging proposals with peers, converging on agreement.
    Establish { round: u32 },
    /// Agreed — applying transaction set to produce new ledger.
    Accept,
}

/// The consensus engine state for one ledger round.
pub struct ConsensusState {
    pub phase: Phase,
    /// Our proposed transaction set (tx hashes).
    pub our_set: HashSet<Hash256>,
    /// Peer proposals: validator key hex → their tx hash set.
    pub peer_proposals: HashMap<String, HashSet<Hash256>>,
    /// The previous ledger hash we're building on.
    pub previous_ledger: Hash256,
    /// How many times our position has been unchanged.
    pub stable_rounds: u32,
}

impl ConsensusState {
    pub fn new(previous_ledger: Hash256) -> Self {
        Self {
            phase: Phase::Open,
            our_set: HashSet::new(),
            peer_proposals: HashMap::new(),
            previous_ledger,
            stable_rounds: 0,
        }
    }

    /// Add a transaction to our proposed set (Open phase).
    pub fn add_transaction(&mut self, tx_hash: Hash256) {
        self.our_set.insert(tx_hash);
    }

    /// Record a peer's proposal.
    pub fn add_peer_proposal(&mut self, validator_key: String, tx_set: HashSet<Hash256>) {
        self.peer_proposals.insert(validator_key, tx_set);
    }

    /// Transition from Open → Establish.
    pub fn begin_establish(&mut self) {
        self.phase = Phase::Establish { round: 1 };
        self.stable_rounds = 0;
    }

    /// Run one round of threshold convergence.
    /// Returns true if our position changed.
    pub fn converge_round(&mut self, unl: &UNL) -> bool {
        let round = match self.phase {
            Phase::Establish { round } => round,
            _ => return false,
        };

        let changed = threshold::update_position(
            &mut self.our_set,
            &self.peer_proposals,
            unl,
            round,
        );

        if changed {
            self.stable_rounds = 0;
        } else {
            self.stable_rounds += 1;
        }

        // Advance round
        self.phase = Phase::Establish { round: round + 1 };

        changed
    }

    /// Check if consensus has been reached.
    pub fn is_consensus_reached(&self, unl: &UNL) -> bool {
        // Need stable position + supermajority agreement
        self.stable_rounds >= 2
            && threshold::is_consensus_reached(&self.our_set, &self.peer_proposals, unl)
    }

    /// Transition to Accept phase.
    pub fn accept(&mut self) {
        self.phase = Phase::Accept;
    }

    /// Reset for the next ledger round.
    pub fn reset(&mut self, new_previous_ledger: Hash256) {
        self.phase = Phase::Open;
        self.our_set.clear();
        self.peer_proposals.clear();
        self.previous_ledger = new_previous_ledger;
        self.stable_rounds = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash256 {
        Hash256([b; 32])
    }

    #[test]
    fn lifecycle() {
        let mut state = ConsensusState::new(h(0x00));
        assert_eq!(state.phase, Phase::Open);

        state.add_transaction(h(0xAA));
        state.add_transaction(h(0xBB));
        assert_eq!(state.our_set.len(), 2);

        state.begin_establish();
        assert!(matches!(state.phase, Phase::Establish { round: 1 }));

        state.accept();
        assert_eq!(state.phase, Phase::Accept);

        state.reset(h(0x01));
        assert_eq!(state.phase, Phase::Open);
        assert!(state.our_set.is_empty());
    }
}
