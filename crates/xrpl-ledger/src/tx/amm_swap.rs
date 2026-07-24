//! AMM swap engine — faithful port of rippled's AMMHelpers under the
//! fixAMMv1_1/fixAMMv1_2 amendments.
//!
//! The arithmetic core mirrors rippled's `Number`: a 16-significant-digit
//! decimal float where every operation is correctly rounded under an explicit
//! rounding mode. `swapAssetIn`/`swapAssetOut` use the amendment's directed
//! per-step rounding (always favoring the AMM); the book-anchored offer
//! sizing (`changeSpotPriceQuality`) runs in to-nearest like rippled's
//! `NumberRoundModeGuard mg(Number::to_nearest)`.
//!
//! Single-path consumption semantics (AMMOffer::limitIn/limitOut): the
//! generated offer only decides *whether* and *at what anchor* the AMM
//! participates; the amounts actually consumed are a direct swap of the
//! binding limit against the live pool balances.

use super::offer::{self as ox, Leg, Me};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use std::cmp::Ordering;

const LO: u128 = 1_000_000_000_000_000; // 1e15
const HI: u128 = 10_000_000_000_000_000; // 1e16
const N_ONE: Me = (LO, -15);
const N_TWO: Me = (2 * LO, -15);
const N_FOUR: Me = (4 * LO, -15);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rnd {
    Down,
    Up,
    Near,
}

/// Correctly round an unnormalized positive (mantissa, exponent) to 16
/// significant digits. `sticky` marks value already shifted off (the true
/// value lies strictly between the mantissa and its successor).
fn round16(m: u128, e: i32, sticky: bool, rnd: Rnd) -> Me {
    if m == 0 {
        return (0, 0);
    }
    let mut shift = 0u32;
    let mut t = m;
    while t >= HI {
        t /= 10;
        shift += 1;
    }
    let (mut q, mut e) = if shift > 0 {
        let d = 10u128.pow(shift);
        let q0 = m / d;
        let r = m % d;
        let up = match rnd {
            Rnd::Down => false,
            Rnd::Up => r > 0 || sticky,
            Rnd::Near => {
                let twice = r.saturating_mul(2);
                twice > d || (twice == d && (sticky || q0 & 1 == 1))
            }
        };
        (q0 + up as u128, e + shift as i32)
    } else if sticky && rnd == Rnd::Up {
        (m + 1, e)
    } else {
        (m, e)
    };
    if q >= HI {
        q /= 10;
        e += 1;
    }
    while q < LO {
        q *= 10;
        e -= 1;
    }
    (q, e)
}

fn n_norm(x: Me) -> Me {
    round16(x.0, x.1, false, Rnd::Near)
}

fn n_cmp(a: Me, b: Me) -> Ordering {
    let a = n_norm(a);
    let b = n_norm(b);
    if a.0 == 0 || b.0 == 0 {
        return a.0.cmp(&b.0);
    }
    match a.1.cmp(&b.1) {
        Ordering::Equal => a.0.cmp(&b.0),
        o => o,
    }
}

fn n_mul(a: Me, b: Me, rnd: Rnd) -> Me {
    if a.0 == 0 || b.0 == 0 {
        return (0, 0);
    }
    let a = n_norm(a);
    let b = n_norm(b);
    round16(a.0 * b.0, a.1 + b.1, false, rnd)
}

fn n_div(a: Me, b: Me, rnd: Rnd) -> Me {
    if a.0 == 0 || b.0 == 0 {
        return (0, 0);
    }
    let a = n_norm(a);
    let b = n_norm(b);
    let num = a.0 * 100_000_000_000_000_000u128; // ×1e17 ≤ 1e33, fits u128
    let q = num / b.0;
    let r = num % b.0;
    round16(q, a.1 - b.1 - 17, r != 0, rnd)
}

fn n_add(a: Me, b: Me, rnd: Rnd) -> Me {
    if a.0 == 0 {
        return n_norm(b);
    }
    if b.0 == 0 {
        return n_norm(a);
    }
    let a = n_norm(a);
    let b = n_norm(b);
    let (hi, lo) = if a.1 >= b.1 { (a, b) } else { (b, a) };
    let diff = (hi.1 - lo.1) as u32;
    if diff <= 22 {
        round16(hi.0 * 10u128.pow(diff) + lo.0, lo.1, false, rnd)
    } else {
        // lo is far below one ulp of hi: pure sticky
        round16(hi.0, hi.1, true, rnd)
    }
}

/// a − b, clamped to zero when b ≥ a (callers treat zero as "nothing left").
fn n_sub(a: Me, b: Me, rnd: Rnd) -> Me {
    if b.0 == 0 {
        return n_norm(a);
    }
    if a.0 == 0 || n_cmp(a, b) != Ordering::Greater {
        return (0, 0);
    }
    let a = n_norm(a);
    let b = n_norm(b);
    if (a.1 - b.1).unsigned_abs() <= 22 {
        let emin = a.1.min(b.1);
        let av = a.0 * 10u128.pow((a.1 - emin) as u32);
        let bv = b.0 * 10u128.pow((b.1 - emin) as u32);
        round16(av - bv, emin, false, rnd)
    } else {
        // b is negligible: a − ε
        match rnd {
            Rnd::Down => {
                if a.0 > LO {
                    (a.0 - 1, a.1)
                } else {
                    (HI - 1, a.1 - 1)
                }
            }
            _ => a, // Up and Near: ε is below half an ulp
        }
    }
}

fn isqrt(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let mut x = (n as f64).sqrt() as u128 + 2;
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            break;
        }
        x = y;
    }
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x
}

/// Correctly rounded (to nearest) square root.
fn n_sqrt(x: Me) -> Me {
    if x.0 == 0 {
        return (0, 0);
    }
    let (m, e) = n_norm(x);
    // Scale mantissa into [1e30,1e32) keeping the remaining exponent even.
    let s: i32 = if (e - 16).rem_euclid(2) == 0 { 16 } else { 15 };
    let big = m * 10u128.pow(s as u32);
    let mut r = isqrt(big);
    // nearest: round up when the remainder passes the (r+1/2)² midpoint
    if big - r * r > r {
        r += 1;
    }
    let mut e2 = (e - s) / 2;
    if r >= HI {
        r /= 10;
        e2 += 1;
    }
    round16(r, e2, false, Rnd::Near)
}

/// tfee basis points → fee fraction (exact in decimal, mode-independent).
fn fee_n(tfee: u16) -> Me {
    if tfee == 0 {
        return (0, 0);
    }
    n_norm((tfee as u128, -5))
}

fn to_amount(x: Me, xrp: bool, rnd: Rnd) -> Me {
    if x.0 == 0 {
        return (0, 0);
    }
    if !xrp {
        return n_norm(x);
    }
    (ox::me_rescale(x, 0, rnd == Rnd::Up), 0)
}

/// getRate-encode a rate value: ((exp+100)<<56) | mantissa∈[1e15,1e16).
fn encode_rate(n: Me) -> u64 {
    if n.0 == 0 {
        return 0;
    }
    let n = n_norm(n);
    (((n.1 + 100) as u64) << 56) | n.0 as u64
}

fn decode_rate(q: u64) -> Me {
    let m = q & 0x00FF_FFFF_FFFF_FFFF;
    let e = ((q >> 56) as i32) - 100;
    n_norm((m as u128, e))
}

/// rippled getRate (STAmount divide): truncating muldiv @1e17, +5, then one
/// banker's-rounding pass over the excess tail — identical to
/// `keylet::offer_quality` but on Me inputs. rate = pays/gets = in/out.
/// The fill's rate (in per out) as a Number — rippled Quality{out, in}.
fn rate_of_me_pair(inp: Me, out: Me) -> Me {
    n_div(inp, out, Rnd::Near)
}

fn rate_of(pays: Me, gets: Me) -> u64 {
    if pays.0 == 0 || gets.0 == 0 {
        return 0;
    }
    let (nm, ne) = n_norm(pays);
    let (dm, de) = n_norm(gets);
    let v = nm * 100_000_000_000_000_000u128 / dm + 5;
    let mut e = ne - de - 17;
    let mut k = 0u32;
    let mut t = v;
    while t >= HI {
        t /= 10;
        k += 1;
    }
    let d = 10u128.pow(k);
    let (mut q, r) = (v / d, v % d);
    let twice = r * 2;
    if twice > d || (twice == d && q & 1 == 1) {
        q += 1;
    }
    e += k as i32;
    if q >= HI {
        q /= 10;
        e += 1;
    }
    encode_rate((q, e))
}

/// swapAssetIn (fixAMMv1_1): `out` released for `asset_in` paid into the pool.
pub(crate) fn swap_asset_in(pool_in: Me, pool_out: Me, asset_in: Me, tfee: u16, out_xrp: bool) -> Me {
    let num = n_mul(pool_in, pool_out, Rnd::Up);
    let omf = n_sub(N_ONE, fee_n(tfee), Rnd::Down);
    let eff = n_mul(asset_in, omf, Rnd::Down);
    let den = n_add(pool_in, eff, Rnd::Down);
    if den.0 == 0 {
        return (0, 0);
    }
    let ratio = n_div(num, den, Rnd::Up);
    let out = n_sub(pool_out, ratio, Rnd::Down);
    to_amount(out, out_xrp, Rnd::Down)
}

/// swapAssetOut (fixAMMv1_1): `in` required to take `asset_out` from the
/// pool. None when the request would drain the pool.
pub(crate) fn swap_asset_out(pool_in: Me, pool_out: Me, asset_out: Me, tfee: u16, in_xrp: bool) -> Option<Me> {
    let num = n_mul(pool_in, pool_out, Rnd::Up);
    let den = n_sub(pool_out, asset_out, Rnd::Down);
    if den.0 == 0 {
        return None;
    }
    let ratio = n_div(num, den, Rnd::Up);
    let num2 = n_sub(ratio, pool_in, Rnd::Up);
    let omf = n_sub(N_ONE, fee_n(tfee), Rnd::Down);
    let swap_in = n_div(num2, omf, Rnd::Up);
    Some(to_amount(swap_in, in_xrp, Rnd::Up))
}

/// changeSpotPriceQuality (post-fixAMMv1_1): the offer (in, out) that moves
/// the pool spot quality down to `target` (getRate in/out). None when the
/// fee makes it ungeneratable.
fn anchored_offer(
    pool_in: Me,
    pool_out: Me,
    in_xrp: bool,
    out_xrp: bool,
    target: u64,
    tfee: u16,
) -> Option<(Me, Me)> {
    let r = decode_rate(target);
    if r.0 == 0 {
        return None;
    }
    let f = n_sub(N_ONE, fee_n(tfee), Rnd::Near);
    if out_xrp {
        // getAMMOfferStartWithTakerGets: a=1, b = I(1−1/f)/r − 2O (< 0),
        // c = O² − IO/r (> 0 required for a positive root)
        let inv_f = n_div(N_ONE, f, Rnd::Near);
        let fm1 = n_sub(inv_f, N_ONE, Rnd::Near); // (1/f − 1) ≥ 0
        let term = n_div(n_mul(pool_in, fm1, Rnd::Near), r, Rnd::Near);
        let b_mag = n_add(term, n_mul(N_TWO, pool_out, Rnd::Near), Rnd::Near);
        let o2 = n_mul(pool_out, pool_out, Rnd::Near);
        let io_r = n_div(n_mul(pool_in, pool_out, Rnd::Near), r, Rnd::Near);
        if n_cmp(o2, io_r) != Ordering::Greater {
            return None;
        }
        let c_mag = n_sub(o2, io_r, Rnd::Near);
        let b2 = n_mul(b_mag, b_mag, Rnd::Near);
        let fourc = n_mul(N_FOUR, c_mag, Rnd::Near);
        if n_cmp(b2, fourc) != Ordering::Greater {
            return None;
        }
        let d = n_sub(b2, fourc, Rnd::Near);
        // citardauq, b<0 c>0: root = 2c/(−b + √d) = 2c/(|b| + √d)
        let root = n_div(
            n_mul(N_TWO, c_mag, Rnd::Near),
            n_add(b_mag, n_sqrt(d), Rnd::Near),
            Rnd::Near,
        );
        let ocon = n_sub(pool_out, n_div(pool_in, n_mul(r, f, Rnd::Near), Rnd::Near), Rnd::Near);
        if ocon.0 == 0 {
            return None;
        }
        let pick = if n_cmp(ocon, root) == Ordering::Less { ocon } else { root };
        let make = |o_prop: Me| -> Option<(Me, Me)> {
            let o = to_amount(o_prop, true, Rnd::Down);
            if o.0 == 0 || n_cmp(o, pool_out) != Ordering::Less {
                return None;
            }
            let i = swap_asset_out(pool_in, pool_out, o, tfee, in_xrp)?;
            if i.0 == 0 {
                return None;
            }
            Some((i, o))
        };
        let (i, o) = make(pick)?;
        if rate_of(i, o) > target {
            // reduceOffer: shave 0.01% (truncating) and retry once
            return make(n_mul(o, (9_999_000_000_000_000, -16), Rnd::Down));
        }
        Some((i, o))
    } else {
        // getAMMOfferStartWithTakerPays: a=f, b=I(1+f) (>0), c=I²−IOr (<0)
        let b = n_mul(pool_in, n_add(N_ONE, f, Rnd::Near), Rnd::Near);
        let i2 = n_mul(pool_in, pool_in, Rnd::Near);
        let ior = n_mul(n_mul(pool_in, pool_out, Rnd::Near), r, Rnd::Near);
        if n_cmp(ior, i2) != Ordering::Greater {
            return None;
        }
        let c_mag = n_sub(ior, i2, Rnd::Near);
        let d = n_add(
            n_mul(b, b, Rnd::Near),
            n_mul(n_mul(N_FOUR, f, Rnd::Near), c_mag, Rnd::Near),
            Rnd::Near,
        );
        // citardauq, b>0 c<0: root = 2c/(−b − √d) = 2|c|/(b + √d)
        let root = n_div(
            n_mul(N_TWO, c_mag, Rnd::Near),
            n_add(b, n_sqrt(d), Rnd::Near),
            Rnd::Near,
        );
        let icon = n_sub(n_mul(pool_out, r, Rnd::Near), n_div(pool_in, f, Rnd::Near), Rnd::Near);
        if icon.0 == 0 {
            return None;
        }
        let pick = if n_cmp(icon, root) == Ordering::Less { icon } else { root };
        let make = |i_prop: Me| -> Option<(Me, Me)> {
            let i = to_amount(i_prop, in_xrp, Rnd::Down);
            if i.0 == 0 {
                return None;
            }
            let o = swap_asset_in(pool_in, pool_out, i, tfee, out_xrp);
            if o.0 == 0 {
                return None;
            }
            Some((i, o))
        };
        let (i, o) = make(pick)?;
        if rate_of(i, o) > target {
            return make(n_mul(i, (9_999_000_000_000_000, -16), Rnd::Down));
        }
        Some((i, o))
    }
}

/// AMM discovered for a currency pair, with the taker's effective fee.
pub(crate) struct Amm {
    pub account: [u8; 20],
    pub tfee: u16,
}

fn hex20(s: &str) -> Option<[u8; 20]> {
    let b = hex::decode(s).ok()?;
    <[u8; 20]>::try_from(b.as_slice()).ok()
}

/// Find the AMM for (spend, want), resolving the taker's effective trading
/// fee (auction-slot discount when the taker holds or is authorized on an
/// unexpired slot).
pub(crate) fn discover(sandbox: &Sandbox, spend: &Leg, want: &Leg, taker: &[u8; 20]) -> Option<Amm> {
    let key = keylet::amm_key(&spend.cur, &spend.issuer, &want.cur, &want.issuer);
    let obj = ox::json_at(sandbox, &key)?;
    if obj.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("AMM") {
        return None;
    }
    let account = obj.get("Account").and_then(|v| v.as_str()).and_then(hex20)?;
    let mut tfee = obj.get("TradingFee").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    if let Some(slot) = obj.get("AuctionSlot") {
        let expires = slot.get("Expiration").and_then(|v| v.as_u64()).unwrap_or(0);
        let close = sandbox.base().header.close_time as u64;
        if expires > close {
            let taker_hex = hex::encode(taker);
            let mut in_slot = slot
                .get("Account")
                .and_then(|v| v.as_str())
                .map(|a| a.eq_ignore_ascii_case(&taker_hex))
                .unwrap_or(false);
            if let Some(auth) = slot.get("AuthAccounts").and_then(|v| v.as_array()) {
                for a in auth {
                    if a["AuthAccount"]["Account"]
                        .as_str()
                        .map(|x| x.eq_ignore_ascii_case(&taker_hex))
                        .unwrap_or(false)
                    {
                        in_slot = true;
                    }
                }
            }
            if in_slot {
                tfee = slot.get("DiscountedFee").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            }
        }
    }
    Some(Amm { account, tfee })
}

/// Pool balance of `leg` held by the AMM account (rippled ammAccountHolds:
/// full XRP balance with NO reserve subtraction; signed line balance toward
/// the account for IOU).
pub(crate) fn holds(sandbox: &Sandbox, acct: &[u8; 20], leg: &Leg) -> Me {
    if leg.xrp {
        let key = keylet::account_root_key(acct);
        let Some(a) = ox::json_at(sandbox, &key) else { return (0, 0) };
        let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        (bal, 0)
    } else {
        let lkey = keylet::ripple_state_key(acct, &leg.issuer, &leg.cur);
        let Some(line) = ox::json_at(sandbox, &lkey) else { return (0, 0) };
        let (neg, bal) = ox::signed_value(&line["Balance"]);
        let party_low = acct < &leg.issuer;
        let party_holds = if party_low { !neg } else { neg };
        if party_holds && bal.0 > 0 {
            bal
        } else {
            (0, 0)
        }
    }
}

/// One MULTI-PATH AMM turn (rippled generateFibSeqOffer): with more than one
/// strand (every IOU↔IOU crossing bridges), the pool competes as a CLOB-like
/// offer sized by the Fibonacci sequence off the pool balances at the START
/// of the crossing. The slice's AVERAGE quality (spot + slippage + fee) is
/// what competes with the books — the razor margin that decides
/// pool-vs-bridge (#105666830 A8830CA4). Fills proportionally, capped by the
/// taker's remainders.
#[allow(clippy::too_many_arguments)]
pub(crate) fn consume_fib(
    sandbox: &mut Sandbox,
    amm: &Amm,
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    rem_pays: Me,
    rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    sell: bool,
    init: (Me, Me),
    iters: u32,
    best_book: Option<Me>,
) -> (Me, Me, bool) {
    if ox::me_is_zero(rem_pays) || ox::me_is_zero(rem_gets) {
        return (rem_pays, rem_gets, false);
    }
    let Some((s_in, s_out)) = fib_slice(sandbox, amm, init, iters, pays_leg, gets_leg) else {
        return (rem_pays, rem_gets, false);
    };
    let q = rate_of_me_pair(s_in, s_out);
    if std::env::var("DX_AMM").is_ok() {
        eprintln!("DX_AMM fib iter={iters} q={q:?} best_book={best_book:?} thr={threshold:x} slice=({s_in:?},{s_out:?})");
    }
    // Strictly better than the best book offer, and within the taker limit.
    if let Some(bb) = best_book {
        if n_cmp(q, bb) != Ordering::Less {
            return (rem_pays, rem_gets, false);
        }
    }
    if threshold != u64::MAX && n_cmp(q, decode_rate(threshold)) == Ordering::Greater {
        return (rem_pays, rem_gets, false);
    }
    // CLOB-like proportional consumption against the remainders.
    let mut take_in = s_in;
    let mut take_out = s_out;
    if !sell && n_cmp(take_out, rem_pays) == Ordering::Greater {
        take_in = to_amount(ox::me_muldiv(rem_pays, s_in, s_out, true), gets_leg.xrp, Rnd::Up);
        take_out = rem_pays;
    }
    if n_cmp(take_in, rem_gets) == Ordering::Greater {
        take_out = to_amount(ox::me_muldiv(rem_gets, s_out, s_in, false), pays_leg.xrp, Rnd::Down);
        take_in = rem_gets;
    }
    if take_in.0 == 0 || take_out.0 == 0 {
        return (rem_pays, rem_gets, false);
    }
    ox::move_leg(sandbox, taker, &amm.account, gets_leg, take_in);
    ox::move_leg(sandbox, &amm.account, beneficiary, pays_leg, take_out);
    (
        ox::me_sub(rem_pays, take_out),
        ox::me_sub(rem_gets, take_in),
        true,
    )
}

/// Pool balances (in = what the taker pays in, out = what they receive) for
/// the fib base — captured at crossing start.
pub(crate) fn pool_balances(sandbox: &Sandbox, amm: &Amm, pays_leg: &Leg, gets_leg: &Leg) -> (Me, Me) {
    (
        holds(sandbox, &amm.account, gets_leg),
        holds(sandbox, &amm.account, pays_leg),
    )
}

/// The current fib-sequence slice (in, out) for a pool, without applying it.
pub(crate) fn fib_slice(
    sandbox: &Sandbox,
    amm: &Amm,
    init: (Me, Me),
    iters: u32,
    pays_leg: &Leg,
    gets_leg: &Leg,
) -> Option<(Me, Me)> {
    let pool_in = holds(sandbox, &amm.account, gets_leg);
    let pool_out = holds(sandbox, &amm.account, pays_leg);
    if pool_in.0 == 0 || pool_out.0 == 0 || init.0.0 == 0 || init.1.0 == 0 {
        return None;
    }
    const FIB: [u32; 16] = [1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597];
    let pct: Me = (2_500_000_000_000_000, -19); // kInitialFibSeqPct = 5/20000
    let base_in = to_amount(n_mul(init.0, pct, Rnd::Up), gets_leg.xrp, Rnd::Up);
    if base_in.0 == 0 {
        return None;
    }
    let (s_in, s_out);
    if iters == 0 {
        s_in = base_in;
        s_out = swap_asset_in(init.0, init.1, s_in, amm.tfee, pays_leg.xrp);
    } else {
        let out0 = swap_asset_in(init.0, init.1, base_in, amm.tfee, pays_leg.xrp);
        let idx = (iters as usize - 1).min(FIB.len() - 1);
        s_out = to_amount(
            n_mul(out0, ((FIB[idx] as u128) * LO, -15), Rnd::Down),
            pays_leg.xrp,
            Rnd::Down,
        );
        if s_out.0 == 0 || n_cmp(s_out, pool_out) != Ordering::Less {
            return None;
        }
        match swap_asset_out(pool_in, pool_out, s_out, amm.tfee, gets_leg.xrp) {
            Some(i) => s_in = i,
            None => return None,
        }
    }
    (s_in.0 > 0 && s_out.0 > 0).then_some((s_in, s_out))
}

/// Average rate (in per out) of a fill — public face of the Quality compare.
pub(crate) fn slice_rate(inp: Me, out: Me) -> Me {
    rate_of_me_pair(inp, out)
}

/// Move a slice through the pool: taker pays `take_in` of gets, receives
/// `take_out` of pays.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_slice(
    sandbox: &mut Sandbox,
    amm: &Amm,
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    pays_leg: &Leg,
    gets_leg: &Leg,
    take_in: Me,
    take_out: Me,
) {
    ox::move_leg(sandbox, taker, &amm.account, gets_leg, take_in);
    ox::move_leg(sandbox, &amm.account, beneficiary, pays_leg, take_out);
}

/// One AMM turn inside the crossing walk. `clob` is the next book
/// directory's getRate quality (None once the book is exhausted);
/// `threshold` is the taker's limit rate (u64::MAX = unlimited). Consumes
/// AMM liquidity when the pool's spot quality strictly beats the book
/// (beyond 1e-7 relative distance), moving balances in the sandbox.
/// Returns updated (rem_pays, rem_gets, used).
#[allow(clippy::too_many_arguments)]
pub(crate) fn consume(
    sandbox: &mut Sandbox,
    amm: &Amm,
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    rem_pays: Me,
    rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    sell: bool,
    clob: Option<u64>,
) -> (Me, Me, bool) {
    if ox::me_is_zero(rem_pays) || ox::me_is_zero(rem_gets) {
        return (rem_pays, rem_gets, false);
    }
    // pool.in = what the taker pays into the pool (spend = gets_leg);
    // pool.out = what the taker receives (want = pays_leg)
    let pool_in = holds(sandbox, &amm.account, gets_leg);
    let pool_out = holds(sandbox, &amm.account, pays_leg);
    if pool_in.0 == 0 || pool_out.0 == 0 {
        return (rem_pays, rem_gets, false);
    }
    // Spot-price quality embeds the trading fee: paying dIn yields
    // dOut = dIn·(1−f)·out/in, so rate = in / (out·(1−f)) — the feeless
    // ratio admitted fills rippled rejects at the boundary (#105035381
    // DDFDD49B killed vs 2C4DF181 filled, same pool).
    let omf_spot = n_sub(N_ONE, fee_n(amm.tfee), Rnd::Near);
    let spot = rate_of(pool_in, n_mul(pool_out, omf_spot, Rnd::Near));
    if std::env::var("DX_AMM").is_ok() {
        eprintln!("DX_AMM spot={spot:x} thr={threshold:x} clob={clob:?} tfee={} pool_in={pool_in:?} pool_out={pool_out:?}", amm.tfee);
    }
    if spot == 0 || spot > threshold {
        return (rem_pays, rem_gets, false);
    }
    // AMM participates only when strictly better than the CLOB and not
    // within 1e-7 relative distance of it (AMMLiquidity::getOffer).
    if let Some(qb) = clob {
        if spot >= qb {
            return (rem_pays, rem_gets, false);
        }
        let (rs, rb) = (decode_rate(spot), decode_rate(qb));
        let dist = n_div(n_sub(rb, rs, Rnd::Near), rb, Rnd::Near);
        if n_cmp(dist, (LO, -22)) == Ordering::Less {
            return (rem_pays, rem_gets, false); // within 1e-7
        }
    }
    // Generated offer: anchored to the book when present (with the maxOffer
    // fallback of fixAMMv1_2), else maxOffer = 99% of pool.out.
    let max_offer = || -> Option<(Me, Me)> {
        let out = to_amount(
            n_mul(pool_out, (9_900_000_000_000_000, -16), Rnd::Down),
            pays_leg.xrp,
            Rnd::Down,
        );
        if out.0 == 0 || n_cmp(out, pool_out) != Ordering::Less {
            return None;
        }
        swap_asset_out(pool_in, pool_out, out, amm.tfee, gets_leg.xrp).map(|i| (i, out))
    };
    let offer = if let Some(qb) = clob {
        anchored_offer(pool_in, pool_out, gets_leg.xrp, pays_leg.xrp, qb, amm.tfee).or_else(|| {
            // fixAMMv1_2: fall back to maxOffer when it still beats the book
            max_offer().filter(|(i, o)| rate_of(*i, *o) < qb)
        })
    } else {
        max_offer()
    };
    let Some((mut take_in, mut take_out)) = offer else {
        return (rem_pays, rem_gets, false);
    };
    // Single-path limit semantics: the binding limit is re-swapped directly
    // against the pool (AMMOffer::limitOut / limitIn). For a tfSell offer the
    // pays side (rem_pays) is only a minimum, not a cap — the taker takes the
    // surplus — so only rem_gets bounds the fill.
    if !sell && n_cmp(take_out, rem_pays) == Ordering::Greater {
        take_out = rem_pays;
        match swap_asset_out(pool_in, pool_out, take_out, amm.tfee, gets_leg.xrp) {
            Some(i) => take_in = i,
            None => return (rem_pays, rem_gets, false),
        }
    }
    if n_cmp(take_in, rem_gets) == Ordering::Greater {
        take_in = rem_gets;
        take_out = swap_asset_in(pool_in, pool_out, take_in, amm.tfee, pays_leg.xrp);
    }
    // Taker limit handling (rippled StrandFlow limitOut + post-check): the
    // requested OUT is trimmed via the pool's QualityFunction so the fill's
    // average quality equals the limit — outFromAvgQ computed under global
    // Upward rounding with m/b built at Near — then the achieved quality is
    // re-checked, forgiving only a 1e-7 relative round-off on trimmed
    // requests. Directed roundings decide the boundary (#105035381:
    // DDFDD49B killed at 6 ppm over, 2C4DF181 filled inside).
    if threshold != u64::MAX && n_cmp(rate_of_me_pair(take_in, take_out), decode_rate(threshold)) == Ordering::Greater {
        let thr_me = decode_rate(threshold);
        let cfee = n_sub(N_ONE, fee_n(amm.tfee), Rnd::Near);
        let m = n_div(cfee, pool_in, Rnd::Near);
        let b = n_div(n_mul(pool_out, cfee, Rnd::Near), pool_in, Rnd::Near);
        // out = (1/rate − b)/(−m), signed-Upward per op: 1/rate rounds up;
        // the negative difference rounds toward zero (magnitude down); the
        // positive quotient rounds up.
        let r1 = n_div(N_ONE, thr_me, Rnd::Up);
        let num = n_sub(b, r1, Rnd::Down);
        if num.0 == 0 {
            return (rem_pays, rem_gets, false);
        }
        let mut out_req = n_div(num, m, Rnd::Up);
        let mut adjusted = true;
        if !sell && n_cmp(out_req, rem_pays) != Ordering::Less {
            out_req = rem_pays;
            adjusted = false;
        }
        let out_amt = to_amount(out_req, pays_leg.xrp, Rnd::Near);
        if out_amt.0 == 0 {
            return (rem_pays, rem_gets, false);
        }
        let Some(in_req) = swap_asset_out(pool_in, pool_out, out_amt, amm.tfee, gets_leg.xrp) else {
            return (rem_pays, rem_gets, false);
        };
        take_in = in_req;
        take_out = out_amt;
        if n_cmp(take_in, rem_gets) == Ordering::Greater {
            take_in = rem_gets;
            take_out = swap_asset_in(pool_in, pool_out, take_in, amm.tfee, pays_leg.xrp);
            adjusted = false;
        }
        if take_in.0 == 0 || take_out.0 == 0 {
            return (rem_pays, rem_gets, false);
        }
        let q = rate_of_me_pair(take_in, take_out);
        if n_cmp(q, thr_me) == Ordering::Greater {
            // Achieved quality below the limit: tolerate only trimmed
            // requests within 1e-7 relative distance.
            let dist = n_div(n_sub(q, thr_me, Rnd::Near), thr_me, Rnd::Near);
            if !adjusted || n_cmp(dist, (LO, -22)) != Ordering::Less {
                if std::env::var("DX_AMM").is_ok() {
                    eprintln!("DX_AMM limit reject q={q:?} thr={thr_me:?} adjusted={adjusted}");
                }
                return (rem_pays, rem_gets, false);
            }
        }
    }
    if take_in.0 == 0 || take_out.0 == 0 {
        return (rem_pays, rem_gets, false);
    }
    if std::env::var("DX_AMM").is_ok() {
        eprintln!("DX_AMM CONSUMED acct={} take_in={take_in:?} take_out={take_out:?} spot={spot:x} thr={threshold:x} clob={clob:?}",
            hex::encode(amm.account));
    }
    ox::move_leg(sandbox, taker, &amm.account, gets_leg, take_in);
    ox::move_leg(sandbox, &amm.account, beneficiary, pays_leg, take_out);
    (
        ox::me_sub(rem_pays, take_out),
        ox::me_sub(rem_gets, take_in),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(s: &str) -> Me {
        let (int, frac) = match s.split_once('.') {
            Some((a, b)) => (a, b),
            None => (s, ""),
        };
        let digits = format!("{int}{frac}");
        let m: u128 = digits.trim_start_matches('0').parse().unwrap();
        n_norm((m, -(frac.len() as i32)))
    }

    /// Mainnet tx 4ED3F03D… (ledger 105035380): LEDGEND→XRP self-arb,
    /// TradingFee 0. Pool 1071.86732039761 LEDGEND / 241,780,980 drops.
    #[test]
    fn swap_in_matches_mainnet_ledgend_fill() {
        let out = swap_asset_in(me("1071.86732039761"), (241_780_980, 0), me("27.1356091373919"), 0, true);
        assert_eq!(out, (5_969_842, 0));
    }

    /// Mainnet tx 9E5C67FE… (ledger 105666725): aura→XRP, TradingFee 254.
    #[test]
    fn swap_in_matches_mainnet_aura_fill() {
        let out = swap_asset_in(me("5948941191.77095"), (7_604_882_252, 0), me("200000"), 254, true);
        assert_eq!(out, (255_013, 0));
    }

    /// Mainnet tx D8A3244B… (ledger 105035381): XRP→Mars OfferCreate fill,
    /// TradingFee 1000. Delivered exactly 0.8942732854482 Mars — the
    /// directed-rounding tail that plain rational arithmetic misses.
    #[test]
    fn swap_in_matches_mainnet_mars_fill() {
        let out = swap_asset_in((177_984_940, 0), me("179.7202514951749"), (899_058, 0), 1000, false);
        assert_eq!(out, (8_942_732_854_482_000, -16));
    }

    /// AMM-favoring rounding: buying back the delivered amount never costs
    /// more than what was swapped in.
    #[test]
    fn swap_out_bounds_swap_in() {
        let pool_in = me("1071.86732039761");
        let pool_out = (241_780_980u128, 0);
        let inp = me("27.1356091373919");
        let out = swap_asset_in(pool_in, pool_out, inp, 0, true);
        let back = swap_asset_out(pool_in, pool_out, out, 0, false).unwrap();
        assert!(n_cmp(back, inp) != Ordering::Greater);
    }

    /// The AMM keylet must reproduce the mainnet AMMID of the XRP/LEDGEND
    /// pool (6DAA4FDF…).
    #[test]
    fn amm_keylet_matches_mainnet_ammid() {
        let mut led_cur = [0u8; 20];
        led_cur[..7].copy_from_slice(b"LEDGEND");
        let mut c = [0u8; 20];
        c.copy_from_slice(&hex::decode("4C454447454E4400000000000000000000000000").unwrap());
        let iss = <[u8; 20]>::try_from(
            hex::decode("2FA8DB6A7F8FE411B3759B82FE19D983496EA501").unwrap().as_slice(),
        )
        .unwrap();
        let key = keylet::amm_key(&[0u8; 20], &[0u8; 20], &c, &iss);
        assert_eq!(
            hex::encode_upper(key.0),
            "6DAA4FDF97EBFFF94E197FFAF09E9982E1CDE2A0F3D3AF4CB5539BF3A28C8502"
        );
    }
}
