//! rippled `STAmount`'s rounding arithmetic (`src/libxrpl/protocol/
//! STAmount.cpp`: `mulRoundImpl`, `divRoundImpl`, `canonicalizeRound`,
//! `canonicalizeRoundStrict`, `STAmount::canonicalize`), ported as ONE set of
//! functions the path engine calls wherever rippled calls `mulRound`,
//! `mulRoundStrict`, `divRound`, `divRoundStrict` — `Quality::ceilIn/Out`,
//! `limitStepIn/Out`, `mulRatio`, `composedQuality`.
//!
//! The engine in `tx::offer` grew one hand-written primitive per site
//! (`mul_round16_up`, `mul_round_drops_strict`, `div_round16_up`, …), each
//! calibrated by a finding. Here the four rippled functions are written once,
//! from the source, over an `StAmount` that carries what rippled's carries
//! (native or IOU, sign, 64-bit mantissa, exponent), and the track-1 fuzz
//! pins every (function × result asset × rounding) cell against libxrpl.
//!
//! Shape of both (STAmount.cpp 1456-1563 and 1300-1360):
//!
//!   1. integral operands (XRP) get their mantissa scaled up to 16 digits;
//!   2. a 128-bit `muldivRound` at 1e14 (multiply) or 1e17 (divide), with a
//!      rounding term only when `resultNegative != roundUp`;
//!   3. in that same case a canonicalisation of the 17–18 digit mantissa —
//!      the LEGACY one (`canonicalizeRound`: truncate, then +9 ceiling on the
//!      last digit; for XRP the `+9 / +10` loop) for `mulRound` and both
//!      divisions, the STRICT one (`canonicalizeRoundStrict`: the ceiling
//!      honours every dropped digit) for `mulRoundStrict`;
//!   4. the `STAmount` constructor, whose canonicalisation runs `Number`
//!      under a mode: ToNearest for the legacy multiply (DontAffectNumber
//!      RoundMode), TowardsZero for the strict multiply, and for the
//!      divisions Upward/Downward by `roundUp ^ resultNegative` — but only
//!      the STRICT division installs that guard; the legacy one keeps
//!      ToNearest;
//!   5. `roundUp && !resultNegative && result == 0` → the minimum unit.
use crate::tx::number::{Number, Rounding};

/// The mantissa range STAmount keeps for an IOU.
pub const K_MIN_VALUE: u64 = 1_000_000_000_000_000;
pub const K_MAX_VALUE: u64 = 9_999_999_999_999_999;
pub const K_MIN_OFFSET: i32 = -96;
const K_TEN_TO_14: u128 = 100_000_000_000_000;
const K_TEN_TO_17: u128 = 100_000_000_000_000_000;
/// `kMaxNativeN`: 1e17 drops.
const K_MAX_NATIVE: u64 = 100_000_000_000_000_000;

/// An STAmount as rippled's arithmetic sees it. `native` amounts keep
/// whole drops in `mantissa` with `exponent == 0`; an IOU is canonical
/// (mantissa in [1e15, 1e16) or zero).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StAmount {
    pub native: bool,
    pub negative: bool,
    pub mantissa: u64,
    pub exponent: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StError {
    DivideByZero,
    Overflow,
}

impl StAmount {
    pub fn drops(d: u64) -> StAmount {
        StAmount { native: true, negative: false, mantissa: d, exponent: 0 }
    }
    /// `STAmount(issue, mantissa, exponent, negative)` — rippled's constructor
    /// CANONICALISES (`canonicalize()`), so a ten-digit rate mantissa such as
    /// `1001500000e-9` becomes `1001500000000000e-15` before any arithmetic
    /// touches it. The raw form fed `mulRoundImpl` a short mantissa and the
    /// crossing budget of offer_fill_cluster_is_byte_exact lost five digits
    /// (4.0705300485e-6 for rippled's 4.070530048487853e-6).
    pub fn iou(negative: bool, mantissa: u64, exponent: i32) -> StAmount {
        Self::construct(false, negative, mantissa as u128, exponent, Rounding::ToNearest)
            .unwrap_or(StAmount { native: false, negative, mantissa, exponent })
    }
    /// An IOU from the engine's `(u128, i32)` pair, canonicalised at
    /// ToNearest (the constructor's default mode).
    pub fn iou_me(negative: bool, m: (u128, i32)) -> StAmount {
        Self::construct(false, negative, m.0, m.1, Rounding::ToNearest).unwrap_or(StAmount::iou(false, 0, 0))
    }
    pub fn is_zero(&self) -> bool {
        self.mantissa == 0
    }
    pub fn zero(native: bool) -> StAmount {
        StAmount { native, negative: false, mantissa: 0, exponent: 0 }
    }

    /// `STAmount(asset, mantissa, offset, negative)` → `canonicalize()`
    /// under `mode`. Native: `Number(neg, value, offset)` through
    /// `XRPAmount{num}` (`Number::operator rep()`); IOU: `IOUAmount(Number)`.
    pub fn construct(native: bool, negative: bool, mantissa: u128, exponent: i32, mode: Rounding) -> Result<StAmount, StError> {
        if native {
            if mantissa == 0 || exponent <= -20 {
                return Ok(StAmount::zero(true));
            }
            if exponent > 17 {
                return Err(StError::Overflow); // "Native currency amount out of range"
            }
            // Number(isNegative_, value_, offset_, Unchecked): the raw pair is
            // NOT normalised before operator rep() drops its digits.
            let drops = to_drops_unchecked(negative, mantissa, exponent, mode)?;
            if drops.unsigned_abs() > K_MAX_NATIVE as u128 {
                return Err(StError::Overflow);
            }
            return Ok(StAmount { native: true, negative: drops < 0, mantissa: drops.unsigned_abs() as u64, exponent: 0 });
        }
        // `*this = iou()`: IOUAmount(Number{mantissa, offset}) — normalised
        // under `mode`, then IOUAmount's clamp.
        let n = Number::from_parts(negative, mantissa, exponent, mode).map_err(|_| StError::Overflow)?;
        if n.is_zero() || n.exponent < K_MIN_OFFSET {
            return Ok(StAmount::zero(false));
        }
        if n.exponent > 80 {
            return Err(StError::Overflow);
        }
        Ok(StAmount { native: false, negative: n.negative, mantissa: n.mantissa, exponent: n.exponent })
    }
}

/// `XRPAmount{Number{neg, value, offset, Unchecked}}` in
/// `STAmount::canonicalize`: `operator rep()` on the raw pair — see
/// `Number::rep_from_raw`.
fn to_drops_unchecked(negative: bool, mantissa: u128, exponent: i32, mode: Rounding) -> Result<i128, StError> {
    Number::rep_from_raw(negative, mantissa, exponent, mode).map(|d| d as i128).map_err(|_| StError::Overflow)
}

/// `muldivRound(a, b, divisor, rounding)`: `(a·b + rounding) / divisor` in
/// 128 bits; an answer past u64 is an overflow.
fn muldiv_round(a: u64, b: u64, divisor: u128, rounding: u128) -> Result<u64, StError> {
    let r = (a as u128 * b as u128 + rounding) / divisor;
    if r > u64::MAX as u128 {
        return Err(StError::Overflow);
    }
    Ok(r as u64)
}

/// `canonicalizeRound(integral, value, offset, _)` — the LEGACY form: XRP
/// walks the negative offset up with a `+9 / +10 then /10` on the last
/// step; an IOU mantissa over 16 digits is truncated to 17 and ceiled on
/// its last digit.
fn canonicalize_round(integral: bool, value: &mut u64, offset: &mut i32) {
    if integral {
        if *offset < 0 {
            let mut loops = 0;
            while *offset < -1 {
                *value /= 10;
                *offset += 1;
                loops += 1;
            }
            *value += if loops >= 2 { 9 } else { 10 };
            *value /= 10;
            *offset += 1;
        }
    } else if *value > K_MAX_VALUE {
        while *value > 10 * K_MAX_VALUE {
            *value /= 10;
            *offset += 1;
        }
        *value += 9;
        *value /= 10;
        *offset += 1;
    }
}

/// `canonicalizeRoundStrict(integral, value, offset, roundUp)`: like the
/// legacy form, but the XRP ceiling remembers whether ANY dropped digit was
/// non-zero (`+10` only then, and only rounding up).
fn canonicalize_round_strict(integral: bool, value: &mut u64, offset: &mut i32, round_up: bool) {
    if integral {
        if *offset < 0 {
            let mut had_remainder = false;
            while *offset < -1 {
                let nv = *value / 10;
                had_remainder |= *value != nv * 10;
                *value = nv;
                *offset += 1;
            }
            *value += if had_remainder && round_up { 10 } else { 9 };
            *value /= 10;
            *offset += 1;
        }
    } else if *value > K_MAX_VALUE {
        while *value > 10 * K_MAX_VALUE {
            *value /= 10;
            *offset += 1;
        }
        *value += 9;
        *value /= 10;
        *offset += 1;
    }
}

/// Scale an integral operand's mantissa up to 16 digits (`while value <
/// kMinValue`), as both Impls do before the 128-bit step.
fn integral_to_16(v: &StAmount) -> (u64, i32) {
    let (mut m, mut e) = (v.mantissa, v.exponent);
    if v.native {
        while m != 0 && m < K_MIN_VALUE {
            m *= 10;
            e -= 1;
        }
    }
    (m, e)
}

/// The minimum positive unit of the result asset (the `roundUp && !result`
/// clamp): one drop, or `kMinValue × 10^kMinOffset`.
fn minimum_unit(native: bool) -> StAmount {
    if native {
        StAmount::drops(1)
    } else {
        StAmount::iou(false, K_MIN_VALUE, K_MIN_OFFSET)
    }
}

/// `mulRoundImpl<CanonicalizeFunc, MightSaveRound>(v1, v2, asset, roundUp)`.
fn mul_round_impl(v1: &StAmount, v2: &StAmount, result_native: bool, round_up: bool, strict: bool) -> Result<StAmount, StError> {
    if v1.is_zero() || v2.is_zero() {
        return Ok(StAmount::zero(result_native));
    }
    if v1.native && v2.native && result_native {
        let (min_v, max_v) = (v1.mantissa.min(v2.mantissa), v1.mantissa.max(v2.mantissa));
        if min_v > 3_000_000_000 || ((max_v >> 32) * min_v) > 2_095_475_792 {
            return Err(StError::Overflow); // "Native value overflow"
        }
        return Ok(StAmount::drops(min_v * max_v));
    }
    let (value1, offset1) = integral_to_16(v1);
    let (value2, offset2) = integral_to_16(v2);
    let result_negative = v1.negative != v2.negative;
    let rounding = if result_negative != round_up { K_TEN_TO_14 - 1 } else { 0 };
    let mut amount = muldiv_round(value1, value2, K_TEN_TO_14, rounding)?;
    let mut offset = offset1 + offset2 + 14;
    if result_negative != round_up {
        if strict {
            canonicalize_round_strict(result_native, &mut amount, &mut offset, round_up);
        } else {
            canonicalize_round(result_native, &mut amount, &mut offset);
        }
    }
    // MightSaveRound(TowardsZero) for the strict form; DontAffectNumberRoundMode
    // (the thread default, ToNearest) for the legacy one.
    let mode = if strict { Rounding::TowardsZero } else { Rounding::ToNearest };
    let result = StAmount::construct(result_native, result_negative, amount as u128, offset, mode)?;
    if round_up && !result_negative && result.is_zero() {
        return Ok(minimum_unit(result_native));
    }
    Ok(result)
}

/// `divRoundImpl<MightSaveRound>(num, den, asset, roundUp)` — the legacy
/// `canonicalizeRound` in BOTH forms; only the strict one installs the
/// Upward/Downward guard at the constructor.
fn div_round_impl(num: &StAmount, den: &StAmount, result_native: bool, round_up: bool, strict: bool) -> Result<StAmount, StError> {
    if den.is_zero() {
        return Err(StError::DivideByZero);
    }
    if num.is_zero() {
        return Ok(StAmount::zero(result_native));
    }
    let (num_val, num_offset) = integral_to_16(num);
    let (den_val, den_offset) = integral_to_16(den);
    let result_negative = num.negative != den.negative;
    let rounding = if result_negative != round_up { den_val as u128 - 1 } else { 0 };
    let mut amount = muldiv_round(num_val, K_TEN_TO_17 as u64, den_val as u128, rounding)?;
    let mut offset = num_offset - den_offset - 17;
    if result_negative != round_up {
        canonicalize_round(result_native, &mut amount, &mut offset);
    }
    let mode = if strict {
        if round_up ^ result_negative { Rounding::Upward } else { Rounding::Downward }
    } else {
        Rounding::ToNearest
    };
    let result = StAmount::construct(result_native, result_negative, amount as u128, offset, mode)?;
    if round_up && !result_negative && result.is_zero() {
        return Ok(minimum_unit(result_native));
    }
    Ok(result)
}

/// `mulRound(v1, v2, asset, roundUp)`.
pub fn mul_round(v1: &StAmount, v2: &StAmount, result_native: bool, round_up: bool) -> Result<StAmount, StError> {
    mul_round_impl(v1, v2, result_native, round_up, false)
}

/// `mulRoundStrict(v1, v2, asset, roundUp)`.
pub fn mul_round_strict(v1: &StAmount, v2: &StAmount, result_native: bool, round_up: bool) -> Result<StAmount, StError> {
    mul_round_impl(v1, v2, result_native, round_up, true)
}

/// `divRound(num, den, asset, roundUp)`.
pub fn div_round(num: &StAmount, den: &StAmount, result_native: bool, round_up: bool) -> Result<StAmount, StError> {
    div_round_impl(num, den, result_native, round_up, false)
}

/// `divRoundStrict(num, den, asset, roundUp)`.
pub fn div_round_strict(num: &StAmount, den: &StAmount, result_native: bool, round_up: bool) -> Result<StAmount, StError> {
    div_round_impl(num, den, result_native, round_up, true)
}

/// The rate an offer is filed under, as the IOU-shaped STAmount rippled's
/// `Quality::rate()` returns (`amountFromQuality`): mantissa = low 56 bits,
/// exponent = high byte − 100.
pub fn rate_amount(quality: u64) -> StAmount {
    let mantissa = quality & 0x00FF_FFFF_FFFF_FFFF;
    let exponent = ((quality >> 56) as i32) - 100;
    StAmount::iou(false, mantissa, exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrp_products_and_the_minimum_unit() {
        // 2 × 3 drops, both native.
        assert_eq!(mul_round(&StAmount::drops(2), &StAmount::drops(3), true, true).unwrap(), StAmount::drops(6));
        // Finding 281: 1.000000000000027e-2 × 7.384998913361 rounds UP to one drop.
        let a = StAmount::iou(false, 1_000_000_000_000_027, -17);
        let b = StAmount::iou(false, 7_384_998_913_361_000, -15);
        assert_eq!(mul_round(&a, &b, true, true).unwrap(), StAmount::drops(1));
        // Finding 280: strict, rounding down, 9117636388749335e-10 × 1000000000000867e-6 = 911763638875723 drops.
        let a = StAmount::iou(false, 9_117_636_388_749_335, -10);
        let b = StAmount::iou(false, 1_000_000_000_000_867, -6);
        assert_eq!(mul_round_strict(&a, &b, true, false).unwrap(), StAmount::drops(911_763_638_875_723));
    }

    #[test]
    fn iou_division_rounds_where_rippled_rounds() {
        // 1 / 3 rounding up → 3333333333333334e-16.
        let one = StAmount::iou(false, 1_000_000_000_000_000, -15);
        let three = StAmount::iou(false, 3_000_000_000_000_000, -15);
        let r = div_round(&one, &three, false, true).unwrap();
        assert_eq!((r.mantissa, r.exponent), (3_333_333_333_333_334, -16));
        let r = div_round_strict(&one, &three, false, false).unwrap();
        assert_eq!((r.mantissa, r.exponent), (3_333_333_333_333_333, -16));
        assert_eq!(div_round(&one, &StAmount::zero(false), false, true), Err(StError::DivideByZero));
    }
}

#[cfg(test)]
mod gross_budget_tests {
    use super::*;

    /// offer_fill_cluster_is_byte_exact (#106688646): CreateOffer's
    /// `multiplyRound(takerAmount.in, gatewayXferRate, issue, true)` —
    /// 4.064433398390267e-6 ETH × 1.0015 rounds UP to 4.070530048487853e-6;
    /// the port's crossing budget came out 4.0705300485e-6 (eleven digits).
    #[test]
    fn multiply_round_keeps_sixteen_digits_on_the_gross_budget() {
        let a = StAmount::iou(false, 4_064_433_398_390_267, -21);
        let rate = StAmount::iou(false, 1_001_500_000, -9);
        let r = mul_round(&a, &rate, false, true).unwrap();
        assert_eq!((r.mantissa, r.exponent), (4_070_530_048_487_853, -21));
    }
}

#[cfg(test)]
mod f375_tests {
    //! f375 fuzz samples (m3060, libxrpl 3.3.0): short-mantissa operands go
    //! through the STAmount constructor first, as the shim's do.
    use super::*;

    fn st(m: u64, e: i32) -> StAmount {
        StAmount::iou_me(false, (m as u128, e))
    }

    #[test]
    fn short_mantissa_operands_are_canonicalised_first() {
        // (77744143657103e3) divRound (7831006862126611e-8) -> 9927732796800356e-7 down, …357 up
        let r = div_round(&st(77744143657103, 3), &st(7831006862126611, -8), false, false).unwrap();
        assert_eq!((r.mantissa, r.exponent), (9927732796800356, -7));
        let r = div_round(&st(77744143657103, 3), &st(7831006862126611, -8), false, true).unwrap();
        assert_eq!((r.mantissa, r.exponent), (9927732796800357, -7));
        // strict down: …356; strict up: …357
        let r = div_round_strict(&st(77744143657103, 3), &st(7831006862126611, -8), false, false).unwrap();
        assert_eq!((r.mantissa, r.exponent), (9927732796800356, -7));
        // XRP results: 73418486384455 drops / 18852246739166e-5 -> 389442 (legacy both ways), strict down 389441
        let r = div_round(&StAmount::drops(73418486384455), &st(18852246739166, -5), true, false).unwrap();
        assert_eq!(r, StAmount::drops(389442));
        let r = div_round_strict(&StAmount::drops(73418486384455), &st(18852246739166, -5), true, false).unwrap();
        assert_eq!(r, StAmount::drops(389441));
        // 63 drops / 16775381466947e1 -> 0 down, 1 up
        let r = div_round(&StAmount::drops(63), &st(16775381466947, 1), true, false).unwrap();
        assert_eq!(r, StAmount::drops(0));
        let r = div_round(&StAmount::drops(63), &st(16775381466947, 1), true, true).unwrap();
        assert_eq!(r, StAmount::drops(1));
        // 19837722428813037 drops / 99153503678441e-12 -> 200070816389380 legacy, strict down 200070816389379
        let r = div_round(&StAmount::drops(19837722428813037), &st(99153503678441, -12), true, false).unwrap();
        assert_eq!(r, StAmount::drops(200070816389380));
        let r = div_round_strict(&StAmount::drops(19837722428813037), &st(99153503678441, -12), true, false).unwrap();
        assert_eq!(r, StAmount::drops(200070816389379));
    }
}
