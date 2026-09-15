//! rippled `BookStep` (`src/xrpld/app/paths/detail/BookStep.cpp`): the
//! step that crosses one order book, as `BookPaymentStep` (a payment) or
//! `BookOfferCrossingStep` (an OfferCreate — self-cross removal on the
//! default path, the `limitQuality` prune, fee waivers when the taker is
//! the owner). The three passes are literal:
//!
//!   * `forEachOffer` (717–876): transfer rates, the stream, `execOffer`
//!     (same-quality guard → self-cross → owner authorisation →
//!     `checkQualityThreshold` → `stpAmt` from `ofrAmt` through the in
//!     rate → owner funds clamp with `limitOut(roundUp=false)`) and
//!     `tryAMM` at the tip's quality;
//!   * `revImp` (1014–1135): consume by the OUT wanted, whole offers first,
//!     the last through `limitStepOut`, continue past a satisfied want ("we
//!     need to consume the offer") and stop only on a trimmed offer that
//!     survives (`fully_consumed`);
//!   * `fwdImp` (1137–1308): consume by the IN carried, `limitStepIn` on
//!     the trimming offer (processMore = false), and the cache fix-up when
//!     the forward pass over-delivers.
//!
//! `savedIns` / `savedOuts` are `flat_multiset`s: `sum` folds them in
//! ASCENDING order, which is where an IOU strand's rounding comes from —
//! the port sorts before it folds.
//!
//! The AMM half (`ammLiquidity_`, `getAMMOffer`, `AMMOffer`) lands with
//! slice 3; here `amm_offer` is a hook that returns None, so every book is
//! CLOB-only until then.
use xrpl_core::types::Hash256;

use super::amm::{AmmLiquidity, SharedAmmContext};
use super::amounts::{EitherAmount, IouAmount};
use super::offer_stream::{BookTip, FlowOfferStream, Offer, StepCounter};
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::{Quality, QualityFunction};
use super::steps::{
    check_near, redeems, Asset, Book, DebtDirection, FlowError, OffersToRemove, Step, StrandContext, StrandDirection,
};
use super::view::{account_send, is_authorized, mul_ratio_either, transfer_rate, QUALITY_ONE};

/// `OfferType`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OfferType {
    Amm,
    Clob,
}

/// What kind of BookStep this is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BookKind {
    /// `BookPaymentStep`.
    Payment,
    /// `BookOfferCrossingStep { defaultPath_, qualityThreshold_ }`.
    OfferCrossing { default_path: bool, quality_threshold: Quality },
}

/// `TAmounts<TIn, TOut>` as the step passes them around.
#[derive(Clone, Copy, Debug)]
pub struct StepAmounts {
    pub input: EitherAmount,
    pub output: EitherAmount,
}

/// The callback `forEachOffer` invokes per offer:
/// `(offer, ofrAmt, stpAmt, ownerGives, ofrInRate, ofrOutRate) -> continue?`
pub type OfferCallback<'c> = dyn FnMut(&mut PaymentSandbox, &mut Offer, StepAmounts, StepAmounts, EitherAmount, u32, u32) -> Result<bool, FlowError> + 'c;

pub struct BookStep {
    pub book: Book,
    kind: BookKind,
    strand_src: [u8; 20],
    strand_dst: [u8; 20],
    /// `prevStep_->directStepSrcAcct()` and `prevStep_->bookStepBook()`
    /// are the only two things this step asks its predecessor; they are
    /// captured at construction (the strand is immutable once built).
    prev_direct_src: Option<[u8; 20]>,
    prev_is_book: bool,
    /// `prevStep_->debtDirection(sb, dir)` for each pass, captured too.
    prev_debt_dir: Option<fn(StrandDirection) -> DebtDirection>,
    prev_dir_reverse: DebtDirection,
    prev_dir_forward: DebtDirection,
    owner_pays_transfer_fee: bool,
    max_offers_to_consume: u32,
    inactive: bool,
    offers_used: u32,
    cache: Option<(EitherAmount, EitherAmount)>,
    /// `ammLiquidity_`: set when the pair has a pool with LP tokens.
    amm: Option<AmmLiquidity>,
    amm_ctx: SharedAmmContext,
}

impl BookStep {
    /// `BookStep(ctx, in, out)` for a payment or a crossing. `prev` carries
    /// what `ctx.prevStep` would answer.
    pub fn new(ctx: &StrandContext, input: Asset, output: Asset, prev: Option<&dyn Step>, sb: &PaymentSandbox, amm_ctx: &SharedAmmContext) -> BookStep {
        let kind = match ctx.offer_crossing {
            super::steps::OfferCrossing::No => BookKind::Payment,
            _ => BookKind::OfferCrossing {
                default_path: ctx.is_default_path,
                quality_threshold: ctx.limit_quality.expect("Offer requires quality."),
            },
        };
        let (prev_direct_src, prev_is_book, prev_dir_reverse, prev_dir_forward) = match prev {
            Some(p) => (
                p.direct_step_src_acct(),
                p.book_step_book().is_some(),
                p.debt_direction(sb, StrandDirection::Reverse),
                p.debt_direction(sb, StrandDirection::Forward),
            ),
            None => (None, false, DebtDirection::Issues, DebtDirection::Issues),
        };
        BookStep {
            book: Book { input, output, domain: ctx.domain },
            kind,
            strand_src: ctx.strand_src,
            strand_dst: ctx.strand_dst,
            prev_direct_src,
            prev_is_book,
            prev_debt_dir: None,
            prev_dir_reverse,
            prev_dir_forward,
            owner_pays_transfer_fee: ctx.owner_pays_transfer_fee,
            // fix1515 is live: 1000.
            max_offers_to_consume: 1000,
            inactive: false,
            offers_used: 0,
            cache: None,
            amm: AmmLiquidity::discover(sb.sandbox(), amm_ctx, &input, &output),
            amm_ctx: amm_ctx.clone(),
        }
    }

    fn amm_multi_path(&self) -> bool {
        self.amm.is_some() && self.amm_ctx.borrow().multi_path()
    }

    fn in_is_xrp(&self) -> bool {
        self.book.input.is_xrp()
    }
    fn out_is_xrp(&self) -> bool {
        self.book.output.is_xrp()
    }

    /// The `rate` lambda: parity for XRP and for the strand's destination,
    /// else the issuer's TransferRate.
    fn rate(&self, sb: &PaymentSandbox, issuer: Option<[u8; 20]>) -> u32 {
        match issuer {
            None => QUALITY_ONE,
            Some(id) if id == self.strand_dst => QUALITY_ONE,
            Some(id) => transfer_rate(sb.sandbox(), &id),
        }
    }

    // ---- the TDerived hooks --------------------------------------------

    /// `limitSelfCrossQuality`: a crossing on the default path removes the
    /// taker's own offer at or above the limit quality.
    fn limit_self_cross_quality(&self, offer: &Offer, ofr_q: &mut Option<Quality>, offers: &mut FlowOfferStream, offer_attempted: bool) -> bool {
        let BookKind::OfferCrossing { default_path, quality_threshold } = self.kind else { return false };
        // `offer.quality() >= qualityThreshold_`: a LOWER encoded value is
        // a better quality.
        if default_path && offer.quality.0 <= quality_threshold.0 && self.strand_src == offer.owner && self.strand_dst == offer.owner {
            if let Some(key) = offer.key {
                offers.perm_rm_offer_pub(key);
            }
            if !offer_attempted {
                *ofr_q = None;
            }
            return true;
        }
        false
    }

    /// `checkQualityThreshold`: a payment looks at any quality; a crossing
    /// on the default path prunes worse than its limit.
    fn check_quality_threshold(&self, quality: Quality) -> bool {
        match self.kind {
            BookKind::Payment => true,
            BookKind::OfferCrossing { default_path, quality_threshold } => !default_path || quality.0 <= quality_threshold.0,
        }
    }

    /// `qualityThreshold(lobQuality)` for the AMM offer generator.
    #[allow(dead_code)]
    fn quality_threshold(&self, lob_quality: Quality, amm_multi_path: bool) -> Option<Quality> {
        match self.kind {
            BookKind::Payment => Some(lob_quality),
            BookKind::OfferCrossing { quality_threshold, .. } => {
                if !amm_multi_path && quality_threshold.0 < lob_quality.0 {
                    None
                } else {
                    Some(lob_quality)
                }
            }
        }
    }

    /// `getOfrInRate`: a crossing whose previous step is the taker's own
    /// DirectStep pays no in-fee on the taker's offer.
    fn ofr_in_rate(&self, owner: &[u8; 20], tr_in: u32) -> u32 {
        match self.kind {
            BookKind::Payment => tr_in,
            BookKind::OfferCrossing { .. } => {
                if self.prev_direct_src == Some(*owner) { QUALITY_ONE } else { tr_in }
            }
        }
    }

    /// `getOfrOutRate`: a crossing whose previous step is a book and whose
    /// offer owner is the strand's destination pays no out-fee.
    fn ofr_out_rate(&self, owner: &[u8; 20], tr_out: u32) -> u32 {
        match self.kind {
            BookKind::Payment => tr_out,
            BookKind::OfferCrossing { .. } => {
                if self.prev_is_book && *owner == self.strand_dst { QUALITY_ONE } else { tr_out }
            }
        }
    }

    /// `adjustQualityWithFees`: a payment composes the in/out transfer
    /// rates into the tip's quality; a crossing leaves a CLOB (or
    /// multi-path AMM) quality alone and composes only the in rate for a
    /// single-path AMM offer.
    fn adjust_quality_with_fees(&self, sb: &PaymentSandbox, ofr_q: Quality, prev_dir: DebtDirection, waive: bool, offer_type: OfferType, amm_multi_path: bool) -> Quality {
        let tr_in = if redeems(prev_dir) { self.rate(sb, self.book.input.issuer) } else { QUALITY_ONE };
        let (tr_out, apply) = match self.kind {
            BookKind::Payment => (if self.owner_pays_transfer_fee && !waive { self.rate(sb, self.book.output.issuer) } else { QUALITY_ONE }, true),
            BookKind::OfferCrossing { .. } => {
                if offer_type == OfferType::Clob || amm_multi_path {
                    return ofr_q;
                }
                (QUALITY_ONE, true)
            }
        };
        if !apply {
            return ofr_q;
        }
        // `Quality q1{getRate(STAmount(trOut), STAmount(trIn))}` then
        // `composed_quality(q1, ofrQ)` = mulRound(q1.rate, ofrQ.rate, up).
        let q1 = crate::ledger::keylet::rate_encode_native(tr_in as u128, 0, false, tr_out as u128, 0, false).map(Quality);
        match q1 {
            Some(q1) => composed_quality(q1, ofr_q),
            None => ofr_q,
        }
    }

    // ---- the AMM hook (slice 3) ----------------------------------------

    /// `getAMMOffer(view, clobQuality)`.
    fn amm_offer(&self, sb: &PaymentSandbox, clob_quality: Option<Quality>) -> Option<Offer> {
        self.amm.as_ref().and_then(|l| l.get_offer(sb.sandbox(), clob_quality)).map(Offer::from_amm)
    }

    /// `tip(view)`: the raw BookTip quality, or the AMM offer when it is
    /// strictly better (slice 3).
    fn tip(&self, sb: &PaymentSandbox) -> Option<(Quality, OfferType)> {
        let mut bt = BookTip::new(&self.book.input, &self.book.output, self.book.domain.as_ref());
        let lob_quality = if bt.peek(sb) { bt.quality() } else { None };
        if let Some(amm) = self.amm_offer(sb, lob_quality) {
            if lob_quality.is_none_or(|lq| amm.quality.0 < lq.0) {
                return Some((amm.quality, OfferType::Amm));
            }
        }
        lob_quality.map(|q| (q, OfferType::Clob))
    }

    // ---- forEachOffer ------------------------------------------------------

    /// `forEachOffer(sb, afView, prevStepDir, callback)` → (permToRemove,
    /// steps counted).
    pub fn for_each_offer(&self, sb: &mut PaymentSandbox, prev_step_dir: DebtDirection, callback: &mut OfferCallback<'_>) -> Result<(Vec<Hash256>, u32), FlowError> {
        let tr_in = if redeems(prev_step_dir) { self.rate(sb, self.book.input.issuer) } else { QUALITY_ONE };
        let tr_out = if self.owner_pays_transfer_fee { self.rate(sb, self.book.output.issuer) } else { QUALITY_ONE };
        let mut counter = StepCounter::new(self.max_offers_to_consume);
        let expire = sb.sandbox().base().header.close_time as u64;
        let mut offers = FlowOfferStream::new(self.book.input, self.book.output, self.book.domain.as_ref(), expire);
        let mut offer_attempted = false;
        let mut ofr_q: Option<Quality> = None;

        // `execOffer`, written as a closure over the same state.
        let mut exec_offer = |this: &BookStep, sb: &mut PaymentSandbox, offers: &mut FlowOfferStream, offer: &mut Offer, offer_attempted: &mut bool, ofr_q: &mut Option<Quality>, callback: &mut OfferCallback<'_>| -> Result<bool, FlowError> {
            match ofr_q {
                None => *ofr_q = Some(offer.quality),
                Some(q) if *q != offer.quality => return Ok(false),
                _ => {}
            }
            if this.limit_self_cross_quality(offer, ofr_q, offers, *offer_attempted) {
                return Ok(true);
            }
            // Owner authorisation to hold the IN asset from its issuer.
            if !offer.asset_in.is_xrp() && Some(offer.owner) != offer.asset_in.issuer {
                if !is_authorized(sb.base_view(), &offer.asset_in, &offer.owner) {
                    if let Some(key) = offer.key {
                        offers.perm_rm_offer_pub(key);
                    }
                    if !*offer_attempted {
                        *ofr_q = None;
                    }
                    return Ok(true);
                }
            }
            if !this.check_quality_threshold(offer.quality) {
                return Ok(false);
            }
            let (ofr_in_rate, ofr_out_rate) = offer.adjust_rates(this.ofr_in_rate(&offer.owner, tr_in), this.ofr_out_rate(&offer.owner, tr_out));
            let mut ofr_amt = StepAmounts { input: offer.amount_in, output: offer.amount_out };
            let mut stp_amt = StepAmounts { input: mul_ratio_either(ofr_amt.input, ofr_in_rate, QUALITY_ONE, true), output: ofr_amt.output };
            // The owner pays the transfer fee.
            let mut owner_gives = mul_ratio_either(ofr_amt.output, ofr_out_rate, QUALITY_ONE, false);
            let funds = if offer.is_funded() { owner_gives } else { offers.owner_funds().unwrap_or(owner_gives) };
            if funds.lt(&owner_gives) {
                owner_gives = funds;
                stp_amt.output = mul_ratio_either(owner_gives, QUALITY_ONE, ofr_out_rate, false);
                let (i, o) = offer.limit_out(ofr_amt.input, ofr_amt.output, stp_amt.output, false);
                ofr_amt = StepAmounts { input: i, output: o };
                stp_amt.input = mul_ratio_either(ofr_amt.input, ofr_in_rate, QUALITY_ONE, true);
            }
            *offer_attempted = true;
            callback(sb, offer, ofr_amt, stp_amt, owner_gives, ofr_in_rate, ofr_out_rate)
        };

        // `tryAMM(lobQuality)`: slice 3 fills `amm_offer`; a domain book
        // never has one.
        let try_amm = |this: &BookStep, sb: &mut PaymentSandbox, offers: &mut FlowOfferStream, lob_quality: Option<Quality>, offer_attempted: &mut bool, ofr_q: &mut Option<Quality>, callback: &mut OfferCallback<'_>| -> Result<bool, FlowError> {
            if this.book.domain.is_some() {
                return Ok(true);
            }
            let quality_threshold = lob_quality.and_then(|lq| this.quality_threshold(lq, this.amm_multi_path()));
            match this.amm_offer(sb, quality_threshold) {
                None => Ok(true),
                Some(mut amm) => exec_offer(this, sb, offers, &mut amm, offer_attempted, ofr_q, callback),
            }
        };

        let stepped = offers.step(sb, &mut counter);
        if std::env::var("XRPL_FLOW_TRACE").is_ok() {
            eprintln!(
                "FLOW   book {}: amm={} tip={} perm_rm={}",
                self.log_string(),
                self.amm.is_some(),
                offers.tip().map(|o| format!("{} q={:x} in={} out={}", hex::encode(&o.key.map(|k| k.0).unwrap_or([0; 32])[..6]), o.quality.0, o.amount_in, o.amount_out)).unwrap_or_else(|| "none".into()),
                offers.perm_to_remove().len()
            );
        }
        if stepped {
            let tip_q = offers.tip().map(|o| o.quality);
            if try_amm(self, sb, &mut offers, tip_q, &mut offer_attempted, &mut ofr_q, callback)? {
                loop {
                    let mut offer = offers.tip().cloned().expect("stream sits on an offer");
                    let cont = exec_offer(self, sb, &mut offers, &mut offer, &mut offer_attempted, &mut ofr_q, callback)?;
                    // The callback may have consumed the offer: carry its
                    // amounts back into the stream's copy.
                    if let Some(t) = offers.tip_mut() {
                        t.amount_in = offer.amount_in;
                        t.amount_out = offer.amount_out;
                    }
                    if !cont {
                        break;
                    }
                    if !offers.step(sb, &mut counter) {
                        break;
                    }
                }
            }
        } else {
            try_amm(self, sb, &mut offers, None, &mut offer_attempted, &mut ofr_q, callback)?;
        }
        Ok((offers.perm_to_remove().to_vec(), counter.count()))
    }

    /// `consumeOffer`: the owner receives `ofrAmt.in` from the IN issuer
    /// and pays `ownerGives` to the OUT issuer; the offer is reduced.
    fn consume_offer(&self, sb: &mut PaymentSandbox, offer: &mut Offer, ofr_amt: StepAmounts, _stp_amt: StepAmounts, owner_gives: EitherAmount) -> Result<(), FlowError> {
        let in_issuer = self.book.input.issuer.unwrap_or([0; 20]);
        let out_issuer = self.book.output.issuer.unwrap_or([0; 20]);
        if !offer.check_invariant(ofr_amt.input, ofr_amt.output) {
            // fixAMMOverflowOffer: tecINVARIANT_FAILED.
            return Err(FlowError(crate::ledger::transactor::TxResult::InvariantFailed));
        }
        account_send(sb, &in_issuer, &offer.owner, &self.book.input, ofr_amt.input).map_err(FlowError)?;
        account_send(sb, &offer.owner, &out_issuer, &self.book.output, owner_gives).map_err(FlowError)?;
        offer.consume(sb, Some(&self.amm_ctx), ofr_amt.input, ofr_amt.output);
        Ok(())
    }

    /// `revImp(sb, afView, ofrsToRm, out)`.
    pub fn rev_imp(&mut self, sb: &mut PaymentSandbox, ofrs_to_rm: &mut OffersToRemove, out: EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        self.cache = None;
        let out_xrp = self.out_is_xrp();
        let in_xrp = self.in_is_xrp();
        let mut result = (EitherAmount::zero(in_xrp), EitherAmount::zero(out_xrp));
        let mut remaining_out = out;
        let mut saved_ins: Vec<EitherAmount> = Vec::new();
        let mut saved_outs: Vec<EitherAmount> = Vec::new();
        let this: *const BookStep = self;
        let mut each_offer = |sb: &mut PaymentSandbox, offer: &mut Offer, ofr_amt: StepAmounts, stp_amt: StepAmounts, owner_gives: EitherAmount, tr_in: u32, tr_out: u32| -> Result<bool, FlowError> {
            // SAFETY: `self` outlives the closure and is not mutated while
            // the callback runs (only `cache`/`offers_used` after).
            let this = unsafe { &*this };
            if remaining_out.signum() <= 0 {
                return Ok(false);
            }
            if stp_amt.output.le(&remaining_out) {
                saved_ins.push(stp_amt.input);
                saved_outs.push(stp_amt.output);
                result = (sum(&saved_ins, in_xrp), sum(&saved_outs, out_xrp));
                remaining_out = out.sub(result.1);
                this.consume_offer(sb, offer, ofr_amt, stp_amt, owner_gives)?;
                // Even if the payment is satisfied, we need to consume the
                // offer.
                Ok(true)
            } else {
                let (ofr_adj, stp_adj, owner_gives_adj) = limit_step_out(offer, ofr_amt, stp_amt, owner_gives, tr_in, tr_out, remaining_out);
                remaining_out = EitherAmount::zero(out_xrp);
                saved_ins.push(stp_adj.input);
                saved_outs.push(remaining_out);
                result = (sum(&saved_ins, in_xrp), out);
                this.consume_offer(sb, offer, ofr_adj, stp_adj, owner_gives_adj)?;
                // Given stpAmt.out > remainingOut the offer is usually still
                // funded — but two IOU mantissas within ten of each other
                // subtract to zero.
                Ok(offer.fully_consumed())
            }
        };
        let prev_dir = self.prev_dir_reverse;
        let (to_rm, consumed) = self.for_each_offer(sb, prev_dir, &mut each_offer)?;
        self.offers_used = consumed;
        ofrs_to_rm.extend(to_rm.into_iter().map(|k| k.0));
        if consumed >= self.max_offers_to_consume {
            // fix1515: use the liquidity, mark the strand inactive.
            self.inactive = true;
        }
        match remaining_out.signum() {
            -1 => {
                self.cache = Some((EitherAmount::zero(in_xrp), EitherAmount::zero(out_xrp)));
                return Ok((EitherAmount::zero(in_xrp), EitherAmount::zero(out_xrp)));
            }
            0 => {
                // Normalisation can zero remainingOut without result.out ==
                // out; force it.
                result.1 = out;
            }
            _ => {}
        }
        self.cache = Some(result);
        Ok(result)
    }

    /// `fwdImp(sb, afView, ofrsToRm, in)`.
    pub fn fwd_imp(&mut self, sb: &mut PaymentSandbox, ofrs_to_rm: &mut OffersToRemove, input: EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        let cache = self.cache.expect("fwdImp: cache is set");
        let out_xrp = self.out_is_xrp();
        let in_xrp = self.in_is_xrp();
        let mut result = (EitherAmount::zero(in_xrp), EitherAmount::zero(out_xrp));
        let mut remaining_in = input;
        let mut saved_ins: Vec<EitherAmount> = Vec::new();
        let mut saved_outs: Vec<EitherAmount> = Vec::new();
        let this: *const BookStep = self;
        let mut each_offer = |sb: &mut PaymentSandbox, offer: &mut Offer, ofr_amt: StepAmounts, stp_amt: StepAmounts, owner_gives: EitherAmount, tr_in: u32, tr_out: u32| -> Result<bool, FlowError> {
            let this = unsafe { &*this };
            if remaining_in.signum() <= 0 {
                return Ok(false);
            }
            let process_more;
            let mut ofr_adj = ofr_amt;
            let mut stp_adj = stp_amt;
            let mut owner_gives_adj = owner_gives;
            let last_out_idx;
            if stp_amt.input.le(&remaining_in) {
                saved_ins.push(stp_amt.input);
                saved_outs.push(stp_amt.output);
                last_out_idx = saved_outs.len() - 1;
                result = (sum(&saved_ins, in_xrp), sum(&saved_outs, out_xrp));
                // Consume the offer even if stepAmt.in == remainingIn.
                process_more = true;
            } else {
                let (a, s, g) = limit_step_in(offer, ofr_amt, stp_amt, owner_gives, tr_in, tr_out, remaining_in);
                ofr_adj = a;
                stp_adj = s;
                owner_gives_adj = g;
                saved_ins.push(remaining_in);
                saved_outs.push(stp_adj.output);
                last_out_idx = saved_outs.len() - 1;
                result.1 = sum(&saved_outs, out_xrp);
                result.0 = input;
                process_more = false;
            }
            if result.1.gt(&cache.1) && result.0.le(&cache.0) {
                // The forward pass produced more output than the reverse
                // pass for the same (or less) input: re-derive the input the
                // reverse output needs and, when it equals what is left,
                // deliver exactly the cached output.
                let last_out_amt = saved_outs.remove(last_out_idx);
                let remaining_out = cache.1.sub(sum(&saved_outs, out_xrp));
                let (ofr_rev, stp_rev, gives_rev) = limit_step_out(offer, ofr_amt, stp_amt, owner_gives, tr_in, tr_out, remaining_out);
                if stp_rev.input == remaining_in {
                    result = (input, cache.1);
                    saved_ins.clear();
                    saved_ins.push(result.0);
                    saved_outs.clear();
                    saved_outs.push(result.1);
                    ofr_adj = ofr_rev;
                    stp_adj = StepAmounts { input: remaining_in, output: remaining_out };
                    owner_gives_adj = gives_rev;
                } else {
                    saved_outs.push(last_out_amt);
                }
            }
            remaining_in = input.sub(result.0);
            this.consume_offer(sb, offer, ofr_adj, stp_adj, owner_gives_adj)?;
            // Two IOU mantissas within ten of each other subtract to zero:
            // the offer may be spent even when stpAmt.in > remainingIn.
            Ok(process_more || offer.fully_consumed())
        };
        let prev_dir = self.prev_dir_forward;
        let (to_rm, consumed) = self.for_each_offer(sb, prev_dir, &mut each_offer)?;
        self.offers_used = consumed;
        ofrs_to_rm.extend(to_rm.into_iter().map(|k| k.0));
        if consumed >= self.max_offers_to_consume {
            self.inactive = true;
        }
        match remaining_in.signum() {
            -1 => {
                self.cache = Some((EitherAmount::zero(in_xrp), EitherAmount::zero(out_xrp)));
                return Ok((EitherAmount::zero(in_xrp), EitherAmount::zero(out_xrp)));
            }
            0 => {
                result.0 = input;
            }
            _ => {}
        }
        self.cache = Some(result);
        Ok(result)
    }

    /// `check(ctx)`, the part that is the step's own: a book whose in and
    /// out are the same issue is `temBAD_PATH` (false). Loops, issuer
    /// existence and the previous DirectStep's line/NoRipple tests are
    /// the strand builder's (slice 4).
    pub fn check(&self) -> bool {
        self.book.input != self.book.output
    }
}

/// `sum(col)` over a `flat_multiset`: ascending order, `accumulate` from
/// the second element onto the first.
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

/// `limitStepIn(offer, ofrAmt, stpAmt, ownerGives, trIn, trOut, limit)`.
pub fn limit_step_in(offer: &Offer, mut ofr_amt: StepAmounts, mut stp_amt: StepAmounts, mut owner_gives: EitherAmount, tr_in: u32, tr_out: u32, limit: EitherAmount) -> (StepAmounts, StepAmounts, EitherAmount) {
    if limit.lt(&stp_amt.input) {
        stp_amt.input = limit;
        let in_lmt = mul_ratio_either(stp_amt.input, QUALITY_ONE, tr_in, false);
        // fixReducedOffersV2 is NOT on mainnet: the legacy `ceil_in`
        // (`offer.limitIn`; the pool's offer re-prices).
        let (i, o) = offer.limit_in(ofr_amt.input, ofr_amt.output, in_lmt);
        ofr_amt = StepAmounts { input: i, output: o };
        stp_amt.output = ofr_amt.output;
        owner_gives = mul_ratio_either(ofr_amt.output, tr_out, QUALITY_ONE, false);
    }
    (ofr_amt, stp_amt, owner_gives)
}

/// `limitStepOut(offer, ofrAmt, stpAmt, ownerGives, trIn, trOut, limit)`.
pub fn limit_step_out(offer: &Offer, mut ofr_amt: StepAmounts, mut stp_amt: StepAmounts, mut owner_gives: EitherAmount, tr_in: u32, tr_out: u32, limit: EitherAmount) -> (StepAmounts, StepAmounts, EitherAmount) {
    if limit.lt(&stp_amt.output) {
        stp_amt.output = limit;
        owner_gives = mul_ratio_either(stp_amt.output, tr_out, QUALITY_ONE, false);
        // fixReducedOffersV1 is live: `ceil_out_strict(.., roundUp = true)`
        // (`offer.limitOut`; the pool's offer re-prices).
        let (i, o) = offer.limit_out(ofr_amt.input, ofr_amt.output, stp_amt.output, true);
        ofr_amt = StepAmounts { input: i, output: o };
        stp_amt.input = mul_ratio_either(ofr_amt.input, tr_in, QUALITY_ONE, true);
    }
    (ofr_amt, stp_amt, owner_gives)
}

/// `composed_quality(lhs, rhs)`: `mulRound(lhs.rate, rhs.rate, up)` encoded.
pub fn composed_quality(lhs: Quality, rhs: Quality) -> Quality {
    let l = super::st_amount::rate_amount(lhs.0);
    let r = super::st_amount::rate_amount(rhs.0);
    match super::st_amount::mul_round(&l, &r, false, true) {
        Ok(p) if p.mantissa != 0 => {
            let stored_exponent = (p.exponent + 100) as u64;
            Quality((stored_exponent << 56) | p.mantissa)
        }
        _ => rhs,
    }
}

impl Step for BookStep {
    fn rev(&mut self, sb: &mut PaymentSandbox, ofrs_to_rm: &mut OffersToRemove, out: &EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        self.rev_imp(sb, ofrs_to_rm, *out)
    }

    fn fwd(&mut self, sb: &mut PaymentSandbox, ofrs_to_rm: &mut OffersToRemove, input: &EitherAmount) -> Result<(EitherAmount, EitherAmount), FlowError> {
        self.fwd_imp(sb, ofrs_to_rm, *input)
    }

    fn cached_in(&self) -> Option<EitherAmount> {
        self.cache.map(|c| c.0)
    }
    fn cached_out(&self) -> Option<EitherAmount> {
        self.cache.map(|c| c.1)
    }

    fn debt_direction(&self, _sb: &PaymentSandbox, _dir: StrandDirection) -> DebtDirection {
        if self.owner_pays_transfer_fee { DebtDirection::Issues } else { DebtDirection::Redeems }
    }

    fn quality_upper_bound(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (Option<Quality>, DebtDirection) {
        let dir = self.debt_direction(sb, StrandDirection::Forward);
        let Some((q, offer_type)) = self.tip(sb) else { return (None, dir) };
        let waive = offer_type == OfferType::Amm;
        (Some(self.adjust_quality_with_fees(sb, q, prev_step_dir, waive, offer_type, self.amm_multi_path())), dir)
    }

    /// `getQualityFunc`: `tipOfferQualityF` — a CLOB tip's constant
    /// function, or the pool offer's (sloped under single path), with the
    /// fee-adjusted parity quality composed in front of an AMM function.
    fn get_quality_func(&self, sb: &PaymentSandbox, prev_step_dir: DebtDirection) -> (Option<QualityFunction>, DebtDirection) {
        let dir = self.debt_direction(sb, StrandDirection::Forward);
        let mut bt = BookTip::new(&self.book.input, &self.book.output, self.book.domain.as_ref());
        let lob_quality = if bt.peek(sb) { bt.quality() } else { None };
        let amm = self.amm_offer(sb, lob_quality).filter(|a| lob_quality.is_none_or(|lq| a.quality.0 < lq.0));
        match amm {
            Some(a) => {
                let Some(res) = a.amm.as_ref().and_then(|x| x.get_quality_func()) else { return (None, dir) };
                if res.is_const() {
                    return (Some(res), dir);
                }
                // `adjustQualityWithFees(qOne, …, WaiveTransferFee::Yes, AMM)`
                let q_one = Quality(crate::ledger::keylet::rate_encode_native(1, 0, false, 1, 0, false).unwrap_or(0));
                let q = self.adjust_quality_with_fees(sb, q_one, prev_step_dir, true, OfferType::Amm, self.amm_multi_path());
                if q == q_one {
                    return (Some(res), dir);
                }
                let mut qf = match QualityFunction::clob_like(q) { Ok(f) => f, Err(_) => return (None, dir) };
                if qf.combine(&res).is_err() {
                    return (None, dir);
                }
                (Some(qf), dir)
            }
            None => {
                let Some(q) = lob_quality else { return (None, dir) };
                let q = self.adjust_quality_with_fees(sb, q, prev_step_dir, false, OfferType::Clob, self.amm_multi_path());
                (QualityFunction::clob_like(q).ok(), dir)
            }
        }
    }

    fn offers_used(&self) -> u32 {
        self.offers_used
    }

    fn book_step_book(&self) -> Option<Book> {
        Some(self.book)
    }

    fn inactive(&self) -> bool {
        self.inactive
    }

    fn valid_fwd(&mut self, sb: &mut PaymentSandbox, input: &EitherAmount) -> Result<(bool, EitherAmount), FlowError> {
        let Some(sav) = self.cache else {
            return Ok((false, EitherAmount::zero(self.out_is_xrp())));
        };
        let mut dummy = OffersToRemove::new();
        match self.fwd_imp(sb, &mut dummy, *input) {
            Ok(_) => {}
            Err(_) => return Ok((false, EitherAmount::zero(self.out_is_xrp()))),
        }
        let cache = self.cache.expect("fwdImp sets the cache");
        if !(check_near(&sav.0, &cache.0) && check_near(&sav.1, &cache.1)) {
            return Ok((false, cache.1));
        }
        Ok((true, cache.1))
    }

    fn equal(&self, other: &dyn Step) -> bool {
        other.book_step_book().is_some_and(|b| b == self.book)
    }

    fn log_string(&self) -> String {
        format!(
            "{}: in {}/{} out {}/{}",
            match self.kind {
                BookKind::Payment => "BookPaymentStep",
                BookKind::OfferCrossing { .. } => "BookOfferCrossingStep",
            },
            hex::encode(self.book.input.currency),
            self.book.input.issuer.map(hex::encode).unwrap_or_else(|| "XRP".into()),
            hex::encode(self.book.output.currency),
            self.book.output.issuer.map(hex::encode).unwrap_or_else(|| "XRP".into()),
        )
    }
}

#[allow(dead_code)]
fn _unused(_: IouAmount) {}
