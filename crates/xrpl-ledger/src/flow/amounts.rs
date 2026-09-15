//! The amount types the path engine moves: rippled's `XRPAmount`,
//! `IOUAmount` and the `EitherAmount` variant the `Step` interface speaks
//! (`include/xrpl/tx/paths/detail/EitherAmount.h`).
//!
//! An `IOUAmount` is a signed 16-digit decimal — rippled's `Number` at the
//! Small scale with STAmount's exponent range — so its arithmetic is
//! `tx::number` at ToNearest, exactly `IOUAmount::operator+=` (IOUAmount.cpp:
//! `Number{*this} + Number{other}`, then `IOUAmount(Number)` clamps the
//! exponent to [kMinOffset, kMaxOffset]).
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
    pub fn from_number(n: Number) -> IouAmount {
        if n.is_zero() || n.exponent < K_MIN_OFFSET || n.exponent > K_MAX_OFFSET {
            return IouAmount::ZERO;
        }
        IouAmount { negative: n.negative, mantissa: n.mantissa, exponent: n.exponent }
    }

    /// The engine's unsigned `Me` pair with a sign, normalised.
    pub fn from_me(negative: bool, m: Me) -> IouAmount {
        match Number::from_parts(negative, m.0, m.1, Rounding::ToNearest) {
            Ok(n) => IouAmount::from_number(n),
            Err(_) => IouAmount::ZERO,
        }
    }

    pub fn number(self) -> Number {
        Number { negative: self.negative, mantissa: self.mantissa, exponent: self.exponent }
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
}
