//! OfferCreate and OfferCancel transaction types.
//!
//! OfferCreate: places a new order on the DEX, crossing against existing offers.
//! OfferCancel: removes an existing order.
//!
//! Offer crossing walks the order book and matches compatible offers.
//! When offers cross: balances adjust, consumed offers are deleted,
//! partial fills modify the remaining amount.
//!
//! # DEAD CODE WARNING
//!
//! This module is **not called** by the live validator. Production transaction
//! application is delegated to rippled's C++ engine via FFI — see
//! `crates/xrpl-ffi/src/lib.rs` and `crates/xrpl-node/src/ffi_engine.rs`.
//!
//! This code is retained as a reference implementation / learning artifact.
//! Tests in this module prove the code works in isolation; they do NOT prove
//! the validator is correct.
//!
//! If you are adding a new amendment or tx type: add it to the FFI path,
//! not here. See `ffi/ARCHITECTURE.md` for the architectural decision record.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use xrpl_core::types::Hash256;

/// Parse an Amount field — returns (drops, is_xrp).
/// XRP amounts are strings of drops. IOU amounts are objects with currency/issuer/value.
fn parse_amount(val: &serde_json::Value) -> Option<Amount> {
    match val {
        serde_json::Value::String(s) => {
            let drops: u64 = s.parse().ok()?;
            Some(Amount::Xrp(drops))
        }
        serde_json::Value::Number(n) => {
            Some(Amount::Xrp(n.as_u64()?))
        }
        serde_json::Value::Object(obj) => {
            let currency = obj.get("currency")?.as_str()?.to_string();
            let issuer = obj.get("issuer")?.as_str()?.to_string();
            let value: f64 = obj.get("value")?.as_str()?.parse().ok()?;
            if !value.is_finite() || value < 0.0 {
                return None;
            }
            Some(Amount::Iou { currency, issuer, value })
        }
        _ => None,
    }
}

/// 20-byte currency code of an amount (XRP = zeros; 3-char ASCII at bytes
/// 12..15; 40-hex verbatim).
pub(crate) fn amount_currency20(v: &serde_json::Value) -> Option<[u8; 20]> {
    match v {
        serde_json::Value::String(_) => Some([0u8; 20]), // XRP
        serde_json::Value::Object(o) => {
            let c = o.get("currency")?.as_str()?;
            if c == "XRP" {
                return Some([0u8; 20]);
            }
            if c.len() == 40 {
                let b = hex::decode(c).ok()?;
                return <[u8; 20]>::try_from(b.as_slice()).ok();
            }
            let cb = c.as_bytes();
            if cb.is_empty() || cb.len() > 8 {
                return None;
            }
            let mut b = [0u8; 20];
            b[12..12 + cb.len()].copy_from_slice(cb);
            Some(b)
        }
        _ => None,
    }
}

/// 20-byte issuer of an amount (XRP = account-zero; hex or base58 r-address).
pub(crate) fn amount_issuer20(v: &serde_json::Value) -> Option<[u8; 20]> {
    match v {
        serde_json::Value::String(_) => Some([0u8; 20]),
        serde_json::Value::Object(o) => {
            let s = o.get("issuer")?.as_str()?;
            if let Ok(b) = hex::decode(s) {
                if b.len() == 20 {
                    return <[u8; 20]>::try_from(b.as_slice()).ok();
                }
            }
            xrpl_core::types::AccountId::from_address(s).ok().map(|a| a.0)
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
enum Amount {
    Xrp(u64),
    Iou { currency: String, issuer: String, value: f64 },
}

impl Amount {
    fn is_xrp(&self) -> bool {
        matches!(self, Amount::Xrp(_))
    }

    /// Compute exchange rate as f64 (self per 1 unit of other).
    fn rate_against(&self, other: &Amount) -> f64 {
        let self_val = match self {
            Amount::Xrp(d) => *d as f64,
            Amount::Iou { value, .. } => *value,
        };
        let other_val = match other {
            Amount::Xrp(d) => *d as f64,
            Amount::Iou { value, .. } => *value,
        };
        if other_val == 0.0 { return f64::MAX; }
        self_val / other_val
    }
}

// ===========================================================================
// Integer crossing engine — (mantissa u128, exponent i32) arithmetic, faithful
// keylet-quality math (see keylet::offer_quality), directed rounding at fill
// boundaries (taker pays UP, receives DOWN — rippled Quality::ceil_in/out).
// The differential gate compares (key, kind) sets; exact fill values decide
// full-vs-partial maker consumption, so the integer math matters even though
// stored byte values are not compared.
// ===========================================================================

/// Mainnet owner reserve (drops): base 1 XRP + 0.2 XRP per owned object.
const XRP_RESERVE_BASE: u128 = 1_000_000;
pub(crate) const XRP_RESERVE_INC: u128 = 200_000;

pub(crate) type Me = (u128, i32);

#[derive(Clone, Debug, PartialEq)]
pub struct Leg {
    pub xrp: bool,
    pub cur: [u8; 20],
    pub issuer: [u8; 20],
}

pub(crate) fn leg_of(v: &serde_json::Value) -> Option<Leg> {
    Some(Leg { xrp: v.is_string(), cur: amount_currency20(v)?, issuer: amount_issuer20(v)? })
}

pub(crate) fn decode20(s: &str) -> Option<[u8; 20]> {
    if let Ok(b) = hex::decode(s) {
        if b.len() == 20 {
            return <[u8; 20]>::try_from(b.as_slice()).ok();
        }
    }
    xrpl_core::types::AccountId::from_address(s).ok().map(|a| a.0)
}

pub(crate) fn me_rescale(a: Me, e: i32, ceil: bool) -> u128 {
    if a.1 >= e {
        let d = (a.1 - e).min(38) as u32;
        a.0.saturating_mul(10u128.saturating_pow(d))
    } else {
        let d = 10u128.saturating_pow(((e - a.1).min(38)) as u32);
        if ceil { a.0.div_ceil(d) } else { a.0 / d }
    }
}

pub(crate) fn me_cmp(a: Me, b: Me) -> std::cmp::Ordering {
    let e = a.1.min(b.1);
    me_rescale(a, e, false).cmp(&me_rescale(b, e, false))
}

pub(crate) fn me_is_zero(a: Me) -> bool {
    a.0 == 0
}

/// STAmount-faithful RUNNING-REMAINDER subtraction. rippled's walk
/// remainders are STAmounts: every subtraction re-rounds to 16 significant
/// digits (Number half-even), so a subtrahend below the minuend's 16-digit
/// window VANISHES and the remainder stays canonical. Our exact me kept the
/// full difference — l106267220 round 5 (the mulRatio-campaign cliff):
/// a 2-drop residual offer priced at 1e-16 turned the trial's 1e6 grant
/// into a 22-DIGIT mantissa; every later comparison rescaled into
/// saturation, the walk stalled after the dust, the balance-diff measured
/// ZERO (the 1e-16 sits below the granted line's ulp), and size_book_hop's
/// ladder read "no liquidity" — the unbounded sentinel then bought 653.86
/// for 2 drops and the payment failed tecPATH_PARTIAL.
///
/// Kept EXACT while the difference fits 17 digits — the regime every
/// calibrated specimen lives in (e.g. #105923760's final-fill truncation,
/// #105831615's 4.38e-11 round accounting) — and normalized half-even to
/// 16 only when operand misalignment (≥2 orders past the STAmount window)
/// would mint an 18+-digit mantissa.
pub(crate) fn me_sub16(a: Me, b: Me) -> Me {
    let r = me_sub(a, b);
    let mut digits = 0u32;
    let mut x = r.0;
    while x > 0 {
        x /= 10;
        digits += 1;
    }
    if digits < 18 {
        return r;
    }
    let over = digits - 16;
    let p = 10u128.pow(over);
    let q = r.0 / p;
    let rem = r.0 % p;
    let m = match (2 * rem).cmp(&p) {
        std::cmp::Ordering::Greater => q + 1,
        std::cmp::Ordering::Equal => q + (q & 1),
        std::cmp::Ordering::Less => q,
    };
    (m, r.1 + over as i32)
}

pub(crate) fn me_sub(a: Me, b: Me) -> Me {
    let e = a.1.min(b.1);
    (me_rescale(a, e, false).saturating_sub(me_rescale(b, e, false)), e)
}

pub(crate) fn me_norm(mut a: Me) -> Me {
    while a.0 >= 100_000_000_000_000_000_000 {
        a.0 /= 10;
        a.1 += 1;
    }
    a
}

/// a*b/c with directed rounding (mantissas kept ≤ ~1e20 so the product fits).
/// `a * b` rounded UP to 16 significant digits — rippled's
/// `mulRound(a, b, asset, roundUp=true)`, which is how `Quality::ceil_out`
/// prices an out-limited fill (`result.in = mulRound(limit, quality.rate(),
/// …)`, Quality.cpp `ceilOutImpl`).
///
/// `norm16` cannot serve: it TRUNCATES the mantissa, and the discarded digits
/// are exactly what decides the achieved-quality judge. The drop must be a
/// single division so the round-up is applied once — dropping digits one at a
/// time and carrying each non-zero remainder over-rounds by up to a digit.
/// rippled's `AMMContext` for one flow. `ammIters_` is FLOW-wide — it counts
/// iterations that consumed AMM liquidity, not per pool — while each
/// `AMMLiquidity` captures its own `initialBalances_` the first time it is
/// used and sizes every later fib slice against those, not against the
/// balances as they move (AMMLiquidity.cpp `generateFibSeqOffer`).
///
/// Only a MULTI-STRAND payment needs this. `AMMContext::multiPath()` is
/// `activeStrands.size() > 1`, and with one strand rippled sizes the pool by
/// `maxOffer` instead — which is exactly what `amm_swap::consume` already
/// does, so single-strand callers pass None and are untouched.
#[derive(Clone, Default, Debug)]
pub(crate) struct AmmFib {
    pub(crate) iters: u32,
    // Set when a pool moved value during the CURRENT pass; the round loop
    // folds it into `iters` once per WINNING round — rippled's
    // `ammContext.update()` counts an AMM iteration once per driver
    // iteration, however many pools the strand touched. Incrementing per
    // consumption ran the fib at double pace on two-pool strands:
    // #106360400 E133BD25's slices went ×1,2,5,13 where rippled's go
    // ×1,1,2,3,5,8,13 (FLOWDRIVER-DESIGN §5.1).
    pub(crate) used: bool,
    pub(crate) init: std::collections::BTreeMap<[u8; 20], (Me, Me)>,
}

pub(crate) fn mul_round16_up(a: Me, b: Me) -> Me {
    // rippled's `mulRound(v1, v2, asset, roundUp = true)` — STAmount.cpp
    // `mulRoundImpl<canonicalizeRound, DontAffectNumberRoundMode>` (:1705).
    // It IS a round-up, but a LOSSY one, and the loss is the point:
    //
    //   amount = muldivRound(m1, m2, 1e14, 1e14 - 1);   // ceil -> 17-18 digits
    //   canonicalizeRound(false, amount, offset, true): // :1477
    //       while (value > 10 * kMaxValue) { value /= 10; ++offset; }  // TRUNCATE to 17
    //       value += 9; value /= 10; ++offset;                         // ceil on digit 17
    //
    // So everything below the 17th digit is DISCARDED before the ceiling is
    // applied. Rounding up on the full remainder — what we did — is the
    // `canonicalizeRoundStrict` behaviour, which rippled reserves for
    // `mulRoundStrict` (:1711) and does NOT use here. rippled's own comment at
    // :1511 names the difference: the original "ignored low order bits that
    // could influence rounding decisions".
    //
    // Being stricter than rippled is a defect, same as being more precise: our
    // partial fills came out ONE ULP high whenever digit 17 was 0 and something
    // below it was not, which is the 1-lsd class — 8 offer residuals all
    // exactly one low, each with its trust line paired against it.
    //
    // ⚠ The `MightSaveRound(TowardsZero)` on the STAmount construction is NOT
    // the rounding; it only stops `Number` rounding a SECOND time. Reading it
    // as "mulRound truncates" is wrong, and
    // `an_out_limited_fill_is_priced_through_the_offers_encoded_quality`
    // (anchored to rippled's own `limitQuality` log for #105924683) catches it.
    let (a, b) = (norm16(a), norm16(b));
    if a.0 == 0 || b.0 == 0 {
        return (0, 0);
    }
    const TEN14: u128 = 100_000_000_000_000;
    const MAXV: u128 = 9_999_999_999_999_999;
    // 16-digit mantissas => product < 1e32, well inside u128.
    let prod = a.0 * b.0;
    let mut m = prod / TEN14 + u128::from(prod % TEN14 != 0);
    let mut e = a.1 + b.1 + 14;
    if m > MAXV {
        while m > 10 * MAXV {
            m /= 10;
            e += 1;
        }
        m = (m + 9) / 10;
        e += 1;
    }
    while m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    (m, e)
}

/// `mulRound(a, b, iouAsset, roundUp = false)` — the floor sibling of
/// `mul_round16_up`: product floored at the 14-shift, canonicalized by pure
/// truncation (no ceil step). `TOffer::limitOut(…, roundUp=false)` lands
/// here when a maker's FUNDS bound the fill (BookStep.cpp:779-793) — the in
/// side of a funds-limited partial rounds DOWN, "so that the quality of an
/// offer left in the ledger is as good or better than its book page".
pub(crate) fn mul_round16_down(a: Me, b: Me) -> Me {
    let (a, b) = (norm16(a), norm16(b));
    if a.0 == 0 || b.0 == 0 {
        return (0, 0);
    }
    const TEN14: u128 = 100_000_000_000_000;
    const MAXV: u128 = 9_999_999_999_999_999;
    let prod = a.0 * b.0;
    let mut m = prod / TEN14;
    let mut e = a.1 + b.1 + 14;
    while m > MAXV {
        m /= 10;
        e += 1;
    }
    while m > 0 && m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    (m, e)
}

/// `mulRoundStrict(a, b, XRP, round_up)` — `a * b` as whole DROPS.
///
/// `TOffer::limitOut` does not call `mulRound`: "It turns out that the ceil_out
/// implementation has some slop in it, which ceil_out_strict removes"
/// (Offer.h:200-202). For an IOU result the two canonicalisers are byte
/// identical — `canonicalizeRoundStrict`'s non-integral branch is a verbatim
/// copy of `canonicalizeRound`'s — which is why `mul_round16_up` is right
/// everywhere else. For an INTEGRAL (XRP) result they diverge, and only the
/// strict form keeps the digits that decide a whole drop
/// (STAmount.cpp:1477 vs :1516). After dividing down to offset −1:
///
///   canonicalizeRound        value += 9;                                  value /= 10;
///   canonicalizeRoundStrict  value += (hadRemainder && roundUp) ? 10 : 9; value /= 10;
///
/// With `roundUp` that `10` makes it a true CEILING of the exact product,
/// while the legacy form discards everything below a TENTH OF A DROP before
/// the carry can happen.
///
/// #105924683 7CB7E834 is one drop of difference and it decides a payment:
/// 20.17908820997 RLUSD at the filed 921429.6903074811 is
/// 18593611.000000000249 drops. The legacy form drops the `…249` and yields
/// 18593611 — exactly the SendMax remaining — so the fill looks affordable and
/// we hand over the full 20.17908820997. rippled ceilings to 18593612, finds
/// it cannot afford that, and re-derives the output from the 18593611 it does
/// have: `New flow iter (iter, in, out): 3 18593611 20.17908820996999`.
fn mul_round_drops_strict(a: Me, b: Me, round_up: bool) -> u128 {
    let (a, b) = (norm16(a), norm16(b));
    if a.0 == 0 || b.0 == 0 {
        return 0;
    }
    const TEN14: u128 = 100_000_000_000_000;
    // `muldiv_round(m1, m2, tenTo14, tenTo14m1)` — a ceiling division.
    let prod = a.0 * b.0;
    let mut m = prod / TEN14 + u128::from(prod % TEN14 != 0);
    let mut e = a.1 + b.1 + 14;
    if e < 0 {
        let mut had_remainder = false;
        while e < -1 {
            let nv = m / 10;
            had_remainder |= m != nv * 10;
            m = nv;
            e += 1;
        }
        m += if had_remainder && round_up { 10 } else { 9 };
        m /= 10;
        e += 1;
    }
    m.saturating_mul(10u128.saturating_pow(e.clamp(0, 38) as u32))
}

/// `mulRound(a, b, XRP, roundUp)` — `a * b` as whole DROPS, the NON-strict way.
///
/// The mirror of `mul_round_drops_strict`, and the distinction is which rippled
/// call site you are modelling. `TOffer::limitOut` uses `ceilOutStrict` and gets
/// a true ceiling; `flowCross`'s residual uses plain `mulRound`
///     afterCross.in = mulRound(afterCross.out, rate, takerAmount.in.asset(), true);
/// which lands in `canonicalizeRound`, whose INTEGRAL branch IGNORES `roundUp`
/// entirely and reads (STAmount.cpp:1477):
///     while (offset < -1) { value /= 10; ++offset; ++loops; }
///     value += (loops >= 2) ? 9 : 10;   // add before last divide
///     value /= 10;
/// So it ceils at a TENTH OF A DROP: everything below 0.1 drop is discarded
/// before the carry. That "slop" is not a defect to be corrected here — it IS
/// the behaviour, and a plain ceiling cannot reproduce it.
///
/// Two mainnet residuals pin it, and no single rounding direction satisfies
/// rippled `divRoundStrict(num, rate, xrpAsset, roundUp = false)` — the
/// tfSell residual's TakerPays: numerator x 1e17 / rate, FLOOR at every
/// stage, canonicalized straight to whole drops (STAmount.cpp divRoundImpl
/// with the strict canonicalize, which for round-down is plain truncation).
fn div_round_drops_strict_floor(a: Me, rate: Me) -> u128 {
    let (a, r) = (norm16(a), norm16(rate));
    if a.0 == 0 || r.0 == 0 {
        return 0;
    }
    const TEN17: u128 = 100_000_000_000_000_000;
    let mut m = a.0.saturating_mul(TEN17) / r.0;
    let mut e = a.1 - r.1 - 17;
    while e < 0 && m > 0 {
        m /= 10;
        e += 1;
    }
    while e > 0 {
        m = m.saturating_mul(10);
        e -= 1;
    }
    m
}

/// rippled `divRoundStrict(num, rate, iouAsset, roundUp = false)` — 16-digit
/// floor of the quotient.
fn div_round16_down(a: Me, rate: Me) -> Me {
    let (a, r) = (norm16(a), norm16(rate));
    if a.0 == 0 || r.0 == 0 {
        return (0, 0);
    }
    const TEN17: u128 = 100_000_000_000_000_000;
    let mut m = a.0.saturating_mul(TEN17) / r.0;
    let mut e = a.1 - r.1 - 17;
    while m >= 10_000_000_000_000_000 {
        m /= 10;
        e += 1;
    }
    (m, e)
}

/// both:
///   #106308202 6B6D7EFC  283956.0452856067 -> 283956  (0.045 discarded)
///   #105672435 B409D45C  165388.7424863569 -> 165389  (0.7 carries)
fn mul_round_drops(a: Me, b: Me) -> u128 {
    let (a, b) = (norm16(a), norm16(b));
    if a.0 == 0 || b.0 == 0 {
        return 0;
    }
    const TEN14: u128 = 100_000_000_000_000;
    let prod = a.0 * b.0;
    let mut m = prod / TEN14 + u128::from(prod % TEN14 != 0);
    let mut e = a.1 + b.1 + 14;
    if e < 0 {
        let mut loops = 0;
        while e < -1 {
            m /= 10;
            e += 1;
            loops += 1;
        }
        m += if loops >= 2 { 9 } else { 10 };
        m /= 10;
        e += 1;
    }
    m.saturating_mul(10u128.saturating_pow(e.clamp(0, 38) as u32))
}

pub(crate) fn me_muldiv(a: Me, b: Me, c: Me, ceil: bool) -> Me {
    if c.0 == 0 {
        return (0, 0);
    }
    let (a, b, c) = (me_norm(a), me_norm(b), me_norm(c));
    let mut num = a.0.saturating_mul(b.0);
    let mut e = a.1 + b.1 - c.1;
    // Keep ~16 significant digits in the quotient: mixed-scale operands
    // (XRP drops against 1e-11-exponent IOU mantissas) otherwise truncate
    // to zero (bridge slice sizing found this the hard way).
    while num != 0
        && num < c.0.saturating_mul(1_000_000_000_000_000)
        && num < u128::MAX / 10
    {
        num = num.saturating_mul(10);
        e -= 1;
    }
    let m = if ceil { num.div_ceil(c.0) } else { num / c.0 };
    me_norm((m, e))
}

pub(crate) fn me_to_value_string(a: Me) -> String {
    if a.0 == 0 {
        return "0".into();
    }
    let (mut m, mut e) = a;
    while m % 10 == 0 && e < 0 {
        m /= 10;
        e += 1;
    }
    if e >= 0 {
        format!("{}{}", m, "0".repeat(e.min(40) as usize))
    } else {
        let s = m.to_string();
        let k = (-e) as usize;
        if s.len() > k {
            format!("{}.{}", &s[..s.len() - k], &s[s.len() - k..])
        } else {
            format!("0.{}{}", "0".repeat(k - s.len()), s)
        }
    }
}

/// Remainder amount in the same JSON shape as the original tx field.
pub(crate) fn me_amount_json(orig: &serde_json::Value, a: Me) -> serde_json::Value {
    match orig {
        serde_json::Value::String(_) => {
            serde_json::Value::String(me_rescale(a, 0, false).to_string())
        }
        serde_json::Value::Object(o) => serde_json::json!({
            "currency": o.get("currency").cloned().unwrap_or_default(),
            "issuer": o.get("issuer").cloned().unwrap_or_default(),
            "value": me_to_value_string(a),
        }),
        _ => serde_json::Value::Null,
    }
}

pub(crate) fn json_at(sandbox: &Sandbox, key: &xrpl_core::types::Hash256) -> Option<serde_json::Value> {
    sandbox.read(key).and_then(|d| serde_json::from_slice(&d).ok())
}

pub(crate) fn put_json(sandbox: &mut Sandbox, key: xrpl_core::types::Hash256, v: &serde_json::Value) {
    sandbox.write(key, serde_json::to_vec(v).unwrap_or_default());
}

pub(crate) fn dirnum(v: &serde_json::Value) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(s) = v.as_str() {
        return u64::from_str_radix(s, 16).ok().or_else(|| s.parse().ok()).unwrap_or(0);
    }
    0
}

/// Signed (negative, magnitude) of an amount value string.
pub(crate) fn signed_value(v: &serde_json::Value) -> (bool, Me) {
    let s = match v {
        serde_json::Value::Object(o) => o.get("value").and_then(|x| x.as_str()).unwrap_or("0"),
        serde_json::Value::String(s) => s.as_str(),
        _ => "0",
    };
    let neg = s.starts_with('-');
    let me = keylet::amount_mant_exp(&serde_json::Value::String(s.trim_start_matches('-').to_string()))
        .unwrap_or((0, 0));
    (neg && me.0 > 0, me)
}

/// Signed add with rippled's STAmount alignment.
///
/// `STAmount operator+` brings both operands to the LARGER exponent by integer
/// division:
///     while (ov1 < ov2) { vv1 /= 10; ++ov1; }
///     while (ov2 < ov1) { vv2 /= 10; ++ov2; }
/// so an addend too small to affect the other simply becomes ZERO and the
/// balance is unchanged. `signed_add` aligns to the SMALLER exponent instead,
/// keeping precision u128 can hold but a real IOU cannot.
///
/// #105840045 3F942A682131 pays 0.000001 ZERPS between accounts holding
/// 137,330,862,022.1269 and 254,893,727,053.43. rippled truncates the addend to
/// zero, both balances are untouched, and its meta is ONE node — the sender's
/// AccountRoot for the fee — still tesSUCCESS. We moved the value and invented
/// two Modified nodes.
pub(crate) fn stamount_signed_add(aneg: bool, a: Me, bneg: bool, b: Me) -> (bool, Me) {
    if b.0 == 0 {
        return (aneg, a);
    }
    if a.0 == 0 {
        return (bneg, b);
    }
    // Canonicalise to STAmount's 16-digit mantissa BEFORE aligning. rippled
    // keeps every IOU in that form, so its exponent is always tied to the
    // magnitude; our Me can hold un-normalised pairs like (1, 6), and aligning
    // to such an exponent would truncate precision rippled retains. Skipping
    // this step zeroed legitimate movements: batch2 #105933892 D0BDF094CE78
    // went from clean to missing a Modified line, and #105949459 from 1
    // divergence to 3.
    // ROUNDED to 16 digits, not truncated. An STAmount operand is ALREADY
    // canonical in rippled — it was rounded when it was produced — so reducing
    // an over-precise operand by truncation drops exactly the digit that
    // decides the result. DX_TRUNC caught it naming its own caller:
    //   DX_TRUNC norm16 10899215705147927 e-7 -> 1089921570514792 (dropped 7/10)
    //      1: stamount_signed_add   2: offer_residual
    // a 7 discarded where half-even must carry.
    use crate::tx::amm_swap::{round16, Rnd};
    let a = round16(a.0, a.1, false, Rnd::Near);
    let b = round16(b.0, b.1, false, Rnd::Near);
    // rippled adds IOUs through `Number` (STAmount.cpp:391, IOUAmount.cpp:142,
    // both gated on `getSTNumberSwitchover()` — fixUniversalNumber, long since
    // enabled): the operands are aligned with a GUARD DIGIT plus a sticky bit
    // and the exact result is rounded HALF-EVEN back to 16 digits.
    //
    // Aligning to the larger exponent and dropping the tail is the LEGACY
    // branch immediately below that switchover (`mantissa_ /= 10` in a loop),
    // and it lands one ulp low whenever the discarded tail reaches half.
    // #106143011's pool ran 2000892.236615386 + 100.153148870651: the exact
    // sum is 2000992.389764256|651, so mainnet stores ...257 and we stored
    // ...256. One ulp on the pool balance re-prices every later AMM slice —
    // it walked into `changeSpotPriceQuality`, moved the generated offer just
    // inside the target quality where rippled's lands just outside, and gave
    // us a 6th AMM turn rippled never takes.
    let e = a.1.min(b.1);
    if (a.1.max(b.1) - e) as u32 > 22 {
        // The smaller operand sits more than a full mantissa below the larger:
        // under half an ulp, so it can only ever be sticky.
        return if a.1 > b.1 { (aneg, a) } else { (bneg, b) };
    }
    let av = a.0 * 10u128.pow((a.1 - e) as u32);
    let bv = b.0 * 10u128.pow((b.1 - e) as u32);
    if aneg == bneg {
        return (aneg, round16(av + bv, e, false, Rnd::Near));
    }
    match av.cmp(&bv) {
        std::cmp::Ordering::Equal => (false, (0, 0)),
        std::cmp::Ordering::Greater => (aneg, round16(av - bv, e, false, Rnd::Near)),
        std::cmp::Ordering::Less => (bneg, round16(bv - av, e, false, Rnd::Near)),
    }
}

pub(crate) fn signed_add(aneg: bool, a: Me, bneg: bool, b: Me) -> (bool, Me) {
    if aneg == bneg {
        let e = a.1.min(b.1);
        return (aneg, (me_rescale(a, e, false) + me_rescale(b, e, false), e));
    }
    match me_cmp(a, b) {
        std::cmp::Ordering::Equal => (false, (0, 0)),
        std::cmp::Ordering::Greater => (aneg, me_sub(a, b)),
        std::cmp::Ordering::Less => (bneg, me_sub(b, a)),
    }
}

pub(crate) fn owner_count_add(sandbox: &mut Sandbox, id: &[u8; 20], delta: i64) {
    let key = keylet::account_root_key(id);
    if let Some(mut a) = json_at(sandbox, &key) {
        let c = a["OwnerCount"].as_u64().unwrap_or(0) as i64;
        a["OwnerCount"] = serde_json::Value::Number(((c + delta).max(0) as u64).into());
        put_json(sandbox, key, &a);
    }
}

/// How much of `leg` the account can actually deliver.
pub(crate) fn available(sandbox: &Sandbox, id: &[u8; 20], leg: &Leg) -> Me {
    if leg.xrp {
        let key = keylet::account_root_key(id);
        let Some(a) = json_at(sandbox, &key) else { return (0, 0) };
        let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let oc = a["OwnerCount"].as_u64().unwrap_or(0) as u128;
        let reserve = XRP_RESERVE_BASE + XRP_RESERVE_INC * oc;
        (bal.saturating_sub(reserve), 0)
    } else if id == &leg.issuer {
        (u128::MAX / 4, 20) // issuers deliver their own IOU without limit
    } else {
        // rippled reads spendable IOU through `accountHolds` with
        // `fhZERO_IF_FROZEN`, so a FROZEN holder has no funds at all, however
        // large the balance. `isFrozen` (RippleStateHelpers.cpp:127) is the
        // issuer's GLOBAL freeze, or the ISSUER's side of the line —
        // `(issuer > account) ? lsfHighFreeze : lsfLowFreeze`.
        //
        // #105878507 475EA928 and ten siblings: rLiq73yy holds 5811047220.15868
        // ARK and offers to sell it, but rBWfabv7 has frozen that line, so
        // mainnet claims the fee and returns tecUNFUNDED_OFFER (1 mutation)
        // while we crossed and placed (4). Reading the balance alone is what
        // let a frozen holder trade.
        if let Some(iss) = json_at(sandbox, &keylet::account_root_key(&leg.issuer)) {
            if iss["Flags"].as_u64().unwrap_or(0) & 0x0040_0000 != 0 {
                return (0, 0); // lsfGlobalFreeze
            }
        }
        let lkey = keylet::ripple_state_key(id, &leg.issuer, &leg.cur);
        let Some(line) = json_at(sandbox, &lkey) else { return (0, 0) };
        let issuer_freeze = if &leg.issuer > id { 0x0080_0000 } else { 0x0040_0000 };
        if line["Flags"].as_u64().unwrap_or(0) & issuer_freeze != 0 {
            return (0, 0);
        }
        let (neg, bal) = signed_value(&line["Balance"]);
        let holds = if id < &leg.issuer { !neg } else { neg }; // positive toward the party?
        // Balance is from the LOW account's perspective: positive = high owes
        // low. The party HOLDS the IOU when the balance points toward them.
        let party_low = id < &leg.issuer;
        let party_holds = if party_low { !neg } else { neg };
        let _ = holds;
        if party_holds && bal.0 > 0 { bal } else { (0, 0) }
    }
}

/// The most a payment can land on `dest`'s trust line for `leg` — rippled
/// `DirectStepI::maxPaymentFlow` (DirectStep.cpp:476-488), the ceiling the
/// strand's final issuer→dest step imposes. When `dest` already holds the IOU
/// (balance ≥ 0) the room is `creditLimit(dest,issuer) − held`: a destination
/// at or over its own trust limit can receive NOTHING and the whole strand is
/// dry (every strand shares that last step). When `dest` instead owes the
/// issuer (negative balance) it may receive up to what it owes — the Redeems
/// branch (:483). Returns `None` when there is no such ceiling: XRP, or
/// `dest == issuer` (the issuer redeems its own IOU without limit), or no line
/// yet (a missing line is the caller's own dry guard, not a zero cap). A
/// returned zero means the destination can receive nothing ⇒ `tecPATH_DRY`.
pub(crate) fn dest_receivable(sandbox: &Sandbox, dest: &[u8; 20], leg: &Leg) -> Option<Me> {
    if leg.xrp || dest == &leg.issuer {
        return None;
    }
    let lkey = keylet::ripple_state_key(dest, &leg.issuer, &leg.cur);
    let line = json_at(sandbox, &lkey)?;
    // Balance is stored from the LOW account's view (positive ⇒ high owes low
    // ⇒ low holds), so `dest` holds when it is low with a positive balance or
    // high with a negative one — the same test `available()` uses above.
    let (bneg, bmag) = signed_value(&line["Balance"]);
    // An issuer with lsfRequireAuth cannot put its IOU into a line that lacks
    // the ISSUER-side auth flag while that line is still EMPTY — all three
    // conditions together (DirectStep.cpp:430-437, DirectIPaymentStep::check).
    // An existing balance is grandfathered. This runs at STRAND CONSTRUCTION,
    // so a failing strand is never built and the payment has no path at all:
    // tecPATH_DRY, not a short fill.
    //
    // 6 payments in the fresh batch answered tesSUCCESS where mainnet said
    // tecPATH_DRY — e.g. #105933892 845CB9790984, all self-payments converting
    // XRP to RUBY under tfPartialPayment with no Paths. The XRP/RUBY book is
    // empty; the liquidity we found was the XRP/RUBY AMM (644 XRP). Issuer
    // rG71TpU2 sets lsfRequireAuth and the senders' RUBY lines are
    // unauthorized with balance 0, so mainnet has no usable strand at all.
    const LSF_REQUIRE_AUTH: u64 = 0x0004_0000; // AccountRoot
    const LSF_LOW_AUTH: u64 = 0x0004_0000;     // RippleState
    const LSF_HIGH_AUTH: u64 = 0x0008_0000;    // RippleState
    if bmag.0 == 0 {
        let issuer_requires_auth = json_at(sandbox, &keylet::account_root_key(&leg.issuer))
            .and_then(|a| a["Flags"].as_u64())
            .is_some_and(|f| f & LSF_REQUIRE_AUTH != 0);
        if issuer_requires_auth {
            // rippled: authField = (issuer > dest) ? lsfHighAuth : lsfLowAuth —
            // the flag sits on the ISSUER's side of the line.
            let auth_bit = if &leg.issuer < dest { LSF_LOW_AUTH } else { LSF_HIGH_AUTH };
            if line["Flags"].as_u64().unwrap_or(0) & auth_bit == 0 {
                return Some((0, 0));
            }
        }
    }
    let dest_low = dest < &leg.issuer;
    let dest_holds = if dest_low { !bneg } else { bneg };
    if !dest_holds && bmag.0 > 0 {
        // Negative balance: dest owes the issuer and may redeem up to that.
        return Some(bmag);
    }
    // Non-negative balance: ceiling is the trust limit minus what is held.
    let held = if dest_holds { bmag } else { (0, 0) };
    let limit_field = if dest_low { "LowLimit" } else { "HighLimit" };
    let limit = keylet::amount_mant_exp(&line[limit_field]).unwrap_or((0, 0));
    if me_cmp(limit, held).is_gt() {
        Some(me_sub(limit, held))
    } else {
        Some((0, 0))
    }
}

/// `requireAuth(view, issue, account)` — RippleStateHelpers.cpp:556 — as the
/// BOOK walk needs it: may `owner` hold `leg` at all? XRP and the issuer
/// itself always may. Under the issuer's lsfRequireAuth the line must carry
/// the ISSUER-side auth flag (issuer low ⇒ lsfLowAuth, else lsfHighAuth —
/// the 0x40000/0x80000 RippleState bits, NOT the 0x10000/0x20000 reserve
/// bits), and a MISSING line is tecNO_LINE. There is NO balance grandfather
/// here — that mercy is DirectStep.cpp:430's endpoint check
/// (`dest_receivable` above), not this one. BookStep.cpp:755 runs this per
/// offer OWNER — an AMM pool account included — and perm-removes the
/// unauthorized "even if no crossing occurs".
///
/// #106588526 0CEFB5D8/B0E70AB8: the CAMP/PINGU pool (rnSAXkQ…) holds its
/// CAMP on a line with flags 0x01010000 — no auth bit — while issuer
/// rfxucs… (0x408C0000) sets lsfRequireAuth: rippled's rev pass builds the
/// synthetic, the stream then yields nothing, "Strand found dry in rev".
///
/// Returns None when a verdict needs state not in hand (issuer root
/// unhydrated; or no line AND no owner root — the walk's "an unhydrated
/// maker is never condemned" rule). Some(false) ⇒ reap/skip.
pub(crate) fn require_auth_known(sandbox: &Sandbox, leg: &Leg, owner: &[u8; 20]) -> Option<bool> {
    const LSF_REQUIRE_AUTH: u64 = 0x0004_0000; // AccountRoot
    const LSF_LOW_AUTH: u64 = 0x0004_0000; // RippleState
    const LSF_HIGH_AUTH: u64 = 0x0008_0000; // RippleState
    if leg.xrp || owner == &leg.issuer {
        return Some(true);
    }
    let iss = json_at(sandbox, &keylet::account_root_key(&leg.issuer))?;
    if iss["Flags"].as_u64().unwrap_or(0) & LSF_REQUIRE_AUTH == 0 {
        return Some(true);
    }
    let line = json_at(sandbox, &keylet::ripple_state_key(owner, &leg.issuer, &leg.cur));
    match line {
        Some(line) => {
            let auth_bit = if &leg.issuer < owner { LSF_LOW_AUTH } else { LSF_HIGH_AUTH };
            Some(line["Flags"].as_u64().unwrap_or(0) & auth_bit != 0)
        }
        None => json_at(sandbox, &keylet::account_root_key(owner)).map(|_| false),
    }
}

/// Adjust one party's side of an IOU movement (line balance ±amt), creating
/// the line if the receiver has none (rippled offer-crossing behavior).
pub(crate) fn line_adjust(sandbox: &mut Sandbox, party: &[u8; 20], leg: &Leg, amt: Me, receiving: bool) {
    if party == &leg.issuer {
        return;
    }
    let lkey = keylet::ripple_state_key(party, &leg.issuer, &leg.cur);
    let party_low = party < &leg.issuer;
    // rippled flag layout (per side of the line).
    const LOW_RESERVE: u64 = 0x0001_0000;
    const HIGH_RESERVE: u64 = 0x0002_0000;
    const LOW_NO_RIPPLE: u64 = 0x0010_0000;
    const HIGH_NO_RIPPLE: u64 = 0x0020_0000;
    const LOW_FREEZE: u64 = 0x0040_0000;
    const HIGH_FREEZE: u64 = 0x0080_0000;
    if let Some(mut line) = json_at(sandbox, &lkey) {
        // Snapshot so an arithmetically-inert update writes NOTHING. IOU values
        // carry ~16 significant digits, so adding a tiny amount to a large
        // balance rounds straight back to the stored mantissa — rippled then
        // emits no node for that line at all.
        //
        // #105840045 3F942A682131 pays 0.000001 ZERPS between accounts holding
        // 137,330,862,022.1269 and 254,893,727,053.43. Neither balance can
        // represent the change, so mainnet's meta is ONE node — the sender's
        // AccountRoot for the fee — and the result is still tesSUCCESS. We
        // wrote both lines and invented two Modified nodes (3C38BC1E, AEFF2748).
        let line_before = line.clone();
        let (lneg, lbal) = signed_value(&line["Balance"]);
        // Compare the balance NUMERICALLY, not as JSON text: mainnet serialises
        // this line as "1373308620221269e-4" while me_to_value_string writes
        // "137330862022.1269". Same value, different string — a text comparison
        // would call every inert update a change.
        let balance_before = (lneg && lbal.0 > 0, lbal);
        // party's holding: low holds when balance positive, high when negative
        let (pneg, pmag) = if party_low { (lneg, lbal) } else { (!lneg, lbal) };
        let (nneg, nmag) = stamount_signed_add(pneg && pmag.0 > 0, pmag, !receiving, amt);
        // DX_LINEADJ=<key prefix|1>: print every balance adjustment landing on
        // a matching RippleState — the credit-granularity receipt for the
        // 1-ulp census class (each flush is one half-even 16-digit add, so the
        // SEQUENCE of flushes decides the final ulp, invisible in any total).
        if let Ok(want) = std::env::var("DX_LINEADJ") {
            let kh = hex::encode_upper(lkey.0);
            if want == "1" || kh.starts_with(&want) {
                eprintln!(
                    "DX_LINEADJ {kh} party={} recv={receiving} amt={amt:?} prev={}{:?} new={}{:?}",
                    hex::encode(party),
                    if pneg && pmag.0 > 0 { "-" } else { "+" },
                    pmag,
                    if nneg && nmag.0 > 0 { "-" } else { "+" },
                    nmag,
                );
            }
        }
        let (wneg, wmag) = if party_low { (nneg, nmag) } else { (!nneg, nmag) };
        let sign = if wneg && wmag.0 > 0 { "-" } else { "" };
        let moved = (wneg && wmag.0 > 0) != (lneg && lbal.0 > 0)
            || me_cmp(wmag, lbal) != std::cmp::Ordering::Equal;
        if moved {
            line["Balance"]["value"] =
                serde_json::Value::String(format!("{}{}", sign, me_to_value_string(wmag)));
        }

        // rippled rippleCreditIOU: a line the SENDER just spent from positive
        // down to zero-or-below reverts to its default state — release their
        // reserve, clear their reserve flag, and delete the line outright
        // once the counterparty holds no reserve on it either. Guarded the
        // rippled way: their side must be unfrozen, NoRipple-vs-DefaultRipple
        // divergent, with a zero limit and zero quality settings.
        let spent_out = !receiving && pmag.0 > 0 && nmag.0 == 0;
        if spent_out {
            let flags = line["Flags"].as_u64().unwrap_or(0);
            let (my_reserve, my_no_ripple, my_freeze, my_limit, their_reserve) = if party_low {
                (LOW_RESERVE, LOW_NO_RIPPLE, LOW_FREEZE, "LowLimit", HIGH_RESERVE)
            } else {
                (HIGH_RESERVE, HIGH_NO_RIPPLE, HIGH_FREEZE, "HighLimit", LOW_RESERVE)
            };
            let limit_zero = line[my_limit]["value"].as_str().map(|v| v == "0").unwrap_or(true);
            let default_ripple = json_at(sandbox, &keylet::account_root_key(party))
                .and_then(|a| a["Flags"].as_u64())
                .map(|f| f & 0x0080_0000 != 0)
                .unwrap_or(false);
            let quality_zero = line.get("LowQualityIn").is_none()
                && line.get("LowQualityOut").is_none()
                && line.get("HighQualityIn").is_none()
                && line.get("HighQualityOut").is_none();
            if flags & my_reserve != 0
                && (flags & my_no_ripple != 0) != default_ripple
                && flags & my_freeze == 0
                && limit_zero
                && quality_zero
            {
                owner_count_add(sandbox, party, -1);
                line["Flags"] = serde_json::Value::from(flags & !my_reserve);
                if flags & their_reserve == 0 {
                    // Default on both sides: the line stops existing.
                    //
                    // Both removals need the line's LowNode/HighNode page hint,
                    // exactly as TrustSet's trustDelete passes them. Without a
                    // hint owner_dir_remove cannot locate the entry in a
                    // MULTI-page directory, and the side that silently fails is
                    // always the issuer's — a counterparty on one trust line has
                    // a single-page directory, while an IOU issuer holding
                    // thousands of lines does not.
                    //
                    // 8 Payments diverged on exactly this, each missing exactly
                    // one Modified DirectoryNode and nothing else: six in
                    // #105854147 all missing 6ABA617A (owner rU5wZyCbZ2, the
                    // HIYO issuer) while the sender's own dir EE79B4E5 came out
                    // right, plus #105843539 BB9651ABA1B6 and #105872154
                    // D217CCCAE1E3 against their own issuers.
                    let hint = |v: &serde_json::Value| -> Option<u64> {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                    };
                    let (party_node, issuer_node) = if party_low {
                        (hint(&line["LowNode"]), hint(&line["HighNode"]))
                    } else {
                        (hint(&line["HighNode"]), hint(&line["LowNode"]))
                    };
                    sandbox.delete(lkey);
                    crate::ledger::directory::owner_dir_remove(sandbox, party, &lkey, party_node, false);
                    crate::ledger::directory::owner_dir_remove(
                        sandbox,
                        &leg.issuer,
                        &lkey,
                        issuer_node,
                        false,
                    );
                    return;
                }
            }
        }
        let balance_after = (wneg && wmag.0 > 0, wmag);
        let balance_moved = balance_after.0 != balance_before.0
            || me_cmp(balance_after.1, balance_before.1) != std::cmp::Ordering::Equal;
        if balance_moved || line != line_before {
            put_json(sandbox, lkey, &line);
        }
    } else if receiving {
        let (lo, hi) = if party_low { (party, &leg.issuer) } else { (&leg.issuer, party) };
        let bal_neg = !party_low; // holding sits on the party's side
        let sign = if bal_neg { "-" } else { "" };
        let cur_str = hex::encode_upper(leg.cur);
        // rippled rippleCredit's create: the RECEIVER carries the reserve,
        // and their side gets NoRipple unless their account has
        // DefaultRipple set (`noRipple = !receiverAccount->isFlag(
        // lsfDefaultRipple)`, RippleStateHelpers.cpp:458). trustCreate then
        // ALSO stamps the PEER's side when the peer — the issuer here —
        // lacks DefaultRipple: "The other side's default is no rippling"
        // (:277-281). Typical issuers carry DefaultRipple, which kept the
        // missing peer arm invisible; ported with the #106455241 TrustSet
        // sibling.
        let default_ripple = json_at(sandbox, &keylet::account_root_key(party))
            .and_then(|a| a["Flags"].as_u64())
            .map(|f| f & 0x0080_0000 != 0)
            .unwrap_or(false);
        let peer_default_ripple = json_at(sandbox, &keylet::account_root_key(&leg.issuer))
            .and_then(|a| a["Flags"].as_u64())
            .map(|f| f & 0x0080_0000 != 0)
            .unwrap_or(false);
        let mut flags = if party_low { LOW_RESERVE } else { HIGH_RESERVE };
        if !default_ripple {
            flags |= if party_low { LOW_NO_RIPPLE } else { HIGH_NO_RIPPLE };
        }
        if !peer_default_ripple {
            flags |= if party_low { HIGH_NO_RIPPLE } else { LOW_NO_RIPPLE };
        }
        let mut line = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Flags": flags,
            // Balance issuer = noAccount()/ACCOUNT_ONE (View.cpp
            // trustCreate) — the zero account is one nibble wrong on the
            // wire (#106455039 A72B9486).
            "Balance": {"currency": cur_str, "issuer": "0000000000000000000000000000000000000001",
                         "value": format!("{}{}", sign, me_to_value_string(amt))},
            "LowLimit": {"currency": cur_str, "issuer": hex::encode(lo), "value": "0"},
            "HighLimit": {"currency": cur_str, "issuer": hex::encode(hi), "value": "0"},
        });
        // The line joins BOTH owner directories, but only the receiver pays
        // the reserve — the issuer's OwnerCount is untouched. Both dir hints
        // are SoeRequired on the line (byte census: ours lacked LowNode).
        let party_node = crate::ledger::directory::owner_dir_insert(sandbox, party, &lkey);
        let issuer_node = crate::ledger::directory::owner_dir_insert(sandbox, &leg.issuer, &lkey);
        let (lo_node, hi_node) =
            if party_low { (party_node, issuer_node) } else { (issuer_node, party_node) };
        line["LowNode"] = serde_json::Value::String(format!("{lo_node:x}"));
        line["HighNode"] = serde_json::Value::String(format!("{hi_node:x}"));
        put_json(sandbox, lkey, &line);
        owner_count_add(sandbox, party, 1);
    }
}

/// Move `net` of `leg` to the receiver while the sender parts with `gross` —
/// rippled's `rippleSend` (View.cpp), which is ONE credit per party:
///
///   saActual = saAmount + saTransitFee;
///   rippleCredit(issuer, receiver, saAmount);   // receiver gets the NET
///   rippleCredit(sender, issuer, saActual);     // sender pays the GROSS
///
/// Debiting the net and then adjusting the fee is TWO roundings of the sender's
/// line where rippled has one, and a balance re-rounds to 16 digits at every
/// touch. #105954798 D5887FD7 is the ledger that cares: the direct head inside
/// its bridge moves 0.02788127288328903 BTC net / 0.02792309479261397 gross
/// against a 0.15% issuer, and the taker's own line lands
///   net then fee   0.3458715267976857 -> 0.3179902539143967 -> …0050718  <- ours
///   gross, once    0.3458715267976857 ------------------------> …0050717  <- mainnet
/// Same family as `4556384` and `44c20d9`: hold no more intermediate precision
/// than the ledger does, and touch a balance as many times as rippled does — no
/// more.
pub(crate) fn move_leg_gross(
    sandbox: &mut Sandbox,
    from: &[u8; 20],
    to: &[u8; 20],
    leg: &Leg,
    net: Me,
    gross: Me,
) {
    if me_cmp(gross, net) == std::cmp::Ordering::Equal {
        move_leg(sandbox, from, to, leg, net);
        return;
    }
    if !me_is_zero(gross) {
        line_adjust(sandbox, from, leg, gross, false);
    }
    if !me_is_zero(net) {
        line_adjust(sandbox, to, leg, net, true);
    }
}

/// rippled `mulRatio(IOUAmount, num, den, roundUp)` — IOUAmount.cpp:182.
/// NOT a plain ceiling. The 128-bit product/quotient is scaled to ~18-19
/// digits (roomToGrow), the IOUAmount ctor then NORMALIZES to 16 digits at
/// Number's half-even NEAREST, and only THEN a remainder bumps the rounded
/// mantissa by one ulp (roundUp && positive). Net effect for a positive
/// amount: nearest16(exact) + 1 when inexact — which exceeds a true ceil
/// whenever the discarded fraction is above one half.
///
/// #106455044 32DD7192 (full-ledger replay, shim-traced): net
/// 0.02184144723105913 x 1.003 = ...230.739 exact; our exact ceil said
/// ...231, rippled's LIMITSTEPOUT prints stpIn=0.02190697157275232.
pub(crate) fn mul_ratio(a: Me, num: u128, den: u128, round_up: bool) -> Me {
    let (m, e) = norm16(a);
    if m == 0 || num == 0 {
        return (0, 0);
    }
    let prod = m * num;
    let mut low = prod / den;
    let mut rem = prod % den;
    let mut exp = e;
    let ceil_log10 = |v: u128| -> i32 {
        let mut d = 0i32;
        let mut x = 1u128;
        while x < v {
            x = x.saturating_mul(10);
            d += 1;
        }
        d
    };
    if rem != 0 {
        let room = 18 - ceil_log10(low);
        if room > 0 {
            let p = 10u128.pow(room as u32);
            low *= p;
            rem *= p;
            exp -= room;
            low += rem / den;
            rem %= den;
        }
    }
    let mut has_rem = rem != 0;
    let shrink = ceil_log10(low) - 18;
    if shrink > 0 {
        let p = 10u128.pow(shrink as u32);
        let sav = low;
        low /= p;
        exp += shrink;
        has_rem |= sav != low * p;
    }
    // IOUAmount(low, exp) normalization: reduce to 16 digits at half-even.
    let (mut nm, mut ne) = (low, exp);
    while nm >= 10_000_000_000_000_000 {
        let over = ceil_log10(nm) - 16;
        let p = 10u128.pow(over.max(1) as u32);
        let q = nm / p;
        let r = nm % p;
        nm = match (2 * r).cmp(&p) {
            std::cmp::Ordering::Greater => q + 1,
            std::cmp::Ordering::Equal => q + (q & 1),
            std::cmp::Ordering::Less => q,
        };
        ne += over.max(1);
        // The ctor's own rounding loss does NOT feed the roundUp bump —
        // rippled computes hasRem BEFORE `IOUAmount result(...)` and never
        // folds the normalize remainder in.
    }
    if has_rem && round_up {
        nm += 1;
        if nm >= 10_000_000_000_000_000 {
            nm /= 10;
            ne += 1;
        }
    }
    while nm != 0 && nm < 1_000_000_000_000_000 {
        nm *= 10;
        ne -= 1;
    }
    (nm, ne)
}

/// The gross an input transfer rate makes the taker part with for `net`.
/// `mulRatio(ofrAmt.in, ofrInRate, QUALITY_ONE, roundUp)` — BookStep.cpp:770.
///
/// ⚠ Still the exact ceil, NOT `mul_ratio` above. Rerouting this through
/// the faithful mulRatio (nearest + bump) regressed #106455042/43's
/// funding-bound drains: rippled's first DirectStep is GROSS-PRIMARY when
/// the line binds (srcToDst = the whole line, net derived by division), so
/// its debit lands on exactly zero however mulRatio rounds — our net-first
/// fills only drain exactly under the ceil. The one-ulp #106455044
/// 32DD7192 site needs the mulRatio semantics wired together WITH
/// gross-primary funding, one campaign, not piecemeal.
pub(crate) fn gross_in(fee_rate: Option<u64>, net: Me) -> Me {
    match fee_rate {
        // rippled mulRatio (BookStep.cpp:770 stpIn = mulRatio(ofrIn, trIn,
        // QUALITY_ONE, roundUp=true)) — nearest-16 + bump on a pre-normalize
        // remainder, NOT exact ceil. #106455107 7844199C pins it: net
        // 0.01536809504818859 × 1.003 = …315|577 → nearest …316 → bump …317
        // (the mainnet spend-line debit); the ceil said …316 and left the
        // dust remainder one ulp high. The funding-bound drains that the
        // ceil's floor-sizing identity used to protect are covered by the
        // spend-side gross-primary rule in the walk (gets_gross).
        Some(r) => mul_ratio(net, r as u128, 1_000_000_000, true),
        None => net,
    }
}

/// Move `amt` of `leg` from one account to another.
pub(crate) fn move_leg(sandbox: &mut Sandbox, from: &[u8; 20], to: &[u8; 20], leg: &Leg, amt: Me) {
    if me_is_zero(amt) {
        return;
    }
    if leg.xrp {
        let drops = me_rescale(amt, 0, false);
        for (id, add) in [(from, false), (to, true)] {
            let key = keylet::account_root_key(id);
            if let Some(mut a) = json_at(sandbox, &key) {
                let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                let nb = if add { bal.saturating_add(drops) } else { bal.saturating_sub(drops) };
                a["Balance"] = serde_json::Value::String(nb.to_string());
                put_json(sandbox, key, &a);
            }
        }
    } else {
        line_adjust(sandbox, from, leg, amt, false);
        line_adjust(sandbox, to, leg, amt, true);
    }
}

/// Fully remove a maker offer: object + owner-dir entry + book-dir entry +
/// maker OwnerCount.
///
/// During a flow, offer deletions are DEFERRED — rippled queues crossed
/// offers as removable (OfferCreate.cpp:460) and stale ones in `ofrsToRm`
/// (applied between iterations, StrandFlow.h:694), so within the walk the
/// owner's OwnerCount, hence reserve, keeps its PRE-WALK value for every
/// funding check. Our sandbox deletes inline, freeing one owner-reserve
/// unit per deletion; this pins the reserve to the OwnerCount at the
/// maker's FIRST funding peek instead.
/// #106093264 60F15308: after the first full fill, rippled prices the next
/// offer's funding at OwnerCount 23 (2060885); our inline delete said 22
/// (2260885) — and each subsequent reap re-inflated the pot, so the walk
/// milked six extra 200000-clips mainnet never took.
fn walk_available(
    sandbox: &Sandbox,
    maker: &[u8; 20],
    pays_leg: &Leg,
    oc0: Option<&mut std::collections::HashMap<[u8; 20], u64>>,
) -> Me {
    let Some(oc0) = oc0 else { return available(sandbox, maker, pays_leg) };
    if !pays_leg.xrp {
        return available(sandbox, maker, pays_leg);
    }
    let key = keylet::account_root_key(maker);
    let Some(a) = json_at(sandbox, &key) else { return (0, 0) };
    let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let cur = a["OwnerCount"].as_u64().unwrap_or(0);
    let oc = *oc0.entry(*maker).or_insert(cur);
    let reserve = XRP_RESERVE_BASE + XRP_RESERVE_INC * oc as u128;
    (bal.saturating_sub(reserve), 0)
}

pub(crate) fn delete_maker_offer(
    sandbox: &mut Sandbox,
    okey: &xrpl_core::types::Hash256,
    offer: &serde_json::Value,
    maker: &[u8; 20],
) {
    let hint = |f: &str| offer.get(f).map(dirnum).filter(|n| *n > 0);
    let owner_hint = offer.get("OwnerNode").map(dirnum);
    let book_hint = offer.get("BookNode").map(dirnum);
    let _ = hint;
    sandbox.delete(*okey);
    crate::ledger::directory::owner_dir_remove(sandbox, maker, okey, owner_hint, false);
    if let Some(bd) = offer
        .get("BookDirectory")
        .and_then(|v| v.as_str())
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .map(xrpl_core::types::Hash256)
    {
        crate::ledger::directory::dir_remove(sandbox, &bd, okey, book_hint, false);
    }
    owner_count_add(sandbox, maker, -1);
}

/// The issuer-published TickSize governing a pair: the smaller of the two
/// issuers' TickSize fields (XRP has none). 16 means "no tick rounding"
/// (rippled's Quality::maxTickSize).
/// The issuer's TransferRate, QUALITY_ONE-relative (1e9 = no fee). `None` for
/// XRP, for an issuer that charges nothing, or when the issuer account is not
/// hydrated.
pub(crate) fn transfer_rate(sandbox: &Sandbox, leg: &Leg) -> Option<u64> {
    if leg.xrp {
        return None;
    }
    let acct = json_at(sandbox, &keylet::account_root_key(&leg.issuer))?;
    let rate = acct.get("TransferRate")?.as_u64()?;
    (rate > 1_000_000_000).then_some(rate)
}

pub(crate) fn tick_size_for(sandbox: &Sandbox, a: &Leg, b: &Leg) -> u32 {
    let mut ts = 16u32;
    for leg in [a, b] {
        if leg.xrp {
            continue;
        }
        if let Some(acct) = json_at(sandbox, &keylet::account_root_key(&leg.issuer)) {
            if let Some(t) = acct.get("TickSize").and_then(|v| v.as_u64()) {
                if t > 0 {
                    ts = ts.min(t as u32);
                }
            }
        }
    }
    ts
}

/// rippled Quality::round(digits): round the rate mantissa UP to `digits`
/// significant decimal digits.
fn quality_round_up(rate: u64, digits: u32) -> u64 {
    if digits >= 16 {
        return rate;
    }
    let modulus = 10u64.pow(16 - digits);
    let exp = rate >> 56;
    let man = rate & 0x00FF_FFFF_FFFF_FFFF;
    let man = man + modulus - 1;
    let man = man - (man % modulus);
    (exp << 56) | man
}

/// Normalize a mantissa into rippled's STAmount range [1e15, 1e16).
pub(crate) fn norm16(x: Me) -> Me {
    let (mut m, mut e) = x;
    if m == 0 {
        return (0, 0);
    }
    // DX_TRUNC: report every reduction that DISCARDS a nonzero remainder, with
    // a backtrace naming the caller. An STAmount holds 16 digits, so a value
    // carrying more has to be reduced — the only question is whether rippled
    // rounds it or we truncate it, and this says WHERE we decided.
    static DX_TRUNC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if m >= 10_000_000_000_000_000 && *DX_TRUNC.get_or_init(|| std::env::var("DX_TRUNC").is_ok()) {
        let (mut t, mut dropped) = (m, 0u128);
        let mut scale: u128 = 1;
        while t >= 10_000_000_000_000_000 {
            dropped += (t % 10) * scale;
            scale *= 10;
            t /= 10;
        }
        if dropped != 0 {
            eprintln!(
                "DX_TRUNC norm16 {m} e{e} -> {t} (dropped {dropped}/{scale})\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
    }
    while m >= 10_000_000_000_000_000 {
        m /= 10;
        e += 1;
    }
    while m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    (m, e)
}

/// `num/den` rounded half-even to 16 significant digits — Number's real
/// rounding, with the division remainder folded into the tie comparison.
/// The legacy `+5`/`+7` half-adjust tricks approximate this under
/// TRUNCATING canonicalize; mixing them with a nearest pass double-rounds
/// (0EAE58BB fixed / 42071037 broken taught this the hard way).
fn div_nearest_16(num: u128, den: u128, e: i32) -> Me {
    if num == 0 || den == 0 {
        return (0, 0);
    }
    let q = num / den;
    let r = num % den;
    let mut k = 0u32;
    let mut t = q;
    while t >= 10_000_000_000_000_000 {
        t /= 10;
        k += 1;
    }
    let d = 10u128.pow(k);
    let (mut m, rr) = (q / d, q % d);
    // ⚠ The division REMAINDER IS DISCARDED, not folded into the tie.
    // `Number::operator/=` at the SMALL 16-digit scale — the one mainnet runs —
    // takes `zm = numerator / dm` and leaves `dropped = false`; Stages 2 and 3,
    // which would recover the remainder for the Guard, are gated on
    // `range.scale != MantissaScale::Small` and never execute. Only the digits
    // shed while normalising into range reach the rounding.
    //
    // This is the same defect `e7183b3` fixed in `amm_swap::n_div`, in the
    // sibling primitive — which is why that commit moved Payment and
    // OfferCreate but left all 11 AMM hits untouched: `st_divide` is used ONLY
    // by the AMM deposit/withdraw sizing.
    //
    // ⚠ Inert for the multiply caller below, which passes den = 1 so r is
    // always 0 and the comparison is unchanged.
    let _ = r;
    let lhs = 2 * rr;
    let rhs = d;
    if lhs > rhs || (lhs == rhs && m & 1 == 1) {
        m += 1;
    }
    let mut e = e + k as i32;
    if m >= 10_000_000_000_000_000 {
        m /= 10;
        e += 1;
    }
    while m > 0 && m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    (m, e)
}

/// rippled `divide(num, den, issue)` under Number semantics: exact quotient
/// rounded half-even at 16 digits (drops for XRP).
pub(crate) fn st_divide(num: Me, den: Me, xrp: bool) -> Me {
    if num.0 == 0 || den.0 == 0 {
        return (0, 0);
    }
    let (nm, ne) = norm16(num);
    let (dm, de) = norm16(den);
    if xrp {
        let v = nm * 100_000_000_000_000_000u128 / dm + 5;
        (me_rescale_nearest((v, ne - de - 17)), 0)
    } else {
        div_nearest_16(nm * 100_000_000_000_000_000u128, dm, ne - de - 17)
    }
}

/// rippled `multiply(v1, v2, issue)` under Number semantics: exact product
/// rounded half-even at 16 digits (drops for XRP).
fn st_multiply(a: Me, b: Me, xrp: bool) -> Me {
    if a.0 == 0 || b.0 == 0 {
        return (0, 0);
    }
    let (am, ae) = norm16(a);
    let (bm, be) = norm16(b);
    if xrp {
        let v = am * bm / 100_000_000_000_000u128 + 7;
        (me_rescale_nearest((v, ae + be + 14)), 0)
    } else {
        div_nearest_16(am * bm, 1, ae + be)
    }
}

/// rippled `getRate(pays, gets)` on raw mantissa/exponent pairs — the same
/// algorithm as `keylet::offer_quality`, without the JSON round-trip. Both
/// now share one implementation, so the book level an offer is PLACED on and
/// the level it is read back from can never disagree.
///
/// Offer crossing judges a pass at its quality COMPOSED with the OUT
/// issuer's transfer rate: `BookOfferCrossingStep::getQualityFunc` builds
/// the CLOB quality function with `WaiveTransferFee::No`, so "Path
/// rejected by limitQuality" fires on filed × trOut while
/// `qualityUpperBound` (admission) deliberately waives the fee. Same rule,
/// in the walk's encoding: judge the RAW realised quality against the
/// threshold tightened by the rate. AMM fills waive (OfferType::Amm at
/// BookStep.cpp:580), and so does a bridge leg-B offer owned by the taker
/// (getOfrOutRate's prev-is-book exemption, :497-508). rate(out, strandDst)
/// is parity when the taker IS the issuer.
///
/// #106067994 C966717C pins it: a tfPassive RLUSD→BTC.Bitstamp offer whose
/// bridged strand BOUNDS at 63986.14 (admitted against limit 64036.19) and
/// then flows an iteration judged at 63986 × 1.002 ≈ 64108 — rejected,
/// "Total flow: in: 0 out: 0", the offer rests whole. We accepted the same
/// fills judged raw: 39 mutations against mainnet's 8. The instrumented
/// oracle showed NO offer ever executes — the rejection is at sizing time,
/// which per-fill judging before any mutation reproduces.
pub(crate) fn crossing_judge_threshold(
    sandbox: &Sandbox,
    pays_leg: &Leg,
    gets_leg: &Leg,
    taker: &[u8; 20],
    threshold: u64,
) -> u64 {
    // NEITHER side's transfer rate composes into the judge. The trOut
    // factor briefly lived here (`0d4ce0f`) on the strength of C966717C —
    // which the unbounded-AMM rule has since re-explained: those fills
    // never happen at all, so the judge never sees them. What the factor
    // actually did was false-reject legitimate fills: #106225714 8ED637DD
    // buys USD.Bitstamp through a CLOB tip at 971817.3 against a limit of
    // 971914.49 — the MAKER pays the 1.0015 fee on top (`ownerGives`
    // 3091.368 vs `stpOut` 3086.738), the step amounts rippled judges are
    // fee-free, and mainnet fills; the composed threshold 970457 rejected
    // it into tecKILLED. trIn cancels for the reason below; trOut never
    // belonged: `New flow iter` in/out are STEP amounts, and the owner's
    // fee rides OUTSIDE them.
    //
    // trIn: rippled charges the IN issuer's rate on the taker's gross
    // spend AND looses the crossing's limitQuality by the same rate — the
    // two cancel in the judge. The gate proved it: the IoC canary
    // #105887283 4A13D048 (SGB 1.003 on the gets side) went 8v17 the
    // moment the gets-side factor landed.
    let _ = (sandbox, pays_leg, gets_leg, taker);
    threshold
}

fn rate_of_me(pays: Me, gets: Me) -> Option<u64> {
    keylet::rate_encode(pays.0, pays.1, gets.0, gets.1)
}

/// Scale a mantissa/exponent to an integer drop count, rounding to nearest
/// (rippled's `XRPAmount{Number}` conversion under the default round mode).
fn me_rescale_nearest(x: Me) -> u128 {
    let (m, e) = x;
    if m == 0 {
        return 0;
    }
    if e >= 0 {
        return m * 10u128.pow(e.unsigned_abs().min(30));
    }
    let shift = e.unsigned_abs().min(39);
    let d = 10u128.pow(shift);
    let q = m / d;
    let r = m % d;
    let twice = r.saturating_mul(2);
    if twice > d || (twice == d && q & 1 == 1) {
        q + 1
    } else {
        q
    }
}

/// Apply issuer tick-size rounding to a requested offer, exactly as
/// rippled's CreateOffer does BEFORE crossing: round the rate up to the
/// tick, then re-derive the side that isn't held exact — TakerPays for a
/// tfSell offer, TakerGets otherwise.
pub(crate) fn apply_tick_size(pays: Me, gets: Me, sell: bool, tick: u32, pays_xrp: bool, gets_xrp: bool) -> (Me, Me) {
    if tick >= 16 {
        return (pays, gets);
    }
    let Some(rate) = rate_of_me(pays, gets) else {
        return (pays, gets);
    };
    let rounded = quality_round_up(rate, tick);
    let rate_me = ((rounded & 0x00FF_FFFF_FFFF_FFFF) as u128, ((rounded >> 56) as i32) - 100);
    // ⚠ The two sides round DIFFERENTLY, and mainnet pins each of them:
    //
    // * The DIVIDE side takes rippled's STAmount `divide` — truncating muldiv
    //   at 10^17 then `+5`. #105777146 sells 0.00059094 WETH (issuer TickSize
    //   6) for 1 XRP and mainnet stored TakerGets 0.0005909397123305481, one
    //   ulp ABOVE the half-even result, which is what makes its rate encode to
    //   `…ABFB5800`. Using the fill-path divide gave `…5480` and rested the
    //   offer a level off at `…ABFB5801` (4v4, the ledger's sole divergence).
    // * The MULTIPLY side keeps Number's exact half-even (`st_multiply`).
    //   #105667130 `42071037` and #105761560 `DE32DB155A71` are both tfSell
    //   WETH offers whose stored book level is `…E000`; the `+7` STAmount form
    //   puts them at `…E001`. These are the same two ledgers `0EAE58BB` /
    //   `42071037` taught the half-even lesson on originally.
    //
    // Do not "unify" these on one rule — it has now been tried in both
    // directions and each choice breaks the other side's mainnet cases.
    if sell {
        // Hold TakerGets, re-derive TakerPays = TakerGets × rate.
        let p = st_multiply(gets, rate_me, pays_xrp);
        if p.0 == 0 { (pays, gets) } else { (p, gets) }
    } else {
        // Hold TakerPays, re-derive TakerGets = TakerPays ÷ rate.
        let g = stamount_divide(pays, rate_me, gets_xrp);
        if g.0 == 0 { (pays, gets) } else { (pays, g) }
    }
}

/// Fold a raw muldiv result down to 16 significant digits, half-even over the
/// discarded tail — rippled's STAmount canonicalize.
fn fold16(v: u128, e0: i32) -> Me {
    const LO: u128 = 1_000_000_000_000_000;
    const HI: u128 = 10_000_000_000_000_000;
    if v == 0 {
        return (0, 0);
    }
    let mut k = 0u32;
    let mut t = v;
    while t >= HI {
        t /= 10;
        k += 1;
    }
    let d = 10u128.pow(k);
    let (mut m, r) = (v / d, v % d);
    if 2 * r > d || (2 * r == d && m & 1 == 1) {
        m += 1;
    }
    let mut e = e0 + k as i32;
    if m >= HI {
        m /= 10;
        e += 1;
    }
    while m > 0 && m < LO {
        m *= 10;
        e -= 1;
    }
    (m, e)
}

/// rippled `STAmount divide(num, den, asset)`: truncating muldiv at 10^17,
/// `+5`, canonicalize. Distinct from [`st_divide`], which implements Number's
/// exact half-even for the flow/fill path — see [`keylet::rate_encode`].
fn stamount_divide(num: Me, den: Me, xrp: bool) -> Me {
    if num.0 == 0 || den.0 == 0 {
        return (0, 0);
    }
    let (nm, ne) = norm16(num);
    let (dm, de) = norm16(den);
    let v = nm * 100_000_000_000_000_000u128 / dm + 5;
    let e = ne - de - 17;
    if xrp {
        (me_rescale_nearest((v, e)), 0)
    } else {
        fold16(v, e)
    }
}

/// Quality-ordered (rate, offer key) ladder of a book — every resting offer,
/// best first, capped.
fn book_offer_ladder(sandbox: &Sandbox, base: &Hash256, cap: usize) -> Vec<(u64, Hash256)> {
    let mut out = Vec::new();
    for dk in sandbox.keys_with_prefix(&base.0[..24]) {
        let q = u64::from_be_bytes(dk.0[24..32].try_into().unwrap_or_default());
        let mut page_key = dk;
        for _ in 0..10_000 {
            let Some(page) = json_at(sandbox, &page_key) else { break };
            for ent in page.get("Indexes").and_then(|v| v.as_array()).into_iter().flatten() {
                if let Some(k) = ent.as_str().and_then(|s| hex::decode(s).ok())
                    .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                {
                    out.push((q, Hash256(k)));
                    if out.len() >= cap {
                        return out;
                    }
                }
            }
            let next = page.get("IndexNext").map(dirnum).unwrap_or(0);
            if next == 0 {
                break;
            }
            page_key = keylet::dir_page_key(&dk, next);
        }
    }
    out
}

/// Decode a u64-encoded rate into (mantissa, exponent).
/// The best quality ONE hop can offer, in-per-out — the book head competing
/// with the pool's current slice, and NOT a function of how much the pass will
/// move. That independence is the whole point: it is the per-step
/// `qualityUpperBound` that `ActiveStrands::activateNext` ranks strands on.
///
/// Same formula as `d_tip` in `cross_bridged`, which matched rippled's own
/// FLOWDBG `ub strand` value exactly on #105912454.
pub(crate) fn hop_tip(
    sandbox: &Sandbox,
    taker: &[u8; 20],
    in_leg: &Leg,
    out_leg: &Leg,
    amm_iters: u32,
) -> Option<Me> {
    let base = keylet::book_base(&in_leg.cur, &out_leg.cur, &in_leg.issuer, &out_leg.issuer);
    let book = book_offer_ladder(sandbox, &base, 1).first().map(|(q, _)| rate_me(*q));
    let pool = crate::tx::amm_swap::discover(sandbox, in_leg, out_leg, taker).and_then(|a| {
        let init = crate::tx::amm_swap::pool_balances(sandbox, &a, out_leg, in_leg);
        crate::tx::amm_swap::fib_slice(sandbox, &a, init, amm_iters, out_leg, in_leg)
            .map(|s| crate::tx::amm_swap::slice_rate(s.0, s.1))
    });
    match (book, pool) {
        (Some(b), Some(p)) => Some(if me_cmp(p, b).is_lt() { p } else { b }),
        (Some(b), None) => Some(b),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    }
}

/// The hop's candidates SPLIT: the live CLOB tip rate and the AMM with its
/// current frozen-aware pool balances — the raw inputs `limitOut`'s quality
/// function needs (`hop_tip` folds them into one rate; the QF cannot use
/// that fold because only the AMM contributes a non-constant term).
pub(crate) fn hop_tip_parts(
    sandbox: &Sandbox,
    taker: &[u8; 20],
    in_leg: &Leg,
    out_leg: &Leg,
) -> (Option<Me>, Option<(crate::tx::amm_swap::Amm, Me, Me)>) {
    let base = keylet::book_base(&in_leg.cur, &out_leg.cur, &in_leg.issuer, &out_leg.issuer);
    let lob = book_offer_ladder(sandbox, &base, 1).first().map(|(q, _)| rate_me(*q));
    let amm = crate::tx::amm_swap::discover(sandbox, in_leg, out_leg, taker).map(|a| {
        let (pin, pout) = crate::tx::amm_swap::pool_balances(sandbox, &a, out_leg, in_leg);
        (a, pin, pout)
    });
    (lob, amm)
}

/// Decode a u64-encoded rate into (mantissa, exponent).
pub(crate) fn rate_me(q: u64) -> Me {
    ((q & 0x00FF_FFFF_FFFF_FFFF) as u128, ((q >> 56) as i32) - 100)
}

/// First ladder entry whose offer still exists, is an Offer, is non-empty,
/// and is funded. With `mutate`, dead offers encountered on the way are
/// deleted exactly like the direct walk does — but ONLY when their funding
/// state is actually KNOWN (a maker whose root/line was never loaded is
/// skipped, not condemned; phantom deletions poisoned the first bridge
/// attempt). Without `mutate`, this is a pure peek: nothing is touched and
/// the caller's index is not advanced.
fn live_head(
    sandbox: &mut Sandbox,
    ladder: &[(u64, Hash256)],
    start: &mut usize,
    taker: &[u8; 20],
    maker_pays_leg: &Leg,
    // What the maker RECEIVES — the book's IN asset, which its owner must be
    // authorized to hold (`require_auth_known`).
    maker_gets_leg: &Leg,
    // Self-offers and DEAD offers (expired/unfunded) are removed under
    // separate flags: a stream-step reaps dead offers it passes even in a
    // PEEK once the strand is running (#106093637 1BE79D4A), but a peeked
    // SELF-offer must stay — strands are priced against the unmodified view
    // and removals apply only when execution steps onto them (#105930662).
    mutate_self: bool,
    mutate_dead: bool,
    stale: &mut Vec<Hash256>,
) -> Option<(u64, Hash256, serde_json::Value, [u8; 20], Me, Me)> {
    let mut i = *start;
    let result = loop {
        if i >= ladder.len() {
            break None;
        }
        let (q, okey) = ladder[i];
        let Some(offer) = json_at(sandbox, &okey) else { i += 1; continue };
        if offer.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("Offer") {
            i += 1;
            continue;
        }
        let Some(maker) = offer.get("Account").and_then(|v| v.as_str()).and_then(decode20) else {
            i += 1;
            continue;
        };
        if &maker == taker {
            if mutate_self {
                delete_maker_offer(sandbox, &okey, &offer, &maker);
                stale.push(okey);
            }
            i += 1;
            continue;
        }
        // EXPIRED OFFERS ARE NEVER CROSSED. `hasExpired` is inclusive against
        // the BASE ledger's close time (View.cpp:48; BookStep.cpp builds the
        // stream with `sb.parentCloseTime()`), and rippled's stream collects
        // them as removable — "Removing expired offer".
        //
        // The DIRECT walk has tested this since #105776250 and
        // `reap_to_live_head` tests it too; `live_head` — which is what the
        // BRIDGED walk steps with — did not. A rule at one call site and not
        // its sibling, again.
        //
        // #106348756 2763653C: the leg-B book head at iteration 3 is
        // rLDyWWMiW6sQ's 0CCFCEC1, funded with 14.545506 RLUSD and EXPIRED.
        // rippled removes it (with rD8VchnJEJ5A, rBtVeRQ8NWUj, rwnJpjMn18m7)
        // and walks on to rURtT5MM at a worse rate — 533196 drops for the
        // remaining 0.533553078068178. We crossed the expired offer instead and
        // paid 533119, taking the same output from liquidity that does not
        // exist. 43 mutations against 59.
        //
        // ⚠ Funding is NOT the discriminator here: every expired offer above is
        // well funded. An offer can be live, funded and still uncrossable.
        if let Some(exp) = offer.get("Expiration").and_then(|v| v.as_u64()) {
            if exp != 0 && sandbox.base().header.close_time as u64 >= exp {
                if mutate_dead {
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                }
                i += 1;
                continue;
            }
        }
        let (Some(gives), Some(wants)) = (
            offer.get("TakerGets").and_then(keylet::amount_mant_exp),
            offer.get("TakerPays").and_then(keylet::amount_mant_exp),
        ) else {
            i += 1;
            continue;
        };
        // Funding is only judgeable when the backing object is in state.
        let funding_known = if maker_pays_leg.xrp {
            json_at(sandbox, &keylet::account_root_key(&maker)).is_some()
        } else {
            maker == maker_pays_leg.issuer
                || json_at(sandbox, &keylet::ripple_state_key(&maker, &maker_pays_leg.issuer, &maker_pays_leg.cur)).is_some()
            // ...or whose ACCOUNT ROOT we hold. A maker we have hydrated but
            // that has NO trust line for the currency it is selling holds ZERO
            // of it — absence IS the answer, not a gap in what we loaded. The
            // guard exists so an UNHYDRATED maker is never condemned; a maker
            // whose root is in hand plainly was hydrated.
            //
            // #106096771 69A1FF138D85: rDaQRnUv rests three STSH offers
            // (F8068209, 7E1CCB88, 8EBA0006) across three quality levels and
            // holds no STSH trust line AT ALL — `ledger_entry` says
            // entryNotFound on mainnet itself. rippled logs `Removing unfunded
            // offer` for each; we read "cannot judge" and left all three, plus
            // their emptied book pages: 8 mutations against 16. The same book
            // costs the Payment 9EBC82AB5041 two more.
            //
            // Safe now that a dropped fetch aborts the verdict (cae6b85): the
            // only way a real line goes missing is a hydration failure, and
            // that no longer reaches a divergence verdict.
                || json_at(sandbox, &keylet::account_root_key(&maker)).is_some()
        };
        if !funding_known {
            i += 1;
            continue;
        }
        if gives.0 == 0 || wants.0 == 0 || me_is_zero(available(sandbox, &maker, maker_pays_leg)) {
            if mutate_dead {
                delete_maker_offer(sandbox, &okey, &offer, &maker);
                stale.push(okey);
            }
            i += 1;
            continue;
        }
        // BookStep.cpp:755 — an owner unauthorized for the asset flowing INTO
        // it is never crossable; the stream perm-removes it "even if no
        // crossing occurs" (deletion under the same flag as the other dead
        // arms; a peek still advances past it).
        if require_auth_known(sandbox, maker_gets_leg, &maker) == Some(false) {
            if mutate_dead {
                delete_maker_offer(sandbox, &okey, &offer, &maker);
                stale.push(okey);
            }
            i += 1;
            continue;
        }
        break Some((q, okey, offer, maker, gives, wants));
    };
    // Only the true mutation-walk persists the cursor: a reaping PEEK must
    // not advance it past skipped self-offers — `d_tip` prices the strand
    // from the ladder at this index, and moving it re-decides multiPath.
    if mutate_self {
        *start = i;
    }
    result
}

/// Advance one book level to its first crossable offer, deleting the dead
/// ones ahead of it, and report whether such an offer remains.
///
/// rippled steps the offer stream BEFORE it consults the pool
/// (`BookStep::forEachOffer`, BookStep.cpp:835 `if (offers.step())` …
/// `tryAMM(offers.tip().quality())`), and `OfferStream::step` reaps every
/// dead offer it advances past — missing, expired, empty, frozen, unfunded —
/// into `permToRemove`, which the step returns whether or not anything
/// crossed (BookStep.cpp:852). Two consequences we were missing: dead offers
/// are removed even when no value moves, and the pool's `clobQuality` is the
/// first LIVE offer's quality, not the first book level's.
///
/// The reap set here is a subset of what the page walk in `cross_engine_to`
/// already deletes on reaching an offer, so a level the walk fully enters
/// behaves exactly as before; what changes is the levels it never enters —
/// the pool satisfying the whole fill at the first level, or a level past the
/// taker's limit. A self-owned offer stops the scan instead of being
/// cancelled: `step` has no owner==taker case (self-crossing belongs to offer
/// crossing, not to a payment's book step), so the page walk keeps that job.
///
/// #105795716 5CDFDC74: the XRP→RPLS book's only level held one offer that
/// had expired 1h50m before the parent closed. Mainnet reaped it — offer and
/// its emptied book page Deleted, the maker's root and owner dir Modified,
/// and **no RippleState**, so nothing crossed — then filled from the pool.
/// We anchored the pool to that dead level, filled the whole 39549 drops from
/// it, and broke out before ever reading the page (8 muts vs 12).
///
/// Funding is judged only when the maker's backing object is in state; an
/// unhydrated maker is treated as live rather than condemned, as in
/// `live_head` (phantom deletions poisoned the first bridge attempt).
/// `shouldRmSmallIncreasedQOffer` (OfferStream.cpp:136), applied by `step`
/// immediately after the unfunded test (OfferStream.cpp:302): an offer backed
/// by so little owner funding that the fill it could actually make floors away
/// — or lands at a strictly worse quality than it advertises — is REMOVED
/// rather than crossed for nothing.
///
/// #105778999 6B2A11B3: three offers each selling 5,907,469.49 POSAA for
/// 343.742177 XRP whose owners held 0.0028–0.0103 POSAA. Clamped to owner
/// funds the input is ~0.36 drops, which floors to zero, so mainnet deleted
/// all three — `OwnerCount` decremented, not one balance moved.
fn is_dust_offer(
    sandbox: &mut Sandbox,
    maker: &[u8; 20],
    m_wants0: Me,
    m_gives0: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
) -> bool {
    // "If `TakerGets` is XRP, the worst this offer's quality can change is to
    // about 10^-81 `TakerPays` and 1 drop `TakerGets`. This will be remarkably
    // good quality for any realistic asset, so these offers don't need this
    // extra check."
    if pays_leg.xrp {
        return false;
    }
    // Both sides IOU: only when `TakerPays` < `TakerGets`.
    if !gets_leg.xrp && me_cmp(m_wants0, m_gives0).is_ge() {
        return false;
    }
    let funds = available(sandbox, maker, pays_leg);
    // `ceilOutStrict(ofrAmts, ownerFunds, roundUp=false)` — note the rounding
    // is DOWN here, unlike the crossing walk's `pay`, which is what makes a
    // dust-backed fill vanish instead of costing a drop.
    let (eff_in, eff_out) = if maker != &pays_leg.issuer && me_cmp(funds, m_gives0).is_lt() {
        let mut i = me_muldiv(funds, m_wants0, m_gives0, false);
        if gets_leg.xrp {
            i = (me_rescale(i, 0, false), 0);
        }
        (i, funds)
    } else {
        (m_wants0, m_gives0)
    };
    if me_is_zero(eff_in) || me_is_zero(eff_out) {
        return true;
    }
    // `effectiveAmounts.in > TTakerPays::minPositiveAmount()` — above the
    // smallest representable input the quality cannot have shifted enough to
    // matter. For an IOU input that bound is ~1e-81, so only the zero case
    // above ever fires there.
    if !gets_leg.xrp || me_cmp(eff_in, (1, 0)).is_gt() {
        return false;
    }
    // `effectiveQuality < offer_.quality()` — our rates are inverted, so a
    // strictly WORSE effective quality is a strictly GREATER rate.
    match (rate_of_me(eff_in, eff_out), rate_of_me(m_wants0, m_gives0)) {
        (Some(e), Some(o)) => e > o,
        _ => false,
    }
}

/// The dead tests `OfferStream::step` applies to one offer, in its order
/// (OfferStream.cpp:192): expired, either amount zero, owner unfunded. Returns
/// whether the offer was reaped; `false` means the stream stops here.
///
/// An offer the stream cannot judge is left alone rather than condemned — an
/// unhydrated maker reads as unfunded and phantom deletions poisoned the first
/// bridge attempt. Deep-frozen and out-of-domain, the two remaining
/// `permRmOffer` conditions, are unmodelled here exactly as they are in the
/// crossing walk.
///
/// A SELF-OWNED offer is judged by these tests like any other. `step` has no
/// owner==taker case — every condition it applies reads `offer_.owner()` and
/// none compares it to the taker — so a self-owned offer that is DEAD is
/// reaped here; only a LIVE one stops the scan, because cancelling that is
/// offer crossing's `limitSelfCrossQuality`, not the stream's job. This
/// function used to bail on `maker == taker` before any test, which inverted
/// that reading: it took "step has no owner==taker case" as grounds to skip
/// self-owned offers entirely, when it is grounds to treat them normally.
///
/// #105949459 4A03010A4B1E: `rKkBNf2d` buys 63 ShearPepe while holding NONE,
/// and its own `EC95059B` (seq 93095690) sits at the head of that very book
/// promising to sell 63 it does not have. The pool fills the whole 708735
/// drops, so the walk never enters the page, and the bail then stopped the
/// level scan from reaping it — 4 mutations against 7, the missing three being
/// exactly that offer, its emptied book page `079C9589`, and the owner
/// directory `7B6745CD`. rippled logs `Removing unfunded offer EC95059B…`.
///
/// ⚠ Unmodelled: rippled separates "found unfunded" from "became unfunded"
/// (OfferStream.cpp, `originalFunds == *ownerFunds_`) and only the former is a
/// `permRmOffer`. This runs before the level's AMM turn, so current funds are
/// still the pristine funds and the distinction cannot bite here; a reap sited
/// after a fill would need it.
fn reap_if_dead(
    sandbox: &mut Sandbox,
    okey: &Hash256,
    offer: &serde_json::Value,
    maker: &[u8; 20],
    pays_leg: &Leg,
    gets_leg: &Leg,
    oc0: Option<&mut std::collections::HashMap<[u8; 20], u64>>,
    stale: &mut Vec<Hash256>,
) -> bool {
    // `hasExpired`: the BASE ledger's close time is the test (View.cpp:48, and
    // BookStep.cpp:705 builds the stream with `sb.parentCloseTime()`).
    if let Some(exp) = offer.get("Expiration").and_then(|v| v.as_u64()) {
        if exp != 0 && sandbox.base().header.close_time as u64 >= exp {
            delete_maker_offer(sandbox, okey, offer, maker);
            stale.push(*okey);
            return true;
        }
    }
    let (Some(m_gives0), Some(m_wants0)) = (
        offer.get("TakerGets").and_then(keylet::amount_mant_exp),
        offer.get("TakerPays").and_then(keylet::amount_mant_exp),
    ) else { return false };
    if m_gives0.0 == 0 || m_wants0.0 == 0 {
        delete_maker_offer(sandbox, okey, offer, maker);
        stale.push(*okey);
        return true;
    }
    if std::env::var("DX_RM").is_ok() {
        eprintln!(
            "DX_RM peek maker={} gives0={m_gives0:?} avail={:?} acct={}",
            hex::encode(maker),
            available(sandbox, maker, pays_leg),
            json_at(sandbox, &keylet::account_root_key(maker))
                .map(|a| format!("bal={:?} oc={:?}", a.get("Balance"), a.get("OwnerCount")))
                .unwrap_or_default(),
        );
    }
    let funding_known = if pays_leg.xrp {
        json_at(sandbox, &keylet::account_root_key(maker)).is_some()
    } else {
        maker == &pays_leg.issuer
            || json_at(sandbox, &keylet::ripple_state_key(maker, &pays_leg.issuer, &pays_leg.cur)).is_some()
            // See `live_head` above: a hydrated maker with no line for what it
            // sells holds zero of it, and absence is the answer.
            || json_at(sandbox, &keylet::account_root_key(maker)).is_some()
    };
    if funding_known && me_is_zero(walk_available(sandbox, maker, pays_leg, oc0)) {
        delete_maker_offer(sandbox, okey, offer, maker);
        stale.push(*okey);
        return true;
    }
    if funding_known && is_dust_offer(sandbox, maker, m_wants0, m_gives0, pays_leg, gets_leg) {
        delete_maker_offer(sandbox, okey, offer, maker);
        stale.push(*okey);
        return true;
    }
    // BookStep.cpp:755 — after the stream's own dead tests, the caller
    // perm-removes any offer whose OWNER may not hold the asset flowing INTO
    // it (the maker receives `gets_leg`), "even if no crossing occurs".
    if require_auth_known(sandbox, gets_leg, maker) == Some(false) {
        delete_maker_offer(sandbox, okey, offer, maker);
        stale.push(*okey);
        return true;
    }
    false
}

fn reap_to_live_head(
    sandbox: &mut Sandbox,
    dk: &Hash256,
    pays_leg: &Leg,
    gets_leg: &Leg,
    mut oc0: Option<&mut std::collections::HashMap<[u8; 20], u64>>,
    stale: &mut Vec<Hash256>,
) -> bool {
    let mut page_key_h = *dk;
    for _ in 0..10_000 {
        // An unreadable page is unknown, not empty: claiming the level dead
        // would suppress the pool's anchor on evidence we do not have. Report
        // it live and leave the level exactly as it behaved before.
        let Some(page) = json_at(sandbox, &page_key_h) else { return true };
        let entries: Vec<String> = page
            .get("Indexes")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        for ent in entries {
            let Some(okey) = hex::decode(&ent)
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                .map(xrpl_core::types::Hash256)
            else { continue };
            let Some(offer) = json_at(sandbox, &okey) else { continue };
            if offer.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("Offer") {
                continue;
            }
            let Some(maker) = offer.get("Account").and_then(|v| v.as_str()).and_then(decode20)
            else { continue };
            if !reap_if_dead(sandbox, &okey, &offer, &maker, pays_leg, gets_leg, oc0.as_deref_mut(), stale) {
                return true;
            }
        }
        let next = page.get("IndexNext").map(dirnum).unwrap_or(0);
        if next == 0 {
            return false;
        }
        page_key_h = keylet::dir_page_key(dk, next);
    }
    false
}

/// A maker offer's remaining side after a fill, as an STAmount.
///
/// `TOffer::consume` subtracts through STAmount, so the result is rounded back
/// to 16 significant digits (half-even, via `Number` under fixUniversalNumber).
/// `me_sub` is EXACT — it keeps every digit of the difference — so whenever
/// `gives0 - give` needs more than 16 digits we stored precision the ledger
/// cannot hold and rippled does not have.
///
/// Same family as the fused-vs-stepwise arithmetic elsewhere: `Number`'s
/// operations are implementations, not ideal maths, and being more precise than
/// rippled is a defect.
fn offer_residual(a: Me, b: Me) -> Me {
    stamount_signed_add(false, a, true, b).1
}

/// Consume `give` of the maker's gives / `pay` of their wants against one
/// offer: move both legs and update or delete the offer object.
#[allow(clippy::too_many_arguments)]
fn settle_fill(
    sandbox: &mut Sandbox,
    okey: &Hash256,
    offer: &serde_json::Value,
    maker: &[u8; 20],
    from_taker: &[u8; 20],
    to_beneficiary: &[u8; 20],
    pays_leg: &Leg,
    gets_leg: &Leg,
    give: Me,
    pay: Me,
    // What the TAKER parts with on the gets leg — `pay` plus the input
    // issuer's transfer fee, which the issuer destroys. Equal to `pay` where
    // no rate applies. See `move_leg_gross`.
    pay_gross: Me,
    gives0: Me,
    wants0: Me,
) {
    move_leg(sandbox, maker, to_beneficiary, pays_leg, give);
    move_leg_gross(sandbox, from_taker, maker, gets_leg, pay, pay_gross);
    let funded = available(sandbox, maker, pays_leg);
    let consumed = me_cmp(give, gives0).is_ge() || me_is_zero(funded);
    if consumed {
        delete_maker_offer(sandbox, okey, offer, maker);
    } else if me_is_zero(give) && me_is_zero(pay) {
        // Finding 40 (#106668867 575275AC, live shadow PRE-UNKNOWN(no-meta)):
        // a ZERO fill is an offer examined, not an offer moved — rewriting it
        // with identical amounts changes nothing except the threading stamp,
        // and rippled leaves walked-past offers untouched (canonical stamp on
        // the specimen still reads ledger 95475679 from 2023). The delete arm
        // above still handles the walk-past-unfunded case (funded == 0).
    } else {
        let mut off2 = offer.clone();
        off2["TakerGets"] = me_amount_json(&offer["TakerGets"], offer_residual(gives0, give));
        off2["TakerPays"] = me_amount_json(&offer["TakerPays"], offer_residual(wants0, pay));
        put_json(sandbox, *okey, &off2);
    }
}

/// IOU↔IOU crossing with XRP autobridging (rippled FlowCross): at every step
/// the DIRECT book competes with the two-book XRP bridge; the better maker
/// rate is consumed one offer (or one bridge slice) at a time. Engaged only
/// when both bridge books have depth — otherwise the plain direct walk runs.
/// Reaching here means two strands, i.e. rippled's `AMMContext::multiPath` is
/// true, which is why the pool competes with Fibonacci slices below.
#[allow(clippy::too_many_arguments)]
/// Delete the taker's OWN offers off the tip of the book instead of crossing
/// them — rippled `limitSelfCrossQuality`, BookStep.cpp:415-455, whose own
/// commentary explains the choice: "We could skip over the self offer in the
/// book and only cross offers that are not our own. This would make a lot of
/// sense, but we don't do it. Part of the rationale is that we can only operate
/// on the tip of the order book. We can't leave an offer behind -- it would sit
/// on the tip and block access to other offers." Removal is unconditional on
/// any fill: "Remove this offer even if no crossing occurs."
///
/// Three conditions, all required (BookStep.cpp:443):
///   a. `defaultPath_` — the DIRECT strand only, never the autobridged one;
///   b. `offer.quality() >= qualityThreshold_` — inside the taker's limit;
///   c. `strandSrc == strandDst == offer.owner()` — offer crossing, and ours.
///
/// (b) is the condition both earlier attempts at this omitted, and it is doing
/// real work: it is met here only because the taker's limit is priced off the
/// transfer-rate-inflated `sendMax`. At rate 1.0 nothing qualifies.
///
/// The check lives in `execOffer`, which `forEachOffer` reaches only when
/// `tryAMM` did NOT exhaust the strand (BookStep.cpp:855-865 — `if
/// (tryAMM(offers.tip().quality())) { do { execOffer(offers.tip()) } while
/// (offers.step()); }`). The pool therefore gets its turn FIRST, and an order
/// the pool fills on its own removes nothing at all. Call sites must preserve
/// that order.
fn reap_self_offers_at_head(
    sandbox: &mut Sandbox,
    ladder: &[(u64, Hash256)],
    start: usize,
    taker: &[u8; 20],
    threshold: u64,
    stale: &mut Vec<Hash256>,
) {
    for &(q, okey) in ladder.iter().skip(start) {
        if q > threshold {
            return;
        }
        let Some(offer) = json_at(sandbox, &okey) else { continue };
        if offer.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("Offer") {
            continue;
        }
        let Some(maker) = offer.get("Account").and_then(|v| v.as_str()).and_then(decode20)
        else { continue };
        // Stops at the first offer that is not ours: rippled removes only what
        // sits AT the tip, then crosses whatever it uncovers.
        if &maker != taker {
            return;
        }
        delete_maker_offer(sandbox, &okey, &offer, &maker);
        stale.push(okey);
    }
}

fn cross_bridged(
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    mut rem_pays: Me,
    mut rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    // rippled's true `limitQuality` (transfer-rate inflated): self-cross gate only.
    threshold_self: u64,
    sell: bool,
    inv_base: &Hash256,
    amm: &Option<crate::tx::amm_swap::Amm>,
    // The crossing's GROSS in-budget (see `gets_gross_cap` on the walk):
    // rippled consumes flowCross's grossed sendMax, and the slice that
    // exhausts it takes the remainder VERBATIM, net by division (F50/F51).
    gets_gross_cap: Option<Me>,
    sandbox: &mut Sandbox,
    stale: &mut Vec<Hash256>,
) -> Option<(Me, Me, u32)> {
    let mut in_gross_spent: Me = (0, 0);
    // Fee-composed judge threshold for CLOB leg-B fills (AMM and
    // taker-owned leg-B offers waive) — crossing_judge_threshold.
    let thr_judge = crossing_judge_threshold(sandbox, pays_leg, gets_leg, taker, threshold);
    let xrp_leg = Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] };
    let zero = [0u8; 20];
    // Leg A's input issuer charges its TransferRate because the taker is
    // REDEEMING the IOU it spends: `trIn = redeems(prevStepDir) ? rate(book_.in,
    // strandDst_) : parity` (BookStep.cpp:352), applied as
    // `stpAmt.in = mulRatio(ofrAmt.in, ofrInRate, QUALITY_ONE, roundUp)` (:770).
    // The MAKER receives `gets_in`; the TAKER parts with `gets_in * rate` and
    // the issuer DESTROYS the difference. `None` when the taker issues the
    // currency itself — issuing is free.
    //
    // ⚠ This changes NOTHING about sizing, and must not. Measured on
    // #105887283 4A13D048, our slices already match rippled's to the last
    // digit once its `in` is read as GROSS:
    //     ours 0.005771047782      x1.0015 = 0.005779704353673   = rippled
    //     ours 0.0034269515228004  x1.0015 = 0.003432091950084601 = rippled
    // and TakerGets bounds the NET, which is why rippled's total in
    // (0.009211796303757601) legitimately EXCEEDS TakerGets (0.0092).
    // Three earlier attempts divided a budget by this rate instead — the
    // direct walk, the call site, and `net_cap` on both bridged clamps. All
    // three re-sized fills that were already correct and all three regressed,
    // #105887283 as far as 87/88 at KEY level. The missing piece was only ever
    // the taker's debit.
    let fee_rate = match transfer_rate(sandbox, gets_leg) {
        Some(r) if taker != &gets_leg.issuer => Some(r),
        _ => None,
    };
    // Leg A: spend our gets, acquire XRP. Leg B: spend XRP, acquire our pays.
    let base_a = keylet::book_base(&gets_leg.cur, &zero, &gets_leg.issuer, &zero);
    let base_b = keylet::book_base(&zero, &pays_leg.cur, &zero, &pays_leg.issuer);
    let la = book_offer_ladder(sandbox, &base_a, 128);
    let lb = book_offer_ladder(sandbox, &base_b, 128);
    // Per-leg AMM liquidity: each bridge leg is a BookStep of its own pair.
    let amm_a = crate::tx::amm_swap::discover(sandbox, gets_leg, &xrp_leg, taker);
    let amm_b = crate::tx::amm_swap::discover(sandbox, &xrp_leg, pays_leg, taker);
    if std::env::var("DX_BRIDGE").is_ok() {
        let ka = keylet::amm_key(&gets_leg.cur, &gets_leg.issuer, &xrp_leg.cur, &xrp_leg.issuer);
        let kb = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &pays_leg.cur, &pays_leg.issuer);
        eprintln!(
            "DX_BRIDGE ammdisc a={} a_key={} a_in_sb={} b={} b_key={} b_in_sb={}",
            amm_a.is_some(), hex::encode(&ka.0[..8]), sandbox.exists(&ka),
            amm_b.is_some(), hex::encode(&kb.0[..8]), sandbox.exists(&kb),
        );
    }
    let amm_a_init = amm_a
        .as_ref()
        .map(|a| crate::tx::amm_swap::pool_balances(sandbox, a, &xrp_leg, gets_leg))
        .unwrap_or(((0, 0), (0, 0)));
    let amm_b_init = amm_b
        .as_ref()
        .map(|a| crate::tx::amm_swap::pool_balances(sandbox, a, pays_leg, &xrp_leg))
        .unwrap_or(((0, 0), (0, 0)));
    // rippled keeps ONE `ammIters_` for the whole flow (AMMContext), not one
    // per pool, and `ammContext.update()` bumps it once per ITERATION in which
    // any AMM was used. Every pool's fib slice is therefore indexed by the
    // SAME counter — consuming the direct pair's pool grows the BRIDGE legs'
    // slices too, which is what makes a bridged strand's quality decay as the
    // crossing proceeds.
    //
    // #105876680 26AF14542086: rippled's bridge upper bound goes 73.44102575
    // -> 73.44102375 -> 73.4592084 across the three iterations while its leg A
    // slice doubles (1124.099/13860373 -> 2248.755/27720746). Ours stayed at
    // 73.44102575 because leg A had its own counter that never moved — the
    // direct pool was doing the crossing — so on iteration 2 the bridge looked
    // BETTER than the pool and we abandoned the pool for it: 10 mutations
    // against 5, all five extra.
    if (la.is_empty() && amm_a.is_none()) || (lb.is_empty() && amm_b.is_none()) {
        if std::env::var("DX_BRIDGE").is_ok() {
            eprintln!("DX_BRIDGE bail la={} lb={} base_a={} base_b={}",
                la.len(), lb.len(), hex::encode_upper(&base_a.0[..12]), hex::encode_upper(&base_b.0[..12]));
        }
        return None; // no bridge: caller runs the direct walk
    }
    let ld = book_offer_ladder(sandbox, inv_base, 128);
    let thr = (threshold != u64::MAX).then(|| rate_me(threshold));
    let (mut di, mut ai, mut bi) = (0usize, 0usize, 0usize);
    let mut crossed = 0u32;
    // Offer crossing is always multi-strand (direct + XRP bridge), so the pool
    // competes with FIB-sequence offers off its starting balances. Single-
    // strand walks never reach here — they size the AMM through
    // `amm_swap::consume` on the direct walk instead.
    let amm_init = amm.as_ref().map(|a| crate::tx::amm_swap::pool_balances(sandbox, a, pays_leg, gets_leg));
    let mut amm_iters = 0u32;
    // Set when any pool moved value this iteration; folded into `amm_iters`
    // at the end of the pass, mirroring `ammContext.update()`.
    let mut amm_used;
    let done = |rp: Me, rg: Me| me_is_zero(rg) || (!sell && me_is_zero(rp));
    // `AMMContext::multiPath()` as the ADMISSION sees it: `activateNext` runs
    // BEFORE `setMultiPath`, so each iteration's `qualityUpperBound` prices
    // its AMM contributions under the PREVIOUS iteration's flag — true on
    // entry (Flow.cpp:106, `strands.size() > 1`). The round's own `multi_now`
    // (computed after admission, below) becomes next round's flag.
    let mut multi_prev = true;
    for _ in 0..512 {
        if done(rem_pays, rem_gets) {
            break;
        }
        amm_used = false;
        // PEEK both sources (no mutation) to pick the better rate within the
        // threshold; only the chosen source is then walked with mutation, so
        // dead-offer cleanup happens exactly where rippled's walk reaches.
        // Each BRIDGE LEG is a BookStep of its own pair, so its book head
        // competes with that pair's pool (fib slice) — #105666830's XAH leg
        // filled from the XAH/XRP pool on mainnet.
        // Once the crossing has EXECUTED a fill, the strand is definitely
        // built and every subsequent stream-step removes the dead offers it
        // passes — rippled reaps them even in an iteration whose liquidity
        // then comes from the AMM. #106093637 1BE79D4A: iter 0 crosses the
        // legB head (rD8V, funded 0.59584264555), iter 1's stream steps over
        // the EXPIRED rsqztuzpo (exp 839271795 ≤ parent close 839271841) and
        // removes it — offer, book page, owner dir and OwnerCount — while
        // the 0.404 fill itself comes from the pool. Our peeks skipped it
        // silently. Before any fill the peeks stay non-mutating: a strand
        // that is never built reaps nothing (#105795013-analog).
        let peek_rm = crossed > 0;
        let dpeek = live_head(sandbox, &ld, &mut di, taker, pays_leg, gets_leg, false, peek_rm, stale);
        let apeek = live_head(sandbox, &la, &mut ai, taker, &xrp_leg, gets_leg, false, peek_rm, stale);
        let bpeek = live_head(sandbox, &lb, &mut bi, taker, pays_leg, &xrp_leg, false, peek_rm, stale);
        let a_fib = amm_a.as_ref().and_then(|am| {
            crate::tx::amm_swap::fib_slice(sandbox, am, amm_a_init, amm_iters, &xrp_leg, gets_leg)
                .map(|s| (crate::tx::amm_swap::slice_rate(s.0, s.1), s))
        });
        let b_fib = amm_b.as_ref().and_then(|am| {
            crate::tx::amm_swap::fib_slice(sandbox, am, amm_b_init, amm_iters, pays_leg, &xrp_leg)
                .map(|s| (crate::tx::amm_swap::slice_rate(s.0, s.1), s))
        });
        // A BOOK offer competing with a POOL is not worth its face quality: in
        // offer crossing `ownerPaysTransferFee_` is true, so the offer OWNER
        // pays the output issuer's TransferRate and the taker receives that
        // much less (BookStep.cpp:737-739). An AMM offer pays no such fee.
        // Only a leg whose OUTPUT carries an issuer is affected — leg A pays
        // out XRP and is never discounted.
        let out_rate = transfer_rate(sandbox, pays_leg);
        let discount = |q: Me| match out_rate {
            Some(r) => me_muldiv(q, (r as u128, 0), (1_000_000_000, 0), true),
            None => q,
        };
        let qa_book = apeek.as_ref().map(|(q, ..)| rate_me(*q));
        let qb_book = bpeek.as_ref().map(|(q, ..)| rate_me(*q));
        // Each leg pool's SPOT quality — `Quality{balances}`, the comparison
        // quality `maxOffer` hands `tip`. Raw in/out off CURRENT balances, no
        // trading fee: `Quality{balances}` applies none.
        let spot_of = |am: Option<&crate::tx::amm_swap::Amm>, out_leg: &Leg, in_leg: &Leg, sandbox: &mut Sandbox| -> Option<Me> {
            let a = am?;
            let (pin, pout) = crate::tx::amm_swap::pool_balances(sandbox, a, out_leg, in_leg);
            if pin.0 == 0 || pout.0 == 0 { return None; }
            Some(me_muldiv(pin, (1u128, 0i32), pout, true))
        };
        let spot_a = spot_of(amm_a.as_ref(), &xrp_leg, gets_leg, sandbox);
        let spot_b = spot_of(amm_b.as_ref(), pays_leg, &xrp_leg, sandbox);
        // The pool wins a leg only when STRICTLY better than the book head.
        let a_use_amm = match (&a_fib, qa_book) {
            (Some((qf, _)), Some(qb)) => me_cmp(*qf, qb).is_lt(),
            (Some(_), None) => true,
            _ => false,
        };
        let b_use_amm = match (&b_fib, qb_book) {
            (Some((qf, _)), Some(qb)) => me_cmp(*qf, discount(qb)).is_lt(),
            (Some(_), None) => true,
            _ => false,
        };
        // fixAMMv1_1 `qualityThreshold` override (BookStep.cpp:461, 475-481):
        // in SINGLE-path crossing, when the strand's limitQuality is BETTER
        // than a leg's LOB tip — a unit-blind Quality compare, so on bridge
        // legs it contrasts cross-pair rates and drops-inflation decides — the
        // AMM is not anchored to the tip: `getAMMOffer(nullopt)` emits the
        // unbounded curve-priced maxOffer, and `checkQualityThreshold` prunes
        // every CLOB offer by the same compare, so the pool is that leg's ONLY
        // liquidity. C966717C legB receipt: `TRYAMM lob=59555359684 thr=none`
        // — the better-priced 59555 CLOB tip never executes; the pool prices
        // 0.02981 BTC at 1809.549786 XRP and the realised 64107.6 misses
        // limit 64036.19, resting the offer. `qualityThreshold_` is rippled's
        // TRUE limitQuality — our transfer-rate-inflated `threshold_self`.
        // Single-path only ("Multi-path AMM offers work the same as LOB
        // offers"), so the forcing is applied after `multi_now` is known;
        // admission (ub) and candidate selection stay tip-based, exactly as
        // rippled's `qualityUpperBound` does.
        let a_unb_raw = amm_a.is_some()
            && threshold_self != 0
            && apeek.as_ref().is_some_and(|(q, ..)| threshold_self < *q);
        let b_unb_raw = amm_b.is_some()
            && threshold_self != 0
            && bpeek.as_ref().is_some_and(|(q, ..)| threshold_self < *q);
    // An AMM leg does NOT price linearly. `b_out_full/b_in_full` is the ratio of
    // that leg's fib SLICE — on #105912454 leg B the slice is ~26.1M drops while
    // the pass moves 1M — so scaling it down linearly under-delivers and every
    // quality computed from it comes out pessimistic. Reprice the actual amount
    // through the pool instead, which is what rippled's AMMOffer does on
    // execution (swapAssetIn / swapAssetOut), reserving the slice purely for
    // sizing.
    let reprice_b = |use_amm: bool, xrp: Me, lin: Me, sandbox: &Sandbox| -> Me {
        match (use_amm, amm_b.as_ref()) {
            (true, Some(am)) => {
                let (pin, pout) = crate::tx::amm_swap::pool_balances(sandbox, am, pays_leg, &xrp_leg);
                if pin.0 == 0 || pout.0 == 0 {
                    return lin;
                }
                crate::tx::amm_swap::swap_asset_in(pin, pout, xrp, am.tfee, pays_leg.xrp)
            }
            _ => lin,
        }
    };
    // The INVERSES of reprice_a/reprice_b. `AMMOffer::limitIn`/`limitOut` run
    // the conservation function in BOTH directions for a single-path pool, so
    // backing `xrp` out of a clamped leg amount must also go through the pool.
    // Doing that step linearly at the offer's average ratio is what still broke
    // #105940336 after the forward repricing was fixed: at `maxOffer` the
    // average is ~100x off spot, so the reverse map produced an absurd `xrp`.
    let unreprice_b = |use_amm: bool, pays_out: Me, lin: Me, sandbox: &Sandbox| -> Me {
        match (use_amm, amm_b.as_ref()) {
            (true, Some(am)) => {
                let (pin, pout) = crate::tx::amm_swap::pool_balances(sandbox, am, pays_leg, &xrp_leg);
                if pin.0 == 0 || pout.0 == 0 {
                    return lin;
                }
                crate::tx::amm_swap::swap_asset_out(pin, pout, pays_out, am.tfee, xrp_leg.xrp)
                    .filter(|v| v.0 != 0)
                    .unwrap_or(lin)
            }
            _ => lin,
        }
    };
    let unreprice_a = |use_amm: bool, gets_in: Me, lin: Me, sandbox: &Sandbox| -> Me {
        match (use_amm, amm_a.as_ref()) {
            (true, Some(am)) => {
                let (pin, pout) = crate::tx::amm_swap::pool_balances(sandbox, am, &xrp_leg, gets_leg);
                if pin.0 == 0 || pout.0 == 0 {
                    return lin;
                }
                let v = crate::tx::amm_swap::swap_asset_in(pin, pout, gets_in, am.tfee, xrp_leg.xrp);
                if v.0 == 0 { lin } else { v }
            }
            _ => lin,
        }
    };
    let reprice_a = |use_amm: bool, xrp: Me, lin: Me, sandbox: &Sandbox| -> Me {
        match (use_amm, amm_a.as_ref()) {
            (true, Some(am)) => {
                let (pin, pout) = crate::tx::amm_swap::pool_balances(sandbox, am, &xrp_leg, gets_leg);
                if pin.0 == 0 || pout.0 == 0 {
                    return lin;
                }
                match crate::tx::amm_swap::swap_asset_out(pin, pout, xrp, am.tfee, gets_leg.xrp) {
                    Some(v) if v.0 != 0 => v,
                    _ => lin,
                }
            }
            _ => lin,
        }
    };
        let qa = if a_use_amm { a_fib.as_ref().map(|(q, _)| *q) } else { qa_book };
        let qb = if b_use_amm { b_fib.as_ref().map(|(q, _)| *q) } else { qb_book };
        let dq = dpeek.as_ref().map(|(q, ..)| rate_me(*q));
        let bq = match (qa, qb) {
            (Some((am, ae)), Some((bm, be))) => Some(norm16((am * bm, ae + be))),
            _ => None,
        };
        // The bridged strand's UPPER BOUND — a different quantity from `bq`,
        // and it must not reuse it.
        //
        // rippled: "It is important that `qualityUpperBound` is an upper bound
        // on the quality (it is used to ignore strands whose quality cannot
        // meet a minimum threshold). When calculating quality assume no fee is
        // charged, or the estimate will no longer be an upper bound."
        // `adjustQualityWithFees` (BookStep.cpp) therefore returns `ofrQ`
        // UNCHANGED for a CLOB tip and for a multi-path AMM, and even in the
        // single-path AMM case charges only `trIn` — `trOut` is hardcoded to
        // parity because "AMM doesn't pay the transfer fee on the out amount".
        //
        // `discount()` above is right for CHOOSING book-vs-pool, because at
        // EXECUTION `ownerPaysTransferFee_` really does make a book offer
        // deliver that much less. It is wrong for ADMISSION, where charging it
        // makes the estimate pessimistic and stops being a bound at all.
        //
        // Measured on #105912454 FE592890B233 against rippled's own FLOWDBG:
        //     rippled ub strand 1  63800.63315852392
        //     ours    bq           63862.84779828923   (discount flipped legB
        //                                               to the pool)
        //     ours    bq_ub        63800.63315852392   <- raw book, matches
        // so we read "no candidate clears the limit" where rippled kept a live
        // bridge strand. `bq` prices; `bq_ub` admits.
        let b_use_amm_ub = match (&b_fib, qb_book) {
            (Some((qf, _)), Some(qbk)) => me_cmp(*qf, qbk).is_lt(),
            (Some(_), None) => true,
            _ => false,
        };
        // A leg's ub contribution under the PREVIOUS iteration's multiPath
        // flag. Multi: the fib offer's own quality (as before). SINGLE-path,
        // the anchored-case admission (#106093637 BDB95F8A): with the
        // qualityThreshold override (unb — the taker's limit beats the leg's
        // book) `getAMMOffer(nullopt)` emits maxOffer, whose quality() is the
        // pool's feeless SPOT — `ub strand 1` 64717.04 = tip 1.06832e-6 ×
        // spot 6.05785e10 in the FLOWDBG receipt — and likewise with no book
        // at all; the pool only participates when spot strictly beats the
        // book (`getOffer` bails "higher clob quality"). WITHOUT the
        // override the anchored offer's quality EQUALS the book's, so the
        // strand admits at its BOOK. (`adjustQualityWithFees` would charge
        // trIn on a single-path AMM tip; every specimen's in-asset is
        // rate-free, so that factor is not modeled — noted, not forgotten.)
        let single_ub = |spot: Option<Me>, book: Option<Me>, unb: bool| -> Option<Me> {
            match (spot, book) {
                (Some(s), Some(bk)) if unb && me_cmp(s, bk).is_lt() => Some(s),
                (Some(s), None) => Some(s),
                (_, bk) => bk,
            }
        };
        let qa_ub = if multi_prev { qa } else { single_ub(spot_a, qa_book, a_unb_raw) };
        let qb_ub = if multi_prev {
            if b_use_amm_ub { b_fib.as_ref().map(|(q, _)| *q) } else { qb_book }
        } else {
            single_ub(spot_b, qb_book, b_unb_raw)
        };
        let bq_ub = match (qa_ub, qb_ub) {
            (Some((am, ae)), Some((bm, be))) => Some(norm16((am * bm, ae + be))),
            _ => None,
        };
        // DX_ULP — is the admission decided by the last digit of the composed
        // upper bound?
        //
        // `norm16` TRUNCATES the product to 16 significant digits. On
        // #105912454 that gives …641 where rippled's `ub strand 1` is …642 —
        // exactly one ulp, and `mul_round16_up` reproduces rippled's value.
        // In me-space (in-per-out) SMALLER is better and `within` admits on
        // `q <= thr`, so truncating makes our bound one ulp MORE OPTIMISTIC:
        // the risk is ADMITTING a strand rippled drops, not dropping one.
        // (The 005ad9a commit note stated that backwards.)
        //
        // Whether rippled systematically rounds this composition up is ONE
        // data point, not a rule — so this detector is direction-agnostic: it
        // fires only where the two roundings give DIFFERENT admission
        // verdicts, i.e. where a ledger could actually decide the question.
        // Nothing is changed on inference; the floor is calibrated by d6f7589.
        if std::env::var("DX_ULP").is_ok() {
            if let (Some((am, ae)), Some((bm, be)), Some(t)) = (qa_ub, qb_ub, thr) {
                let trunc = norm16((am * bm, ae + be));
                let up = mul_round16_up((am, ae), (bm, be));
                if me_cmp(trunc, t).is_le() != me_cmp(up, t).is_le() {
                    eprintln!(
                        "DX_ULP ADMISSION-DECIDED-BY-LAST-DIGIT trunc={trunc:?} up={up:?} \
thr={t:?} admits_trunc={} admits_up={}",
                        me_cmp(trunc, t).is_le(),
                        me_cmp(up, t).is_le()
                    );
                } else if trunc.1 == t.1 {
                    // Near-miss telemetry. A bare "no ledger decides it" is
                    // weak — it cannot distinguish "close, we got lucky" from
                    // "nowhere near". Report the distance to the limit in ulps
                    // of the 16-digit mantissa so the closest approach across a
                    // sweep is a real number.
                    // `DX_ULP=<n>` caps the report at n ulps; any other value
                    // reports every one. Calibration: on #105912454 the bound
                    // sits 3.81e12 ulps from the limit — 0.06% away — so a cap
                    // of a few thousand reports NOTHING and reads as a false
                    // all-clear. Pick the cap from that scale, not from the
                    // size of the rounding error being chased.
                    let ulps = (trunc.0 as i128 - t.0 as i128).abs();
                    let cap = std::env::var("DX_ULP")
                        .ok()
                        .and_then(|v| v.parse::<i128>().ok())
                        .unwrap_or(i128::MAX);
                    if ulps <= cap {
                        eprintln!("DX_ULP NEAR ulps={ulps} trunc={trunc:?} thr={t:?}");
                    }
                }
            }
        }
        // The DIRECT strand's ADMISSION quality — a different question from
        // `dq`, and it must not reuse it. `BookStep::tip` reads the raw book
        // through `BookTip`, which applies NO owner filter, so an offer of the
        // TAKER's own still prices the strand for admission; self-offers are
        // dropped later, inside `forEachOffer`'s `limitSelfCrossQuality`. `dq`
        // comes from `live_head`, which skips them — right for pricing what we
        // can actually trade, wrong here.
        //
        // The ladder entry deliberately keeps pricing this strand after
        // `reap_self_offers_at_head` has deleted the offer: removals are
        // collected as `ofrsToRm` and applied only once the whole flow is
        // done, so within it every strand is still judged against the
        // unmodified view. #105930662 turns on this — rippled takes a SECOND
        // fib slice, which it could not if the reap dropped the direct strand
        // and with it multi-path, and the pass instead ends where the bridge's
        // own composition falls outside the limit.
        let d_tip = {
            let book = ld.get(di).map(|(q, _)| rate_me(*q));
            let pool = amm.as_ref().zip(amm_init.as_ref()).and_then(|(a, init)| {
                crate::tx::amm_swap::fib_slice(sandbox, a, *init, amm_iters, pays_leg, gets_leg)
                    .map(|s| crate::tx::amm_swap::slice_rate(s.0, s.1))
            });
            match (book, pool) {
                (Some(b), Some(p)) => Some(if me_cmp(p, b).is_lt() { p } else { b }),
                (Some(b), None) => Some(b),
                (None, Some(p)) => Some(p),
                (None, None) => None,
            }
        };
        if std::env::var("DX_BRIDGE").is_ok() {
            let hk = |p: &Option<(u64, Hash256, serde_json::Value, [u8; 20], Me, Me)>| {
                p.as_ref().map(|(_, k, ..)| hex::encode_upper(&k.0[..6])).unwrap_or_else(|| "-".to_string())
            };
            let raw_a = la.get(ai).map(|(q, _)| rate_me(*q));
            let raw_b = lb.get(bi).map(|(q, _)| rate_me(*q));
            let rk = |l: &[(u64, Hash256)], i: usize| {
                l.get(i).map(|(_, k)| hex::encode_upper(&k.0[..6])).unwrap_or_else(|| "-".to_string())
            };
            eprintln!(
                "DX_HEADS d={} a={} b={} qa_book={qa_book:?} qb_book={qb_book:?} qa={qa:?} qb={qb:?} a_amm={a_use_amm} b_amm={b_use_amm}",
                hk(&dpeek), hk(&apeek), hk(&bpeek)
            );
            eprintln!(
                "DX_RAWHEADS a={} b={} raw_a={raw_a:?} raw_b={raw_b:?} ai={ai} bi={bi} la={} lb={}",
                rk(&la, ai), rk(&lb, bi), la.len(), lb.len()
            );
        }
        // rippled evaluates `activateNext` (drop strands whose upper bound
        // misses limitQuality) and `setMultiPath(active > 1)` BEFORE the
        // iteration's strand passes — so the FIRST pass already runs
        // single-path when the bridge is out. Carrying last round's verdict
        // (`multi_prev`) made round 1 consume a FIB slice where rippled's
        // iteration 0 anchors at the LOB: #106455042 446DCA57's oracle trace
        // reads "FLOWDBG iter 0 activeStrands 1 multiPath false" with bridge
        // ub 1.351643746978908 (== our bq_ub to the ulp) against
        // limitQuality 1.3436715, and ONE anchored slice
        // 197.6261655980808 → 147.15278879855 at exactly the 1.343 tip.
        // `multi_prev` still shapes the UPPER-BOUND composition at the round
        // top — rippled's qualityUpperBound reads the PREVIOUS iteration's
        // ammContext there, the same one-round lag.
        let thr_admit = (threshold_self != 0 && threshold_self != u64::MAX)
            .then(|| rate_me(threshold_self));
        let multi_now = match thr_admit {
            Some(t) => {
                let w = |q: Option<Me>| q.is_some_and(|v| me_cmp(v, t).is_le());
                (w(d_tip) as u8) + (w(bq_ub) as u8) > 1
            }
            // No limitQuality (a payment): rippled enters with multiPath =
            // `strands.size() > 1`, which a bridged payment satisfies.
            None => true,
        };
        // The iteration-ENTRY multiPath — rippled's getOffer sees strands
        // BUILT (>1) for iteration 0 and the PREVIOUS iteration's active
        // count after (#106455293's FFI_GETOFFER: multiPath=1 iters=0 while
        // the same round's activeStrands ends 0).
        let mp_entry = multi_prev;
        multi_prev = multi_now;
        // AMM turn: the direct-pair pool competes with the best BOOK rate.
        // Under MULTI-path it offers FIB slices; a SINGLE-path round anchors
        // instead — rippled re-arbitrates every driver iteration
        // (`activateNext` drops strands whose upper bound misses the limit,
        // then `setMultiPath(activeStrands.size() > 1)`, StrandFlow.h:672-674),
        // and single-path `qualityThreshold` returns nullopt whenever the
        // LIMIT is better than the LOB tip (BookStep.cpp:474-481: "The limit
        // out value generates the maximum AMM offer in this case, which
        // matches the quality threshold"), with the lone strand's `limitOut`
        // solving `outFromAvgQ(limitQuality)` (StrandFlow.h:680-686). For the
        // pool that IS a slice anchored AT THE LIMIT; a LOB tip inside the
        // limit anchors at the tip as usual.
        //
        // #106455041 098271FA (full-ledger replay): CNY→XLM, limit 1.34067,
        // bridge upper bound outside it → single-path from rippled's first
        // iteration. Mainnet's CNY/XLM pool fills 29.9838423467 →
        // 22.3648193416 — realized EXACTLY 1.34067 — while our fib slice
        // (q 1.3406974) was refused every round and the pool never crossed.
        // `multi_prev` carries last round's active-strand verdict, so the
        // anchored arm engages one round after the dust CLOB fill drops the
        // tip past the limit — same fills, same final state.
        if let (Some(a), Some(init)) = (amm, &amm_init) {
            let (rp, rg, used) = if !multi_now && threshold != u64::MAX {
                // Anchor from the LIVE direct head (dpeek skips consumed and
                // self offers) — `ld[di]` lags one fill behind and anchored
                // round 2 at the already-consumed dust offer's rate, which
                // sat BELOW the pool's spot and refused the slice.
                let (anchor_clob, limit_anchor) = match dpeek.as_ref().map(|(q, ..)| *q) {
                    Some(t) if t <= threshold => (Some(t), false),
                    _ => (None, true),
                };
                // ADMISSION precedes limitOut, and the pool leg's admission
                // quality is CONTEXT-DEPENDENT (FFI_GETOFFER receipts,
                // AMMLiquidity.cpp):
                //  - multiPath context (strands built > 1 — rippled sets it
                //    BEFORE iteration 0): getOffer emits the FIB slice and
                //    the ub is `Quality{amounts}` — the slice's AVERAGE.
                //    #106455293 4BC56D5F: fib avg 1.350266180729305 >
                //    limitQuality 1.35 → activeStrands 0 → the offer PLACES
                //    (our limit-anchored consume had filled 2.48 XLM).
                //  - single-path (the driver collapsed to one active
                //    strand): the no-clob/override branch emits maxOffer,
                //    whose constructed quality is `Quality{balances}` — the
                //    RAW SPOT (:150-152), NOT the amounts' average.
                //    #106455041 098271FA iter 1: ub 1.336342189754254 =
                //    spot ≤ 1.34067 → admitted → the limitOut-trimmed fill
                //    realizes the limit exactly.
                // `multi_prev` is our carry of rippled's iteration-entry
                // multiPath (the 4ee4288 calibration).
                let pool_admitted = if limit_anchor {
                    let thr_me = rate_me(threshold);
                    if mp_entry {
                        match crate::tx::amm_swap::fib_slice(
                            sandbox, a, *init, amm_iters, pays_leg, gets_leg,
                        ) {
                            Some((fi, fo)) => {
                                let q = crate::tx::amm_swap::slice_rate(fi, fo);
                                let within_limit = !me_cmp(q, thr_me).is_gt();
                                // getOffer's own gate: a fib slice not
                                // strictly better than the CLOB tip is
                                // withheld (Quality{amounts} < clobQuality).
                                let beats_tip = match dpeek.as_ref().map(|(t, ..)| *t) {
                                    Some(t) => me_cmp(q, rate_me(t)).is_lt(),
                                    None => true,
                                };
                                within_limit && beats_tip
                            }
                            None => false,
                        }
                    } else {
                        crate::tx::amm_swap::spot_upper_bound(sandbox, a, pays_leg, gets_leg)
                            <= threshold
                    }
                } else {
                    true
                };
                if std::env::var("DX_AMM").is_ok() {
                    eprintln!(
                        "DX_AMM site=bridged-single anchor={anchor_clob:?} limit_anchor={limit_anchor} admitted={pool_admitted} mp={mp_entry} iters={amm_iters} thr={threshold:x}"
                    );
                }
                if !pool_admitted {
                    (rem_pays, rem_gets, false)
                } else {
                crate::tx::amm_swap::consume(
                    sandbox, a, taker, beneficiary, None, None, rem_pays, rem_gets, pays_leg, gets_leg,
                    threshold, sell, anchor_clob, None, limit_anchor,
                )
                }
            } else {
                let best_book = match (dq, bq) {
                    (Some(d), Some(b)) => Some(if me_cmp(d, b).is_le() { d } else { b }),
                    (Some(d), None) => Some(d),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                if std::env::var("DX_AMM").is_ok() {
                    eprintln!("DX_AMM site=bridged best_book={best_book:?}");
                }
                crate::tx::amm_swap::consume_fib(
                    sandbox, a, taker, beneficiary, None, None, rem_pays, rem_gets, pays_leg, gets_leg,
                    threshold, sell, *init, amm_iters, best_book, None,
                )
            };
            rem_pays = rp;
            rem_gets = rg;
            if used {
                amm_iters += 1;
                crossed += 1;
                if done(rem_pays, rem_gets) {
                    break;
                }
                continue;
            }
            if done(rem_pays, rem_gets) {
                break;
            }
        }
        // The DIRECT strand is always built in offer crossing — "Always invoke
        // flow() with the default path", OfferCreate.cpp:376 — so its BookStep
        // runs whether or not the bridge turns out to be the better source this
        // iteration, and its `execOffer` clears our own offers off the tip
        // before any crossing decision is taken. The peeks above skip
        // self-offers silently, which is right for PRICING (a removed offer
        // never trades) but leaves them sitting in the ledger.
        //
        // #105922825 E2EB1A413C5E: the taker's own 5767DC43 is the tip at
        // 1898.22 RLUSD/ETH, just inside its own 1898.02 limit; the bridge won
        // the iteration, so we priced past it and deleted nothing. Mainnet
        // removes it and its now-empty book page and crosses no value at all.
        // The direct walk in `cross_engine_to` already does this inline, after
        // its own AMM turn.
        if taker == beneficiary {
            reap_self_offers_at_head(sandbox, &ld, di, taker, threshold_self, stale);
        }
        // Choose on what each candidate REALISES, not on its marginal rate.
        // A source's marginal quality prices that source's own slice; the pass
        // moves a different (usually much smaller) amount and prices better.
        // rippled flows EACH strand's actual pass and keeps the best by the
        // quality it realised (StrandFlow `flow()` per strand + BestStrand).
        //
        // #105912454 FE592890B233: marginal dq 63857.53 beats marginal bq
        // 63862.85, so we took the DIRECT strand, whose fill is worse than the
        // 63856.19 limit and gets rejected — crossing nothing. rippled takes
        // the BRIDGE, realising 63847.50, inside the limit. The bridge's
        // realised rate beats its marginal one because leg B's fib slice is
        // ~26.1M drops while the pass moves only 1M.
        //
        // Both estimates come from the non-mutating peeks, mirroring the sizing
        // the execution branches do, so those branches stay untouched.
        let est_direct = dpeek.as_ref().and_then(|(_, _, _, maker, gives0, wants0)| {
            let funded = available(sandbox, maker, pays_leg);
            let m_gives = if me_cmp(funded, *gives0).is_lt() { funded } else { *gives0 };
            let mut give = if !sell && me_cmp(rem_pays, m_gives).is_lt() { rem_pays } else { m_gives };
            let mut pay = me_muldiv(give, *wants0, *gives0, true);
            if me_cmp(pay, rem_gets).is_gt() {
                pay = rem_gets;
                give = me_muldiv(pay, *gives0, *wants0, false);
            }
            if me_is_zero(give) || me_is_zero(pay) { return None; }
            rate_of_me(pay, give)
        });
        let est_bridge = (|| {
            let (a_cap, a_in_f, a_out_f) = if a_use_amm {
                let (_, (s_in, s_out)) = a_fib.as_ref()?;
                (*s_out, *s_in, *s_out)
            } else {
                let (_, _, _, am, ag, aw) = apeek.as_ref()?;
                let funded = available(sandbox, am, &xrp_leg);
                let cap = if me_cmp(funded, *ag).is_lt() { funded } else { *ag };
                (cap, *aw, *ag)
            };
            let (b_cap, b_in_f, b_out_f) = if b_use_amm {
                let (_, (s_in, s_out)) = b_fib.as_ref()?;
                (*s_in, *s_in, *s_out)
            } else {
                let (_, _, _, _, bg, bw) = bpeek.as_ref()?;
                (*bw, *bw, *bg)
            };
            let mut xrp = if me_cmp(a_cap, b_cap).is_lt() { a_cap } else { b_cap };
            let mut gets_in = me_muldiv(xrp, a_in_f, a_out_f, true);
            if me_cmp(gets_in, rem_gets).is_gt() {
                gets_in = rem_gets;
                xrp = me_muldiv(gets_in, a_out_f, a_in_f, false);
            }
            let mut pays_out = reprice_b(b_use_amm, xrp, me_muldiv(xrp, b_out_f, b_in_f, false), sandbox);
            if !sell && me_cmp(pays_out, rem_pays).is_gt() {
                pays_out = rem_pays;
                xrp = me_muldiv(pays_out, b_in_f, b_out_f, true);
                gets_in = reprice_a(a_use_amm, xrp, me_muldiv(xrp, a_in_f, a_out_f, true), sandbox);
            }
            if me_is_zero(gets_in) || me_is_zero(pays_out) { return None; }
            rate_of_me(gets_in, pays_out)
        })();
        // ⛔ DX_SEL REMOVED 2026-08-07 — it had served its purpose and become
        // a trap. It compared rippled's upper-bound model against our
        // then-current estimate-based one; since `03c2cb9` the engine SELECTS
        // by that upper-bound model (see `order` below), so the detector was
        // comparing an abandoned model against the live one and reporting
        // "mismatches" that described nothing. `DX_CAND` (under `DX_BRIDGE`)
        // covers the useful ground: each candidate's realised fill and the
        // judge's verdict.
        //
        // What it established, still true and worth keeping:
        //
        // rippled does NOT keep the best strand by realised quality — the
        // `BestStrand` type name says so and is misleading; it holds the
        // strand that was PICKED, not the winner of a contest.
        // `ActiveStrands::activateNext` sorts candidates by
        // `qualityUpperBound`, best first, and DROPS any whose bound misses
        // limitQuality; the loop walks that order and on `Path rejected by
        // limitQuality` does `continue` — falling through to the NEXT
        // candidate — taking the FIRST survivor (StrandFlow.h:647-722).
        //
        // Measured against rippled's own FLOWDBG bounds on #105912454:
        //
        //     rippled  ub strand 0 (direct) 63857.53          limit 63856.194
        //     rippled  ub strand 1 (bridge) 63800.63315852392
        //     ours     d_tip                63857.53            <- EXACT match
        //     ours     bq                   63862.84779828923   <- WRONG quantity
        //
        // `d_tip` is faithful to `qualityUpperBound`; `bq` was not, because it
        // carried the output-transfer discount. Hence `bq_ub` (`005ad9a`):
        // `bq` prices, `bq_ub` admits.
        // Candidate set and ORDER, per `ActiveStrands::activateNext`: sort by
        // `qualityUpperBound`, BEST FIRST, and drop any strand whose upper
        // bound misses `limitQuality`. Not by estimated realised quality —
        // rippled never ranks strands on what they realise, it ranks on the
        // bound and then lets the pass prove itself (see the loop below).
        let ub_ok = |q: Option<Me>| q.filter(|v| thr.is_none_or(|t| me_cmp(*v, t).is_le()));
        let order: &[bool] = match (ub_ok(d_tip), ub_ok(bq_ub)) {
            (Some(d), Some(b)) => {
                if me_cmp(d, b).is_le() {
                    &[true, false]
                } else {
                    &[false, true]
                }
            }
            (Some(_), None) => &[true],
            (None, Some(_)) => &[false],
            // Every candidate's upper bound misses the limit: rippled drops
            // them all and the flow ends — "All strands dry".
            (None, None) => break,
        };
        if std::env::var("DX_BRIDGE").is_ok() {
            eprintln!("DX_EST direct={est_direct:?} bridge={est_bridge:?} thr={threshold} order={order:?}");
            eprintln!("DX_BRIDGE dq={dq:?} bq={bq:?} bq_ub={bq_ub:?} thr={thr:?} order={order:?} di={di} ai={ai} bi={bi} ld={} la={} lb={}", ld.len(), la.len(), lb.len());
        }
        // No marginal bail: the realised estimates above already gated this
        // iteration, and the judge below checks what it actually moved.
        // A bridged pass cannot stop at the pool. Off the default path
        // `BookOfferCrossingStep::checkQualityThreshold` (BookStep.cpp:~470,
        // `!defaultPath_ || quality >= qualityThreshold_`) is disabled, so
        // after consuming the AMM offer the step keeps stepping into that
        // leg's CLOB, and StrandFlow judges the pass AS A WHOLE — rejecting
        // and DISCARDING it entirely when the average quality misses
        // limitQuality (StrandFlow.h:717-722). No `limitOut` trim rescues it:
        // that needs a single active strand (:657) and offer crossing always
        // has two. A fib slice is 0.025% of the pool, so any request
        // materially larger than one slice is dominated by that CLOB
        // continuation — the bridge is usable only when its BOOK composition
        // also clears the limit.
        //
        // #105807256 84FD7DC8 sells 3105.662739 BBRL for RLUSD. Leg A's pool
        // (20.898474 XRP / 116.0000692564219 BBRL, fee 1%) quotes 5.60671
        // BBRL/XRP while leg A's BOOK starts at 399.996 — 71x worse. We took
        // six pool slices and crossed 8 objects; mainnet rested all 4 nodes.
        //
        // ⚠ The book-only composition this gate used to test was a MODEL of
        // that pass. Traced 2026-08-04 on #105930662 40FB322EC16C, the model's
        // premise is false: rippled never steps into leg A's CLOB there, and
        // the real rule is narrower and observable.
        //
        // A bridged leg's POOL may price the strand only while MORE THAN ONE
        // strand is still a candidate. `AMMContext::multiPath()` is
        // `activeStrands.size() > 1` (StrandFlow.h:649), re-evaluated every
        // iteration, and it selects which offer `AMMLiquidity::getOffer` hands
        // back (AMMLiquidity.cpp):
        //   multi  — `generateFibSeqOffer`, priced at the POOL's own quality,
        //            so `BookStep::tip` returns the pool whenever it beats the
        //            LOB tip;
        //   single — `changeSpotPriceQuality` matched to the LOB tip, whose
        //            quality EQUALS the LOB's, so `tip`'s
        //            `ammOffer->quality() > lobQuality` is false and the
        //            strand falls back to its BOOK.
        //
        // The second single-path branch, which DOES lift the strand — but at
        // the pool's SPOT quality, not at any amount it would actually trade.
        // `BookOfferCrossingStep::qualityThreshold` (BookStep.cpp:475-480)
        // returns lobQuality only while `qualityThreshold_ <= lobQuality`; when
        // the taker's limit BEATS the leg's book it returns nullopt, and
        // `getAMMOffer(view, nullopt)` yields `maxOffer` — whose AMMOffer is
        // built `AMMOffer(*this, amounts, balances, Quality{balances})`
        // (AMMLiquidity.cpp maxOffer). **Its quality() is the pool's SPOT
        // quality**, while its AMOUNTS drain ~99% of the pool's output side.
        // `tip` compares on quality(), so the pool wins whenever spot beats the
        // book, and being an Amm tip its transfer fee is WAIVED where a Clob
        // tip pays the output issuer's rate (BookStep::qualityUpperBound).
        //
        // ⚠ An AMM offer's COMPARISON quality and its EXECUTION rate are
        // different quantities and this gate wants the first. Measured on
        // #105940336 leg B (pool rQBeAgh, 103807148357 drops /
        // 1.745915504076971 BTC):
        //     spot / quality()   1.681884e-11 BTC/drop   <- what `tip` compares
        //     fib slice          1.681144e-11   (0.025% of the pool)
        //     maxOffer AVERAGE   1.681564e-13   (99% of pool BTC out, 100x)
        // Reading that 100x average as the comparison quality is what made this
        // ledger look unexplainable; the figure itself has been misread by 100x
        // three separate times. Compute, never eyeball.
        //
        // The pool contributes nothing when spot is no better than the book:
        // `getOffer` bails "higher clob quality" on
        // `spotPriceQ <= clobQuality` BEFORE either branch. That is why
        // #105807256 84FD7DC8 stays rejected — its leg B pool is worse than its
        // book, so the strand reads book-only either way.
        //
        // A strand whose `qualityUpperBound` misses limitQuality is dropped
        // from the candidate set — silently, `continue` with no log line, in
        // both `activateNext` and the strand loop (StrandFlow.h:682-690).
        //
        // #105930662 40FB322EC16C is the case that pins this. The taker's OWN
        // offer 991CFC15 is the direct book's tip at 5.1422 BBRL/RLUSD, just
        // inside its own 5.14324 limit, so the direct strand stays a candidate,
        // two strands stay active, and leg A's pool — 72x better than its book
        // — prices the bridge as a fib slice. rippled crosses two slices
        // (0.05836943136803854 BBRL for 0.01135940238 RLUSD) and we crossed
        // nothing: 6 mutations against 13, all 7 missing ones Modified.
        //
        // #105807256 84FD7DC8 is the mirror and stays rejected: its direct book
        // tip 5.11260 misses the 5.11221 limit, so the direct strand is dropped
        // at admission, one strand remains, leg A's pool is throttled to its
        // book — 71x worse — and the bridge is dropped too. Mainnet moves no
        // value and rests all 4 nodes. Its trace is the proof: no `New flow
        // iter` line at all, straight to `All strands dry`.
        // rippled evaluates `setMultiPath(activeStrands.size() > 1)` AFTER
        // `activateNext` filtered the candidates, and `AMMLiquidity::getOffer`
        // picks the slice SHAPE from it.
        // Admission compares fee-free upper bounds against rippled's TRUE
        // limitQuality — the transfer-rate-INFLATED `threshold_self`, not the
        // net `threshold` the realised-side gates use (there trIn's grossing
        // cancels the inflation; at admission nothing grosses, so the
        // inflated limit is what admits fee-bearing strands). 4A13D048
        // (l105887283, BTC.Bitstamp 1.0015): direct-tip 1.5271e-5 admits
        // against 1.5288e-5 but fails the net 1.5265e-5 — rippled's receipt
        // is `activeStrands 2 multiPath true`, AMM anchored, CLOB fills.
        // (multi_now is computed ABOVE the AMM turn now — rippled evaluates
        // activateNext + setMultiPath BEFORE the strand passes of the same
        // iteration, StrandFlow.h:672-674.)
        // The qualityThreshold override is single-path only: force the pool
        // leg now that multiPath is known (see a_unb_raw above).
        let a_use_amm = a_use_amm || (!multi_now && a_unb_raw);
        let b_use_amm = b_use_amm || (!multi_now && b_unb_raw);
        // ⚠ Under multiPath, a clamped POOL fill is priced at the OFFER'S
        // QUALITY, not re-swapped through the conservation function:
        //     if (ammLiquidity_.multiPath())
        //         return quality().ceilOutStrict(offerAmount, limit, roundUp);
        //     return {swapAssetOut(balances_, limit, tradingFee()), limit};
        // (AMMOffer::limitOut, AMMOffer.cpp; `limitIn` branches the same way).
        // The comment on `unreprice_a` below already said the conservation
        // function runs "for a SINGLE-PATH pool" — the gate was just never
        // applied, so we repriced through the pool on every path.
        //
        // #105802230 EF1642A9 is the specimen: tfIoC, 1000 XAH for 3 RLUSD,
        // autobridged with leg A on a pool and TWO active strands, so
        // `multiPath` is true. rippled's leg-A offer is
        // 1092.406235156812 XAH / 13576137 XRP, and taking 2720349 drops at
        // that offer's own quality is
        //     2720349 * 1092.406235156812 / 13576137 = 218.8933574699931
        // to the digit — its `Total flow: in`. Re-swapping the same 2720349
        // through the pool gives 218.8500354154254, which is 0.0433220545677
        // cheaper and is what we charged.
        let rp_a = |xrp: Me, lin: Me, sandbox: &Sandbox| -> Me {
            if multi_now { lin } else { reprice_a(a_use_amm, xrp, lin, sandbox) }
        };
        let rp_b = |xrp: Me, lin: Me, sandbox: &Sandbox| -> Me {
            if multi_now { lin } else { reprice_b(b_use_amm, xrp, lin, sandbox) }
        };
        let urp_a = |gets_in: Me, lin: Me, sandbox: &Sandbox| -> Me {
            if multi_now { lin } else { unreprice_a(a_use_amm, gets_in, lin, sandbox) }
        };
        let urp_b = |pays_out: Me, lin: Me, sandbox: &Sandbox| -> Me {
            if multi_now { lin } else { unreprice_b(b_use_amm, pays_out, lin, sandbox) }
        };
        // SINGLE PATH takes `maxOffer`, not a fib slice. The UPPER BOUND above
        // keeps the fib slice deliberately — `activateNext` runs BEFORE
        // `setMultiPath`, and one tx traces both shapes: `created 5458/XRP` for
        // the bound, `created 2183017500/XRP` for the pass.
        // Single-path leg fills follow rippled's tryAMM decision tree
        // (BookStep.cpp:461-481): threshold better than the leg's tip ⇒
        // `getAMMOffer(nullopt)` = unbounded maxOffer, the leg's ONLY
        // liquidity (a_unb_raw — the C966717C case); a live tip WITHOUT the
        // override ⇒ the offer is ANCHORED at the tip, and once consumed the
        // next round's strictly-better head test fails and the walk CONTINUES
        // into that leg's CLOB (#106093637 BDB95F8A: mainnet takes the pool
        // slice then the rURtT5MM clip; maxOffer here fed the whole fill
        // through the pool and rested what mainnet crossed); no tip at all ⇒
        // maxOffer.
        let slice_of = |amm: &Option<crate::tx::amm_swap::Amm>, out_leg: &Leg, in_leg: &Leg,
                        tip: Option<u64>, unb: bool, sandbox: &Sandbox|
         -> Option<(Me, (Me, Me))> {
            let am = amm.as_ref()?;
            let s = match (tip, unb) {
                (Some(qb), false) => {
                    crate::tx::amm_swap::anchored_slice(sandbox, am, out_leg, in_leg, qb)?
                }
                _ => crate::tx::amm_swap::max_offer(sandbox, am, out_leg, in_leg)?,
            };
            Some((crate::tx::amm_swap::slice_rate(s.0, s.1), s))
        };
        let a_fill = if multi_now {
            a_fib
        } else {
            slice_of(&amm_a, &xrp_leg, gets_leg, apeek.as_ref().map(|(q, ..)| *q), a_unb_raw, sandbox)
        };
        let b_fill = if multi_now {
            b_fib
        } else {
            slice_of(&amm_b, pays_leg, &xrp_leg, bpeek.as_ref().map(|(q, ..)| *q), b_unb_raw, sandbox)
        };
        // `limitOut` — SIZE THE PASS TO THE LIMIT instead of taking the
        // maximum and then discarding it (StrandFlow.h:357; call site at
        // :655, "Limit only if one strand and limitQuality").
        //
        // A CLOB step's average quality is constant; an AMM's degrades
        // linearly in the output taken. Composing the strand's
        // `QualityFunction` and solving `outFromAvgQ(limitQuality)` gives the
        // output whose AVERAGE quality lands exactly on the taker's limit.
        // Without it `maxOffer` has no cap and we spend nearly the whole
        // request: on #106134431 our 3rd pass was 1905.22 RLUSD against
        // rippled's 20.53, so its realised quality missed and the judge threw
        // the pass away — we rested one offer short of mainnet.
        //
        // Only for a SINGLE active strand: with two, sizing one to the limit
        // would misstate the other's share.
        let strand_qf = |sandbox: &Sandbox| -> Option<crate::tx::amm_swap::QualityFn> {
            use crate::tx::amm_swap::{pool_balances, QualityFn};
            let leg = |use_amm: bool, amm: &Option<crate::tx::amm_swap::Amm>,
                       book: Option<Me>, out_leg: &Leg, in_leg: &Leg| -> Option<QualityFn> {
                if use_amm {
                    let am = amm.as_ref()?;
                    let (pin, pout) = pool_balances(sandbox, am, out_leg, in_leg);
                    QualityFn::amm(pin, pout, am.tfee)
                } else {
                    QualityFn::clob(book?)
                }
            };
            // Strand order is source-first: leg A (gets -> XRP), then leg B.
            let mut qf = leg(a_use_amm, &amm_a, qa_book, &xrp_leg, gets_leg)?;
            qf.combine(&leg(b_use_amm, &amm_b, qb_book, pays_leg, &xrp_leg)?);
            Some(qf)
        };
        // `remainingOut` for this pass: a tfSell offer is not bounded by the
        // pays side (rippled sets `deliver` to the max amount); everything
        // else is.
        let rem_out = (!sell).then_some(rem_pays);
        let limited = if multi_now {
            None
        } else {
            thr.and_then(|t| strand_qf(sandbox).and_then(|q| q.out_from_avg_q(t)))
        };
        // `std::min(out, remainingOut)`, plus "a tiny difference could be due
        // to the round off": within 1e-9 relative rippled keeps `remainingOut`
        // so `adjustedRemOut` stays FALSE.
        let (out_cap, adjusted) = match (limited, rem_out) {
            (Some(l), Some(r)) => {
                if me_cmp(l, r).is_ge()
                    || me_is_zero(me_muldiv(me_sub(r, l), (1_000_000_000, 0), r, false))
                {
                    (Some(r), false)
                } else {
                    (Some(l), true)
                }
            }
            (Some(l), None) => (Some(l), true),
            (None, r) => (r, false),
        };
        if std::env::var("DX_BRIDGE").is_ok() {
            eprintln!("DX_LIMITOUT limited={limited:?} rem_out={rem_out:?} cap={out_cap:?} adjusted={adjusted}");
        }
        // Walk the candidates in UPPER-BOUND order, taking the FIRST whose
        // realised fill survives the judge. rippled rejects a pass with
        // `continue` — which advances to the NEXT strand, not out of the loop
        // (StrandFlow.h:647-722) — so the judge and the fall-through are ONE
        // mechanism. Building the judge alone is what failed twice before: it
        // rejects the leading candidate and then crosses nothing.
        let mut filled = false;
        for &want_direct in order {
            let snap = sandbox.snapshot();
            let (rp0, rg0, cr0, di0, ai0, bi0, it0) =
                (rem_pays, rem_gets, crossed, di, ai, bi, amm_iters);
            let st0 = stale.len();
            amm_used = false;
            // (in, out) of this candidate's fill, in the same orientation as
            // `est_direct`/`est_bridge`: gets-side in, pays-side out.
            let mut fee_judged = false;
            let mut fill: Option<(Me, Me)> = None;
            'attempt: {
            if !want_direct {
                if let Some(t) = thr {
                    let within = |q: Option<Me>| q.is_some_and(|v| me_cmp(v, t).is_le());
                    // `activateNext` runs BEFORE `setMultiPath`, so it filters
                    // against the PREVIOUS iteration's flag — true on entry, from
                    // `Flow.cpp:106`'s `strands.size() > 1`. Both candidates are
                    // therefore judged with the pool in play, and the count that
                    // survives is what `setMultiPath` then sees.
                    let multi = multi_now;
                let _ = &within;
                    // Single path: the pool is matched to the book, so the book's
                    // composition IS the strand's quality. A leg with no book at
                    // all then composes nothing — off the default path the step
                    // keeps going into that leg's CLOB and StrandFlow judges the
                    // pass as a whole, and with no CLOB behind the pool there is
                    // nothing for it to clear.
                    //
                    // All 8 protected cases are Flags=65536 exactly (tfPassive),
                    // buy side, IOU<->IOU with no XRP leg, so every one is bridged.
                    // #105813899 44E799C6FF9B: lb=0, and its direct tip 1.895174
                    // misses thr 1.890193, so it is single-path and breaks here.
                    // Single path: each leg's tip is its BOOK, unless the taker's
                    // limit beats that book — then `qualityThreshold` is nullopt,
                    // `maxOffer` is generated, and its quality() is the pool SPOT.
                    // rippled compares the tx-level limit against a LEG's quality
                    // directly (`qualityThreshold_ > lobQuality`), which is
                    // dimensionally odd but is what the code does; in me-space
                    // (in-per-out, smaller is better) that is `thr < leg_book`.
                    //
                    // ⚠ KNOWN GAP (#106093637 BDB95F8A, parked): in the ANCHORED
                    // case rippled's admission also prices a pool-better leg at
                    // the feeless spot (`ub strand 1` 64717.04 = tip 1.06832e-6 ×
                    // spot 6.05785e10 — admitted, fills 12.03 through the
                    // rURtT5MM clip). Loosening ONLY the admission re-broke
                    // #105807256 84FD7DC8's protection: rippled then EXECUTES the
                    // pool capped at the ANCHORED SLICE and continues into the
                    // leg's CLOB, judging the pass whole — our leg execution is
                    // pool-only and unbounded, so admitting fills what mainnet
                    // rests. Fixing it needs the anchored-slice cap + CLOB
                    // continuation per leg — one build, both sites together.
                    let single_tip = |book: Option<Me>, spot: Option<Me>| -> Option<Me> {
                        let b = book?;
                        match spot {
                            // `spotPriceQ <= clobQuality` bails "higher clob
                            // quality" first, so the pool must be STRICTLY better.
                            Some(s) if me_cmp(t, b).is_lt() && me_cmp(s, b).is_lt() => Some(s),
                            _ => Some(b),
                        }
                    };
                    let admit = if multi {
                        bq
                    } else {
                        match (single_tip(qa_book, spot_a), single_tip(qb_book, spot_b)) {
                            (Some(a), Some(b)) => Some(norm16((a.0 * b.0, a.1 + b.1))),
                            _ => None,
                        }
                    };
                    if std::env::var("DX_BRIDGE").is_ok() {
                        eprintln!("DX_ADMIT d_tip={d_tip:?} multi={multi} admit={admit:?} thr={t:?}");
                    }
                    if !within(admit) {
                        break 'attempt;
                    }
                }
            }
            if want_direct {
                let Some((q, okey, offer, maker, gives0, wants0)) =
                    live_head(sandbox, &ld, &mut di, taker, pays_leg, gets_leg, true, true, stale)
                else { break 'attempt };
                let funded = available(sandbox, &maker, pays_leg);
                let m_gives = if me_cmp(funded, gives0).is_lt() { funded } else { gives0 };
                // PRICE A PARTIAL FILL AT THE FILED RATE — the rule `e2a9c99`
                // gave the two bridge legs, which this third fill site inside
                // the same function never got. An offer carries two rates: the
                // one it is FILED under (low 64 bits of `BookDirectory`, a
                // 16-digit mantissa fixed at creation, which `live_head` hands
                // back as element 0 and this site used to discard) and its
                // current `TakerPays / TakerGets`, which drifts every time a
                // residual is re-rounded. rippled prices a PARTIAL fill at the
                // filed rate and a FULL one at the amounts.
                //
                // #105796380 A449F15D, a tfIoC selling RLUSD for BBRL against
                // maker offer 1C91F38A (1050.174129632103 BBRL for
                // 205.7501154775216 RLUSD, filed at 0.19592). The taker's line
                // runs out at 125.383911425968 RLUSD, so the fill is
                // INPUT-limited and the output is re-derived:
                //   filed  125.383911425968 / 0.19592            = 639.9750481113107  <- mainnet
                //   own    x 1050.174129632103/205.7501154775216 = 639.9750481113108  <- ours
                // one last place, reported twice — on the maker's BBRL line and
                // on the TakerGets it leaves behind.
                //
                // `whole` is None when the maker cannot fund the offer outright,
                // because a funding-limited head is a partial fill however much
                // of it trades.
                let whole = me_cmp(funded, gives0).is_ge().then_some(gives0);
                let price = |g: Me| -> Me {
                    if whole.is_none_or(|w| me_cmp(g, w).is_lt()) {
                        mul_round16_up(g, rate_me(q))
                    } else {
                        me_muldiv(g, wants0, gives0, true)
                    }
                };
                let mut give = if !sell && me_cmp(rem_pays, m_gives).is_lt() { rem_pays } else { m_gives };
                let mut pay = price(give);
                let mut in_exhausted = false;
                if me_cmp(pay, rem_gets).is_gt() {
                    pay = rem_gets;
                    // Gross-primary on the exhausting fill (F50/F51): the
                    // remaining gross budget verbatim, its division as net.
                    if let Some(cap) = gets_gross_cap {
                        let verb = me_sub(cap, in_gross_spent);
                        if !me_is_zero(verb) {
                            pay = match fee_rate {
                                None => verb,
                                Some(r) => mul_ratio(verb, 1_000_000_000, r as u128, false),
                            };
                            in_exhausted = true;
                        }
                    }
                    // The clamp IS the partial case, so this always prices off
                    // the page — `a_unprice` takes the filed rate unconditionally
                    // for the same reason.
                    give = me_muldiv(pay, (1u128, 0i32), rate_me(q), false);
                    if me_is_zero(give) {
                        break 'attempt;
                    }
                }
                // Same input transfer fee as leg A — this is the DIRECT head
                // competing inside the bridge, and it spends the taker's gets
                // side too. #105954798 D5887FD7 needs both: it takes one
                // bridged slice and the rest through here, so charging only
                // leg A moved it 4.25e-5 -> 4.18e-5 and no further.
                //
                // Charged as ONE debit of the gross, not a net debit plus a fee
                // adjustment — `move_leg_gross` has the arithmetic.
                let d_gross = match (in_exhausted, gets_gross_cap) {
                    (true, Some(cap)) => me_sub(cap, in_gross_spent),
                    _ => gross_in(fee_rate, pay),
                };
                in_gross_spent = stamount_signed_add(false, in_gross_spent, false, d_gross).1;
                settle_fill(sandbox, &okey, &offer, &maker, taker, beneficiary,
                            pays_leg, gets_leg, give, pay, d_gross, gives0, wants0);
                // The taker's RUNNING remainder is an STAmount in rippled, so it
                // re-rounds to 16 digits at EVERY subtraction. `me_sub` is
                // exact, so ours accumulates precision rippled never has and
                // then loses it all at once, truncated, in the final fill.
                //
                // #105923760 3C6A5F07 is the specimen and it replays exactly.
                // A tfPassive 185722760 drops for 199.7244 RLUSD, nine makers;
                // the sixth fill is 0.499999223256619, which pushes the
                // remainder to EIGHTEEN digits:
                //   199.7244 -7.430635 -16.884494 -1.016862 -0.500628 -2.041095
                //     -0.499999223256619 -> 171.350686776743381
                //                        -> STAmount 171.3506867767434
                //     -1.051875          -> 170.2988117767434
                //     -1.489338          -> 168.8094737767434   <- mainnet
                // Ours held 168.809473776743381 and `norm16` TRUNCATED it to
                // 168.8094737767433, leaving the maker's TakerGets one high.
                //
                // Same defect as `offer_residual` (4556384), one level up: that
                // fixed what we STORE, this fixes what we CARRY.
                if std::env::var("DX_FILL").is_ok() {
                    eprintln!("DX_FILL give={give:?} pay={pay:?} rem_pays={rem_pays:?} rem_gets={rem_gets:?}");
                }
                rem_pays = me_sub(rem_pays, give);
                rem_gets = me_sub(rem_gets, pay);
                if in_exhausted {
                    rem_gets = (0, 0);
                }
                crossed += 1;
                fill = Some((pay, give));
            } else {
                // Resolve each leg's source: book maker offer or that pair's
                // pool fib slice. A-side capacity/rate in (XRP-out, gets-in),
                // B-side in (pays-out, XRP-in).
                let a_book = if a_use_amm {
                    None
                } else {
                    match live_head(sandbox, &la, &mut ai, taker, &xrp_leg, gets_leg, true, true, stale) {
                        Some(h) => Some(h),
                        None => break 'attempt,
                    }
                };
                let b_book = if b_use_amm {
                    None
                } else {
                    match live_head(sandbox, &lb, &mut bi, taker, pays_leg, &xrp_leg, true, true, stale) {
                        Some(h) => Some(h),
                        None => break 'attempt,
                    }
                };
                // (xrp capacity, gets per that xrp) for leg A.
                //
                // `a_qbook` / `b_qbook` are the offers' filed BookDirectory
                // rates, carried alongside the amounts so a PARTIAL fill can be
                // priced the way rippled prices it — see `a_price` below.
                let (a_cap_xrp, a_in_full, a_out_full, a_qbook) = match (&a_book, &a_fill) {
                    (Some((q, _, _, amaker, a_gives0, a_wants0)), _) => {
                        let funded = available(sandbox, amaker, &xrp_leg);
                        let a_gives = if me_cmp(funded, *a_gives0).is_lt() { funded } else { *a_gives0 };
                        // Whole-offer only when the maker can actually fund it;
                        // a funding-limited head is a partial fill.
                        let whole = me_cmp(funded, *a_gives0).is_ge().then_some(*a_gives0);
                        (a_gives, *a_wants0, *a_gives0, Some((*q, whole)))
                    }
                    (None, Some((_, (s_in, s_out)))) => (*s_out, *s_in, *s_out, None),
                    (None, None) => break 'attempt,
                };
                let (b_cap_xrp, b_in_full, b_out_full, b_qbook) = match (&b_book, &b_fill) {
                    (Some((q, _, _, _, b_gives0, b_wants0)), _) => {
                        (*b_wants0, *b_wants0, *b_gives0, Some((*q, Some(*b_wants0))))
                    }
                    (None, Some((_, (s_in, s_out)))) => (*s_in, *s_in, *s_out, None),
                    (None, None) => break 'attempt,
                };
                // rippled prices a PARTIAL fill at the offer's filed
                // BookDirectory quality, NOT at its current TakerPays/TakerGets
                // ratio — the two drift apart once an offer has been partially
                // consumed and its residual re-rounded. The DIRECT walk has
                // done this since `full_offer` above; the bridge did not.
                //
                // #105770848 F1880BA0 is the specimen, and it is exact. A tfSell
                // OfferCreate bridging XAH -> XRP -> RLUSD; leg A's maker offer
                // 67747F4E is partially consumed, so its own ratio has drifted
                // from the rate it is filed under:
                //   book rate 7.401924500370096e-5 x 881703 = 65.26299037749815  <- mainnet
                //   own ratio 7.40192434033347e-5  x 881703 = 65.26298896645042  <- ours
                // 1.411048e-6 XAH, which DX_VALCHECK reports as a conserved pair
                // on the two XAH trust lines plus the same figure left on the
                // maker's TakerPays.
                //
                // The book rate is only a 16-digit mantissa, so it cannot be
                // recovered from the amounts — it has to be read off the page.
                let a_price = |xrp: Me| -> Me {
                    match a_qbook {
                        Some((q, whole)) if whole.is_none_or(|w| me_cmp(xrp, w).is_lt()) => {
                            mul_round16_up(xrp, rate_me(q))
                        }
                        _ => me_muldiv(xrp, a_in_full, a_out_full, true),
                    }
                };
                let a_unprice = |gets: Me| -> Me {
                    match a_qbook {
                        Some((q, _)) => me_muldiv(gets, (1u128, 0i32), rate_me(q), false),
                        None => me_muldiv(gets, a_out_full, a_in_full, false),
                    }
                };
                let b_price = |xrp: Me| -> Me {
                    match b_qbook {
                        Some((q, whole)) if whole.is_none_or(|w| me_cmp(xrp, w).is_lt()) => {
                            me_muldiv(xrp, (1u128, 0i32), rate_me(q), false)
                        }
                        _ => me_muldiv(xrp, b_out_full, b_in_full, false),
                    }
                };
                let b_unprice = |pays: Me| -> Me {
                    match b_qbook {
                        Some((q, _)) => mul_round16_up(pays, rate_me(q)),
                        None => me_muldiv(pays, b_in_full, b_out_full, true),
                    }
                };
                let mut xrp = if me_cmp(a_cap_xrp, b_cap_xrp).is_lt() { a_cap_xrp } else { b_cap_xrp };
                // `AMMOffer::limitOut` — for a SINGLE-PATH pool the clamped
                // fill is re-priced through the conservation function,
                // `{swapAssetOut(balances_, limit, fee), limit}`, NOT scaled by
                // the offer's average quality ("The offer quality is increased
                // in this case, but it doesn't matter since there is only one
                // path", AMMOffer.cpp:82-101). `reprice_a` IS that swap.
                //
                // Scaling linearly is harmless for a fib slice, whose average
                // is near spot, and catastrophic for `maxOffer`, whose average
                // is ~100x worse: it inflates `gets_in` until the pass misses
                // the limit and the judge discards it. That is exactly what
                // regressed #105940336 the first time maxOffer was wired in.
                let mut gets_in = rp_a(xrp, a_price(xrp), sandbox);
                // Set when this slice exhausts the gross in-budget: the in is
                // then GROSS-PRIMARY — the remainder verbatim on the debit,
                // its division on the net — exactly rippled's in-limited
                // iteration (F51, #106688646 5524F0F2: gross 4.070530048487853e-6
                // minus iter0's 3.146629230966003e-6 leaves 9.2390081752185e-7,
                // whose net is …911 where our net chain carried …912; the pool
                // then pays 1642 drops, not 1643).
                let mut in_exhausted = false;
                if me_cmp(gets_in, rem_gets).is_gt() {
                    gets_in = rem_gets;
                    if let Some(cap) = gets_gross_cap {
                        let verb = me_sub(cap, in_gross_spent);
                        if !me_is_zero(verb) {
                            gets_in = match fee_rate {
                                None => verb,
                                Some(r) => mul_ratio(verb, 1_000_000_000, r as u128, false),
                            };
                            in_exhausted = true;
                        }
                    }
                    xrp = urp_a(gets_in, a_unprice(gets_in), sandbox);
                }
                let mut pays_out = rp_b(xrp, b_price(xrp), sandbox);
                // Clamp on the limitOut-adjusted cap. For a tfSell offer
                // `rem_pays` is not a bound, but the limit-sized cap is.
                let mut out_clamped = false;
                if let Some(cap) = out_cap {
                    if me_cmp(pays_out, cap).is_gt() {
                        pays_out = cap;
                        xrp = urp_b(pays_out, b_unprice(pays_out), sandbox);
                        gets_in = rp_a(xrp, a_price(xrp), sandbox);
                        out_clamped = true;
                    }
                }
                // A leg-B book maker can only deliver what it HOLDS. Leg A has
                // clamped its maker to `available()` since the bridge was
                // built (`a_gives` above); leg B took the offer's whole
                // TakerPays as capacity and never consulted the maker at all —
                // `live_head` tests funding only for ZERO, so an underfunded
                // maker passed straight through at full size.
                //
                // #106146362 75511674AD58: `rwnJpjMn18m7xd` holds
                // 0.64751384169623 RLUSD and rests TWO offers against it —
                // F5B677B8C8 for 9.328507 and 66B29DEB for 12.735909. We drove
                // the whole 1 RLUSD through F5B677B8C8 and wrote its owner's
                // trust line to **-0.35248615830377**: a non-issuer minting
                // 0.35 RLUSD it never had. rippled's iteration 0 delivers
                // exactly 0.64751384169623 — the balance to the last digit.
                //
                // Clamping here rather than in `b_cap_xrp` above lands the fill
                // ON the funded amount instead of on whatever the XRP-side
                // conversion rounds to, and it reuses the same re-derivation
                // the `out_cap` clamp does. The loop then re-enters, `live_head`
                // finds the maker at zero and reaps BOTH offers — which is
                // rippled's `Removing became unfunded offer 66B29DEB` — and
                // walks on to the next maker's offer for the remainder.
                if let Some((_, _, _, bmaker, _, _)) = &b_book {
                    let funded = available(sandbox, bmaker, pays_leg);
                    // What the leg-B head can ACTUALLY deliver at this moment.
                    // The walk's DEPTH turns on this: a head that is
                    // funding-limited forces the step onward to the next offer,
                    // and mid-ledger balances are not the pre-state balances.
                    if std::env::var("DX_WALK").is_ok() {
                        eprintln!("DX_BFUND maker={} funded={funded:?} want={pays_out:?} clamped={}",
                            hex::encode(bmaker), me_cmp(pays_out, funded).is_gt());
                    }
                    if me_cmp(pays_out, funded).is_gt() {
                        pays_out = funded;
                        xrp = urp_b(pays_out, b_unprice(pays_out), sandbox);
                        gets_in = rp_a(xrp, a_price(xrp), sandbox);
                        out_clamped = true;
                    }
                }
                // DX_XRP: the bridged mid-leg is XRP and has to land on whole
                // drops. Every value-divergent BRIDGED crossing found so far
                // carries the same ±1-drop footprint, so measure whether this
                // truncation is the one throwing it away — before touching it.
                if std::env::var("DX_XRP").is_ok() {
                    let trunc = (me_rescale(xrp, 0, false), 0);
                    let up = (me_rescale(xrp, 0, true), 0);
                    eprintln!(
                        "DX_XRP xrp_exact={xrp:?} trunc={trunc:?} ceil={up:?} fractional={}",
                        me_cmp(trunc, up).is_lt(),
                    );
                }
                // Whole drops for the mid-leg. The ceil is the calibrated
                // default — a BOOK maker charges for the whole drop
                // (#105663160 below), and an OUT-limited pass sizes by
                // `ceil_out`, which rounds the in side UP for pool legs too
                // (#106674447 2049BE47 buys exactly 1.0 RLUSD; flooring its
                // pool mid broke six nodes and win98). ONLY the IN-EXHAUSTED
                // pool slice floors: rippled's in-limited fwd rounds the swap
                // output DOWN and the pool keeps the fraction (F51,
                // #106688646: 1642 drops for the …911 net, where the ceil
                // said 1643 and pulled a phantom drop out of the pool).
                let xrp = (me_rescale(xrp, 0, !(a_use_amm && in_exhausted)), 0);
                // ...and REPRICE leg A for it. `gets_in` above was computed
                // from the FRACTIONAL xrp; rounding the mid-leg up to whole
                // drops without redoing that leaves leg A buying a whole drop
                // and paying for a fraction less of one.
                //
                // That stale price is the entire residual left after the ceil
                // landed. On #105663160 893234589E652228 the leg A offer
                // A8B6AE1A99 is 14939.63236543909 XAH for 210947609 drops, so
                // 277768 drops cost 19.67195467422096 XAH — mainnet's number.
                // We wrote 19.67191855819117, which is what 277767.49 drops
                // cost. All four bridged residuals are exactly this, short by
                // 0.51, 0.65, 0.71 and 0.98 drops' worth: the fraction the
                // rescale rounded away.
                let gets_in = if in_exhausted {
                    // Gross-primary: rippled charges the whole remaining in
                    // however the drop rounding lands — its in-limited
                    // iteration's `in` is the remainder verbatim.
                    gets_in
                } else {
                    let repriced = rp_a(xrp, a_price(xrp), sandbox);
                    // The earlier clamp to `rem_gets` still binds — a sub-drop
                    // reprice must not push the pass past what the taker has.
                    if me_cmp(repriced, rem_gets).is_gt() { rem_gets } else { repriced }
                };
                // The IN-EXHAUSTED slice's leg B consumes the WHOLE-drop mid,
                // not the fractional value it was first priced from (F51's
                // rider on #106688646: 1642 drops at the filed rate is
                // 0.002271007836142991 where the fractional 1642.578… priced
                // 0.002271807732519948). Scoped to the exhausting slice: an
                // out-side clamp fixed `pays_out` canonically, and every
                // other pass keeps its calibrated pre-F51 pricing.
                if in_exhausted && !out_clamped {
                    pays_out = rp_b(xrp, b_price(xrp), sandbox);
                }
                if std::env::var("DX_BRIDGE").is_ok() {
                    eprintln!("DX_BRIDGE slice xrp={xrp:?} gets_in={gets_in:?} pays_out={pays_out:?} a_amm={a_use_amm} b_amm={b_use_amm} rem_g={rem_gets:?} rem_p={rem_pays:?}");
                }
                if me_is_zero(xrp) || me_is_zero(gets_in) || me_is_zero(pays_out) {
                    break 'attempt;
                }
                // Leg A: taker sells gets for XRP (XRP rides in-flight via the
                // taker and nets out of their mutation set). The taker pays the
                // input transfer fee on top of what leg A's maker received and
                // the issuer destroys it — as ONE debit of the gross, per
                // `move_leg_gross`. An exhausting slice debits the remaining
                // gross budget VERBATIM (F51) — re-grossing its divided net
                // can land an ulp off the remainder.
                let a_gross = match (in_exhausted, gets_gross_cap) {
                    (true, Some(cap)) => me_sub(cap, in_gross_spent),
                    _ => gross_in(fee_rate, gets_in),
                };
                in_gross_spent = stamount_signed_add(false, in_gross_spent, false, a_gross).1;
                match (&a_book, &a_fill) {
                    (Some((_, akey, aoffer, amaker, a_gives0, a_wants0)), _) => {
                        if std::env::var("DX_WALK").is_ok() {
                            eprintln!("DX_FILL legA book okey={} maker={} in={gets_in:?} out={xrp:?} gives0={a_gives0:?} wants0={a_wants0:?}",
                                hex::encode(akey.0), hex::encode(amaker));
                        }
                        settle_fill(sandbox, akey, aoffer, amaker, taker, taker,
                                    &xrp_leg, gets_leg, xrp, gets_in, a_gross, *a_gives0, *a_wants0);
                    }
                    (None, Some(_)) => {
                        crate::tx::amm_swap::apply_slice(
                            sandbox, amm_a.as_ref().unwrap(), taker, taker, &xrp_leg, gets_leg, gets_in, a_gross, xrp,
                        );
                        amm_used = true;
                    }
                    _ => break 'attempt,
                }
                // Leg B: taker sells that XRP for the pays side. Its input is
                // the XRP middle, which carries no issuer rate.
                match (&b_book, &b_fill) {
                    (Some((_, bkey, boffer, bmaker, b_gives0, b_wants0)), _) => {
                        // ONE offer per leg per ITERATION. rippled's BookStep
                        // walks as many offers as the step needs and DELETES
                        // each as it is exhausted; a divergence in walk DEPTH
                        // shows up here as "we modify what mainnet deletes".
                        if std::env::var("DX_WALK").is_ok() {
                            eprintln!("DX_FILL legB book okey={} maker={} in={xrp:?} out={pays_out:?} gives0={b_gives0:?} wants0={b_wants0:?}",
                                hex::encode(bkey.0), hex::encode(bmaker));
                        }
                        if bmaker != taker {
                            fee_judged = true;
                        }
                        settle_fill(sandbox, bkey, boffer, bmaker, taker, beneficiary,
                                    pays_leg, &xrp_leg, pays_out, xrp, xrp, *b_gives0, *b_wants0);
                    }
                    (None, Some(_)) => {
                        crate::tx::amm_swap::apply_slice(
                            sandbox, amm_b.as_ref().unwrap(), taker, beneficiary, pays_leg, &xrp_leg, xrp, xrp, pays_out,
                        );
                        amm_used = true;
                    }
                    _ => break 'attempt,
                }
                rem_gets = me_sub(rem_gets, gets_in);
                rem_pays = me_sub(rem_pays, pays_out);
                if in_exhausted {
                    // The gross budget is spent; the net chain's leftover is
                    // division dust rippled never sees (see the walk's rule).
                    rem_gets = (0, 0);
                }
                crossed += 1;
                fill = Some((gets_in, pays_out));
                // One bump per ITERATION, however many of the two legs the pools
                // carried — `ammContext.update()` is `if (ammUsed_) ++ammIters_`.
                if amm_used {
                    amm_iters += 1;
                }
            }
            }
            // The judge — `q < limitQuality` rejects the pass outright.
            // rippled rejects when `q < limitQuality` UNLESS `limitOut`
            // actually reduced the output and the miss is within 1e-7
            // relative — "limitOut() finds output to generate exact requested
            // limitQuality. But the actual limit quality might be slightly off
            // due to the round off" (StrandFlow.h:714-718). The tolerance is
            // GATED on `adjustedRemOut`: an unadjusted pass is judged exactly,
            // which is what keeps #106137477 resting on its 1.3e-5 miss.
            let jthr = if fee_judged { thr_judge } else { threshold };
            let accepted = match fill {
                None => false,
                Some((gin, pout)) => {
                    jthr == u64::MAX
                        || rate_of_me(gin, pout).is_some_and(|q| {
                            q <= jthr
                                || (adjusted && {
                                    // `withinRelativeDistance(q, limitQuality, 1e-7)`
                                    // (AMMHelpers.h:112-121):
                                    //   ((min.rate() - max.rate()) / min.rate()) < dist
                                    // A HIGHER Quality is better while `rate()` is its
                                    // inverse, so `min` quality is the LARGER rate — the
                                    // worse one, which here is the realised `q`. The
                                    // denominator is therefore the larger rate, not the
                                    // threshold, and the comparison is STRICTLY less.
                                    let (a, b) = (rate_me(q), rate_me(jthr));
                                    me_cmp(
                                        me_muldiv(me_sub(a, b), (10_000_000, 0), a, false),
                                        (1, 0),
                                    )
                                    .is_lt()
                                })
                        })
                }
            };
            if std::env::var("DX_BRIDGE").is_ok() {
                eprintln!("DX_CAND want_direct={want_direct} fill={fill:?} accepted={accepted}");
            }
            if accepted {
                filled = true;
                break;
            }
            // DX_RM — hunt the known `ofrsToRm` deviation.
            //
            // rippled banks a failed strand's offer removals: `setUnion(ofrsToRm,
            // f.ofrsToRm)` runs BEFORE the `!f.success` check, commented "rm bad
            // offers even if the strand fails" (StrandFlow.h:717-722). Our
            // rollback undoes them, resurrecting an offer rippled would have
            // deleted. Report any attempt whose rejection discards a removal, so
            // a ledger that distinguishes the two can be found by measurement.
            if std::env::var("DX_RM").is_ok() {
                use crate::ledger::sandbox::SandboxEntry;
                let after = sandbox.snapshot();
                let dropped: Vec<String> = after
                    .iter()
                    .filter(|(k, v)| {
                        matches!(v, SandboxEntry::Deleted)
                            && !matches!(snap.get(*k), Some(SandboxEntry::Deleted))
                    })
                    .map(|(k, _)| hex::encode_upper(&k.0[..8]))
                    .collect();
                // ★ The sharp signal. `live_head(mutate)` deletes the TAKER'S
                // OWN offers as it walks (rippled's `limitSelfCrossQuality`)
                // and records them in `stale` — which is a plain Vec OUTSIDE
                // the sandbox, so the rollback undoes the DELETION while the
                // key stays marked stale. The offer is resurrected in the
                // ledger and never revisited. Deletions with `had_fill=true`
                // are mostly offers the fill CONSUMED, which rippled discards
                // too (it drops the failed strand's sandbox and carries out
                // only `ofrsToRm`), so they are not the deviation.
                if stale.len() > st0 {
                    eprintln!(
                        "DX_RM STALE-RESURRECTED want_direct={want_direct} had_fill={} \
reaped={} deleted={}",
                        fill.is_some(),
                        stale.len() - st0,
                        dropped.len()
                    );
                } else if !dropped.is_empty() {
                    eprintln!(
                        "DX_RM ROLLBACK-DROPS-REMOVAL want_direct={want_direct} \
had_fill={} n={} keys={:?}",
                        fill.is_some(),
                        dropped.len(),
                        dropped
                    );
                }
            }
            // Rejected: undo this candidate and try the next one — but the
            // REMOVALS STAY. rippled banks a failed strand's offer removals:
            // `setUnion(ofrsToRm, f.ofrsToRm)` runs BEFORE the `!f.success`
            // check, "rm bad offers even if the strand fails"
            // (StrandFlow.h:717-722). This file already re-applies `stale`
            // after the tx-level FillOrKill/IOC rollbacks for exactly that
            // reason; the candidate rollback has to do the same or a
            // self-offer that `live_head` reaped comes back in the ledger
            // while its key stays marked stale — deleted from our bookkeeping,
            // alive in the state, and never revisited.
            sandbox.restore_snapshot(snap);
            // Re-apply the failed candidate's DEAD-offer reaps (rippled banks
            // ofrsToRm even when the strand fails) — but NOT its SELF-cross
            // deletions: limitSelfCrossQuality removes the taker's own offers
            // INSIDE the strand's sandbox, so they roll back with it.
            // #106453801: 2D468DC2's rejected bridge leg had reaped the
            // taker's own A20C602E; mainnet still holds it until EA6D14A1
            // places on that book ten txs later and self-crosses it there —
            // our banked reap mis-attributed the deletion (8v6 + 4v6 mirror).
            // Skipped keys leave `stale` so the later tx can revisit them.
            let mut keep = Vec::new();
            for okey in stale.drain(st0..) {
                let Some(off) = json_at(sandbox, &okey) else { continue };
                let Some(mk) = off.get("Account").and_then(|v| v.as_str()).and_then(decode20)
                else {
                    continue;
                };
                if mk == *taker {
                    continue; // self-cross deletion — resurrects with the rollback
                }
                delete_maker_offer(sandbox, &okey, &off, &mk);
                keep.push(okey);
            }
            stale.extend(keep);
            rem_pays = rp0; rem_gets = rg0; crossed = cr0;
            di = di0; ai = ai0; bi = bi0; amm_iters = it0;
            amm_used = false;
        }
        if !filled {
            break;
        }
    }
    Some((rem_pays, rem_gets, crossed))
}

/// Walk the inverse book from best quality and cross while the maker's rate is
/// within `threshold`. Returns (remaining pays, remaining gets, crossed count).
///
/// `taker` funds the gets side and owns any self-crossed offers; the acquired
/// pays side is credited to `beneficiary`. For OfferCreate they are the same
/// account, but a Payment's strand output belongs to the DESTINATION — routing
/// it through the sender would materialize an intermediate trust line that
/// rippled never creates (and, when the destination is the issuer, the IOU is
/// redeemed rather than held).
/// `sell` selects tfSell semantics: the taker sells the ENTIRE gets side
/// (`rem_gets`), accepting more of the pays side than requested. The binding
/// constraint becomes `rem_gets` alone — `rem_pays` (the minimum to acquire)
/// stops bounding each fill and no longer terminates the walk once reached.
/// `offer_crossing` distinguishes rippled's FlowCross from Flow, and decides
/// BOTH structural questions at once:
///
///  * **Autobridging.** Only offer crossing synthesizes the XRP bridge
///    (`flow()` builds the direct and bridged strands itself). A payment's
///    strands come from its explicit Paths plus the default path, so a
///    payment reaches a book only where its path names one — it never routes
///    through an XRP leg the transaction did not ask for.
///  * **The AMM offer generator** (`AMMContext::multiPath`, set from the
///    strand count at `Flow.cpp:106` `setMultiPath(strands.size() > 1)`).
///    Bridged crossing is two strands, so the pool competes with Fibonacci
///    slices; every other walk is single-strand and sizes the AMM by
///    `changeSpotPriceQuality`/`maxOffer` instead. That correspondence is
///    exact here: `cross_bridged` runs iff multiPath would be true.
///
/// The one case this does not model is a payment carrying two or more
/// explicit Paths, which rippled would treat as multi-path; we walk only the
/// first path, so such a payment stays on the single-strand generator.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cross_engine_to(
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    rem_pays: Me,
    rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    threshold_self: u64,
    sell: bool,
    offer_crossing: bool,
    single_pass: bool,
    amm_fib: Option<&mut AmmFib>,
    domain: Option<&Hash256>,
    sandbox: &mut Sandbox,
    stale: &mut Vec<Hash256>,
) -> (Me, Me, u32) {
    cross_engine_to_net(
        taker, beneficiary, rem_pays, rem_gets, pays_leg, gets_leg, threshold, threshold_self,
        sell, offer_crossing, single_pass, amm_fib, domain, None, None, sandbox, stale,
    )
}

pub(crate) fn cross_engine_to_net(
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    mut rem_pays: Me,
    mut rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    // rippled's true `limitQuality` (transfer-rate inflated): self-cross gate only.
    threshold_self: u64,
    sell: bool,
    offer_crossing: bool,
    // Stop after the first book level that moves value — one PASS, in rippled's
    // sense. `flow()` runs a strand for a single quality level per BookStep and
    // then re-enters, which is what lets the outer strand loop interleave two
    // strands by marginal quality instead of draining the better one first
    // (StrandFlow.h:640-756). Only the multi-strand payment loop asks for this;
    // every other caller walks the whole book as before.
    single_pass: bool,
    // Flow-wide AMM fib state; Some only for a multi-strand payment.
    mut amm_fib: Option<&mut AmmFib>,
    domain: Option<&Hash256>,
    // The strand's WANT-side issuer rate and the NET the rev pass sized this
    // walk for. rippled's fwd DirectStep credits the destination the rev
    // cache's srcToDst — NET, once per iteration — and only a partial fill
    // recomputes out/rate (DirectStep.cpp:492 cache; :646 mulRatio nearest).
    // Some ⇒ the beneficiary settlement converts: full delivery of the ask =
    // the cache hit, credited `net_ask` verbatim; anything less divides.
    // Meaningful only for single_pass walks (one settlement per call — the
    // per-iteration shape); every other caller passes None via the wrapper.
    benef_net: Option<(u64, Me)>,
    // The walk's GROSS in-cap (the sender-side line or SendMax bound, in
    // gross units). rippled's in-limited fills are GROSS-PRIMARY — the offer
    // that exhausts remainingIn takes stpIn = the remaining cap VERBATIM and
    // derives the net by division (limitStepIn; the LIMITSTEPIN receipts) —
    // so a balance-bound drain lands the spend line on exactly zero however
    // mulRatio rounds the earlier fills. #106455039 A08513AF is the receipt:
    // a full-balance GBP dump that mainnet drains to 0 and the per-fill
    // mulRatio sum left at 1e-16. Some ⇒ the fill/slice that exhausts
    // rem_gets debits cap − (gross already spent) instead of gross_in.
    gets_gross_cap: Option<Me>,
    sandbox: &mut Sandbox,
    stale: &mut Vec<Hash256>,
) -> (Me, Me, u32) {
    let ask0 = rem_pays;
    let mut in_gross_spent: Me = (0, 0);
    if std::env::var("DX_ENTRY").is_ok() {
        eprintln!(
            "DX_ENTRY rem_pays={rem_pays:?} rem_gets={rem_gets:?} benef_net={benef_net:?} gets_gross_cap={gets_gross_cap:?} single={single_pass} thr={threshold:016x} thr_self={threshold_self:016x}"
        );
    }
    let mut crossed = 0u32;
    // rippled's flowCross runs through flow(): ONE quality level (or one AMM
    // slice) per iteration — the stream's level-change check breaks the pass
    // (BookStep.cpp:720-724) — and each iteration re-derives remainingOut as
    // outReq − the sorted ascending 16-digit fold of savedOuts
    // (StrandFlow.h:639-642), never a full-width running subtraction. Over a
    // 40-level walk the running form drifts: #106455246 5BEBD5DB's last fill
    // asks 322.94295330646129 where rippled's fold-derived remainder is
    // …64600 to the digit ("New flow iter 39: 220679892 322.94295330646"),
    // and the partially-consumed maker's residual splits 13 ulp. CROSSINGS
    // ONLY: single_pass callers run one level and their drivers (payment.rs)
    // already keep StrandFlow's totals.
    let fold_rem = offer_crossing && !single_pass;
    // Per-iteration strand-admission verdict for the beyond-strict self-reap
    // sweep: None = not yet judged this iteration; re-judged only after a
    // fill or slice starts a new iteration (see the sweep arm).
    let mut sweep_admitted: Option<bool> = None;
    let out_req0 = rem_pays;
    let mut saved_level_outs: Vec<Me> = Vec::new();
    let mut level_out_acc: Me = (0, 0);
    fn fold16(v: &mut Vec<Me>) -> Me {
        v.sort_by(|a, b| me_cmp(*a, *b));
        let mut t: Me = (0, 0);
        for e in v.iter() {
            t = stamount_signed_add(false, t, false, *e).1;
        }
        t
    }
    // rippled judges a flow ITERATION on the quality it ACTUALLY REALISED, not
    // on the filed rates of the offers it took, and a pass that misses
    // `limitQuality` is thrown away WHOLE — "Path rejected by limitQuality"
    // (StrandFlow.h:720) leaves the strand dry and `Total flow: in: 0 out: 0`.
    // Every offer we take already passed the per-offer `q <= threshold` gate on
    // its FILED rate; what the filed rate cannot see is the taker's transfer fee
    // and the flooring of an integral output, both of which land on the
    // funding-limited tail of the pass.
    //
    // #106347648 622A7DD2 (an IoC repeating every ledger from one bot): the
    // maker is filed at 923.617 drops/SGB against a 920.20 limit, so it crosses
    // — but the taker's whole 0.5156870490053416 SGB buys only 474 drops once
    // the 1.003 gateway takes its cut and the drops floor, realising 919.16 and
    // missing. rippled crosses NOTHING and returns tecKILLED with one mutation;
    // we kept the fill, and IoC's `crossed == 0` test then read it as a success.
    //
    // ⚠ APPROXIMATION: rippled would keep earlier ITERATIONS and reject only
    // the failing one, where we judge the walk's aggregate. The two agree
    // whenever the pass is a single iteration, which is the shape that fails —
    // an aggregate can only miss when the funding-limited tail dominates it,
    // since every other fill is filed at or better than the limit. A specimen
    // where mainnet keeps an early fill and drops a later one is what would
    // justify carrying per-iteration state through the walk.
    let cross_snap = offer_crossing.then(|| sandbox.snapshot());
    // AMM in-fee (see amm_swap::consume_fib): the strand pays the
    // in-issuer's rate ON TOP of the pool's input (rippled BookStep trIn =
    // redeems(prevStepDir) ? rate(book.in) : parity, BookStep.cpp:697).
    // The implied first hop src → SendMax-issuer REDEEMS, so this applies
    // in CROSSINGS too — the old "never in crossing" arm mismatched the
    // CLOB fill sites, which already charge it (#105877543). #106455037
    // 564C4C30 (full-ledger replay): sell 0.00328 BTC.rvYA into the
    // XRP/BTC pool — mainnet debits the taker exactly 1.0015x the pool's
    // receipt. Waived only when the taker IS the in-issuer (then the
    // implied hop doesn't exist and the book is the strand's first step).
    let pay_in_rate = transfer_rate(sandbox, gets_leg).filter(|_| taker != &gets_leg.issuer);
    // The fee-composed judge threshold for CLOB fills of a crossing (see
    // crossing_judge_threshold). Payments and AMM turns keep the raw one.
    let thr_judge = if offer_crossing {
        crossing_judge_threshold(sandbox, pays_leg, gets_leg, taker, threshold)
    } else {
        threshold
    };
    let (entry_pays, entry_gets) = (rem_pays, rem_gets);
    if threshold == 0 {
        return (rem_pays, rem_gets, crossed);
    }
    // Strand is exhausted when the gets side is spent (always) or, for a
    // buy, when the wanted pays side is fully acquired.
    let done = |rp: Me, rg: Me| me_is_zero(rg) || (!sell && me_is_zero(rp));
    // rippled settles the TAKER once per PASS, not per fill. In the forward
    // pass the leading DirectStep debits the taker Σ stpAmt.in — the sum
    // BookStep accumulated with per-offer STAmount adds (`result.in +=
    // stpAmt.in`) — and the closing DirectStep credits Σ out the same way.
    // Only the MAKERS are settled per offer (consumeOffer's two
    // `offer.send`s, BookStep.cpp:900-915). Storing the taker's line at
    // every fill instead rounds it to 16 digits N times, and the
    // intermediate roundings do not cancel.
    //
    // #106455075 28881BD5 (full-ledger replay) is the specimen: three fills
    // 3.30620463597954 + 3.30620463597954 + 4356.73862306011 (each fill
    // sized byte-exactly — DX_FILL traced). The aggregated debit is
    // 4363.351032332069 and mainnet's taker line lands 3490.843917749044;
    // per-fill stores shave a .46 tail at TWO intermediates and come out one
    // ulp low. XRP legs stay per-fill: whole drops cannot round.
    //
    // AMM slices inside THIS walk fold into the same accumulator (threaded
    // into `consume`/`consume_fib`): a mixed AMM+CLOB pass is still one
    // debit in rippled. #106455040 0F821DBF is the proof by regression —
    // three CLOB fills + one pool slice; aggregating only the CLOB side
    // makes TWO stores where mainnet's single debit 3338.173228234692
    // needs none, and lands two ulp off. The bridged controller settles per
    // ROUND (each round is a pass) and keeps passing None.
    //
    // .0 = pays_leg, taker receives; .1 = gets_leg GROSS, taker parts with.
    //
    // The settlement boundary is the flow ITERATION = ONE QUALITY LEVEL
    // (or one AMM slice): a BookStep execution stops at the level edge so
    // the pass's marginal quality is well-defined, and `flow()` re-enters
    // for the next level — which is the same fact the `single_pass` comment
    // above records for strand interleaving. Fills at one level therefore
    // share a single taker debit; a new level (or a pool turn, which only
    // happens at level heads) closes the group.
    //
    // Calibrated by exhaustive partition search over four specimens (the
    // full-walk and per-slice-boundary schemes each fix some and break
    // others; per-level satisfies all):
    //   #106455075 28881BD5  CC|C    (same-level pair aggregates)
    //   #106455040 0F821DBF  C|C|C|X (three levels, tail slice)
    //   #106455036 1D61A047  C|X|C|X
    //   #106455038 136FE701  X|C|X   (anchored slice, fill, tail slice)
    let mut taker_accs: (Me, Me) = ((0, 0), (0, 0));
    let mut acc_level: Option<u64> = None;
    macro_rules! settle_taker {
        () => {
            if !me_is_zero(taker_accs.0) {
                if std::env::var("DX_SETTLE").is_ok() {
                    eprintln!(
                        "DX_SETTLE pot={:?} ask0={ask0:?} benef_net={benef_net:?} single={single_pass}",
                        taker_accs.0
                    );
                }
                // See `benef_net` at the signature: the destination of a
                // rate-bearing want leg receives NET — the rev-sized net on a
                // full delivery of the ask (the fwd cache hit), out/rate at
                // mulRatio-nearest otherwise.
                let credit = match benef_net {
                    Some((rate, net_ask)) => {
                        if me_cmp(taker_accs.0, ask0) == std::cmp::Ordering::Equal {
                            net_ask
                        } else {
                            mul_ratio(taker_accs.0, 1_000_000_000, rate as u128, false)
                        }
                    }
                    None => taker_accs.0,
                };
                line_adjust(sandbox, beneficiary, pays_leg, credit, true);
            }
            if !me_is_zero(taker_accs.1) {
                line_adjust(sandbox, taker, gets_leg, taker_accs.1, false);
            }
        };
    }
    // AMM for the pair competes with the book at every quality level
    // (rippled BookStep + AMMLiquidity) — EXCEPT in a permissioned-domain
    // book, which no pool participates in: `BookStep::tryAMM` returns early
    // on `book_.domain` ("amm doesn't support domain yet", BookStep.cpp:820).
    // A domain offer therefore crosses domain offers only, and rests in full
    // when none match, however good the pool's price looks (#105761560
    // C9948B9C crossed 1.238909 XRP against the EUROP pool where mainnet
    // moved no value at all).
    let amm = match domain {
        Some(_) => None,
        None => crate::tx::amm_swap::discover(sandbox, gets_leg, pays_leg, taker),
    };
    let inv_base = match domain {
        Some(d) => keylet::book_base_domain(&gets_leg.cur, &pays_leg.cur, &gets_leg.issuer, &pays_leg.issuer, d),
        None => keylet::book_base(&gets_leg.cur, &pays_leg.cur, &gets_leg.issuer, &pays_leg.issuer),
    };
    // IOU↔IOU pairs autobridge through XRP (open books; the direct-pair AMM
    // competes inside the controller).
    if offer_crossing && !pays_leg.xrp && !gets_leg.xrp && domain.is_none() {
        if let Some(r) = cross_bridged(
            taker, beneficiary, rem_pays, rem_gets, pays_leg, gets_leg, threshold, threshold_self,
            sell, &inv_base, &amm, gets_gross_cap, sandbox, stale,
        ) {
            return r;
        }
    }
    let dirs = sandbox.keys_with_prefix(&inv_base.0[..24]);
    // Set once the fill is satisfied but rippled would still have stepped the
    // stream. The stream spans the whole BOOK, not one level: `step` carries no
    // quality test, so it keeps reaping across levels until it reaches a live
    // offer, and only then does `execOffer`'s `checkQualityThreshold` end the
    // walk. See the `done` branch at the end of the maker loop.
    let mut trailing = false;
    // The first level always anchors the pool: rippled's single `tryAMM` fires
    // on the first live tip whatever it is, self-offer included.
    let mut prev_level_crossed = true;
    // A self-offer SKIPPED without consumption stays the tip of rippled's book
    // until an EXECUTING pass steps onto it (`limitSelfCrossQuality` removes it
    // then; removals apply between flow iterations, StrandFlow.h:694). Every
    // later pass therefore re-anchors tryAMM on ITS quality — even a pass that
    // then finds nothing and goes dry. #106010546 7E746D30: the 11536 tip is
    // the taker's own; iter 1 anchors the pool there, the pool's moved spot is
    // worse, the next offer (11585.9) fails the 11563.2 threshold, the strand
    // is DRY and 0.5888785811 rests. Our tail turn ran unanchored (maxOffer)
    // and swept the pool instead. Remember the first skipped self-offer's
    // level; a later CLOB consumption clears it (a delivering pass that
    // stepped past the self-offer applied its removal — the tail then really
    // is unanchored).
    let mut self_anchor_q: Option<u64> = None;
    // Reserve pinning: every funding check inside the walk prices a maker's
    // reserve at the OwnerCount of its FIRST peek — rippled's deletions are
    // deferred past the walk and never free reserve units mid-flow
    // (walk_available).
    let mut oc0: std::collections::HashMap<[u8; 20], u64> = Default::default();
    // The first beyond-threshold level the walk stopped at — the residual
    // raw tip rippled's next pass would anchor tryAMM on (see the tail turn).
    let mut residual_q: Option<u64> = None;
    // TRUE once any level activated the strand (tip or pool within the
    // limit) — the moment rippled would have BUILT the offer stream.
    let mut stream_ran = false;
    'dirs: for dk in dirs {
        let (level_pays_in, level_gets_in) = (rem_pays, rem_gets);
        let q = u64::from_be_bytes(dk.0[24..32].try_into().unwrap_or_default());
        if trailing {
            if reap_to_live_head(sandbox, &dk, pays_leg, gets_leg, Some(&mut oc0), stale) {
                break 'dirs;
            }
            continue;
        }
        // Step the offer stream to this level's first LIVE offer before the
        // pool is consulted, reaping the dead ones passed on the way. A level
        // with nothing live never happened as far as the crossing is
        // concerned: it lends no `clobQuality`, so the stream keeps stepping
        // and the pool anchors on the next live level (or, past the last one,
        // on the unanchored tail turn below).
        //
        // Only within the taker's limit, though. An OFFER-CROSSING strand
        // whose best possible quality is already worse than `limitQuality` is
        // dropped before `flow()` is ever called (StrandFlow.h:682-690
        // `qualityUpperBound(sb, *strand) < *limitQuality => continue`, and
        // the same filter in `activateNext`, StrandFlow.h:465), so no stream
        // is built and nothing is reaped — the removals on line 694 are
        // collected from a flow that RAN, "even if the strand fails".
        // #105795013 428E0550 sells RLUSD priced above the whole book: mainnet
        // rests it in 4 nodes and leaves the expired E39542EC alone, for
        // rfPBiFvFeBQ's own later 612F4E95 to clear. A payment carries no
        // `limitQuality` and so no such gate — `threshold` is u64::MAX there
        // and this reads as always-reap, which is what #105795716 needs.
        // ...or when the POOL alone is what activates the strand. rippled
        // filters strands on an optimistic `qualityUpperBound`, and for a book
        // with an AMM that bound is the pool's FEELESS spot — so a strand whose
        // every book level is worse than the limit still gets built, its offer
        // stream still steps, and the dead offers it steps past are still
        // reaped, even though the fee then makes the pool's real offer miss the
        // limit and nothing crosses at all.
        //
        // #105922825 851508DADF49 sells 1.0723 RLUSD for 1 XRP. EVERY offer in
        // the XRP/RLUSD book is worse than its 1.0723e-6 limit — the tip
        // ED7B21F8 is 1.073114e-6 — so on the book alone the strand is skipped
        // and nothing is touched, which is what we did. But the pool's feeless
        // spot is 1.07227e-6, inside the limit by a hair, so mainnet builds the
        // strand, reaps the tip (expired at 838611910 against a parent close of
        // 838611912) together with its now-empty book page, and rests the offer
        // in full: 8 nodes to our 4.
        let strand_active = q <= threshold
            || amm.as_ref().is_some_and(|a| {
                crate::tx::amm_swap::spot_upper_bound(sandbox, a, pays_leg, gets_leg) <= threshold
            });
        stream_ran = stream_ran || strand_active;
        if strand_active && !reap_to_live_head(sandbox, &dk, pays_leg, gets_leg, Some(&mut oc0), stale) {
            continue;
        }
        // AMM turn: consume pool liquidity while its spot quality strictly
        // beats this book level (anchored so the book resumes at `q`).
        //
        // ONE turn per PASS, not per level. rippled calls `tryAMM` exactly once
        // per `forEachOffer`, anchored on the first live tip
        // (BookStep.cpp:855-865), and a pass ends only when an offer is really
        // CONSUMED — the strand then re-runs and the pool gets another turn. An
        // offer merely REMOVED never ends the pass: `limitSelfCrossQuality`
        // returns before anything is consumed and the walk keeps stepping
        // inside the same `forEachOffer`, with no second `tryAMM`.
        //
        // #105945386 7EF34E79F13A: the two levels ahead of the pool hold
        // nothing but the taker's OWN offers, so re-anchoring on each of them
        // took three slices (182925 + 113464 + 422213 drops) that bought the
        // order outright. rippled takes ONE slice of 182925 and then flows the
        // remaining 535677 as a single pass costing 46.96292384379671 SPEPE —
        // one unit past its own limit — which the achieved-quality check then
        // rejects, leaving exactly that remainder to rest.
        //
        // ...AND ONLY ON A LEVEL THE TAKER COULD ACTUALLY TRADE. `tryAMM` is
        // handed `offers.tip().quality()`, and a tip beyond the limit is not a
        // tip the stream ever reaches — rippled then calls it with NO clob
        // quality at all, which is `maxOffer`, and `limitOut` trims that to the
        // limit instead. Anchoring to an unreachable level sizes the slice off
        // a price the taker rejected.
        //
        // #106295504 E9F363D7 (and #106297489 3BEC441E, the same market maker
        // hours later): tfSell|tfPassive, 1250 XRP for 16.604535 SOL, so a
        // limit of 75280230.658627. The book's tip is 75323299.971751 — WORSE —
        // and the XRP/SOL pool sits between them at 75152490.428151. rippled
        // builds maxOffer, trims it, flows 133055 drops realising
        // 75280687.18924356, misses its own limit by 6.06e-6 (past the 1e-7
        // forgiveness) and REJECTS the pass: `Total flow: in: 0 out: 0`, the
        // offer rests whole. Anchored to that unreachable tip we sized 128056
        // drops instead, whose realised quality CLEARS the limit — so we
        // crossed a pool mainnet never touched, three extra mutations.
        // The break below then falls through to the unanchored tail turn, which
        // is exactly the call rippled makes.
        if let Some(a) = amm.as_ref().filter(|_| prev_level_crossed && q <= threshold) {
            if std::env::var("DX_AMM").is_ok() {
                eprintln!("DX_AMM site=direct-walk q={q:x}");
            }
            // A turn happens at a level head: close the open level's debit
            // (harmless when the turn declines — the level was ending
            // anyway). The slice itself settles per-slice inside consume.
            settle_taker!();
            taker_accs = ((0, 0), (0, 0));
            acc_level = None;
            let (rp, rg, used) = amm_turn(
                amm_fib.as_deref_mut(), sandbox, a, taker, beneficiary,
                benef_net.map(|(r, na)| (r, na, ask0)),
                gets_gross_cap.map(|c| me_sub(c, in_gross_spent)), rem_pays, rem_gets,
                pays_leg, gets_leg, threshold, sell, Some(q), pay_in_rate,
            );
            if used {
                // Mirror of the slice settlement's own gross (settle_slice):
                // exhausting rem_gets takes the remaining cap, else gross_in.
                let slice_net = me_sub(rem_gets, rg);
                let g = match gets_gross_cap {
                    Some(cap) if me_is_zero(rg) => me_sub(cap, in_gross_spent),
                    _ => gross_in(pay_in_rate, slice_net),
                };
                in_gross_spent = stamount_signed_add(false, in_gross_spent, false, g).1;
            }
            // An AMM slice is its own flow iteration (BookStep.cpp:818):
            // bank the pending level entry, then the slice's out, and
            // re-derive from the fold.
            if fold_rem && used {
                if !me_is_zero(level_out_acc) {
                    saved_level_outs.push(level_out_acc);
                    level_out_acc = (0, 0);
                }
                let slice_out = me_sub(rem_pays, rp);
                if !me_is_zero(slice_out) {
                    saved_level_outs.push(slice_out);
                }
                rem_pays = me_sub(out_req0, fold16(&mut saved_level_outs));
            } else {
                rem_pays = rp;
            }
            if used {
                sweep_admitted = None;
            }
            rem_gets = rg;
            crossed += used as u32;
            if done(rem_pays, rem_gets) {
                break 'dirs;
            }
            // ONE AMM CONSUMPTION PER PAYMENT-ENGINE ITERATION. "At any payment
            // engine iteration, AMM offer can only be consumed once"
            // (BookStep.cpp:818) — so when a payment wants more pool liquidity
            // than one AMM offer gives, rippled ENDS the pass and `flow()`
            // re-enters the strand for the next one. Each iteration writes the
            // maker's residual, so N iterations are N roundings; taking fib
            // slices inside a single pass rounds once and lands elsewhere.
            //
            // #105795329 ED4F899F is the specimen: two rounds of 220.414... and
            // 144.460... spend 193488035 drops + 10 fee, which is mainnet's
            // spend to the drop, and the transaction goes 4 hits to 1.
            //
            // PAYMENTS ONLY. The boundary is only meaningful because
            // `apply_path_payment`'s round loop re-enters the strand with what
            // is left, exactly as `flow()`'s driver does. FlowCross has no such
            // loop, so returning here would abandon the rest of the book rather
            // than come back to it — measured, and it moves the XRP side of an
            // offer-crossing pass that is byte-exact today.
            if used && !offer_crossing {
                settle_taker!();
                return (rem_pays, rem_gets, crossed);
            }
        }
        // Set when this level CONSUMES an offer, which is what ends a pass and
        // earns the pool its next turn. Removals leave it false.
        let mut level_crossed = false;
        if std::env::var("DX_BOOK").is_ok() {
            eprintln!("DX_BOOK dir q={q:016x} threshold={threshold:016x} cross={}", q <= threshold);
        }
        if q > threshold {
            if residual_q.is_none() {
                residual_q = Some(q);
            }
            // OFFER CROSSING steps PAST the strict threshold. In the stream
            // loop `limitSelfCrossQuality` runs BEFORE `checkQualityThreshold`
            // (BookStep.cpp:729/770) and both gate on the TRUE limitQuality
            // (`qualityThreshold_`, transfer-rate inflated) — so a level
            // between the strict and inflated thresholds is still VISITED:
            // dead offers are reaped by `step()` on the way and the taker's
            // own offers are perm-removed — "Remove this offer even if no
            // crossing occurs" (BookStep.cpp:441-443). The first LIVE foreign
            // offer ends the walk: its fill would flunk the strand limit and
            // the pass rejects, but a flow that RAN keeps its removals
            // (StrandFlow.h:694).
            //
            // #106455225 8F42D06D: the taker's opposite-side offer from
            // #106455221 rests ONE level beyond strict (dir rate +0.102%,
            // inside the BTC issuer's TransferRate inflation); mainnet
            // deletes offer + book page without trading and rests the new
            // offer. Our strict page gate broke here and left both behind.
            //
            // ⚠ ONLY IF THE STRAND IS ADMITTED: a strand whose quality
            // upper bound misses limitQuality is skipped before flow ever
            // runs — no fills, NO REMOVALS (StrandFlow.h:682-690).
            // #106455051 C925148B is the counter-specimen: same bot, same
            // book, self offer at the TIP inside the same band — mainnet
            // keeps offer and page, oracle: `admitted false / All strands
            // dry`.
            //
            // The ub's composition is ASYMMETRIC (adjustQualityWithFees,
            // BookStep.cpp:513-545): a CLOB tip enters RAW — "assume no
            // fee is charged, or the estimate will no longer be an upper
            // bound" — while a single-path AMM synthetic (made only when
            // the fee-inclusive spot beats the tip, `AMMLiquidity::
            // getOffer` else "higher clob quality") composes trIn. Both
            // oracle receipts, three ledgers apart on the same book:
            //   #225 8F42D06D: pool declines, ub = tip RAW 1.8709e-11
            //     ≤ limit 1.87179e-11 → admitted, self offer removed;
            //   #051 C925148B: synthetic 1.84732e-11 × 1.0015 =
            //     1.85010e-11 > limit 1.84916e-11 → refused, kept.
            // The raw-spot bound cannot decide #051 (spot×trIn passes,
            // synthetic×trIn fails) — the tip-anchored synthetic itself
            // is required, which is exactly `anchored_slice`.
            // Admission is judged ONCE PER ITERATION, at the iteration's
            // tip — and a self-removal with no offer yet attempted RESETS
            // the stream's level anchor (`if (!offerAttempted) ofrQ =
            // std::nullopt`, BookStep.cpp:441-448), so consecutive
            // self-offer levels all ride the FIRST level's admission with
            // no re-judging in between. #106455252 3F75D634: two self
            // offers at 1.86775e-11 and 1.86899e-11 — the pool peek at the
            // second level would refuse (our old per-level re-admission
            // kept the second offer), but rippled removes BOTH inside
            // iteration 0, admitted once at the q1 tip ("FLOWDBG iter 0 …
            // admitted true", then All strands dry). The verdict is
            // re-judged only after a fill or slice — a new iteration.
            if offer_crossing && threshold_self != 0
                && threshold_self != u64::MAX && q <= threshold_self
                && *sweep_admitted.get_or_insert_with(|| {
                    match amm.as_ref().and_then(|a| {
                        crate::tx::amm_swap::anchored_slice(sandbox, a, pays_leg, gets_leg, q)
                    }) {
                        Some((si, so)) => rate_of_me(gross_in(pay_in_rate, si), so)
                            .is_some_and(|ub| ub != 0 && ub <= threshold_self),
                        None => true,
                    }
                })
            {
                let mut page_key_h = dk;
                for _ in 0..10_000 {
                    let Some(page) = json_at(sandbox, &page_key_h) else { break };
                    let entries: Vec<String> = page
                        .get("Indexes")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()
                        })
                        .unwrap_or_default();
                    for ent in entries {
                        let Some(okey) = hex::decode(&ent)
                            .ok()
                            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                            .map(xrpl_core::types::Hash256)
                        else { continue };
                        let Some(offer) = json_at(sandbox, &okey) else { continue };
                        if offer.get("LedgerEntryType").and_then(|v| v.as_str())
                            != Some("Offer")
                        {
                            continue;
                        }
                        let Some(maker) =
                            offer.get("Account").and_then(|v| v.as_str()).and_then(decode20)
                        else { continue };
                        if &maker == taker {
                            if std::env::var("DX_WALK").is_ok() {
                                eprintln!(
                                    "DX_WALK selfreap q={q:016x} okey={}",
                                    hex::encode(okey.0)
                                );
                            }
                            delete_maker_offer(sandbox, &okey, &offer, &maker);
                            stale.push(okey);
                            continue;
                        }
                        if reap_if_dead(
                            sandbox, &okey, &offer, &maker, pays_leg, gets_leg,
                            Some(&mut oc0), stale,
                        ) {
                            continue;
                        }
                        break 'dirs;
                    }
                    let next = page.get("IndexNext").map(dirnum).unwrap_or(0);
                    if next == 0 {
                        break;
                    }
                    page_key_h = keylet::dir_page_key(&dk, next);
                }
                continue;
            }
            // rippled's offer stream has no quality gate for STEPPING: once
            // the strand was BUILT, rev keeps stepping past DEAD offers on
            // levels beyond the limit — reaping them — until a LIVE offer
            // stops it (`checkQualityThreshold` ends the walk at execution,
            // not at step). #106091383 6DCDD907: after the 1914.82 RLUSD
            // fill, mainnet reaps the expired offers on the beyond-limit
            // 4F03CA62 level (3 deletions + the owner root we were missing)
            // before the live beyond-limit tip ends the walk. A strand never
            // built reaps nothing — #105795013 rests in 4 nodes and leaves
            // the expired E39542EC alone.
            if stream_ran {
                if reap_to_live_head(sandbox, &dk, pays_leg, gets_leg, Some(&mut oc0), stale) {
                    break 'dirs;
                }
                continue;
            }
            break;
        }
        let mut page_key_h = dk;
        for _ in 0..10_000 {
            let Some(page) = json_at(sandbox, &page_key_h) else { break };
            if std::env::var("DX_WALK").is_ok() {
                eprintln!("DX_WALK page={} json={}", hex::encode(page_key_h.0), page);
            }
            let entries: Vec<String> = page
                .get("Indexes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            for ent in entries {
                let Some(okey) = hex::decode(&ent)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                    .map(xrpl_core::types::Hash256)
                else { continue };
                if std::env::var("DX_WALK").is_ok() {
                    let o = json_at(sandbox, &okey);
                    eprintln!(
                        "DX_WALK entry={} loaded={} type={:?}",
                        &ent[..16],
                        o.is_some(),
                        o.as_ref().and_then(|v| v.get("LedgerEntryType").and_then(|t| t.as_str())).unwrap_or("-"),
                    );
                }
                let Some(offer) = json_at(sandbox, &okey) else { continue };
                if offer.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("Offer") {
                    continue;
                }
                let Some(maker) = offer.get("Account").and_then(|v| v.as_str()).and_then(decode20)
                else { continue };
                if trailing {
                    if reap_if_dead(sandbox, &okey, &offer, &maker, pays_leg, gets_leg, Some(&mut oc0), stale) {
                        continue;
                    }
                    break 'dirs;
                }
                if offer_crossing && &maker == taker {
                    // Self-crossing: rippled cancels the older own offer.
                    //
                    // ⚠ OFFER CROSSING ONLY. A PAYMENT consumes the payer's own
                    // offer like anyone else's: `BookPaymentStep::
                    // limitSelfCrossQuality` is `{ return false; }` under the
                    // comment "Never limit self cross quality on a payment"
                    // (BookStep.cpp:295-308); the removal lives solely in
                    // `BookOfferCrossingStep`.
                    //
                    // #106137720 `F2C989D4843A`: a circular-arb Payment
                    // (Account == Destination) sells CNY for XRP with
                    // tfPartialPayment. The CNY→XRP book holds exactly ONE
                    // offer and the payer owns it — 960 852 drops against a
                    // DeliverMin of 1 937 153 — so mainnet crosses it, falls
                    // short and returns tecPATH_PARTIAL. Cancelling it
                    // unconditionally emptied the book and gave tecPATH_DRY.
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                    if self_anchor_q.is_none() {
                        self_anchor_q = Some(q);
                    }
                    continue;
                }
                // Expired offers are never crossed — the stream collects them
                // as removable and they are deleted (`hasExpired`: expiry is
                // reached once parentCloseTime >= Expiration, so the base
                // ledger's close time is the one to test).
                // #105776250 CD408C1D crosses a book whose head expired 17s
                // before the parent closed; mainnet deletes it and kills the
                // offer, we consumed it.
                if let Some(exp) = offer.get("Expiration").and_then(|v| v.as_u64()) {
                    if exp != 0 && sandbox.base().header.close_time as u64 >= exp {
                        delete_maker_offer(sandbox, &okey, &offer, &maker);
                        stale.push(okey);
                        continue;
                    }
                }
                let (Some(m_gives0), Some(m_wants0)) = (
                    offer.get("TakerGets").and_then(keylet::amount_mant_exp),
                    offer.get("TakerPays").and_then(keylet::amount_mant_exp),
                ) else { continue };
                if m_gives0.0 == 0 || m_wants0.0 == 0 {
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                    continue;
                }
                // rippled re-checks the maker's ACTUAL quality against the
                // taker's limit, not just the quantized book-dir level it rests
                // in: an offer can sit in a page that ties the threshold while
                // its exact getRate is one ULP worse, and rippled leaves such an
                // offer untouched. #105787531 BB6660FA rests in dir
                // 5A090CC3291B2B61 (== threshold) but its 5211839/20.46019077105154
                // rate encodes to 2547307138198370, one over the threshold's
                // ...369 — mainnet never crosses it; we did (10v4). This is NOT
                // the fill's achieved rate (that check is below) but the maker
                // offer's own advertised rate. The equal-quality maker of
                // #105672435 (rate == threshold) still crosses.
                if threshold != u64::MAX {
                    if let Some(mq) = rate_of_me(m_wants0, m_gives0) {
                        if mq > threshold {
                            continue;
                        }
                    }
                }
                let funded = walk_available(sandbox, &maker, pays_leg, Some(&mut oc0));
                if std::env::var("DX_WALK").is_ok() {
                    eprintln!(
                        "DX_WALK maker={} okey={} gives0={m_gives0:?} wants0={m_wants0:?} funded={funded:?} rem_pays={rem_pays:?} rem_gets={rem_gets:?}",
                        hex::encode(maker),
                        hex::encode(okey.0),
                    );
                }
                if me_is_zero(funded) {
                    // Unfunded offers found during the walk are removed.
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                    continue;
                }
                // `step` applies the tiny-offer test to every offer it reaches,
                // not just the ones ahead of the head (OfferStream.cpp:302).
                if is_dust_offer(sandbox, &maker, m_wants0, m_gives0, pays_leg, gets_leg) {
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                    continue;
                }
                let m_gives = if me_cmp(funded, m_gives0).is_lt() { funded } else { m_gives0 };
                // A sell takes the whole maker offer (bounded only by rem_gets
                // below); a buy stops at the wanted rem_pays.
                let buy_bound = !sell && me_cmp(rem_pays, m_gives).is_lt();
                let mut give = if buy_bound { rem_pays } else { m_gives };
                // Track whether the TAKER's own side bounds this fill. The
                // achieved-quality re-check below only rejects (IoC/FoK style)
                // when it does — a taker whose partial buy or gets-side clamp
                // floors the output worse than its limit. A maker that is simply
                // UNDERFUNDED at the book quality is crossed to its funded amount
                // and the taker rests the remainder; rippled does not re-reject
                // that, so the ≤1e-7 round-up on `pay` must not strand the whole
                // offer (#105672435 B409D45C: best maker 499CA86D is funded only
                // 22.283542 of 22.928591 at the taker's EXACT limit q=0x5a091beb…
                // — we rested full, mainnet crossed it and rested 0.645).
                let mut taker_clamped = buy_bound;
                // Set when the RATED in-clamp below sizes this fill off the
                // remaining GROSS budget: the settlement then debits that
                // remainder verbatim and the walk's net residual is spent
                // round-down dust, not liquidity still owed.
                let mut in_exhausted = false;
                if pays_leg.xrp {
                    give = (me_rescale(give, 0, false), 0);
                }
                // Price the fill through the offer's ENCODED quality, not its
                // raw TakerPays/TakerGets ratio. `Quality::ceil_out` is
                //   result.in = mulRound(limit, quality.rate(), asset, roundUp)
                // (Quality.cpp ceilOutImpl, and `ceil_out` always passes
                // roundUp=true), and `quality.rate()` is the 16-digit rate the
                // BookDirectory encodes — which is what `q` already carries.
                //
                // The two differ in the last digit, and that digit decides
                // whether the achieved-quality judge below fires.
                // #105924683 E7399DA36A79: maker 3B4AD7E0 is 709289 drops for
                // 61 SPEPE, and the fill takes 418487 drops.
                //   raw 418487*61/709289 = 35.9905581504859091…, ceil ->
                //                          35.99055815048591  <- what we had
                //   encoded rate 0.00008600161570248517 (ceil of the raw
                //     ratio's …51647…, and the value `book_offers` reports)
                //   mulRound(418487, rate, up) = 35.99055815048592 <- rippled
                // One ulp, and it is load-bearing: at …591 the achieved rate
                // TIES the taker's limit and the pass is kept; at …592 it is
                // 3 units worse and rippled discards the whole pass —
                //   Path rejected by limitQuality
                //     limit: 5773207684604483397  path q: 5773207684604483400
                // then rests the remainder. We crossed it: 9 muts against 7.
                // A fill that takes the WHOLE offer transfers the offer's own
                // amounts verbatim — rippled starts every fill from
                // `auto ofrAmt = offer.amount()` (BookStep.cpp:769) and only
                // re-prices through the quality when a limit actually BINDS
                // (`limitStepIn`/`limitStepOut`, :635-679, both guarded by
                // `if (limit < stpAmt.…)`).
                //
                // Pricing an unbound fill through the BookDirectory rate instead
                // is wrong whenever an offer's own ratio has drifted from the
                // page it sits on, and it drifts as soon as the offer is
                // partially consumed. #105828322 17A866423C61, maker 8101994315B3
                // offering 179.501643 RLUSD for 164467776 drops:
                //   own ratio 916246.6329068642 -> 164467776   <- mainnet
                //   page rate 916246.7164381367 -> 164467790.99 -> 164467790
                // 14 drops overpaid on a fully-consumed offer. rippled uses that
                // same page rate for strand ADMISSION — its FLOWDBG prints
                // `strandQ 916246.7164381367` — which is why the quality is
                // right and the payment is not.
                let full_offer = !buy_bound && me_cmp(funded, m_gives0).is_ge();
                let mut pay = if full_offer {
                    m_wants0
                } else if buy_bound {
                    // Taker-out-limited partial: limitStepOut →
                    // offer.limitOut(…, roundUp = TRUE) (BookStep.cpp:675),
                    // the lossy ceil calibrated on #105924683.
                    mul_round16_up(give, rate_me(q))
                } else {
                    // Maker-FUNDS-limited partial: the funds clamp calls
                    // offer.limitOut(…, roundUp = FALSE) (BookStep.cpp:790)
                    // — the in rounds DOWN. #106455040 530B9F6E via the
                    // full-ledger replay: maker rQ3fNyLjb's XRP runs out at
                    // 998199991 drops and mainnet prices the CNY in at
                    // 9441.640416087924 (floor of …92422…), one ulp under
                    // our ceil.
                    mul_round16_down(give, rate_me(q))
                };
                if gets_leg.xrp {
                    // Whole drops FLOOR, they do not ceil. Observed twice in
                    // libxrpl's own trace (XRPL_FFI_TRACE):
                    //
                    //   #105672435 B409D45C — maker 499CA86D funded 22.283542
                    //   of 22.928591 against TakerPays 5878826. Exact input is
                    //   5713437.2575…; rippled charges `New flow iter: 0
                    //   5713437 22.283542`. Ceiling to 5713438 makes the fill's
                    //   ACHIEVED quality worse than the taker's limit, which is
                    //   what forced the achieved-quality re-check below to be
                    //   narrowed to taker-clamped fills to avoid stranding it.
                    //
                    //   #105814446 2162E284EAB3 — the same drop. Our carry
                    //   23.19788237285018 already equals mainnet's total from
                    //   the first two offers exactly, but we spend the whole
                    //   3061631 SendMax getting there while rippled has ONE
                    //   drop left, which it spends on offer D9D7D788 for
                    //   3e-13 CNY (TakerPays 60930000 -> 60929999) to land the
                    //   full Amount. We stopped 3.1e-13 short.
                    //
                    // The direction follows WHICH SIDE bounds the fill, which
                    // is rippled's rev/fwd split. When the taker's remaining
                    // want bounds it, the input is computed FROM that output
                    // (`rev`) and must round UP so the taker pays enough — that
                    // is how the CNY dust fill costs a whole drop for 3e-13.
                    // When the MAKER's funding bounds it, the output is what is
                    // fixed and the input follows it (`fwd`), rounding DOWN —
                    // B409D45C's 5713437.2575 becomes 5713437, not 5713438.
                    //
                    // Rounding up in both directions overcharges the
                    // owner-limited fill by a drop, and that drop is
                    // load-bearing: it is the difference between reaching the
                    // last offer and stopping short of it.
                    // ...and when the TAKER's remaining want bounds the fill,
                    // that is `limitOut`, which goes through `ceilOutStrict`
                    // rather than `ceilOut` — so the drops come from the STRICT
                    // canonicaliser on the exact product, not from ceiling an
                    // already-16-digit-rounded value. The two disagree by one
                    // drop whenever the product's tail sits below a tenth of a
                    // drop, and #105924683 7CB7E834 is that drop.
                    //
                    // The maker-funding-limited branch keeps the legacy form:
                    // its direction is calibrated by #105672435 B409D45C and
                    // #105814446 2162E284EAB3, and the strict rule would round
                    // B409D45C's 5713437.2575 up to 5713438 where mainnet
                    // charges 5713437.
                    pay = if buy_bound {
                        (mul_round_drops_strict(give, rate_me(q), true), 0)
                    } else {
                        (me_rescale(pay, 0, buy_bound), 0)
                    };
                }
                // CLAMP IN — `ceilOutImpl` ends with exactly this, and it is a
                // named step in rippled's source, not a rounding artefact:
                //     Amounts result(MulRoundFunc(limit, quality.rate(), …), limit);
                //     // Clamp in
                //     if (result.in > amount.in)
                //         result.in = amount.in;
                // (Quality.cpp). An out-limited fill is priced through the
                // offer's QUALITY, and the filed 16-digit rate can price the
                // taker's slightly-smaller `out` at MORE input than the whole
                // offer asks for. The taker can never be charged more than the
                // offer's own TakerPays.
                //
                // #106297387 2B053E4F: hop 0 needs 1537.679915688961 XLM of an
                // offer holding 1537.679915688963 for 244393959 drops.
                //   own ratio  x 158936.82196564192 -> 244393958.99999968 -> 244393959
                //   filed rate x 158936.8226130111  -> 244393959.99544    -> 244393960
                // We charged the filed-rate ceiling; rippled clamps to 244393959,
                // which is the whole TakerPays — so the residual pays side is
                // ZERO and the offer dies. That one clamp is BOTH divergences:
                // a conserved 1-drop overpay (maker +1, sender −1) and the
                // offer surviving as Modified where mainnet Deletes it, taking
                // its book page D58F81E8 and owner-dir entry 171D3DDD with it.
                if buy_bound && me_cmp(pay, m_wants0).is_gt() {
                    pay = m_wants0;
                }
                // DX_PRICE: what the out-limited pricing actually produced,
                // printed BEFORE the clamp test so a fill that does not reach
                // the clamp still says why.
                if std::env::var("DX_CLAMP").is_ok() {
                    eprintln!(
                        "DX_PRICE q={q:016x} give={give:?} full_offer={full_offer} buy_bound={buy_bound} sell={sell} pay={pay:?} rem_gets={rem_gets:?} gets_xrp={}",
                        gets_leg.xrp
                    );
                }
                if me_cmp(pay, rem_gets).is_gt() {
                    pay = rem_gets;
                    taker_clamped = true;
                    // Finding 30 (#106629200 9264210055): the FUNDS-exhausting
                    // fill sizes from the gross-cap chain (STAmount adds — the
                    // stored line's own arithmetic), not the rem_gets me_sub
                    // chain; the two drift ±1 ulp apart over multi-fill
                    // crossings and rippled re-reads the line per iteration.
                    // UNRATED: `pay` takes the remainder verbatim. RATED
                    // (F50, #106679738 DA3C22D8): the remainder is GROSS and
                    // the maker's net is its DIVISION, rounded down —
                    // rippled's in-limited fill is gross-primary end to end:
                    // `stpAmt.in = limit; inLmt = mulRatio(stpAmt.in,
                    // QUALITY_ONE, transferRateIn, roundUp=false)`
                    // (BookStep.cpp limitStepIn). Tracking the net budget
                    // instead re-rounds once too often: sendMax 6.892359…225
                    // grosses to 6.902697718337578, the AMM leg takes
                    // 3.567913642620374, and mainnet's book fill is the
                    // remainder over the rate = 3.329789391629759 where the
                    // net chain said …760 — the class-B line-ULP family.
                    if !gets_leg.xrp {
                        if let Some(cap) = gets_gross_cap {
                            let verb = me_sub(cap, in_gross_spent);
                            if !me_is_zero(verb) {
                                match transfer_rate(sandbox, gets_leg)
                                    .filter(|_| taker != &gets_leg.issuer && maker != gets_leg.issuer)
                                {
                                    None => pay = verb,
                                    Some(r) => {
                                        pay = mul_ratio(verb, 1_000_000_000, r as u128, false);
                                        in_exhausted = true;
                                    }
                                }
                            }
                        }
                    }
                    // The IN-limited mirror of the out-limited pricing above.
                    // `Quality::ceil_in` is
                    //   result.out = divRound(limit, quality.rate(), asset, roundUp)
                    // with roundUp always true (Quality.cpp `ceilInImpl`;
                    // `ceil_in` passes /* roundUp */ true) — the input divided
                    // by the offer's ENCODED 16-digit rate, rounded UP, not the
                    // raw TakerGets/TakerPays ratio rounded down.
                    //
                    // Left on the raw ratio by 9426fbe for want of a failing
                    // ledger; #105843839 02A79DBAD8BD is it. That payment
                    // ripples RLUSD -> CNY -> USD -> XRP and every hop is
                    // liquidity-bound, so every hop takes this branch and each
                    // one landed an ulp short:
                    //   hop 0  rippled 193.0651944        ours 193.0651944000001
                    //   hop 1  rippled 28.55994           ours 28.55993999999999
                    //   hop 2  rippled 25940000 drops     ours 25939999
                    // One drop at the end, and it is the difference between
                    // consuming offer 7C982E04 outright (mainnet Deletes it)
                    // and leaving it resting: 11 mutations against 13.
                    //
                    // ⚠ KNOWN GAP, no failing ledger: this rounds up to 16
                    // SIGNIFICANT DIGITS and the XRP rescale below then FLOORS
                    // to whole drops, where rippled's `divRound` with an XRP
                    // asset rounds up to whole DROPS directly. #105843839 is
                    // unaffected — its 16-digit ceil already crosses the drop
                    // boundary — but a fill landing at e.g. 348830.16 drops
                    // gives 348830 here and 348831 in rippled. Do not "fix" it
                    // on inference: the floor direction in this branch is
                    // calibrated by d6f7589 against #105672435 B409D45C and
                    // #105814446 2162E284EAB3.
                    // ROUNDS DOWN. This is the INPUT-LIMITED branch, and it is
                    // BookStep.cpp:653 verbatim — not inference:
                    //   ofrAmt = offer.limitIn(ofrAmt, inLmt, /* roundUp */ false);
                    // with rippled's own rationale above it: "we can prevent
                    // order book blocking by (strictly) rounding down the
                    // ceil_in() result. By rounding down we guarantee that the
                    // quality of an offer left in the ledger is as good or
                    // better than the quality of the containing order book
                    // page. This adjustment changes transaction outcomes, so
                    // it must be made under an amendment."
                    //
                    // #105795073 1F30308A is the specimen: STX -> XRP (one AMM
                    // slice) -> EVR against a 1.002 issuer. 18899558 drops at
                    // the filed rate is 239.835773801400255.., and the EVR the
                    // taker receives decides the destination's credit:
                    //   floor 239.8357738014002 /1.002 -> 239.3570596820361  <- mainnet
                    //   ceil  239.8357738014003 /1.002 -> 239.3570596820362  <- ours
                    give = me_muldiv(pay, (1u128, 0i32), rate_me(q), false);
                    // DX_CLAMP: does a given fill actually REACH this branch?
                    // The 2026-08-08 ceil attempt assumed the twelve one-drop
                    // residuals came from here and regressed two calibrated
                    // ledgers. Establish it before touching the rounding again.
                    if std::env::var("DX_CLAMP").is_ok() {
                        let after = if pays_leg.xrp { (me_rescale(give, 0, false), 0) } else { give };
                        eprintln!(
                            "DX_CLAMP q={q:016x} pay={pay:?} give_exact={give:?} give_after={after:?} xrp_in={} lost={}",
                            pays_leg.xrp,
                            pays_leg.xrp && me_cmp(after, give).is_lt(),
                        );
                    }
                    if pays_leg.xrp {
                        // ⛔ FLIPPING THIS TO CEIL WAS TRIED 2026-08-08 AND
                        // REVERTED — and this time it was MEASURED, not
                        // inferred, so the warning above is now backed by data.
                        //
                        // The motive looked strong: DX_VALCHECK found the
                        // residual offer's TakerPays exactly ONE DROP HIGH on
                        // twelve fixtures (70135729 vs 70135728, 9233546767 vs
                        // 9233546766, 8770441575 vs 8770441574, and nine more),
                        // which is precisely what a fill one drop short leaves
                        // behind. `Quality::ceil_in` really is `divRound(…,
                        // asset, roundUp=true)`.
                        //
                        // It regresses anyway, on TWO independent signals:
                        //   • #105284280 dropped 78/78 -> 77/78 — a KEY-level
                        //     regression, the strongest evidence this repo has.
                        //   • #105091578 went from 0 value hits to TWENTY-ONE,
                        //     on a ledger that was previously perfect.
                        // Trading twelve one-drop residuals for twenty-one
                        // fresh divergences plus a broken match is not a fix.
                        //
                        // So the twelve are NOT this site, or the direction is
                        // conditional the way the `pay` rescale above is
                        // (`buy_bound` — which side bounds the fill). Whoever
                        // picks this up next: find which of the twelve reach
                        // THIS branch at all before touching the rounding again.
                        give = (me_rescale(give, 0, false), 0);
                    }
                    if me_is_zero(give) {
                        break 'dirs;
                    }
                }
                // The book gate above compares ENCODED qualities, where the
                // taker's limit and a maker can tie at 16 digits while the
                // maker is really a hair worse. What actually decides it is
                // the quality the fill ACHIEVES: once the fill is clamped to
                // the taker's remaining input and the output floored to whole
                // drops, the realised rate can land well outside the limit.
                // rippled re-checks exactly that and drops the whole path,
                // forgiving only a 1e-7 relative round-off (StrandFlow.h:720
                // "Path rejected by limitQuality").
                //
                // #105780948 101AD681: an IoC offer ties with the book head at
                // 0x5503BD5CE357AF28, but it is 2.1e-9 XMusic short of buying
                // the full 4621775 drops, so the fill floors to 4621774 and
                // realises 2.16e-7 worse than its limit. Mainnet crosses
                // nothing and returns tecKILLED; we filled it.
                // rippled judges the pass it actually flowed and discards it
                // outright when the realised quality misses the limit:
                //
                //   if (limitQuality && q < *limitQuality &&
                //       (!adjustedRemOut ||
                //        !withinRelativeDistance(q, *limitQuality, Number(1,-7))))
                //
                // StrandFlow.h:717-722. The 1e-7 forgiveness applies ONLY when
                // `limitOut()` had already REDUCED the requested output to hit
                // the limit (`adjustedRemOut`, :643-651); otherwise the
                // comparison is exact. Observed on #105945386 7EF34E79F13A,
                // where rippled prices the whole 46.96292384379671 -> 535677
                // fill and throws the pass away over ONE unit —
                //   Path rejected by limitQuality
                //     limit: 5773374545669852371  path q: 5773374545669852372
                // then rests exactly that remainder.
                //
                // This used to run only on taker-clamped fills, because the
                // input ceil overcharged an owner-limited fill by a drop and
                // the check then stranded #105672435 B409D45C. With the input
                // rounding following rev/fwd correctly that overcharge is gone,
                // so the check can be what rippled's is: always, and exact.
                if thr_judge != u64::MAX && !me_is_zero(give) && !me_is_zero(pay) {
                    if std::env::var("DX_JUDGE").is_ok() {
                        eprintln!("DX_JUDGE give={give:?} pay={pay:?} ach={:?} thr={thr_judge} reject={}",
                            rate_of_me(pay, give),
                            rate_of_me(pay, give).is_some_and(|a| a > thr_judge));
                    }
                    if let Some(ach) = rate_of_me(pay, give) {
                        if ach > thr_judge {
                            // A rejected pass ends the crossing outright —
                            // rippled logs "All strands dry" and its Total flow
                            // is whatever the ACCEPTED passes delivered. Falling
                            // through to the unanchored tail turn instead lets
                            // the pool supply the very amount the book was just
                            // refused, at a better rate, which is how
                            // #105945386 still bought its remainder (46.91474
                            // SPEPE for the same 535677 drops) after the check
                            // had correctly rejected 46.96292384379671.
                            settle_taker!();
                            return (rem_pays, rem_gets, crossed);
                        }
                    }
                }
                // A fill at a NEW quality level ends the previous iteration:
                // settle its taker debit before this level starts
                // accumulating (see the boundary note at `taker_accs`).
                if acc_level != Some(q) {
                    settle_taker!();
                    taker_accs = ((0, 0), (0, 0));
                    acc_level = Some(q);
                }
                // Maker debited per fill; the taker's credit accumulates for
                // the per-level settlement (see `taker_accs` above). The
                // IOU split is exactly `move_leg`'s own two `line_adjust`s
                // pulled apart in time.
                if pays_leg.xrp {
                    move_leg(sandbox, &maker, beneficiary, pays_leg, give);
                } else {
                    line_adjust(sandbox, &maker, pays_leg, give, false);
                    taker_accs.0 = stamount_signed_add(false, taker_accs.0, false, give).1;
                }
                // The taker pays the INPUT issuer's rate on top of what the
                // maker receives, and the issuer destroys the difference:
                //   trIn = redeems(prevStepDir) ? rate(book_.in, strandDst_) : parity
                //   stpAmt.in = mulRatio(ofrAmt.in, ofrInRate, QUALITY_ONE, roundUp)
                // (BookStep.cpp:352, :770). Same rule `94a028e` applied to the
                // bridged walk, on the DIRECT walk.
                //
                // ⚠ ADDITIVE ONLY — sizing is untouched, and must be. Three
                // attempts on 2026-08-11 divided a budget by this rate instead
                // (call site, direct walk, both bridged clamps) and every one
                // re-sized fills that were already correct; one regressed
                // #105887283 to 87/88 at KEY level. TakerGets bounds the NET.
                //
                // #105877543 435A9AEB is the specimen this walk was missing
                // when the earlier attempt was reverted for want of one: a
                // tfSell|tfIoC selling 446 SOLO direct to XRP, issuer rate
                // 1.0001. Mainnet debits the taker 446.0446 and credits the
                // maker 446; we debited 446 flat, leaving the taker 0.0446 rich.
                //
                // Charged as ONE debit of the gross rather than a net debit
                // followed by a fee adjustment — see `move_leg_gross`.
                {
                    let r = transfer_rate(sandbox, gets_leg)
                        .filter(|_| taker != &gets_leg.issuer && maker != gets_leg.issuer);
                    if gets_leg.xrp {
                        move_leg_gross(sandbox, taker, &maker, gets_leg, pay, gross_in(r, pay));
                    } else {
                        // Maker credited the NET per fill; the taker's GROSS
                        // debit accumulates (rippled grosses per offer —
                        // `stpAmt.in = mulRatio(ofrAmt.in, trIn, …)` — and
                        // sums the grossed values). The fill that EXHAUSTS the
                        // walk's net avail is gross-primary: it takes the
                        // remaining gross cap verbatim (see `gets_gross_cap`).
                        let g = match gets_gross_cap {
                            Some(cap) if in_exhausted || !me_cmp(pay, rem_gets).is_lt() => {
                                me_sub(cap, in_gross_spent)
                            }
                            _ => gross_in(r, pay),
                        };
                        in_gross_spent = stamount_signed_add(false, in_gross_spent, false, g).1;
                        line_adjust(sandbox, &maker, gets_leg, pay, true);
                        taker_accs.1 = stamount_signed_add(false, taker_accs.1, false, g).1;
                    }
                }
                if std::env::var("DX_FILL").is_ok() {
                    eprintln!("DX_FILL give={give:?} pay={pay:?} rem_pays={rem_pays:?} rem_gets={rem_gets:?}");
                }
                rem_pays = me_sub(rem_pays, give);
                rem_gets = me_sub(rem_gets, pay);
                if in_exhausted {
                    // The gross budget is spent to the last unit; the net
                    // chain's leftover is the division's round-down dust,
                    // which rippled — tracking gross — never sees. Chasing
                    // it would buy a phantom ulp from the next offer.
                    rem_gets = (0, 0);
                }
                if fold_rem {
                    // The iteration's actualOut accumulates STAmount-style
                    // (one 16-digit add per fill on the level).
                    level_out_acc = stamount_signed_add(false, level_out_acc, false, give).1;
                }
                crossed += 1;
                level_crossed = true;
                self_anchor_q = None;
                sweep_admitted = None;
                // `TOffer::fully_consumed()` is `amount().in == 0 || amount().out
                // == 0` — EITHER side, not just the gets side we were testing.
                // An out-limited fill clamped to the offer's whole TakerPays
                // drives the pays residual to zero while a dust remainder still
                // sits on the gets side, and rippled removes that offer.
                // #106297387 2B053E4F leaves TakerGets 2e-12 against TakerPays
                // 0, and mainnet Deletes it.
                let res_gets = offer_residual(m_gives0, give);
                let res_pays = offer_residual(m_wants0, pay);
                let consumed = me_cmp(give, m_gives0).is_ge()
                    || me_cmp(give, funded).is_ge()
                    || me_is_zero(res_gets)
                    || me_is_zero(res_pays);
                if consumed {
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                } else if me_is_zero(give) && me_is_zero(pay) {
                    // Finding 40, autobridge twin: zero fill = examined, not
                    // moved — no rewrite, no phantom threading stamp.
                } else {
                    let mut off2 = offer.clone();
                    off2["TakerGets"] = me_amount_json(&offer["TakerGets"], res_gets);
                    off2["TakerPays"] = me_amount_json(&offer["TakerPays"], res_pays);
                    put_json(sandbox, okey, &off2);
                }
                // What the walk actually TOOK from this offer. `DX_WALK` above
                // prints each offer as ENCOUNTERED; this prints the fill, which
                // is what a divergence in walk DEPTH turns on — "deleted here,
                // modified there" is the boundary offer where two walks part.
                if std::env::var("DX_WALK").is_ok() {
                    eprintln!(
                        "DX_FILL okey={} maker={} give={give:?} pay={pay:?} gives0={m_gives0:?} wants0={m_wants0:?} funded={funded:?} consumed={consumed} rem_pays={rem_pays:?} rem_gets={rem_gets:?}",
                        hex::encode(okey.0),
                        hex::encode(maker),
                    );
                }
                if done(rem_pays, rem_gets) {
                    // rippled does not stop at a satisfied fill. Its reverse
                    // pass returns TRUE from the offer callback whenever the
                    // step's output fits inside what is still wanted —
                    // "return true b/c even if the payment is satisfied, we
                    // need to consume the offer" (BookStep.cpp:1036) — so
                    // `while (offers.step())` runs once more and reaps the
                    // dead offers sitting behind the one just consumed.
                    // Only the `limitStepOut` branch, where the offer had to
                    // be trimmed to the REMAINING OUTPUT, returns
                    // `offer.fullyConsumed()` (BookStep.cpp:1062) and ends the
                    // walk — that is exactly our `buy_bound` fill.
                    //
                    // #105778999 6B2A11B3: a tfPartialPayment with a sentinel
                    // Amount, so nothing ever trims to remaining output. We
                    // spent all 140 XRP against the page's first offer
                    // (FFF5869C, the only funded one) and stopped; mainnet
                    // consumed the same offer for the same value, stepped, and
                    // reaped the three unfunded spam offers behind it —
                    // 3 offers Deleted, 2 maker roots + 2 owner dirs + the
                    // book page Modified, and no RippleState among them.
                    // 5v15 with those 8 missing and nothing extra.
                    // …and `offer.fullyConsumed()` is that branch's CONTINUE
                    // flag: a fill trimmed to the remaining output that ALSO
                    // exhausts the offer keeps the stream stepping — mainnet
                    // reaps the dead level behind it. #106030404 50E1F824:
                    // the tip covers the full 28.59217928509999 want with its
                    // whole 26366653-drop side (consumed), and rippled steps
                    // on to reap rGodbj1's zero-funded offer one level back
                    // (offer + page deleted, root + dir modified, 7v11 for
                    // us). Only a trimmed fill that leaves the offer alive
                    // ends the walk here.
                    if buy_bound && !consumed {
                        break 'dirs;
                    }
                    trailing = true;
                    continue;
                }
            }
            let next = page.get("IndexNext").map(dirnum).unwrap_or(0);
            if next == 0 {
                break;
            }
            page_key_h = keylet::dir_page_key(&dk, next);
        }
        if single_pass && (rem_pays != level_pays_in || rem_gets != level_gets_in) {
            // The pass is over — but rippled's stream keeps stepping and
            // REAPS the dead offers it lands on before it stops
            // (BookStep.cpp:1062 returns fullyConsumed; #106030404 50E1F824:
            // the level behind the consumed tip holds only rGodbj1's
            // zero-funded offer, and mainnet deletes offer + page and
            // modifies root + owner dir within the SAME iteration). Sweep in
            // trailing mode — the 'dirs trailing branch reaps to the first
            // LIVE offer and breaks — then return below, before the tail AMM
            // turn (a single pass must not take pool liquidity the level
            // boundary already excluded).
            trailing = true;
            continue;
        }
        // Level boundary = iteration boundary: bank the level's actualOut and
        // re-derive the remainder from the fold (StrandFlow.h:639-642).
        if fold_rem && !me_is_zero(level_out_acc) {
            saved_level_outs.push(level_out_acc);
            level_out_acc = (0, 0);
            rem_pays = me_sub(out_req0, fold16(&mut saved_level_outs));
        }
        prev_level_crossed = level_crossed;
    }
    // A single pass whose level boundary tripped exits before the tail turn.
    if single_pass && trailing {
        settle_taker!();
        return (rem_pays, rem_gets, crossed);
    }
    // Final AMM turn once the book is exhausted (maxOffer sizing).
    if let Some(a) = &amm {
        // For a CROSSING with no remembered anchor, the strand must first be
        // ADMITTED. rippled's next pass anchors tryAMM on the residual raw
        // tip; when that tip sits WITHIN the inflated limitQuality
        // (`threshold_self`), the anchored AMM offer's quality IS the tip,
        // and grossed by trIn it misses the limit exactly because the tip is
        // beyond the NET threshold — which is why the walk stopped there.
        // #106225714 0FE0E3C5: tip 1.02905e-6 ∈ (net 1.0290100e-6, gross
        // 1.0305435e-6] → `ub strand 0 = 1.030594e-6`, `admitted false`,
        // the remainder rests. A tip beyond even the INFLATED limit — or no
        // tip at all — is the maxOffer branch: ub = spot × trIn, which is
        // the same comparison consume()'s fee-inclusive spot gate makes
        // (#106211626 D322E925 fills exactly there; #106295504's receipt is
        // the same branch). The interval is empty without a gets-side
        // transfer rate, so unrated crossings are untouched. Payments carry
        // no limitQuality and skip this.
        let tail_admitted = !(offer_crossing
            && threshold_self != 0
            && threshold_self != u64::MAX
            && self_anchor_q.is_none())
            || match residual_q {
                Some(q) => !(q > threshold && q <= threshold_self),
                None => true,
            };
        if tail_admitted && !done(rem_pays, rem_gets) {
            if std::env::var("DX_AMM").is_ok() {
                eprintln!("DX_AMM site=direct-tail");
            }
            // Tail turn = past the last level: close the open group first
            // (see the level-boundary note at `taker_accs`).
            settle_taker!();
            taker_accs = ((0, 0), (0, 0));
            acc_level = None;
            let (rp, rg, used) = amm_turn(
                amm_fib.as_deref_mut(), sandbox, a, taker, beneficiary,
                benef_net.map(|(r, na)| (r, na, ask0)),
                gets_gross_cap.map(|c| me_sub(c, in_gross_spent)), rem_pays, rem_gets,
                pays_leg, gets_leg, threshold, sell,
                // A remembered self-offer within the limit is still the tip
                // rippled's tail pass anchors on (see self_anchor_q above);
                // beyond the limit rippled anchors nothing (#106295504).
                self_anchor_q.filter(|qs| *qs <= threshold),
                pay_in_rate,
            );
            if used {
                let slice_net = me_sub(rem_gets, rg);
                let g = match gets_gross_cap {
                    Some(cap) if me_is_zero(rg) => me_sub(cap, in_gross_spent),
                    _ => gross_in(pay_in_rate, slice_net),
                };
                in_gross_spent = stamount_signed_add(false, in_gross_spent, false, g).1;
            }
            // Tail slice = its own iteration too (see the level-head turn).
            if fold_rem && used {
                if !me_is_zero(level_out_acc) {
                    saved_level_outs.push(level_out_acc);
                    level_out_acc = (0, 0);
                }
                let slice_out = me_sub(rem_pays, rp);
                if !me_is_zero(slice_out) {
                    saved_level_outs.push(slice_out);
                }
                rem_pays = me_sub(out_req0, fold16(&mut saved_level_outs));
            } else {
                rem_pays = rp;
            }
            if used {
                sweep_admitted = None;
            }
            rem_gets = rg;
            crossed += used as u32;
        }
    }
    // The judge. Realised quality is measured on the NET spent, against the NET
    // threshold — rippled measures the GROSS against a transfer-rate inflated
    // `limitQuality` (OfferCreate.cpp:378-392), and since both sides scale by
    // the same rate the comparison is identical. That is also why substituting
    // the inflated limit as our crossing threshold was wrong: it inflates one
    // side of a comparison whose other side is net.
    if let Some(snap) = cross_snap {
        let spent = me_sub(entry_gets, rem_gets);
        let got = me_sub(entry_pays, rem_pays);
        // A miss inside 1e-7 relative is FORGIVEN, exactly as the bridged judge
        // forgives it: "limitOut() finds output to generate exact requested
        // limitQuality. But the actual limit quality might be slightly off due
        // to the round off" (StrandFlow.h:718), compared through
        // `withinRelativeDistance` (AMMHelpers.h:112-121) against the LARGER
        // rate — the worse one, which is the realised `q`.
        //
        // ⚠ Judging exactly cost four transactions and six value hits at v104:
        // #105035381 2C4DF181 is a tfSell whose pass misses by 1.66e-8 and which
        // mainnet FILLS; we killed it (3 muts against 6). #105717461, #105798519
        // and #105843839 are the same marginal shape. The specimen this judge
        // exists for misses by 4.1e-3 — five orders out — so the tolerance costs
        // it nothing.
        //
        // rippled gates the forgiveness on `limitOut` having actually reduced
        // the output, which the aggregate has no way to know. Applying it
        // unconditionally is the looser reading; the nearest specimen either way
        // is #106137477, resting on a 1.3e-5 miss that stays rejected regardless.
        let missed = crossed > 0
            && threshold != u64::MAX
            && !me_is_zero(got)
            && rate_of_me(spent, got).is_some_and(|q| {
                q > threshold && {
                    let (a, b) = (rate_me(q), rate_me(threshold));
                    !me_cmp(me_muldiv(me_sub(a, b), (10_000_000, 0), a, false), (1, 0)).is_lt()
                }
            });
        if missed {
            if std::env::var("DX_FOK").is_ok() {
                eprintln!(
                    "DX_PASS rejected by limitQuality: spent={spent:?} got={got:?} q={:?} threshold={threshold:016x}",
                    rate_of_me(spent, got));
            }
            sandbox.restore_snapshot(snap);
            // A rejected pass still leaves the stale-offer cleanup standing —
            // rippled applies its `removableOffers` to the cancel sandbox too
            // (OfferCreate.cpp:460), so the reap must survive the rollback that
            // just restored them.
            for okey in stale.iter() {
                let Some(off) = json_at(sandbox, okey) else { continue };
                let Some(maker) = off.get("Account").and_then(|v| v.as_str()).and_then(decode20)
                else {
                    continue;
                };
                delete_maker_offer(sandbox, okey, &off, &maker);
            }
            return (entry_pays, entry_gets, 0);
        }
    }
    // Per-pass taker settlement — AFTER the judge: a rolled-back pass never
    // sees these writes (the snapshot restore above returns without them).
    settle_taker!();
    (rem_pays, rem_gets, crossed)
}

/// One AMM turn. With no fib state this is the single-path `maxOffer` sizing
/// `consume` has always done. With fib state — a multi-strand payment, where
/// rippled's `multiPath()` is true — the pool offers ONE fib slice off its
/// initial balances and the flow-wide counter advances only when the slice is
/// actually taken, mirroring `ammContext.update()`.
#[allow(clippy::too_many_arguments)]
fn amm_turn(
    fib: Option<&mut AmmFib>,
    sandbox: &mut Sandbox,
    a: &crate::tx::amm_swap::Amm,
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    // See `settle_slice`: the strand-tail net-credit rule.
    benef_net: Option<(u64, Me, Me)>,
    // Remaining GROSS in-cap (see `gets_gross_cap` on the walk): a slice
    // that exhausts rem_gets debits this verbatim.
    in_gross_cap: Option<Me>,
    rem_pays: Me,
    rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    sell: bool,
    clob: Option<u64>,
    // Payment-mode IN-side transfer rate (see consume_fib). Crossing = None.
    in_gross_rate: Option<u64>,
) -> (Me, Me, bool) {
    let Some(f) = fib else {
        return crate::tx::amm_swap::consume(
            sandbox, a, taker, beneficiary, benef_net, in_gross_cap, rem_pays, rem_gets, pays_leg, gets_leg, threshold, sell, clob,
            in_gross_rate, false,
        );
    };
    let init = match f.init.get(&a.account) {
        Some(v) => *v,
        None => {
            let v = crate::tx::amm_swap::pool_balances(sandbox, a, pays_leg, gets_leg);
            f.init.insert(a.account, v);
            v
        }
    };
    let r = crate::tx::amm_swap::consume_fib(
        sandbox, a, taker, beneficiary, benef_net, in_gross_cap, rem_pays, rem_gets, pays_leg, gets_leg, threshold, sell,
        init, f.iters, clob.map(rate_me), in_gross_rate,
    );
    if r.2 {
        f.used = true;
    }
    r
}

/// Cross with the taker as its own beneficiary (OfferCreate semantics).
pub(crate) fn cross_engine(
    taker: &[u8; 20],
    rem_pays: Me,
    rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    threshold_self: u64,
    sell: bool,
    domain: Option<&Hash256>,
    sandbox: &mut Sandbox,
    stale: &mut Vec<Hash256>,
) -> (Me, Me, u32) {
    // FlowCross always builds both the direct and the XRP-bridged strand, so
    // offer crossing is multi-path by construction.
    cross_engine_to(taker, taker, rem_pays, rem_gets, pays_leg, gets_leg, threshold, threshold_self, sell, true, false, None, domain, sandbox, stale)
}

pub struct OfferCreateTransactor;

impl Transactor for OfferCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "OfferCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("TakerPays").is_none() || tx.fields.get("TakerGets").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }

        // rippled CreateOffer::preclaim: an offer is unfunded when
        // accountFunds(TakerGets) <= 0 — for XRP that is balance minus
        // reserve, NOT the full sell amount (partially funded offers still
        // cross). Returns tecUNFUNDED_OFFER, not the generic tecUNFUNDED.
        // IOU-side funding is enforced by available() in do_apply.
        if let Some(leg) = leg_of(&tx.fields["TakerGets"]) {
            if me_is_zero(available(sandbox, &tx.account, &leg)) {
                return TxResult::UnfundedOffer;
            }
        }

        // Expiry is checked AFTER funding. rippled's OfferCreate::preclaim
        // (:171) runs terNO_ACCOUNT -> checkGlobalFrozen x2 -> accountFunds ->
        // tecUNFUNDED_OFFER -> temBAD_SEQUENCE -> hasExpired -> tecEXPIRED ->
        // checkAcceptAsset, so an offer that is BOTH expired and unfunded
        // answers tecUNFUNDED_OFFER.
        //
        // d753d7a placed this first, reading OfferCreate.cpp:224-229's "it
        // saves us a call to checkAcceptAsset and possible false negative" as
        // "before funding". It is not — checkAcceptAsset comes AFTER funding,
        // so that comment only places the expiry check ahead of checkAcceptAsset.
        // #105950082 000702037C38 and C79FCB34C1B7 are both expired AND
        // unfunded: mainnet says tecUNFUNDED_OFFER, we said tecEXPIRED.
        //
        // hasExpired is inclusive, `parentCloseTime() >= Expiration`
        // (View.cpp:48-54); the base ledger's close time IS the parent close
        // time of the ledger being replayed. #105887283 17075103474C:
        // Expiration 838475074 against parent close 838475151.
        if let Some(exp) = tx.fields.get("Expiration").and_then(|v| v.as_u64()) {
            if sandbox.base().header.close_time as u64 >= exp {
                return TxResult::Expired;
            }
        }

        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let tp_json = tx.fields["TakerPays"].clone();
        let tg_json = tx.fields["TakerGets"].clone();
        let (Some(tp0), Some(tg0)) =
            (keylet::amount_mant_exp(&tp_json), keylet::amount_mant_exp(&tg_json))
        else {
            return TxResult::Malformed;
        };
        let (Some(pays_leg), Some(gets_leg)) = (leg_of(&tp_json), leg_of(&tg_json)) else {
            return TxResult::Malformed;
        };
        if tp0.0 == 0 || tg0.0 == 0 {
            return TxResult::Malformed;
        }
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let sell = flags & 0x0008_0000 != 0;
        let ioc = flags & 0x0002_0000 != 0;
        let fok = flags & 0x0004_0000 != 0;

        // The kill-path snapshot is taken BEFORE the cancel-and-replace: the
        // cancellation is transactional state like any other, and a tec
        // discards it. #106455085 6BCE59E0/32206C22 (full-ledger replay,
        // ticketed IoCs with OfferSequence): both tecKILLED, and mainnet's
        // meta keeps the old offers (105193242/105193222) alive — only the
        // walk's stale-offer reap survives a kill (OfferCreate.cpp:460), not
        // the cancel. With the snapshot after the cancel, the Killed arm's
        // restore preserved the deletion and we reaped two resting offers,
        // their book pages, and the maker's OwnerCount.
        let snap = sandbox.snapshot();

        // Cancel-and-replace: an OfferSequence names a prior offer to cancel
        // before crossing/placing (rippled does this first, unconditionally —
        // but inside the region a tec rolls back, see above).
        if let Some(old_seq) = tx.fields.get("OfferSequence").and_then(|v| v.as_u64()) {
            let old_key = keylet::offer_key(&tx.account, old_seq as u32);
            if let Some(old) = json_at(sandbox, &old_key) {
                delete_maker_offer(sandbox, &old_key, &old, &tx.account);
            }
        }

        // Issuer tick size rounds the requested rate up to N significant
        // digits and re-derives the non-exact side — BEFORE crossing, so the
        // crossing and the placed remainder both use the rounded amounts
        // (rippled CreateOffer::applyGuts).
        let tick = tick_size_for(sandbox, &pays_leg, &gets_leg);
        let (tp0, tg0) = apply_tick_size(tp0, tg0, sell, tick, pays_leg.xrp, gets_leg.xrp);
        if tp0.0 == 0 || tg0.0 == 0 {
            return TxResult::Success; // rounded to nothing: fee-only
        }

        // Taker funding: the offer can only sell what the account actually
        // holds of the TakerGets asset (issuers mint freely; XRP is balance
        // minus reserve; IOU is the trust-line holding). Holding none is an
        // unfunded offer — fee-only, nothing crossed or placed — no matter
        // how willing a counterparty is (rippled CreateOffer accountFunds).
        if me_is_zero(available(sandbox, &tx.account, &gets_leg)) {
            return TxResult::UnfundedOffer;
        }

        // A DomainID (XLS-80) scopes both crossing and placement to the
        // domain's book.
        let domain: Option<Hash256> = tx
            .fields
            .get("DomainID")
            .and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s).ok())
            .filter(|b| b.len() == 32)
            .map(|b| {
                let mut d = [0u8; 32];
                d.copy_from_slice(&b);
                Hash256(d)
            });

        // Cross against the inverse book while the maker's rate is within the
        // taker's limit price (threshold = quality with the sides swapped).
        //
        let mut threshold = rate_of_me(tg0, tp0).unwrap_or(0);
        // rippled's `limitQuality` is NOT priced off TakerGets: when the gateway
        // of the side we SPEND charges a transfer fee it takes its cut "without
        // any special consent from the offer taker", so the input is scaled up
        // by the rate and the limit priced off that — `sendMax =
        // multiplyRound(takerAmount.in, gatewayXferRate, ..., /*roundUp*/ true)`
        // then `Quality threshold{takerAmount.out, sendMax}`, OfferCreate.cpp
        // :345-364, "Payment flow code compares quality after the transfer rate
        // is included". A fee makes the taker's limit MORE permissive.
        //
        // We carry it as a SECOND value read only by the self-cross gate, not
        // as the crossing limit. Substituting it wholesale regressed two
        // ledgers, both by over-consuming the POOL rather than the book:
        // #105933892 141D8C8F796B (7 muts vs mainnet's 4, three extra Modified,
        // book `cross=false` throughout) and #105954798 6283AA245088. rippled
        // sizes an AMM offer against `lobQuality` and gates it afterwards
        // (BookStep.cpp:845-851, 479-486), where `amm_swap::consume` sizes
        // AGAINST the limit it is handed — an approximation its own comment
        // already flags as "a MODEL of the pass, not a port". Widening that
        // input therefore buys extra pool liquidity rather than reach up the
        // book. Feeding the book gates the inflated value is the correct next
        // step, but it belongs with the AMM sizing work, not here.
        let xfer_in = match transfer_rate(sandbox, &gets_leg) {
            Some(r) if tx.account != gets_leg.issuer => Some(r),
            _ => None,
        };
        let send_max = match xfer_in {
            Some(r) => me_muldiv(tg0, (r as u128, 0), (1_000_000_000, 0), true),
            _ => tg0,
        };
        let mut threshold_self = rate_of_me(send_max, tp0).unwrap_or(0);
        // tfPassive: a passive offer crosses only STRICTLY better makers and
        // rests behind equal-quality ones instead of consuming them (rippled
        // OfferCreate.cpp:396 `++threshold`). Our rate encoding is inverted
        // (lower = better) and monotone in the u64, so tighten by one ULP: an
        // equal-quality maker then ties the pre-decrement value and fails the
        // `q <= threshold` / maker-rate gates, while strictly-better makers keep
        // crossing. #105807256 (tfSell|tfPassive) rests where we consumed the
        // equal-priced makers (12v4); #105803327 (tfPassive) crossed too much
        // (10v8).
        if flags & 0x0001_0000 != 0 {
            threshold = threshold.saturating_sub(1);
            threshold_self = threshold_self.saturating_sub(1);
        }
        // Offers the walk removes as STALE — expired, unfunded, empty or the
        // taker's own — are not part of the crossing: rippled applies its
        // removableOffers to the cancel sandbox as well, so the cleanup
        // survives a kill that rolls every fill back (OfferCreate.cpp:460).
        let mut stale: Vec<Hash256> = Vec::new();
        // Crossing spends only what the account actually HOLDS, which can be
        // far less than TakerGets: rippled clamps the crossing input to
        // accountFunds (XRP balance minus reserve) — "Don't send more than our
        // balance", OfferCreate.cpp:399-401 — while TakerGets still bounds
        // what may REST. The threshold above is computed from the unclamped
        // amounts, as rippled builds it before the clamp (OfferCreate.cpp:392).
        //
        // ⚠ The balance bounds the GROSS, not the net. rippled clamps `sendMax`
        // — already transfer-rate inflated at this point — so the comparison is
        // `TakerGets x rate > balance` and the surviving budget is a GROSS one
        // the flow then divides down. `accountFunds` is what the taker parts
        // with; the maker receives that over the rate, because the gateway
        // "takes its cut without any special consent from the offer taker".
        // Clamping the NET to the raw balance instead makes the taker part with
        // `balance x rate` — more of the currency than it holds — and buys the
        // fee's worth of extra output.
        //
        // #106347648 622A7DD2 is the specimen (and the same pair repeats every
        // ledger from a bot): 0.5156870490053416 SGB held against a 1.003
        // issuer, selling for 4762 drops. rippled sends the whole balance as
        // GROSS, the maker receives 0.514144615159862 and pays 474 drops. We
        // fed 0.5156870490053416 as the net and took 476.
        //
        // ⚠⚠ This is NOT the division the 2026-08-11 attempts made and the note
        // above forbids. TakerGets bounds the NET and is still untouched; only
        // the BALANCE, which bounds the gross, is divided down.
        let avail = available(sandbox, &tx.account, &gets_leg);
        let underfunded = me_cmp(avail, send_max) == std::cmp::Ordering::Less;
        let tg_cross = if underfunded {
            // Round DOWN: the taker must not part with more than it holds.
            match xfer_in {
                Some(r) => me_muldiv(avail, (1_000_000_000, 0), (r as u128, 0), false),
                _ => avail,
            }
        } else {
            tg0
        };
        // The surviving budget is a GROSS one — thread it as the walk's
        // gross cap so an exhausting fill or slice debits `cap − spent`
        // VERBATIM (the a12ffd5 gross-primary rule; rippled's clamped
        // sendMax IS the crossing's remaining-in). Without it the drain
        // round-trips balance/rate×rate and overshoots by an ulp:
        // #106455229 7D1380A7 sells its whole 1.58026353300976e-5 BTC line
        // through the pool — mainnet lands canonical zero, the re-gross
        // left +1e-20. Underfunded+rated only: the unclamped path grosses
        // the same product on both sides and never splits.
        // Finding 30 (#106629200 9264210055): armed for UNRATED underfunded
        // too — not for a gross/net split (none exists unrated) but because
        // the funds-EXHAUSTING fill must take `cap − spent` VERBATIM: the
        // walk's rem_gets me_sub chain and the line's STAmount adds round
        // through different sequences and drift ±1 ulp over a multi-fill
        // crossing (slice 5 pays …443496 off the chain where the stored line
        // remainder — rippled's per-iteration re-read — is …443500, and the
        // maker's owed line must close to EXACT ZERO).
        // Fully funded and RATED, the cap is armed too: flowCross grosses
        // sendMax UNCONDITIONALLY — `sendMax = multiplyRound(takerAmount.in,
        // gatewayXferRate, roundUp=true)` (OfferCreate.cpp:354) — and flow()
        // consumes THAT budget, so the in-limited final fill takes the gross
        // remainder verbatim and derives its net by division. `send_max`
        // above is exactly that product. #106679738 DA3C22D8 (F50).
        let gross_cap = if underfunded {
            Some(avail)
        } else {
            xfer_in.map(|_| send_max)
        };
        let (rem_pays, rem_gets_cross, crossed) = cross_engine_to_net(
            &tx.account, &tx.account, tp0, tg_cross, &pays_leg, &gets_leg, threshold,
            threshold_self, sell, true, false, None, domain.as_ref(), None, gross_cap,
            sandbox, &mut stale,
        );
        // Re-express the leftover against the ORIGINAL TakerGets: only the
        // funded part could be spent, but the whole unspent remainder rests.
        // Left exactly as returned when the clamp did not bite, so the fully
        // funded path keeps its value rather than re-deriving it.
        let rem_gets = if underfunded {
            me_norm(me_sub(tg0, me_sub(tg_cross, rem_gets_cross)))
        } else {
            rem_gets_cross
        };
        // Re-run the stale removals after a rollback restores them.
        let reap = |sandbox: &mut Sandbox, stale: &[Hash256]| {
            for okey in stale {
                let Some(off) = json_at(sandbox, okey) else { continue };
                let Some(maker) = off.get("Account").and_then(|v| v.as_str()).and_then(decode20)
                else { continue };
                delete_maker_offer(sandbox, okey, &off, &maker);
            }
        };

        if std::env::var("DX_FOK").is_ok() {
            eprintln!(
                "DX_FOK crossed={crossed} underfunded={underfunded} avail_after={:?} rem_pays={rem_pays:?} rem_gets={rem_gets:?} rem_gets_cross={rem_gets_cross:?} tg_cross={tg_cross:?} fok={fok} sell={sell}",
                available(sandbox, &tx.account, &gets_leg),
            );
        }
        // A tfSell offer is complete when its INPUT is spent, and the taker's
        // FUNDING bounds that input — so judge the sell side against the funded
        // budget (`rem_gets_cross`), not against the original TakerGets that
        // `rem_gets` was re-expanded to just above.
        //
        // rippled calls `flow(..., partialPayment = !tfFillOrKill, ...)`, so a
        // FillOrKill offer either completes or moves nothing. For a BUY the
        // completion test is the OUT side — TakerPays must be delivered in full
        // — and for a SELL it is the IN side. When crossing exhausts the
        // account, `flowCross` clears BOTH sides of the remainder ("If offer
        // crossing exhausted the account's funds don't create the offer") and
        // `placeOffer.in == 0` then reads as "Offer fully crossed!", returning
        // tesSUCCESS BEFORE FillOrKill is considered (OfferCreate.cpp).
        //
        // #106310137 shows both halves in one ledger, which is what pins the
        // distinction: three plain-FoK offers at indexes 5, 6 and 8 are KILLED
        // by mainnet, while `2441A597` at index 30 — the only one carrying
        // tfSell — SUCCEEDS. It sells 911939809 drops, its entire spendable
        // balance, for 178632521.62303 ATM, and our own fill already matched
        // that to the digit; only the completion test disagreed.
        //
        // ⚠ An earlier attempt generalised this to "crossing drained the taker
        // ⇒ fully crossed" for EVERY offer. Measured 447/449 -> 441/449 and was
        // reverted: it let those three plain-FoK offers succeed, and the
        // liquidity they wrongly consumed then starved this very transaction,
        // which is why the target stayed tecKILLED and the fix looked inert.
        let filled = if sell { me_is_zero(rem_gets_cross) } else { me_is_zero(rem_pays) };
        if fok && !filled {
            // FillOrKill not fully filled: nothing survives but the fee and
            // the stale-offer cleanup.
            sandbox.restore_snapshot(snap);
            reap(sandbox, &stale);
            return TxResult::Killed;
        }
        if ioc {
            if crossed == 0 {
                sandbox.restore_snapshot(snap);
                reap(sandbox, &stale);
                return TxResult::Killed;
            }
            return TxResult::Success; // keep fills, never place
        }
        if me_is_zero(rem_pays) || me_is_zero(rem_gets) {
            return TxResult::Success; // fully consumed
        }

        if crossed == 0 {
            // Pure-placement refusals (mainnet meta: fee-only AccountRoot).
            if me_is_zero(available(sandbox, &tx.account, &gets_leg)) {
                return TxResult::UnfundedOffer;
            }
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(a) = json_at(sandbox, &acct_key) {
                let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                let oc = a["OwnerCount"].as_u64().unwrap_or(0) as u128;
                // rippled compares `preFeeBalance_` — the balance as of BEFORE
                // this transaction's fee was taken (OfferCreate.cpp:834-841).
                // `do_apply` runs after apply_common has already deducted it,
                // so add it back; otherwise the account reads short by exactly
                // one fee at the boundary.
                // #105845719 90E12EF294C9: post-fee 2799988 vs reserve 2800000
                // (12 drops, one fee) — mainnet places the offer, and the same
                // account's later OfferCreate at index 46 in that ledger *does*
                // get tecINSUF_RESERVE_OFFER, so this is a genuine off-by-fee
                // rather than a wrong reserve.
                if bal + (tx.fee as u128) // pre-fee balance
                    < XRP_RESERVE_BASE + XRP_RESERVE_INC * (oc + 1)
                {
                    return TxResult::InsufReserveOffer;
                }
            }
        }

        // "If offer crossing exhausted the account's funds don't create the
        // offer" (OfferCreate.cpp:479-484): rippled clears the residual rather
        // than resting something the account cannot begin to fund. The result
        // stays tesSUCCESS; only the offer is dropped. This is the companion
        // to the pre-crossing `accountFunds` clamp above (`1de6d87`) — that one
        // bounds what may be SPENT, this one bounds what may REST.
        //
        // The test is on the CLAMPED input: `rem_gets_cross` is what survives
        // of `accountFunds`, so exhausting it means the crossing consumed
        // everything the account had. Reading the account again here instead
        // would be ambiguous, because deleting a self-crossed offer has already
        // released a 0.2 XRP reserve increment by this point.
        //
        // #105803327 09DA2A02, a tfPassive buy of 9.905704 RLUSD for 11.013387
        // XRP: the account held 10.937718 XRP at OwnerCount 8, so it could
        // spend 8337706 drops, and the whole amount would have cost 8921861.
        // It spent every spendable drop and landed on exactly 2600000 — its
        // 1 + 8×0.2 XRP reserve. Mainnet placed nothing; we rested an offer
        // plus its book page it never had (10v8, both extra, nothing missing —
        // our crossing itself is byte-identical to mainnet's).
        if std::env::var("DX_PLACE").is_ok() {
            let a = json_at(sandbox, &keylet::account_root_key(&tx.account));
            eprintln!(
                "DX_PLACE crossed={crossed} rem_pays={rem_pays:?} rem_gets={rem_gets:?} avail={:?} bal={:?} oc={:?}",
                available(sandbox, &tx.account, &gets_leg),
                a.as_ref().map(|x| x["Balance"].clone()),
                a.as_ref().map(|x| x["OwnerCount"].clone()),
            );
        }
        if underfunded && me_is_zero(rem_gets_cross) {
            return TxResult::Success;
        }

        // Place the remainder at the taker's ORIGINAL quality (rippled
        // preserves the price for partial fills).
        //
        // ...and that has to hold for the AMOUNTS, not just the book page.
        // Subtracting the fill actually delivered degrades the residual's price
        // whenever the crossing filled BETTER than the offer's own quality —
        // which is the normal case, since a taker crosses at the book's price,
        // not at its own limit. The residual would then offer takers a better
        // deal than its owner ever signed, sitting on a page (below, from
        // `tp0`/`tg0`) whose quality its own ratio contradicts.
        //
        // rippled charges the offer at ITS OWN quality: the gets side falls by
        // `paid * tg0/tp0`, not by the currency actually handed over.
        // #105945386 7EF34E79F13A — 63 ShearPepe for 718602 drops, filled
        // 182925 drops off the pool for 16.01209440508811 SPepe. We rested
        // 63 - 16.01209440508811 = 46.98790559491189; mainnet rests
        // 535677 * 63/718602 = 46.96292384379671, and 63 - 182925*63/718602 is
        // the same number. Verified 16-digit exact on #105912454 FE592890B233
        // too (1913.346495384532 vs our 1913.34664).
        //
        // Re-derived in BOTH directions. The one-way clamp here was a
        // workaround for #105930662 40FB322EC16C while the reprice still used
        // the RAW ratio; `070ca31` fixed the rate itself and that ledger is
        // byte-exact now, so the direction guard only suppressed the reprice
        // where it needed to move the residual UP.
        //
        // #105843839 C1F9FB1F is what it was hiding, and it is the only
        // remaining hit whose stored value is ILLEGAL rather than merely wrong:
        // an OfferCreate crossing an AMM (rDL7HrRz, no Offer SLE) sells
        // 0.410952568532 AUDD, and falling through to `rem_gets` stored the
        // exact subtraction —
        //   7157.9408748 - 0.410952568532 = 7157.5299222314683491
        // TWENTY significant digits, which an IOU STAmount cannot hold at all,
        // against mainnet's repriced 7157.5299222315. `derived` is 16 digits by
        // construction, so using it fixes the value and the representation at
        // once.
        //
        // ⚠ INVISIBLE TO THE GATE: the book page key is built from tp0/tg0, so
        // a residual with the wrong amounts still lands on the correct page and
        // every mutation-set leg stays green. This was found with DX_VALCHECK
        // and its regression evidence is a value sweep, not a key sweep.
        // tfSell rests the SUBTRACTED in-side and REPRICES the out-side —
        // OfferCreate.cpp:491-521: afterCross.in = takerGets − the crossed
        // NET (gateway fee divided back out), a 16-digit STAmount result,
        // then afterCross.out = divRoundStrict(afterCross.in, rate, out,
        // roundUp=false). The buy branch below is the mirror (E7399DA3).
        // #106455038 75F01CB4 (full-ledger replay): 28.729093 LTC minus the
        // pool's 0.0439975579446888 net rests 28.68509544205531 (16-digit
        // truncation of the exact 28.6850954420553112), repriced pays
        // 1058491178 drops.
        let (rem_pays, rem_gets) = if sell {
            if me_cmp(rem_gets, tg0).is_lt() && !me_is_zero(rem_gets) {
                // HALF-EVEN, not truncation: rippled's afterCross.in is an
                // STAmount SUBTRACTION (takerGets − the crossed in), and
                // Number normalizes to 16 digits at to-nearest. #106455088
                // F2E338D5: 19.314637 − 0.0017358593779919 leaves …62200|81,
                // which nearest carries to 19.31290114062201 (mainnet's
                // rested TakerGets) and truncation dropped to …200. The
                // #106455038 75F01CB4 calibration (tail .12) rounds down
                // under both rules and never discriminated.
                let g16 = crate::tx::amm_swap::round16(
                    rem_gets.0,
                    rem_gets.1,
                    false,
                    crate::tx::amm_swap::Rnd::Near,
                );
                let p = match rate_of_me(tg0, tp0) {
                    Some(q) if pays_leg.xrp => (div_round_drops_strict_floor(g16, rate_me(q)), 0),
                    Some(q) => div_round16_down(g16, rate_me(q)),
                    None => me_muldiv(g16, tp0, tg0, false),
                };
                (p, g16)
            } else {
                (rem_pays, rem_gets)
            }
        } else {
            (rem_pays, rem_gets)
        };
        let rem_gets = if sell {
            rem_gets
        } else {
            // Priced at the offer's ENCODED ratio, not its raw one. rippled
            // reprices the rested remainder through `Quality::rate()`, which is
            // `getRate(TakerGets, TakerPays)` — a 16-DIGIT value — and the last
            // digit of that decides the last digit of the residual.
            //
            // #105924683 E7399DA3 is the specimen and it is exact. 61 ShaPepe
            // for 709289 drops; an AMM takes 290802 drops for 25.0046017186344,
            // leaving 418487 drops to rest:
            //   raw     418487 * 61/709289          = 35.9905581504859091..
            //                                   ceil = 35.99055815048591  <- ours
            //   encoded 418487 * 0.00008600161570248517
            //                                        = 35.99055815048592  <- mainnet
            // Note the remainder is NOT (original - consumed): that would rest
            // 61 - 25.0046017186344 = 35.9953982813656. rippled reprices.
            //
            // ⚠ ONLY when the offer actually crossed. An UNCROSSED offer rests
            // exactly as submitted — repricing `tp0` through the encoded ratio
            // reconstructs `tg0` a hair low and writes 5907469.489999999 for
            // 5907469.49, or 4.999999999999999 for 5. That regressed four
            // ledgers (105778999, 105843839, 105091578, 105847200) before the
            // guard went in, all of them offers nothing had touched.
            let derived = if me_cmp(rem_pays, tp0).is_lt() {
                match rate_of_me(tg0, tp0) {
                    // An XRP gets side is an INTEGRAL asset, so rippled's
                    // `mulRound` canonicalises to whole drops itself — ceiling
                    // at a tenth of a drop — rather than producing a 16-digit
                    // value for a separate rescale to round again. See
                    // `mul_round_drops`.
                    Some(q) if gets_leg.xrp => (mul_round_drops(rem_pays, rate_me(q)), 0),
                    Some(q) => mul_round16_up(rem_pays, rate_me(q)),
                    None => me_muldiv(rem_pays, tg0, tp0, true),
                }
            } else {
                me_muldiv(rem_pays, tg0, tp0, true)
            };
            // `me_muldiv`'s round-up is at 16 SIGNIFICANT DIGITS. On an XRP leg
            // the value still has to become whole drops, and truncating there
            // throws the round-up away — rippled's `divRound` with an XRP asset
            // rounds up to whole DROPS directly. offer.rs already carried this
            // as a KNOWN GAP with no failing ledger; #105672435 B409D45C is it.
            //
            // Its fill is exactly right (DX_JUDGE `pay=(5713437)`, mainnet's
            // `New flow iter 0: 5713437`) — only the RESIDUAL was a drop out:
            //   exact          165388.7424863568808
            //   16-digit ceil  165388.7424863569 -> floor to drops -> 165388  ours
            //   ceil to drops                                        165389   mainnet
            let derived = if gets_leg.xrp && !me_is_zero(derived) {
                (me_rescale(derived, 0, true), 0)
            } else {
                derived
            };
            if me_is_zero(derived) { rem_gets } else { derived }
        };
        let seq = if tx.uses_ticket() { tx.ticket_seq.unwrap_or(0) } else { tx.sequence };
        let offer_key = keylet::offer_key(&tx.account, seq);
        let owner_node = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &offer_key);
        // The STORED flags are the lsf bits, not the tx's tf bits: tfPassive
        // (0x00010000) → lsfPassive (0x00010000) but tfSell (0x00080000) →
        // lsfSell (0x00020000), and tfUniversal/IoC/FoK never persist (byte
        // census: we stored 0x80000 and even 0x80000000 where mainnet has
        // 0x20000 / 0).
        let lsf_flags = (flags & 0x0001_0000) | if flags & 0x0008_0000 != 0 { 0x0002_0000 } else { 0 };
        let mut offer_obj = serde_json::json!({
            "LedgerEntryType": "Offer",
            "Account": hex::encode(tx.account),
            "Sequence": seq,
            "TakerPays": me_amount_json(&tp_json, rem_pays),
            "TakerGets": me_amount_json(&tg_json, rem_gets),
            "Flags": lsf_flags,
            "OwnerNode": format!("{owner_node:x}"),
        });
        // Book quality comes from the offer as REQUESTED (after tick
        // rounding), not from the residual: rippled keeps a partially
        // crossed offer at its original price (uRate is computed before
        // crossing).
        //
        // The directory hints MUST be stored on the offer itself: a later
        // OfferCancel (or crossing reap) reads BookDirectory off the object to
        // unlink the book page. A hydrated offer carries them from mainnet;
        // one WE placed only has what we write here — omitting them left the
        // book page undeleted when a same-ledger cancel followed (D63363BB).
        if let Some(q) = rate_of_me(tp0, tg0) {
            let base = match &domain {
                Some(d) => keylet::book_base_domain(&pays_leg.cur, &gets_leg.cur, &pays_leg.issuer, &gets_leg.issuer, d),
                None => keylet::book_base(&pays_leg.cur, &gets_leg.cur, &pays_leg.issuer, &gets_leg.issuer),
            };
            let bdir = keylet::book_dir_key(&base, q);
            // Fresh book pages carry the canonical pair fields + rate (byte
            // census: every page we minted lacked them; hydrated pages have
            // them from mainnet).
            let mut book_extra = serde_json::json!({
                "ExchangeRate": format!("{:016x}", u64::from_be_bytes(bdir.0[24..32].try_into().unwrap_or([0u8;8]))),
                "TakerPaysCurrency": hex::encode(pays_leg.cur),
                "TakerPaysIssuer": hex::encode(pays_leg.issuer),
                "TakerGetsCurrency": hex::encode(gets_leg.cur),
                "TakerGetsIssuer": hex::encode(gets_leg.issuer),
            });
            // Permissioned books: fresh pages carry the scoping DomainID too
            // (#106455036 52F5AD01… via the full-ledger replay).
            if let Some(d) = &domain {
                book_extra["DomainID"] = serde_json::Value::String(hex::encode_upper(d.0));
            }
            let book_node = crate::ledger::directory::dir_insert_with(
                sandbox, &bdir, None, &offer_key, Some(&book_extra), true,
            );
            offer_obj["BookDirectory"] = serde_json::Value::String(hex::encode_upper(bdir.0));
            offer_obj["BookNode"] = serde_json::Value::String(format!("{book_node:x}"));
        }
        if let Some(e) = tx.fields.get("Expiration") {
            offer_obj["Expiration"] = e.clone();
        }
        if let Some(d) = &domain {
            offer_obj["DomainID"] = serde_json::Value::String(hex::encode_upper(d.0));
        }
        sandbox.write(offer_key, serde_json::to_vec(&offer_obj).expect("serializing valid JSON Value"));
        owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

/// OfferCancel transactor — cancel an existing DEX offer.
pub struct OfferCancelTransactor;

impl Transactor for OfferCancelTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "OfferCancel" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("OfferSequence").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let offer_seq = match tx.fields.get("OfferSequence").and_then(|s| s.as_u64()) {
            Some(s) => s as u32,
            None => return TxResult::Malformed,
        };

        let offer_key = keylet::offer_key(&tx.account, offer_seq);

        if let Some(data) = sandbox.read(&offer_key) {
            // Pull the offer's directory hints before deleting it: OwnerNode
            // (page in the owner's dir), BookDirectory (root key of the order
            // book's quality dir) and BookNode (page within it). rippled's
            // offerDelete unlinks both directories via these hints.
            let offer: Option<serde_json::Value> = serde_json::from_slice(&data).ok();
            let hint = |v: Option<&serde_json::Value>| {
                v.and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                })
            };
            let owner_node = offer.as_ref().and_then(|o| hint(o.get("OwnerNode")));
            let book_node = offer.as_ref().and_then(|o| hint(o.get("BookNode")));
            let book_dir = offer
                .as_ref()
                .and_then(|o| o.get("BookDirectory"))
                .and_then(|v| v.as_str())
                .and_then(|s| hex::decode(s).ok())
                .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                .map(xrpl_core::types::Hash256);

            sandbox.delete(offer_key);
            crate::ledger::directory::owner_dir_remove(sandbox, &tx.account, &offer_key, owner_node, false);
            if let Some(bd) = book_dir {
                crate::ledger::directory::dir_remove(sandbox, &bd, &offer_key, book_node, false);
            }

            // Decrement OwnerCount
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(data) = sandbox.read(&acct_key) {
                if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                    if count > 0 {
                        acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
                    }
                    sandbox.write(acct_key, serde_json::to_vec(&acct).expect("serializing valid JSON Value"));
                }
            }
        }

        TxResult::Success
    }
}

#[cfg(test)]
mod ulp_tests {
    use super::*;

    /// Proves the DX_ULP detector CAN fire, so a silent sweep means "no ledger
    /// decides it" rather than "the detector is mis-wired".
    ///
    /// `norm16` truncates a 16-significant-digit product; `mul_round16_up`
    /// rounds it up. Where the product has a remainder the two differ by one
    /// ulp, and any threshold placed strictly between them admits under one
    /// and rejects under the other — which is exactly the condition the
    /// detector tests.
    #[test]
    fn a_truncated_composition_and_a_rounded_one_can_straddle_the_limit() {
        // Two mantissas whose product needs truncating (remainder non-zero).
        let a: Me = (1_234_567_890_123_457, 0);
        let b: Me = (9_876_543_210_987_653, 0);
        let trunc = norm16((a.0 * b.0, a.1 + b.1));
        let up = mul_round16_up(a, b);
        assert_ne!(trunc, up, "the product must actually need rounding");
        assert_eq!(
            me_cmp(trunc, up),
            std::cmp::Ordering::Less,
            "truncation is the more OPTIMISTIC bound in me-space (smaller is better)"
        );
        // A limit sitting exactly on the truncated value: `within` admits on
        // `q <= thr`, so the truncated bound is admitted and the rounded one
        // is not. That verdict split is what DX_ULP reports.
        let thr = trunc;
        assert!(me_cmp(trunc, thr).is_le(), "truncated bound is admitted");
        assert!(!me_cmp(up, thr).is_le(), "rounded-up bound is dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::{apply_modifications, Sandbox};
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    /// IOU addition rounds the EXACT sum half-even to 16 digits (rippled's
    /// `Number`), it does not truncate the smaller operand's tail.
    ///
    /// The pool of #106143011 at its third AMM turn, taken from mainnet's own
    /// trace: 2000892.236615386 + 100.153148870651 is exactly
    /// 2000992.389764256|651, so the stored value is ...257. Truncating gives
    /// ...256, one ulp low, which re-prices every later slice off that pool.
    #[test]
    fn iou_addition_rounds_half_even_not_truncated() {
        let (neg, sum) = stamount_signed_add(
            false,
            (2_000_892_236_615_386, -9),
            false,
            (1_001_531_488_706_510, -13),
        );
        assert!(!neg);
        assert_eq!(norm16(sum), (2_000_992_389_764_257, -9));

        // A tail below half still rounds down, and the half-even tie breaks to
        // the even mantissa rather than always up.
        let (_, down) =
            stamount_signed_add(false, (2_000_892_236_615_386, -9), false, (1_000_000_000_000_000, -13));
        assert_eq!(norm16(down), (2_000_992_236_615_386, -9));
        let (_, tie) = stamount_signed_add(false, (1_000_000_000_000_001, -15), false, (5, -16));
        assert_eq!(norm16(tie), (1_000_000_000_000_002, -15));
        let (_, tie_even) = stamount_signed_add(false, (1_000_000_000_000_002, -15), false, (5, -16));
        assert_eq!(norm16(tie_even), (1_000_000_000_000_002, -15));

        // Opposite signs subtract, and a negligible operand leaves the larger
        // untouched instead of overflowing the alignment.
        let (neg, diff) =
            stamount_signed_add(false, (1_000_000_000_000_000, -15), true, (2_000_000_000_000_000, -15));
        assert!(neg);
        assert_eq!(norm16(diff), (1_000_000_000_000_000, -15));
        let (_, tiny) = stamount_signed_add(false, (1_234_567_890_123_456, 0), false, (1, -40));
        assert_eq!(norm16(tiny), (1_234_567_890_123_456, 0));
    }

    fn read_balance(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        json_at(sandbox, &keylet::account_root_key(id))
            .and_then(|a| a["Balance"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0)
    }

    fn make_state_with_account(id: &[u8; 20], balance: u64) -> LedgerState {
        let header = LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        let mut state = LedgerState::new_unverified(header);
        let acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": 1,
            "OwnerCount": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        state
    }

    #[test]
    fn offer_create_places_on_book() {
        let acct = [0x01u8; 20];
        let state = make_state_with_account(&acct, 50_000_000);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "10"},
                "TakerGets": "1000000",
            }),
        };
        assert_eq!(OfferCreateTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Offer should exist on the book
        let offer_key = keylet::offer_key(&acct, 5);
        assert!(sandbox.exists(&offer_key));

        // OwnerCount incremented
        let acct_key = keylet::account_root_key(&acct);
        let data = sandbox.read(&acct_key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["OwnerCount"].as_u64().unwrap(), 1);
    }

    #[test]
    fn offer_cancel_removes_from_book() {
        let acct = [0x01u8; 20];
        let state = make_state_with_account(&acct, 50_000_000);

        // First create
        let mut sandbox = Sandbox::new(&state);
        let create_tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "1000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "10"},
            }),
        };
        OfferCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Then cancel
        let cancel_tx = TxFields {
            account: acct,
            tx_type: "OfferCancel".to_string(),
            fee: 12,
            sequence: 6,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"OfferSequence": 5}),
        };
        assert_eq!(OfferCancelTransactor.do_apply(&cancel_tx, &mut sandbox), TxResult::Success);

        let offer_key = keylet::offer_key(&acct, 5);
        assert!(!sandbox.exists(&offer_key));

        // OwnerCount back to 0
        let acct_key = keylet::account_root_key(&acct);
        let data = sandbox.read(&acct_key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["OwnerCount"].as_u64().unwrap(), 0);
    }

    #[test]
    fn quality_one_matches_rippled_constant() {
        // getRate(1 XRP, 1 XRP) — rippled's QUALITY_ONE.
        let one = serde_json::Value::String("1000000".into());
        assert_eq!(keylet::offer_quality(&one, &one), Some(0x55038D7EA4C68000));
        // mainnet-verified vector (#105666725 offer 95551964FE):
        // 602250000 drops / 602.25 RLUSD-ish IOU — sanity: nonzero, monotonic
        let pays = serde_json::Value::String("3500000".into());
        let gets = serde_json::json!({"currency":"ABC","issuer":"0000000000000000000000000000000000000001","value":"7"});
        let q1 = keylet::offer_quality(&pays, &gets).unwrap();
        let gets2 = serde_json::json!({"currency":"ABC","issuer":"0000000000000000000000000000000000000001","value":"14"});
        let q2 = keylet::offer_quality(&pays, &gets2).unwrap();
        assert!(q2 < q1); // paying the same for more = better (lower) quality
    }

    #[test]
    fn immediate_or_cancel_no_place() {
        let acct = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut state = make_state_with_account(&acct, 50_000_000);
        // Fund the taker's TakerGets (10 USD) so the offer is not rejected as
        // unfunded before the IoC-crosses-nothing path is even reached.
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");
        let (lo, hi) = if acct < issuer { (acct, issuer) } else { (issuer, acct) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if acct < issuer { "10" } else { "-10" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(keylet::ripple_state_key(&acct, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "1000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "10"},
                "Flags": 0x00020000u64, // tfImmediateOrCancel
            }),
        };
        // Mainnet (ImmediateOfferKilled amendment): IoC that crosses nothing
        // is tecKILLED, and nothing is placed.
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Killed);

        // IOC offer should NOT be placed on the book (no crossing happened)
        let offer_key = keylet::offer_key(&acct, 5);
        assert!(!sandbox.exists(&offer_key));
    }

    /// Mainnet tx A2AED79309E6… (ledger 105035380): a tfSell FoK selling XRP
    /// from an account whose balance sits BELOW its reserve (available <= 0)
    /// is tecUNFUNDED_OFFER, not tecUNFUNDED — rippled's accountFunds for a
    /// native sell is balance-minus-reserve.
    #[test]
    fn xrp_sell_below_reserve_is_unfunded_offer() {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        // Balance 725_581 with OwnerCount 1 → reserve 1_200_000 → available < 0.
        let mut state = make_state_with_account(&taker, 725_581);
        {
            let key = keylet::account_root_key(&taker);
            let mut a = json_at(&Sandbox::new(&state), &key).unwrap();
            a["OwnerCount"] = serde_json::json!(1);
            state.state_map.insert(key, serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 5,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "468801653",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "Flags": 0x000C_0000u64,
            }),
        };
        // The probe runs preclaim first — it must agree on tecUNFUNDED_OFFER,
        // not the generic tecUNFUNDED.
        assert_eq!(OfferCreateTransactor.preclaim(&tx, &sandbox), TxResult::UnfundedOffer);
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::UnfundedOffer);
    }

    /// Mainnet tx 8A70D6556E… (ledger 105035380): a tfSell FoK offering to
    /// sell an IOU the account holds NONE of is unfunded — rippled returns
    /// tecUNFUNDED_OFFER (fee-only) without crossing anything, even though a
    /// willing maker exists. The taker's funding of the TakerGets asset caps
    /// the crossing; zero holdings means nothing to sell.
    #[test]
    fn sell_offer_unfunded_when_taker_holds_none() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 50_000_000);
        for id in [&maker, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        // Maker holds 100 USD and offers to buy it back for XRP — a willing
        // counterparty, so any spurious crossing would show up.
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
                "TakerGets": "10000000",
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Taker holds NO USD but tries to sell 50 USD for XRP, tfSell + FoK.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "50"},
                "TakerPays": "1000000",
                "Flags": 0x000C_0000u64,
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::UnfundedOffer);
        // Nothing crossed: maker's offer and taker's XRP untouched.
        assert!(sandbox.exists(&keylet::offer_key(&maker, 2)));
        assert_eq!(read_balance(&sandbox, &taker), 50_000_000);
    }

    /// Mainnet tx 1C8E3BF4C2B2… (ledger 105802230): a FillOrKill offering 50
    /// XRP from an account holding ~14 XRP against a 36-object reserve — only
    /// ~5.8 XRP is actually spendable. rippled clamps the crossing input to
    /// accountFunds ("Don't send more than our balance", OfferCreate.cpp:399
    /// -401), so the offer cannot fully fill and is killed. TakerGets bounds
    /// what may REST, never what may be spent: crossing the full 50 XRP would
    /// spend money the account does not have, and on mainnet the DeepTide it
    /// wrongly acquired was then sold on by two later Payments in the same
    /// ledger that mainnet returned tecPATH_DRY.
    #[test]
    fn fill_or_kill_killed_when_taker_cannot_fund_taker_gets() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        // 14 XRP with OwnerCount 36 → reserve 8.2 XRP → only 5.8 XRP spendable.
        let mut state = make_state_with_account(&taker, 14_000_000);
        {
            let key = keylet::account_root_key(&taker);
            let mut a = json_at(&Sandbox::new(&state), &key).unwrap();
            a["OwnerCount"] = serde_json::json!(36);
            state.state_map.insert(key, serde_json::to_vec(&a).unwrap()).unwrap();
        }
        for id in [&maker, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        // Maker holds 100 USD and rests an offer selling all of it for 50 XRP.
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "50000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Taker wants the whole 100 USD for 50 XRP — priced exactly at the
        // maker's rate, so only full funding could fill it.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "50000000",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
                "Flags": 0x0004_0000u64,
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Killed);
        // Killed rolls everything back: the maker's offer survives untouched
        // and the taker keeps every drop.
        assert!(sandbox.exists(&keylet::offer_key(&maker, 2)));
        assert_eq!(read_balance(&sandbox, &taker), 14_000_000);
    }

    /// A PLAIN offer crossing an UNDERFUNDED maker at exactly the taker's limit
    /// quality must cross the maker to its funded amount and rest the remainder,
    /// not strand the whole offer. The section-1 achieved-quality break — meant
    /// for the #105780948 IoC tecKILLED, where the TAKER's side clamps the fill
    /// short — also fired here: the ≤1e-7 round-up on `pay` for the small
    /// maker-funds-clamped fill tips just past the 1e-7 forgiveness, so we broke
    /// and rested the whole offer. Mainnet #105672435 B409D45C crosses maker
    /// 499CA86D (funded 22.283542 of 22.928591 at the taker's exact q).
    #[test]
    fn plain_offer_crosses_underfunded_maker_at_equal_quality() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"666");

        let mut state = make_state_with_account(&taker, 50_000_000);
        for id in [&maker, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let mline = |bal: &str| serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { bal.to_string() } else { format!("-{bal}") }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
        });
        // Maker holds 100 666 and rests a full-size sell: 22.928591 for 5878826
        // drops (q = the taker's exact limit below).
        state.state_map.insert(mkey, serde_json::to_vec(&mline("100")).unwrap()).unwrap();
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "5878826",
                "TakerGets": {"currency": "666", "issuer": hex::encode(issuer), "value": "22.928591"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();
        assert!(json_at(&Sandbox::new(&state), &keylet::offer_key(&maker, 2)).is_some());
        // Maker then spends most of its 666 elsewhere: the resting offer is now
        // underfunded — only 22.283542 backs the 22.928591 it still advertises.
        state.state_map.insert(mkey, serde_json::to_vec(&mline("22.283542")).unwrap()).unwrap();

        // Taker (PLAIN, Flags 0): buy 22.928591 666 for 5878826 drops.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "5878826",
                "TakerPays": {"currency": "666", "issuer": hex::encode(issuer), "value": "22.928591"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
        // The maker's funds are exhausted so its offer is consumed, and the taker
        // spent XRP acquiring the 666. Without the gate the achieved-quality
        // break stranded the whole crossing: the maker offer survived and the
        // taker spent only its fee.
        assert!(!sandbox.exists(&keylet::offer_key(&maker, 2)), "underfunded maker crossed to exhaustion");
        assert!(read_balance(&sandbox, &taker) < 49_000_000, "taker spent XRP crossing the maker");
    }

    /// tfPassive: a passive offer does NOT consume makers at its own quality —
    /// it rests behind them, crossing only strictly-better offers (rippled
    /// OfferCreate.cpp:396 `++threshold`). An identical NON-passive offer
    /// crosses the same maker. #105807256 / #105803327 over-mutated by
    /// consuming the equal-priced makers a passive offer must leave alone.
    #[test]
    fn passive_offer_rests_behind_equal_quality_maker() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        // Maker holds 100 USD and rests: sell 100 USD for 50 XRP (rate 500000
        // drops/USD, exact). The taker below buys at that same exact rate.
        let base = || -> LedgerState {
            let mut state = make_state_with_account(&taker, 200_000_000);
            for id in [&maker, &issuer] {
                let a = serde_json::json!({
                    "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                    "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
                });
                state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
            }
            let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
            let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if maker < issuer { "100" } else { "-100" }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
            });
            state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();
            let mut sandbox = Sandbox::new(&state);
            let maker_offer = TxFields {
                account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({
                    "TakerPays": "50000000",
                    "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
                }),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
            state
        };
        let taker_tx = |flags: u64| TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "50000000",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
                "Flags": flags,
            }),
        };

        // Non-passive at the maker's exact rate: crosses and consumes it.
        let st = base();
        let mut sb = Sandbox::new(&st);
        assert_eq!(OfferCreateTransactor.do_apply(&taker_tx(0), &mut sb), TxResult::Success);
        assert!(!sb.exists(&keylet::offer_key(&maker, 2)), "non-passive crosses the equal-quality maker");
        assert!(read_balance(&sb, &taker) < 199_000_000, "non-passive spent XRP");

        // Passive (tfPassive): rests behind the equal-quality maker, untouched.
        let stp = base();
        let mut sbp = Sandbox::new(&stp);
        assert_eq!(OfferCreateTransactor.do_apply(&taker_tx(0x0001_0000), &mut sbp), TxResult::Success);
        assert!(sbp.exists(&keylet::offer_key(&maker, 2)), "passive rests behind the equal maker");
        assert_eq!(read_balance(&sbp, &taker), 200_000_000, "passive consumed nothing");
    }

    /// tfSell means "sell the ENTIRE TakerGets, even if that acquires more
    /// than TakerPays." A FillOrKill sell against a maker offering a better
    /// rate than the taker's minimum must consume the whole TakerGets (taking
    /// the surplus), not stop once TakerPays is reached — otherwise the
    /// unsold remainder fails the fill check and the offer is wrongly killed.
    #[test]
    fn fill_or_kill_sell_takes_surplus_over_taker_pays() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 50_000_000);
        for id in [&maker, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        // Maker holds 100 USD.
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();

        // Maker sells 100 USD for 10 XRP (10 USD per XRP — very generous).
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "10000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        assert!(sandbox.exists(&keylet::offer_key(&maker, 2)), "maker offer placed");
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();
        assert!(json_at(&Sandbox::new(&state), &keylet::offer_key(&maker, 2)).is_some(), "maker offer persisted");

        // Taker: tfSell + tfFillOrKill, sell 10 XRP for at least 5 USD.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "10000000",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "Flags": 0x000C_0000u64, // tfSell | tfFillOrKill
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Taker sold all 10 XRP and acquired ~100 USD — far above TakerPays 5.
        assert_eq!(read_balance(&sandbox, &taker), 40_000_000);
        let tkey = keylet::ripple_state_key(&taker, &issuer, &cur);
        let tl = json_at(&sandbox, &tkey).expect("taker USD line");
        let (_neg, mag) = signed_value(&tl["Balance"]);
        assert!(me_cmp(mag, (50, 0)).is_gt(), "acquired well over TakerPays 5, got {mag:?}");
    }

    /// Build a state with `holder` and `issuer` accounts (no trust line).
    fn state_for_line(holder: &[u8; 20], issuer: &[u8; 20]) -> crate::ledger::state::LedgerState {
        let mut state = make_state_with_account(holder, 50_000_000);
        let iss = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(issuer),
            "Balance": "50000000",
            "Sequence": 1,
            "OwnerCount": 0,
        });
        state
            .state_map
            .insert(keylet::account_root_key(issuer), serde_json::to_vec(&iss).unwrap())
            .unwrap();
        state
    }

    fn owner_count(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        json_at(sandbox, &keylet::account_root_key(id))
            .and_then(|a| a["OwnerCount"].as_u64())
            .unwrap_or(0)
    }

    /// rippled's trustCreate charges the reserve to the RECEIVER only: the
    /// line joins both owner directories, but just one OwnerCount moves. The
    /// receiver's side also gets NoRipple (their account lacks DefaultRipple).
    #[test]
    fn line_creation_charges_only_the_receiver() {
        let holder = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = state_for_line(&holder, &issuer);
        let mut sandbox = Sandbox::new(&state);
        let leg = Leg { xrp: false, cur: *b"USD\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", issuer };

        line_adjust(&mut sandbox, &holder, &leg, (5, 0), true);

        let line = json_at(&sandbox, &keylet::ripple_state_key(&holder, &issuer, &leg.cur))
            .expect("line created");
        let flags = line["Flags"].as_u64().unwrap();
        let holder_low = holder < issuer;
        let (reserve, no_ripple) = if holder_low {
            (0x0001_0000, 0x0010_0000)
        } else {
            (0x0002_0000, 0x0020_0000)
        };
        assert_eq!(flags & reserve, reserve, "receiver reserve flag");
        assert_eq!(flags & no_ripple, no_ripple, "receiver NoRipple flag");
        assert_eq!(owner_count(&sandbox, &holder), 1);
        assert_eq!(owner_count(&sandbox, &issuer), 0, "issuer pays no reserve");
    }

    /// Mainnet tx 0A207078B3A4… (ledger 105666725): a line spent back to
    /// exactly zero returns to its default state and rippled DELETES it,
    /// releasing the holder's reserve and unlinking both owner directories.
    #[test]
    fn line_spent_to_zero_is_deleted() {
        let holder = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = state_for_line(&holder, &issuer);
        let mut sandbox = Sandbox::new(&state);
        let leg = Leg { xrp: false, cur: *b"USD\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", issuer };
        let lkey = keylet::ripple_state_key(&holder, &issuer, &leg.cur);

        line_adjust(&mut sandbox, &holder, &leg, (5, 0), true);
        assert!(sandbox.exists(&lkey));
        assert_eq!(owner_count(&sandbox, &holder), 1);

        // Spend every unit back out.
        line_adjust(&mut sandbox, &holder, &leg, (5, 0), false);

        assert!(!sandbox.exists(&lkey), "default line deleted at zero balance");
        assert_eq!(owner_count(&sandbox, &holder), 0, "reserve released");
    }

    /// A line spent only PART of the way down survives untouched.
    #[test]
    fn line_partially_spent_survives() {
        let holder = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = state_for_line(&holder, &issuer);
        let mut sandbox = Sandbox::new(&state);
        let leg = Leg { xrp: false, cur: *b"USD\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", issuer };
        let lkey = keylet::ripple_state_key(&holder, &issuer, &leg.cur);

        line_adjust(&mut sandbox, &holder, &leg, (5, 0), true);
        line_adjust(&mut sandbox, &holder, &leg, (2, 0), false);

        assert!(sandbox.exists(&lkey));
        assert_eq!(owner_count(&sandbox, &holder), 1);
    }

    /// Mainnet tx 9870DA80… (ledger 105091579): the STX issuer publishes
    /// TickSize 6, so rippled rounds the offer rate UP to 6 significant
    /// digits and re-derives the non-exact side before placing. The tx asks
    /// to sell 8539920 drops for 813087.72688567 STX; mainnet stored an
    /// offer of 8539914 drops at book quality 5321D3536A38DBA4.
    #[test]
    fn offer_placement_honors_issuer_tick_size() {
        let acct = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut state = make_state_with_account(&acct, 500_000_000);
        // Issuer account publishing TickSize 6.
        let iss_acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(issuer),
            "Balance": "50000000",
            "Sequence": 1,
            "OwnerCount": 0,
            "TickSize": 6,
        });
        state
            .state_map
            .insert(keylet::account_root_key(&issuer), serde_json::to_vec(&iss_acct).unwrap())
            .unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "STX", "issuer": hex::encode(issuer), "value": "813087.72688567"},
                "TakerGets": "8539920",
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let placed = json_at(&sandbox, &keylet::offer_key(&acct, 5)).expect("offer placed");
        assert_eq!(placed["TakerGets"].as_str(), Some("8539914"));
        assert_eq!(placed["TakerPays"]["value"].as_str(), Some("813087.72688567"));

        // ...and it lands in the book page for the tick-rounded quality.
        let q = keylet::offer_quality(&placed["TakerPays"], &placed["TakerGets"]).unwrap();
        assert_eq!(format!("{q:016x}"), "5321d3536a38dba4");
    }

    /// A holder whose line the ISSUER has frozen can sell nothing, however
    /// large the balance: rippled reads spendable IOU through `accountHolds`
    /// with `fhZERO_IF_FROZEN`, and `isFrozen` (RippleStateHelpers.cpp:127) is
    /// the issuer's global freeze OR the issuer's side of the line —
    /// `(issuer > account) ? lsfHighFreeze : lsfLowFreeze`.
    ///
    /// #105878507 475EA928 and ten siblings across six ledgers: rLiq73yy holds
    /// 5811047220.15868 ARK and offers to sell it, but rBWfabv7 froze the line.
    /// Mainnet claims the fee and returns tecUNFUNDED_OFFER (1 mutation); we
    /// read the balance alone, crossed, and placed (4).
    #[test]
    fn a_frozen_holder_cannot_fund_an_offer() {
        let acct = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&acct, 500_000_000);
        let iss = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
        });
        state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&iss).unwrap()).unwrap();
        // issuer (0x02..) > acct (0x01..) ⇒ the issuer is the HIGH side, so its
        // freeze flag is lsfHighFreeze. 100 USD held, and every drop frozen.
        let (lo, hi) = (acct, issuer);
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64 | 0x0080_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": "100"},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(keylet::ripple_state_key(&acct, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 5,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "1000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "10"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::UnfundedOffer);
        assert!(!sandbox.exists(&keylet::offer_key(&acct, 5)), "and nothing is placed");
    }

    /// A bridged pass cannot stop at the pool: off the default path rippled's
    /// per-offer quality gate is disabled, so the step keeps stepping into the
    /// leg's CLOB and StrandFlow judges the pass as a whole. Here leg A's pool
    /// is 100x better than leg A's book, and the pool slice is a fraction of
    /// the request — so mainnet's pass would be dominated by that book and
    /// rejected outright. #105807256 84FD7DC8 is the live case: it rests in
    /// 4 nodes while we crossed 8 objects off six pool slices.
    #[test]
    fn a_bridged_pass_needs_its_book_composition_within_the_limit() {
        let taker = [0x01u8; 20];
        let mk_a = [0x04u8; 20];
        let mk_b = [0x05u8; 20];
        let iss_a = [0x02u8; 20];
        let iss_b = [0x03u8; 20];
        let pool = [0x06u8; 20];
        let (mut ca, mut cb) = ([0u8; 20], [0u8; 20]);
        ca[12..15].copy_from_slice(b"AAA");
        cb[12..15].copy_from_slice(b"BBB");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for id in [&mk_a, &mk_b, &iss_a, &iss_b, &pool] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut line = |who: &[u8; 20], iss: &[u8; 20], cur: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < iss { (*who, *iss) } else { (*iss, *who) };
            let v = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < iss { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, iss, cur), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        line(&taker, &iss_a, &ca, "1000");   // the taker's AAA to sell
        line(&mk_b, &iss_b, &cb, "1000");    // leg B maker's BBB
        line(&pool, &iss_a, &ca, "100");     // leg A pool: 100 AAA / 100 XRP

        // Leg A pool at 1 AAA per XRP — 100x better than leg A's book below.
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(pool), "TradingFee": 0,
        });
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let a_leg = leg_of(&serde_json::json!({"currency":"AAA","issuer":hex::encode(iss_a),"value":"1"})).unwrap();
        let akey = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &a_leg.cur, &a_leg.issuer);
        state.state_map.insert(akey, serde_json::to_vec(&amm).unwrap()).unwrap();

        // Leg A book: 1 XRP for 100 AAA. Leg B book: 100 BBB for 1 XRP.
        for (who, pays, gets) in [
            (mk_a, serde_json::json!({"currency":"AAA","issuer":hex::encode(iss_a),"value":"100"}), serde_json::json!("1000000")),
            (mk_b, serde_json::json!("1000000"), serde_json::json!({"currency":"BBB","issuer":hex::encode(iss_b),"value":"100"})),
        ] {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({"TakerPays": pays, "TakerGets": gets}),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
        }

        // Sell 10 AAA for 100 BBB — limit 0.1 AAA/BBB. Bridged via the pool
        // that is 0.01; via the two BOOKS it is 1.0, ten times past the limit.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 9,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "AAA", "issuer": hex::encode(iss_a), "value": "10"},
                "TakerPays": {"currency": "BBB", "issuer": hex::encode(iss_b), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Nothing crossed: both makers untouched and the offer rests in full.
        assert_eq!(
            json_at(&sandbox, &keylet::offer_key(&mk_a, 2)).unwrap()["TakerGets"].as_str(),
            Some("1000000"),
            "leg A's book maker must be untouched",
        );
        assert_eq!(
            json_at(&sandbox, &keylet::offer_key(&mk_b, 2)).unwrap()["TakerGets"]["value"].as_str(),
            Some("100"),
            "leg B's book maker must be untouched",
        );
        let rested = json_at(&sandbox, &keylet::offer_key(&taker, 9)).expect("offer rests");
        assert_eq!(rested["TakerGets"]["value"].as_str(), Some("10"), "and rests in full");
    }

    /// The taker's OWN resting offer keeps a second strand alive, and that is
    /// what lets a bridge leg's POOL price the pass.
    ///
    /// `AMMContext::multiPath()` is `activeStrands.size() > 1`
    /// (StrandFlow.h:649) and it decides what `AMMLiquidity::getOffer` returns:
    /// with two strands a FIB slice at the pool's own quality, with one a
    /// `changeSpotPriceQuality` offer matched to the book's, which `tip` then
    /// discards in favour of the book. `BookTip` applies no owner filter, so an
    /// offer of the taker's own still prices the direct strand for admission
    /// even though it will be removed rather than crossed.
    ///
    /// Same books and pool as the test above, whose only difference is that
    /// there the direct strand does not exist — so the composition gate must
    /// come out the other way here. #105930662 40FB322EC16C is the live case:
    /// the taker's own 991CFC15 tips the direct book at 5.1422 BBRL/RLUSD
    /// inside its own 5.14324 limit, leg A's pool is 72x better than leg A's
    /// book, and mainnet crosses two fib slices where we crossed nothing —
    /// 6 mutations against 13, all 7 missing ones Modified.
    #[test]
    fn a_takers_own_offer_keeps_the_second_strand_and_the_pool_prices_the_bridge() {
        let taker = [0x01u8; 20];
        let mk_a = [0x04u8; 20];
        let mk_b = [0x05u8; 20];
        let iss_a = [0x02u8; 20];
        let iss_b = [0x03u8; 20];
        let pool = [0x06u8; 20];
        let (mut ca, mut cb) = ([0u8; 20], [0u8; 20]);
        ca[12..15].copy_from_slice(b"AAA");
        cb[12..15].copy_from_slice(b"BBB");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for id in [&mk_a, &mk_b, &iss_a, &iss_b, &pool] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut line = |who: &[u8; 20], iss: &[u8; 20], cur: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < iss { (*who, *iss) } else { (*iss, *who) };
            let v = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < iss { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, iss, cur), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        line(&taker, &iss_a, &ca, "1000");   // the taker's AAA to sell
        line(&taker, &iss_b, &cb, "1000");   // and the BBB its own resting offer sells
        line(&mk_b, &iss_b, &cb, "1000");    // leg B maker's BBB
        line(&pool, &iss_a, &ca, "100");     // leg A pool: 100 AAA / 100 XRP

        // Leg A pool at 1 AAA per XRP — 100x better than leg A's book below.
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(pool), "TradingFee": 0,
        });
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let a_leg = leg_of(&serde_json::json!({"currency":"AAA","issuer":hex::encode(iss_a),"value":"1"})).unwrap();
        let akey = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &a_leg.cur, &a_leg.issuer);
        state.state_map.insert(akey, serde_json::to_vec(&amm).unwrap()).unwrap();

        // Leg A book: 1 XRP for 100 AAA. Leg B book: 100 BBB for 1 XRP. Then
        // the TAKER's own offer on the direct AAA/BBB book, asking 9 AAA for
        // 100 BBB — 0.09, just inside the 0.1 limit the crossing tx sets, so it
        // tips the direct strand inside the limit and keeps it a candidate.
        for (who, seq, pays, gets) in [
            (mk_a, 2u32, serde_json::json!({"currency":"AAA","issuer":hex::encode(iss_a),"value":"100"}), serde_json::json!("1000000")),
            (mk_b, 2, serde_json::json!("1000000"), serde_json::json!({"currency":"BBB","issuer":hex::encode(iss_b),"value":"100"})),
            (taker, 3, serde_json::json!({"currency":"AAA","issuer":hex::encode(iss_a),"value":"9"}),
                       serde_json::json!({"currency":"BBB","issuer":hex::encode(iss_b),"value":"100"})),
        ] {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: seq,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({"TakerPays": pays, "TakerGets": gets}),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
        }
        // Sell 10 AAA for 100 BBB — limit 0.1 AAA/BBB. Bridged via the pool
        // that is 0.01; via the two BOOKS it is 1.0, ten times past the limit.
        let mut sandbox = Sandbox::new(&state);
        assert!(json_at(&sandbox, &keylet::offer_key(&taker, 3)).is_some(),
                "the taker's own offer must be resting on the direct book");
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 9,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "AAA", "issuer": hex::encode(iss_a), "value": "10"},
                "TakerPays": {"currency": "BBB", "issuer": hex::encode(iss_b), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // The POOL carried leg A, so leg A's book maker is never touched while
        // leg B's is consumed. Book-only admission crosses neither.
        assert_eq!(
            json_at(&sandbox, &keylet::offer_key(&mk_a, 2)).unwrap()["TakerGets"].as_str(),
            Some("1000000"),
            "leg A's book maker must be untouched — the pool priced that leg",
        );
        assert!(
            json_at(&sandbox, &keylet::offer_key(&mk_b, 2)).is_none(),
            "leg B's book maker must be consumed by the bridged pass",
        );
        assert!(
            json_at(&sandbox, &keylet::offer_key(&taker, 3)).is_none(),
            "and the taker's own offer is removed, not crossed",
        );
    }

    /// An out-limited fill is priced through the offer's ENCODED quality, and
    /// the last digit of that decides whether the pass survives.
    ///
    /// `Quality::ceil_out` is `result.in = mulRound(limit, quality.rate(),
    /// asset, roundUp)` with roundUp always true (Quality.cpp `ceilOutImpl`),
    /// so a partial take is priced off the 16-digit BookDirectory rate, NOT off
    /// the offer's raw TakerPays/TakerGets ratio. The two differ in the last
    /// digit whenever the raw ratio needs more than 16 digits.
    ///
    /// Here maker2 ties the taker's limit exactly (both 61 USD / 709289 drops),
    /// maker1 fills the first 290802 drops more cheaply, and the remaining
    /// 418487 drops are taken from maker2:
    ///   raw 418487*61/709289 = 35.9905581504859091…, ceil -> 35.99055815048591
    ///   mulRound(418487, encoded 0.00008600161570248517, up)
    ///                                                 -> 35.99055815048592
    /// At …591 the achieved rate ties the limit and the fill is kept; at …592
    /// it is 3 units worse and the whole pass is discarded, so maker2 must be
    /// left untouched and the remainder rested.
    ///
    /// #105924683 E7399DA36A79 is the live case, same numbers: rippled logs
    ///   Path rejected by limitQuality
    ///     limit: 5773207684604483397  path q: 5773207684604483400
    /// and rests. We crossed it — 9 mutations against 7.
    #[test]
    fn an_out_limited_fill_is_priced_through_the_offers_encoded_quality() {
        let taker = [0x01u8; 20];
        let mk1 = [0x04u8; 20];
        let mk2 = [0x05u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for id in [&mk1, &mk2, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut line = |who: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < &issuer { (*who, issuer) } else { (issuer, *who) };
            let v = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &issuer { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, &issuer, &cur), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        line(&taker, "1000");
        line(&mk1, "0");
        line(&mk2, "0");

        // mk1 is strictly cheaper and clears first; mk2 ties the taker's limit.
        for (who, gets, pays) in [
            (mk1, "290802", "25"),
            (mk2, "709289", "61"),
        ] {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({
                    "TakerGets": gets,
                    "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": pays},
                }),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
        }

        // Buy 709289 drops for 61 USD — the same ratio mk2 rests at.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 9,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "61"},
                "TakerPays": "709289",
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        assert!(
            json_at(&sandbox, &keylet::offer_key(&mk1, 2)).is_none(),
            "the cheaper maker must be fully consumed",
        );
        assert_eq!(
            json_at(&sandbox, &keylet::offer_key(&mk2, 2)).unwrap()["TakerGets"].as_str(),
            Some("709289"),
            "the tying maker must be UNTOUCHED — its fill prices 3 units past \
             the limit and rippled discards the whole pass",
        );
    }

    /// A single-path leg is priced by its pool's SPOT quality when the taker's
    /// limit beats that leg's book.
    ///
    /// `BookOfferCrossingStep::qualityThreshold` returns nullopt in that case
    /// (BookStep.cpp:475-480), so `getAMMOffer(view, nullopt)` yields
    /// `maxOffer` — built `AMMOffer(*this, amounts, balances, Quality{balances})`
    /// (AMMLiquidity.cpp), whose **quality() is the pool SPOT** even though its
    /// amounts drain ~99% of the pool. `tip` compares on quality(), so the pool
    /// wins whenever spot beats the book.
    ///
    /// Here leg B's pool is 9800 drops/BBB against a 10000 drops/BBB book. Via
    /// the books the bridge composes to 1.0 AAA/BBB and misses the taker's
    /// 0.995 limit; via leg B's spot it composes to 0.98 and clears it. There
    /// is no direct book, so only one strand is ever a candidate — this is the
    /// single-path branch, not the multiPath one.
    ///
    /// #105940336 CA2C624ED031 is the live case: leg B (XRP->BTC, pool
    /// rQBeAgh) spot 1.681884e-11 BTC/drop beats its book's 1.679720e-11, and
    /// rippled admits the strand at 63928.92 against a 63958.12 limit where the
    /// book composition is 64011.27. We rested: 6 mutations against 10.
    #[test]
    fn a_single_path_leg_is_priced_by_its_pools_spot_quality() {
        let taker = [0x01u8; 20];
        let mk_a = [0x04u8; 20];
        let mk_b = [0x05u8; 20];
        let iss_a = [0x02u8; 20];
        let iss_b = [0x03u8; 20];
        let pool_b = [0x07u8; 20];
        let (mut ca, mut cb) = ([0u8; 20], [0u8; 20]);
        ca[12..15].copy_from_slice(b"AAA");
        cb[12..15].copy_from_slice(b"BBB");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for (id, bal) in [(&mk_a, "500000000"), (&mk_b, "500000000"), (&iss_a, "500000000"),
                          (&iss_b, "500000000"), (&pool_b, "98000000")] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": bal, "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut line = |who: &[u8; 20], iss: &[u8; 20], cur: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < iss { (*who, *iss) } else { (*iss, *who) };
            let v = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < iss { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, iss, cur), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        line(&taker, &iss_a, &ca, "1000");    // the taker's AAA to sell
        line(&mk_b, &iss_b, &cb, "1000");     // leg B book maker's BBB
        line(&pool_b, &iss_b, &cb, "10000");  // leg B pool: 98 XRP / 10000 BBB

        // Leg B pool spot = 98000000/10000 = 9800 drops per BBB, strictly
        // better than the 10000 its book head asks.
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(pool_b), "TradingFee": 0,
        });
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let b_leg = leg_of(&serde_json::json!({"currency":"BBB","issuer":hex::encode(iss_b),"value":"1"})).unwrap();
        let akey = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &b_leg.cur, &b_leg.issuer);
        state.state_map.insert(akey, serde_json::to_vec(&amm).unwrap()).unwrap();

        // Leg A book: 1 XRP for 100 AAA. Leg B book: 100 BBB for 1 XRP.
        // No direct AAA/BBB book at all, so only one strand is ever a candidate.
        for (who, pays, gets) in [
            (mk_a, serde_json::json!({"currency":"AAA","issuer":hex::encode(iss_a),"value":"100"}), serde_json::json!("1000000")),
            (mk_b, serde_json::json!("1000000"), serde_json::json!({"currency":"BBB","issuer":hex::encode(iss_b),"value":"100"})),
        ] {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({"TakerPays": pays, "TakerGets": gets}),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
        }

        // Sell 99.5 AAA for 100 BBB — limit 0.995 AAA/BBB. Book composition is
        // 1.0 and misses it; leg B's spot composition is 0.98 and clears it.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 9,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "AAA", "issuer": hex::encode(iss_a), "value": "99.5"},
                "TakerPays": {"currency": "BBB", "issuer": hex::encode(iss_b), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Leg A's book is the only source on that leg, so it must be consumed;
        // book-only admission crosses nothing at all and rests the lot.
        let a_left = json_at(&sandbox, &keylet::offer_key(&mk_a, 2))
            .and_then(|o| o["TakerGets"].as_str().map(|s| s.to_string()));
        assert_ne!(
            a_left.as_deref(), Some("1000000"),
            "leg A's book maker must be crossed — the strand was admitted on leg B's spot",
        );
        let rested = json_at(&sandbox, &keylet::offer_key(&taker, 9));
        let gets_left = rested.as_ref().and_then(|o| o["TakerGets"]["value"].as_str());
        assert_ne!(gets_left, Some("99.5"), "and the taker's offer cannot rest in full");
    }

    /// A maker with NO trust line for what it is selling holds ZERO of it, and
    /// its offer must be reaped.
    ///
    /// `reap_if_dead` refuses to condemn a maker whose funding it cannot see —
    /// an unhydrated maker reading as unfunded caused phantom deletions once
    /// already. But a maker whose ACCOUNT ROOT we hold plainly WAS hydrated,
    /// so a missing trust line for the sold currency is the answer, not a gap.
    ///
    /// #106096771 69A1FF138D85: rDaQRnUv rests three STSH offers across three
    /// quality levels and has no STSH trust line at all — `ledger_entry`
    /// returns entryNotFound on mainnet itself. rippled logs `Removing unfunded
    /// offer` for each; we read "cannot judge" and left all three plus their
    /// emptied book pages, 8 mutations against 16. The same book cost the
    /// Payment 9EBC82AB5041 two more, and one fix closed both.
    #[test]
    fn a_maker_with_no_line_for_what_it_sells_is_reaped() {
        let taker = [0x01u8; 20];
        let mkr = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for id in [&mkr, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let line = |who: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < &issuer { (*who, issuer) } else { (issuer, *who) };
            serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &issuer { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            })
        };
        // The TAKER has a USD line to receive on. The MAKER has none at all —
        // its AccountRoot is present, so this is absence, not ignorance.
        state
            .state_map
            .insert(keylet::ripple_state_key(&taker, &issuer, &cur), serde_json::to_vec(&line(&taker, "0")).unwrap())
            .unwrap();

        // Place the maker's offer while it still has the USD to back it, then
        // take the line away — on chain the account simply never had one.
        state
            .state_map
            .insert(keylet::ripple_state_key(&mkr, &issuer, &cur), serde_json::to_vec(&line(&mkr, "100")).unwrap())
            .unwrap();
        let mut sandbox = Sandbox::new(&state);
        let mk = TxFields {
            account: mkr, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                // 1e6 drops per USD — WORSE than the taker's 9e5 limit below,
                // so the crossing walk never enters this level.
                "TakerPays": "100000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&mk, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();
        state.state_map.delete(&keylet::ripple_state_key(&mkr, &issuer, &cur)).unwrap();

        // A 500 XRP / 1000 USD pool, priced inside the taker's limit, so the
        // strand is built and the level scan runs.
        let pool = [0x06u8; 20];
        let a = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(pool),
            "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
        });
        state.state_map.insert(keylet::account_root_key(&pool), serde_json::to_vec(&a).unwrap()).unwrap();
        state
            .state_map
            .insert(keylet::ripple_state_key(&pool, &issuer, &cur), serde_json::to_vec(&line(&pool, "1000")).unwrap())
            .unwrap();
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let usd_leg = leg_of(&serde_json::json!({
            "currency": "USD", "issuer": hex::encode(issuer), "value": "1"
        }))
        .unwrap();
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(pool), "TradingFee": 0,
        });
        let akey = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &usd_leg.cur, &usd_leg.issuer);
        state.state_map.insert(akey, serde_json::to_vec(&amm).unwrap()).unwrap();

        let okey = keylet::offer_key(&mkr, 2);
        let mut sandbox = Sandbox::new(&state);
        assert!(sandbox.exists(&okey), "the maker's offer starts on the book");
        assert!(
            json_at(&sandbox, &keylet::ripple_state_key(&mkr, &issuer, &cur)).is_none(),
            "and the maker has no line for the USD it sells",
        );

        // Cross PAST the maker's level: it asks 1e6 drops/USD and the taker
        // will pay only 0.9e6, so the crossing walk never reaches it and
        // cannot delete it on the way through. Only the level scan can, and
        // the scan runs at all because the POOL (0.5e6 drops/USD) keeps the
        // strand inside the limit — the #105922825 shape.
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 9,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "90000000",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        assert!(
            !sandbox.exists(&okey),
            "an offer backed by no line at all must be reaped, even on a level \
             the crossing never enters",
        );
    }

    /// A bridge leg with a POOL but NO BOOK cannot carry the pass at all.
    ///
    /// The composition gate was written `if let (Some(a), Some(b), Some(t)) =
    /// (qa_book, qb_book, thr)`, so an empty leg book made `qb_book` None, the
    /// pattern failed, and the gate was SKIPPED — the bridge then crossed on
    /// that leg's pool alone. The reasoning above applies with more force, not
    /// less: off the default path the step keeps going into that leg's CLOB and
    /// StrandFlow judges the pass as a whole, so with no CLOB behind the pool
    /// there is nothing for it to clear.
    ///
    /// All 8 cases in the fresh batch were Flags=65536 exactly (tfPassive),
    /// buy side, IOU<->IOU with no XRP leg — every one bridged. #105813899
    /// 44E799C6FF9B: lb=0, bq=1889698131091803e-12 just inside
    /// thr=1890192915687964e-12, so one AMM slice crossed and the next
    /// iteration broke — 7 extra mutations, nothing missing, where mainnet
    /// moved no value and simply rested the offer.
    /// A leg-B book maker can only deliver what it HOLDS.
    ///
    /// Leg A has clamped its maker to `available()` since the bridge was built;
    /// leg B took the offer's whole TakerPays as capacity and never consulted
    /// the maker, because `live_head` tests funding only for ZERO. An
    /// underfunded maker therefore passed straight through at full size and its
    /// trust line went NEGATIVE — a non-issuer minting the currency.
    ///
    /// #106146362 75511674AD58 is the live case: `rwnJpjMn18m7xd` holds
    /// 0.64751384169623 RLUSD and rests TWO offers against it (9.328507 and
    /// 12.735909). We drove a whole 1 RLUSD through the first and wrote that
    /// line to -0.35248615830377; rippled delivers exactly the balance, then
    /// logs `Removing became unfunded offer` for the second and walks on.
    ///
    /// ★ The differential probe compares mutation KEYS, so it reported this as
    /// "5 missing nodes" — the minting itself was invisible to it. That is why
    /// this assertion is on the VALUE.
    #[test]
    fn a_bridge_leg_b_maker_cannot_deliver_more_than_it_holds() {
        let taker = [0x01u8; 20];
        let iss_a = [0x02u8; 20];
        let iss_b = [0x03u8; 20];
        let mk_a = [0x04u8; 20];
        let mk_b = [0x05u8; 20];
        let (mut ca, mut cb) = ([0u8; 20], [0u8; 20]);
        ca[12..15].copy_from_slice(b"AAA");
        cb[12..15].copy_from_slice(b"BBB");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for id in [&iss_a, &iss_b, &mk_a, &mk_b] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut line = |who: &[u8; 20], iss: &[u8; 20], cur: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < iss { (*who, *iss) } else { (*iss, *who) };
            let v = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < iss { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, iss, cur), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        line(&taker, &iss_a, &ca, "1000"); // the taker's AAA to sell
        line(&taker, &iss_b, &cb, "0");    // and where it receives BBB
        line(&mk_a, &iss_a, &ca, "0");     // leg A maker can receive AAA
        // ★ leg B's maker rests an offer for 10 BBB while holding only 3.
        line(&mk_b, &iss_b, &cb, "3");

        // Leg A: mk_a gives 10 XRP for 5 AAA. Leg B: mk_b gives 10 BBB for
        // 5 XRP. Composed that is 0.25 AAA/BBB, well inside the taker's limit
        // of 2, so nothing here is decided by the quality judge.
        for (who, seq, pays, gets) in [
            (mk_a, 2u32, serde_json::json!({"currency":"AAA","issuer":hex::encode(iss_a),"value":"5"}),
                        serde_json::json!("10000000")),
            (mk_b, 2u32, serde_json::json!("5000000"),
                        serde_json::json!({"currency":"BBB","issuer":hex::encode(iss_b),"value":"10"})),
        ] {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: seq,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({"TakerPays": pays, "TakerGets": gets}),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
        }

        // Sell 10 AAA for 5 BBB. Leg B can only fund 3 of those 5.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 9,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "AAA", "issuer": hex::encode(iss_a), "value": "10"},
                "TakerPays": {"currency": "BBB", "issuer": hex::encode(iss_b), "value": "5"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let bal = |sb: &Sandbox, who: &[u8; 20], iss: &[u8; 20], cur: &[u8; 20]| -> f64 {
            json_at(sb, &keylet::ripple_state_key(who, iss, cur))
                .and_then(|v| v["Balance"]["value"].as_str().and_then(|s| s.parse::<f64>().ok()))
                .unwrap_or(0.0)
                .abs()
        };
        let maker_left = bal(&sandbox, &mk_b, &iss_b, &cb);
        let taker_got = bal(&sandbox, &taker, &iss_b, &cb);
        assert!(
            maker_left < 1e-9,
            "the leg B maker must be drained to zero, never past it: {maker_left}",
        );
        assert!(
            taker_got <= 3.0 + 1e-9,
            "the taker cannot receive more BBB than the maker held (3): got {taker_got}",
        );
    }

    #[test]
    fn a_bridge_leg_with_a_pool_but_no_book_carries_nothing() {
        let taker = [0x01u8; 20];
        let mk_a = [0x04u8; 20];
        let pool_b = [0x07u8; 20];
        let iss_a = [0x02u8; 20];
        let iss_b = [0x03u8; 20];
        let (mut ca, mut cb) = ([0u8; 20], [0u8; 20]);
        ca[12..15].copy_from_slice(b"AAA");
        cb[12..15].copy_from_slice(b"BBB");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for (id, bal) in [(&mk_a, "500000000"), (&iss_a, "500000000"),
                          (&iss_b, "500000000"), (&pool_b, "100000000")] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": bal, "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut line = |who: &[u8; 20], iss: &[u8; 20], cur: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < iss { (*who, *iss) } else { (*iss, *who) };
            let v = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < iss { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, iss, cur), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        line(&taker, &iss_a, &ca, "1000");     // the taker's AAA to sell
        line(&mk_a, &iss_a, &ca, "0");         // leg A maker can receive AAA
        line(&pool_b, &iss_b, &cb, "10000");   // leg B pool: 100 XRP / 10000 BBB

        // Leg B pool at 0.01 XRP per BBB. There is deliberately NO leg B book.
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(pool_b), "TradingFee": 0,
        });
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let b_leg = leg_of(&serde_json::json!({"currency":"BBB","issuer":hex::encode(iss_b),"value":"1"})).unwrap();
        let bkey = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &b_leg.cur, &b_leg.issuer);
        state.state_map.insert(bkey, serde_json::to_vec(&amm).unwrap()).unwrap();

        // Leg A book only: 1 XRP for 1 AAA.
        {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: mk_a, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({
                    "TakerPays": {"currency":"AAA","issuer":hex::encode(iss_a),"value":"1"},
                    "TakerGets": "1000000",
                }),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
        }

        let pool_xrp_before = read_balance(&Sandbox::new(&state), &pool_b);

        // Sell 10 AAA for 100 BBB — limit 0.1 AAA/BBB. Composed through leg A's
        // book (1 AAA/XRP) and leg B's POOL (0.01 XRP/BBB) that is 0.01, well
        // inside the limit — so without the gate the pass crosses.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 9,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "AAA", "issuer": hex::encode(iss_a), "value": "10"},
                "TakerPays": {"currency": "BBB", "issuer": hex::encode(iss_b), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        assert_eq!(
            read_balance(&sandbox, &pool_b), pool_xrp_before,
            "the pool behind an empty leg book must not be touched",
        );
        assert_eq!(
            json_at(&sandbox, &keylet::offer_key(&mk_a, 2)).unwrap()["TakerGets"].as_str(),
            Some("1000000"),
            "leg A's book maker must be untouched",
        );
        let rested = json_at(&sandbox, &keylet::offer_key(&taker, 9)).expect("offer rests");
        assert_eq!(rested["TakerGets"]["value"].as_str(), Some("10"), "and rests in full");
    }

    /// Mainnet tx 9BA91A9E… (ledger 105777146): the WETH issuer publishes
    /// TickSize 6, so the rate rounds up to 6 significant digits and the
    /// non-exact side is re-derived. rippled re-derives with the STAmount
    /// `divide` — truncating muldiv at 10^17 then `+5` — NOT the Number-exact
    /// half-even the fill path uses, and the two disagree in the last digit.
    /// Mainnet stored TakerGets 0.0005909397123305481, one ulp ABOVE the
    /// half-even result, which is what makes the rate encode to `…ABFB5800`.
    /// With the fill-path divide we derived `…480` and rested the offer one
    /// book level off at `…ABFB5801` — the ledger's sole divergence (4v4).
    #[test]
    fn tick_rounding_re_derives_with_the_stamount_divide() {
        let acct = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..16].copy_from_slice(b"WETH");

        let mut state = make_state_with_account(&acct, 500_000_000);
        let iss_acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "50000000", "Sequence": 1, "OwnerCount": 0, "TickSize": 6,
        });
        state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&iss_acct).unwrap()).unwrap();
        let (lo, hi) = if acct < issuer { (acct, issuer) } else { (issuer, acct) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if acct < issuer { "1" } else { "-1" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(keylet::ripple_state_key(&acct, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 5,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "1000000",
                "TakerGets": {"currency": "WETH", "issuer": hex::encode(issuer), "value": "0.00059094"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let placed = json_at(&sandbox, &keylet::offer_key(&acct, 5)).expect("offer placed");
        assert_eq!(placed["TakerGets"]["value"].as_str(), Some("0.0005909397123305481"));
        let q = keylet::offer_quality(&placed["TakerPays"], &placed["TakerGets"]).unwrap();
        assert_eq!(format!("{q:016X}"), "5E060310ABFB5800");
    }

    /// A book level holding nothing but a dead offer, with a pool that can
    /// satisfy the whole fill on its own.
    ///
    /// Returns `(state, taker, maker, legs)`: the maker rests 100 USD for
    /// 100 XRP and that offer has expired, while the XRP/USD pool is deep and
    /// priced far better, so the pool alone covers the taker's 1 XRP.
    fn state_with_expired_head_and_pool(
    ) -> (LedgerState, [u8; 20], [u8; 20], Leg, Leg) {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let pool = [0x05u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 50_000_000);
        for id in [&maker, &issuer, &pool] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let line = |who: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < &issuer { (*who, issuer) } else { (issuer, *who) };
            serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &issuer { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "10000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "10000000"},
            })
        };
        for (who, bal) in [(&maker, "100"), (&pool, "1000")] {
            state
                .state_map
                .insert(keylet::ripple_state_key(who, &issuer, &cur), serde_json::to_vec(&line(who, bal)).unwrap())
                .unwrap();
        }

        // The maker rests 100 USD for 100 XRP — the book's only level.
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "100000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // It expired before the base ledger closed (close_time 10).
        let okey = keylet::offer_key(&maker, 2);
        {
            let mut o = json_at(&Sandbox::new(&state), &okey).expect("maker offer");
            o["Expiration"] = serde_json::json!(5);
            state.state_map.insert(okey, serde_json::to_vec(&o).unwrap()).unwrap();
        }

        // A 500 XRP / 1000 USD pool, priced far inside the dead level.
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let usd_leg = leg_of(&serde_json::json!({
            "currency": "USD", "issuer": hex::encode(issuer), "value": "1"
        }))
        .unwrap();
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(pool), "TradingFee": 0,
        });
        let akey = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &usd_leg.cur, &usd_leg.issuer);
        state.state_map.insert(akey, serde_json::to_vec(&amm).unwrap()).unwrap();

        (state, taker, maker, usd_leg, xrp_leg)
    }

    /// rippled steps the offer stream BEFORE it consults the pool, and every
    /// dead offer stepped past is removed whether or not anything crossed
    /// (`BookStep::forEachOffer` BookStep.cpp:835-852, `OfferStream::step`
    /// OfferStream.cpp:192). We consulted the pool first and broke out the
    /// moment it satisfied the fill, so a level whose page we never read kept
    /// its dead offers. #105795716 5CDFDC74: the XRP→RPLS book's only level
    /// held an offer that expired 1h50m before the parent closed; mainnet
    /// reaped it — offer and emptied book page Deleted, maker's root and owner
    /// dir Modified, and no RippleState, so nothing crossed — then filled from
    /// the pool (8 muts vs 12).
    #[test]
    fn dead_book_level_is_reaped_even_when_the_pool_fills_everything() {
        let (state, taker, maker, usd_leg, xrp_leg) = state_with_expired_head_and_pool();
        let okey = keylet::offer_key(&maker, 2);
        let mut sandbox = Sandbox::new(&state);
        assert!(sandbox.exists(&okey), "the expired offer starts on the book");

        let mut stale = Vec::new();
        // Payment semantics: no limitQuality, so the pool is free to fill it all.
        let (rp, rg, _crossed) = cross_engine_to(
            &taker, &taker, (1_000_000_000_000_000, -15), (1_000_000, 0), &usd_leg, &xrp_leg,
            u64::MAX, u64::MAX, false, false, false, None, None, &mut sandbox, &mut stale,);

        assert!(!sandbox.exists(&okey), "the expired offer must be reaped");
        assert!(stale.contains(&okey), "and reported as removed for the cancel view");
        // The reap is bookkeeping, not a crossing: the maker's reserve is
        // released but it never sold anything, so its USD is untouched.
        let mroot = json_at(&sandbox, &keylet::account_root_key(&maker)).unwrap();
        assert_eq!(mroot["OwnerCount"].as_u64().unwrap(), 0);
        assert!(me_cmp(available(&mut sandbox, &maker, &usd_leg), (100_000_000_000_000_000, -15)).is_eq());
        // The pool alone delivered the full 1 USD — which is why the page was
        // never read and the dead offer used to survive.
        assert!(me_is_zero(rp), "the pool covered the whole USD request");
        assert!(me_cmp(rg, (1_000_000, 0)).is_lt(), "and was paid for in XRP");
    }

    /// ...but ONLY when the crossing actually opens the book. An offer-crossing
    /// strand whose best possible quality is worse than the taker's limit is
    /// dropped before `flow()` runs (StrandFlow.h:682-690), so no stream is
    /// built and nothing is reaped. #105795013 428E0550 sells RLUSD priced
    /// above the entire book: mainnet rests it in 4 nodes and leaves the
    /// expired E39542EC for its owner's own later 612F4E95 to clear. Reaping
    /// it here moved 4 mutations into the wrong transaction.
    #[test]
    fn dead_offer_survives_a_taker_priced_past_the_whole_book() {
        let (state, taker, maker, usd_leg, xrp_leg) = state_with_expired_head_and_pool();
        let okey = keylet::offer_key(&maker, 2);
        let mut sandbox = Sandbox::new(&state);

        let mut stale = Vec::new();
        // `threshold` 1 is better than any real book level, so every level is
        // past the limit and the strand never runs.
        cross_engine_to(
            &taker, &taker, (1_000_000_000_000_000, -15), (1_000_000, 0), &usd_leg, &xrp_leg,
            1, 1, true, true, false, None, None, &mut sandbox, &mut stale,);

        assert!(sandbox.exists(&okey), "an unopened book must not be reaped");
        assert!(stale.is_empty());
    }

    /// The dead tests apply to the taker's OWN offer too. `OfferStream::step`
    /// has no owner==taker case — every condition reads `offer_.owner()` and
    /// none compares it to the taker — so a self-owned offer that is UNFUNDED
    /// is reaped exactly like a stranger's. Only a LIVE self-owned offer stops
    /// the scan, because cancelling that is offer crossing's job.
    ///
    /// #105949459 4A03010A4B1E: `rKkBNf2d` buys 63 ShearPepe holding NONE, and
    /// its own `EC95059B` heads that book promising 63 it does not have. The
    /// pool filled all 708735 drops so the page was never read, and the
    /// `maker == taker` bail then stopped the level scan from reaping it —
    /// 4 mutations against 7. rippled logs `Removing unfunded offer EC95059B…`.
    /// A PAYMENT consumes the payer's own offer; only OFFER CROSSING cancels
    /// it. rippled says so outright — `BookPaymentStep::limitSelfCrossQuality`
    /// is `{ return false; }` under the comment "Never limit self cross
    /// quality on a payment" (BookStep.cpp:295-308), while the removal lives
    /// only in `BookOfferCrossingStep`.
    ///
    /// #106137720 `F2C989D4843A`: a circular-arb Payment (Account ==
    /// Destination) sells CNY for XRP with tfPartialPayment. The CNY→XRP book
    /// holds exactly ONE offer and the payer owns it, worth 960 852 drops
    /// against a DeliverMin of 1 937 153 — so mainnet crosses it, falls short,
    /// and returns tecPATH_PARTIAL. We cancelled the offer unconditionally,
    /// found the book empty, and returned tecPATH_DRY.
    fn self_offer_book(
    ) -> (LedgerState, [u8; 20], [u8; 20], Leg, Leg, Hash256) {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 500_000_000);
        let a = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
        });
        state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&a).unwrap()).unwrap();
        let (lo, hi) = if taker < issuer { (taker, issuer) } else { (issuer, taker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur),
                        "issuer": "0000000000000000000000000000000000000000",
                        "value": if taker < issuer { "100".to_string() } else { "-100".to_string() }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "10000000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "10000000"},
        });
        state.state_map
            .insert(keylet::ripple_state_key(&taker, &issuer, &cur), serde_json::to_vec(&line).unwrap())
            .unwrap();

        // The payer rests the book's ONLY level: 100 XRP offered for 100 USD.
        // Far larger than the request below, so a crossing leaves it standing.
        let mut sandbox = Sandbox::new(&state);
        let own = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "100000000",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&own, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let usd_leg = leg_of(&serde_json::json!({
            "currency": "USD", "issuer": hex::encode(issuer), "value": "1"
        }))
        .unwrap();
        let okey = keylet::offer_key(&taker, 2);
        (state, taker, issuer, usd_leg, xrp_leg, okey)
    }

    #[test]
    fn a_payment_crosses_the_payers_own_offer() {
        let (state, taker, _issuer, usd_leg, xrp_leg, okey) = self_offer_book();
        let mut sandbox = Sandbox::new(&state);
        assert!(sandbox.exists(&okey), "the payer's own offer is the book");

        let mut stale = Vec::new();
        // offer_crossing = false → payment semantics.
        let (rp, _rg, crossed) = cross_engine_to(
            &taker, &taker, (1_000_000, 0), (1_000_000_000_000_000, -15), &xrp_leg, &usd_leg,
            u64::MAX, u64::MAX, false, false, false, None, None, &mut sandbox, &mut stale,
        );
        assert!(crossed > 0, "a payment must CROSS the payer's own offer, not skip it");
        assert!(me_is_zero(rp), "and deliver the requested XRP");
        assert!(sandbox.exists(&okey), "the offer is consumed, not cancelled");
        assert!(!stale.contains(&okey), "and never reported as removed");
    }

    #[test]
    fn offer_crossing_cancels_the_takers_own_offer_instead() {
        let (state, taker, _issuer, usd_leg, xrp_leg, okey) = self_offer_book();
        let mut sandbox = Sandbox::new(&state);
        let mut stale = Vec::new();
        // offer_crossing = true → the older own offer is cancelled.
        let (rp, _rg, crossed) = cross_engine_to(
            &taker, &taker, (1_000_000, 0), (1_000_000_000_000_000, -15), &xrp_leg, &usd_leg,
            u64::MAX, u64::MAX, false, true, false, None, None, &mut sandbox, &mut stale,
        );
        assert_eq!(crossed, 0, "nothing crosses: the only level was the taker's own");
        assert!(!me_is_zero(rp), "so the request goes unfilled");
        assert!(!sandbox.exists(&okey), "the own offer is cancelled");
        assert!(stale.contains(&okey), "and reported as removed");
    }

    #[test]
    fn the_takers_own_unfunded_offer_is_reaped_when_the_pool_fills_everything() {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let pool = [0x05u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 50_000_000);
        for id in [&issuer, &pool] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let line = |who: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < &issuer { (*who, issuer) } else { (issuer, *who) };
            serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &issuer { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "10000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "10000000"},
            })
        };
        for (who, bal) in [(&taker, "100"), (&pool, "1000")] {
            state
                .state_map
                .insert(keylet::ripple_state_key(who, &issuer, &cur), serde_json::to_vec(&line(who, bal)).unwrap())
                .unwrap();
        }

        // The TAKER rests 100 USD for 100 XRP — the book's only level. It has
        // to be funded to be placed at all (`tecUNFUNDED_OFFER`), so the USD
        // goes afterwards, exactly as it does on chain when the account sells
        // its holding and leaves the offer standing.
        let mut sandbox = Sandbox::new(&state);
        let own = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "100000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&own, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();
        state
            .state_map
            .insert(keylet::ripple_state_key(&taker, &issuer, &cur), serde_json::to_vec(&line(&taker, "0")).unwrap())
            .unwrap();

        // A 500 XRP / 1000 USD pool, priced far inside that level.
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let usd_leg = leg_of(&serde_json::json!({
            "currency": "USD", "issuer": hex::encode(issuer), "value": "1"
        }))
        .unwrap();
        let amm = serde_json::json!({
            "LedgerEntryType": "AMM", "Account": hex::encode(pool), "TradingFee": 0,
        });
        let akey = keylet::amm_key(&xrp_leg.cur, &xrp_leg.issuer, &usd_leg.cur, &usd_leg.issuer);
        state.state_map.insert(akey, serde_json::to_vec(&amm).unwrap()).unwrap();

        let okey = keylet::offer_key(&taker, 2);
        let mut sandbox = Sandbox::new(&state);
        assert!(sandbox.exists(&okey), "the taker's own offer starts on the book");
        assert!(me_is_zero(available(&mut sandbox, &taker, &usd_leg)), "and is unfunded");

        let mut stale = Vec::new();
        let (rp, _rg, _crossed) = cross_engine_to(
            &taker, &taker, (1_000_000_000_000_000, -15), (1_000_000, 0), &usd_leg, &xrp_leg,
            u64::MAX, u64::MAX, false, false, false, None, None, &mut sandbox, &mut stale,);

        assert!(!sandbox.exists(&okey), "the taker's own unfunded offer must be reaped");
        assert!(stale.contains(&okey), "and reported as removed for the cancel view");
        assert!(me_is_zero(rp), "the pool covered the whole USD request");
    }

    /// One book level holding two identical offers — 100 USD for 100 XRP —
    /// from two makers, funded as given, `first` resting first.
    /// Build a book holding two offers owned by `taker` (the better one first)
    /// and, optionally, a BETTER one owned by someone else ahead of both.
    fn state_with_own_offers(foreign_head: bool) -> (LedgerState, [u8; 20], Vec<(u64, Hash256)>) {
        let taker = [0x01u8; 20];
        let other = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for id in [&other, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        for who in [&taker, &other] {
            let (lo, hi) = if who < &issuer { (*who, issuer) } else { (issuer, *who) };
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &issuer { "1000".to_string() } else { "-1000".to_string() }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();
        }
        // All on the SAME side of the book (give USD, want XRP), so none of
        // them cross each other; a bigger TakerPays is a worse level.
        let mut plan: Vec<([u8; 20], u32, &str)> = vec![(taker, 2, "100000000"), (taker, 3, "200000000")];
        if foreign_head {
            plan.insert(0, (other, 2, "50000000"));
        }
        let mut keys: Vec<(u64, Hash256)> = Vec::new();
        for (who, seq, pays) in plan {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: seq,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({
                    "TakerPays": pays,
                    "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
                }),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
            let okey = keylet::offer_key(&who, seq);
            let o = json_at(&Sandbox::new(&state), &okey).expect("offer written");
            // The engine does not stamp BookDirectory into the offer it rests,
            // so derive the level the same way it does when inserting.
            let tp = keylet::amount_mant_exp(&o["TakerPays"]).unwrap();
            let tg = keylet::amount_mant_exp(&o["TakerGets"]).unwrap();
            keys.push((rate_of_me(tp, tg).unwrap(), okey));
        }
        keys.sort_by_key(|(q, _)| *q);
        (state, taker, keys)
    }

    /// rippled DELETES our own offers off the book tip rather than crossing
    /// them (BookStep.cpp:415-455) — but only those inside the taker's limit:
    /// `offer.quality() >= qualityThreshold_`. That gate is the whole difficulty
    /// here. Two earlier attempts removed self-offers unconditionally; each
    /// closed #105949459 and broke #105808080, netting zero, because they also
    /// removed offers priced outside the limit.
    #[test]
    fn a_self_offer_is_removed_at_the_tip_only_within_the_limit() {
        let (state, taker, keys) = state_with_own_offers(false);
        let mut sb = Sandbox::new(&state);
        assert_eq!(keys.len(), 2);
        let mut stale = Vec::new();

        // Threshold admits the tip only — the second, worse level is outside it.
        reap_self_offers_at_head(&mut sb, &keys, 0, &taker, keys[0].0, &mut stale);
        assert!(json_at(&sb, &keys[0].1).is_none(), "the tip is ours and inside the limit: removed");
        assert!(
            json_at(&sb, &keys[1].1).is_some(),
            "the next level is ours too but priced OUTSIDE the limit — rippled leaves it",
        );
        assert_eq!(stale, vec![keys[0].1]);

        // Widen the limit to cover both and the second one goes as well.
        let mut sb = Sandbox::new(&state);
        let mut stale = Vec::new();
        reap_self_offers_at_head(&mut sb, &keys, 0, &taker, keys[1].0, &mut stale);
        assert_eq!(stale.len(), 2, "both levels inside the limit are removed");
    }

    /// Removal operates on the TIP only. A stranger's offer ahead of ours ends
    /// the walk: rippled crosses that one normally and never reaches ours.
    #[test]
    fn a_foreign_offer_at_the_tip_ends_self_offer_removal() {
        let (state, taker, keys) = state_with_own_offers(true);
        let mut sb = Sandbox::new(&state);
        assert_eq!(keys.len(), 3);
        let mut stale = Vec::new();
        // Threshold wide enough to admit every level.
        reap_self_offers_at_head(&mut sb, &keys, 0, &taker, keys[2].0, &mut stale);
        assert!(stale.is_empty(), "the tip belongs to someone else, so nothing is removed");
        for (_, k) in &keys {
            assert!(json_at(&sb, k).is_some());
        }
    }

    /// An "unbounded" want-cap CANNOT be recovered by subtraction, so nothing
    /// may account for a multi-fill total that way. `me_rescale` saturates
    /// (`saturating_mul`), and a sentinel that huge saturates on every fill
    /// whose exponent is smaller — which resets the running remainder to
    /// u128::MAX and ERASES the fills before it. Sixteen significant digits
    /// cannot represent 1e76 minus 2.3; no choice of sentinel fixes it.
    ///
    /// This is why `PaymentTransactor` measures an intermediate hop's carry as
    /// the balance delta on the account's own line instead. #105912291
    /// 2AE3693EF556, a circular 1 XRP -> RLUSD -> DMNDBR partial payment, read
    /// its first hop as 0.0231 RLUSD when 2.309 had been bought — the last
    /// fill's mantissa at the last fill's exponent — and failed
    /// tecPATH_PARTIAL where mainnet delivers in full.
    #[test]
    fn an_unbounded_want_cap_cannot_be_recovered_by_subtraction() {
        const CAP: Me = (9_990_000_000_000_000, 60);

        // ONE fill round-trips, which is what makes the bug survivable in
        // simple cases and invisible in single-hop tests.
        let one = me_sub(CAP, (2_309_435_512_000_000, -15));
        assert_eq!(me_sub(CAP, one), (2_309_435_512_000_000, -15));

        // A SECOND fill at a smaller exponent re-saturates and erases the first.
        let two = me_sub(one, (1_062_984_355_120_000, -17));
        assert_eq!(
            me_sub(CAP, two),
            (1_062_984_355_120_000, -17),
            "recovers only the LAST fill — the 2.309e-15 before it is gone",
        );

        // The saturation itself, stated plainly.
        assert_eq!(me_rescale(CAP, -15, false), u128::MAX);
    }

    fn state_with_two_offers(
        first_usd: &str,
        second_usd: &str,
        expire_second: bool,
        second_pays: &str,
    ) -> (LedgerState, [u8; 20], [u8; 20], [u8; 20], Leg, Leg) {
        let taker = [0x01u8; 20];
        let m1 = [0x04u8; 20];
        let m2 = [0x06u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 500_000_000);
        for id in [&m1, &m2, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        for (who, bal) in [(&m1, first_usd), (&m2, second_usd)] {
            let (lo, hi) = if who < &issuer { (*who, issuer) } else { (issuer, *who) };
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &issuer { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();
        }
        // Equal `TakerPays` ⇒ equal quality ⇒ one shared book page, m1 first;
        // a larger `second_pays` puts m2 on its own, worse level instead.
        for (who, pays) in [(&m1, "100000000"), (&m2, second_pays)] {
            let mut sandbox = Sandbox::new(&state);
            let tx = TxFields {
                account: *who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
                ticket_seq: None, last_ledger_seq: None,
                fields: serde_json::json!({
                    "TakerPays": pays,
                    "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
                }),
            };
            assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
        }
        if expire_second {
            let okey = keylet::offer_key(&m2, 2);
            let mut o = json_at(&Sandbox::new(&state), &okey).expect("second offer");
            o["Expiration"] = serde_json::json!(5);
            state.state_map.insert(okey, serde_json::to_vec(&o).unwrap()).unwrap();
        }
        let xrp_leg = leg_of(&serde_json::json!("1")).unwrap();
        let usd_leg = leg_of(&serde_json::json!({
            "currency": "USD", "issuer": hex::encode(issuer), "value": "1"
        }))
        .unwrap();
        (state, taker, m1, m2, usd_leg, xrp_leg)
    }

    /// A satisfied fill does not end the walk. rippled's reverse pass returns
    /// true from the offer callback whenever the step's output fits inside what
    /// is still wanted — "return true b/c even if the payment is satisfied, we
    /// need to consume the offer" (BookStep.cpp:1036) — so `offers.step()` runs
    /// again and reaps the dead offers behind the one just consumed. We stopped
    /// the instant the taker's input was spent. #105778999 6B2A11B3 crossed its
    /// 140 XRP against the page's one funded offer exactly as mainnet did, then
    /// left mainnet's three reaped spam offers untouched (5v15).
    #[test]
    fn a_satisfied_fill_still_reaps_the_offers_behind_it() {
        let (state, taker, m1, m2, usd_leg, xrp_leg) = state_with_two_offers("100", "100", true, "100000000");
        let (live, dead) = (keylet::offer_key(&m1, 2), keylet::offer_key(&m2, 2));
        let mut sandbox = Sandbox::new(&state);

        let mut stale = Vec::new();
        // 10 XRP to spend against a sentinel want, so the fill is bounded by
        // the taker's INPUT — never trimmed to the remaining output.
        let (_rp, rg, _c) = cross_engine_to(
            &taker, &taker, (1_000_000_000_000_000, -8), (10_000_000, 0), &usd_leg, &xrp_leg,
            u64::MAX, u64::MAX, false, false, false, None, None, &mut sandbox, &mut stale,);

        assert!(me_is_zero(rg), "the 10 XRP is spent");
        assert!(sandbox.exists(&live), "the funded maker is only part-filled");
        assert!(!sandbox.exists(&dead), "the expired offer behind it must be reaped");
        assert!(stale.contains(&dead));
    }

    /// `shouldRmSmallIncreasedQOffer` (OfferStream.cpp:136, applied at :302):
    /// an offer whose owner-funds-limited fill floors away is removed, not
    /// crossed. #105778999's three deleted offers each sold 5,907,469.49 POSAA
    /// for 343.742177 XRP on 0.0028–0.0103 POSAA of backing — ~0.36 drops of
    /// input, which floors to zero. Mainnet deleted them and moved no balance;
    /// we saw a non-zero balance, called them funded, and crossed for a drop.
    #[test]
    fn a_dust_funded_offer_is_reaped_rather_than_crossed() {
        let (state, taker, m1, _m2, usd_leg, xrp_leg) =
            state_with_two_offers("0.0000001", "100", false, "100000000");
        let dusty = keylet::offer_key(&m1, 2);
        let mut sandbox = Sandbox::new(&state);
        let before = read_balance(&sandbox, &m1);

        let mut stale = Vec::new();
        cross_engine_to(
            &taker, &taker, (1_000_000_000_000_000, -8), (10_000_000, 0), &usd_leg, &xrp_leg,
            u64::MAX, u64::MAX, false, false, false, None, None, &mut sandbox, &mut stale,);

        assert!(!sandbox.exists(&dusty), "a dust-backed offer must be reaped");
        assert!(stale.contains(&dusty));
        assert_eq!(read_balance(&sandbox, &m1), before, "and never crossed for value");
    }

    /// "If offer crossing exhausted the account's funds don't create the offer"
    /// (OfferCreate.cpp:479-484). A crossing clamped to `accountFunds` that
    /// spends every drop leaves a residual the account cannot begin to fund,
    /// and rippled drops it rather than resting it — tesSUCCESS, no offer.
    /// #105803327 09DA2A02: 10.937718 XRP at OwnerCount 8 could spend 8337706
    /// drops against a 8921861-drop ask; it spent all of them, landed on its
    /// exact 2600000 reserve, and mainnet placed nothing while we rested an
    /// offer and a book page (10v8, both extra).
    #[test]
    fn a_crossing_that_exhausts_the_account_places_no_remainder() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        // 5 XRP at OwnerCount 0 ⇒ 1 XRP reserve ⇒ 4 XRP spendable.
        let mut state = make_state_with_account(&taker, 5_000_000);
        for id in [&maker, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
        });
        state.state_map.insert(keylet::ripple_state_key(&maker, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();

        // Maker sells 100 USD at 0.5 XRP each — better than the taker's limit.
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "50000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Taker asks for 9 USD and offers 5 XRP, but can only spend 4 — which
        // buys 8 USD at the maker's price. The 1 USD it still wants is the
        // residual that must NOT be placed.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 7,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "5000000",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "9"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        assert_eq!(read_balance(&sandbox, &taker), 1_000_000, "every spendable drop is gone");
        assert!(
            !sandbox.exists(&keylet::offer_key(&taker, 7)),
            "an exhausted account rests no remainder",
        );
    }

    /// The trailing step follows the stream across BOOK LEVELS, not just to the
    /// end of the level it was crossing on. `OfferStream::step` carries no
    /// quality test at all — it keeps reaping until it reaches a live offer,
    /// and only then does `execOffer`'s `checkQualityThreshold` end the walk
    /// (BookStep.cpp:851). #105798806 10465EB9: we crossed the one live offer
    /// on level 4f03f3de…, then stopped; mainnet went on to reap rfPBiFvFeBQ's
    /// 8CFBD89A on the NEXT level (4f03f3f5…), expired 28s before the parent
    /// closed — offer and its emptied page Deleted, owner dir and root
    /// Modified. 5v9, those exact four missing and nothing extra.
    #[test]
    fn the_trailing_step_crosses_book_levels() {
        // m2 wants 110 XRP for the same 100 USD, so it rests on its own,
        // strictly worse level — and it has expired.
        let (state, taker, m1, m2, usd_leg, xrp_leg) =
            state_with_two_offers("100", "100", true, "110000000");
        let (live, dead) = (keylet::offer_key(&m1, 2), keylet::offer_key(&m2, 2));
        let mut sandbox = Sandbox::new(&state);
        let level = |k: &Hash256| {
            let o = json_at(&sandbox, k).unwrap();
            keylet::offer_quality(&o["TakerPays"], &o["TakerGets"]).unwrap()
        };
        assert_ne!(level(&live), level(&dead), "the two offers must rest on different book levels");

        let mut stale = Vec::new();
        let (_rp, rg, _c) = cross_engine_to(
            &taker, &taker, (1_000_000_000_000_000, -8), (10_000_000, 0), &usd_leg, &xrp_leg,
            u64::MAX, u64::MAX, false, false, false, None, None, &mut sandbox, &mut stale,);

        assert!(me_is_zero(rg), "the 10 XRP is spent on the first level");
        assert!(sandbox.exists(&live), "the funded maker is only part-filled");
        assert!(!sandbox.exists(&dead), "the expired offer one level down must be reaped");
        assert!(stale.contains(&dead));
    }

    /// Build a state whose `who` sells USD for XRP: an AccountRoot at the
    /// given balance/OwnerCount plus a funded trust line, and an empty book.
    fn state_selling_usd(who: &[u8; 20], issuer: &[u8; 20], balance: u64, owner_count: u64)
        -> (LedgerState, [u8; 20])
    {
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");
        let mut state = make_state_with_account(who, balance);
        let a = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(who),
            "Balance": balance.to_string(), "Sequence": 1, "OwnerCount": owner_count,
        });
        state.state_map.insert(keylet::account_root_key(who), serde_json::to_vec(&a).unwrap()).unwrap();

        let iss = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "500000000", "Sequence": 1, "OwnerCount": 0,
        });
        state.state_map.insert(keylet::account_root_key(issuer), serde_json::to_vec(&iss).unwrap()).unwrap();

        let (lo, hi) = if who < issuer { (*who, *issuer) } else { (*issuer, *who) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if who < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
        });
        state.state_map.insert(keylet::ripple_state_key(who, issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();
        (state, cur)
    }

    fn sell_usd_tx(who: &[u8; 20], issuer: &[u8; 20], seq: u32, expiration: Option<u64>) -> TxFields {
        let mut fields = serde_json::json!({
            "TakerPays": "5000000",
            "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "10"},
        });
        if let Some(e) = expiration {
            fields["Expiration"] = serde_json::json!(e);
        }
        TxFields {
            account: *who, tx_type: "OfferCreate".to_string(), fee: 12, sequence: seq,
            ticket_seq: None, last_ledger_seq: None, fields,
        }
    }

    /// rippled tests the owner reserve against `preFeeBalance_` — the balance
    /// as of BEFORE this transaction's fee (OfferCreate.cpp:834-841) — while
    /// our check runs in `do_apply`, after the fee is already gone. At the
    /// boundary that is an off-by-one-fee rejection.
    ///
    /// #105845719 90E12EF294C9 (rMWVf1qJsgHgEd1Tuy378Zs53noKh4BujK): post-fee
    /// balance 2799988, OwnerCount 8, reserve(9) 2800000 — short by exactly the
    /// 12-drop fee. Mainnet placed the offer (4 nodes). That the same account's
    /// LATER OfferCreate in that ledger (index 46) really does earn
    /// tecINSUF_RESERVE_OFFER is what proves the reserve itself is right and
    /// only the pre/post-fee basis was wrong.
    #[test]
    fn the_owner_reserve_is_tested_against_the_pre_fee_balance() {
        let who = [0x01u8; 20];
        let issuer = [0x02u8; 20];

        // Exactly the mainnet arithmetic: reserve(8 + 1) = 2_800_000, and the
        // post-fee balance lands one fee below it.
        let reserve = XRP_RESERVE_BASE + XRP_RESERVE_INC * 9;
        assert_eq!(reserve, 2_800_000);
        let (state, _) = state_selling_usd(&who, &issuer, reserve as u64 - 12, 8);

        let mut sandbox = Sandbox::new(&state);
        let tx = sell_usd_tx(&who, &issuer, 7, None);
        assert_eq!(
            OfferCreateTransactor.do_apply(&tx, &mut sandbox),
            TxResult::Success,
            "pre-fee the account is exactly at reserve, so the offer rests",
        );
        assert!(sandbox.exists(&keylet::offer_key(&who, 7)));

        // One drop lower and even the pre-fee balance is short — the gate still
        // bites, so this is a shifted boundary and not a removed one.
        let (state, _) = state_selling_usd(&who, &issuer, reserve as u64 - 13, 8);
        let mut sandbox = Sandbox::new(&state);
        assert_eq!(
            OfferCreateTransactor.do_apply(&sell_usd_tx(&who, &issuer, 7, None), &mut sandbox),
            TxResult::InsufReserveOffer,
        );
    }

    /// Funding outranks expiry. rippled's OfferCreate::preclaim (:171) runs
    /// accountFunds -> tecUNFUNDED_OFFER BEFORE hasExpired -> tecEXPIRED, so an
    /// offer that is both unfunded AND expired answers tecUNFUNDED_OFFER.
    ///
    /// d753d7a had this backwards, reading OfferCreate.cpp:224-229's "it saves
    /// us a call to checkAcceptAsset and possible false negative" as "before
    /// funding" — but checkAcceptAsset comes after funding, so that comment
    /// only orders expiry ahead of checkAcceptAsset. #105950082 000702037C38
    /// and C79FCB34C1B7 are both expired and unfunded; mainnet says
    /// tecUNFUNDED_OFFER and we said tecEXPIRED.
    #[test]
    fn an_unfunded_expired_offer_answers_unfunded_not_expired() {
        let who = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        // Funded but expired -> tecEXPIRED (the control).
        let (state, _) = state_selling_usd(&who, &issuer, 100_000_000, 0);
        assert_eq!(
            OfferCreateTransactor.preclaim(&sell_usd_tx(&who, &issuer, 7, Some(9)), &Sandbox::new(&state)),
            TxResult::Expired,
            "funded and expired is still tecEXPIRED",
        );

        // Same transaction, but the USD line is empty: funding is checked first.
        let (mut state, _) = state_selling_usd(&who, &issuer, 100_000_000, 0);
        let key = keylet::ripple_state_key(&who, &issuer, &cur);
        let mut line: serde_json::Value =
            serde_json::from_slice(&state.state_map.lookup(&key).unwrap().to_vec()).unwrap();
        line["Balance"]["value"] = serde_json::json!("0");
        state.state_map.insert(key, serde_json::to_vec(&line).unwrap()).unwrap();

        assert_eq!(
            OfferCreateTransactor.preclaim(&sell_usd_tx(&who, &issuer, 7, Some(9)), &Sandbox::new(&state)),
            TxResult::UnfundedOffer,
            "unfunded outranks expired — rippled checks accountFunds first",
        );
    }

    /// `CreateOffer::preclaim` rejects an already-expired offer before it ever
    /// looks at funding (OfferCreate.cpp:224-229), and `hasExpired` is
    /// `parentCloseTime() >= exp` — inclusive (View.cpp:48-54).
    ///
    /// #105887283 17075103474C: Expiration 838475074 against a parent close of
    /// 838475151, so mainnet returned tecEXPIRED with no state touched; we ran
    /// on to the reserve check and answered tecINSUF_RESERVE_OFFER.
    #[test]
    fn an_offer_expiring_at_or_before_the_parent_close_is_dead_on_arrival() {
        let who = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        // make_state_with_account closes the base ledger at 10, which is the
        // parent close time of the ledger being replayed.
        let (state, _) = state_selling_usd(&who, &issuer, 100_000_000, 0);
        let sandbox = Sandbox::new(&state);

        for exp in [9u64, 10] {
            assert_eq!(
                OfferCreateTransactor.preclaim(&sell_usd_tx(&who, &issuer, 7, Some(exp)), &sandbox),
                TxResult::Expired,
                "Expiration {exp} is at or before the parent close of 10",
            );
        }
        assert_eq!(
            OfferCreateTransactor.preclaim(&sell_usd_tx(&who, &issuer, 7, Some(11)), &sandbox),
            TxResult::Success,
            "one tick past the parent close is still live",
        );
    }

    /// A trust line torn down mid-payment must leave BOTH owner directories,
    /// and both removals need the line's LowNode/HighNode page hint — exactly
    /// as TrustSet's trustDelete passes them.
    ///
    /// `dir_remove` returns immediately when the directory ROOT is not in the
    /// sandbox (directory.rs:274). The probe hydrates the page mainnet's meta
    /// names, not the whole chain, so for a big IOU issuer the named page is
    /// present while the root is not — and a hintless removal silently no-ops
    /// on precisely the issuer's side. A counterparty with one trust line has a
    /// single-page directory and never shows the bug.
    ///
    /// 8 Payments diverged on this, each missing exactly one Modified
    /// DirectoryNode and nothing else: six in #105854147 all missing 6ABA617A
    /// (owner rU5wZyCbZ2, the HIYO issuer) while the sender's own dir came out
    /// right, plus #105843539 BB9651ABA1B6 and #105872154 D217CCCAE1E3.
    #[test]
    fn a_line_deleted_mid_payment_leaves_the_issuers_page_too() {
        let party = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&party, 100_000_000);
        let iss = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "100000000", "Sequence": 1, "OwnerCount": 9,
        });
        state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&iss).unwrap()).unwrap();

        // The party holds 5 USD and is about to spend all of it. Their side
        // carries the reserve; the issuer's side does not, so the line reverts
        // to default on both sides and is deleted.
        let party_low = party < issuer;
        let line_key = keylet::ripple_state_key(&party, &issuer, &cur);
        let (lo, hi) = if party_low { (party, issuer) } else { (issuer, party) };
        let (reserve, no_ripple) = if party_low {
            (0x0001_0000u64, 0x0010_0000u64)
        } else {
            (0x0002_0000u64, 0x0020_0000u64)
        };
        // The line lives on PAGE 7 of the issuer's directory, page 0 of the
        // party's — the asymmetry the bug turns on.
        let (low_node, high_node) = if party_low { (0u64, 7u64) } else { (7u64, 0u64) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Flags": reserve | no_ripple,
            "Balance": {"currency": hex::encode_upper(cur),
                        "issuer": "0000000000000000000000000000000000000000",
                        "value": if party_low { "5" } else { "-5" }},
            "LowLimit":  {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo),
                          "value": if party_low { "0" } else { "0" }},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi),
                          "value": "0"},
            "LowNode": low_node,
            "HighNode": high_node,
        });
        state.state_map.insert(line_key, serde_json::to_vec(&line).unwrap()).unwrap();

        // The issuer's directory: page 7 is hydrated and holds the line; the
        // ROOT is deliberately absent, mirroring what the probe actually loads.
        let iss_root = keylet::owner_dir_key(&issuer);
        let iss_page7 = keylet::dir_page_key(&iss_root, 7);
        let page7 = serde_json::json!({
            "LedgerEntryType": "DirectoryNode",
            "Owner": hex::encode(issuer),
            "RootIndex": hex::encode_upper(iss_root.0),
            // A second, unrelated line keeps the page non-empty, so removal
            // leaves it Modified rather than Deleted — what mainnet shows for
            // an issuer holding thousands of lines.
            "Indexes": [hex::encode_upper(line_key.0), hex::encode_upper([0xEEu8; 32])],
        });
        state.state_map.insert(iss_page7, serde_json::to_vec(&page7).unwrap()).unwrap();
        assert!(
            state.state_map.lookup(&iss_root).is_none(),
            "the issuer's dir ROOT is intentionally unhydrated — that is the bug's precondition",
        );

        // The party's own single-page directory, root present.
        let mut sandbox = Sandbox::new(&state);
        crate::ledger::directory::owner_dir_insert(&mut sandbox, &party, &line_key);

        // Spend the whole 5 USD away: the line reverts to default and is deleted.
        let leg = Leg { xrp: false, cur, issuer };
        line_adjust(&mut sandbox, &party, &leg, (5_000_000_000_000_000, -15), false);

        assert!(!sandbox.exists(&line_key), "the emptied line is deleted");
        let page: serde_json::Value =
            serde_json::from_slice(&sandbox.read(&iss_page7).expect("issuer page 7 still exists")).unwrap();
        let idx = page["Indexes"].as_array().expect("page keeps its Indexes");
        assert_eq!(
            idx.len(), 1,
            "the hint must reach the issuer's page 7 — hintless, dir_remove bails on the absent root",
        );
        assert_eq!(
            idx[0].as_str().unwrap(),
            hex::encode_upper([0xEEu8; 32]),
            "the deleted line is gone; the unrelated one stays",
        );
    }

    /// An issuer with lsfRequireAuth cannot put its IOU into a line that lacks
    /// the ISSUER-side auth flag while that line is still EMPTY — all three
    /// conditions at once (DirectStep.cpp:430-437). An existing balance is
    /// grandfathered. Because the test runs at strand construction, a failure
    /// means the payment has NO path: tecPATH_DRY, not a short fill.
    ///
    /// 6 payments in the fresh batch answered tesSUCCESS where mainnet said
    /// tecPATH_DRY — #105933892 845CB9790984 and friends, self-payments
    /// converting XRP to RUBY under tfPartialPayment with no Paths. The
    /// XRP/RUBY book is EMPTY; the liquidity we found was the XRP/RUBY AMM
    /// (644 XRP). Issuer rG71TpU2 sets lsfRequireAuth and the senders' RUBY
    /// lines are unauthorized with balance 0.
    #[test]
    fn an_unauthorized_empty_line_can_receive_nothing() {
        let dest = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");
        let leg = Leg { xrp: false, cur, issuer };
        let dest_low = dest < issuer;
        let (lo, hi) = if dest_low { (dest, issuer) } else { (issuer, dest) };
        // The auth flag lives on the ISSUER's side of the line.
        let issuer_auth_bit: u64 = if issuer < dest { 0x0004_0000 } else { 0x0008_0000 };

        let build = |issuer_flags: u64, line_flags: u64, balance: &str| -> LedgerState {
            let mut state = make_state_with_account(&dest, 100_000_000);
            let iss = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
                "Balance": "100000000", "Sequence": 1, "OwnerCount": 0,
                "Flags": issuer_flags,
            });
            state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&iss).unwrap()).unwrap();
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": line_flags,
                "Balance": {"currency": hex::encode_upper(cur),
                            "issuer": "0000000000000000000000000000000000000000",
                            "value": if dest_low { balance.to_string() } else { format!("-{balance}") }},
                "LowLimit":  {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(&dest, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();
            state
        };
        let recv = |st: &LedgerState| dest_receivable(&Sandbox::new(st), &dest, &leg);

        // RequireAuth + unauthorized + empty  ⇒ receives NOTHING.
        let st = build(0x0004_0000, 0, "0");
        assert_eq!(recv(&st), Some((0, 0)), "unauthorized empty line under RequireAuth");

        // Authorized ⇒ normal capacity.
        let st = build(0x0004_0000, issuer_auth_bit, "0");
        assert!(
            recv(&st).is_none_or(|r| r.0 > 0),
            "an authorized line receives normally",
        );

        // Unauthorized but NON-EMPTY ⇒ grandfathered, still receives.
        let st = build(0x0004_0000, 0, "5");
        assert!(
            recv(&st).is_none_or(|r| r.0 > 0),
            "an existing balance is grandfathered — rippled tests Balance == 0",
        );

        // Issuer does NOT require auth ⇒ the flag is irrelevant.
        let st = build(0, 0, "0");
        assert!(
            recv(&st).is_none_or(|r| r.0 > 0),
            "no RequireAuth on the issuer means no auth gate",
        );
    }

    /// An arithmetically-inert IOU move writes NOTHING. Values carry ~16
    /// significant digits, so adding a tiny amount to a large balance rounds
    /// back to the stored mantissa — and rippled emits no node for that line.
    ///
    /// #105840045 3F942A682131 pays 0.000001 ZERPS between accounts holding
    /// 137,330,862,022.1269 and 254,893,727,053.43. Neither can represent the
    /// change, so mainnet's meta is ONE node — the sender's AccountRoot for the
    /// fee — and the result is still tesSUCCESS. We wrote both lines and
    /// invented two Modified nodes.
    #[test]
    fn an_inert_iou_move_writes_no_node() {
        let party = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");
        let leg = Leg { xrp: false, cur, issuer };

        let party_low = party < issuer;
        let (lo, hi) = if party_low { (party, issuer) } else { (issuer, party) };
        let big = "137330862022.1269";
        let mut state = make_state_with_account(&party, 100_000_000);
        let iss = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(issuer),
            "Balance": "100000000", "Sequence": 1, "OwnerCount": 0,
        });
        state.state_map.insert(keylet::account_root_key(&issuer), serde_json::to_vec(&iss).unwrap()).unwrap();
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur),
                        "issuer": "0000000000000000000000000000000000000000",
                        "value": if party_low { big.to_string() } else { format!("-{big}") }},
            "LowLimit":  {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000000000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000000000"},
        });
        let lkey = keylet::ripple_state_key(&party, &issuer, &cur);
        state.state_map.insert(lkey, serde_json::to_vec(&line).unwrap()).unwrap();

        // Spend 0.000001 — 18 significant digits away, so it cannot land.
        let mut sandbox = Sandbox::new(&state);
        line_adjust(&mut sandbox, &party, &leg, (1_000_000_000_000_000, -21), false);
        let mods = sandbox.into_modifications();
        assert!(
            !mods.contains_key(&lkey),
            "a balance that cannot change must not be written at all",
        );

        // Control: a move the balance CAN represent still writes.
        let mut sandbox = Sandbox::new(&state);
        line_adjust(&mut sandbox, &party, &leg, (1_000_000_000_000_000, -12), false);
        let mods = sandbox.into_modifications();
        assert!(mods.contains_key(&lkey), "a representable move still writes");
    }
}
