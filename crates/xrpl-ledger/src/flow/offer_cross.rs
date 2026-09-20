//! rippled `CreateOffer::flowCross` (`CreateOffer.cpp`): an OfferCreate
//! crosses the books by running the payment flow from the taker to
//! itself — `deliver` = TakerPays (a sell delivers "the largest possible
//! amount"), `sendMax` = TakerGets grossed up by its issuer's transfer
//! rate and capped at what the taker holds, `limitQuality` =
//! `Quality{takerAmount.out, sendMax}` (one notch stricter under
//! tfPassive), the XRP bridge as the one explicit path when both sides are
//! IOUs, default path on, owner pays the transfer fee, partial unless
//! tfFillOrKill. `result.removableOffers` are deleted in the view (and the
//! cancel view) whatever the result; the rest of the offer (`afterCross`)
//! is re-derived from what the flow actually moved.
use super::amounts::{EitherAmount, IouAmount};
use super::pay_steps::PathElement;
use super::payment_flow::{flow, StAmountJson};
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::Quality;
use super::st_amount::{div_round_strict, mul_round, StAmount};
use super::steps::{Asset, OfferCrossing};
use super::view::{account_holds_iou, transfer_rate, xrp_liquid, AuthHandling, FreezeHandling, QUALITY_ONE};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::TxResult;
use xrpl_core::types::Hash256;

/// What the transactor gets back: the TER, the offer that remains
/// (`afterCross`: in = TakerGets, out = TakerPays), what the flow moved,
/// and the offers the flow removed (already deleted in `sandbox`; the
/// kill path re-deletes them after its snapshot restore).
pub struct CrossResult {
    pub ter: TxResult,
    pub after_in: EitherAmount,
    pub after_out: EitherAmount,
    pub actual_in: EitherAmount,
    pub actual_out: EitherAmount,
    pub removed: Vec<Hash256>,
}

fn st(v: EitherAmount) -> StAmount {
    match v {
        EitherAmount::Xrp(d) => StAmount { native: true, negative: d < 0, mantissa: d.unsigned_abs() as u64, exponent: 0 },
        EitherAmount::Iou(a) => StAmount::iou(a.negative, a.mantissa, a.exponent),
    }
}

fn un_st(a: StAmount) -> EitherAmount {
    if a.native {
        let d = a.mantissa as i128;
        EitherAmount::Xrp(if a.negative { -d } else { d })
    } else {
        EitherAmount::Iou(IouAmount { negative: a.negative, mantissa: a.mantissa, exponent: a.exponent })
    }
}

/// `accountFunds(psb, account, takerAmount.in, fhZERO_IF_FROZEN)`: the
/// issuer of an IOU has unlimited funds; XRP is `xrpLiquid`.
fn account_funds(ps: &PaymentSandbox, account: &[u8; 20], asset: &Asset, amount: EitherAmount) -> EitherAmount {
    match amount {
        EitherAmount::Xrp(_) => EitherAmount::Xrp(xrp_liquid(ps, account, 0)),
        EitherAmount::Iou(_) => {
            if asset.issuer == Some(*account) {
                // `STAmount{issue, cMaxValue, cMaxOffset}`.
                EitherAmount::Iou(IouAmount { negative: false, mantissa: 9_999_999_999_999_999, exponent: 80 })
            } else {
                EitherAmount::Iou(account_holds_iou(ps, account, asset, FreezeHandling::ZeroIfFrozen, AuthHandling::IgnoreAuth))
            }
        }
    }
}

/// `Quality{out, in}` = in / out.
fn quality(out: EitherAmount, input: EitherAmount) -> Option<Quality> {
    let (im, om) = (input.mantissa_exp(), out.mantissa_exp());
    keylet::rate_encode_native(im.0, im.1, input.is_xrp(), om.0, om.1, out.is_xrp()).map(Quality)
}

/// `CreateOffer::flowCross(psb, psbCancel, takerAmount, domainID)`.
/// `taker_in` is TakerGets, `taker_out` is TakerPays.
#[allow(clippy::too_many_arguments)]
pub fn flow_cross(sandbox: &mut Sandbox, account: &[u8; 20], asset_in: Asset, taker_in: EitherAmount, asset_out: Asset, taker_out: EitherAmount, flags: u64, domain: Option<Hash256>) -> CrossResult {
    let mut ps = PaymentSandbox::new(sandbox);
    let in_start_balance = account_funds(&ps, account, &asset_in, taker_in);
    if in_start_balance.signum() <= 0 {
        return CrossResult { ter: TxResult::UnfundedOffer, after_in: taker_in, after_out: taker_out, actual_in: EitherAmount::zero(taker_in.is_xrp()), actual_out: EitherAmount::zero(taker_out.is_xrp()), removed: Vec::new() };
    }
    // The gateway's transfer rate on what the taker gives, unless the
    // taker is its issuer.
    let mut gateway_rate = QUALITY_ONE;
    let mut send_max = taker_in;
    if let (EitherAmount::Iou(_), Some(iss)) = (taker_in, asset_in.issuer) {
        if *account != iss {
            gateway_rate = transfer_rate(ps.sandbox(), &iss);
            if gateway_rate != QUALITY_ONE {
                // `multiplyRound(takerAmount.in, rate, issue, true)`.
                let rate_st = StAmount::iou(false, gateway_rate as u64, -9);
                send_max = mul_round(&st(taker_in), &rate_st, false, true).map(un_st).unwrap_or(taker_in);
            }
        }
    }
    let Some(mut threshold) = quality(taker_out, send_max) else {
        return CrossResult { ter: TxResult::Malformed, after_in: taker_in, after_out: taker_out, actual_in: EitherAmount::zero(taker_in.is_xrp()), actual_out: EitherAmount::zero(taker_out.is_xrp()), removed: Vec::new() };
    };
    let tf_passive = flags & 0x0001_0000 != 0;
    let tf_sell = flags & 0x0008_0000 != 0;
    let tf_fill_or_kill = flags & 0x0004_0000 != 0;
    if tf_passive {
        // `++threshold`: one notch better (a smaller encoded value).
        threshold = Quality(threshold.0.saturating_sub(1));
    }
    if send_max.gt(&in_start_balance) {
        send_max = in_start_balance;
    }
    // The XRP bridge as the one explicit path for an IOU/IOU crossing.
    let mut paths: Vec<Vec<PathElement>> = Vec::new();
    if !taker_in.is_xrp() && !taker_out.is_xrp() {
        paths.push(vec![PathElement::offer(Some([0u8; 20]), None)]);
    }
    let mut deliver = taker_out;
    let offer_crossing = if tf_sell {
        deliver = match taker_out {
            EitherAmount::Xrp(_) => EitherAmount::Xrp(100_000_000_000_000_000),
            EitherAmount::Iou(_) => EitherAmount::Iou(IouAmount { negative: false, mantissa: 9_999_999_999_999_999 / 2, exponent: 80 }),
        };
        OfferCrossing::Sell
    } else {
        OfferCrossing::Yes
    };
    let deliver_j = StAmountJson { asset: asset_out, amount: deliver };
    let send_max_j = StAmountJson { asset: asset_in, amount: send_max };
    let result = flow(&mut ps, &deliver_j, *account, *account, &paths, true, !tf_fill_or_kill, true, offer_crossing, Some(threshold), Some(&send_max_j), domain);
    // `for toRemove in result.removableOffers: offerDelete(psb, …)` — on
    // success the flow already deleted them in its view; on failure they
    // are deleted here, and the caller's kill path re-deletes them after
    // its snapshot restore.
    let removed: Vec<Hash256> = result.removable_offers.iter().map(|k| Hash256(*k)).collect();
    for k in &removed {
        super::offer_stream::offer_delete(&mut ps, k);
    }
    let mut after_in = taker_in;
    let mut after_out = taker_out;
    if result.ter == TxResult::Success {
        let taker_in_balance = account_funds(&ps, account, &asset_in, taker_in);
        if taker_in_balance.signum() <= 0 {
            after_in = EitherAmount::zero(taker_in.is_xrp());
            after_out = EitherAmount::zero(taker_out.is_xrp());
        } else {
            // `STAmount const rate{Quality{takerAmount.out, takerAmount.in}.rate()}`
            let q = quality(taker_out, taker_in).unwrap_or(Quality(0));
            let rate = super::st_amount::rate_amount(q.0);
            if tf_sell {
                let mut non_gateway_in = result.input;
                if gateway_rate != QUALITY_ONE {
                    // `divideRound(actualAmountIn, gatewayXferRate, issue, true)`
                    let rate_st = StAmount::iou(false, gateway_rate as u64, -9);
                    non_gateway_in = super::st_amount::div_round(&st(result.input), &rate_st, result.input.is_xrp(), true).map(un_st).unwrap_or(result.input);
                }
                after_in = after_in.sub(non_gateway_in);
                if after_in.signum() < 0 {
                    after_in = EitherAmount::zero(taker_in.is_xrp());
                }
                // fixReducedOffersV1: `divRoundStrict(afterCross.in, rate, out issue, false)`.
                after_out = div_round_strict(&st(after_in), &rate, taker_out.is_xrp(), false).map(un_st).unwrap_or(after_out);
            } else {
                after_out = after_out.sub(result.out);
                if after_out.signum() < 0 {
                    after_out = EitherAmount::zero(taker_out.is_xrp());
                }
                after_in = mul_round(&st(after_out), &rate, taker_in.is_xrp(), true).map(un_st).unwrap_or(after_in);
            }
        }
    }
    CrossResult { ter: result.ter, after_in, after_out, actual_in: result.input, actual_out: result.out, removed }
}
