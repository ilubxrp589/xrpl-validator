//! Consensus engine — RPCA (Ripple Protocol Consensus Algorithm).
//!
//! Implements the three-phase consensus process:
//! - **Open**: accumulate transactions from the mempool
//! - **Establish**: exchange proposals with trusted validators, converge
//! - **Accept**: apply the agreed transaction set, produce new ledger

pub mod amendment_vote;
pub mod close_time;
pub mod fee_vote;
pub mod proposal;
pub mod state;
pub mod threshold;
pub mod unl;
pub mod validation;

pub use state::{ConsensusState, Phase};
pub use unl::UNL;
