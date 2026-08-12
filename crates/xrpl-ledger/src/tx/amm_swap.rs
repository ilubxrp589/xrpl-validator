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
pub(crate) enum Rnd {
    Down,
    Up,
    Near,
}

/// Correctly round an unnormalized positive (mantissa, exponent) to 16
/// significant digits. `sticky` marks value already shifted off (the true
/// value lies strictly between the mantissa and its successor).
pub(crate) fn round16(m: u128, e: i32, sticky: bool, rnd: Rnd) -> Me {
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
    // ⚠ The division REMAINDER IS DISCARDED, not carried as a sticky bit.
    // `Number::operator/=` scales by 10^17 and integer-divides exactly as this
    // does, but for the SMALL (16-digit) mantissa scale — the one mainnet runs,
    // see the amendment check in [the resume memo] — it takes `zm = numerator /
    // dm` and leaves `dropped = false`. Stages 2 and 3, which would recover the
    // remainder and feed it to the Guard, are gated on
    // `range.scale != MantissaScale::Small` and never execute. Only the digits
    // dropped while normalising into range reach the rounding.
    //
    // Feeding the remainder in instead rounds UP on quotients rippled leaves
    // alone. Caught by instrumenting `swapAssetOut` in the FFI shim on
    // #105916476 5F89F8E5: identical inputs, and rippled reports
    //   numerator=502924504664754.8 denom=5960396041 ratio=84377.69926784548
    // where the exact quotient is 84377.699267845480404… — its 17-digit
    // quotient is 84377699267845480, normalising drops one '0' with NO sticky,
    // so Upward does not increment. We produced …549 and the whole swap came
    // out 1.0045e-11 high.
    round16(q, a.1 - b.1 - 17, false, rnd)
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
    n_sqrt_rnd(x, Rnd::Near)
}

/// rippled `ammLPTokens` (AMMHelpers.cpp): what a pool of (`a`, `b`) is worth
/// in LP tokens — `root2(a * b)`, upholding the AMM invariant
/// `sqrt(asset1 * asset2) >= LPTokensBalance`. fixAMMv1_3 runs the WHOLE
/// computation under `RoundingMode::Downward` (a `NumberRoundModeGuard` over
/// both the product and the root), so the invariant can never be broken by a
/// rounding-up.
///
/// ⚠ **XRP counts in DROPS here.** #105666951 `07DC0E81` pairs 100 XRC with
/// `"10000000"` drops and mainnet mints `sqrt(100 * 10000000) = sqrt(1e9) =
/// 31622.77660168379` — reading that side as 10 XRP gives 31.6, off by 1000x.
pub(crate) fn amm_lp_tokens(a: Me, b: Me) -> Me {
    n_sqrt_rnd(n_mul(a, b, Rnd::Down), Rnd::Down)
}

fn n_sqrt_rnd(x: Me, rnd: Rnd) -> Me {
    if x.0 == 0 {
        return (0, 0);
    }
    let (m, e) = n_norm(x);
    // Scale mantissa into [1e30,1e32) keeping the remaining exponent even.
    let s: i32 = if (e - 16).rem_euclid(2) == 0 { 16 } else { 15 };
    let big = m * 10u128.pow(s as u32);
    let mut r = isqrt(big);
    // nearest: round up when the remainder passes the (r+1/2)² midpoint.
    // Downward keeps the floor, so the root never exceeds the true value.
    if rnd == Rnd::Near && big - r * r > r {
        r += 1;
    }
    let mut e2 = (e - s) / 2;
    if r >= HI {
        r /= 10;
        e2 += 1;
    }
    round16(r, e2, false, rnd)
}

/// tfee basis points → fee fraction (exact in decimal, mode-independent).
fn fee_n(tfee: u16) -> Me {
    if tfee == 0 {
        return (0, 0);
    }
    n_norm((tfee as u128, -5))
}

/// `feeMultHalf` — half the trading fee is charged on a single-asset deposit,
/// because only the half that crosses the pool pays it.
fn fee_half_n(tfee: u16) -> Me {
    n_div(fee_n(tfee), N_TWO, Rnd::Near)
}

/// rippled `lpTokensOut` (AMMHelpers.cpp, "Equation 3") — the LP tokens minted
/// by a SINGLE-ASSET deposit of `deposit` into a pool holding `balance` of that
/// asset, against `lpt` outstanding:
///
/// ```text
/// f1 = 1 - tfee/100000
/// f2 = (1 - tfee/200000) / f1
/// r  = deposit / balance
/// c  = sqrt(f2^2 + r/f1) - f2
/// t  = lpt * (r - c) / (1 + c)
/// ```
///
/// Only the final multiply is directed — fixAMMv1_3 does it Downward, commented
/// "minimize tokens out"; every intermediate runs at the ambient ToNearest.
/// ⚠ The doc comment ABOVE the function in rippled writes the root as
/// `sqrt(f2**2 - b/(B*f1))`; the CODE adds. Trust the code — with a minus the
/// root goes imaginary for any real deposit.
///
/// Pinned to mainnet #105719563 `98A6B1C4`: 4435 PLX into a 1793413.846406219
/// pool at fee 250 against 152935.7034799657 LPT mints 188.7469143481687.
pub(crate) fn lp_tokens_out(balance: Me, deposit: Me, lpt: Me, tfee: u16) -> Me {
    if balance.0 == 0 || deposit.0 == 0 || lpt.0 == 0 {
        return (0, 0);
    }
    let f1 = n_sub(N_ONE, fee_n(tfee), Rnd::Near);
    if f1.0 == 0 {
        return (0, 0);
    }
    let f2 = n_div(n_sub(N_ONE, fee_half_n(tfee), Rnd::Near), f1, Rnd::Near);
    let r = n_div(deposit, balance, Rnd::Near);
    let under = n_add(n_mul(f2, f2, Rnd::Near), n_div(r, f1, Rnd::Near), Rnd::Near);
    let c = n_sub(n_sqrt(under), f2, Rnd::Near);
    let num = n_sub(r, c, Rnd::Near);
    if num.0 == 0 {
        return (0, 0);
    }
    n_mul(lpt, n_div(num, n_add(N_ONE, c, Rnd::Near), Rnd::Near), Rnd::Down)
}

/// rippled `ammAssetIn` (AMMHelpers.cpp, "Equation 4") — the single-asset
/// deposit that mints exactly `lp_tokens`, i.e. Equation 3 solved for the
/// asset:
///
/// ```text
/// f1 = 1 - tfee/100000,  f2 = (1 - tfee/200000)/f1
/// t1 = lp_tokens/lpt,  t2 = 1 + t1,  d = f2 - t1/t2
/// a  = 1/t2^2,  b = 2d/t2 - 1/f1,  c = d^2 - f2^2
/// frac = (-b + sqrt(b^2 - 4ac)) / (2a)          (solveQuadraticEq)
/// in   = multiply(balance, frac, UPWARD)        ("maximize deposit")
/// ```
///
/// ⚠ `c` is NEGATIVE in every real regime (d < f2), so `-4ac` ADDS to the
/// discriminant; `b` is positive, so the root is `sqrt(D) - b` — a cancellation
/// of two ~1.0 values down to ~3e-4. rippled cancels identically, so mirroring
/// it is the point; do not "improve" it algebraically.
/// Signs are tracked explicitly because `Me` is unsigned.
pub(crate) fn amm_asset_in(balance: Me, lpt: Me, lp_tokens: Me, tfee: u16, xrp: bool) -> Option<Me> {
    if balance.0 == 0 || lpt.0 == 0 || lp_tokens.0 == 0 {
        return None;
    }
    let f1 = n_sub(N_ONE, fee_n(tfee), Rnd::Near);
    if f1.0 == 0 {
        return None;
    }
    let f2 = n_div(n_sub(N_ONE, fee_half_n(tfee), Rnd::Near), f1, Rnd::Near);
    let t1 = n_div(lp_tokens, lpt, Rnd::Near);
    let t2 = n_add(N_ONE, t1, Rnd::Near);
    let t1t2 = n_div(t1, t2, Rnd::Near);
    let (d, d_neg) = match n_cmp(f2, t1t2) {
        Ordering::Less => (n_sub(t1t2, f2, Rnd::Near), true),
        _ => (n_sub(f2, t1t2, Rnd::Near), false),
    };
    let a = n_div(N_ONE, n_mul(t2, t2, Rnd::Near), Rnd::Near);
    if a.0 == 0 {
        return None;
    }
    // b = 2d/t2 - 1/f1
    let lhs = n_div(n_mul(N_TWO, d, Rnd::Near), t2, Rnd::Near);
    let rhs = n_div(N_ONE, f1, Rnd::Near);
    let (b, b_neg) = if d_neg {
        (n_add(lhs, rhs, Rnd::Near), true)
    } else {
        match n_cmp(lhs, rhs) {
            Ordering::Less => (n_sub(rhs, lhs, Rnd::Near), true),
            _ => (n_sub(lhs, rhs, Rnd::Near), false),
        }
    };
    // c = d^2 - f2^2
    let (d2, f22) = (n_mul(d, d, Rnd::Near), n_mul(f2, f2, Rnd::Near));
    let (c, c_neg) = match n_cmp(d2, f22) {
        Ordering::Less => (n_sub(f22, d2, Rnd::Near), true),
        _ => (n_sub(d2, f22, Rnd::Near), false),
    };
    // D = b^2 - 4ac
    let b2 = n_mul(b, b, Rnd::Near);
    let four_ac = n_mul(n_mul(N_FOUR, a, Rnd::Near), c, Rnd::Near);
    let disc = if c_neg {
        n_add(b2, four_ac, Rnd::Near)
    } else if n_cmp(b2, four_ac) == Ordering::Less {
        return None;
    } else {
        n_sub(b2, four_ac, Rnd::Near)
    };
    let root = n_sqrt(disc);
    let num = if b_neg {
        n_add(root, b, Rnd::Near)
    } else if n_cmp(root, b) != Ordering::Greater {
        return None;
    } else {
        n_sub(root, b, Rnd::Near)
    };
    let frac = n_div(num, n_mul(N_TWO, a, Rnd::Near), Rnd::Near);
    if frac.0 == 0 {
        return None;
    }
    Some(to_amount(n_mul(balance, frac, Rnd::Up), xrp, Rnd::Up))
}

/// rippled `adjustAssetInByTokens` (AMMHelpers.cpp) — a single-asset deposit
/// pays what the ADJUSTED tokens are actually worth, not what was asked for.
///
/// `ammAssetIn` rounds the asset UP, so it can land ABOVE the requested amount;
/// rippled's comment is literally "Rounding didn't work the right way". It then
/// pulls the request down by the overshoot, re-derives the tokens from that
/// smaller amount, and re-prices — returning `min(amount, assetAdj)`.
///
/// #105813899 `2C8C37F9` is the specimen: 396118 drops requested, mainnet
/// deposits **396117** and mints for that, which is why its LP credit is
/// 0.0651047 lower than a mint priced on the full 396118.
pub(crate) fn adjust_asset_in_by_tokens(
    balance: Me,
    amount: Me,
    lpt: Me,
    tokens: Me,
    tfee: u16,
    xrp: bool,
) -> (Me, Me) {
    let Some(mut asset_adj) = amm_asset_in(balance, lpt, tokens, tfee, xrp) else {
        return (tokens, amount);
    };
    let mut tokens_adj = tokens;
    if n_cmp(asset_adj, amount) == Ordering::Greater {
        let over = n_sub(asset_adj, amount, Rnd::Near);
        let adj_amount = n_sub(amount, over, Rnd::Near);
        if adj_amount.0 == 0 {
            return (tokens, amount);
        }
        let t = adjust_lp_tokens(lpt, lp_tokens_out(balance, adj_amount, lpt, tfee), true);
        if t.0 == 0 {
            return (tokens, amount);
        }
        tokens_adj = t;
        match amm_asset_in(balance, lpt, tokens_adj, tfee, xrp) {
            Some(v) => asset_adj = v,
            None => return (tokens, amount),
        }
    }
    let deposited = match n_cmp(amount, asset_adj) {
        Ordering::Less => amount,
        _ => asset_adj,
    };
    (tokens_adj, deposited)
}

/// rippled `adjustAssetOutByTokens` (AMMHelpers.cpp) — the withdraw mirror of
/// `adjust_asset_in_by_tokens`. A single-asset withdrawal pays what the
/// ADJUSTED tokens are worth, not what was asked for.
///
/// `ammAssetOut` rounds the asset DOWN, but the retry still exists for the case
/// where it lands ABOVE the request: pull the request down by the overshoot,
/// re-derive the tokens from that smaller amount, re-price, and return
/// `min(amount, assetAdj)`.
pub(crate) fn adjust_asset_out_by_tokens(
    balance: Me,
    amount: Me,
    lpt: Me,
    tokens: Me,
    tfee: u16,
    xrp: bool,
) -> (Me, Me) {
    let Some(mut asset_adj) = amm_asset_out(balance, lpt, tokens, tfee, xrp) else {
        return (tokens, amount);
    };
    let mut tokens_adj = tokens;
    if n_cmp(asset_adj, amount) == Ordering::Greater {
        let over = n_sub(asset_adj, amount, Rnd::Near);
        let adj_amount = n_sub(amount, over, Rnd::Near);
        if adj_amount.0 == 0 {
            return (tokens, amount);
        }
        let Some(t) = lp_tokens_in(balance, adj_amount, lpt, tfee) else {
            return (tokens, amount);
        };
        let t = adjust_lp_tokens(lpt, t, false);
        if t.0 == 0 {
            return (tokens, amount);
        }
        tokens_adj = t;
        match amm_asset_out(balance, lpt, tokens_adj, tfee, xrp) {
            Some(v) => asset_adj = v,
            None => return (tokens, amount),
        }
    }
    let out = match n_cmp(amount, asset_adj) {
        Ordering::Less => amount,
        _ => asset_adj,
    };
    (tokens_adj, out)
}

/// rippled `adjustLPTokens` (AMMHelpers.cpp), reached from `adjustLPTokensOut`
/// (AMMDeposit.cpp:623) on EVERY deposit path once fixAMMv1_3 is enabled:
///
/// ```text
/// (lptAMMBalance + tokens) - lptAMMBalance      both ops Downward
/// ```
///
/// "Force rounding downward to ensure adjusted tokens are less or equal to
/// requested tokens." The round trip quantizes the mint to the POOL BALANCE's
/// ulp, which is coarser than the token amount's own precision whenever the
/// pool is large.
///
/// ⚠ Do not go looking for this in `adjustAmountsByLPTokens` — that one RETURNS
/// EARLY under fixAMMv1_3, because the adjustment moved out to each call site.
///
/// #105666725 `4FAE75AC` is the specimen: Equation 3 yields 32.98772104121774
/// against a pool of 22752296.08014551 whose 16-digit ulp is 1e-8, so mainnet
/// credits exactly **32.98772104** and the LP trust line it creates stores that
/// — not the raw root.
pub(crate) fn adjust_lp_tokens(lpt: Me, tokens: Me, deposit: bool) -> Me {
    if lpt.0 == 0 || tokens.0 == 0 {
        return tokens;
    }
    if deposit {
        return n_sub(n_add(lpt, tokens, Rnd::Down), lpt, Rnd::Down);
    }
    // Withdraw is `(tokens - lptAMMBalance) + lptAMMBalance`, and that
    // intermediate is NEGATIVE. Downward means toward -inf, so it rounds the
    // intermediate's MAGNITUDE UP and the positive result back DOWN — which is
    // not what feeding both steps through an unsigned `n_sub` would do.
    match n_cmp(tokens, lpt) {
        Ordering::Less => n_sub(lpt, n_sub(lpt, tokens, Rnd::Up), Rnd::Down),
        _ => tokens,
    }
}

/// Deposit-direction shorthand.
pub(crate) fn adjust_lp_tokens_out(lpt: Me, tokens: Me) -> Me {
    adjust_lp_tokens(lpt, tokens, true)
}

fn to_amount(x: Me, xrp: bool, rnd: Rnd) -> Me {
    if x.0 == 0 {
        return (0, 0);
    }
    if !xrp {
        return n_norm(x);
    }
    match rnd {
        Rnd::Up => (ox::me_rescale(x, 0, true), 0),
        Rnd::Down => (ox::me_rescale(x, 0, false), 0),
        // `rnd == Rnd::Up` alone silently FLOORED a nearest request. Only the
        // `limitOut` branch of `consume` asks for Near — every other call site
        // states Up or Down — so this is the one place it ever mattered, and
        // there it decides a whole drop.
        //
        // #105843839 C1F9FB1F: limitOut solves to 261170.9076568765 drops.
        // Floored that is 261170; mainnet takes 261171 and its 6 metadata hits
        // are all that single drop — two conserved pairs (taker/pool in AUDD
        // and in XRP) plus the rested offer reflecting them.
        //
        // ⚠ 261171 is very slightly WORSE than the taker's limit (7.4e-10
        // relative). rippled keeps it because `limitOut` actually reduced the
        // output, which arms the `adjustedRemOut` 1e-7 forgiveness — the
        // tolerance and this rounding are one mechanism, not two.
        Rnd::Near => {
            let t = ox::me_rescale(x, -1, false); // floor(x * 10)
            let (q, d) = (t / 10, t % 10);
            (if d >= 5 { q + 1 } else { q }, 0)
        }
    }
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
/// LP tokens burned by a SINGLE-ASSET withdrawal of `withdraw` from a pool
/// holding `balance` of that asset, against `total_lp` outstanding.
///
/// rippled `lpTokensIn` (AMMHelpers.cpp), documented at
/// `AMMWithdraw::singleWithdraw` as `t = T*(c - sqrt(c^2 - 4R))/2`:
///     fr   = withdraw / balance
///     f1   = fee fraction
///     c    = fr*f1 + 2 - f1
///     frac = (c - sqrt(c^2 - 4*fr)) / 2
///     t    = lptAMMBalance * frac        — rounded UP under fixAMMv1_3
///                                          ("maximize tokens in")
/// Asset paid out when `lp_tokens` are redeemed against a single side of the
/// pool — rippled `ammAssetOut` (AMMHelpers.cpp), equation 8:
///     t1   = lpTokens / lptAMMBalance
///     frac = (t1^2 - t1*(2 - f)) / (t1*f - 1)
///     out  = assetBalance * frac      — rounded DOWN ("minimize withdraw")
///
/// Both numerator and denominator are NEGATIVE for t1 < 1 (t1^2 < t1*(2-f), and
/// t1*f < 1), so we work with magnitudes: our Me is unsigned and n_sub would
/// underflow on the signed form.
pub(crate) fn amm_asset_out(
    asset_balance: Me,
    total_lp: Me,
    lp_tokens: Me,
    tfee: u16,
    xrp: bool,
) -> Option<Me> {
    if total_lp.0 == 0 || asset_balance.0 == 0 || lp_tokens.0 == 0 {
        return None;
    }
    let f = fee_n(tfee);
    let t1 = n_div(lp_tokens, total_lp, Rnd::Near);
    // |numerator| = t1*(2 - f) - t1^2
    let num = n_sub(
        n_mul(t1, n_sub(N_TWO, f, Rnd::Near), Rnd::Near),
        n_mul(t1, t1, Rnd::Near),
        Rnd::Near,
    );
    // |denominator| = 1 - t1*f
    let den = n_sub(N_ONE, n_mul(t1, f, Rnd::Near), Rnd::Near);
    if den.0 == 0 || num.0 == 0 {
        return None;
    }
    let frac = n_div(num, den, Rnd::Near);
    let out = n_mul(asset_balance, frac, Rnd::Down);
    Some(to_amount(out, xrp, Rnd::Down))
}

pub(crate) fn lp_tokens_in(balance: Me, withdraw: Me, total_lp: Me, tfee: u16) -> Option<Me> {
    if balance.0 == 0 || total_lp.0 == 0 || withdraw.0 == 0 {
        return None;
    }
    let fr = n_div(withdraw, balance, Rnd::Near);
    let f1 = fee_n(tfee);
    let c = n_sub(n_add(n_mul(fr, f1, Rnd::Near), N_TWO, Rnd::Near), f1, Rnd::Near);
    let c2 = n_mul(c, c, Rnd::Near);
    let four_fr = n_mul(N_FOUR, fr, Rnd::Near);
    if n_cmp(c2, four_fr) == Ordering::Less {
        return None;
    }
    let frac = n_div(n_sub(c, n_sqrt(n_sub(c2, four_fr, Rnd::Near)), Rnd::Near), N_TWO, Rnd::Near);
    if frac.0 == 0 {
        return None;
    }
    Some(n_mul(total_lp, frac, Rnd::Up))
}

/// `changeSpotPriceQuality` — the generated offer, or None when it cannot
/// actually reach `target`.
///
/// The generators below mirror rippled's two `getAMMOfferStart*` helpers,
/// including their single `reduceOffer` retry. What rippled then does, and we
/// did not, is CHECK the retry (AMMHelpers.h:385): an offer that still misses
/// the target quality is rejected outright — strictly, with no round-off
/// tolerance and no smaller fallback slice. The book tip that produced it is
/// simply skipped and the next one anchors the pool instead.
///
/// #106143011 4607B92B is the case that pins it. Against the RLUSD/XRP pool
/// rippled's 4th turn logs `changeSpotPriceQuality failed: ... 2.364604055435306
/// 2151512` and moves on to the next tip for a single 112.4170315686629 slice;
/// we took the rejected 2.3646 crumb AND a reduced 107.6832 after it, then
/// overshot again on a 6th turn rippled never reaches. The offer is worse than
/// its target by 3.3e-10 — the check has to be exact to see that, which is why
/// it only became reachable once IOU addition stopped truncating
/// (`stamount_signed_add`); a one-ulp-low pool balance had been putting the
/// same offer just INSIDE the target.
fn anchored_offer(
    pool_in: Me,
    pool_out: Me,
    in_xrp: bool,
    out_xrp: bool,
    target: u64,
    tfee: u16,
) -> Option<(Me, Me)> {
    let (i, o) = anchored_offer_generate(pool_in, pool_out, in_xrp, out_xrp, target, tfee)?;
    if rate_of(i, o) > target {
        return None;
    }
    Some((i, o))
}

fn anchored_offer_generate(
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

/// The pool's FEELESS spot quality — rippled's OPTIMISTIC strand
/// `qualityUpperBound`, which decides only whether a strand is worth
/// activating at all (StrandFlow.h:696-699 `qualityUpperBound(sb, *strand) <
/// *limitQuality => continue`). Execution uses the fee-inclusive spot in
/// `consume`; this is the best the pool could conceivably do, and being an
/// upper bound it must NOT charge the fee.
///
/// The distinction has teeth: it is the difference between a strand that runs
/// and reaps dead offers on its way down the book, and one rippled never
/// builds at all.
pub(crate) fn spot_upper_bound(sandbox: &Sandbox, amm: &Amm, pays_leg: &Leg, gets_leg: &Leg) -> u64 {
    let (pool_in, pool_out) = pool_balances(sandbox, amm, pays_leg, gets_leg);
    if pool_in.0 == 0 || pool_out.0 == 0 {
        return u64::MAX;
    }
    match rate_of(pool_in, pool_out) {
        0 => u64::MAX,
        r => r,
    }
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

/// rippled's `AMMLiquidity::maxOffer` — the SINGLE-PATH slice.
///
/// `getOffer` picks the shape by `ammContext_.multiPath()`:
///   * multi-path  -> `generateFibSeqOffer` (our `fib_slice`)
///   * single path, no clobQuality -> `maxOffer`
///   * single path, with clobQuality -> `changeSpotPriceQuality`
///
/// `maxOffer` takes 99% of the pool's OUT side and asks `swapAssetOut` what
/// that costs, so its AMOUNTS are enormous while its comparison `quality()`
/// stays the pool SPOT (`Quality{balances}`). rippled then lets BookStep clamp
/// the actual fill; the point is that ONE pass is priced and judged, not a
/// sequence of small slices each judged on its own.
///
/// ```text
/// out = floor(balances.out * 0.99)          (maxOut, RoundingMode::Downward)
/// in  = swapAssetOut(balances, out, fee)
/// ```
///
/// Verified against the FFI trace on 2026-08-07. #106137477, pool 21 830 175
/// drops / 115.121893423695 BBRL at fee 1000:
///     `getOffer, created 2183017500/XRP 113.970674489458/BBRL`
/// and 115.121893423695 * 0.99 = 113.970674489458 exactly, swapAssetOut of
/// which is 2 183 017 500. #106134431's BTC pool matches the same way
/// (1.692470077908021 * 0.99 = 1.675545377128941).
/// rippled's `QualityFunction` — a strand's AVERAGE quality as a function of
/// its output: `1/q(out) = m*out + b`, where `q` is expressed as OUT PER IN.
///
/// A CLOB step is constant (`m = 0`); an AMM step degrades linearly as more is
/// taken, which is the whole point — it lets `limitOut` solve for the output
/// that lands the strand exactly ON the taker's quality limit instead of
/// taking the maximum and then discarding the pass (StrandFlow.h:345-395,
/// QualityFunction.cpp).
///
/// ⚠ `m` is NEVER positive in any shape rippled builds — 0 for a CLOB,
/// `-cfee/in` for an AMM, and `combine` preserves that — so the MAGNITUDE is
/// tracked here. `Me`'s mantissa is unsigned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct QualityFn {
    /// `b`: the value at `out = 0` — the strand's best (spot) quality, out per in.
    pub(crate) b: Me,
    /// `|m|`: how fast that quality decays per unit of output.
    pub(crate) mneg: Me,
}

impl QualityFn {
    /// `QualityFunction(quality, CLOBLikeTag)` — `m = 0`, `b = 1/rate`.
    pub(crate) fn clob(rate: Me) -> Option<Self> {
        (rate.0 != 0).then(|| Self { b: n_div(N_ONE, rate, Rnd::Near), mneg: (0, 0) })
    }

    /// `QualityFunction(amounts, tfee, AMMTag)` — `m = -cfee/in`,
    /// `b = out * cfee / in`, with `cfee = feeMult(tfee) = 1 - tfee/100000`.
    pub(crate) fn amm(pool_in: Me, pool_out: Me, tfee: u16) -> Option<Self> {
        if pool_in.0 == 0 || pool_out.0 == 0 {
            return None;
        }
        let cfee = n_sub(N_ONE, fee_n(tfee), Rnd::Near);
        Some(Self {
            b: n_div(n_mul(pool_out, cfee, Rnd::Near), pool_in, Rnd::Near),
            mneg: n_div(cfee, pool_in, Rnd::Near),
        })
    }

    /// `combine`: `m_ += b_ * qf.m_;  b_ *= qf.b_;` — steps in strand order,
    /// source first.
    pub(crate) fn combine(&mut self, next: &QualityFn) {
        self.mneg = n_add(self.mneg, n_mul(self.b, next.mneg, Rnd::Near), Rnd::Near);
        self.b = n_mul(self.b, next.b, Rnd::Near);
    }

    /// `outFromAvgQ`: solve `m*out + b = 1/limit` for `out`, i.e.
    /// `out = (1/limit - b) / m`. Both sides are negated here so the unsigned
    /// form is `(b - 1/limit) / |m|`.
    ///
    /// `None` when the function is CONSTANT (no AMM in the strand — nothing to
    /// solve) or when `out <= 0` (the strand's best quality already misses the
    /// limit, so no output size can rescue it). rippled returns `remainingOut`
    /// unchanged in both cases, which also leaves `adjustedRemOut` false.
    ///
    /// rippled evaluates the whole expression under
    /// `Number::RoundingMode::Upward`.
    pub(crate) fn out_from_avg_q(&self, limit_rate: Me) -> Option<Me> {
        if self.mneg.0 == 0 || limit_rate.0 == 0 {
            return None;
        }
        let inv = n_div(N_ONE, limit_rate, Rnd::Up);
        if n_cmp(self.b, inv) != Ordering::Greater {
            return None;
        }
        let out = n_div(n_sub(self.b, inv, Rnd::Up), self.mneg, Rnd::Up);
        (out.0 != 0).then_some(out)
    }
}

/// ⚠ NOT WIRED IN YET, deliberately — see the note on `max_offer_amounts`.
#[allow(dead_code)]
pub(crate) fn max_offer(
    sandbox: &Sandbox,
    amm: &Amm,
    pays_leg: &Leg,
    gets_leg: &Leg,
) -> Option<(Me, Me)> {
    let pool_in = holds(sandbox, &amm.account, gets_leg);
    let pool_out = holds(sandbox, &amm.account, pays_leg);
    max_offer_amounts(pool_in, pool_out, amm.tfee, pays_leg.xrp, gets_leg.xrp)
}

/// The pure arithmetic of `max_offer`, split out so it can be pinned directly
/// against the traced `getOffer, created …` amounts without a sandbox.
///
/// ⚠ **Verified but NOT WIRED IN.** Substituting this for the fib slice in
/// `cross_bridged`'s single-path fill is correct for #106137477 (rippled
/// prices ONE maxOffer-shaped pass and rejects it whole, where our four small
/// slices each squeak inside the limit) but it REGRESSES #105940336
/// `CA2C624ED031`, a live case where mainnet crosses: the bigger pass lands
/// outside the limit and our judge kills it, so we rest where mainnet fills.
/// `a_single_path_leg_is_priced_by_its_pools_spot_quality` catches it.
///
/// The missing half is how BookStep CLAMPS the pass — rippled's maxOffer is
/// enormous and the actual fill is bounded by the request and the quality
/// threshold, then RE-PRICED through `swapAssetIn`/`swapAssetOut` at the size
/// actually taken. Sizing alone, without that clamp, moves the number the
/// wrong way on a calibrated case — exactly the "needs both halves" this
/// area's history warns about. Left here because the arithmetic is settled and
/// trace-exact; the clamp is the next piece.
#[allow(dead_code)]
pub(crate) fn max_offer_amounts(
    pool_in: Me,
    pool_out: Me,
    tfee: u16,
    out_xrp: bool,
    in_xrp: bool,
) -> Option<(Me, Me)> {
    if pool_in.0 == 0 || pool_out.0 == 0 {
        return None;
    }
    // maxOut: `Number const res = out * Number{99,-2};` then
    // `toAmount<T>(asset, res, RoundingMode::Downward)`.
    //
    // ⚠ The two roundings are NOT the same one. The MULTIPLY happens in
    // Number's own precision under its default nearest mode; `Downward`
    // governs only the later conversion to the amount type. Doing the multiply
    // downward too costs one ulp — #106134431's BTC pool
    // (1.692470077908021 * 0.99) truncates to …128940 where rippled traces
    // …128941. #106137477's BBRL pool is insensitive (its tail is an exact
    // half), so ONE specimen alone would not have caught this.
    let out = to_amount(n_mul(pool_out, (9_900_000_000_000_000, -16), Rnd::Near), out_xrp, Rnd::Down);
    if out.0 == 0 || n_cmp(out, pool_out) != Ordering::Less {
        return None;
    }
    let inp = swap_asset_out(pool_in, pool_out, out, tfee, in_xrp)?;
    (inp.0 > 0).then_some((inp, out))
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

    /// `ammLPTokens` = sqrt(a*b), rounded DOWNWARD, with XRP in DROPS.
    ///
    /// Pinned to mainnet #105666951 `07DC0E81`: 100 XRC against "10000000"
    /// drops mints exactly 31622.77660168379. Reading the XRP side as 10 gives
    /// 31.6 — a 1000x error the placeholder was hiding.
    #[test]
    fn amm_lp_tokens_matches_mainnets_initial_mint() {
        let minted = amm_lp_tokens((100, 0), (10_000_000, 0));
        assert_eq!(crate::tx::offer::me_to_value_string(minted), "31622.77660168379");

        // Downward, never up: sqrt(2) must truncate at 16 digits, and a perfect
        // square must not drift off its exact root.
        assert_eq!(
            crate::tx::offer::me_to_value_string(amm_lp_tokens((2, 0), (1, 0))),
            "1.414213562373095"
        );
        assert_eq!(crate::tx::offer::me_to_value_string(amm_lp_tokens((4, 0), (4, 0))), "4");
        assert_eq!(amm_lp_tokens((0, 0), (10, 0)), (0, 0));
    }

    /// Equation 3 against #105719563 `98A6B1C4`'s pool: 4435 PLX into
    /// 1793413.846406219 at fee 250, against 152935.7034799657 LPT.
    ///
    /// ⚠ Asserted with a TOLERANCE, deliberately. `c = sqrt(f2^2 + r/f1) - f2`
    /// subtracts two ~1.0012 values to get ~0.00124, so ~3 digits die in the
    /// cancellation and the last few digits of the result depend on the
    /// mantissa width — and rippled's `Number` has three configurable scales
    /// (Small 16-digit, LargeLegacy/Large 19-digit) where our `Me` only ever
    /// models 16. Pinning an exact string here would assert our own rounding,
    /// not rippled's. Exactness is judged where it is observable: DX_VALCHECK
    /// on the stored LPTokenBalance and LP trust line.
    #[test]
    fn single_asset_deposit_mints_mainnets_lp_tokens() {
        let minted = lp_tokens_out(
            me("1793413.846406219"),
            me("4435"),
            me("152935.7034799657"),
            250,
        );
        let got: f64 = crate::tx::offer::me_to_value_string(minted).parse().unwrap();
        let exact = 188.746914348168_72_f64; // 50-digit reference of Equation 3
        assert!(
            ((got - exact) / exact).abs() < 1e-11,
            "Equation 3 gave {got}, expected ≈{exact}"
        );

        // Shape guards: a fee-free pool still mints, and nothing is minted for
        // a zero deposit or against a pool with no tokens outstanding.
        assert!(lp_tokens_out(me("1000"), me("100"), me("500"), 0).0 > 0);
        assert_eq!(lp_tokens_out(me("1000"), (0, 0), me("500"), 250), (0, 0));
        assert_eq!(lp_tokens_out(me("1000"), me("100"), (0, 0), 250), (0, 0));
        // fixAMMv1_3 then quantizes the mint to the POOL's ulp: #105666725
        // 4FAE75AC's pool is 22752296.08014551 (ulp 1e-8), so Equation 3's
        // 32.98772104121774 is credited as exactly 32.98772104.
        assert_eq!(
            crate::tx::offer::me_to_value_string(adjust_lp_tokens_out(
                me("22752296.08014551"),
                me("32.98772104121774")
            )),
            "32.98772104"
        );
        // A pool small enough to represent the whole amount leaves it alone.
        assert_eq!(adjust_lp_tokens_out(me("10"), me("1.5")), me("1.5"));

        // Charging a fee mints strictly fewer tokens for the same deposit.
        let free = lp_tokens_out(me("1000"), me("100"), me("500"), 0);
        let charged = lp_tokens_out(me("1000"), me("100"), me("500"), 1000);
        assert!(n_cmp(charged, free) == Ordering::Less);
    }

    fn me(s: &str) -> Me {
        let (int, frac) = match s.split_once('.') {
            Some((a, b)) => (a, b),
            None => (s, ""),
        };
        let digits = format!("{int}{frac}");
        let m: u128 = digits.trim_start_matches('0').parse().unwrap();
        n_norm((m, -(frac.len() as i32)))
    }

    /// `QualityFn` must reproduce rippled's `QualityFunction` algebra, and the
    /// solved output must ROUND-TRIP: feeding it back through `m*out + b` has
    /// to land on `1/limit`. That check needs no external number and catches a
    /// sign or unit error immediately.
    #[test]
    fn a_solved_output_lands_exactly_on_the_limit() {
        // #106134431's leg B pool: 107 107 916 717 drops / 1.692470077908021
        // BTC, trading fee 0.
        let pool_in = (107_107_916_717u128, 0i32);
        let pool_out = me("1.692470077908021");
        let amm = QualityFn::amm(pool_in, pool_out, 0).expect("pool is live");
        assert_eq!(amm.b, n_div(pool_out, pool_in, Rnd::Near), "b = out/in at fee 0");
        assert_eq!(amm.mneg, n_div(N_ONE, pool_in, Rnd::Near), "|m| = 1/in at fee 0");

        // Leg A is a CLOB at 0.00000102695147247967 RLUSD per drop.
        let clob = QualityFn::clob(me("0.00000102695147247967")).expect("live book");
        assert_eq!(clob.mneg, (0, 0), "a CLOB step is constant");

        // Strand order: source first (leg A), then leg B.
        let mut qf = clob;
        qf.combine(&amm);
        assert!(qf.mneg.0 != 0, "the composed function is NOT constant");

        let limit = me("65044.98739782016");
        let out = qf.out_from_avg_q(limit).expect("spot beats the limit, so a cap exists");

        // ROUND TRIP: 1/q(out) = b - |m|*out must equal 1/limit.
        let back = n_sub(qf.b, n_mul(qf.mneg, out, Rnd::Near), Rnd::Near);
        let inv = n_div(N_ONE, limit, Rnd::Near);
        let (bm, be) = (n_norm(back), n_norm(inv));
        let rel = if bm.1 == be.1 {
            (bm.0 as i128 - be.0 as i128).unsigned_abs()
        } else {
            u128::MAX
        };
        assert!(rel <= 4, "round-trip lands on 1/limit (within {rel} ulp)");

        // And against an independent reference (40-digit Decimal, same algebra):
        // (b - 1/limit)/|m| = 0.001415360439672 BTC.
        let want = me("0.001415360439672");
        let ratio = n_div(out, want, Rnd::Near);
        assert!(
            n_cmp(ratio, me("0.999999")) == Ordering::Greater
                && n_cmp(ratio, me("1.000001")) == Ordering::Less,
            "cap {out:?} must match the reference 0.001415360439672 BTC",
        );
    }

    /// A strand with no AMM has a CONSTANT quality function, so there is
    /// nothing to solve — rippled returns `remainingOut` unchanged, leaving
    /// `adjustedRemOut` false. Getting this wrong would silently enable the
    /// 1e-7 judge tolerance on pure-CLOB strands.
    #[test]
    fn a_pure_clob_strand_has_no_solution() {
        let mut qf = QualityFn::clob(me("0.5")).expect("live");
        qf.combine(&QualityFn::clob(me("2")).expect("live"));
        assert_eq!(qf.mneg, (0, 0), "still constant after combining two CLOBs");
        assert_eq!(qf.out_from_avg_q(me("1")), None, "nothing to solve");
    }

    /// When the strand's BEST quality already misses the limit, no output size
    /// rescues it: `out <= 0` -> None.
    #[test]
    fn a_strand_whose_spot_misses_the_limit_has_no_positive_solution() {
        let amm = QualityFn::amm((1_000_000u128, 0i32), me("1"), 0).expect("live");
        // spot = 1e-6 out per in; a limit demanding 1e-3 out per in is
        // unreachable (rate = 1000 in per out).
        assert_eq!(amm.out_from_avg_q(me("1000")), None);
    }

    /// `max_offer` must reproduce rippled's `AMMLiquidity::maxOffer` exactly.
    ///
    /// Both amounts come from the FFI trace of 2026-08-07, not from working the
    /// formula backwards:
    ///   #106137477 pool 21 830 175 drops / 115.121893423695 BBRL, fee 1000 ->
    ///     `getOffer, created 2183017500/XRP 113.970674489458/BBRL`
    ///   #106134431 pool 107 107 916 717 drops / 1.692470077908021 BTC, fee 0 ->
    ///     `getOffer, created 10605698837763/XRP 1.675545377128941/BTC`
    /// In both the pool's IN side is XRP and the OUT side an IOU.
    #[test]
    fn max_offer_matches_rippleds_traced_amounts() {
        // #106137477 — BBRL pool, 1% trading fee.
        let (inp, out) = max_offer_amounts(
            (21_830_175u128, 0i32),
            me("115.121893423695"),
            1000,
            /*out_xrp=*/ false,
            /*in_xrp=*/ true,
        )
        .expect("maxOffer exists");
        assert_eq!(out, me("113.970674489458"), "maxOut is 99% of the out side");
        assert_eq!(inp, (2_183_017_500u128, 0i32), "in is swapAssetOut of that");

        // #106134431 — BTC pool, no trading fee.
        let (inp2, out2) = max_offer_amounts(
            (107_107_916_717u128, 0i32),
            me("1.692470077908021"),
            0,
            false,
            true,
        )
        .expect("maxOffer exists");
        assert_eq!(out2, me("1.675545377128941"), "99% of the BTC side");
    }

    /// The strand-activation bound must NOT charge the trading fee. rippled
    /// filters strands on an OPTIMISTIC `qualityUpperBound` — the best the
    /// strand could conceivably do — and only then does execution apply the
    /// fee. Charging it in the bound makes a strand look unusable that rippled
    /// still builds, and a strand that is never built reaps nothing.
    ///
    /// #105922825 851508DADF49 lives entirely in that gap: pool
    /// 2005007.508042841 RLUSD / 1869858318641 drops at TradingFee 208, against
    /// a limit of 1.0723 RLUSD per 1e6 drops. Feeless the pool is INSIDE the
    /// limit; with the 0.208% fee applied it is outside it. Mainnet builds the
    /// strand and reaps the expired book tip; charging the fee here would not.
    #[test]
    fn the_strand_activation_bound_does_not_charge_the_fee() {
        let pool_in = me("2005007.508042841");
        let pool_out = (1_869_858_318_641u128, 0i32);
        let limit = rate_of(me("1.0723"), (1_000_000, 0));

        let feeless = rate_of(pool_in, pool_out);
        let with_fee = rate_of(
            pool_in,
            n_mul(pool_out, n_sub(N_ONE, fee_n(208), Rnd::Near), Rnd::Near),
        );

        assert!(feeless < with_fee, "the fee can only make the pool look worse");
        assert!(
            feeless <= limit,
            "feeless {feeless:x} is inside the limit {limit:x} — rippled builds the strand",
        );
        assert!(
            with_fee > limit,
            "fee-inclusive {with_fee:x} is outside it — which is why nothing crosses",
        );
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
