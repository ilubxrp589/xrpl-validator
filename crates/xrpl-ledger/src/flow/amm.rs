//! rippled `AMMContext` (`AMMContext.h`), `AMMLiquidity`
//! (`AMMLiquidity.cpp`) and `AMMOffer` (`AMMOffer.cpp`): the pool's
//! synthetic offer as a BookStep sees it.
//!
//! The arithmetic underneath — `swapAssetIn`, `swapAssetOut`,
//! `changeSpotPriceQuality`, `generateFibSeqOffer`, `maxOffer`, the
//! effective trading fee with the auction-slot discount — is the engine's
//! `tx::amm_swap`, already pinned specimen by specimen (findings 76, 99,
//! 107, 121, 180, 205, 257 …). This file is the STRUCTURE around it:
//! one `AmmContext` per flow (shared by every strand's BookSteps), the
//! `getOffer` decision tree in rippled's order, and an offer whose
//! `limitIn`/`limitOut` re-price through the conservation function in
//! single-path mode and through the filed quality under multi-path.
use std::cell::RefCell;
use std::rc::Rc;

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::tx::amm_swap::{self, Amm};
use crate::tx::offer::{decode20, json_at, rate_me, Leg, Me};

use super::amounts::{EitherAmount, IouAmount};
use super::quality_function::{Quality, QualityFunction};
use super::steps::Asset;

/// `AMMContext`: shared by the whole flow.
#[derive(Debug)]
pub struct AmmContext {
    /// The transaction's account (prices the trading fee: auction slot).
    pub account: [u8; 20],
    multi_path: bool,
    amm_used: bool,
    amm_iters: u16,
}

/// `AMMContext::MaxIterations`.
pub const MAX_ITERATIONS: u16 = 30;

impl AmmContext {
    pub fn new(account: [u8; 20], multi_path: bool) -> AmmContext {
        AmmContext { account, multi_path, amm_used: false, amm_iters: 0 }
    }
    pub fn multi_path(&self) -> bool {
        self.multi_path
    }
    pub fn set_multi_path(&mut self, fs: bool) {
        self.multi_path = fs;
    }
    pub fn set_amm_used(&mut self) {
        self.amm_used = true;
    }
    /// `clear()`: reset the used flag before a strand runs.
    pub fn clear(&mut self) {
        self.amm_used = false;
    }
    /// `update()`: once per payment-engine iteration.
    pub fn update(&mut self) {
        if self.amm_used {
            self.amm_iters += 1;
        }
        self.amm_used = false;
    }
    pub fn max_iters_reached(&self) -> bool {
        self.amm_iters >= MAX_ITERATIONS
    }
    pub fn cur_iters(&self) -> u16 {
        self.amm_iters
    }
}

pub type SharedAmmContext = Rc<RefCell<AmmContext>>;

fn leg(asset: &Asset) -> Leg {
    Leg { xrp: asset.is_xrp(), cur: asset.currency, issuer: asset.issuer.unwrap_or([0; 20]) }
}

fn either(xrp: bool, m: Me) -> EitherAmount {
    if xrp {
        EitherAmount::Xrp(crate::tx::offer::me_rescale(m, 0, false) as i128)
    } else {
        EitherAmount::Iou(IouAmount::from_me(false, m))
    }
}

fn me(a: EitherAmount) -> Me {
    a.mantissa_exp()
}

/// `AMMLiquidity<TIn, TOut>`.
pub struct AmmLiquidity {
    ctx: SharedAmmContext,
    amm: Amm,
    asset_in: Asset,
    asset_out: Asset,
    /// `initialBalances_`: (in, out) as the step was built.
    initial_balances: (Me, Me),
}

impl AmmLiquidity {
    /// The BookStep constructor's discovery: `keylet::amm(in, out)` exists
    /// with a non-zero LPTokenBalance → `AMMLiquidity(view, ammAccount,
    /// getTradingFee(view, amm, ctx.account()), in, out, ctx)`.
    pub fn discover(sb: &Sandbox, ctx: &SharedAmmContext, asset_in: &Asset, asset_out: &Asset) -> Option<AmmLiquidity> {
        let (li, lo) = (leg(asset_in), leg(asset_out));
        let key = keylet::amm_key(&li.cur, &li.issuer, &lo.cur, &lo.issuer);
        let obj = json_at(sb, &key);
        if std::env::var("XRPL_FLOW_TRACE").is_ok() {
            eprintln!("FLOW   amm discover key={} present={} lpt={:?}", hex::encode(&key.0[..12]), obj.is_some(), obj.as_ref().and_then(|o| o.get("LPTokenBalance")).map(|v| v.to_string()).unwrap_or_default());
        }
        let obj = obj?;
        if obj.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("AMM") {
            return None;
        }
        let lpt = obj.get("LPTokenBalance").and_then(|v| v.get("value")).and_then(|v| v.as_str()).unwrap_or("0");
        if lpt.trim_start_matches('-').trim_start_matches('0').trim_start_matches('.').trim_start_matches('0').is_empty() {
            return None;
        }
        let account = obj.get("Account").and_then(|v| v.as_str()).and_then(decode20)?;
        let tfee = amm_swap::effective_trading_fee(sb, &obj, &ctx.borrow().account);
        let amm = Amm { account, tfee };
        let mut l = AmmLiquidity { ctx: ctx.clone(), amm, asset_in: *asset_in, asset_out: *asset_out, initial_balances: ((0, 0), (0, 0)) };
        l.initial_balances = l.fetch_balances(sb);
        Some(l)
    }

    pub fn amm_account(&self) -> [u8; 20] {
        self.amm.account
    }
    pub fn trading_fee(&self) -> u16 {
        self.amm.tfee
    }
    pub fn multi_path(&self) -> bool {
        self.ctx.borrow().multi_path()
    }
    pub fn context(&self) -> &SharedAmmContext {
        &self.ctx
    }
    pub fn issue_in(&self) -> Asset {
        self.asset_in
    }
    pub fn issue_out(&self) -> Asset {
        self.asset_out
    }

    /// `fetchBalances(view)`: `ammAccountHolds` of each side — the pool's
    /// balances, zero when frozen.
    pub fn fetch_balances(&self, sb: &Sandbox) -> (Me, Me) {
        // `pool_balances(sb, amm, pays_leg, gets_leg)` = (holds(gets_leg),
        // holds(pays_leg)): our "pays" is the pool's OUT, "gets" its IN.
        amm_swap::pool_balances(sb, &self.amm, &leg(&self.asset_out), &leg(&self.asset_in))
    }

    /// `AMMLiquidity::getOffer(view, clobQuality)`.
    pub fn get_offer(&self, sb: &Sandbox, clob_quality: Option<Quality>) -> Option<AmmOffer> {
        let r = self.get_offer_inner(sb, clob_quality);
        if std::env::var("XRPL_FLOW_TRACE").is_ok() {
            eprintln!("FLOW   amm getOffer clob={:?} multi={} iters={} -> {}", clob_quality.map(|q| format!("{:x}", q.0)), self.ctx.borrow().multi_path(), self.ctx.borrow().cur_iters(), r.as_ref().map(|o| format!("in={} out={} q={:x}", o.amount_in, o.amount_out, o.quality.0)).unwrap_or_else(|| "none".into()));
        }
        r
    }

    fn get_offer_inner(&self, sb: &Sandbox, clob_quality: Option<Quality>) -> Option<AmmOffer> {
        if self.ctx.borrow().max_iters_reached() {
            return None;
        }
        let balances = self.fetch_balances(sb);
        if std::env::var("XRPL_FLOW_TRACE").is_ok() {
            eprintln!("FLOW   amm balances in={:?} out={:?} initial={:?}", balances.0, balances.1, self.initial_balances);
        }
        if balances.0 .0 == 0 || balances.1 .0 == 0 {
            return None; // "frozen accounts"
        }
        let (pays_leg, gets_leg) = (leg(&self.asset_out), leg(&self.asset_in));
        // `Quality{balances}` — the feeless spot price — against the CLOB:
        // no offer when the spot is worse-or-equal, or within 1e-7 of it.
        if let Some(clob) = clob_quality {
            let spot = amm_swap::spot_upper_bound(sb, &self.amm, &pays_leg, &gets_leg);
            if spot == 0 || spot >= clob.0 || within_relative_distance_q(Quality(spot), clob) {
                return None; // "higher clob quality"
            }
        }
        let multi = self.ctx.borrow().multi_path();
        let iters = self.ctx.borrow().cur_iters() as u32;
        let amounts: Option<(Me, Me)> = if multi {
            // `generateFibSeqOffer`, then `Quality{amounts} < clobQuality`
            // (worse than the CLOB) → no offer.
            let s = amm_swap::fib_slice(sb, &self.amm, self.initial_balances, iters, &pays_leg, &gets_leg)?;
            if let Some(clob) = clob_quality {
                let q = quality_of_me(s.0, s.1, self.asset_in.is_xrp(), self.asset_out.is_xrp());
                if q.is_none_or(|q| q.0 > clob.0) {
                    return None;
                }
            }
            Some(s)
        } else if clob_quality.is_none() {
            amm_swap::max_offer(sb, &self.amm, &pays_leg, &gets_leg)
        } else if let Some(s) = amm_swap::anchored_slice(sb, &self.amm, &pays_leg, &gets_leg, clob_quality.map(|q| q.0).unwrap_or(0)) {
            Some(s)
        } else {
            // fixAMMv1_2: the largest offer, when its quality beats the CLOB.
            let clob = clob_quality.map(|q| q.0).unwrap_or(u64::MAX);
            amm_swap::max_offer(sb, &self.amm, &pays_leg, &gets_leg).filter(|s| {
                quality_of_me(s.0, s.1, self.asset_in.is_xrp(), self.asset_out.is_xrp()).is_some_and(|q| q.0 < clob)
            })
        };
        let (inp, out) = amounts?;
        if inp.0 == 0 || out.0 == 0 {
            return None; // "no valid offer"
        }
        // The offer's quality: `Quality{amounts}` for the Fibonacci and the
        // changeSpotPriceQuality offers, but `Quality{balances}` — the SPOT
        // price — for maxOffer (AMMLiquidity.cpp: `AMMOffer(*this,
        // {swapAssetOut(balances, out), out}, balances, Quality{balances})`).
        let is_max_offer = !multi && (clob_quality.is_none() || amm_swap::anchored_slice(sb, &self.amm, &pays_leg, &gets_leg, clob_quality.map(|q| q.0).unwrap_or(0)).is_none());
        let quality = if is_max_offer {
            quality_of_me(balances.0, balances.1, self.asset_in.is_xrp(), self.asset_out.is_xrp())?
        } else {
            quality_of_me(inp, out, self.asset_in.is_xrp(), self.asset_out.is_xrp())?
        };
        Some(AmmOffer {
            owner: self.amm.account,
            asset_in: self.asset_in,
            asset_out: self.asset_out,
            amount_in: either(self.asset_in.is_xrp(), inp),
            amount_out: either(self.asset_out.is_xrp(), out),
            balances_in: either(self.asset_in.is_xrp(), balances.0),
            balances_out: either(self.asset_out.is_xrp(), balances.1),
            quality,
            tfee: self.amm.tfee,
            multi_path: multi,
            consumed: false,
        })
    }
}

/// `Quality{TAmounts{in, out}}` = `getRate(out, in)`.
fn quality_of_me(inp: Me, out: Me, in_xrp: bool, out_xrp: bool) -> Option<Quality> {
    keylet::rate_encode_native(inp.0, inp.1, in_xrp, out.0, out.1, out_xrp).map(Quality)
}

/// `withinRelativeDistance(Quality, Quality, Number{1, -7})`:
/// `(min.rate() − max.rate()) / min.rate() < 1e-7` where `min`/`max` are
/// the WORSE/BETTER qualities (a better quality has the smaller rate).
pub fn within_relative_distance_q(a: Quality, b: Quality) -> bool {
    if a == b {
        return true;
    }
    // Quality ordering: smaller encoded value = better = larger.
    let (worse, better) = if a.0 > b.0 { (a, b) } else { (b, a) };
    let (rw, rb) = (rate_me(worse.0), rate_me(better.0));
    let diff = amm_swap::n_sub(rw, rb, amm_swap::Rnd::Near);
    let rel = amm_swap::n_div(diff, rw, amm_swap::Rnd::Near);
    amm_swap::n_cmp(rel, (1_000_000_000_000_000, -22)).is_lt()
}

/// `AMMOffer<TIn, TOut>`.
#[derive(Clone, Debug)]
pub struct AmmOffer {
    pub owner: [u8; 20],
    pub asset_in: Asset,
    pub asset_out: Asset,
    pub amount_in: EitherAmount,
    pub amount_out: EitherAmount,
    pub balances_in: EitherAmount,
    pub balances_out: EitherAmount,
    pub quality: Quality,
    pub tfee: u16,
    pub multi_path: bool,
    pub consumed: bool,
}

impl AmmOffer {
    /// `AMMOffer::limitOut`: under multi-path the filed quality
    /// (`ceil_out_strict`); single-path re-prices through `swapAssetOut`.
    pub fn limit_out(&self, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount, round_up: bool) -> (EitherAmount, EitherAmount) {
        if self.multi_path {
            return super::offer_stream::ceil_out_strict(self.quality, amt_in, amt_out, limit, round_up);
        }
        let inp = amm_swap::swap_asset_out(me(self.balances_in), me(self.balances_out), me(limit), self.tfee, self.asset_in.is_xrp())
            .map(|m| either(self.asset_in.is_xrp(), m))
            .unwrap_or(amt_in);
        (inp, limit)
    }

    /// `AMMOffer::limitIn`: multi-path `ceil_in_strict` (fixReducedOffersV2
    /// is live); single-path `swapAssetIn`.
    pub fn limit_in(&self, amt_in: EitherAmount, amt_out: EitherAmount, limit: EitherAmount, round_up: bool) -> (EitherAmount, EitherAmount) {
        if self.multi_path {
            return super::offer_stream::ceil_in_strict(self.quality, amt_in, amt_out, limit, round_up);
        }
        let out = amm_swap::swap_asset_in(me(self.balances_in), me(self.balances_out), me(limit), self.tfee, self.asset_out.is_xrp());
        (limit, either(self.asset_out.is_xrp(), out))
    }

    /// `getQualityFunc`: constant under multi-path, the pool's line
    /// through its balances otherwise.
    pub fn get_quality_func(&self) -> Option<QualityFunction> {
        if self.multi_path {
            return QualityFunction::clob_like(self.quality).ok();
        }
        let n = |m: Me| crate::tx::number::Number::from_parts(false, m.0, m.1, crate::tx::number::Rounding::ToNearest).ok();
        QualityFunction::amm(n(me(self.balances_in))?, n(me(self.balances_out))?, self.tfee as u32).ok()
    }

    /// `checkInvariant(consumed)` (fixAMMv1_3 is live): the product must
    /// not fall, within 1e-7.
    pub fn check_invariant(&self, consumed_in: EitherAmount, consumed_out: EitherAmount) -> bool {
        if consumed_in.gt(&self.amount_in) || consumed_out.gt(&self.amount_out) {
            return false;
        }
        use amm_swap::{n_add, n_cmp, n_div, n_mul, n_sub, Rnd};
        let (bi, bo) = (me(self.balances_in), me(self.balances_out));
        let product = n_mul(bi, bo, Rnd::Near);
        let nbi = n_add(bi, me(consumed_in), Rnd::Near);
        let nbo = n_sub(bo, me(consumed_out), Rnd::Near);
        let new_product = n_mul(nbi, nbo, Rnd::Near);
        if n_cmp(new_product, product).is_ge() {
            return true;
        }
        let diff = n_sub(product, new_product, Rnd::Near);
        let rel = n_div(diff, product, Rnd::Near);
        n_cmp(rel, (1_000_000_000_000_000, -22)).is_lt()
    }

    /// `consume`: nothing to write — the pool moved when the amounts were
    /// transferred in `consumeOffer`; the context learns the AMM was used.
    pub fn consume(&mut self, ctx: &SharedAmmContext, consumed_in: EitherAmount, consumed_out: EitherAmount) -> bool {
        if consumed_in.gt(&self.amount_in) || consumed_out.gt(&self.amount_out) {
            return false; // "Invalid consumed AMM offer."
        }
        self.consumed = true;
        ctx.borrow_mut().set_amm_used();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_counts_iterations_only_when_the_pool_was_used() {
        let mut c = AmmContext::new([0; 20], true);
        c.update();
        assert_eq!(c.cur_iters(), 0);
        c.set_amm_used();
        c.update();
        assert_eq!(c.cur_iters(), 1);
        assert!(!c.max_iters_reached());
    }

    #[test]
    fn relative_distance_on_qualities() {
        let q = |m: u128| Quality(keylet::rate_encode_native(m, -16, false, 1, 0, false).unwrap());
        assert!(within_relative_distance_q(q(1_000_000_000_000_000), q(1_000_000_000_000_000)));
        // 1e-8 apart: within.
        assert!(within_relative_distance_q(q(1_000_000_000_000_000), q(1_000_000_010_000_000)));
        // 1e-6 apart: not.
        assert!(!within_relative_distance_q(q(1_000_000_000_000_000), q(1_000_001_000_000_000)));
    }
}
