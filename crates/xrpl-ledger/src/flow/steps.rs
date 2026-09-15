//! rippled `Step` (`include/xrpl/tx/paths/detail/Steps.h`): the interface
//! every strand element implements, and the context a strand is built in.
//!
//! A strand is a vector of steps; `StrandFlow::flow` runs them in REVERSE
//! (last to first, sizing each step's input from the output the next step
//! asked for) and then FORWARD (first to last, delivering what the sender
//! can actually fund), with each step caching what it computed so the
//! forward pass can be checked against the reverse one (`validFwd`).
//!
//! Slice 1 ports the interface and the shared helpers; the step
//! implementations (`DirectStepI`, `XRPEndpointStep`, `BookStep`) follow in
//! slices 2–4 as `Box<dyn Step>` values built by `pay_steps::to_strand`.
use std::collections::BTreeSet;

use xrpl_core::types::Hash256;

use super::amounts::{EitherAmount, IouAmount};
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::{Quality, QualityFunction};
use crate::ledger::transactor::TxResult;

/// `DebtDirection`: whether a step's source issues (creates) or redeems
/// (returns) the currency it sends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DebtDirection {
    Issues,
    Redeems,
}

pub fn redeems(dir: DebtDirection) -> bool {
    dir == DebtDirection::Redeems
}

pub fn issues(dir: DebtDirection) -> bool {
    dir == DebtDirection::Issues
}

/// `StrandDirection`: which pass is asking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrandDirection {
    Forward,
    Reverse,
}

/// `OfferCrossing`: a payment, an OfferCreate crossing, or a tfSell crossing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OfferCrossing {
    No,
    Yes,
    Sell,
}

/// `Book`: an order book's (in, out) assets. XRP is `issuer == None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Asset {
    pub currency: [u8; 20],
    /// None for XRP.
    pub issuer: Option<[u8; 20]>,
}

impl Asset {
    pub const XRP: Asset = Asset { currency: [0; 20], issuer: None };
    pub fn is_xrp(&self) -> bool {
        self.issuer.is_none()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Book {
    pub input: Asset,
    pub output: Asset,
    /// Permissioned-DEX domain, when the book is domain-scoped.
    pub domain: Option<Hash256>,
}

/// `FlowException`: a step aborts the whole flow with a result code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FlowError(pub TxResult);

/// The set rippled threads through every pass: offers found dead by the
/// stream (`permRmOffer`) that are removed from the base view after the
/// flow whether or not the strand succeeded (StrandFlow.h "rm bad offers
/// even if the strand fails").
pub type OffersToRemove = BTreeSet<[u8; 32]>;

/// rippled `Step`. Amounts cross the boundary as `EitherAmount`; each
/// implementation works in its own (TIn, TOut) types (`StepImp`).
pub trait Step {
    /// `rev`: given the output the next step wants, consume what this step
    /// can provide and return (in, out) — out never above the ask.
    fn rev(
        &mut self,
        sb: &mut PaymentSandbox,
        ofrs_to_rm: &mut OffersToRemove,
        out: &EitherAmount,
    ) -> Result<(EitherAmount, EitherAmount), FlowError>;

    /// `fwd`: given the input the previous step delivers, consume it and
    /// return (in, out) — in never above the offer.
    fn fwd(
        &mut self,
        sb: &mut PaymentSandbox,
        ofrs_to_rm: &mut OffersToRemove,
        input: &EitherAmount,
    ) -> Result<(EitherAmount, EitherAmount), FlowError>;

    /// The amounts the last pass cached, if any.
    fn cached_in(&self) -> Option<EitherAmount>;
    fn cached_out(&self) -> Option<EitherAmount>;

    /// A DirectStep's source account (None for other steps).
    fn direct_step_src_acct(&self) -> Option<[u8; 20]> {
        None
    }

    /// A DirectStep's (src, dst) (None for other steps).
    fn direct_step_accts(&self) -> Option<([u8; 20], [u8; 20])> {
        None
    }

    /// `debtDirection(view, dir)`: whether this step's source issues or
    /// redeems, as seen by the pass `dir`.
    fn debt_direction(&self, sb: &PaymentSandbox, dir: StrandDirection) -> DebtDirection;

    /// `lineQualityIn`: the QualityIn on a DirectStep's destination line
    /// (QUALITY_ONE for other steps).
    fn line_quality_in(&self, _sb: &PaymentSandbox) -> u32 {
        1_000_000_000
    }

    /// `qualityUpperBound(view, prevStepDir)`: the best quality this step
    /// can offer and the debt direction it leaves; None when the step is
    /// dry.
    fn quality_upper_bound(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (Option<Quality>, DebtDirection);

    /// `getQualityFunc`: the constant function of the upper bound unless a
    /// step (an AMM book) overrides with a sloped one.
    fn get_quality_func(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (Option<QualityFunction>, DebtDirection) {
        let (q, dir) = self.quality_upper_bound(sb, prev_step_dir);
        (q.and_then(|q| QualityFunction::clob_like(q).ok()), dir)
    }

    /// Offers this step consumed in the last pass.
    fn offers_used(&self) -> u32 {
        0
    }

    /// A BookStep's book (None for other steps).
    fn book_step_book(&self) -> Option<Book> {
        None
    }

    /// `inactive`: the step has nothing more to give (e.g. a book emptied
    /// to its `kMaxOffersToConsume`).
    fn inactive(&self) -> bool {
        false
    }

    /// `validFwd(sb, afView, in)`: re-run the forward pass on a scratch
    /// view and confirm it matches the cached amounts — (ok, out).
    fn valid_fwd(&mut self, sb: &mut PaymentSandbox, input: &EitherAmount) -> Result<(bool, EitherAmount), FlowError>;

    /// `equal`: structural identity, for strand de-duplication.
    fn equal(&self, other: &dyn Step) -> bool;

    /// `logString`: the step's one-line description for narration.
    fn log_string(&self) -> String;
}

/// `Strand`: the steps of one path, source to destination.
pub type Strand = Vec<Box<dyn Step>>;

/// `offersUsed(strand)`.
pub fn offers_used(strand: &Strand) -> u32 {
    strand.iter().map(|s| s.offers_used()).sum()
}

/// `operator==(Strand, Strand)`.
pub fn strands_equal(lhs: &Strand, rhs: &Strand) -> bool {
    lhs.len() == rhs.len() && lhs.iter().zip(rhs).all(|(a, b)| a.equal(b.as_ref()))
}

/// `checkNear(expected, actual)` (Steps.h): exact on XRP; the IOU form
/// below.
pub fn check_near(expected: &EitherAmount, actual: &EitherAmount) -> bool {
    match (expected, actual) {
        (EitherAmount::Xrp(a), EitherAmount::Xrp(b)) => a == b,
        (EitherAmount::Iou(a), EitherAmount::Iou(b)) => check_near_iou(*a, *b),
        _ => false,
    }
}

/// `checkNear(IOUAmount, IOUAmount)` (PaySteps.cpp:34-56): the forward pass
/// may miss the reverse one by a hair. Exponents more than one apart never
/// match; anything under 1e-20 always does; otherwise the mantissa of the
/// smaller-exponent side is dropped a digit to align and the two must agree
/// within 0.1% of the larger (`ratTol = 0.001`, computed in double).
pub fn check_near_iou(expected: IouAmount, actual: IouAmount) -> bool {
    if (expected.exponent - actual.exponent).abs() > 1 {
        return false;
    }
    if actual.exponent < -20 {
        return true;
    }
    let signed = |a: IouAmount| -> i64 { if a.negative { -(a.mantissa as i64) } else { a.mantissa as i64 } };
    let a = if expected.exponent < actual.exponent { signed(expected) / 10 } else { signed(expected) };
    let b = if actual.exponent < expected.exponent { signed(actual) / 10 } else { signed(actual) };
    if a == b {
        return true;
    }
    let diff = (a - b).abs() as f64;
    let r = diff / (a.abs().max(b.abs()) as f64);
    r <= 0.001
}

/// `StrandContext`: everything `to_strand` hands a step's constructor.
#[derive(Clone, Debug)]
pub struct StrandContext {
    pub strand_src: [u8; 20],
    pub strand_dst: [u8; 20],
    pub strand_deliver: Asset,
    pub limit_quality: Option<Quality>,
    pub is_first: bool,
    pub is_last: bool,
    pub owner_pays_transfer_fee: bool,
    pub offer_crossing: OfferCrossing,
    pub is_default_path: bool,
    pub strand_size: usize,
    pub domain: Option<Hash256>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::number::{Number, Rounding};

    fn iou(m: i64, e: i32) -> IouAmount {
        IouAmount::from_number(Number::new(m, e, Rounding::ToNearest).unwrap())
    }

    #[test]
    fn check_near_accepts_a_hair_and_refuses_more() {
        assert!(check_near_iou(iou(1_000_000_000_000_000, -15), iou(1_000_000_000_000_000, -15)));
        // 1 vs 0.9999 (exponents one apart, aligned mantissas within 0.1%).
        assert!(check_near_iou(iou(1_000_000_000_000_000, -15), iou(9_999_000_000_000_000, -16)));
        // 1 vs 0.99 is outside 0.1%.
        assert!(!check_near_iou(iou(1_000_000_000_000_000, -15), iou(9_900_000_000_000_000, -16)));
        // Exponents two apart never match; dust always does.
        assert!(!check_near_iou(iou(1, 0), iou(1, 2)));
        assert!(check_near_iou(iou(1, -25), iou(7, -25)));
        assert!(check_near(&EitherAmount::Xrp(5), &EitherAmount::Xrp(5)));
        assert!(!check_near(&EitherAmount::Xrp(5), &EitherAmount::Xrp(6)));
    }
}
