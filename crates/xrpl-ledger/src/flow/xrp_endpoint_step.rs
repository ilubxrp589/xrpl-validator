//! rippled `XRPEndpointStep` (`XRPEndpointStep.cpp`): the strand's first
//! step when the source sends XRP, or its last when the destination
//! receives it. The source step is limited by `xrpLiquid` (a crossing's
//! first step may reduce the reserve by one when the taker has no line for
//! the asset it will receive); the destination step is unlimited.
use super::amounts::EitherAmount;
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::{Quality, QualityFunction};
use super::steps::{Asset, DebtDirection, FlowError, OfferCrossing, OffersToRemove, Step, StrandContext, StrandDirection};
use super::view::{account_send, check_freeze, xrp_liquid, StepCheckError};
use crate::ledger::keylet;
use crate::tx::offer::json_at;

pub struct XrpEndpointStep {
    acc: [u8; 20],
    is_last: bool,
    /// `reserveReduction_` (crossing only): −1 when the taker holds no
    /// line for the strand's deliver asset (the crossing may create it).
    reserve_reduction: i64,
    cache: Option<i128>,
}

impl XrpEndpointStep {
    /// `make_XRPEndpointStep(ctx, acc)` with its `check`.
    pub fn make(ctx: &StrandContext, sb: &PaymentSandbox, acc: [u8; 20]) -> Result<XrpEndpointStep, StepCheckError> {
        let reserve_reduction = if ctx.offer_crossing != OfferCrossing::No {
            let deliver: &Asset = &ctx.strand_deliver;
            let has_line = deliver
                .issuer
                .map(|iss| json_at(sb.sandbox(), &keylet::ripple_state_key(&acc, &iss, &deliver.currency)).is_some())
                .unwrap_or(true);
            if ctx.is_first && !has_line { -1 } else { 0 }
        } else {
            0
        };
        let step = XrpEndpointStep { acc, is_last: ctx.is_last, reserve_reduction, cache: None };
        step.check(ctx, sb)?;
        Ok(step)
    }

    pub fn acc(&self) -> [u8; 20] {
        self.acc
    }

    fn liquid(&self, sb: &PaymentSandbox) -> i128 {
        xrp_liquid(sb, &self.acc, self.reserve_reduction)
    }

    fn pass(&mut self, sb: &mut PaymentSandbox, amount: i128) -> Result<(i128, i128), FlowError> {
        let balance = self.liquid(sb);
        let result = if self.is_last { amount } else { balance.min(amount) };
        let zero = [0u8; 20];
        let (sender, receiver) = if self.is_last { (zero, self.acc) } else { (self.acc, zero) };
        if account_send(sb, &sender, &receiver, &Asset::XRP, EitherAmount::Xrp(result)).is_err() {
            return Ok((0, 0));
        }
        self.cache = Some(result);
        Ok((result, result))
    }

    /// `check(ctx)`: the account exists, the step sits at an end of the
    /// strand, and the XRP "line" is not frozen (a no-op for XRP).
    pub fn check(&self, ctx: &StrandContext, sb: &PaymentSandbox) -> Result<(), StepCheckError> {
        if self.acc == [0u8; 20] {
            return Err(StepCheckError::BadPath);
        }
        if json_at(sb.sandbox(), &keylet::account_root_key(&self.acc)).is_none() {
            return Err(StepCheckError::NoAccount);
        }
        if !ctx.is_first && !ctx.is_last {
            return Err(StepCheckError::BadPath);
        }
        let zero = [0u8; 20];
        let (src, dst) = if self.is_last { (zero, self.acc) } else { (self.acc, zero) };
        check_freeze(sb.sandbox(), &src, &dst, &[0u8; 20])?;
        Ok(())
    }
}

impl Step for XrpEndpointStep {
    fn rev(&mut self, sb: &mut PaymentSandbox, _ofrs_to_rm: &mut OffersToRemove, out: &EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        let (i, o) = self.pass(sb, out.xrp())?;
        Ok((EitherAmount::Xrp(i), EitherAmount::Xrp(o)))
    }

    fn fwd(&mut self, sb: &mut PaymentSandbox, _ofrs_to_rm: &mut OffersToRemove, input: &EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        let (i, o) = self.pass(sb, input.xrp())?;
        Ok((EitherAmount::Xrp(i), EitherAmount::Xrp(o)))
    }

    fn cached_in(&self) -> Option<EitherAmount> {
        self.cache.map(EitherAmount::Xrp)
    }
    fn cached_out(&self) -> Option<EitherAmount> {
        self.cache.map(EitherAmount::Xrp)
    }

    fn direct_step_accts(&self) -> Option<([u8; 20], [u8; 20])> {
        let zero = [0u8; 20];
        Some(if self.is_last { (zero, self.acc) } else { (self.acc, zero) })
    }

    fn debt_direction(&self, _sb: &PaymentSandbox, _dir: StrandDirection) -> DebtDirection {
        DebtDirection::Issues
    }

    fn quality_upper_bound(&self, sb: &PaymentSandbox, _prev_step_dir: DebtDirection) -> (Option<Quality>, DebtDirection) {
        let one = keylet::rate_encode_native(1, 0, false, 1, 0, false).map(Quality);
        (one, self.debt_direction(sb, StrandDirection::Forward))
    }

    fn get_quality_func(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (Option<QualityFunction>, DebtDirection) {
        let (q, dir) = self.quality_upper_bound(sb, prev_step_dir);
        (q.and_then(|q| QualityFunction::clob_like(q).ok()), dir)
    }

    fn valid_fwd(&mut self, sb: &mut PaymentSandbox, input: &EitherAmount) -> Result<(bool, EitherAmount), FlowError> {
        if self.cache.is_none() {
            return Ok((false, EitherAmount::Xrp(0)));
        }
        let xrp_in = input.xrp();
        let balance = self.liquid(sb);
        if !self.is_last && balance < xrp_in {
            return Ok((false, EitherAmount::Xrp(balance)));
        }
        Ok((true, *input))
    }

    fn equal(&self, other: &dyn Step) -> bool {
        other.xrp_endpoint_acct() == Some((self.acc, self.is_last))
    }

    fn xrp_endpoint_acct(&self) -> Option<([u8; 20], bool)> {
        Some((self.acc, self.is_last))
    }

    fn log_string(&self) -> String {
        format!("XRPEndpointStep: acc {} last {}", hex::encode(self.acc), self.is_last)
    }
}
