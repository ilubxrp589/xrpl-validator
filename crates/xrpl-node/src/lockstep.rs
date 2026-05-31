//! VALAUDIT lockstep (M1 — shadow harness).
//!
//! Pure evaluation of whether our independently-applied ledger reproduces the
//! network's `ledger_hash`. Used by the env-gated shadow harness in `ws_sync`.
use crate::state_hash::compute_ledger_hash;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

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

/// Rolling window (last 1000) of lockstep match results, for the
/// `xrpl_lockstep_shadow_match_ratio` gauge.
#[derive(Clone, Default)]
pub struct LockstepMeter {
    window: Arc<Mutex<VecDeque<bool>>>,
}

impl LockstepMeter {
    pub fn new() -> Self {
        Self { window: Arc::new(Mutex::new(VecDeque::with_capacity(1000))) }
    }
    pub fn record(&self, matched: bool) {
        let mut w = self.window.lock();
        if w.len() >= 1000 { w.pop_front(); }
        w.push_back(matched);
    }
    /// Fraction matched over the window; 1.0 when empty (no divergence yet).
    pub fn match_ratio(&self) -> f64 {
        let w = self.window.lock();
        if w.is_empty() { return 1.0; }
        w.iter().filter(|&&m| m).count() as f64 / w.len() as f64
    }
    pub fn observed(&self) -> usize { self.window.lock().len() }
}
