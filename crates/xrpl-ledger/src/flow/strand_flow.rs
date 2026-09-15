//! rippled `StrandFlow.h`: `flow()` over one strand (the reverse pass
//! from the last step, the limiting step re-executed, the forward pass
//! from it), `qualityUpperBound(strand)`, `limitOut`, `ActiveStrands`
//! (featureFlowSortStrands: best bound first, one strand flowed per
//! iteration), and `flow()` over all strands with the limitQuality judge,
//! the Fibonacci-iteration AMM context, savedIns/savedOuts summed
//! ascending, and the tfFillOrKill / partial-payment verdicts.
//!
//! Every strand runs inside its own PaymentSandbox layer (`sb.push()`):
//! the layer's opening snapshot is the "all funds" view its OfferStreams
//! judge "found unfunded" against, and the layer is applied to the flow's
//! view only when the strand wins the iteration (`best->sb.apply(sb)`),
//! discarded otherwise — with `ofrsToRm` kept either way ("rm bad offers
//! even if the strand fails").
use super::amm::{AmmContext, SharedAmmContext};
use super::amounts::EitherAmount;
use super::offer_stream::offer_delete;
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::{Quality, QualityFunction};
use super::steps::{check_near, DebtDirection, FlowError, OffersToRemove, OfferCrossing, Strand};
use crate::ledger::transactor::TxResult;
use xrpl_core::types::Hash256;

/// `StrandResult`.
pub struct StrandResult {
    pub success: bool,
    pub input: EitherAmount,
    pub out: EitherAmount,
    pub ofrs_to_rm: OffersToRemove,
    pub ofrs_used: u32,
    pub inactive: bool,
}

/// `isDirectXrpToXrp`: a one-step strand from XRP to XRP.
fn is_direct_xrp_to_xrp(strand: &Strand, in_xrp: bool, out_xrp: bool) -> bool {
    in_xrp && out_xrp && strand.len() == 1
}

fn is_zero(a: &EitherAmount) -> bool {
    a.is_zero() || a.signum() < 0
}

/// `flow(baseView, strand, maxIn, out)`: runs the strand inside a pushed
/// layer of `sb`. On success the layer is LEFT OPEN for the caller to
/// apply or discard; on failure it is discarded here.
pub fn flow_strand(sb: &mut PaymentSandbox, strand: &mut Strand, max_in: Option<EitherAmount>, out: EitherAmount, in_xrp: bool, out_xrp: bool) -> StrandResult {
    let mut ofrs_to_rm = OffersToRemove::new();
    let fail = |strand: &Strand, ofrs_to_rm: OffersToRemove| StrandResult {
        success: false,
        input: EitherAmount::zero(in_xrp),
        out: EitherAmount::zero(out_xrp),
        ofrs_to_rm,
        ofrs_used: super::steps::offers_used(strand),
        inactive: false,
    };
    if strand.is_empty() {
        return fail(strand, ofrs_to_rm);
    }
    if is_direct_xrp_to_xrp(strand, in_xrp, out_xrp) {
        return fail(strand, ofrs_to_rm);
    }
    let s = strand.len();
    let mut limiting_step = s;
    sb.push();
    let mut limit_step_out = EitherAmount::zero(out_xrp);
    // Reverse pass.
    {
        let mut step_out = out;
        let mut i = s;
        while i > 0 {
            i -= 1;
            let r = match strand[i].rev(sb, &mut ofrs_to_rm, &step_out) {
                Ok(r) => r,
                Err(_) => {
                    sb.discard();
                    return fail(strand, ofrs_to_rm);
                }
            };
            if is_zero(&r.1) {
                // "Strand found dry in rev"
                sb.discard();
                return fail(strand, ofrs_to_rm);
            }
            let mut r = r;
            if i == 0 && max_in.is_some_and(|m| m.lt(&r.0)) {
                // Limiting — exceeded maxIn: throw out the sandbox, re-run
                // the first step forward at maxIn.
                let m = max_in.expect("checked");
                sb.discard();
                sb.push();
                limiting_step = i;
                r = match strand[i].fwd(sb, &mut ofrs_to_rm, &m) {
                    Ok(r) => r,
                    Err(_) => {
                        sb.discard();
                        return fail(strand, ofrs_to_rm);
                    }
                };
                limit_step_out = r.1;
                if is_zero(&r.1) {
                    sb.discard();
                    return fail(strand, ofrs_to_rm);
                }
                if r.0 != m {
                    sb.discard();
                    return fail(strand, ofrs_to_rm);
                }
            } else if r.1 != step_out {
                // Limiting: throw out the sandbox (and the all-funds view),
                // re-execute this step at what it can give.
                sb.discard();
                sb.push();
                limiting_step = i;
                step_out = r.1;
                r = match strand[i].rev(sb, &mut ofrs_to_rm, &step_out) {
                    Ok(r) => r,
                    Err(_) => {
                        sb.discard();
                        return fail(strand, ofrs_to_rm);
                    }
                };
                limit_step_out = r.1;
                if is_zero(&r.1) {
                    sb.discard();
                    return fail(strand, ofrs_to_rm);
                }
                if r.1 != step_out {
                    sb.discard();
                    return fail(strand, ofrs_to_rm);
                }
            }
            // The previous node must produce what this one consumes.
            step_out = r.0;
        }
    }
    // Forward pass from the step after the limiting one.
    {
        let mut step_in = limit_step_out;
        let mut i = limiting_step + 1;
        while i < s {
            let r = match strand[i].fwd(sb, &mut ofrs_to_rm, &step_in) {
                Ok(r) => r,
                Err(_) => {
                    sb.discard();
                    return fail(strand, ofrs_to_rm);
                }
            };
            if is_zero(&r.1) {
                sb.discard();
                return fail(strand, ofrs_to_rm);
            }
            if r.0 != step_in {
                sb.discard();
                return fail(strand, ofrs_to_rm);
            }
            step_in = r.1;
            i += 1;
        }
    }
    let strand_in = strand.first().and_then(|st| st.cached_in());
    let strand_out = strand.last().and_then(|st| st.cached_out());
    let (Some(strand_in), Some(strand_out)) = (strand_in, strand_out) else {
        sb.discard();
        return fail(strand, ofrs_to_rm);
    };
    let inactive = strand.iter().any(|st| st.inactive());
    StrandResult { success: true, input: strand_in, out: strand_out, ofrs_to_rm, ofrs_used: super::steps::offers_used(strand), inactive }
}

/// `qualityUpperBound(v, strand)`: the composition of every step's bound.
pub fn strand_quality_upper_bound(sb: &PaymentSandbox, strand: &Strand) -> Option<Quality> {
    let mut q = Quality(crate::ledger::keylet::rate_encode_native(1, 0, false, 1, 0, false)?);
    let mut dir = DebtDirection::Issues;
    for step in strand.iter() {
        let (sq, d) = step.quality_upper_bound(sb, dir);
        dir = d;
        q = super::book_step::composed_quality(q, sq?);
    }
    Some(q)
}

/// `limitOut(v, strand, remainingOut, limitQuality)`: with one active
/// strand whose composed quality function is sloped (an AMM), the output
/// that lands exactly on the limit quality, unless within 1e-9 of the
/// remaining out.
pub fn limit_out(sb: &PaymentSandbox, strand: &Strand, remaining_out: EitherAmount, limit_quality: Quality) -> EitherAmount {
    let mut qf: Option<QualityFunction> = None;
    let mut dir = DebtDirection::Issues;
    for step in strand.iter() {
        let (sqf, d) = step.get_quality_func(sb, dir);
        dir = d;
        let Some(sqf) = sqf else { return remaining_out };
        match qf.as_mut() {
            None => qf = Some(sqf),
            Some(f) => {
                if f.combine(&sqf).is_err() {
                    return remaining_out;
                }
            }
        }
    }
    let Some(qf) = qf else { return remaining_out };
    if qf.is_const() {
        return remaining_out;
    }
    let Ok(Some(out)) = qf.out_from_avg_q(limit_quality) else { return remaining_out };
    let out = match remaining_out {
        EitherAmount::Xrp(_) => EitherAmount::Xrp(out.to_drops(crate::tx::number::Rounding::ToNearest).unwrap_or(0) as i128),
        EitherAmount::Iou(_) => EitherAmount::Iou(super::amounts::IouAmount::from_number(out)),
    };
    // A tiny difference could be round-off.
    if within_relative_distance_amt(out, remaining_out) {
        return remaining_out;
    }
    if out.lt(&remaining_out) { out } else { remaining_out }
}

/// `withinRelativeDistance(out, remainingOut, Number(1, -9))` on amounts.
fn within_relative_distance_amt(a: EitherAmount, b: EitherAmount) -> bool {
    if a == b {
        return true;
    }
    use crate::tx::amm_swap::{n_cmp, n_div, n_sub, Rnd};
    let (mn, mx) = if a.lt(&b) { (a, b) } else { (b, a) };
    let (mnm, mxm) = (mn.mantissa_exp(), mx.mantissa_exp());
    if mxm.0 == 0 {
        return true;
    }
    let rel = n_div(n_sub(mxm, mnm, Rnd::Near), mxm, Rnd::Near);
    n_cmp(rel, (1_000_000_000_000_000, -24)).is_lt()
}

/// `ActiveStrands` under featureFlowSortStrands.
pub struct ActiveStrands {
    cur: Vec<usize>,
    next: Vec<usize>,
}

impl ActiveStrands {
    pub fn new(n: usize) -> ActiveStrands {
        ActiveStrands { cur: Vec::with_capacity(n), next: (0..n).collect() }
    }

    /// `activateNext`: the strands in `next_` become current, sorted by
    /// their bounds (best first, stable), dropping any whose bound misses
    /// the limit quality — only when more than one is pending.
    pub fn activate_next(&mut self, sb: &PaymentSandbox, strands: &[Strand], limit_quality: Option<Quality>) {
        self.cur.clear();
        if !self.next.is_empty() && self.next.len() > 1 {
            let mut quals: Vec<(Quality, usize)> = Vec::with_capacity(self.next.len());
            for &i in &self.next {
                if let Some(q) = strand_quality_upper_bound(sb, &strands[i]) {
                    // `*qual < *limitQuality`: worse than the limit → out.
                    if limit_quality.is_some_and(|lq| q.0 > lq.0) {
                        continue;
                    }
                    quals.push((q, i));
                }
            }
            // Higher qualities first (smaller encoded value); stable.
            quals.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
            self.next = quals.into_iter().map(|(_, i)| i).collect();
        }
        std::mem::swap(&mut self.cur, &mut self.next);
    }

    pub fn get(&self, i: usize) -> Option<usize> {
        self.cur.get(i).copied()
    }
    pub fn push(&mut self, s: usize) {
        self.next.push(s);
    }
    pub fn push_remaining_cur_to_next(&mut self, i: usize) {
        if i >= self.cur.len() {
            return;
        }
        let tail: Vec<usize> = self.cur[i..].to_vec();
        self.next.extend(tail);
    }
    pub fn size(&self) -> usize {
        self.cur.len()
    }
}

/// `FlowResult`.
pub struct FlowResult {
    pub input: EitherAmount,
    pub out: EitherAmount,
    pub removable_offers: OffersToRemove,
    pub ter: TxResult,
}

fn sum(col: &[EitherAmount], xrp: bool) -> EitherAmount {
    if col.is_empty() {
        return EitherAmount::zero(xrp);
    }
    let mut v = col.to_vec();
    v.sort_by(|a, b| a.cmp_like(b));
    let mut acc = v[0];
    for x in &v[1..] {
        acc = acc.add(*x);
    }
    acc
}

/// `Quality{out, in}` of a strand's realised amounts.
fn quality_of(out: EitherAmount, input: EitherAmount) -> Option<Quality> {
    let (om, im) = (out.mantissa_exp(), input.mantissa_exp());
    crate::ledger::keylet::rate_encode_native(im.0, im.1, input.is_xrp(), om.0, om.1, out.is_xrp()).map(Quality)
}

/// `flow(baseView, strands, outReq, partialPayment, offerCrossing,
/// limitQuality, sendMax, ammContext)`. `sb` is the flow's view: the
/// winning strand of each iteration is applied into it; the caller
/// applies or discards `sb` as a whole per the TER.
#[allow(clippy::too_many_arguments)]
pub fn flow_strands(
    sb: &mut PaymentSandbox,
    strands: &mut [Strand],
    out_req: EitherAmount,
    partial_payment: bool,
    offer_crossing: OfferCrossing,
    limit_quality: Option<Quality>,
    send_max: Option<EitherAmount>,
    amm_ctx: &SharedAmmContext,
) -> FlowResult {
    let in_xrp = send_max.map(|m| m.is_xrp()).unwrap_or(false);
    let out_xrp = out_req.is_xrp();
    let max_tries = 1000usize;
    let mut cur_try = 0usize;
    let max_offers_to_consider = 1500u32;
    let mut offers_considered = 0u32;
    let send_max = send_max.filter(|m| m.signum() >= 0);
    let mut remaining_in = send_max;
    let mut remaining_out = out_req;
    let mut active = ActiveStrands::new(strands.len());
    let mut saved_ins: Vec<EitherAmount> = Vec::new();
    let mut saved_outs: Vec<EitherAmount> = Vec::new();
    let mut ofrs_to_rm_on_fail = OffersToRemove::new();

    while remaining_out.signum() > 0 && remaining_in.is_none_or(|r| r.signum() > 0) {
        cur_try += 1;
        if cur_try >= max_tries {
            return FlowResult { input: EitherAmount::zero(in_xrp), out: EitherAmount::zero(out_xrp), removable_offers: ofrs_to_rm_on_fail, ter: TxResult::FailedProcessing };
        }
        active.activate_next(sb, strands, limit_quality);
        amm_ctx.borrow_mut().set_multi_path(active.size() > 1);
        // Limit only if one strand and limitQuality.
        let limit_remaining_out = match (active.size(), limit_quality) {
            (1, Some(lq)) => match active.get(0) {
                Some(i) => limit_out(sb, &strands[i], remaining_out, lq),
                None => remaining_out,
            },
            _ => remaining_out,
        };
        let adjusted_rem_out = limit_remaining_out != remaining_out;
        let mut ofrs_to_rm = OffersToRemove::new();
        // (in, out, strand index, quality) of the best strand; its layer is
        // left open on `sb` while it is the best.
        let mut best: Option<(EitherAmount, EitherAmount, usize, Quality)> = None;
        let mut strand_index = 0usize;
        while strand_index < active.size() {
            let Some(si) = active.get(strand_index) else { strand_index += 1; continue };
            amm_ctx.borrow_mut().clear();
            if offer_crossing != OfferCrossing::No {
                if let Some(lq) = limit_quality {
                    let sq = strand_quality_upper_bound(sb, &strands[si]);
                    if sq.is_none_or(|q| q.0 > lq.0) {
                        strand_index += 1;
                        continue;
                    }
                }
            }
            let f = flow_strand(sb, &mut strands[si], remaining_in, limit_remaining_out, in_xrp, out_xrp);
            // rm bad offers even if the strand fails
            ofrs_to_rm.extend(f.ofrs_to_rm.iter().copied());
            offers_considered += f.ofrs_used;
            if !f.success || f.out.is_zero() {
                if f.success {
                    sb.discard();
                }
                strand_index += 1;
                continue;
            }
            let Some(q) = quality_of(f.out, f.input) else {
                sb.discard();
                strand_index += 1;
                continue;
            };
            // limitOut() finds the output for the exact limit quality, but
            // round-off can leave it slightly off.
            if let Some(lq) = limit_quality {
                if q.0 > lq.0 && (!adjusted_rem_out || !super::amm::within_relative_distance_q(q, lq)) {
                    // "Path rejected by limitQuality"
                    sb.discard();
                    strand_index += 1;
                    continue;
                }
            }
            // featureFlowSortStrands: the first strand that flows is the best.
            if !f.inactive {
                active.push(si);
            }
            best = Some((f.input, f.out, si, q));
            active.push_remaining_cur_to_next(strand_index + 1);
            break;
        }
        let should_break = best.is_none() || offers_considered >= max_offers_to_consider;
        if let Some((bin, bout, _si, _q)) = best {
            saved_ins.push(bin);
            saved_outs.push(bout);
            remaining_out = out_req.sub(sum(&saved_outs, out_xrp));
            if let Some(m) = send_max {
                remaining_in = Some(m.sub(sum(&saved_ins, in_xrp)));
            }
            // `best->sb.apply(sb)`: fold the strand's layer into the flow's view.
            sb.apply_to_parent();
            amm_ctx.borrow_mut().update();
        }
        if !ofrs_to_rm.is_empty() {
            ofrs_to_rm_on_fail.extend(ofrs_to_rm.iter().copied());
            for o in &ofrs_to_rm {
                offer_delete(sb, &Hash256(*o));
            }
        }
        if should_break {
            break;
        }
    }
    let actual_out = sum(&saved_outs, out_xrp);
    let actual_in = sum(&saved_ins, in_xrp);
    // fixFillOrKill is live.
    if actual_out != out_req {
        if actual_out.gt(&out_req) {
            return FlowResult { input: actual_in, out: actual_out, removable_offers: ofrs_to_rm_on_fail, ter: TxResult::FailedProcessing };
        }
        if !partial_payment {
            if offer_crossing == OfferCrossing::No || offer_crossing != OfferCrossing::Sell {
                return FlowResult { input: actual_in, out: actual_out, removable_offers: ofrs_to_rm_on_fail, ter: TxResult::PathPartial };
            }
        } else if actual_out.is_zero() {
            return FlowResult { input: actual_in, out: actual_out, removable_offers: ofrs_to_rm_on_fail, ter: TxResult::PathDry };
        }
    }
    if offer_crossing != OfferCrossing::No && !partial_payment && offer_crossing == OfferCrossing::Sell {
        if remaining_in.is_some_and(|r| !r.is_zero()) {
            return FlowResult { input: actual_in, out: actual_out, removable_offers: ofrs_to_rm_on_fail, ter: TxResult::PathPartial };
        }
    }
    FlowResult { input: actual_in, out: actual_out, removable_offers: ofrs_to_rm_on_fail, ter: TxResult::Success }
}

#[allow(dead_code)]
fn _unused(_: &AmmContext, _: fn(&EitherAmount, &EitherAmount) -> bool) {
    let _ = check_near;
}
