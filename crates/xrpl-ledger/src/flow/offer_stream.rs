//! rippled `BookTip` (`src/xrpld/app/tx/detail/BookTip.cpp`) and
//! `TOfferStreamBase` / `FlowOfferStream` (`OfferStream.cpp`): the cursor a
//! `BookStep` walks a book with, and the stream that steps it past every
//! offer that cannot trade — missing, expired, empty, deep-frozen, out of
//! its domain, unfunded, or tiny at a worse quality than filed — removing
//! the ones rippled removes and recording, per `FlowOfferStream`, which
//! removals are permanent (`permToRemove_`).
//!
//! Two facts the engine in `tx::offer` had to learn per finding are the
//! structure here:
//!
//!   * `BookTip::step` reads the FIRST directory entry as it stands, with no
//!     test at all (findings 233, 288): that raw quality is what
//!     `BookStep::tip` prices admission on.
//!   * the stream distinguishes an offer FOUND unfunded (funds zero in the
//!     `cancelView`, the view as the flow opened — `permRmOffer`, kept even
//!     when the strand is discarded) from one that BECAME unfunded inside
//!     this flow (stepped past, deleted only in the strand's sandbox, gone
//!     with a rejected strand) — findings 153, 275, 277.
use crate::ledger::keylet;
use crate::tx::offer::{decode20, json_at, signed_value};
use xrpl_core::types::Hash256;

use super::amounts::{EitherAmount, IouAmount};
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::Quality;
use super::st_amount::{mul_round, mul_round_strict, StAmount};
use super::steps::Asset;
use super::view::{
    account_funds_iou, book_base, dir_first, is_deep_frozen, page_quality, quality_next, read_offer,
    succ_in_book, xrp_liquid, AuthHandling, FreezeHandling, OfferEntry,
};

/// `BookTip`: the cursor over a book's directory pages, best quality first.
pub struct BookTip {
    base: Hash256,
    /// `m_book`: the key the next `succ` starts strictly after.
    book: Hash256,
    /// `m_end`: `getQualityNext(base)`.
    end: Hash256,
    /// `m_dir`: the page the current entry was read from.
    dir: Option<Hash256>,
    /// `m_index`: the current offer's key.
    index: Option<Hash256>,
    /// `m_entry`: the current offer, None when the directory names a key
    /// that holds no offer.
    entry: Option<OfferEntry>,
    quality: Option<Quality>,
    valid: bool,
}

impl BookTip {
    /// `BookTip(view, book)`.
    pub fn new(input: &Asset, output: &Asset, domain: Option<&Hash256>) -> BookTip {
        let base = book_base(input, output, domain);
        BookTip { base, book: base, end: quality_next(&base), dir: None, index: None, entry: None, quality: None, valid: false }
    }

    pub fn dir(&self) -> Option<Hash256> {
        self.dir
    }
    pub fn index(&self) -> Option<Hash256> {
        self.index
    }
    pub fn entry(&self) -> Option<&OfferEntry> {
        self.entry.as_ref()
    }
    pub fn quality(&self) -> Option<Quality> {
        self.quality
    }

    /// A read-only `step` from a fresh cursor — what `BookStep::tip` does
    /// on its scratch `Sandbox`: the first entry of the first non-empty
    /// page, nothing deleted. Returns false for an empty book.
    pub fn peek(&mut self, ps: &PaymentSandbox) -> bool {
        loop {
            let Some(first_page) = succ_in_book(ps.sandbox(), &self.base, &self.book, &self.end) else {
                return false;
            };
            if let Some((page, index)) = dir_first(ps.sandbox(), &first_page) {
                self.dir = Some(page);
                self.index = Some(index);
                self.entry = read_offer(ps.sandbox(), &index);
                self.quality = Some(Quality(page_quality(&first_page)));
                self.book = first_page;
                dec_key(&mut self.book);
                return true;
            }
            self.book = first_page;
        }
    }

    /// `BookTip::step`: delete the offer the cursor sits on (it has been
    /// consumed or judged dead — "BookTip::step deletes the current offer
    /// from the view before advancing"), then move to the first entry of
    /// the next non-empty page. Returns false when the book is exhausted.
    pub fn step(&mut self, ps: &mut PaymentSandbox) -> bool {
        if self.valid {
            if let Some(key) = self.index.take() {
                if self.entry.is_some() {
                    offer_delete(ps, &key);
                }
            }
            self.entry = None;
        }
        loop {
            let Some(first_page) = succ_in_book(ps.sandbox(), &self.base, &self.book, &self.end) else {
                return false;
            };
            if let Some((page, index)) = dir_first(ps.sandbox(), &first_page) {
                self.dir = Some(page);
                self.index = Some(index);
                self.entry = read_offer(ps.sandbox(), &index);
                self.quality = Some(Quality(page_quality(&first_page)));
                self.valid = true;
                // Next query starts before this directory: the quality
                // immediately before the next quality (`--m_book`).
                self.book = first_page;
                dec_key(&mut self.book);
                return true;
            }
            // An empty directory: advance past it.
            self.book = first_page;
        }
    }
}

/// `--m_book` on a uint256: the key one below.
fn dec_key(k: &mut Hash256) {
    for b in k.0.iter_mut().rev() {
        if *b == 0 {
            *b = 0xFF;
        } else {
            *b -= 1;
            break;
        }
    }
}

/// `offerDelete(view, offer)`: remove the offer from its book page and its
/// owner's directory, and give the owner back one owner-count unit. The
/// engine's `delete_maker_offer` does exactly this over the sandbox.
pub fn offer_delete(ps: &mut PaymentSandbox, key: &Hash256) {
    let Some(offer) = json_at(ps.sandbox(), key) else { return };
    let Some(owner) = offer.get("Account").and_then(|v| v.as_str()).and_then(decode20) else { return };
    // `adjustOwnerCount` → `view.adjustOwnerCountHook(id, cur, next)`: the
    // PaymentSandbox remembers the HIGHER count, so the owner's reserve —
    // and with it `xrpLiquid` — holds at the flow's original count even
    // after this deletion (the maker's XRP funds do not grow mid-flow).
    let cur = super::view::owner_counts(ps.sandbox(), &owner);
    crate::tx::offer::delete_maker_offer(ps.view(), key, &offer, &owner);
    let next = super::view::owner_counts(ps.sandbox(), &owner);
    ps.adjust_owner_count_hook(&owner, cur, next);
}

/// `StepCounter`: the per-transaction budget of stream steps (1000 for a
/// payment flow, `flow()`'s `StepCounter counter(1000, j)`).
pub struct StepCounter {
    limit: u32,
    count: u32,
}

impl StepCounter {
    pub fn new(limit: u32) -> StepCounter {
        StepCounter { limit, count: 0 }
    }
    pub fn count(&self) -> u32 {
        self.count
    }
    /// `StepCounter::step`: false once the limit is reached.
    pub fn step(&mut self) -> bool {
        if self.count >= self.limit {
            return false;
        }
        self.count += 1;
        true
    }
}

/// `TOffer<TIn, TOut>` as the stream hands it to the step: the entry, its
/// FILED quality (the page's, never recomputed — "an important business
/// rule that maintains accuracy when an offer is partially filled"), and
/// its current amounts.
#[derive(Clone, Debug)]
pub struct Offer {
    /// The ledger entry (None for the pool's synthetic offer — `key()`
    /// is `std::nullopt` there, so `permRmOffer` never sees it).
    pub key: Option<Hash256>,
    pub owner: [u8; 20],
    pub quality: Quality,
    pub asset_in: Asset,
    pub asset_out: Asset,
    /// (in, out) = (TakerPays, TakerGets).
    pub amount_in: EitherAmount,
    pub amount_out: EitherAmount,
    /// `AMMOffer`: the pool offer's own state (slice 3).
    pub amm: Option<super::amm::AmmOffer>,
}

impl Offer {
    fn from_entry(e: &OfferEntry, quality: Quality) -> Offer {
        Offer {
            key: Some(e.key),
            owner: e.owner,
            quality,
            asset_in: e.asset_in,
            asset_out: e.asset_out,
            amount_in: either(e.asset_in.is_xrp(), e.taker_pays),
            amount_out: either(e.asset_out.is_xrp(), e.taker_gets),
            amm: None,
        }
    }

    /// The pool's offer as `execOffer` receives it.
    pub fn from_amm(a: super::amm::AmmOffer) -> Offer {
        Offer {
            key: None,
            owner: a.owner,
            quality: a.quality,
            asset_in: a.asset_in,
            asset_out: a.asset_out,
            amount_in: a.amount_in,
            amount_out: a.amount_out,
            amm: Some(a),
        }
    }

    /// `AMMOffer::adjustRates`: "AMM doesn't pay transfer fee on Payment
    /// tx" — the OUT rate is waived, the IN rate stays (AMMOffer.h:136-140);
    /// a CLOB offer keeps both.
    pub fn adjust_rates(&self, ofr_in_rate: u32, ofr_out_rate: u32) -> (u32, u32) {
        if self.amm.is_some() { (ofr_in_rate, super::view::QUALITY_ONE) } else { (ofr_in_rate, ofr_out_rate) }
    }

    /// `checkInvariant`: a CLOB offer always holds; the pool's product
    /// must not fall.
    pub fn check_invariant(&self, consumed_in: EitherAmount, consumed_out: EitherAmount) -> bool {
        match &self.amm {
            Some(a) => a.check_invariant(consumed_in, consumed_out),
            None => true,
        }
    }

    /// `isFunded`: the owner is the OUT issuer — unlimited funds; the
    /// pool's offer is always funded (its amounts are its balances).
    pub fn is_funded(&self) -> bool {
        self.amm.is_some() || self.asset_out.issuer == Some(self.owner)
    }

    /// `fully_consumed`: nothing more can flow through this offer.
    pub fn fully_consumed(&self) -> bool {
        self.amount_in.is_zero() || self.amount_out.is_zero() || self.amount_in.negative() || self.amount_out.negative()
    }

    /// `TOffer::consume`: subtract what a fill took and write the entry.
    /// `AMMOffer::consume`: nothing written (the pool moved with the
    /// transfers); the context learns the pool was used.
    pub fn consume(&mut self, ps: &mut PaymentSandbox, ctx: Option<&super::amm::SharedAmmContext>, consumed_in: EitherAmount, consumed_out: EitherAmount) {
        if let Some(a) = self.amm.as_mut() {
            if let Some(ctx) = ctx {
                a.consume(ctx, consumed_in, consumed_out);
            }
            return;
        }
        self.amount_in = self.amount_in.sub(consumed_in);
        self.amount_out = self.amount_out.sub(consumed_out);
        let Some(key) = self.key else { return };
        let Some(mut offer) = json_at(ps.sandbox(), &key) else { return };
        set_amount(&mut offer, "TakerPays", &self.amount_in);
        set_amount(&mut offer, "TakerGets", &self.amount_out);
        crate::tx::offer::put_json(ps.view(), key, &offer);
    }

    /// `TOffer::limitOut` (fixReducedOffersV1 on mainnet):
    /// `quality().ceil_out_strict(offrAmt, limit, roundUp)`; the pool's
    /// offer re-prices (`AMMOffer::limitOut`).
    pub fn limit_out(&self, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount, round_up: bool) -> (EitherAmount, EitherAmount) {
        match &self.amm {
            Some(a) => a.limit_out(amt_in, amt_out, limit, round_up),
            None => ceil_out_strict(self.quality, amt_in, amt_out, limit, round_up),
        }
    }

    /// `TOffer::limitIn` — fixReducedOffersV2 IS live on mainnet (feature
    /// RPC, 2026-09-15): `quality().ceil_in_strict(offrAmt, limit, roundUp)`;
    /// the pool's offer re-prices.
    pub fn limit_in(&self, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount, round_up: bool) -> (EitherAmount, EitherAmount) {
        match &self.amm {
            Some(a) => a.limit_in(amt_in, amt_out, limit, round_up),
            None => ceil_in_strict(self.quality, amt_in, amt_out, limit, round_up),
        }
    }
}

fn either(xrp: bool, v: (bool, (u128, i32))) -> EitherAmount {
    let (neg, (m, e)) = v;
    if xrp {
        let d = crate::tx::offer::me_rescale(m_e(m, e), 0, false) as i128;
        EitherAmount::Xrp(if neg { -d } else { d })
    } else {
        EitherAmount::Iou(IouAmount::from_me(neg, (m, e)))
    }
}

fn m_e(m: u128, e: i32) -> (u128, i32) {
    (m, e)
}

fn set_amount(offer: &mut serde_json::Value, field: &str, v: &EitherAmount) {
    match v {
        EitherAmount::Xrp(d) => {
            offer[field] = serde_json::Value::String(d.to_string());
        }
        EitherAmount::Iou(a) => {
            if let Some(obj) = offer.get_mut(field).and_then(|x| x.as_object_mut()) {
                obj.insert("value".to_string(), serde_json::Value::String(format!("{}{}", if a.negative && a.mantissa != 0 { "-" } else { "" }, crate::tx::offer::me_to_value_string((a.mantissa as u128, a.exponent)))));
            }
        }
    }
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

/// `Quality::ceil_out_impl<mulRoundStrict>`: when `amount.out > limit`,
/// `in = mulRoundStrict(limit, rate, in.asset, roundUp)` clamped to the
/// offer's in, out = limit.
pub fn ceil_out_strict(quality: Quality, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount, round_up: bool) -> (EitherAmount, EitherAmount) {
    if amt_out.gt(&limit) {
        let rate = super::st_amount::rate_amount(quality.0);
        let mut r_in = mul_round_strict(&st(limit), &rate, amt_in.is_xrp(), round_up).map(un_st).unwrap_or(amt_in);
        if r_in.gt(&amt_in) {
            r_in = amt_in;
        }
        return (r_in, limit);
    }
    (amt_in, amt_out)
}

/// `Quality::ceil_out` (legacy `mulRound`, roundUp = true).
pub fn ceil_out(quality: Quality, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount) -> (EitherAmount, EitherAmount) {
    if amt_out.gt(&limit) {
        let rate = super::st_amount::rate_amount(quality.0);
        let mut r_in = mul_round(&st(limit), &rate, amt_in.is_xrp(), true).map(un_st).unwrap_or(amt_in);
        if r_in.gt(&amt_in) {
            r_in = amt_in;
        }
        return (r_in, limit);
    }
    (amt_in, amt_out)
}

/// `Quality::ceil_in` (legacy `divRound`, roundUp = true): when
/// `amount.in > limit`, `out = divRound(limit, rate, out.asset, true)`
/// clamped to the offer's out, in = limit.
pub fn ceil_in(quality: Quality, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount) -> (EitherAmount, EitherAmount) {
    if amt_in.gt(&limit) {
        let rate = super::st_amount::rate_amount(quality.0);
        let mut r_out = super::st_amount::div_round(&st(limit), &rate, amt_out.is_xrp(), true).map(un_st).unwrap_or(amt_out);
        if r_out.gt(&amt_out) {
            r_out = amt_out;
        }
        return (limit, r_out);
    }
    (amt_in, amt_out)
}

/// `Quality::ceil_in_strict` (`divRoundStrict`).
pub fn ceil_in_strict(quality: Quality, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount, round_up: bool) -> (EitherAmount, EitherAmount) {
    if amt_in.gt(&limit) {
        let rate = super::st_amount::rate_amount(quality.0);
        let mut r_out = super::st_amount::div_round_strict(&st(limit), &rate, amt_out.is_xrp(), round_up).map(un_st).unwrap_or(amt_out);
        if r_out.gt(&amt_out) {
            r_out = amt_out;
        }
        return (limit, r_out);
    }
    (amt_in, amt_out)
}

/// `FlowOfferStream<TIn, TOut>`: the stream over one book inside a flow.
pub struct FlowOfferStream {
    tip: BookTip,
    book_in: Asset,
    book_out: Asset,
    /// `expire_`: the parent ledger's close time.
    expire: u64,
    /// `offer_`: the offer the stream sits on after a successful `step`.
    offer: Option<Offer>,
    /// `ownerFunds_`.
    owner_funds: Option<EitherAmount>,
    /// `permToRemove_`: keys removed for good, whatever the strand's fate.
    perm_to_remove: Vec<Hash256>,
}

impl FlowOfferStream {
    /// `FlowOfferStream(view, cancelView, book, when, counter, j)`.
    pub fn new(book_in: Asset, book_out: Asset, domain: Option<&Hash256>, expire: u64) -> FlowOfferStream {
        FlowOfferStream { tip: BookTip::new(&book_in, &book_out, domain), book_in, book_out, expire, offer: None, owner_funds: None, perm_to_remove: Vec::new() }
    }

    pub fn tip(&self) -> Option<&Offer> {
        self.offer.as_ref()
    }
    pub fn tip_mut(&mut self) -> Option<&mut Offer> {
        self.offer.as_mut()
    }
    pub fn owner_funds(&self) -> Option<EitherAmount> {
        self.owner_funds
    }
    /// `FlowOfferStream::permToRemove`.
    pub fn perm_to_remove(&self) -> &[Hash256] {
        &self.perm_to_remove
    }

    /// `permRmOffer` as `forEachOffer` calls it (self-cross, unauthorised
    /// owner).
    pub fn perm_rm_offer_pub(&mut self, key: Hash256) {
        self.perm_rm_offer(key);
    }

    /// `FlowOfferStream::permRmOffer`: recorded, not deleted here — the
    /// flow deletes the set once it is done, "even if the strand fails"
    /// (StrandFlow.h).
    fn perm_rm_offer(&mut self, key: Hash256) {
        if !self.perm_to_remove.contains(&key) {
            self.perm_to_remove.push(key);
        }
    }

    /// The owner's funds for this offer, read through `view` (the strand's
    /// sandbox — `ownerFunds_`) or the `cancelView` (the view as the flow
    /// opened — "original_funds"). `accountFundsHelper`: an IOU's issuer is
    /// self-funded for the offer's own amount; XRP is `xrpLiquid`.
    fn funds(&self, ps: &PaymentSandbox, offer: &Offer, original: bool) -> EitherAmount {
        match offer.amount_out {
            EitherAmount::Xrp(_) => {
                if original {
                    // The flow's opening view: xrpLiquid over the base view
                    // with no owner-count adjustment recorded.
                    EitherAmount::Xrp(xrp_liquid_original(ps, &offer.owner))
                } else {
                    EitherAmount::Xrp(xrp_liquid(ps, &offer.owner, 0))
                }
            }
            EitherAmount::Iou(amt) => {
                if original {
                    EitherAmount::Iou(account_funds_iou_original(ps, &offer.owner, &offer.asset_out, amt))
                } else {
                    EitherAmount::Iou(account_funds_iou(ps, &offer.owner, &offer.asset_out, amt, FreezeHandling::ZeroIfFrozen, AuthHandling::IgnoreAuth))
                }
            }
        }
    }

    /// `TOfferStreamBase::step` — "Modifying the order or logic of these
    /// operations causes a protocol breaking change."
    pub fn step(&mut self, ps: &mut PaymentSandbox, counter: &mut StepCounter) -> bool {
        loop {
            self.owner_funds = None;
            // BookTip::step deletes the current offer from the view before
            // advancing to the next (unless the ledger entry is missing).
            if !self.tip.step(ps) {
                return false;
            }
            if !counter.step() {
                return false;
            }
            let Some(index) = self.tip.index() else { return false };
            // Remove if missing: the directory names an offer that is not
            // there — erase the directory entry (`erase(view_)`,
            // `erase(cancelView_)`).
            let Some(entry) = self.tip.entry().cloned() else {
                if std::env::var("XRPL_FLOW_TRACE").is_ok() {
                    eprintln!("FLOW   stream: {} missing (directory names no readable offer) — erasing the entry", hex::encode(&index.0[..6]));
                }
                if let Some(dir) = self.tip.dir() {
                    erase_dir_entry(ps, &dir, &index);
                }
                continue;
            };
            let tr = std::env::var("XRPL_FLOW_TRACE").is_ok();
            // Remove if expired: `Expiration <= parentCloseTime`.
            if let Some(exp) = entry.expiration {
                if exp <= self.expire {
                    if tr { eprintln!("FLOW   stream: {} expired ({exp} <= {})", hex::encode(&index.0[..6]), self.expire); }
                    self.perm_rm_offer(index);
                    continue;
                }
            }
            let Some(quality) = self.tip.quality() else { return false };
            let offer = Offer::from_entry(&entry, quality);
            // Remove if either amount is zero ("Removing bad offer").
            if offer.amount_in.is_zero() || offer.amount_out.is_zero() {
                if tr { eprintln!("FLOW   stream: {} bad offer (zero amount)", hex::encode(&index.0[..6])); }
                self.perm_rm_offer(index);
                self.offer = None;
                continue;
            }
            // Deep-frozen owner on the IN side: removed.
            if is_deep_frozen(ps.sandbox(), &offer.owner, &offer.asset_in) {
                if tr { eprintln!("FLOW   stream: {} deep frozen", hex::encode(&index.0[..6])); }
                self.perm_rm_offer(index);
                self.offer = None;
                continue;
            }
            // Remove if no longer in its domain (OfferStream.cpp:293-301,
            // `permissioned_dex::offerInDomain`): an offer carrying a DomainID
            // whose owner fails `accountInDomain` — owner, or an accepted
            // unexpired credential the domain lists — is removed for good,
            // whichever book reached it (finding 337).
            if let Some(d) = entry.domain {
                if !crate::tx::misc::account_in_domain(ps.sandbox(), &offer.owner, &d) {
                    if tr { eprintln!("FLOW   stream: {} no longer in domain", hex::encode(&index.0[..6])); }
                    self.perm_rm_offer(index);
                    self.offer = None;
                    continue;
                }
            }
            // Owner funds.
            let funds = self.funds(ps, &offer, false);
            self.owner_funds = Some(funds);
            if funds.is_zero() || funds.negative() {
                // "Found unfunded" (funds unchanged since the flow opened)
                // is permanent; "became unfunded" is stepped past only.
                let original = self.funds(ps, &offer, true);
                if tr { eprintln!("FLOW   stream: {} unfunded funds={funds} original={original} owner={}", hex::encode(&index.0[..6]), hex::encode(&offer.owner[..6])); }
                if original == funds {
                    self.perm_rm_offer(index);
                }
                self.offer = None;
                continue;
            }
            // `shouldRmSmallIncreasedQOffer`.
            if should_rm_small_increased_q_offer(&offer, funds) {
                let original = self.funds(ps, &offer, true);
                if tr { eprintln!("FLOW   stream: {} small increased-q offer funds={funds}", hex::encode(&index.0[..6])); }
                if original == funds {
                    self.perm_rm_offer(index);
                }
                self.offer = None;
                continue;
            }
            self.offer = Some(offer);
            return true;
        }
    }
}

/// The two `original_funds` reads: `accountFundsHelper(cancelView_, …)`.
/// The cancelView is the view the flow opened with — the base ledger plus
/// what THIS transaction wrote before the flow (fee, sequence): for an
/// offer owner other than the taker, the base ledger's own balance.
fn account_funds_iou_original(ps: &PaymentSandbox, owner: &[u8; 20], asset: &Asset, amt_default: IouAmount) -> IouAmount {
    let Some(issuer) = asset.issuer else { return IouAmount::ZERO };
    if issuer == *owner {
        return amt_default;
    }
    // `isFrozen` on the afView: the issuer's global freeze or its side of
    // the line.
    let global = ps.af_json(&keylet::account_root_key(&issuer)).is_some_and(|a| a["Flags"].as_u64().unwrap_or(0) & super::view::LSF_GLOBAL_FREEZE != 0);
    if global {
        return IouAmount::ZERO;
    }
    let Some(line) = ps.af_json(&keylet::ripple_state_key(owner, &issuer, &asset.currency)) else { return IouAmount::ZERO };
    let bit: u64 = if issuer > *owner { 0x0080_0000 } else { 0x0040_0000 };
    if line["Flags"].as_u64().unwrap_or(0) & bit != 0 {
        return IouAmount::ZERO;
    }
    let (neg, bal) = signed_value(&line["Balance"]);
    let party_low = *owner < issuer;
    let holder_neg = if party_low { neg && bal.0 > 0 } else { !neg && bal.0 > 0 };
    let b = IouAmount::from_me(holder_neg, bal);
    if b <= IouAmount::ZERO { IouAmount::ZERO } else { b }
}

fn xrp_liquid_original(ps: &PaymentSandbox, owner: &[u8; 20]) -> i128 {
    let Some(root) = ps.af_json(&keylet::account_root_key(owner)) else { return 0 };
    let balance: i128 = root["Balance"].as_str().and_then(|s| s.parse::<i128>().ok()).unwrap_or(0);
    if std::env::var("XRPL_FLOW_TRACE").is_ok() {
        let live = json_at(ps.sandbox(), &keylet::account_root_key(owner)).and_then(|r| r["Balance"].as_str().map(|s| s.to_string()));
        eprintln!("FLOW   xrp_liquid_original owner={} af_balance={balance} live_balance={live:?} layers={}", hex::encode(&owner[..6]), ps.depth());
    }
    let raw = super::payment_sandbox::OwnerCounts {
        owner: root["OwnerCount"].as_u64().unwrap_or(0) as u32,
        sponsored: root["SponsoredOwnerCount"].as_u64().unwrap_or(0) as u32,
        sponsoring: root["SponsoringOwnerCount"].as_u64().unwrap_or(0) as u32,
    };
    // `xrpLiquid(cancelView_, …)` runs `view.ownerCountHook(id, OwnerCount)`
    // on the afView, whose chain (`ps_`) is the flow's own sandbox — an
    // offer deleted by an earlier iteration still counts toward the reserve
    // (`adjustOwnerCountHook`). offer_sell_remaining_input_is_the_fold_of_
    // saved_iteration_ins: the maker's first offer went at iteration 2, its
    // root read 73 at iteration 8, the reserve fell 0.2 XRP short and
    // "original" 200000 ≠ current 0 made a found-unfunded offer look like
    // one that merely became so — mainnet removed it (OwnerCount 72).
    let counts = ps.owner_count_hook(owner, raw);
    let reserve = crate::ledger::fees::account_reserve(ps.sandbox(), counts.count() as u64) as i128;
    (balance - reserve).max(0)
}

/// `TOfferStreamBase::erase`: drop a dangling directory entry.
fn erase_dir_entry(ps: &mut PaymentSandbox, dir: &Hash256, index: &Hash256) {
    let Some(mut page) = json_at(ps.sandbox(), dir) else { return };
    let want = hex::encode_upper(index.0);
    let Some(arr) = page.get_mut("Indexes").and_then(|v| v.as_array_mut()) else { return };
    let before = arr.len();
    arr.retain(|v| v.as_str().map(|s| s.to_uppercase()) != Some(want.clone()));
    if arr.len() != before {
        crate::tx::offer::put_json(ps.view(), *dir, &page);
    }
}

/// `shouldRmSmallIncreasedQOffer` (fixRmSmallIncreasedQOffers on): an offer
/// whose effective in — after the owner's funds shrink its out — is at most
/// one minimum unit and whose effective quality is worse than filed.
/// Considered only when TakerPays is XRP, or both sides are IOU with
/// TakerPays < TakerGets; never when TakerGets is XRP.
pub fn should_rm_small_increased_q_offer(offer: &Offer, owner_funds: EitherAmount) -> bool {
    if offer.amount_out.is_xrp() {
        return false;
    }
    if !offer.amount_in.is_xrp() && !offer.amount_out.is_xrp() && !offer.amount_in.lt(&offer.amount_out) {
        return false;
    }
    // fixReducedOffersV1: `ceil_out_strict(ofrAmts, ownerFunds, false)`.
    let (eff_in, eff_out) = if offer.asset_out.issuer != Some(offer.owner) && owner_funds.lt(&offer.amount_out) {
        ceil_out_strict(offer.quality, offer.amount_in, offer.amount_out, owner_funds, false)
    } else {
        (offer.amount_in, offer.amount_out)
    };
    if eff_in.signum() <= 0 || eff_out.signum() <= 0 {
        return true;
    }
    if eff_in.gt(&EitherAmount::min_positive(eff_in.is_xrp())) {
        return false;
    }
    // `Quality{effectiveAmounts} < offer.quality()`: a WORSE quality.
    let eff_q = quality_of(eff_in, eff_out);
    eff_q.is_some_and(|q| q.0 > offer.quality.0)
}

/// `Quality{amounts}` = `getRate(out, in)`.
pub fn quality_of(amt_in: EitherAmount, amt_out: EitherAmount) -> Option<Quality> {
    let ((im, ie), (om, oe)) = (amt_in.mantissa_exp(), amt_out.mantissa_exp());
    crate::ledger::keylet::rate_encode_native(im, ie, amt_in.is_xrp(), om, oe, amt_out.is_xrp()).map(Quality)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_counter_stops_at_its_limit() {
        let mut c = StepCounter::new(2);
        assert!(c.step());
        assert!(c.step());
        assert!(!c.step());
    }

    #[test]
    fn dec_key_borrows() {
        let mut k = Hash256([0u8; 32]);
        k.0[31] = 0;
        k.0[30] = 1;
        dec_key(&mut k);
        assert_eq!(k.0[30], 0);
        assert_eq!(k.0[31], 0xFF);
    }
}
