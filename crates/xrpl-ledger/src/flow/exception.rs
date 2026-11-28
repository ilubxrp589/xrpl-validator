//! A C++ exception inside rippled's flow, and the one transaction rule the
//! Quality arithmetic reads from the thread.
//!
//! rippled's steps signal a failed strand with `FlowException` (caught per
//! strand, StrandFlow.h; this port's `FlowError`). Anything else they throw —
//! an STAmount out of range, an overflow in Quality's `ceil_out` / `ceil_in` —
//! unwinds out of `flow()` altogether: RippleCalc turns it into tecINTERNAL
//! without applying the flow's view (3.4.1 RippleCalc.cpp:95-103), and so does
//! OfferCreate's crossing (OfferCreate.cpp:577-581). Here the throwing site
//! calls [`throw`] and carries on with a placeholder; the strand loop stops at
//! the next strand boundary, and [`payment_flow::flow`] answers tecINTERNAL
//! with nothing applied and no offers removed.
//!
//! The rule is fixCleanup3_5_0, which 3.5.0-rc1's `tryMulRound` reads with
//! `isFeatureEnabled(fixCleanup3_5_0, /*resultIfNoRules*/ true)`
//! (Quality.cpp:110): the ledger's, while a flow runs; true with no rules.
//!
//! [`payment_flow::flow`]: super::payment_flow::flow
use std::cell::Cell;

thread_local! {
    static THROWN: Cell<bool> = const { Cell::new(false) };
    static FIX_CLEANUP_3_5_0: Cell<Option<bool>> = const { Cell::new(None) };
}

/// The flow threw: rippled has left `flow()`.
pub fn throw() {
    THROWN.with(|t| t.set(true));
}

/// Whether the running flow has thrown.
pub fn thrown() -> bool {
    THROWN.with(|t| t.get())
}

/// `isFeatureEnabled(fixCleanup3_5_0, /*resultIfNoRules*/ true)`.
pub fn fix_cleanup_3_5_0() -> bool {
    FIX_CLEANUP_3_5_0.with(|f| f.get()).unwrap_or(true)
}

/// Held for one `flow()`: installs the ledger's fixCleanup3_5_0 and a clean
/// exception flag, and restores both when dropped.
#[must_use = "the flow's state is restored when the guard is dropped"]
pub struct FlowGuard {
    thrown: bool,
    fix: Option<bool>,
}

impl FlowGuard {
    pub fn enter(fix_cleanup_3_5_0: bool) -> FlowGuard {
        FlowGuard {
            thrown: THROWN.with(|t| t.replace(false)),
            fix: FIX_CLEANUP_3_5_0.with(|f| f.replace(Some(fix_cleanup_3_5_0))),
        }
    }
}

impl Drop for FlowGuard {
    fn drop(&mut self) {
        THROWN.with(|t| t.set(self.thrown));
        FIX_CLEANUP_3_5_0.with(|f| f.set(self.fix));
    }
}
