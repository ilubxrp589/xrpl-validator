//! rippled `Flow.cpp` (`flow()`), `RippleCalc::rippleCalculate` and the
//! flow half of `Payment::doApply`: the entry the Payment transactor
//! takes for every "ripple" payment — paths, a SendMax, or a non-XRP
//! Amount — under `XRPL_FLOW_ENGINE=port`.
//!
//! Inputs as rippled derives them (Payment.cpp): `maxSourceAmount` is the
//! SendMax, or the Amount re-issued by the sender for an IOU;
//! `limitQuality` is `Quality{Amounts(maxSourceAmount, dstAmount)}` when
//! tfLimitQuality is set and the max is positive; partial payment from
//! tfPartialPayment; default paths unless tfNoRippleDirect; the flow
//! then runs in a PaymentSandbox over the transactor's sandbox, applied
//! on tesSUCCESS (a partial delivery below DeliverMin is
//! tecPATH_PARTIAL) and discarded otherwise — any ter from the flow is
//! tecPATH_DRY in a closed ledger.
use std::cell::RefCell;
use std::rc::Rc;

use super::amm::{AmmContext, SharedAmmContext};
use super::amounts::{EitherAmount, IouAmount};
use super::pay_steps::{to_strands, PathElement, StrandInputs};
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::Quality;
use super::steps::{Asset, OfferCrossing};
use super::strand_flow::{flow_strands, FlowResult};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{TxFields, TxResult};
use crate::tx::offer::{decode20, Me};
use xrpl_core::types::Hash256;

/// `XRPL_FLOW_ENGINE=port` selects this engine for a transaction.
pub fn port_enabled() -> bool {
    std::env::var("XRPL_FLOW_ENGINE").map(|v| v == "port").unwrap_or(false)
}

/// An amount from a transaction field: XRP drops or an IOU with its asset.
#[derive(Clone, Copy, Debug)]
pub struct StAmountJson {
    pub asset: Asset,
    pub amount: EitherAmount,
}

pub fn amount_from_json(v: &serde_json::Value) -> Option<StAmountJson> {
    if let Some(s) = v.as_str() {
        let d: i128 = s.parse().ok()?;
        return Some(StAmountJson { asset: Asset::XRP, amount: EitherAmount::Xrp(d) });
    }
    let cur = v.get("currency").and_then(|c| c.as_str())?;
    let currency: [u8; 20] = if cur == "XRP" {
        [0; 20]
    } else if cur.len() == 40 {
        hex::decode(cur).ok().and_then(|b| <[u8; 20]>::try_from(b.as_slice()).ok())?
    } else {
        // A 3-letter code: rippled's standard currency layout.
        let mut c = [0u8; 20];
        let b = cur.as_bytes();
        if b.len() != 3 {
            return None;
        }
        c[12..15].copy_from_slice(b);
        c
    };
    let issuer = v.get("issuer").and_then(|i| i.as_str()).and_then(decode20)?;
    let (neg, me): (bool, Me) = crate::tx::offer::signed_value(v);
    Some(StAmountJson { asset: Asset { currency, issuer: Some(issuer) }, amount: EitherAmount::Iou(IouAmount::from_me(neg && me.0 > 0, me)) })
}

/// `getMaxSourceAmount(account, dstAmount, sendMax)`.
fn max_source_amount(account: &[u8; 20], dst: &StAmountJson, send_max: Option<&StAmountJson>) -> StAmountJson {
    if let Some(sm) = send_max {
        return *sm;
    }
    if dst.asset.is_xrp() {
        return *dst;
    }
    StAmountJson { asset: Asset { currency: dst.asset.currency, issuer: Some(*account) }, amount: dst.amount }
}

/// `Quality{Amounts(in, out)}` = `getRate(out, in)`.
fn quality_of(input: EitherAmount, out: EitherAmount) -> Option<Quality> {
    let (im, om) = (input.mantissa_exp(), out.mantissa_exp());
    keylet::rate_encode_native(im.0, im.1, input.is_xrp(), om.0, om.1, out.is_xrp()).map(Quality)
}

/// `flow(sb, deliver, src, dst, paths, defaultPaths, partialPayment,
/// ownerPaysTransferFee, offerCrossing, limitQuality, sendMax, domainID)`
/// — Flow.cpp. Returns the result; on success the flow's writes are in
/// `ps`'s live view already (the flow's own view is applied onto it).
#[allow(clippy::too_many_arguments)]
pub fn flow(
    ps: &mut PaymentSandbox,
    deliver: &StAmountJson,
    src: [u8; 20],
    dst: [u8; 20],
    paths: &[Vec<PathElement>],
    default_paths: bool,
    partial_payment: bool,
    owner_pays_transfer_fee: bool,
    offer_crossing: OfferCrossing,
    limit_quality: Option<Quality>,
    send_max: Option<&StAmountJson>,
    domain: Option<Hash256>,
) -> FlowResult {
    let src_issue: Asset = match send_max {
        Some(sm) => sm.asset,
        None => {
            if !deliver.asset.is_xrp() {
                Asset { currency: deliver.asset.currency, issuer: Some(src) }
            } else {
                Asset::XRP
            }
        }
    };
    let amm_ctx: SharedAmmContext = Rc::new(RefCell::new(AmmContext::new(src, false)));
    let inputs = StrandInputs {
        src,
        dst,
        deliver: deliver.asset,
        limit_quality,
        send_max_issue: send_max.map(|s| s.asset),
        owner_pays_transfer_fee,
        offer_crossing,
        amm_ctx: &amm_ctx,
        domain,
    };
    let mut strands = match to_strands(ps, &inputs, paths, default_paths) {
        Ok(s) => s,
        Err(t) => {
            return FlowResult { input: EitherAmount::zero(src_issue.is_xrp()), out: EitherAmount::zero(deliver.asset.is_xrp()), removable_offers: Default::default(), ter: t };
        }
    };
    amm_ctx.borrow_mut().set_multi_path(strands.len() > 1);
    // The flow's own view: a layer over the caller's, applied on success.
    ps.push();
    let r = flow_strands(ps, &mut strands, deliver.amount, partial_payment, offer_crossing, limit_quality, send_max.map(|s| s.amount), &amm_ctx);
    if r.ter == TxResult::Success {
        ps.apply_to_parent();
    } else {
        ps.discard();
    }
    r
}

/// `RippleCalc::rippleCalculate` + the flow half of `Payment::doApply`.
/// Returns the TER and, on success, delivers into `sandbox`.
pub fn apply_ripple_payment(tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
    let Some(dst_amount) = tx.fields.get("Amount").and_then(amount_from_json) else { return TxResult::Malformed };
    let send_max = tx.fields.get("SendMax").and_then(amount_from_json);
    let deliver_min = tx.fields.get("DeliverMin").and_then(amount_from_json);
    let Some(dst) = tx.fields.get("Destination").and_then(|v| v.as_str()).and_then(decode20) else { return TxResult::Malformed };
    let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
    let partial = flags & 0x0002_0000 != 0;
    let limit_quality_flag = flags & 0x0004_0000 != 0;
    let default_paths = flags & 0x0001_0000 == 0;
    let max_source = max_source_amount(&tx.account, &dst_amount, send_max.as_ref());
    let paths: Vec<Vec<PathElement>> = tx
        .fields
        .get("Paths")
        .and_then(|p| p.as_array())
        .map(|ps| ps.iter().map(|p| p.as_array().map(|els| els.iter().filter_map(PathElement::from_json).collect()).unwrap_or_default()).collect())
        .unwrap_or_default();
    let domain = tx.fields.get("DomainID").and_then(|v| v.as_str()).and_then(|s| hex::decode(s).ok()).and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()).map(Hash256);
    // rippleCalculate's inputs.
    // Finding 304: `Quality{Amounts{maxSourceAmount, dstAmount}}` = getRate,
    // which files rate 0 for a ratio the STAmount range cannot hold (a
    // deliver-max Amount against a small SendMax) — and Quality(0) is the
    // BEST quality: every strand falls short, "Path rejected by
    // limitQuality", tecPATH_DRY. An unencodable ratio used to read as no
    // limit here.
    let limit_quality = if limit_quality_flag && max_source.amount.signum() > 0 { Some(quality_of(max_source.amount, dst_amount.amount).unwrap_or(Quality(0))) } else { None };
    // sendMax: the max unless it is the Amount re-issued by the sender
    // (then no SendMax at all).
    let flow_send_max = {
        let same_as_dst = max_source.asset.currency == dst_amount.asset.currency && max_source.asset.issuer == Some(tx.account) && send_max.is_none();
        if max_source.amount.signum() >= 0 || !same_as_dst { Some(max_source) } else { None }
    };
    let mut ps = PaymentSandbox::new(sandbox);
    let r = flow(&mut ps, &dst_amount, tx.account, dst, &paths, default_paths, partial, false, OfferCrossing::No, limit_quality, flow_send_max.as_ref(), domain);
    let mut ter = r.ter;
    if ter == TxResult::Success && r.out != dst_amount.amount {
        if let Some(dm) = deliver_min {
            if r.out.lt(&dm.amount) {
                ter = TxResult::PathPartial;
            }
        }
        // `ctx_.deliver(actualAmountOut)`: the metadata's delivered_amount
        // — recorded by the transactor from the sandbox's writes.
    }
    // A ter (retry) from the flow is tecPATH_DRY once the ledger closes.
    if matches!(ter, TxResult::NoLine | TxResult::NoRipple | TxResult::NoAccount | TxResult::NoAuth) {
        ter = TxResult::PathDry;
    }
    ter
}
