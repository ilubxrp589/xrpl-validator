//! rippled's `Number` (libxrpl/basics/Number.cpp, 3.3.0), ported line for
//! line at the 16-digit "Small" mantissa scale.
//!
//! `Number` is the decimal float every IOU balance moves through: STAmount
//! addition and subtraction are `IOUAmount::operator+=`, which is
//! `Number{a} + Number{b}`; AMM maths is Number end to end. Its rounding is
//! not "round the exact result to 16 digits": operands are aligned by
//! dropping digits into a four-bit-per-digit `Guard` (sixteen digits plus a
//! sticky bit), the mantissas are added or subtracted, the guard decides the
//! last-digit adjustment under the thread's rounding mode, and the mantissa
//! is then pulled back into `[1e15, 1e16)` WITHOUT re-rounding — so a
//! subtraction that borrows across the 1e15 cusp loses a digit
//! (`1e15 − ε` rounding down is `9999999999999990e-1`, not `…9999e-1`), and a
//! subtrahend more than sixteen digits below the minuend cannot borrow at
//! all (`1e11 − 2.27e-5` at nearest is `1e11`). Track-1 fuzz, 2026-09-14:
//! 1,277 / 115 / 91 of 20,000 subtractions (down / nearest / up) differed from
//! the engine's exact-then-round model; every other operation agreed.
//!
//! The mantissa scale is amendment-gated in rippled (`Rules.cpp`
//! `setCurrentTransactionRules`): the 19-digit "Large" scale switches on with
//! SingleAssetVault or LendingProtocol, neither enabled on mainnet. This port
//! is the Small scale only — `CuspRoundingFix::Disabled` on every branch, so
//! the Enabled320/330 arms of the original are omitted, not approximated.
//!
//! Overflow (`exponent > 32768`) is an error, as rippled throws; the caller
//! decides what a throw means for its transaction.

/// rippled's `Number::RoundingMode` (a thread-local there; explicit here).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding {
    ToNearest,
    TowardsZero,
    Downward,
    Upward,
}

/// A normalised decimal float: `(−1)^negative × mantissa × 10^exponent`, with
/// the mantissa in `[1e15, 1e16)` or the canonical zero `(false, 0, 0)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Number {
    pub negative: bool,
    pub mantissa: u64,
    pub exponent: i32,
}

/// `Number::normalize 1` / `Number::normalize 2` / `Number::addition overflow`
/// / `Number::multiplication overflow`: the exponent left the representable
/// range upward. Division by zero is the other throw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumberError {
    Overflow,
    DivideByZero,
}

const MIN_MANTISSA: u128 = 1_000_000_000_000_000;
const MAX_MANTISSA: u128 = 9_999_999_999_999_999;
/// `kMaxRep`: the largest magnitude the signed 64-bit external mantissa holds.
const K_MAX_REP: u128 = i64::MAX as u128;
const K_MIN_EXPONENT: i32 = -32768;
const K_MAX_EXPONENT: i32 = 32768;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Round {
    Down,
    Even,
    Up,
}

/// `Number::Guard`: the digits shifted out of a mantissa, most recent in the
/// top nibble, plus a sticky bit (`xbit_`) for anything pushed out of the
/// guard itself and the sign of the value being rounded (`sbit_`).
struct Guard {
    digits: u64,
    xbit: bool,
    sbit: bool,
    mode: Rounding,
}

impl Guard {
    fn new(mode: Rounding) -> Self {
        Guard { digits: 0, xbit: false, sbit: false, mode }
    }

    fn set_negative(&mut self) {
        self.sbit = true;
    }

    /// `doPush`: shift a digit in at the top, the bottom nibble falling into
    /// the sticky bit.
    fn push(&mut self, d: u64) {
        debug_assert!(d < 10, "Number::Guard::doPush : valid digit");
        self.xbit = self.xbit || (self.digits & 0xF) != 0;
        self.digits >>= 4;
        self.digits |= (d & 0xF) << 60;
    }

    /// `pop`: the most recently pushed digit, shifting the rest up.
    fn pop(&mut self) -> u64 {
        let d = (self.digits & 0xF000_0000_0000_0000) >> 60;
        self.digits <<= 4;
        d
    }

    fn empty(&self) -> bool {
        self.digits == 0 && !self.xbit
    }

    /// `doDropDigit`: move the mantissa's ones digit into the guard.
    fn drop_digit(&mut self, mantissa: &mut u128, exponent: &mut i32) {
        self.push((*mantissa % 10) as u64);
        *mantissa /= 10;
        *exponent += 1;
    }

    /// `round()`: what the guard says the last digit should do under `mode`.
    fn round(&self) -> Round {
        match self.mode {
            Rounding::TowardsZero => Round::Down,
            Rounding::Downward if !self.sbit => Round::Down,
            Rounding::Upward if self.sbit => Round::Down,
            Rounding::Downward | Rounding::Upward => {
                if self.empty() {
                    Round::Down
                } else {
                    Round::Up
                }
            }
            Rounding::ToNearest => {
                if self.digits > 0x5000_0000_0000_0000 {
                    Round::Up
                } else if self.digits < 0x5000_0000_0000_0000 {
                    Round::Down
                } else if self.xbit {
                    Round::Up
                } else {
                    Round::Even
                }
            }
        }
    }

    /// `bringIntoRange` (cusp fix disabled): one ×10 if the mantissa fell
    /// below 1e15 — no re-rounding of the digit that appears — and the
    /// canonical zero once the exponent underflows.
    fn bring_into_range(&self, negative: &mut bool, mantissa: &mut u128, exponent: &mut i32) {
        if *mantissa < MIN_MANTISSA {
            *mantissa *= 10;
            *exponent -= 1;
        }
        if *exponent < K_MIN_EXPONENT {
            *negative = false;
            *mantissa = 0;
            *exponent = 0;
        }
    }

    /// `doRoundUp` (cusp fix disabled; `pushOverflow` is a no-op there): bump
    /// the mantissa when the guard says so, sliding a digit off if that
    /// overflows the range, then bring it into range.
    fn round_up(&self, negative: &mut bool, mantissa: &mut u128, exponent: &mut i32) -> Result<(), NumberError> {
        let r = self.round();
        if r == Round::Up || (r == Round::Even && (*mantissa & 1) == 1) {
            *mantissa += 1;
            if *mantissa > MAX_MANTISSA || *mantissa > K_MAX_REP {
                *mantissa /= 10;
                *exponent += 1;
            }
        }
        self.bring_into_range(negative, mantissa, exponent);
        if *exponent > K_MAX_EXPONENT {
            return Err(NumberError::Overflow);
        }
        Ok(())
    }

    /// `doRoundDown` (cusp fix disabled): the subtraction side — a guard that
    /// says "up" means the true magnitude is smaller, so decrement.
    fn round_down(&self, negative: &mut bool, mantissa: &mut u128, exponent: &mut i32) {
        let r = self.round();
        if r == Round::Up || (r == Round::Even && (*mantissa & 1) == 1) {
            *mantissa -= 1;
            if *mantissa < MIN_MANTISSA {
                *mantissa *= 10;
                *exponent -= 1;
            }
        }
        self.bring_into_range(negative, mantissa, exponent);
    }
}

/// `doNormalize` (cusp fix disabled, `dropped = false` on every Small-scale
/// path): scale an arbitrary magnitude into `[1e15, 1e16)`, the digits
/// shifted out deciding the last digit under `mode`.
fn normalize(negative: &mut bool, mantissa: &mut u128, exponent: &mut i32, mode: Rounding) -> Result<(), NumberError> {
    if *mantissa == 0 {
        *negative = false;
        *exponent = 0;
        return Ok(());
    }
    let mut m = *mantissa;
    while m < MIN_MANTISSA && *exponent > K_MIN_EXPONENT {
        m *= 10;
        *exponent -= 1;
    }
    let mut g = Guard::new(mode);
    if *negative {
        g.set_negative();
    }
    while m > MAX_MANTISSA {
        if *exponent >= K_MAX_EXPONENT {
            return Err(NumberError::Overflow);
        }
        g.drop_digit(&mut m, exponent);
    }
    if *exponent < K_MIN_EXPONENT || m < MIN_MANTISSA {
        *negative = false;
        *mantissa = 0;
        *exponent = 0;
        return Ok(());
    }
    // `m > repLimit` cannot hold below MAX_MANTISSA; rippled's `normalize 1.5`
    // guard is for the 19-digit scale.
    *mantissa = m;
    g.round_up(negative, mantissa, exponent)
}

impl Number {
    pub const ZERO: Number = Number { negative: false, mantissa: 0, exponent: 0 };

    /// `Number(rep mantissa, int exponent)`: a signed mantissa and exponent,
    /// normalised under `mode`.
    pub fn new(mantissa: i64, exponent: i32, mode: Rounding) -> Result<Number, NumberError> {
        Self::from_parts(mantissa < 0, mantissa.unsigned_abs() as u128, exponent, mode)
    }

    /// A sign, magnitude and exponent, normalised under `mode`. The engine's
    /// `(u128, i32)` pairs enter here; an over-precise magnitude is rounded
    /// exactly as `Number`'s constructor rounds it.
    pub fn from_parts(negative: bool, mantissa: u128, exponent: i32, mode: Rounding) -> Result<Number, NumberError> {
        let (mut n, mut m, mut e) = (negative, mantissa, exponent);
        normalize(&mut n, &mut m, &mut e, mode)?;
        Ok(Number { negative: n, mantissa: m as u64, exponent: e })
    }

    pub fn is_zero(&self) -> bool {
        self.mantissa == 0
    }

    pub fn negated(self) -> Number {
        if self.is_zero() {
            self
        } else {
            Number { negative: !self.negative, ..self }
        }
    }

    /// `operator+=`.
    pub fn add(self, y: Number, mode: Rounding) -> Result<Number, NumberError> {
        if y.is_zero() {
            return Ok(self);
        }
        if self.is_zero() {
            return Ok(y);
        }
        if self == y.negated() {
            return Ok(Number::ZERO);
        }
        let mut xn = self.negative;
        let mut xm = self.mantissa as u128;
        let mut xe = self.exponent;
        let yn = y.negative;
        let mut ym = y.mantissa as u128;
        let mut ye = y.exponent;
        let mut g = Guard::new(mode);

        // `adjust`: drop the smaller-exponent operand's digits into the guard
        // until the exponents meet (the Enabled330 pre-passes do not apply).
        if xe < ye {
            if xn {
                g.set_negative();
            }
            while xe < ye {
                g.drop_digit(&mut xm, &mut xe);
            }
        } else if xe > ye {
            if yn {
                g.set_negative();
            }
            while ye < xe {
                g.drop_digit(&mut ym, &mut ye);
            }
        }

        if xn == yn {
            xm += ym;
            if xm > MAX_MANTISSA || xm > K_MAX_REP {
                g.drop_digit(&mut xm, &mut xe);
            }
            g.round_up(&mut xn, &mut xm, &mut xe)?;
        } else {
            if xm > ym {
                xm -= ym;
            } else {
                xm = ym - xm;
                xe = ye;
                xn = yn;
            }
            // Pull the difference back up to sixteen digits, borrowing the
            // guard digits back one at a time (wrapping as rippled's unsigned
            // arithmetic would if the difference were zero).
            while xm < MIN_MANTISSA && xm.wrapping_mul(10) <= K_MAX_REP {
                xm = xm.wrapping_mul(10).wrapping_sub(g.pop() as u128);
                xe -= 1;
            }
            g.round_down(&mut xn, &mut xm, &mut xe);
        }
        normalize(&mut xn, &mut xm, &mut xe, mode)?;
        Ok(Number { negative: xn, mantissa: xm as u64, exponent: xe })
    }

    /// `operator-=`: `x + (−y)`.
    pub fn sub(self, y: Number, mode: Rounding) -> Result<Number, NumberError> {
        self.add(y.negated(), mode)
    }

    /// `operator*=`.
    pub fn mul(self, y: Number, mode: Rounding) -> Result<Number, NumberError> {
        if self.is_zero() {
            return Ok(self);
        }
        if y.is_zero() {
            return Ok(y);
        }
        let mut zm = self.mantissa as u128 * y.mantissa as u128;
        let mut ze = self.exponent + y.exponent;
        let mut zn = self.negative != y.negative;
        let mut g = Guard::new(mode);
        if zn {
            g.set_negative();
        }
        while zm > MAX_MANTISSA || zm > K_MAX_REP {
            g.drop_digit(&mut zm, &mut ze);
        }
        g.round_up(&mut zn, &mut zm, &mut ze)?;
        // `normalize(g)`: a fresh doNormalize, the guard's digits not carried.
        normalize(&mut zn, &mut zm, &mut ze, mode)?;
        Ok(Number { negative: zn, mantissa: zm as u64, exponent: ze })
    }

    /// `operator/=` (Small scale: stage 1 only — the quotient of the
    /// numerator scaled by 1e17, its remainder discarded).
    pub fn div(self, y: Number, mode: Rounding) -> Result<Number, NumberError> {
        if y.is_zero() {
            return Err(NumberError::DivideByZero);
        }
        if self.is_zero() {
            return Ok(self);
        }
        const FACTOR: u128 = 100_000_000_000_000_000; // 1e17
        let numerator = self.mantissa as u128 * FACTOR;
        let mut zm = numerator / y.mantissa as u128;
        let mut ze = self.exponent - y.exponent - 17;
        let mut zn = self.negative != y.negative;
        normalize(&mut zn, &mut zm, &mut ze, mode)?;
        Ok(Number { negative: zn, mantissa: zm as u64, exponent: ze })
    }

    /// `shiftExponent`: the same mantissa at another exponent — zero below
    /// the range, an error above it.
    fn shift_exponent(self, delta: i32) -> Result<Number, NumberError> {
        let e = self.exponent + delta;
        if e >= K_MAX_EXPONENT {
            return Err(NumberError::Overflow);
        }
        if e < K_MIN_EXPONENT {
            return Ok(Number::ZERO);
        }
        Ok(Number { exponent: e, ..self })
    }

    /// `root2`: Newton's iteration in Number arithmetic from a quadratic
    /// first guess, run until two successive iterates repeat — every
    /// intermediate rounded as `Number` rounds it, so the last digit is
    /// rippled's, not the true square root's. A negative argument is
    /// rippled's `Number::root nan` throw.
    pub fn root2(self, mode: Rounding) -> Result<Number, NumberError> {
        let one = Number { negative: false, mantissa: MIN_MANTISSA as u64, exponent: -15 };
        if self == one {
            return Ok(self);
        }
        if self.negative && !self.is_zero() {
            return Err(NumberError::Overflow);
        }
        if self.is_zero() {
            return Ok(self);
        }
        let mut e = self.exponent + 15 + 1;
        if e % 2 != 0 {
            e += 1;
        }
        let f = self.shift_exponent(-e)?;
        let n = |v: i64| Number::new(v, 0, mode);
        let mut r = n(-60)?.mul(f, mode)?.add(n(144)?, mode)?.mul(f, mode)?.add(n(18)?, mode)?.div(n(105)?, mode)?;
        let mut rm1 = Number::ZERO;
        let mut rm2;
        loop {
            rm2 = rm1;
            rm1 = r;
            r = r.add(f.div(r, mode)?, mode)?.div(n(2)?, mode)?;
            if r == rm1 || r == rm2 {
                break;
            }
        }
        r.shift_exponent(e / 2)
    }

    /// `operator rep()`: the value as whole drops, the fraction rounded under
    /// `mode` through the guard (`XRPAmount{Number}`).
    pub fn to_drops(self, mode: Rounding) -> Result<i64, NumberError> {
        let mut drops = self.mantissa as u128;
        let mut offset = self.exponent;
        let mut g = Guard::new(mode);
        if drops != 0 {
            if self.negative {
                g.set_negative();
            }
            while offset < 0 {
                g.drop_digit(&mut drops, &mut offset);
            }
            while offset > 0 {
                if drops > K_MAX_REP / 10 {
                    return Err(NumberError::Overflow);
                }
                drops *= 10;
                offset -= 1;
            }
            let r = g.round();
            if r == Round::Up || (r == Round::Even && (drops & 1) == 1) {
                if drops >= K_MAX_REP {
                    return Err(NumberError::Overflow);
                }
                drops += 1;
            }
        }
        let d = drops as i64;
        Ok(if self.negative { -d } else { d })
    }

    /// `XRPAmount{Number{negative, mantissa, exponent, Unchecked}}` — the
    /// STAmount constructor's native canonicalisation: `operator rep()` on
    /// the RAW pair, digits below the ones place dropped one at a time into
    /// the guard (no 16-digit normalisation first), `mode` deciding the drop.
    pub fn rep_from_raw(negative: bool, mantissa: u128, exponent: i32, mode: Rounding) -> Result<i64, NumberError> {
        let mut drops = mantissa;
        let mut offset = exponent;
        let mut g = Guard::new(mode);
        if drops != 0 {
            if negative {
                g.set_negative();
            }
            while offset < 0 {
                g.drop_digit(&mut drops, &mut offset);
            }
            while offset > 0 {
                if drops > K_MAX_REP / 10 {
                    return Err(NumberError::Overflow);
                }
                drops *= 10;
                offset -= 1;
            }
            let r = g.round();
            if r == Round::Up || (r == Round::Even && (drops & 1) == 1) {
                if drops >= K_MAX_REP {
                    return Err(NumberError::Overflow);
                }
                drops += 1;
            }
        }
        let d = drops as i64;
        Ok(if negative { -d } else { d })
    }

    /// The signed external mantissa (`Number::mantissa()`).
    pub fn signed_mantissa(&self) -> i64 {
        let m = self.mantissa as i64;
        if self.negative {
            -m
        } else {
            m
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(m: i64, e: i32) -> Number {
        Number::new(m, e, Rounding::ToNearest).unwrap()
    }

    /// The four classes the 2026-09-14 fuzz printed, libxrpl's answers.
    #[test]
    fn subtraction_matches_libxrpl_examples() {
        // 1e15 − 1.198e-8, rounding down: the borrow across the cusp loses a digit.
        let r = n(1_000_000_000_000_000, 0).sub(n(1_198_386_859_085_957, -23), Rounding::Downward).unwrap();
        assert_eq!((r.negative, r.mantissa, r.exponent), (false, 9_999_999_999_999_990, -1));
        // 1e11 − 2.27e-5 at nearest: sixteen digits below, the subtrahend cannot borrow.
        let r = n(1_000_000_000_000_000, -4).sub(n(2_273_254_535_757_127, -20), Rounding::ToNearest).unwrap();
        assert_eq!((r.negative, r.mantissa, r.exponent), (false, 1_000_000_000_000_000, -4));
        // 1e8·1e15 − 6.4e7 at nearest: …90e7 (not the exact …94e7).
        let r = n(1_000_000_000_000_000, 8).sub(n(6_415_834_439_961_301, -8), Rounding::ToNearest).unwrap();
        assert_eq!((r.negative, r.mantissa, r.exponent), (false, 9_999_999_999_999_990, 7));
        // 0.09999999999999999 − 0.1 = −1e-32 exactly, recovered digit by digit.
        let r = n(9_999_999_999_999_999, -17).sub(n(1_000_000_000_000_000, -16), Rounding::ToNearest).unwrap();
        assert_eq!((r.negative, r.mantissa, r.exponent), (true, 1_000_000_000_000_000, -32));
        // Upward: 1e5·1e15 − 9.999999999999999e-11 stays at 1e20.
        let r = n(1_000_000_000_000_000, 5).sub(n(9_999_999_999_999_999, -11), Rounding::Upward).unwrap();
        assert_eq!((r.negative, r.mantissa, r.exponent), (false, 1_000_000_000_000_000, 5));
    }

    #[test]
    fn addition_and_products() {
        // 2000892.236615386 + 100.153148870651 = 2000992.389764256|651 → …257 (finding 209's pool).
        let r = n(2_000_892_236_615_386, -9).add(n(1_001_531_488_706_510, -13), Rounding::ToNearest).unwrap();
        assert_eq!((r.mantissa, r.exponent), (2_000_992_389_764_257, -9));
        let r = n(3, 0).mul(n(7, 0), Rounding::ToNearest).unwrap();
        assert_eq!((r.mantissa, r.exponent), (2_100_000_000_000_000, -14));
        let r = n(1, 0).div(n(3, 0), Rounding::ToNearest).unwrap();
        assert_eq!((r.mantissa, r.exponent), (3_333_333_333_333_333, -16));
        let r = n(2, 0).div(n(3, 0), Rounding::ToNearest).unwrap();
        assert_eq!((r.mantissa, r.exponent), (6_666_666_666_666_667, -16));
        assert_eq!(n(5, 0).div(Number::ZERO, Rounding::ToNearest), Err(NumberError::DivideByZero));
        assert_eq!(n(5, 0).add(n(-5, 0), Rounding::ToNearest).unwrap(), Number::ZERO);
    }

    #[test]
    fn root_and_drops() {
        let r = n(4, 0).root2(Rounding::ToNearest).unwrap();
        assert_eq!((r.mantissa, r.exponent), (2_000_000_000_000_000, -15));
        let r = n(2, 0).root2(Rounding::ToNearest).unwrap();
        assert_eq!((r.mantissa, r.exponent), (1_414_213_562_373_095, -15));
        assert_eq!(n(15, -1).to_drops(Rounding::ToNearest).unwrap(), 2); // 1.5 → ties to even: 2
        assert_eq!(n(25, -1).to_drops(Rounding::ToNearest).unwrap(), 2); // 2.5 → 2
        assert_eq!(n(-25, -1).to_drops(Rounding::ToNearest).unwrap(), -2);
        assert_eq!(n(25, -1).to_drops(Rounding::Upward).unwrap(), 3);
        assert_eq!(n(123, 2).to_drops(Rounding::ToNearest).unwrap(), 12300);
    }
}
