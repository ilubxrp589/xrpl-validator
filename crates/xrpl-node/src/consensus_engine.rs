//! Consensus engine — ties RPCA state machine to the live validator.
//!
//! Runs the Open → Establish → Accept cycle:
//! - Open: collect transactions into a candidate set
//! - Establish: exchange proposals with peers, converge on agreement
//! - Accept: apply agreed tx set, produce new ledger, sign validation

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing;
use xrpl_core::types::Hash256;

use crate::consensus::proposal::compute_tx_set_hash;
use crate::consensus::state::{ConsensusState, Phase};
use crate::consensus::threshold;
pub use crate::consensus::unl::UNL;

/// Mempool: transactions waiting to be proposed.
#[derive(Default)]
pub struct Mempool {
    /// Pending tx hashes → raw transaction bytes.
    pub txs: HashMap<Hash256, Vec<u8>>,
}

impl Mempool {
    /// Add a transaction to the mempool. Returns true if new.
    pub fn add(&mut self, tx_hash: Hash256, raw: Vec<u8>) -> bool {
        if self.txs.contains_key(&tx_hash) {
            return false;
        }
        self.txs.insert(tx_hash, raw);
        true
    }

    /// Get the set of tx hashes currently in the mempool.
    pub fn tx_set(&self) -> HashSet<Hash256> {
        self.txs.keys().copied().collect()
    }

    /// Remove transactions that made it into the consensus set.
    pub fn remove_confirmed(&mut self, confirmed: &HashSet<Hash256>) {
        self.txs.retain(|h, _| !confirmed.contains(h));
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn clear(&mut self) {
        self.txs.clear();
    }
}

/// The live consensus engine.
pub struct ConsensusEngine {
    /// RPCA state machine.
    pub state: ConsensusState,
    /// Transaction mempool.
    pub mempool: Mempool,
    /// Our UNL (trusted validators).
    pub unl: UNL,
    /// Current ledger sequence.
    pub ledger_seq: u32,
    /// Number of convergence rounds this establish phase.
    pub establish_rounds: u32,
    /// Whether we've sent our initial proposal this round.
    pub proposal_sent: bool,
    /// The agreed-upon tx set after consensus.
    pub agreed_set: HashSet<Hash256>,
}

impl ConsensusEngine {
    pub fn new() -> Self {
        Self {
            state: ConsensusState::new(Hash256([0u8; 32])),
            mempool: Mempool::default(),
            unl: UNL::empty(),
            ledger_seq: 0,
            establish_rounds: 0,
            proposal_sent: false,
            agreed_set: HashSet::new(),
        }
    }

    /// Called when we receive a new transaction from a peer.
    /// Adds to mempool if in Open phase.
    pub fn on_transaction(&mut self, tx_hash: Hash256, raw_tx: Vec<u8>) {
        if matches!(self.state.phase, Phase::Open) {
            self.mempool.add(tx_hash, raw_tx);
        }
    }

    /// Called when we see a CLOSING event from the network.
    /// Transitions from Open → Establish and builds our initial proposal.
    pub fn on_close_trigger(&mut self, ledger_seq: u32) -> Option<HashSet<Hash256>> {
        if !matches!(self.state.phase, Phase::Open) {
            return None;
        }

        self.ledger_seq = ledger_seq;
        self.state.our_set = self.mempool.tx_set();
        self.state.begin_establish();
        self.establish_rounds = 0;
        self.proposal_sent = false;

        eprintln!(
            "[consensus] CLOSE triggered for ledger #{} — {} txs in candidate set",
            ledger_seq, self.state.our_set.len()
        );

        Some(self.state.our_set.clone())
    }

    /// Called when we receive a TMProposeSet from a peer.
    ///
    /// SECURITY(7.4): The proposal's `signature` field (from TmProposeSet) is NOT verified
    /// against the `validator_key`. An attacker could forge proposals to influence consensus.
    /// Proper verification requires deserializing the proposal, hashing the signed fields,
    /// and verifying the signature against the validator's known public key.
    pub fn on_peer_proposal(&mut self, validator_key: String, tx_hashes: HashSet<Hash256>) {
        tracing::debug!(
            validator = %validator_key,
            txs = tx_hashes.len(),
            "accepting UNVERIFIED peer proposal — signature not checked"
        );
        self.state.add_peer_proposal(validator_key, tx_hashes);
    }

    /// Run one convergence round. Returns true if our position changed.
    pub fn converge(&mut self) -> bool {
        if !matches!(self.state.phase, Phase::Establish { .. }) {
            return false;
        }

        self.establish_rounds += 1;
        let changed = self.state.converge_round(&self.unl);

        if changed {
            eprintln!(
                "[consensus] Round {} — position changed, {} txs",
                self.establish_rounds, self.state.our_set.len()
            );
        }

        // Check if consensus is reached
        if self.state.is_consensus_reached(&self.unl) {
            self.agreed_set = self.state.our_set.clone();
            self.state.accept();
            eprintln!(
                "[consensus] CONSENSUS REACHED for ledger #{} — {} txs agreed",
                self.ledger_seq, self.agreed_set.len()
            );
        }

        changed
    }

    /// Check if we're in Accept phase (consensus reached).
    pub fn is_accepted(&self) -> bool {
        matches!(self.state.phase, Phase::Accept)
    }

    /// Get the agreed tx set hash.
    pub fn agreed_set_hash(&self) -> Hash256 {
        compute_tx_set_hash(&self.agreed_set)
    }

    /// Reset for the next ledger round.
    pub fn next_round(&mut self, new_ledger_hash: Hash256) {
        // Remove confirmed txs from mempool
        self.mempool.remove_confirmed(&self.agreed_set);
        self.agreed_set.clear();
        self.state.reset(new_ledger_hash);
        self.establish_rounds = 0;
        self.proposal_sent = false;
    }

    /// Get the current phase.
    pub fn phase(&self) -> &Phase {
        &self.state.phase
    }
}

/// Status for the API.
#[derive(Clone, Default, serde::Serialize)]
pub struct ConsensusStatus {
    pub phase: String,
    pub ledger_seq: u32,
    pub mempool_size: usize,
    pub candidate_set_size: usize,
    pub peer_proposals: usize,
    pub establish_rounds: u32,
    pub unl_size: usize,
}

impl ConsensusEngine {
    pub fn status(&self) -> ConsensusStatus {
        ConsensusStatus {
            phase: match self.state.phase {
                Phase::Open => "open".to_string(),
                Phase::Establish { round } => format!("establish(round={})", round),
                Phase::Accept => "accept".to_string(),
            },
            ledger_seq: self.ledger_seq,
            mempool_size: self.mempool.len(),
            candidate_set_size: self.state.our_set.len(),
            peer_proposals: self.state.peer_proposals.len(),
            establish_rounds: self.establish_rounds,
            unl_size: self.unl.trusted_count(),
        }
    }
}
