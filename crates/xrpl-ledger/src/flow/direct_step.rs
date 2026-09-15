//! rippled `DirectStepI` (`src/xrpld/app/paths/detail/DirectStep.cpp`):
//! one trust-line hop between two accounts in one currency, as
//! `DirectIPaymentStep` (line qualities apply, `maxFlow` = what the source
//! can send) or `DirectIOfferCrossingStep` (parity qualities, the last
//! step is non-limiting — "the sell's largest possible amount").
//!
//! `revImp` (505–590): `maxFlow` → `qualities` → `srcToDst = mulRatio(out,
//! QUALITY_ONE, dstQIn, up)`; non-limiting when it fits (`in = mulRatio(
//! srcToDst, srcQOut, QUALITY_ONE, up)`), else the maximum with
//! `actualOut = mulRatio(max, dstQIn, QUALITY_ONE, down)`; `rippleCredit`
//! for `srcToDst` in the issuer the debt direction names.
//! `fwdImp` (633–710): the mirror, `srcToDst = mulRatio(in, QUALITY_ONE,
//! srcQOut, down)`, `setCacheLimiting`.
use super::amounts::{EitherAmount, IouAmount};
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::{Quality, QualityFunction};
use super::steps::{
    check_near_iou, issues, redeems, Asset, DebtDirection, FlowError, OfferCrossing, OffersToRemove, Step,
    StrandContext, StrandDirection,
};
use super::view::{
    account_holds_signed_iou, check_freeze, check_no_ripple, credit_balance, credit_limit, is_authorized_line_flag,
    line_quality, mul_ratio_iou, ripple_credit_pub, transfer_rate, FreezeHandling, QualityDirection, StepCheckError,
    LSF_REQUIRE_AUTH, QUALITY_ONE,
};
use crate::ledger::keylet;
use crate::tx::offer::json_at;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirectKind {
    Payment,
    OfferCrossing,
}

#[derive(Clone, Copy, Debug)]
struct Cache {
    input: IouAmount,
    src_to_dst: IouAmount,
    out: IouAmount,
    src_debt_dir: DebtDirection,
}

pub struct DirectStep {
    kind: DirectKind,
    src: [u8; 20],
    dst: [u8; 20],
    currency: [u8; 20],
    is_last: bool,
    /// What the previous step answers to `lineQualityIn` and
    /// `debtDirection` — captured at construction (see BookStep).
    prev: Option<PrevStep>,
    cache: Option<Cache>,
}

#[derive(Clone, Copy, Debug)]
struct PrevStep {
    line_quality_in: u32,
    debt_dir_reverse: DebtDirection,
    debt_dir_forward: DebtDirection,
    direct_src: Option<[u8; 20]>,
    is_book: bool,
}

impl DirectStep {
    /// `make_DirectStepI(ctx, src, dst, currency)` — the step and its
    /// `check`; `prev` is the strand's previous step, if any.
    pub fn make(ctx: &StrandContext, sb: &PaymentSandbox, prev: Option<&dyn Step>, src: [u8; 20], dst: [u8; 20], currency: [u8; 20]) -> Result<DirectStep, StepCheckError> {
        let kind = if ctx.offer_crossing == OfferCrossing::No { DirectKind::Payment } else { DirectKind::OfferCrossing };
        let prev = prev.map(|p| PrevStep {
            line_quality_in: p.line_quality_in(sb),
            debt_dir_reverse: p.debt_direction(sb, StrandDirection::Reverse),
            debt_dir_forward: p.debt_direction(sb, StrandDirection::Forward),
            direct_src: p.direct_step_src_acct(),
            is_book: p.book_step_book().is_some(),
        });
        let step = DirectStep { kind, src, dst, currency, is_last: ctx.is_last, prev, cache: None };
        step.check(ctx, sb)?;
        Ok(step)
    }

    pub fn src(&self) -> [u8; 20] {
        self.src
    }
    pub fn dst(&self) -> [u8; 20] {
        self.dst
    }
    pub fn currency(&self) -> [u8; 20] {
        self.currency
    }

    /// `quality(sb, qDir)`: the destination line's QualityIn / QualityOut
    /// for a payment; parity for a crossing.
    fn quality(&self, sb: &PaymentSandbox, dir: QualityDirection) -> u32 {
        match self.kind {
            DirectKind::OfferCrossing => QUALITY_ONE,
            DirectKind::Payment => {
                if self.src == self.dst {
                    return QUALITY_ONE;
                }
                line_quality(sb.sandbox(), &self.src, &self.dst, &self.currency, dir)
            }
        }
    }

    /// `maxPaymentFlow`: what the source can send the destination —
    /// its positive balance (redeems) or the destination's limit plus the
    /// (non-positive) balance (issues).
    fn max_payment_flow(&self, sb: &PaymentSandbox) -> (IouAmount, DebtDirection) {
        let asset = Asset { currency: self.currency, issuer: Some(self.dst) };
        let src_owed = account_holds_signed_iou(sb, &self.src, &asset, FreezeHandling::IgnoreFreeze);
        if src_owed.signum() > 0 {
            return (src_owed, DebtDirection::Redeems);
        }
        (credit_limit(sb.sandbox(), &self.dst, &self.src, &self.currency).add(src_owed), DebtDirection::Issues)
    }

    /// `maxFlow(sb, desired)`: a crossing's last step is non-limiting.
    fn max_flow(&self, sb: &PaymentSandbox, desired: IouAmount) -> (IouAmount, DebtDirection) {
        if self.kind == DirectKind::OfferCrossing && self.is_last {
            return (desired, DebtDirection::Issues);
        }
        self.max_payment_flow(sb)
    }

    fn qualities_src_redeems(&self, sb: &PaymentSandbox) -> (u32, u32) {
        let Some(prev) = self.prev else { return (QUALITY_ONE, QUALITY_ONE) };
        let prev_q_in = prev.line_quality_in;
        let mut src_q_out = self.quality(sb, QualityDirection::Out);
        if prev_q_in > src_q_out {
            src_q_out = prev_q_in;
        }
        (src_q_out, QUALITY_ONE)
    }

    fn qualities_src_issues(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (u32, u32) {
        let src_q_out = if redeems(prev_step_dir) { transfer_rate(sb.sandbox(), &self.src) } else { QUALITY_ONE };
        let mut dst_q_in = self.quality(sb, QualityDirection::In);
        if self.is_last && dst_q_in > QUALITY_ONE {
            dst_q_in = QUALITY_ONE;
        }
        (src_q_out, dst_q_in)
    }

    fn qualities(&self, sb: &PaymentSandbox, src_debt_dir: DebtDirection, strand_dir: StrandDirection) -> (u32, u32) {
        if redeems(src_debt_dir) {
            return self.qualities_src_redeems(sb);
        }
        let prev_dir = match (self.prev, strand_dir) {
            (Some(p), StrandDirection::Reverse) => p.debt_dir_reverse,
            (Some(p), StrandDirection::Forward) => p.debt_dir_forward,
            (None, _) => DebtDirection::Issues,
        };
        self.qualities_src_issues(sb, prev_dir)
    }

    /// The issuer `rippleCredit` moves the amount in: the destination when
    /// the source redeems, the source when it issues.
    fn src_to_dst_asset(&self, src_debt_dir: DebtDirection) -> Asset {
        Asset { currency: self.currency, issuer: Some(if redeems(src_debt_dir) { self.dst } else { self.src }) }
    }

    fn set_cache_limiting(&mut self, fwd_in: IouAmount, fwd_src_to_dst: IouAmount, fwd_out: IouAmount, src_debt_dir: DebtDirection) {
        let Some(mut c) = self.cache else {
            self.cache = Some(Cache { input: fwd_in, src_to_dst: fwd_src_to_dst, out: fwd_out, src_debt_dir });
            return;
        };
        if c.input < fwd_in {
            let small_diff = IouAmount { negative: false, mantissa: 1_000_000_000_000_000, exponent: -24 };
            let diff = fwd_in.sub(c.input);
            if diff > small_diff {
                let ratio_big = fwd_in.exponent != c.input.exponent
                    || c.input.mantissa == 0
                    || (fwd_in.mantissa as f64 / c.input.mantissa as f64) > 1.01;
                if ratio_big {
                    self.cache = Some(Cache { input: fwd_in, src_to_dst: fwd_src_to_dst, out: fwd_out, src_debt_dir });
                    return;
                }
            }
        }
        c.input = fwd_in;
        if fwd_src_to_dst < c.src_to_dst {
            c.src_to_dst = fwd_src_to_dst;
        }
        if fwd_out < c.out {
            c.out = fwd_out;
        }
        c.src_debt_dir = src_debt_dir;
        self.cache = Some(c);
    }

    /// `revImp`.
    pub fn rev_imp(&mut self, sb: &mut PaymentSandbox, out: IouAmount) -> Result<(IouAmount, IouAmount), FlowError> {
        self.cache = None;
        let (max_src_to_dst, src_debt_dir) = self.max_flow(sb, out);
        let (src_q_out, dst_q_in) = self.qualities(sb, src_debt_dir, StrandDirection::Reverse);
        let asset = self.src_to_dst_asset(src_debt_dir);
        if max_src_to_dst.signum() <= 0 {
            self.cache = Some(Cache { input: IouAmount::ZERO, src_to_dst: IouAmount::ZERO, out: IouAmount::ZERO, src_debt_dir });
            return Ok((IouAmount::ZERO, IouAmount::ZERO));
        }
        let src_to_dst = mul_ratio_iou(out, QUALITY_ONE, dst_q_in, true);
        if src_to_dst <= max_src_to_dst {
            let input = mul_ratio_iou(src_to_dst, src_q_out, QUALITY_ONE, true);
            self.cache = Some(Cache { input, src_to_dst, out, src_debt_dir });
            ripple_credit_pub(sb, &self.src, &self.dst, &asset, src_to_dst);
            return Ok((input, out));
        }
        let input = mul_ratio_iou(max_src_to_dst, src_q_out, QUALITY_ONE, true);
        let actual_out = mul_ratio_iou(max_src_to_dst, dst_q_in, QUALITY_ONE, false);
        self.cache = Some(Cache { input, src_to_dst: max_src_to_dst, out: actual_out, src_debt_dir });
        ripple_credit_pub(sb, &self.src, &self.dst, &asset, max_src_to_dst);
        Ok((input, actual_out))
    }

    /// `fwdImp`.
    pub fn fwd_imp(&mut self, sb: &mut PaymentSandbox, input: IouAmount) -> Result<(IouAmount, IouAmount), FlowError> {
        let cache = self.cache.expect("DirectStepI::fwdImp : cache is set");
        let (max_src_to_dst, src_debt_dir) = self.max_flow(sb, cache.src_to_dst);
        let (src_q_out, dst_q_in) = self.qualities(sb, src_debt_dir, StrandDirection::Forward);
        let asset = self.src_to_dst_asset(src_debt_dir);
        if max_src_to_dst.signum() <= 0 {
            self.cache = Some(Cache { input: IouAmount::ZERO, src_to_dst: IouAmount::ZERO, out: IouAmount::ZERO, src_debt_dir });
            return Ok((IouAmount::ZERO, IouAmount::ZERO));
        }
        let src_to_dst = mul_ratio_iou(input, QUALITY_ONE, src_q_out, false);
        if src_to_dst <= max_src_to_dst {
            let out = mul_ratio_iou(src_to_dst, dst_q_in, QUALITY_ONE, false);
            self.set_cache_limiting(input, src_to_dst, out, src_debt_dir);
            let c = self.cache.expect("set");
            ripple_credit_pub(sb, &self.src, &self.dst, &asset, c.src_to_dst);
        } else {
            let actual_in = mul_ratio_iou(max_src_to_dst, src_q_out, QUALITY_ONE, true);
            let out = mul_ratio_iou(max_src_to_dst, dst_q_in, QUALITY_ONE, false);
            self.set_cache_limiting(actual_in, max_src_to_dst, out, src_debt_dir);
            let c = self.cache.expect("set");
            ripple_credit_pub(sb, &self.src, &self.dst, &asset, c.src_to_dst);
        }
        let c = self.cache.expect("set");
        Ok((c.input, c.out))
    }

    /// `DirectStepI::check(ctx)` and the derived `check(ctx, sleSrc)`.
    pub fn check(&self, ctx: &StrandContext, sb: &PaymentSandbox) -> Result<(), StepCheckError> {
        let zero = [0u8; 20];
        if self.src == zero || self.dst == zero {
            return Err(StepCheckError::BadPath);
        }
        if self.src == self.dst {
            return Err(StepCheckError::BadPath);
        }
        let Some(sle_src) = json_at(sb.sandbox(), &keylet::account_root_key(&self.src)) else {
            return Err(StepCheckError::NoAccount);
        };
        if !(ctx.is_last && ctx.is_first) {
            check_freeze(sb.sandbox(), &self.src, &self.dst, &self.currency)?;
        }
        if let Some(prev) = self.prev {
            if let Some(prev_src) = prev.direct_src {
                check_no_ripple(sb.sandbox(), &prev_src, &self.src, &self.dst, &self.currency)?;
            }
        }
        // (seenBookOuts / seenDirectIssues loop checks: the strand builder's.)
        match self.kind {
            DirectKind::OfferCrossing => Ok(()),
            DirectKind::Payment => {
                let Some(line) = json_at(sb.sandbox(), &keylet::ripple_state_key(&self.src, &self.dst, &self.currency)) else {
                    return Err(StepCheckError::NoLine);
                };
                let src_flags = sle_src["Flags"].as_u64().unwrap_or(0);
                if src_flags & LSF_REQUIRE_AUTH != 0
                    && !is_authorized_line_flag(&line, &self.src, &self.dst)
                    && line["Balance"]["value"].as_str().map(|v| v == "0").unwrap_or(true)
                {
                    return Err(StepCheckError::NoAuth);
                }
                if let Some(prev) = self.prev {
                    if prev.is_book {
                        let bit: u64 = if self.src > self.dst { 0x0020_0000 } else { 0x0010_0000 };
                        if line["Flags"].as_u64().unwrap_or(0) & bit != 0 {
                            return Err(StepCheckError::NoRipple);
                        }
                    }
                }
                let owed = credit_balance(sb.sandbox(), &self.dst, &self.src, &self.currency);
                if owed.signum() <= 0 {
                    let limit = credit_limit(sb.sandbox(), &self.dst, &self.src, &self.currency);
                    if owed.negated() >= limit {
                        return Err(StepCheckError::PathDry);
                    }
                }
                Ok(())
            }
        }
    }
}

impl Step for DirectStep {
    fn rev(&mut self, sb: &mut PaymentSandbox, _ofrs_to_rm: &mut OffersToRemove, out: &EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        let (i, o) = self.rev_imp(sb, out.iou())?;
        Ok((EitherAmount::Iou(i), EitherAmount::Iou(o)))
    }

    fn fwd(&mut self, sb: &mut PaymentSandbox, _ofrs_to_rm: &mut OffersToRemove, input: &EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        let (i, o) = self.fwd_imp(sb, input.iou())?;
        Ok((EitherAmount::Iou(i), EitherAmount::Iou(o)))
    }

    fn cached_in(&self) -> Option<EitherAmount> {
        self.cache.map(|c| EitherAmount::Iou(c.input))
    }
    fn cached_out(&self) -> Option<EitherAmount> {
        self.cache.map(|c| EitherAmount::Iou(c.out))
    }

    fn direct_step_src_acct(&self) -> Option<[u8; 20]> {
        Some(self.src)
    }
    fn direct_step_accts(&self) -> Option<([u8; 20], [u8; 20])> {
        Some((self.src, self.dst))
    }

    /// `debtDirection`: the cached direction on a forward pass, else by
    /// the source's balance on the line.
    fn debt_direction(&self, sb: &PaymentSandbox, dir: StrandDirection) -> DebtDirection {
        if dir == StrandDirection::Forward {
            if let Some(c) = self.cache {
                return c.src_debt_dir;
            }
        }
        let asset = Asset { currency: self.currency, issuer: Some(self.dst) };
        let src_owed = account_holds_signed_iou(sb, &self.src, &asset, FreezeHandling::IgnoreFreeze);
        if src_owed.signum() > 0 { DebtDirection::Redeems } else { DebtDirection::Issues }
    }

    fn line_quality_in(&self, sb: &PaymentSandbox) -> u32 {
        self.quality(sb, QualityDirection::In)
    }

    /// `qualityUpperBound` (fixQualityUpperBound live): the rate
    /// `getRate(dstQIn, srcQOut)` under the qualities the debt direction
    /// selects.
    fn quality_upper_bound(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (Option<Quality>, DebtDirection) {
        let dir = self.debt_direction(sb, StrandDirection::Forward);
        let (src_q_out, dst_q_in) = if redeems(dir) { self.qualities_src_redeems(sb) } else { self.qualities_src_issues(sb, prev_step_dir) };
        // `Quality(getRate(STAmount(iss, dstQIn), STAmount(iss, srcQOut)))`:
        // getRate(offerOut, offerIn) = offerIn / offerOut = srcQOut / dstQIn —
        // the step's IN per OUT, so `rate_encode_native(pays = srcQOut,
        // gets = dstQIn)`. (Finding 119's vector: the closing DirectStep's
        // 1/1.001 came out as 1.001 with the arguments swapped.)
        let q = keylet::rate_encode_native(src_q_out as u128, 0, false, dst_q_in as u128, 0, false).map(Quality);
        (q, dir)
    }

    fn get_quality_func(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (Option<QualityFunction>, DebtDirection) {
        let (q, dir) = self.quality_upper_bound(sb, prev_step_dir);
        (q.and_then(|q| QualityFunction::clob_like(q).ok()), dir)
    }

    fn valid_fwd(&mut self, sb: &mut PaymentSandbox, input: &EitherAmount) -> Result<(bool, EitherAmount), FlowError> {
        let Some(sav) = self.cache else { return Ok((false, EitherAmount::Iou(IouAmount::ZERO))) };
        let (max_src_to_dst, _) = self.max_flow(sb, sav.src_to_dst);
        if self.fwd_imp(sb, input.iou()).is_err() {
            return Ok((false, EitherAmount::Iou(IouAmount::ZERO)));
        }
        let c = self.cache.expect("set");
        if max_src_to_dst < c.src_to_dst {
            return Ok((false, EitherAmount::Iou(c.out)));
        }
        if !(check_near_iou(sav.input, c.input) && check_near_iou(sav.out, c.out)) {
            return Ok((false, EitherAmount::Iou(c.out)));
        }
        Ok((true, EitherAmount::Iou(c.out)))
    }

    fn equal(&self, other: &dyn Step) -> bool {
        other.direct_step_accts() == Some((self.src, self.dst)) && other.direct_step_currency() == Some(self.currency)
    }

    fn direct_step_currency(&self) -> Option<[u8; 20]> {
        Some(self.currency)
    }

    fn log_string(&self) -> String {
        format!(
            "{}: src {} dst {} cur {}",
            match self.kind {
                DirectKind::Payment => "DirectIPaymentStep",
                DirectKind::OfferCrossing => "DirectIOfferCrossingStep",
            },
            hex::encode(self.src),
            hex::encode(self.dst),
            hex::encode(self.currency)
        )
    }
}

#[allow(dead_code)]
fn _issues(d: DebtDirection) -> bool {
    issues(d)
}
