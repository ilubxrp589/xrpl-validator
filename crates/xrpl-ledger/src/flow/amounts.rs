//! The amount types the path engine moves: rippled's `XRPAmount`,
//! `IOUAmount` and the `EitherAmount` variant the `Step` interface speaks
//! (`include/xrpl/tx/paths/detail/EitherAmount.h`).
//!
//! An `IOUAmount` is a signed 16-digit decimal with STAmount's exponent
//! range, and its arithmetic is `tx::number` at ToNearest, exactly
//! `IOUAmount::operator+=` (IOUAmount.cpp: `Number{*this} + Number{other}`,
//! then `IOUAmount(Number)` rounds to sixteen digits and clamps the exponent
//! to [kMinOffset, kMaxOffset]). At Number's Small scale the operands, the
//! sum and the IOUAmount all have sixteen digits; at Large330 the sum is
//! held at nineteen and rounded once on the way back.
use crate::tx::number::{Number, Rounding};
use crate::tx::offer::Me;

/// STAmount's exponent range for an IOU (`cMinOffset` / `cMaxOffset`).
pub const K_MIN_OFFSET: i32 = -96;
pub const K_MAX_OFFSET: i32 = 80;

/// rippled `IOUAmount`: sign, 16-digit mantissa, exponent. Zero is
/// `(false, (0, 0))`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IouAmount {
    pub negative: bool,
    pub mantissa: u64,
    pub exponent: i32,
}

impl IouAmount {
    pub const ZERO: IouAmount = IouAmount { negative: false, mantissa: 0, exponent: 0 };

    /// `IOUAmount(Number)`: normalise, then `IOUAmount` clamps — below
    /// kMinOffset is zero; above kMaxOffset rippled throws (unreachable from
    /// in-range operands; mapped to zero here and logged nowhere, as the
    /// engine's `stamount_signed_add` already does).
    ///
    /// `IOUAmount::fromNumber` (IOUAmount.cpp:28-37) is `normalizeToRange`
    /// to sixteen digits under the thread's rounding mode — ToNearest
    /// wherever the path engine builds an IOUAmount. At the Small scale the
    /// Number already has sixteen and `iou_parts` hands its fields back; at
    /// Large330 this is the one rounding of a nineteen-digit result.
    pub fn from_number(n: Number) -> IouAmount {
        let Ok((negative, mantissa, exponent)) = n.iou_parts(Rounding::ToNearest) else {
            return IouAmount::ZERO;
        };
        if mantissa == 0 || !(K_MIN_OFFSET..=K_MAX_OFFSET).contains(&exponent) {
            return IouAmount::ZERO;
        }
        IouAmount { negative, mantissa, exponent }
    }

    /// The engine's unsigned `Me` pair with a sign, normalised.
    pub fn from_me(negative: bool, m: Me) -> IouAmount {
        match Number::from_parts(negative, m.0, m.1, Rounding::ToNearest) {
            Ok(n) => IouAmount::from_number(n),
            Err(_) => IouAmount::ZERO,
        }
    }

    /// `Number{IOUAmount}` (IOUAmount.h:120-124) at the thread's scale: the
    /// fields as they stand at Small, padded to nineteen digits at Large330.
    pub fn number(self) -> Number {
        Number::from_iou(self.negative, self.mantissa, self.exponent)
    }

    /// `STAmount::getText()` for an IOU: plain decimal when the exponent
    /// sits in [-25, -5] or is zero, otherwise `<mantissa>e<exponent>`.
    pub fn to_decimal_string(&self) -> String {
        if self.mantissa == 0 {
            return "0".to_string();
        }
        let sign = if self.negative { "-" } else { "" };
        let m = self.mantissa.to_string();
        let e = self.exponent;
        if e != 0 && !(-25..=-5).contains(&e) {
            return format!("{sign}{m}e{e}");
        }
        // Fixed notation: shift the decimal point by `e`.
        let digits = m;
        let mut s = if e >= 0 {
            format!("{digits}{}", "0".repeat(e as usize))
        } else {
            let k = (-e) as usize;
            if digits.len() > k {
                format!("{}.{}", &digits[..digits.len() - k], &digits[digits.len() - k..])
            } else {
                format!("0.{}{}", "0".repeat(k - digits.len()), digits)
            }
        };
        if s.contains('.') {
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        format!("{sign}{s}")
    }
    /// `signum()`: −1, 0, +1.
    pub fn signum(self) -> i8 {
        if self.mantissa == 0 { 0 } else if self.negative { -1 } else { 1 }
    }
    pub fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    pub fn magnitude(self) -> Me {
        (self.mantissa as u128, self.exponent)
    }

    pub fn negated(self) -> IouAmount {
        if self.is_zero() {
            self
        } else {
            IouAmount { negative: !self.negative, ..self }
        }
    }

    /// `IOUAmount::operator+=` — a `Number` addition at ToNearest.
    pub fn add(self, other: IouAmount) -> IouAmount {
        if other.is_zero() {
            return self;
        }
        if self.is_zero() {
            return other;
        }
        match self.number().add(other.number(), Rounding::ToNearest) {
            Ok(n) => IouAmount::from_number(n),
            Err(_) => IouAmount::ZERO,
        }
    }

    pub fn sub(self, other: IouAmount) -> IouAmount {
        self.add(other.negated())
    }
}

impl PartialOrd for IouAmount {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IouAmount {
    /// Signed comparison of two normalised amounts.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Equal,
            (true, false) => return if other.negative { Greater } else { Less },
            (false, true) => return if self.negative { Less } else { Greater },
            _ => {}
        }
        if self.negative != other.negative {
            return if self.negative { Less } else { Greater };
        }
        let mag = crate::tx::offer::me_cmp(self.magnitude(), other.magnitude());
        if self.negative { mag.reverse() } else { mag }
    }
}

/// rippled `XRPAmount`: signed drops (an int64 there; i128 here so the
/// engine's u128 budgets convert without a bounds check at every site).
pub type XrpAmount = i128;

/// `EitherAmount` (EitherAmount.h): the one currency-agnostic amount a Step
/// hands up and down a strand. MPT arrives with the MPTEndpointStep slice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EitherAmount {
    Xrp(XrpAmount),
    Iou(IouAmount),
}

impl EitherAmount {
    pub fn is_zero(&self) -> bool {
        match self {
            EitherAmount::Xrp(d) => *d == 0,
            EitherAmount::Iou(a) => a.is_zero(),
        }
    }
    pub fn xrp(&self) -> XrpAmount {
        match self {
            EitherAmount::Xrp(d) => *d,
            EitherAmount::Iou(_) => panic!("EitherAmount: XRP expected, IOU held"),
        }
    }
    pub fn iou(&self) -> IouAmount {
        match self {
            EitherAmount::Iou(a) => *a,
            EitherAmount::Xrp(_) => panic!("EitherAmount: IOU expected, XRP held"),
        }
    }
    pub fn is_xrp(&self) -> bool {
        matches!(self, EitherAmount::Xrp(_))
    }
    pub fn negative(&self) -> bool {
        match self {
            EitherAmount::Xrp(d) => *d < 0,
            EitherAmount::Iou(a) => a.negative && a.mantissa != 0,
        }
    }
    /// `signum()`: −1, 0, +1.
    pub fn signum(&self) -> i8 {
        if self.is_zero() { 0 } else if self.negative() { -1 } else { 1 }
    }
    /// `operator-` on like amounts (XRP exact; IOU through `Number`).
    pub fn sub(self, other: EitherAmount) -> EitherAmount {
        match (self, other) {
            (EitherAmount::Xrp(a), EitherAmount::Xrp(b)) => EitherAmount::Xrp(a - b),
            (EitherAmount::Iou(a), EitherAmount::Iou(b)) => EitherAmount::Iou(a.sub(b)),
            _ => panic!("EitherAmount: mixed XRP/IOU subtraction"),
        }
    }
    pub fn add(self, other: EitherAmount) -> EitherAmount {
        match (self, other) {
            (EitherAmount::Xrp(a), EitherAmount::Xrp(b)) => EitherAmount::Xrp(a + b),
            (EitherAmount::Iou(a), EitherAmount::Iou(b)) => EitherAmount::Iou(a.add(b)),
            _ => panic!("EitherAmount: mixed XRP/IOU addition"),
        }
    }
    /// rippled 3.4.1 `checkedStepAddOpt` (Steps.h): the add `flow()` and
    /// `BookStep` fold `savedIns` / `savedOuts` with. XRP adds in rippled's
    /// int64 drops (`checkedAdd`, MathUtilities.h): None when the exact sum
    /// is outside that range. The engine holds drops in i128, where such a
    /// sum does not wrap at all, so the bound is checked explicitly: without
    /// it the engine carried on where rippled now stops. An IOU adds through
    /// `Number`, unchanged by 3.4.1 (rippled's throws past the IOU range; see
    /// `IouAmount::from_number` for the engine's side). MPT has no variant
    /// here (MPT strands are not ported), so rippled's MPT arm has nothing to
    /// mirror.
    pub fn checked_step_add(self, other: EitherAmount) -> Option<EitherAmount> {
        match (self, other) {
            (EitherAmount::Xrp(a), EitherAmount::Xrp(b)) => a
                .checked_add(b)
                .filter(|s| (i64::MIN as i128..=i64::MAX as i128).contains(s))
                .map(EitherAmount::Xrp),
            (EitherAmount::Iou(a), EitherAmount::Iou(b)) => Some(EitherAmount::Iou(a.add(b))),
            _ => panic!("EitherAmount: mixed XRP/IOU addition"),
        }
    }
    pub fn cmp_like(&self, other: &EitherAmount) -> core::cmp::Ordering {
        match (self, other) {
            (EitherAmount::Xrp(a), EitherAmount::Xrp(b)) => a.cmp(b),
            (EitherAmount::Iou(a), EitherAmount::Iou(b)) => a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal),
            _ => panic!("EitherAmount: mixed XRP/IOU comparison"),
        }
    }
    pub fn gt(&self, other: &EitherAmount) -> bool {
        self.cmp_like(other).is_gt()
    }
    pub fn lt(&self, other: &EitherAmount) -> bool {
        self.cmp_like(other).is_lt()
    }
    pub fn le(&self, other: &EitherAmount) -> bool {
        self.cmp_like(other).is_le()
    }
    pub fn ge(&self, other: &EitherAmount) -> bool {
        self.cmp_like(other).is_ge()
    }
    /// `minPositiveAmount()`: one drop, or `IOUAmount{1, -81}`.
    pub fn min_positive(xrp: bool) -> EitherAmount {
        if xrp {
            EitherAmount::Xrp(1)
        } else {
            EitherAmount::Iou(IouAmount { negative: false, mantissa: 1_000_000_000_000_000, exponent: K_MIN_OFFSET })
        }
    }
    /// The (mantissa, exponent) pair as the engine's `Me` (magnitude only).
    pub fn mantissa_exp(&self) -> (u128, i32) {
        match self {
            EitherAmount::Xrp(d) => (d.unsigned_abs(), 0),
            EitherAmount::Iou(a) => (a.mantissa as u128, a.exponent),
        }
    }
    pub fn zero(xrp: bool) -> EitherAmount {
        if xrp { EitherAmount::Xrp(0) } else { EitherAmount::Iou(IouAmount::ZERO) }
    }
}

/// `TAmounts<TIn, TOut>`: an offer's or a step's (in, out) pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Amounts {
    pub input: EitherAmount,
    pub output: EitherAmount,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iou(m: i64, e: i32) -> IouAmount {
        IouAmount::from_number(Number::new(m, e, Rounding::ToNearest).unwrap())
    }

    #[test]
    fn add_is_number_add_with_the_iou_clamp() {
        // Finding 47's pool sum: 2000892.236615386 + 100.153148870651 = …257 (half-even at 16 digits).
        let r = iou(2_000_892_236_615_386, -9).add(iou(1_001_531_488_706_510, -13));
        assert_eq!((r.mantissa, r.exponent), (2_000_992_389_764_257, -9));
        // A result under kMinOffset is zero.
        let tiny = iou(1_000_000_000_000_000, -110);
        assert!(tiny.is_zero());
        assert_eq!(iou(5, 0).sub(iou(5, 0)), IouAmount::ZERO);
        assert!(iou(-3, 0) < iou(2, 0));
        assert!(iou(3, 0) > IouAmount::ZERO);
        assert!(iou(-3, 0) < IouAmount::ZERO);
    }

    /// At Number's Large330 scale `IOUAmount::operator+=` holds the result at
    /// nineteen digits and rounds it once to sixteen (IOUAmount.cpp:28-37,
    /// 69-84): the Small scale's cusp quirk — 1e11 − 2.27e-5 stays 1e11 — is
    /// gone, and what comes back is still a 16-digit IOUAmount. Outside the
    /// guard nothing has changed.
    #[test]
    fn iou_arithmetic_follows_the_number_scale() {
        use crate::tx::number::{MantissaScale, ScaleGuard};
        let a = IouAmount { negative: false, mantissa: 1_000_000_000_000_000, exponent: -4 };
        let b = IouAmount { negative: false, mantissa: 2_273_254_535_757_127, exponent: -20 };
        let small = a.sub(b);
        assert_eq!((small.negative, small.mantissa, small.exponent), (false, 1_000_000_000_000_000, -4));
        {
            let _scale = ScaleGuard::new(MantissaScale::Large330);
            // 99999999999.99997726745… to sixteen digits.
            let r = a.sub(b);
            assert_eq!((r.negative, r.mantissa, r.exponent), (false, 9_999_999_999_999_998, -5));
            let r = b.sub(a);
            assert_eq!((r.negative, r.mantissa, r.exponent), (true, 9_999_999_999_999_998, -5));
            // Finding 47's pool sum is 2000992.389764256651 exactly: …257 at either scale.
            let r = iou(2_000_892_236_615_386, -9).add(iou(1_001_531_488_706_510, -13));
            assert_eq!((r.mantissa, r.exponent), (2_000_992_389_764_257, -9));
            assert_eq!(iou(5, 0).sub(iou(5, 0)), IouAmount::ZERO);
            assert!(iou(1_000_000_000_000_000, -110).is_zero());
            // Number{IOUAmount} pads to nineteen digits and comes back unchanged.
            assert_eq!(a.number(), Number::new(1, 11, Rounding::ToNearest).unwrap());
            assert_eq!((a.number().mantissa, a.number().exponent), (1_000_000_000_000_000_000, -7));
            assert_eq!(IouAmount::from_number(a.number()), a);
            assert_eq!(IouAmount::from_number(b.negated().number()), b.negated());
        }
        assert_eq!(a.sub(b), small);
        assert_eq!((a.number().mantissa, a.number().exponent), (1_000_000_000_000_000, -4));
    }

    /// rippled 3.4.1 MathUtilities.h `checkedAdd`'s static_asserts, on the
    /// XRP arm of `checkedStepAddOpt`.
    #[test]
    fn checked_step_add_is_int64_checked_add_on_xrp() {
        let x = |d: i128| EitherAmount::Xrp(d);
        let add = |a: i128, b: i128| x(a).checked_step_add(x(b));
        let (max, min) = (i64::MAX as i128, i64::MIN as i128);
        assert_eq!(add(0, 0), Some(x(0)));
        assert_eq!(add(1, -1), Some(x(0)));
        assert_eq!(add(-5, 2), Some(x(-3)));
        assert_eq!(add(max, 1), None);
        assert_eq!(add(min, -1), None);
        assert_eq!(add(max - 1, 1), Some(x(max)));
        assert_eq!(add(min, max), Some(x(-1)));
        assert_eq!(add(max, min), Some(x(-1)));
        assert_eq!(add(max, max), None);
        assert_eq!(add(min, min), None);
        // The plain add, in i128, carries straight past rippled's range.
        assert_eq!(x(max).add(x(1)), x(max + 1));
        // An IOU still adds through Number.
        let i = |m: i64| EitherAmount::Iou(iou(m, 0));
        assert_eq!(i(2).checked_step_add(i(3)), Some(i(5)));
    }
}

impl core::fmt::Display for EitherAmount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EitherAmount::Xrp(d) => write!(f, "{d}drops"),
            EitherAmount::Iou(a) => write!(f, "{}{}e{}", if a.negative { "-" } else { "" }, a.mantissa, a.exponent),
        }
    }
}
