//! VALAUDIT lockstep (M1 — shadow harness).
//!
//! Pure evaluation of whether our independently-applied ledger reproduces the
//! network's `ledger_hash`. Used by the env-gated shadow harness in `ws_sync`.
use crate::state_hash::compute_ledger_hash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockstepOutcome {
    pub our_ledger_hash: [u8; 32],
    pub matched: bool,
}

/// Recompute the ledger_hash from header fields + our account_hash and compare
/// to the network-reported ledger_hash.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_ledger(
    ledger_seq: u32,
    total_drops: u64,
    parent_hash: &[u8; 32],
    tx_hash: &[u8; 32],
    account_hash: &[u8; 32],
    parent_close_time: u32,
    close_time: u32,
    close_resolution: u8,
    close_flags: u8,
    network_ledger_hash: &[u8; 32],
) -> LockstepOutcome {
    let our = compute_ledger_hash(
        ledger_seq, total_drops, parent_hash, tx_hash, account_hash,
        parent_close_time, close_time, close_resolution, close_flags,
    );
    LockstepOutcome { our_ledger_hash: our.0, matched: our.0 == *network_ledger_hash }
}
