//! Transaction type implementations.
//!
//! Each transaction type gets its own module implementing the Transactor trait.

pub mod account;
pub mod amm;
pub mod amm_swap;
pub mod arith_probe;
pub mod batch;
pub mod number;
pub mod check;
pub mod credential;
pub mod direct_step;
pub mod dispatch;
pub mod escrow;
pub mod misc;
pub mod mpt;
pub mod nftoken;
pub mod offer;
pub mod oracle;
pub mod pay_channel;
pub mod payment;
pub mod pseudo;
pub mod ticket;
pub mod trust_set;
pub mod xchain;

/// Clear every per-transaction thread-local the transactors keep — the
/// offer walk's remembered owner counts, soft-stale and dead-reap marks,
/// passthrough and self-maker credits, the pool walk's cells and context,
/// Batch's inner collection. rippled carries nothing between transactions;
/// a discarded application (a differential-fuzz mutant, a dry run) would
/// otherwise leave the next one its memory. Fuzz #85 off 107009438: 21 dust
/// offers on one 666/XRP page, funded on the ledger, reaped by a mutant
/// that inherited the previous application's remembered owner counts — a
/// walk the same bundle replays clean. Call at the OUTER entry only; Batch
/// inners run nested and rely on the collection.
pub fn reset_thread_state() {
    offer::thread_state_reset();
    amm_swap::thread_state_reset();
    batch::thread_state_reset();
}
