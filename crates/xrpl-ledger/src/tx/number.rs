//! rippled's `Number` (libxrpl/basics/Number.cpp + Number.h, 3.4.1), ported
//! line for line at two mantissa scales: the 16-digit "Small" scale mainnet
//! runs today and the 19-digit "Large330" scale.
//!
//! `Number` is the decimal float every IOU balance moves through: STAmount
//! addition and subtraction are `IOUAmount::operator+=`, which is
//! `Number{a} + Number{b}`; AMM maths is Number end to end. Its rounding is
//! not "round the exact result to 16 digits": operands are aligned by
//! dropping digits into a four-bit-per-digit `Guard` (sixteen digits plus a
//! sticky bit), the mantissas are added or subtracted, the guard decides the
//! last-digit adjustment under the thread's rounding mode, and — at the Small
//! scale — the mantissa is then pulled back into `[1e15, 1e16)` WITHOUT
//! re-rounding, so a subtraction that borrows across the 1e15 cusp loses a
//! digit (`1e15 − ε` rounding down is `9999999999999990e-1`, not `…9999e-1`),
//! and a subtrahend more than sixteen digits below the minuend cannot borrow
//! at all (`1e11 − 2.27e-5` at nearest is `1e11`). Track-1 fuzz, 2026-09-14:
//! 1,277 / 115 / 91 of 20,000 subtractions (down / nearest / up) differed from
//! the engine's exact-then-round model; every other operation agreed.
//!
//! # The mantissa scale
//!
//! rippled keeps the scale in a thread-local (`Number::kRange`,
//! Number.cpp:32-33) that `setCurrentTransactionRules` sets for every
//! transaction step from the ledger's rules (Rules.cpp:35-74): Small while
//! neither SingleAssetVault nor LendingProtocol is enabled, a Large scale as
//! soon as either is. This port keeps the same shape — [`mantissa_scale`] is a
//! thread-local, [`ScaleGuard`] sets it for a scope and restores the previous
//! value (rippled's `NumberMantissaScaleGuard`, Number.h:962-976) — so every
//! operation below runs at the scale its transaction was given and no call
//! site names one. The thread's default here is **Small** (mainnet's scale);
//! rippled's default outside any guard is Large330.
//!
//! Of rippled's three Large scales only **Large330** is ported: Large320 and
//! LargeLegacy apply when fixCleanup3_3_0 / fixCleanup3_2_0 are off, and both
//! are on at mainnet, so a ledger that enables a vault amendment lands on
//! Large330 (Rules.cpp:58-71). What Large330 changes, beside the range
//! `[1e18, 1e19)` (Number.h:198-245; `CuspRoundingFix::Enabled330`):
//!
//!  * a mantissa above `kMaxRep` (2^63 − 1) is held as a multiple of ten, so
//!    the signed 64-bit external view is `mantissa / 10`, `exponent + 1`
//!    (Number.h:691-726) — see [`Number::external`];
//!  * `Guard::round()` answers `Exact` for an empty guard, `doRoundUp` never
//!    increments past the range (it snaps to `kMaxRepUp` or drops a digit and
//!    rounds again), `doRoundDown` decrements on ANY remainder, and
//!    `pushOverflow` rounds the three values between `kMaxRep` and
//!    `kMaxRepUp` (Number.cpp:445-688);
//!  * addition aligns in three steps and hands `doNormalize` a `dropped`
//!    flag (Number.cpp:963-1123), so the result is the correctly rounded sum;
//!  * division appends a second stage (the remainder × 1e5) and a sticky
//!    third (Number.cpp:1292-1354).
//!
//! The Small arms are the 3.3.0 port unchanged. 3.4.1's only edit to
//! Number.cpp is `Guard::doDropDigitWithTarget` (Number.cpp:426-443), a
//! shortcut for the alignment loop: once the mantissa is zero and the guard
//! holds no digit, further drops push zeros onto zeros, so it jumps to the
//! target exponent. The guard and the result are the same either way.
//!
//! Overflow (`exponent > 32768`) is an error, as rippled throws; the caller
//! decides what a throw means for its transaction.

use std::cell::Cell;
use std::cmp::Ordering;
use std::marker::PhantomData;

/// rippled's `Number::RoundingMode` (a thread-local there; explicit here).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding {
    ToNearest,
    TowardsZero,
    Downward,
    Upward,
}

/// rippled's `MantissaRange::MantissaScale` (Number.h:136-150), the two the
/// engine can meet: `Small` (mantissa in `[1e15, 1e16)`) and `Large330`
/// (`[1e18, 1e19)` with `CuspRoundingFix::Enabled330`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MantissaScale {
    Small,
    Large330,
}

impl MantissaScale {
    /// `setCurrentTransactionRules`' choice (Rules.cpp:43-71): Small unless
    /// SingleAssetVault or LendingProtocol is enabled. With either enabled
    /// rippled picks Large330 when fixCleanup3_3_0 is on, Large320 when only
    /// fixCleanup3_2_0 is, LargeLegacy otherwise; both fixes are on at
    /// mainnet, and Large330 is the only Large scale this port has.
    pub fn for_rules(single_asset_vault: bool, lending_protocol: bool) -> MantissaScale {
        if single_asset_vault || lending_protocol {
            MantissaScale::Large330
        } else {
            MantissaScale::Small
        }
    }

    /// `MantissaRange::min` (Number.h:173): 1e15 or 1e18.
    pub const fn min_mantissa(self) -> u64 {
        match self {
            MantissaScale::Small => 1_000_000_000_000_000,
            MantissaScale::Large330 => 1_000_000_000_000_000_000,
        }
    }

    /// `MantissaRange::max` (Number.h:174): `min * 10 - 1`.
    pub const fn max_mantissa(self) -> u64 {
        self.min_mantissa() * 10 - 1
    }

    /// `MantissaRange::log` (Number.h:198-216): the digits of `min`, less one.
    pub const fn log(self) -> i32 {
        match self {
            MantissaScale::Small => 15,
            MantissaScale::Large330 => 18,
        }
    }
}

thread_local! {
    /// `Number::kRange` (Number.cpp:32-33). Small unless a [`ScaleGuard`] is
    /// alive on this thread.
    static SCALE: Cell<MantissaScale> = const { Cell::new(MantissaScale::Small) };
}

/// `Number::getMantissaScale()`: the scale this thread's arithmetic runs at.
pub fn mantissa_scale() -> MantissaScale {
    SCALE.with(|s| s.get())
}

/// rippled's `NumberMantissaScaleGuard` (Number.h:962-976): sets the thread's
/// scale and restores the previous one when dropped. One is taken at each
/// transaction entry from the ledger's amendments
/// (`ledger::amendments::number_scale_guard`); tests take one to run at
/// Large330.
#[must_use = "the scale is restored when the guard is dropped"]
pub struct ScaleGuard {
    saved: MantissaScale,
    /// The scale is a thread-local: the guard must be dropped where it was made.
    _not_send: PhantomData<*const ()>,
}

impl ScaleGuard {
    pub fn new(scale: MantissaScale) -> ScaleGuard {
        ScaleGuard { saved: SCALE.with(|s| s.replace(scale)), _not_send: PhantomData }
    }
}

impl Drop for ScaleGuard {
    fn drop(&mut self) {
        SCALE.with(|s| s.set(self.saved));
    }
}

/// A normalised decimal float: `(−1)^negative × mantissa × 10^exponent`, with
/// the mantissa in the range of the scale it was made at (`[1e15, 1e16)`
/// Small; `[1e18, 1e19)` Large330, a multiple of ten above 2^63 − 1) or the
/// canonical zero `(false, 0, 0)`. The fields are the INTERNAL
/// representation: read a value out through [`Number::external`],
/// [`Number::iou_parts`] or [`Number::to_drops`], which are right at either
/// scale.
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

/// STAmount / IOUAmount's canonical mantissa range (`STAmount::kMinValue`,
/// `kMaxValue`): sixteen digits whatever scale `Number` runs at.
pub const IOU_MIN_MANTISSA: u64 = 1_000_000_000_000_000;
pub const IOU_MAX_MANTISSA: u64 = 9_999_999_999_999_999;

/// `kMaxRep`: the largest magnitude the signed 64-bit external mantissa holds.
const K_MAX_REP: u128 = i64::MAX as u128;
/// `kMaxRepUp` (Number.h:367): `kMaxRep` rounded up to the next multiple of
/// ten, 9,223,372,036,854,775,810 — the largest Large330 mantissa below the
/// "multiple of ten" region that rounding can land on.
const K_MAX_REP_UP: u128 = (K_MAX_REP / 10 + 1) * 10;
const K_MIN_EXPONENT: i32 = -32768;
const K_MAX_EXPONENT: i32 = 32768;

/// What an operation needs of a `MantissaRange`: the bounds and whether the
/// cusp rounding fix is `Enabled330`. `normalizeToRange` passes its own
/// bounds with the fix disabled (Number.h:866-869).
#[derive(Clone, Copy)]
struct Range {
    min: u128,
    max: u128,
    fix330: bool,
}

impl Range {
    const fn of(scale: MantissaScale) -> Range {
        Range {
            min: scale.min_mantissa() as u128,
            max: scale.max_mantissa() as u128,
            fix330: matches!(scale, MantissaScale::Large330),
        }
    }

    /// `repLimit` (Number.cpp:763-765, 957-958, 1166-1167).
    const fn rep_limit(self) -> u128 {
        if self.fix330 {
            K_MAX_REP_UP
        } else {
            K_MAX_REP
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Round {
    /// The guard is empty: nothing was dropped. Enabled330 only
    /// (Number.cpp:315-329).
    Exact,
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
    range: Range,
}

impl Guard {
    fn new(mode: Rounding, range: Range) -> Self {
        Guard { digits: 0, xbit: false, sbit: false, mode, range }
    }

    fn set_negative(&mut self) {
        self.sbit = true;
    }

    /// `setDropped`: non-zero digits were lost before this guard saw them.
    fn set_dropped(&mut self) {
        self.xbit = true;
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

    /// `unrecoverable` (Number.cpp:393-397): no digit left to pop, though the
    /// sticky bit may be set.
    fn unrecoverable(&self) -> bool {
        self.digits == 0
    }

    fn empty(&self) -> bool {
        self.unrecoverable() && !self.xbit
    }

    /// `doDropDigit`: move the mantissa's ones digit into the guard.
    fn drop_digit(&mut self, mantissa: &mut u128, exponent: &mut i32) {
        self.push((*mantissa % 10) as u64);
        *mantissa /= 10;
        *exponent += 1;
    }

    /// `doDropDigitWithTarget` (Number.cpp:426-443): drop digits until the
    /// exponent reaches `target`. Once the mantissa is zero and the guard has
    /// no digit to lose, every further drop pushes a zero onto zeros — the
    /// guard cannot change — so the exponent jumps straight to the target.
    fn drop_digit_with_target(&mut self, mantissa: &mut u128, exponent: &mut i32, target: i32) {
        while *exponent < target {
            if *mantissa == 0 && self.unrecoverable() {
                *exponent = target;
                return;
            }
            self.drop_digit(mantissa, exponent);
        }
    }

    /// `pushOverflow` (Number.cpp:445-501), Enabled330 only: a mantissa in
    /// `[kMaxRep, kMaxRepUp)` can only be stored as `kMaxRep` or `kMaxRepUp`,
    /// so its position between the two is pushed onto the guard as if it
    /// were a dropped digit (0, 3 or 6 of the spread of 3) — after the digits
    /// already in the guard have had their say on a value below the top.
    /// `mantissa` is rippled's by-value copy: the caller's is not changed.
    fn push_overflow(&mut self, mantissa: u128) {
        if self.range.fix330 && (K_MAX_REP..K_MAX_REP_UP).contains(&mantissa) {
            let mut m = mantissa;
            if m % 10 < 9 {
                let r = self.round();
                if r == Round::Up || (r == Round::Even && m == K_MAX_REP + 1) {
                    m += 1;
                }
            }
            let spread = K_MAX_REP_UP - K_MAX_REP;
            let digit = ((m - K_MAX_REP) * 10) / spread;
            self.push(digit as u64);
        }
    }

    /// `round()`: what the guard says the last digit should do under `mode`.
    fn round(&self) -> Round {
        if self.range.fix330 && self.empty() {
            return Round::Exact;
        }
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

    /// `bringIntoRange` (Number.cpp:551-576): one ×10 if the mantissa fell
    /// below the minimum — no re-rounding of the digit that appears — and
    /// the canonical zero once the exponent underflows. Enabled330 leaves a
    /// zero mantissa alone and makes it the canonical zero.
    fn bring_into_range(&self, negative: &mut bool, mantissa: &mut u128, exponent: &mut i32) {
        if *mantissa < self.range.min && (!self.range.fix330 || *mantissa != 0) {
            *mantissa *= 10;
            *exponent -= 1;
        }
        if *exponent < K_MIN_EXPONENT || (self.range.fix330 && *mantissa == 0) {
            *negative = false;
            *mantissa = 0;
            *exponent = 0;
        }
    }

    /// `doRoundUp` (Number.cpp:578-652): bump the mantissa when the guard
    /// says so, then bring it into range.
    ///
    /// Cusp fix disabled: increment, and slide a digit off (unrounded) if
    /// that left the range. Enabled330: increment only when the result stays
    /// inside both the range and `kMaxRep`; a value between `kMaxRep` and
    /// `kMaxRepUp` snaps to `kMaxRepUp` (up) or `kMaxRep` (down); otherwise
    /// drop a digit and round again — once, the shortened mantissa being far
    /// below both limits.
    fn round_up(&mut self, negative: &mut bool, mantissa: &mut u128, exponent: &mut i32) -> Result<(), NumberError> {
        self.push_overflow(*mantissa);
        let r = self.round();
        if r == Round::Up || (r == Round::Even && (*mantissa & 1) == 1) {
            if self.range.fix330 {
                if *mantissa < self.range.max && *mantissa < K_MAX_REP {
                    *mantissa += 1;
                } else if *mantissa > K_MAX_REP && *mantissa < K_MAX_REP_UP {
                    *mantissa = K_MAX_REP_UP;
                } else {
                    self.drop_digit(mantissa, exponent);
                    return self.round_up(negative, mantissa, exponent);
                }
            } else {
                *mantissa += 1;
                if *mantissa > self.range.max || *mantissa > K_MAX_REP {
                    *mantissa /= 10;
                    *exponent += 1;
                }
            }
        } else if self.range.fix330 && *mantissa > K_MAX_REP && *mantissa < K_MAX_REP_UP {
            *mantissa = K_MAX_REP;
        }
        self.bring_into_range(negative, mantissa, exponent);
        if *exponent > K_MAX_EXPONENT {
            return Err(NumberError::Overflow);
        }
        Ok(())
    }

    /// `doRoundDown` (Number.cpp:654-688), the subtraction side. Cusp fix
    /// disabled: a guard that says "up" means the true magnitude is smaller,
    /// so decrement. Enabled330: ANY remainder decrements (the caller then
    /// tells `doNormalize` digits were dropped, which rounds it back up
    /// where the mode asks).
    fn round_down(&self, negative: &mut bool, mantissa: &mut u128, exponent: &mut i32) {
        let r = self.round();
        if self.range.fix330 {
            if r != Round::Exact {
                *mantissa = mantissa.wrapping_sub(1);
            }
        } else if r == Round::Up || (r == Round::Even && (*mantissa & 1) == 1) {
            *mantissa -= 1;
            if *mantissa < self.range.min {
                *mantissa *= 10;
                *exponent -= 1;
            }
        }
        self.bring_into_range(negative, mantissa, exponent);
    }
}

/// `doNormalize` (Number.cpp:750-833): scale an arbitrary magnitude into
/// `range`, the digits shifted out deciding the last digit under `mode`.
/// `dropped` says non-zero digits were already lost (division's trailing
/// remainder, an Enabled330 addition's leftover guard).
fn normalize(
    negative: &mut bool,
    mantissa: &mut u128,
    exponent: &mut i32,
    range: Range,
    dropped: bool,
    mode: Rounding,
) -> Result<(), NumberError> {
    if *mantissa == 0 {
        *negative = false;
        *exponent = 0;
        return Ok(());
    }
    let mut m = *mantissa;
    while m < range.min && *exponent > K_MIN_EXPONENT {
        m *= 10;
        *exponent -= 1;
    }
    let mut g = Guard::new(mode, range);
    if *negative {
        g.set_negative();
    }
    if dropped {
        g.set_dropped();
    }
    while m > range.max {
        if *exponent >= K_MAX_EXPONENT {
            return Err(NumberError::Overflow);
        }
        g.drop_digit(&mut m, exponent);
    }
    if *exponent < K_MIN_EXPONENT || m < range.min {
        *negative = false;
        *mantissa = 0;
        *exponent = 0;
        return Ok(());
    }
    // `Number::normalize 1.5` (Number.cpp:802-820): at the Large scale the
    // mantissa must fit the signed 64-bit view while it is rounded, so one
    // more digit comes off; `bringIntoRange` puts a zero back in its place.
    // Never true below 1e16 (the Small scale, `normalizeToRange`).
    if m > range.rep_limit() {
        if *exponent >= K_MAX_EXPONENT {
            return Err(NumberError::Overflow);
        }
        g.drop_digit(&mut m, exponent);
    }
    *mantissa = m;
    g.round_up(negative, mantissa, exponent)
}

/// `Number::operator rep()` on a magnitude and exponent (Number.cpp:1365-1392):
/// digits below the ones place dropped into the guard, `doRound` deciding the
/// last unit under `mode`.
fn rep_of(negative: bool, mantissa: u128, exponent: i32, mode: Rounding, range: Range) -> Result<i64, NumberError> {
    let mut drops = mantissa;
    let mut offset = exponent;
    let mut g = Guard::new(mode, range);
    if drops != 0 {
        if negative {
            g.set_negative();
        }
        if offset < 0 {
            g.drop_digit_with_target(&mut drops, &mut offset, 0);
        }
        while offset > 0 {
            if drops > K_MAX_REP / 10 {
                return Err(NumberError::Overflow);
            }
            drops *= 10;
            offset -= 1;
        }
        // `doRound` (Number.cpp:690-716).
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

/// The `adjust` lambda of `operator+=` (Number.cpp:970-1002): bring the
/// operand with the smaller exponent (`shrink`) up to the other's. Cusp fix
/// disabled: drop its digits into the guard. Enabled330: first strip its
/// trailing zeros, then grow the other operand (`expand`) up to
/// `upper_limit`, and only then drop what is left.
fn adjust(g: &mut Guard, upper_limit: u128, expand_m: &mut u128, expand_e: &mut i32, shrink_m: &mut u128, shrink_e: &mut i32) {
    if g.range.fix330 {
        while *shrink_e < *expand_e && *shrink_m % 10 == 0 {
            g.drop_digit(shrink_m, shrink_e);
        }
        while *shrink_e < *expand_e && *expand_e > K_MIN_EXPONENT && *expand_m < upper_limit {
            *expand_m *= 10;
            *expand_e -= 1;
        }
    }
    if *shrink_e < *expand_e {
        g.drop_digit_with_target(shrink_m, shrink_e, *expand_e);
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Number {
    pub const ZERO: Number = Number { negative: false, mantissa: 0, exponent: 0 };

    /// `Number(rep mantissa, int exponent)`: a signed mantissa and exponent,
    /// normalised under `mode` at the thread's scale.
    pub fn new(mantissa: i64, exponent: i32, mode: Rounding) -> Result<Number, NumberError> {
        Self::from_parts(mantissa < 0, mantissa.unsigned_abs() as u128, exponent, mode)
    }

    /// A sign, magnitude and exponent, normalised under `mode` at the
    /// thread's scale (`Number{negative, mantissa, exponent, Normalized{}}`).
    /// The engine's `(u128, i32)` pairs enter here; an over-precise magnitude
    /// is rounded exactly as `Number`'s constructor rounds it.
    pub fn from_parts(negative: bool, mantissa: u128, exponent: i32, mode: Rounding) -> Result<Number, NumberError> {
        let (mut n, mut m, mut e) = (negative, mantissa, exponent);
        normalize(&mut n, &mut m, &mut e, Range::of(mantissa_scale()), false, mode)?;
        Ok(Number { negative: n, mantissa: m as u64, exponent: e })
    }

    /// `Number::one()` (Number.cpp:743-748): `min × 10^-log` at the thread's scale.
    pub fn one() -> Number {
        let scale = mantissa_scale();
        Number { negative: false, mantissa: scale.min_mantissa(), exponent: -scale.log() }
    }

    /// `Number::min()` (Number.h:812-816): the smallest positive value.
    pub fn min_value() -> Number {
        Number { negative: false, mantissa: mantissa_scale().min_mantissa(), exponent: K_MIN_EXPONENT }
    }

    /// `Number::max()` (Number.h:818-822): the mantissa is capped at `kMaxRep`.
    pub fn max_value() -> Number {
        let m = (mantissa_scale().max_mantissa() as u128).min(K_MAX_REP) as u64;
        Number { negative: false, mantissa: m, exponent: K_MAX_EXPONENT }
    }

    /// `Number::lowest()` (Number.h:824-828).
    pub fn lowest() -> Number {
        Number { negative: true, ..Self::max_value() }
    }

    /// `isnormal()` (Number.h:830-838) at the thread's scale.
    pub fn is_normal(&self) -> bool {
        let scale = mantissa_scale();
        let m = self.mantissa as u128;
        *self == Number::ZERO
            || (scale.min_mantissa() <= self.mantissa
                && self.mantissa <= scale.max_mantissa()
                && (m <= K_MAX_REP || m % 10 == 0)
                && (K_MIN_EXPONENT..=K_MAX_EXPONENT).contains(&self.exponent))
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

    /// `abs(Number)` (Number.h:875-881).
    pub fn abs(self) -> Number {
        Number { negative: false, ..self }
    }

    /// `operator+=` (Number.cpp:919-1129).
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
        let range = Range::of(mantissa_scale());
        let fix330 = range.fix330;
        let mut xn = self.negative;
        let mut xm = self.mantissa as u128;
        let mut xe = self.exponent;
        let yn = y.negative;
        let mut ym = y.mantissa as u128;
        let mut ye = y.exponent;
        let mut g = Guard::new(mode, range);
        let upper_limit = range.min * 1000;

        // Bring the exponents into agreement; the guard takes the sign of
        // the operand whose digits it holds.
        if xe < ye {
            if xn {
                g.set_negative();
            }
            adjust(&mut g, upper_limit, &mut ym, &mut ye, &mut xm, &mut xe);
        } else if xe > ye {
            if yn {
                g.set_negative();
            }
            adjust(&mut g, upper_limit, &mut xm, &mut xe, &mut ym, &mut ye);
        } else if fix330 && ((xm < ym && xn) || (ym < xm && yn)) {
            // Same exponent (Number.cpp:1020-1027): the sign of the operand
            // with the smaller mantissa.
            g.set_negative();
        }

        if xn == yn {
            xm += ym;
            // Enabled330 leaves the sum to `doNormalize` (Number.cpp:1033-1045).
            if !fix330 {
                if xm > range.max || xm > range.rep_limit() {
                    g.drop_digit(&mut xm, &mut xe);
                }
                g.round_up(&mut xn, &mut xm, &mut xe)?;
            }
        } else {
            if xm > ym {
                xm -= ym;
            } else {
                xm = ym - xm;
                xe = ye;
                xn = yn;
            }
            if fix330 {
                // Pull guard digits back until the difference carries a few
                // digits more than the range, or the guard is empty
                // (Number.cpp:1069-1086): `doNormalize` then has what it
                // needs to round correctly.
                while xm < upper_limit && !g.empty() {
                    xm = xm.wrapping_mul(10).wrapping_sub(g.pop() as u128);
                    xe -= 1;
                }
            } else {
                // Pull the difference back up to sixteen digits, borrowing the
                // guard digits back one at a time (wrapping as rippled's unsigned
                // arithmetic would if the difference were zero).
                while xm < range.min && xm.wrapping_mul(10) <= range.rep_limit() {
                    xm = xm.wrapping_mul(10).wrapping_sub(g.pop() as u128);
                    xe -= 1;
                }
            }
            g.round_down(&mut xn, &mut xm, &mut xe);
        }
        normalize(&mut xn, &mut xm, &mut xe, range, fix330 && !g.empty(), mode)?;
        Ok(Number { negative: xn, mantissa: xm as u64, exponent: xe })
    }

    /// `operator-=`: `x + (−y)`.
    pub fn sub(self, y: Number, mode: Rounding) -> Result<Number, NumberError> {
        self.add(y.negated(), mode)
    }

    /// `operator*=` (Number.cpp:1131-1183).
    pub fn mul(self, y: Number, mode: Rounding) -> Result<Number, NumberError> {
        if self.is_zero() {
            return Ok(self);
        }
        if y.is_zero() {
            return Ok(y);
        }
        let range = Range::of(mantissa_scale());
        let mut zm = self.mantissa as u128 * y.mantissa as u128;
        let mut ze = self.exponent + y.exponent;
        let mut zn = self.negative != y.negative;
        let mut g = Guard::new(mode, range);
        if zn {
            g.set_negative();
        }
        while zm > range.max || zm > range.rep_limit() {
            g.drop_digit(&mut zm, &mut ze);
        }
        g.round_up(&mut zn, &mut zm, &mut ze)?;
        // `normalize(g)`: a fresh doNormalize, the guard's digits not carried.
        normalize(&mut zn, &mut zm, &mut ze, range, false, mode)?;
        Ok(Number { negative: zn, mantissa: zm as u64, exponent: ze })
    }

    /// `operator/=` (Number.cpp:1185-1363). Small scale: stage 1 only — the
    /// quotient of the numerator scaled by 1e17, its remainder discarded.
    /// Large330: stage 2 appends the remainder's quotient scaled by 1e5 (an
    /// effective factor of 1e22) and stage 3 tells `doNormalize` whether
    /// anything is still left over.
    pub fn div(self, y: Number, mode: Rounding) -> Result<Number, NumberError> {
        if y.is_zero() {
            return Err(NumberError::DivideByZero);
        }
        if self.is_zero() {
            return Ok(self);
        }
        let scale = mantissa_scale();
        const FACTOR: u128 = 100_000_000_000_000_000; // 1e17
        let dm = y.mantissa as u128;
        let numerator = self.mantissa as u128 * FACTOR;
        let mut zm = numerator / dm;
        let mut ze = self.exponent - y.exponent - 17;
        let mut zn = self.negative != y.negative;
        let mut dropped = false;
        if scale != MantissaScale::Small {
            const CORRECTION: u128 = 100_000; // 1e5
            let remainder = numerator % dm;
            if remainder != 0 {
                let partial = remainder * CORRECTION;
                let correction = partial / dm;
                if correction != 0 {
                    zm = zm * CORRECTION + correction;
                    ze -= 5;
                }
                dropped = partial % dm != 0;
            }
        }
        normalize(&mut zn, &mut zm, &mut ze, Range::of(scale), dropped, mode)?;
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

    /// `power(f, n)` (Number.cpp:1513-1525): square-and-multiply in rippled's
    /// order, one rounding per multiplication.
    pub fn power(self, n: u32, mode: Rounding) -> Result<Number, NumberError> {
        if n == 0 {
            return Ok(Number::one());
        }
        if n == 1 {
            return Ok(self);
        }
        let mut r = self.power(n / 2, mode)?;
        r = r.mul(r, mode)?;
        if n % 2 != 0 {
            r = r.mul(self, mode)?;
        }
        Ok(r)
    }

    /// `root(f, d)` (Number.cpp:1536-1608): `f^(1/d)` by Newton's iteration
    /// in Number arithmetic from a quadratic first guess, run until two
    /// successive iterates repeat. `Number::root infinity` and
    /// `Number::root nan` are the `Overflow` error.
    pub fn root(self, d: u32, mode: Rounding) -> Result<Number, NumberError> {
        let one = Number::one();
        let mut f = self;
        if f == one || d == 1 {
            return Ok(f);
        }
        if d == 0 {
            if f == one.negated() {
                return Ok(one);
            }
            if f.abs() < one {
                return Ok(Number::ZERO);
            }
            return Err(NumberError::Overflow);
        }
        if f.negative && !f.is_zero() && d % 2 == 0 {
            return Err(NumberError::Overflow);
        }
        if f.is_zero() {
            return Ok(f);
        }
        // Scale f into (0, 1) with an exponent that is a multiple of d.
        let di = d as i64;
        let mut e = f.exponent as i64 + mantissa_scale().log() as i64 + 1;
        let k = (if e >= 0 { e } else { e - (di - 1) }) / di;
        let k2 = e - k * di;
        e += if k2 == 0 { 0 } else { di - k2 };
        const SHIFT_CAP: i64 = 1 << 30;
        f = f.shift_exponent((-e).clamp(-SHIFT_CAP, SHIFT_CAP) as i32)?;
        let neg = f.negative;
        if neg {
            f = f.negated();
        }
        // Quadratic least-squares fit of f^(1/d) on [0, 1].
        let dw = di as i128;
        let big_d = ((((6 * dw + 11) * dw + 6) * dw) + 1) as i64;
        let a0 = (3 * dw * (((2 * dw - 3) * dw) + 1)) as i64;
        let a1 = (24 * dw * ((2 * dw) - 1)) as i64;
        let a2 = (-30 * (dw - 1) * dw) as i64;
        let n = |v: i64| Number::new(v, 0, mode);
        let mut r = n(a2)?.mul(f, mode)?.add(n(a1)?, mode)?.mul(f, mode)?.add(n(a0)?, mode)?.div(n(big_d)?, mode)?;
        if neg {
            f = f.negated();
            r = r.negated();
        }
        let mut rm1 = Number::ZERO;
        let mut rm2;
        loop {
            rm2 = rm1;
            rm1 = r;
            r = n(di - 1)?.mul(r, mode)?.add(f.div(r.power(d - 1, mode)?, mode)?, mode)?.div(n(di)?, mode)?;
            if r == rm1 || r == rm2 {
                break;
            }
        }
        r.shift_exponent((e / di).clamp(-SHIFT_CAP, SHIFT_CAP) as i32)
    }

    /// `root2`: Newton's iteration in Number arithmetic from a quadratic
    /// first guess, run until two successive iterates repeat — every
    /// intermediate rounded as `Number` rounds it, so the last digit is
    /// rippled's, not the true square root's. A negative argument is
    /// rippled's `Number::root nan` throw.
    pub fn root2(self, mode: Rounding) -> Result<Number, NumberError> {
        let one = Number::one();
        if self == one {
            return Ok(self);
        }
        if self.negative && !self.is_zero() {
            return Err(NumberError::Overflow);
        }
        if self.is_zero() {
            return Ok(self);
        }
        let mut e = self.exponent + mantissa_scale().log() + 1;
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

    /// `power(f, n, d)` (Number.cpp:1657-1684): `f^(n/d)` as
    /// `root(power(f, n), d)` after reducing the fraction. `Number::power
    /// nan` and `Number::power infinity` are the `Overflow` error.
    pub fn power_frac(self, n: u32, d: u32, mode: Rounding) -> Result<Number, NumberError> {
        let one = Number::one();
        if self == one {
            return Ok(self);
        }
        let g = gcd(n, d);
        if g == 0 {
            return Err(NumberError::Overflow);
        }
        if d == 0 {
            if self == one.negated() {
                return Ok(one);
            }
            if self.abs() < one {
                return Ok(Number::ZERO);
            }
            return Err(NumberError::Overflow);
        }
        if n == 0 {
            return Ok(one);
        }
        let (n, d) = (n / g, d / g);
        if n % 2 == 1 && d % 2 == 0 && self.negative && !self.is_zero() {
            return Err(NumberError::Overflow);
        }
        self.power(n, mode)?.root(d, mode)
    }

    /// `truncate()` (Number.cpp:1394-1410): the integer part, towards zero.
    /// The digits are cut from the internal mantissa; the re-normalisation
    /// only pads, so no rounding mode is involved.
    pub fn truncate(self) -> Number {
        if self.exponent >= 0 || self.mantissa == 0 {
            return self;
        }
        let (mut m, mut e) = (self.mantissa, self.exponent);
        while e < 0 && m != 0 {
            e += 1;
            m /= 10;
        }
        Number::from_parts(self.negative, m as u128, e, Rounding::ToNearest).unwrap_or(Number::ZERO)
    }

    /// The external view (`Number::mantissa()` / `exponent()`,
    /// Number.h:691-726): a signed 64-bit mantissa and its exponent. A
    /// Large330 mantissa above 2^63 − 1 is a multiple of ten held one digit
    /// longer than the view allows, so it reads as `mantissa / 10`,
    /// `exponent + 1`. STNumber serialises this pair, and `operator rep()`
    /// starts from it.
    pub fn external(&self) -> (i64, i32) {
        let (mut m, mut e) = (self.mantissa as u128, self.exponent);
        if m > K_MAX_REP {
            m /= 10;
            e += 1;
        }
        let m = m as i64;
        (if self.negative { -m } else { m }, e)
    }

    /// The signed external mantissa (`Number::mantissa()`).
    pub fn signed_mantissa(&self) -> i64 {
        self.external().0
    }

    /// The external exponent (`Number::exponent()`).
    pub fn external_exponent(&self) -> i32 {
        self.external().1
    }

    /// `operator rep()`: the value as whole drops, the fraction rounded under
    /// `mode` through the guard (`XRPAmount{Number}`, `MPTAmount{Number}`).
    /// It reads the external view, so every integer up to 2^63 − 1 comes
    /// back exactly at Large330.
    pub fn to_drops(self, mode: Rounding) -> Result<i64, NumberError> {
        let (m, e) = self.external();
        rep_of(self.negative, m.unsigned_abs() as u128, e, mode, Range::of(mantissa_scale()))
    }

    /// `XRPAmount{Number{negative, mantissa, exponent, Unchecked}}` — the
    /// STAmount constructor's native canonicalisation: `operator rep()` on
    /// the RAW pair, digits below the ones place dropped one at a time into
    /// the guard (no normalisation first), `mode` deciding the drop. The
    /// same at either scale: nothing is normalised, and `doRound` treats an
    /// exact guard and a rounded-down one alike.
    pub fn rep_from_raw(negative: bool, mantissa: u128, exponent: i32, mode: Rounding) -> Result<i64, NumberError> {
        rep_of(negative, mantissa, exponent, mode, Range::of(mantissa_scale()))
    }

    /// `normalizeToRange<min, max>()` (Number.h:840-873): this value's sign,
    /// mantissa and exponent with the mantissa in `[min, max]`, rounded under
    /// `mode` with the cusp fix disabled whatever the thread's scale.
    pub fn normalize_to_range(self, min: u64, max: u64, mode: Rounding) -> Result<(bool, u64, i32), NumberError> {
        let (mut n, mut m, mut e) = (self.negative, self.mantissa as u128, self.exponent);
        normalize(&mut n, &mut m, &mut e, Range { min: min as u128, max: max as u128, fix330: false }, false, mode)?;
        Ok((n, m as u64, e))
    }

    /// `IOUAmount::fromNumber` (IOUAmount.cpp:28-37): the value as an IOU's
    /// sign, 16-digit mantissa and exponent, `normalizeToRange<1e15,
    /// 1e16 − 1>` under `mode`. At the Small scale a Number already is that
    /// triple and comes back untouched; at Large330 this is where nineteen
    /// digits are rounded to sixteen. The caller applies IOUAmount's exponent
    /// clamp.
    pub fn iou_parts(self, mode: Rounding) -> Result<(bool, u64, i32), NumberError> {
        match mantissa_scale() {
            MantissaScale::Small => Ok((self.negative, self.mantissa, self.exponent)),
            MantissaScale::Large330 => self.normalize_to_range(IOU_MIN_MANTISSA, IOU_MAX_MANTISSA, mode),
        }
    }

    /// `Number{IOUAmount}` (IOUAmount.h:120-124): an IOU's sign, mantissa and
    /// exponent as a Number at the thread's scale. A canonical IOU is a Small
    /// Number as it stands; at Large330 its sixteen digits are padded to
    /// nineteen, exactly.
    pub fn from_iou(negative: bool, mantissa: u64, exponent: i32) -> Number {
        match mantissa_scale() {
            MantissaScale::Small => Number { negative, mantissa, exponent },
            MantissaScale::Large330 => {
                Number::from_parts(negative, mantissa as u128, exponent, Rounding::ToNearest).unwrap_or(Number::ZERO)
            }
        }
    }

    /// `Number{IOUAmount{n}}`: the value rounded to an IOU's sixteen digits
    /// under `mode` and taken back as a Number — what a `Number` becomes when
    /// rippled stores it in an STAmount and reads it again. The identity at
    /// the Small scale.
    pub fn iou_rounded(self, mode: Rounding) -> Result<Number, NumberError> {
        match mantissa_scale() {
            MantissaScale::Small => Ok(self),
            MantissaScale::Large330 => {
                let (negative, mantissa, exponent) = self.iou_parts(mode)?;
                Ok(Number::from_iou(negative, mantissa, exponent))
            }
        }
    }
}

/// `operator<` (Number.h:452-487): sign, then exponent, then mantissa. Both
/// operands must be normal at the same scale.
fn less(l: &Number, r: &Number) -> bool {
    if l.negative != r.negative {
        return l.negative;
    }
    if l.mantissa == 0 {
        return r.mantissa > 0;
    }
    if r.mantissa == 0 {
        return false;
    }
    if l.exponent > r.exponent {
        return l.negative;
    }
    if l.exponent < r.exponent {
        return !l.negative;
    }
    if l.negative {
        l.mantissa > r.mantissa
    } else {
        l.mantissa < r.mantissa
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(if self == other {
            Ordering::Equal
        } else if less(self, other) {
            Ordering::Less
        } else {
            Ordering::Greater
        })
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

    // -----------------------------------------------------------------
    // The mantissa scale, and Large330 (19 digits).
    //
    // The Large330 vectors are rippled 3.4.1's own, from
    // src/tests/libxrpl/basics/Number.cpp — cited per test as "T:lines" —
    // read at the Large330 arm of each `scale` switch. Expected values are
    // built the way that suite builds them: through the constructor, under
    // the same rounding mode.
    // -----------------------------------------------------------------

    use Rounding::{Downward, ToNearest, TowardsZero, Upward};

    const MODES: [Rounding; 4] = [ToNearest, TowardsZero, Downward, Upward];
    const K_MAX: i64 = i64::MAX;
    const K_MAX_U: u64 = i64::MAX as u64;
    const K_MAX_UP: u64 = 9_223_372_036_854_775_810;
    const S_MIN: u64 = 1_000_000_000_000_000;
    const L_MIN: u64 = 1_000_000_000_000_000_000;
    const L_MAX: u64 = 9_999_999_999_999_999_999;

    fn large() -> ScaleGuard {
        ScaleGuard::new(MantissaScale::Large330)
    }

    /// `Number{m, e}` under `mode`.
    fn nm(m: i64, e: i32, mode: Rounding) -> Number {
        Number::new(m, e, mode).unwrap()
    }

    /// `Number{negative, m, e, Number::Normalized{}}` under `mode`.
    fn nu(negative: bool, m: u64, e: i32, mode: Rounding) -> Number {
        Number::from_parts(negative, m as u128, e, mode).unwrap()
    }

    fn raw(x: Number) -> (bool, u64, i32) {
        (x.negative, x.mantissa, x.exponent)
    }

    /// The exact integer a Number with a non-negative external exponent holds.
    fn int(x: Number) -> i128 {
        let (m, e) = x.external();
        assert!(e >= 0, "{x:?} is not an integer");
        m as i128 * 10i128.pow(e as u32)
    }

    /// Number.cpp:32-33, Number.h:962-976 (the guard), Rules.cpp:43-71 (the
    /// choice), Number.h:198-226 (the ranges), Number.cpp:743-748 (`one`).
    #[test]
    fn scale_guard_sets_and_restores_the_thread_scale() {
        assert_eq!(mantissa_scale(), MantissaScale::Small);
        assert_eq!(raw(Number::one()), (false, S_MIN, -15));
        {
            let _g = large();
            assert_eq!(mantissa_scale(), MantissaScale::Large330);
            assert_eq!(raw(Number::one()), (false, L_MIN, -18));
            assert_eq!(raw(n(1, 0)), (false, L_MIN, -18));
            assert!(n(1, 0).is_normal());
            {
                let _s = ScaleGuard::new(MantissaScale::Small);
                assert_eq!(raw(n(1, 0)), (false, S_MIN, -15));
            }
            assert_eq!(mantissa_scale(), MantissaScale::Large330);
        }
        assert_eq!(mantissa_scale(), MantissaScale::Small);

        assert_eq!(MantissaScale::for_rules(false, false), MantissaScale::Small);
        assert_eq!(MantissaScale::for_rules(true, false), MantissaScale::Large330);
        assert_eq!(MantissaScale::for_rules(false, true), MantissaScale::Large330);
        assert_eq!(MantissaScale::for_rules(true, true), MantissaScale::Large330);

        assert_eq!((MantissaScale::Small.min_mantissa(), MantissaScale::Small.max_mantissa(), MantissaScale::Small.log()), (S_MIN, 9_999_999_999_999_999, 15));
        assert_eq!((MantissaScale::Large330.min_mantissa(), MantissaScale::Large330.max_mantissa(), MantissaScale::Large330.log()), (L_MIN, L_MAX, 18));
    }

    /// At the Small scale a Number already is an IOU's sign, mantissa and
    /// exponent: the conversions the Large scale needs change nothing.
    #[test]
    fn small_iou_conversions_are_the_identity() {
        let x = n(-1_234_567_890_123_456, -7);
        for mode in MODES {
            assert_eq!(x.iou_parts(mode).unwrap(), raw(x));
            assert_eq!(x.iou_rounded(mode).unwrap(), x);
        }
        assert_eq!(Number::from_iou(true, 1_234_567_890_123_456, -7), x);
        assert_eq!(x.external(), (-1_234_567_890_123_456, -7));
        assert_eq!(raw(Number::min_value()), (false, S_MIN, -32768));
        assert_eq!(raw(Number::max_value()), (false, 9_999_999_999_999_999, 32768));
        assert_eq!(raw(Number::lowest()), (true, 9_999_999_999_999_999, 32768));
    }

    /// T:100-116 (zero), T:118-228 (limits), Number.h:812-828 (min/max).
    #[test]
    fn large330_zero_and_limits() {
        let _g = large();
        for z in [n(0, 0), Number::ZERO] {
            assert_eq!(z.external(), (0, 0));
            assert_eq!(z.add(z, ToNearest).unwrap(), z);
            assert_eq!(z.sub(z, ToNearest).unwrap(), z);
            assert_eq!(z.negated(), z);
        }
        assert_eq!(raw(Number::min_value()), (false, L_MIN, -32768));
        assert_eq!(raw(Number::max_value()), (false, K_MAX_U, 32768));
        assert_eq!(raw(Number::lowest()), (true, K_MAX_U, 32768));

        assert_eq!(Number::from_parts(false, L_MIN as u128 * 10, 32768, ToNearest), Err(NumberError::Overflow));
        assert_eq!(nu(false, L_MIN * 10, 32767, ToNearest), nu(false, L_MIN, 32768, ToNearest));
        assert_eq!(nu(false, L_MIN, -32769, ToNearest), Number::ZERO);
        // 1e18·1000 + 1500 at exponent 32000: …001.5 ties to the even …002.
        assert_eq!(
            nu(false, L_MIN, 32000, ToNearest).mul(n(1000, 0), ToNearest).unwrap().add(nu(false, 1500, 32000, ToNearest), ToNearest).unwrap(),
            nu(false, L_MIN + 2, 32003, ToNearest)
        );
        // int64's ends: −2^63 is not representable and rounds to −(2^63 − 1).
        assert_eq!(raw(n(i64::MIN, 0)), (true, K_MAX_U, 0));
        assert_eq!(n(i64::MIN, 0), nu(true, 9_223_372_036_854_775_808, 0, ToNearest));
        assert_eq!(raw(n(i64::MIN + 1, 0)), (true, K_MAX_U, 0));
        assert_eq!(raw(n(K_MAX, 0)), (false, K_MAX_U, 0));
        assert_eq!(nu(false, L_MIN, 32767, ToNearest).mul(n(100, 0), ToNearest), Err(NumberError::Overflow));
        assert_eq!(Number::new(1, 2_000_000, ToNearest), Err(NumberError::Overflow));
        // kMaxRepUp at the top exponent normalises (Large330 only); its
        // external view is one digit shorter, one exponent higher.
        let big = nu(false, K_MAX_UP, 32768, ToNearest);
        assert_eq!(raw(big), (false, K_MAX_UP, 32768));
        assert_eq!(big.external(), (922_337_203_685_477_581, 32769));
        // Number.cpp:807-814, the worked example of "normalize 1.5".
        let wide = nu(false, 9_900_000_000_000_123_456, 0, ToNearest);
        assert_eq!(raw(wide), (false, 9_900_000_000_000_123_460, 0));
        assert_eq!(wide.external(), (990_000_000_000_012_346, 1));
        assert_eq!(raw(nu(false, 9_900_000_000_000_123_456, 0, Downward)), (false, 9_900_000_000_000_123_450, 0));
    }

    /// T:230-420: cLarge (271-349), cLargeCorrected (359-361), cBigNums
    /// (392-403) and the overflow (406-418).
    #[test]
    fn large330_add() {
        let _g = large();
        let u = |negative: bool, m: u64, e: i32| nu(negative, m, e, ToNearest);
        let cases = [
            (n(1_000_000_000_000_000, -15), n(6_555_555_555_555_555, -29), n(1_000_000_000_000_065_556, -18)),
            (n(-1_000_000_000_000_000, -15), n(-6_555_555_555_555_555, -29), n(-1_000_000_000_000_065_556, -18)),
            (n(-1_000_000_000_000_000, -15), n(6_555_555_555_555_555, -29), u(true, 9_999_999_999_999_344_444, -19)),
            (n(-6_555_555_555_555_555, -29), n(1_000_000_000_000_000, -15), u(false, 9_999_999_999_999_344_444, -19)),
            (Number::ZERO, n(5, 0), n(5, 0)),
            (n(5, 0), Number::ZERO, n(5, 0)),
            (n(5_555_555_555_555_555_000, -32768), n(-5_555_555_555_555_554_000, -32768), n(0, 0)),
            (n(-9_999_999_999_999_999, -31), n(1_000_000_000_000_000, -15), n(9_999_999_999_999_990, -16)),
            // The Small cases widened to nineteen digits.
            (n(1_000_000_000_000_000_000, -18), n(6_555_555_555_555_555_555, -35), n(1_000_000_000_000_000_066, -18)),
            (n(-1_000_000_000_000_000_000, -18), n(-6_555_555_555_555_555_555, -35), n(-1_000_000_000_000_000_066, -18)),
            (n(-1_000_000_000_000_000_000, -18), n(6_555_555_555_555_555_555, -35), u(true, 9_999_999_999_999_999_344, -19)),
            (n(-6_555_555_555_555_555_555, -35), n(1_000_000_000_000_000_000, -18), u(false, 9_999_999_999_999_999_344, -19)),
            (n(5_555_555_555_555_555_555, -32768), n(-5_555_555_555_555_555_554, -32768), n(0, 0)),
            (u(true, 9_999_999_999_999_999_999, -37), n(1_000_000_000_000_000_000, -18), u(false, 9_999_999_999_999_999_990, -19)),
            (n(K_MAX - 1, 0), n(1, 0), n(K_MAX, 0)),
            // Each operand rounds up to 1e19 as it is built.
            (u(false, L_MAX, 0), u(false, L_MAX, 0), n(2, 19)),
            // Mantissas above kMaxRep: the 128-bit sum does not overflow.
            (u(false, 9_999_999_999_999_999_990, 0), u(false, 9_999_999_999_999_999_990, 0), u(false, 1_999_999_999_999_999_998, 1)),
            // cLargeCorrected: kMaxRep + 0.6 stays kMaxRep (below the midpoint to kMaxRepUp).
            (n(K_MAX, 0), n(6, -1), n(K_MAX, 0)),
            // cBigNums: max + 3 at the top exponent is kMaxRepUp.
            (Number::max_value(), n(3, 32768), u(false, K_MAX_UP, 32768)),
        ];
        for (i, (x, y, z)) in cases.into_iter().enumerate() {
            assert_eq!(x.add(y, ToNearest).unwrap(), z, "add case {i}: {x:?} + {y:?}");
        }
        // The first case digit for digit: the sum is 1.000000000000065555|555…
        assert_eq!(
            raw(n(1_000_000_000_000_000, -15).add(n(6_555_555_555_555_555, -29), ToNearest).unwrap()),
            (false, 1_000_000_000_000_065_556, -18)
        );
        let overflow = Number::from_parts(false, L_MAX as u128, 32768, ToNearest)
            .and_then(|a| nu(false, L_MIN, 32767, ToNearest).mul(n(5, 0), ToNearest).and_then(|b| a.add(b, ToNearest)));
        assert_eq!(overflow, Err(NumberError::Overflow));
    }

    /// T:422-572: operands at the two ends of the exponent range, every mode.
    #[test]
    fn large330_add_sub_extreme_exponents() {
        let _g = large();
        for mode in MODES {
            // maxMantissa is not representable at the Large scale: a digit
            // shorter, and with room to round up under Upward.
            let big = L_MAX / 10 - if mode == Upward { 67 } else { 0 };
            for (mantissa, offset) in [(L_MIN, 0), (big, 1)] {
                let x = nu(false, mantissa, 32768, mode);
                let y = nu(false, mantissa, -32768 + offset, mode);
                let m = mantissa as i64;
                assert_eq!(x.external(), (m, 32768), "{mode:?} {mantissa}");
                assert_eq!(y.external(), (m, -32768 + offset), "{mode:?} {mantissa}");
                let (up, down, neg_up) = (nm(m + 1, 32768, mode), nm(m - 1, 32768, mode), nm(-m + 1, 32768, mode));

                let want = if mode == Upward { up } else { x };
                assert_eq!(x.add(y, mode).unwrap(), want, "x + y {mode:?} {mantissa}");
                assert_eq!(y.add(x, mode).unwrap(), want, "y + x {mode:?} {mantissa}");
                // TowardsZero was broken before Large330; here it rounds like Downward.
                let want = if mode == TowardsZero || mode == Downward { down } else { x };
                assert_eq!(x.sub(y, mode).unwrap(), want, "x - y {mode:?} {mantissa}");
                let want = if mode == TowardsZero || mode == Upward { neg_up } else { x.negated() };
                assert_eq!(y.sub(x, mode).unwrap(), want, "y - x {mode:?} {mantissa}");
            }
        }
    }

    /// T:574-728: cLargeAll (604-651) and cLarge330 (675-699).
    #[test]
    fn large330_sub() {
        let _g = large();
        let u = |negative: bool, m: u64, e: i32| nu(negative, m, e, ToNearest);
        let two_63 = n(2, 0).power(63, ToNearest).unwrap();
        let cases = [
            (n(1_000_000_000_000_000, -15), n(6_555_555_555_555_555, -29), u(false, 9_999_999_999_999_344_444, -19)),
            (n(6_555_555_555_555_555, -29), n(1_000_000_000_000_000, -15), u(true, 9_999_999_999_999_344_444, -19)),
            (n(1_000_000_000_000_000, -15), n(1_000_000_000_000_000, -15), n(0, 0)),
            (n(1_000_000_000_000_000, -15), n(1_000_000_000_000_001, -15), n(-1_000_000_000_000_000, -30)),
            (n(1_000_000_000_000_001, -15), n(1_000_000_000_000_000, -15), n(1_000_000_000_000_000, -30)),
            (n(1_000_000_000_000_000_000, -18), n(6_555_555_555_555_555_555, -32), u(false, 9_999_999_999_999_344_444, -19)),
            (n(6_555_555_555_555_555_555, -32), n(1_000_000_000_000_000_000, -18), u(true, 9_999_999_999_999_344_444, -19)),
            (n(1_000_000_000_000_000_000, -18), n(1_000_000_000_000_000_000, -18), n(0, 0)),
            (n(1_000_000_000_000_000_000, -18), n(1_000_000_000_000_000_001, -18), n(-1_000_000_000_000_000_000, -36)),
            (n(1_000_000_000_000_000_001, -18), n(1_000_000_000_000_000_000, -18), n(1_000_000_000_000_000_000, -36)),
            (n(K_MAX, 0), n(6, -1), n(K_MAX - 1, 0)),
            // cLarge330. kMaxRep + 1 is below the midpoint and is built as kMaxRep.
            (u(false, K_MAX_U + 1, 0), n(1, 0), n(K_MAX - 1, 0)),
            (u(false, K_MAX_U + 1, 0), n(3, 0), n(K_MAX - 3, 0)),
            // kMaxRepUp − 1 is above it and is built as kMaxRepUp; less 1 rounds up again.
            (u(false, K_MAX_UP - 1, 0), n(1, 0), n(K_MAX / 10 + 1, 1)),
            (u(false, K_MAX_UP - 1, 0), n(3, 0), n(K_MAX, 0)),
            // 2^63 is kMaxRep + 1.
            (two_63, n(3, 0), n(K_MAX - 3, 0)),
        ];
        for (i, (x, y, z)) in cases.into_iter().enumerate() {
            assert_eq!(x.sub(y, ToNearest).unwrap(), z, "sub case {i}: {x:?} - {y:?}");
        }
    }

    /// `IOUAmount::operator+=` (IOUAmount.cpp:69-84) at Large330: the
    /// difference is held at nineteen digits and rounded ONCE to sixteen, so
    /// the Small scale's two cusp quirks (`subtraction_matches_libxrpl_examples`
    /// above, same operands) are gone. Expected values worked by hand
    /// through Number.cpp:919-1129 and `normalizeToRange` (Number.h:840-873).
    #[test]
    fn large330_iou_subtraction_has_no_cusp_quirk() {
        let _g = large();
        let iou = |m: u64, e: i32| Number::from_iou(false, m, e);
        // 1e15 − 1.198e-8, rounding down: 999999999999999.99999998801… The
        // mantissa is above kMaxRep, so it keeps eighteen digits and a zero.
        let r = iou(1_000_000_000_000_000, 0).sub(iou(1_198_386_859_085_957, -23), Downward).unwrap();
        assert_eq!(raw(r), (false, 9_999_999_999_999_999_990, -4));
        assert_eq!(r.iou_parts(Downward).unwrap(), (false, 9_999_999_999_999_999, -1));
        // 1e11 − 2.27e-5 at nearest: 99999999999.99997726745… (Small: 1e11).
        let r = iou(1_000_000_000_000_000, -4).sub(iou(2_273_254_535_757_127, -20), ToNearest).unwrap();
        assert_eq!(raw(r), (false, 9_999_999_999_999_997_730, -8));
        assert_eq!(r.iou_parts(ToNearest).unwrap(), (false, 9_999_999_999_999_998, -5));
    }

    /// The twelve operand pairs of T:730-1038, built under `mode`.
    fn mul_operands(mode: Rounding) -> [(Number, Number); 12] {
        let m = |v: i64, e: i32| nm(v, e, mode);
        [
            (m(7, 0), m(8, 0)),
            (m(1_414_213_562_373_095, -15), m(1_414_213_562_373_095, -15)),
            (m(-1_414_213_562_373_095, -15), m(1_414_213_562_373_095, -15)),
            (m(-1_414_213_562_373_095, -15), m(-1_414_213_562_373_095, -15)),
            (m(3_214_285_714_285_706, -15), m(3_111_111_111_111_119, -15)),
            (m(1_000_000_000_000_000_000, -32768), m(1_000_000_000_000_000_000, -32768)),
            (m(1_414_213_562_373_095_049, -18), m(1_414_213_562_373_095_049, -18)),
            (m(-1_414_213_562_373_095_048, -18), m(1_414_213_562_373_095_048, -18)),
            (m(-1_414_213_562_373_095_048, -18), m(-1_414_213_562_373_095_049, -18)),
            (m(3_214_285_714_285_714_278, -18), m(3_111_111_111_111_111_119, -18)),
            (nu(false, L_MAX, 0, mode), nu(false, L_MAX, 0, mode)),
            (m(K_MAX, 0), m(K_MAX, 0)),
        ]
    }

    /// T:730-1038: cLarge under ToNearest (782-823), TowardsZero (843-889),
    /// Downward (909-955) and Upward (975-1021), and the overflow (1024-1036).
    #[test]
    fn large330_mul() {
        let _g = large();
        // (negative, mantissa, exponent) of each expected product, to be
        // built under the mode like the operands.
        let (p, q) = (false, true);
        let expected: [(Rounding, [(bool, u64, i32); 12]); 4] = [
            (
                ToNearest,
                [
                    (p, 56, 0),
                    (p, 1_999_999_999_999_999_862, -18),
                    (q, 1_999_999_999_999_999_862, -18),
                    (p, 1_999_999_999_999_999_862, -18),
                    (p, 9_999_999_999_999_999_579, -18),
                    (p, 0, 0),
                    // sqrt(2)^2 != 2 at the higher precision.
                    (p, 2_000_000_000_000_000_001, -18),
                    (q, 1_999_999_999_999_999_998, -18),
                    (p, 1_999_999_999_999_999_999, -18),
                    (p, 10, 0),
                    // The maximum mantissa range rounds up to 1e19 each: 1e38.
                    (p, 1, 38),
                    // kMaxRep^2 = 85'070'591'730'234'615'847'396'907'784'232'501'249.
                    (p, 8_507_059_173_023_461_585, 19),
                ],
            ),
            (
                TowardsZero,
                [
                    (p, 56, 0),
                    (p, 1_999_999_999_999_999_861, -18),
                    (q, 1_999_999_999_999_999_861, -18),
                    (p, 1_999_999_999_999_999_861, -18),
                    (p, 9_999_999_999_999_999_579, -18),
                    (p, 0, 0),
                    (p, 2, 0),
                    (q, 1_999_999_999_999_999_997, -18),
                    (p, 1_999_999_999_999_999_999, -18),
                    (p, 10, 0),
                    (p, L_MAX / 10 - 1, 20),
                    (p, 8_507_059_173_023_461_584, 19),
                ],
            ),
            (
                Downward,
                [
                    (p, 56, 0),
                    (p, 1_999_999_999_999_999_861, -18),
                    (q, 1_999_999_999_999_999_862, -18),
                    (p, 1_999_999_999_999_999_861, -18),
                    (p, 9_999_999_999_999_999_579, -18),
                    (p, 0, 0),
                    (p, 2, 0),
                    (q, 1_999_999_999_999_999_998, -18),
                    (p, 1_999_999_999_999_999_999, -18),
                    (p, 10, 0),
                    (p, L_MAX / 10 - 1, 20),
                    (p, 8_507_059_173_023_461_584, 19),
                ],
            ),
            (
                Upward,
                [
                    (p, 56, 0),
                    (p, 1_999_999_999_999_999_862, -18),
                    (q, 1_999_999_999_999_999_861, -18),
                    (p, 1_999_999_999_999_999_862, -18),
                    (p, 999_999_999_999_999_958, -17),
                    (p, 0, 0),
                    (p, 2_000_000_000_000_000_001, -18),
                    (q, 1_999_999_999_999_999_997, -18),
                    (p, 2, 0),
                    (p, 1_000_000_000_000_000_001, -17),
                    (p, 1, 38),
                    (p, 8_507_059_173_023_461_585, 19),
                ],
            ),
        ];
        for (mode, want) in expected {
            for (i, ((x, y), (negative, m, e))) in mul_operands(mode).into_iter().zip(want).enumerate() {
                assert_eq!(x.mul(y, mode).unwrap(), nu(negative, m, e, mode), "mul {mode:?} case {i}: {x:?} * {y:?}");
            }
        }
        let overflow = Number::from_parts(false, L_MAX as u128, 32768, ToNearest)
            .and_then(|a| Number::from_parts(false, L_MIN as u128 * 5, 32767, ToNearest).and_then(|b| a.mul(b, ToNearest)));
        assert_eq!(overflow, Err(NumberError::Overflow));
    }

    /// T:1040-1228: cLarge under ToNearest (1083-1103), TowardsZero
    /// (1120-1140), Downward (1157-1177) and Upward (1194-1214).
    #[test]
    fn large330_div() {
        let _g = large();
        // 2/3, −2/3 and 1/7 are the quotients the mode decides.
        let thirds_and_sevenths: [(Rounding, [(bool, u64, i32); 3]); 4] = [
            (ToNearest, [(false, 6_666_666_666_666_666_667, -19), (true, 6_666_666_666_666_666_667, -19), (false, 1_428_571_428_571_428_571, -19)]),
            (TowardsZero, [(false, 6_666_666_666_666_666_666, -19), (true, 6_666_666_666_666_666_666, -19), (false, 1_428_571_428_571_428_571, -19)]),
            (Downward, [(false, 6_666_666_666_666_666_666, -19), (true, 6_666_666_666_666_666_667, -19), (false, 1_428_571_428_571_428_571, -19)]),
            (Upward, [(false, 6_666_666_666_666_666_667, -19), (true, 6_666_666_666_666_666_666, -19), (false, 1_428_571_428_571_428_572, -19)]),
        ];
        for (mode, varying) in thirds_and_sevenths {
            let m = |v: i64, e: i32| nm(v, e, mode);
            let u = |negative: bool, v: u64, e: i32| nu(negative, v, e, mode);
            let cases = [
                (m(1, 0), m(2, 0), m(5, -1)),
                (m(1, 0), m(10, 0), m(1, -1)),
                (m(1, 0), m(-10, 0), m(-1, -1)),
                (m(0, 0), m(100, 0), m(0, 0)),
                (m(1_414_213_562_373_095, -10), m(1_414_213_562_373_095, -10), m(1, 0)),
                (m(9_999_999_999_999_999, 0), m(1_000_000_000_000_000, 0), m(9_999_999_999_999_999, -15)),
                (m(2, 0), m(3, 0), u(varying[0].0, varying[0].1, varying[0].2)),
                (m(-2, 0), m(3, 0), u(varying[1].0, varying[1].1, varying[1].2)),
                (m(1, 0), m(7, 0), u(varying[2].0, varying[2].1, varying[2].2)),
                (m(1_414_213_562_373_095_049, -13), m(1_414_213_562_373_095_049, -13), m(1, 0)),
                (u(false, L_MAX, 0), m(1_000_000_000_000_000_000, 0), u(false, L_MAX, -18)),
            ];
            for (i, (x, y, z)) in cases.into_iter().enumerate() {
                assert_eq!(x.div(y, mode).unwrap(), z, "div {mode:?} case {i}: {x:?} / {y:?}");
            }
        }
        assert_eq!(n(1_000_000_000_000_000, -15).div(n(0, 0), ToNearest), Err(NumberError::DivideByZero));
    }

    /// T:2250-2428: stages 2 and 3 of the division (Number.cpp:1292-1354).
    /// With the trailing remainder flagged, Upward never returns less than
    /// the exact quotient, Downward never more, and ToNearest sees the
    /// digits past an apparent tie. Compared exactly in integers: the stored
    /// quotient is `m × 10^e` with `e < 0`, so `stored − a/b` has the sign of
    /// `m·b − a·10^(−e)`, and one unit in the last place is `b` on that scale.
    #[test]
    fn large330_division_keeps_the_trailing_remainder() {
        let _g = large();
        let error = |a: i64, b: i64, mode: Rounding| -> i128 {
            let (m, e) = nm(a, 0, mode).div(nm(b, 0, mode), mode).unwrap().external();
            assert!(e < 0);
            m as i128 * b as i128 - a as i128 * 10i128.pow((-e) as u32)
        };
        // 10^18 + 7 is prime.
        let b: i64 = 1_000_000_000_000_000_007;
        let d = error(2, b, Upward);
        assert!(d >= 0 && d < b as i128, "Upward 2/b: {d}");
        let d = error(-2, b, Downward);
        assert!(d <= 0 && d > -(b as i128), "Downward -2/b: {d}");
        // The dropped digits are "5000000000000000001…": not a tie.
        let (a, b): (i64, i64) = (1_269_917_268_816_087_809, 3_458_525_013_821_685_511);
        let d = error(a, b, ToNearest);
        assert!(d >= 0 && d < b as i128, "ToNearest a/b: {d}");
    }

    /// T:1230-1297: cSmall (1246-1255) is run at every scale — at Large its
    /// nineteen-digit expectations are exact — and cLarge (1256-1269) under
    /// TowardsZero.
    #[test]
    fn large330_root() {
        let _g = large();
        let cases = [
            (n(2, 0), 2, n(1_414_213_562_373_095_049, -18)),
            (n(2_000_000, 0), 2, n(1_414_213_562_373_095_049, -15)),
            (n(2, -30), 2, n(1_414_213_562_373_095_049, -33)),
            (n(-27, 0), 3, n(-3, 0)),
            (n(1, 0), 5, n(1, 0)),
            (n(-1, 0), 0, n(1, 0)),
            (n(5, -1), 0, n(0, 0)),
            (n(0, 0), 5, n(0, 0)),
            (n(5625, -4), 2, n(75, -2)),
        ];
        for (i, (x, d, z)) in cases.into_iter().enumerate() {
            assert_eq!(x.root(d, ToNearest).unwrap(), z, "root case {i}: root({x:?}, {d})");
        }
        assert_eq!(raw(n(2, 0).root(2, ToNearest).unwrap()), (false, 1_414_213_562_373_095_049, -18));

        let u = |m: u64, e: i32| nu(false, m, e, TowardsZero);
        let cases = [
            (u(L_MAX - 9, -1), 2, u(999_999_999_999_999_999, -9)),
            (u(L_MAX - 9, 0), 2, u(3_162_277_660_168_379_330, -9)),
            (nm(K_MAX, 0, TowardsZero), 2, u(3_037_000_499_976_049_692, -9)),
            (nm(K_MAX, 0, TowardsZero), 4, u(5_510_898_747_006_743_627, -14)),
        ];
        for (i, (x, d, z)) in cases.into_iter().enumerate() {
            assert_eq!(x.root(d, TowardsZero).unwrap(), z, "root TowardsZero case {i}: root({x:?}, {d})");
        }
        assert_eq!(n(-2, 0).root(0, ToNearest), Err(NumberError::Overflow)); // "Number::root infinity"
        assert_eq!(n(-2, 0).root(4, ToNearest), Err(NumberError::Overflow)); // "Number::root nan"
    }

    /// T:1299-1339: `root2(x) == root(x, 2)`.
    #[test]
    fn large330_root2() {
        let _g = large();
        for x in [n(2, 0), n(2_000_000, 0), n(2, -30), n(27, 0), n(1, 0), n(5, -1), n(0, 0), n(5625, -4), n(K_MAX, 0)] {
            assert_eq!(x.root2(ToNearest).unwrap(), x.root(2, ToNearest).unwrap(), "root2({x:?})");
        }
        assert_eq!(raw(n(2, 0).root2(ToNearest).unwrap()), (false, 1_414_213_562_373_095_049, -18));
        assert_eq!(raw(n(4, 0).root2(ToNearest).unwrap()), (false, 2_000_000_000_000_000_000, -18));
        assert_eq!(n(-2, 0).root2(ToNearest), Err(NumberError::Overflow)); // "Number::root nan"
    }

    /// T:1341-1360 (power1) and T:1362-1409 (power2).
    #[test]
    fn large330_power() {
        let _g = large();
        let cases = [
            (n(64, 0), 0, n(1, 0)),
            (n(64, 0), 1, n(64, 0)),
            (n(64, 0), 2, n(4096, 0)),
            (n(-64, 0), 2, n(4096, 0)),
            (n(64, 0), 3, n(262_144, 0)),
            (n(-64, 0), 3, n(-262_144, 0)),
            (n(64, 0), 11, nu(false, 7_378_697_629_483_820_646, 1, ToNearest)),
            (n(-64, 0), 11, nu(true, 7_378_697_629_483_820_646, 1, ToNearest)),
        ];
        for (i, (x, p, z)) in cases.into_iter().enumerate() {
            assert_eq!(x.power(p, ToNearest).unwrap(), z, "power case {i}: power({x:?}, {p})");
        }
        let cases = [
            (n(1, 0), 3, 7, n(1, 0)),
            (n(-1, 0), 1, 0, n(1, 0)),
            (n(-1, -1), 1, 0, n(0, 0)),
            (n(16, 0), 0, 5, n(1, 0)),
            (n(34, 0), 3, 3, n(34, 0)),
            (n(4, 0), 3, 2, n(8, 0)),
        ];
        for (i, (x, p, d, z)) in cases.into_iter().enumerate() {
            assert_eq!(x.power_frac(p, d, ToNearest).unwrap(), z, "power case {i}: power({x:?}, {p}, {d})");
        }
        assert_eq!(n(7, 0).power_frac(0, 0, ToNearest), Err(NumberError::Overflow)); // "Number::power nan"
        assert_eq!(n(7, 0).power_frac(1, 0, ToNearest), Err(NumberError::Overflow)); // "Number::power infinity"
        assert_eq!(n(-1, -1).power_frac(3, 2, ToNearest), Err(NumberError::Overflow)); // "Number::power nan"
    }

    /// T:1439-1608 (to_integer) and T:1994-2102 (rounding): `operator rep()`
    /// under each mode. Same answers at every scale; here at Large330.
    #[test]
    fn large330_to_integer() {
        let _g = large();
        let exact: [(i64, i32, i64); 14] = [
            (0, 0, 0),
            (1, 0, 1),
            (2, 0, 2),
            (3, 0, 3),
            (-1, 0, -1),
            (-2, 0, -2),
            (-3, 0, -3),
            (10, 0, 10),
            (99, 0, 99),
            (1155, 0, 1155),
            (9_999_999_999_999_999, 0, 9_999_999_999_999_999),
            (9_999_999_999_999_999, 1, 99_999_999_999_999_990),
            (9_999_999_999_999_999, 2, 999_999_999_999_999_900),
            (-9_999_999_999_999_999, 2, -999_999_999_999_999_900),
        ];
        // 1.5 1.4 1.6 2.5 0.6 0.5 0.4, then the same negated (T:1463-1476 and its three siblings).
        let tenths = [15, 14, 16, 25, 6, 5, 4];
        let by_mode: [(Rounding, [i64; 7], [i64; 7]); 4] = [
            (ToNearest, [2, 1, 2, 2, 1, 0, 0], [-2, -1, -2, -2, -1, 0, 0]),
            (TowardsZero, [1, 1, 1, 2, 0, 0, 0], [-1, -1, -1, -2, 0, 0, 0]),
            (Downward, [1, 1, 1, 2, 0, 0, 0], [-2, -2, -2, -3, -1, -1, -1]),
            (Upward, [2, 2, 2, 3, 1, 1, 1], [-1, -1, -1, -2, 0, 0, 0]),
        ];
        for (mode, positive, negative) in by_mode {
            for (m, e, want) in exact {
                assert_eq!(nm(m, e, mode).to_drops(mode), Ok(want), "{m}e{e} {mode:?}");
            }
            for (i, t) in tenths.into_iter().enumerate() {
                assert_eq!(nm(t, -1, mode).to_drops(mode), Ok(positive[i]), "{t}e-1 {mode:?}");
                assert_eq!(nm(-t, -1, mode).to_drops(mode), Ok(negative[i]), "-{t}e-1 {mode:?}");
            }
            // T:1597-1606: 9223372036854776000 is past int64.
            assert_eq!(nm(9_223_372_036_854_776, 3, mode).to_drops(mode), Err(NumberError::Overflow), "{mode:?}");
            // Number.h:111-116: what the Large scale is for — every int64
            // magnitude survives the round trip.
            assert_eq!(nm(K_MAX, 0, mode).to_drops(mode), Ok(K_MAX), "{mode:?}");
            assert_eq!(nm(-K_MAX, 0, mode).to_drops(mode), Ok(-K_MAX), "{mode:?}");
            assert_eq!(nm(100_000_000_000_000_001, 0, mode).to_drops(mode), Ok(100_000_000_000_000_001), "{mode:?}");
        }
        // T:2004-2088, [ToNearest, TowardsZero, Downward, Upward].
        let rounding: [(i64, i32, [i64; 4]); 16] = [
            (13, -1, [1, 1, 1, 2]),
            (23, -1, [2, 2, 2, 3]),
            (15, -1, [2, 1, 1, 2]),
            (25, -1, [2, 2, 2, 3]),
            (152, -2, [2, 1, 1, 2]),
            (252, -2, [3, 2, 2, 3]),
            (17, -1, [2, 1, 1, 2]),
            (27, -1, [3, 2, 2, 3]),
            (-13, -1, [-1, -1, -2, -1]),
            (-23, -1, [-2, -2, -3, -2]),
            (-15, -1, [-2, -1, -2, -1]),
            (-25, -1, [-2, -2, -3, -2]),
            (-152, -2, [-2, -1, -2, -1]),
            (-252, -2, [-3, -2, -3, -2]),
            (-17, -1, [-2, -1, -2, -1]),
            (-27, -1, [-3, -2, -3, -2]),
        ];
        for (m, e, want) in rounding {
            for (i, mode) in MODES.into_iter().enumerate() {
                assert_eq!(n(m, e).to_drops(mode), Ok(want[i]), "{m}e{e} {mode:?}");
            }
        }
    }

    /// T:1966-1992.
    #[test]
    fn large330_truncate() {
        let _g = large();
        let cases = [
            (n(25, 1), n(250, 0)),
            (n(25, 0), n(25, 0)),
            (n(25, -1), n(2, 0)),
            (n(25, -2), n(0, 0)),
            (n(99, -2), n(0, 0)),
            (n(-25, 1), n(-250, 0)),
            (n(-25, 0), n(-25, 0)),
            (n(-25, -1), n(-2, 0)),
            (n(-25, -2), n(0, 0)),
            (n(-99, -2), n(0, 0)),
            (n(0, 0), n(0, 0)),
            (n(0, 30000), n(0, 0)),
            (n(0, -30000), n(0, 0)),
            (n(100, -30000), n(0, 0)),
            (n(-100, -30000), n(0, 0)),
        ];
        for (i, (x, z)) in cases.into_iter().enumerate() {
            assert_eq!(x.truncate(), z, "truncate case {i}: {x:?}");
        }
        // A mantissa above kMaxRep truncates through its internal digits.
        assert_eq!(nu(false, K_MAX_UP, -1, ToNearest).truncate(), n(922_337_203_685_477_581, 0));
    }

    /// T:2104-2158, the Large arm (2134-2156).
    #[test]
    fn large330_int64() {
        let _g = large();
        // kInitialXrp (1e17 drops) fits the mantissa: exponent ≤ 0.
        assert!(n(100_000_000_000_000_000, 0).external_exponent() <= 0);
        assert!(n(10, 0).external_exponent() <= 0);
        let max_int = n(K_MAX, 0);
        assert!(max_int.external_exponent() <= 0);
        // 85'070'591'730'234'615'847'396'907'784'232'501'249 — 38 digits.
        assert_eq!(max_int.power(2, ToNearest).unwrap(), n(8_507_059_173_023_461_585, 19));

        let max = nu(false, L_MAX, 0, TowardsZero);
        assert_eq!(max.external(), ((L_MAX / 10) as i64, 1));
        // 99'999'999'999'999'999'800'000'000'000'000'000'100.
        assert_eq!(max.power(2, TowardsZero).unwrap(), nu(false, L_MAX / 10 - 1, 20, TowardsZero));
    }

    /// T:2160-2226: a product that lands on kMaxRep and must round up goes
    /// to kMaxRepUp (LargeLegacy rounded it DOWN).
    #[test]
    fn large330_upward_product_at_the_k_max_rep_cusp() {
        let _g = large();
        let (a, b): (i64, i64) = (1_000_000_000_000_049_863, 9_223_372_036_854_315_903);
        let product = nm(a, 0, Upward).mul(nm(b, 0, Upward), Upward).unwrap();
        assert_eq!(product.external(), (K_MAX / 10 + 1, 19));
        assert_eq!(raw(product), (false, K_MAX_UP, 18));
    }

    /// T:2430-2556: a large minus a tiny. Upward and ToNearest land one above
    /// the exact difference (the nearest representable); Downward and
    /// TowardsZero one unit in the last place below that.
    #[test]
    fn large330_subtraction_rounding() {
        let _g = large();
        for (offset, extra_b) in [(2u32, true), (2, false)] {
            let a = n(1, 18 + offset as i32);
            let b = (if extra_b { n(1, 18) } else { Number::ZERO }).add(n(1, 0), ToNearest).unwrap().negated();
            assert_eq!(int(a), 10i128.pow(18 + offset));
            assert_eq!(int(b), if extra_b { -1_000_000_000_000_000_001 } else { -1 });
            let exact = int(a) + int(b);
            let epsilon = 10i128.pow(offset);
            for mode in MODES {
                let sum = a.add(b, mode).unwrap();
                assert!(sum.external_exponent() <= offset as i32, "{mode:?} {sum:?}");
                let diff = int(sum) - exact;
                match mode {
                    Upward | ToNearest => assert_eq!(diff, 1, "{mode:?} offset {offset} extra {extra_b}"),
                    _ => assert_eq!(diff, -(epsilon - 1), "{mode:?} offset {offset} extra {extra_b}"),
                }
            }
        }
        // Offset 30: 1e48 − 1. One above the exact value is 1e48 itself;
        // 1e30 − 1 below it is 999999999999999999e30.
        let (a, b) = (n(1, 48), n(-1, 0));
        for mode in MODES {
            let sum = a.add(b, mode).unwrap();
            assert!(sum.external_exponent() <= 30, "{mode:?} {sum:?}");
            match mode {
                Upward | ToNearest => assert_eq!(sum, a, "{mode:?}"),
                _ => assert_eq!(sum, n(999_999_999_999_999_999, 30), "{mode:?}"),
            }
        }
    }

    /// T:2558-2643: kMaxRep + 1 is one from kMaxRep and two from kMaxRepUp.
    #[test]
    fn large330_normalization_at_the_k_max_rep_cusp() {
        let _g = large();
        let below = n(K_MAX, 0);
        let above = nu(false, K_MAX_U + 3, 0, ToNearest);
        assert_eq!(raw(above), (false, K_MAX_UP, 0));
        let construct = |mode: Rounding| nu(false, K_MAX_U + 1, 0, mode);
        assert_eq!(construct(Upward), above);
        assert_eq!(construct(ToNearest), below);
        assert_eq!(construct(Downward), below);
        assert_eq!(construct(TowardsZero), below);
    }

    /// T:2645-2813: two operands with the same exponent whose sum needs a
    /// twentieth digit. Downward must not land above the exact sum, Upward
    /// not below it — whatever the signs (before Large330 a negative sum
    /// rounded the wrong way).
    #[test]
    fn large330_add_directed_rounding_follows_the_sign() {
        let _g = large();
        let pairs = [
            // Two negatives: −6e18 + (−6e18 − 3).
            (n(-6, 18), n(-6, 18).sub(n(3, 0), ToNearest).unwrap(), -12_000_000_000_000_000_003i128),
            // The positive control.
            (n(6, 18), n(6, 18).add(n(3, 0), ToNearest).unwrap(), 12_000_000_000_000_000_003),
            // Mixed signs, the negative the larger.
            (n(1, 18), n(-9, 18).sub(n(3, 0), ToNearest).unwrap(), -8_000_000_000_000_000_003),
            // Mixed signs, the positive the larger.
            (n(-1, 18), n(9, 18).add(n(3, 0), ToNearest).unwrap(), 8_000_000_000_000_000_003),
        ];
        for (a, b, exact) in pairs {
            assert_eq!(a.external_exponent(), b.external_exponent());
            assert_eq!(int(a) + int(b), exact);
            let down = int(a.add(b, Downward).unwrap());
            let up = int(a.add(b, Upward).unwrap());
            assert!(down <= exact, "Downward {a:?} + {b:?}: {down} > {exact}");
            assert!(up >= exact, "Upward {a:?} + {b:?}: {up} < {exact}");
            assert!(up - down <= 10, "{a:?} + {b:?}: {down} .. {up}");
        }
    }

    /// T:2815-2875: x + y at nearest is the exact sum rounded to the result's
    /// last place — `expected q` in units of 1e28.
    #[test]
    fn large330_add_to_nearest_picks_the_nearer() {
        let _g = large();
        let x = n(-1_074_956_551_220_905_975, 28);
        let cases: [(Number, i64); 7] = [
            (n(5_175_909_259_972_499_745, 22), -1_074_951_375_311_646_003),
            (n(1, 0), -1_074_956_551_220_905_975),
            (n(1, 10), -1_074_956_551_220_905_975),
            (n(1, 20), -1_074_956_551_220_905_975),
            (n(1, 27), -1_074_956_551_220_905_975),
            (n(1, 28), -1_074_956_551_220_905_974),
            (n(1, 31), -1_074_956_551_220_904_975),
        ];
        for (y, q) in cases {
            assert_eq!(x.add(y, ToNearest).unwrap(), n(q, 28), "{x:?} + {y:?}");
        }
    }

    /// T:2877-3051: only kMaxRep and kMaxRepUp are representable in between,
    /// so kMaxRep + f and kMaxRepUp − f pick one of the two by the mode
    /// (the midpoint is kMaxRep + 1.5).
    #[test]
    fn large330_cusp_rounding_with_fractional_parts() {
        let _g = large();
        let below = n(K_MAX, 0);
        let above = nu(false, K_MAX_UP, 0, ToNearest);
        for mode in MODES {
            for tenths in [4, 5, 6, 14, 15, 16, 24, 25, 26] {
                let operand = n(tenths, -1);
                let towards_zero = mode == TowardsZero || mode == Downward;
                let want = if towards_zero || (mode == ToNearest && tenths < 15) { below } else { above };
                assert_eq!(below.add(operand, mode).unwrap(), want, "kMaxRep + {tenths}e-1 {mode:?}");
                let want = if towards_zero || (mode == ToNearest && tenths > 15) { below } else { above };
                assert_eq!(above.sub(operand, mode).unwrap(), want, "kMaxRepUp - {tenths}e-1 {mode:?}");
            }
        }
    }

    /// IOUAmount.h:120-124 (`Number{IOUAmount}`), IOUAmount.cpp:28-37
    /// (`fromNumber`), T:1411-1437 (conversions).
    #[test]
    fn large330_iou_conversions() {
        let _g = large();
        // IOUAmount{5, 6} is 5000000000000000e-9; as a Number it is Number{5, 6}.
        let y = Number::from_iou(false, 5_000_000_000_000_000, -9);
        assert_eq!(y, n(5, 6));
        assert_eq!(raw(y), (false, 5_000_000_000_000_000_000, -12));
        for mode in MODES {
            assert_eq!(y.iou_parts(mode).unwrap(), (false, 5_000_000_000_000_000, -9), "{mode:?}");
            assert_eq!(y.iou_rounded(mode).unwrap(), y, "{mode:?}");
        }
        assert_eq!(Number::from_iou(false, 0, 0), Number::ZERO);
        assert_eq!(Number::ZERO.iou_parts(ToNearest).unwrap(), (false, 0, 0));
        // Nineteen digits round to sixteen under the mode, the sign deciding
        // which way Upward and Downward go (the guard is negative).
        let v = n(1_000_000_000_000_065_556, -18);
        let (lo, hi) = (1_000_000_000_000_065u64, 1_000_000_000_000_066u64);
        assert_eq!(v.iou_parts(ToNearest).unwrap(), (false, hi, -15));
        assert_eq!(v.iou_parts(TowardsZero).unwrap(), (false, lo, -15));
        assert_eq!(v.iou_parts(Downward).unwrap(), (false, lo, -15));
        assert_eq!(v.iou_parts(Upward).unwrap(), (false, hi, -15));
        assert_eq!(v.negated().iou_parts(ToNearest).unwrap(), (true, hi, -15));
        assert_eq!(v.negated().iou_parts(TowardsZero).unwrap(), (true, lo, -15));
        assert_eq!(v.negated().iou_parts(Downward).unwrap(), (true, hi, -15));
        assert_eq!(v.negated().iou_parts(Upward).unwrap(), (true, lo, -15));
        assert_eq!(v.iou_rounded(ToNearest).unwrap(), n(1_000_000_000_000_066, -15));
        // A mantissa above kMaxRep goes through its internal digits.
        assert_eq!(nu(false, 9_999_999_999_999_999_990, -4, ToNearest).iou_parts(Downward).unwrap(), (false, 9_999_999_999_999_999, -1));
        assert_eq!(nu(false, 9_999_999_999_999_999_990, -4, ToNearest).iou_parts(ToNearest).unwrap(), (false, 1_000_000_000_000_000, 0));
        // `normalizeToRange` with another range (Number.h:840-873).
        assert_eq!(n(123_456, 0).normalize_to_range(100, 999, ToNearest).unwrap(), (false, 123, 3));
        assert_eq!(n(123_456, 0).normalize_to_range(100, 999, Upward).unwrap(), (false, 124, 3));
    }
}
