//! Consensus engine — RPCA (Ripple Protocol Consensus Algorithm).
//!
//! Implements the three-phase consensus process:
//! - **Open**: accumulate transactions from the mempool
//! - **Establish**: exchange proposals with trusted validators, converge
//! - **Accept**: apply the agreed transaction set, produce new ledger
