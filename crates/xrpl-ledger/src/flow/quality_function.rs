//! rippled `QualityFunction` (`include/xrpl/protocol/QualityFunction.h`,
//! `src/libxrpl/protocol/QualityFunction.cpp`): the average quality of a
//! step as a function of its output, `q(out) = m·out + b`. A CLOB-like step
//! is constant (`m = 0`, `b = 1/rate`); an AMM step is the line through its
//! pool balances at the trading fee; a strand's function is the composition
//! of its steps' functions (`combine`), and `outFromAvgQ` inverts it to size
//! the output that lands exactly on a target average quality (StrandFlow's
//! `limitOut`).
//!
//! Arithmetic is `tx::number` — rippled computes these in `Number` at the
//! thread's rounding mode, ToNearest except the Upward `outFromAvgQ`.
use crate::tx::number::{Number, NumberError, Rounding};

/// rippled `Quality`: a 64-bit encoded rate (exponent byte, 56-bit
/// mantissa), rate = in / out — a SMALLER value is a BETTER quality.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Quality(pub u64);

impl Quality {
    /// `Quality::rate()`: the encoded rate as a Number (in per out).
    pub fn rate(self) -> Number {
        let (m, e) = crate::tx::offer::rate_me(self.0);
        Number::from_parts(false, m, e, Rounding::ToNearest).unwrap_or(Number::ZERO)
    }
}

/// AMMCore.h `getFee` / `feeMult`: the trading fee in 1/100000 units.
pub fn fee_mult(tfee: u32, mode: Rounding) -> Result<Number, NumberError> {
    let fee = Number::new(tfee as i64, 0, mode)?.div(Number::new(100_000, 0, mode)?, mode)?;
    Number::new(1, 0, mode)?.sub(fee, mode)
}

#[derive(Clone, Copy, Debug)]
pub struct QualityFunction {
    m: Number,
    b: Number,
    quality: Option<Quality>,
}

impl QualityFunction {
    /// `QualityFunction(Quality, CLOBLikeTag)`: constant, `b = 1 / rate`.
    pub fn clob_like(quality: Quality) -> Result<QualityFunction, NumberError> {
        let rate = quality.rate();
        if rate.is_zero() {
            return Err(NumberError::DivideByZero); // "QualityFunction quality rate is 0."
        }
        let one = Number::new(1, 0, Rounding::ToNearest)?;
        Ok(QualityFunction { m: Number::ZERO, b: one.div(rate, Rounding::ToNearest)?, quality: Some(quality) })
    }

    /// `QualityFunction(TAmounts, tfee, AMMTag)`: `m = −cfee/in`,
    /// `b = out·cfee/in` for the pool's (in, out) balances.
    pub fn amm(pool_in: Number, pool_out: Number, tfee: u32) -> Result<QualityFunction, NumberError> {
        if pool_in.is_zero() || pool_out.is_zero() || pool_in.negative || pool_out.negative {
            return Err(NumberError::DivideByZero); // "QualityFunction amounts are 0."
        }
        let r = Rounding::ToNearest;
        let cfee = fee_mult(tfee, r)?;
        let m = cfee.negated().div(pool_in, r)?;
        let b = pool_out.mul(cfee, r)?.div(pool_in, r)?;
        Ok(QualityFunction { m, b, quality: None })
    }

    /// `combine`: `m += b·qf.m; b *= qf.b`; a non-zero slope drops the
    /// constant quality.
    pub fn combine(&mut self, qf: &QualityFunction) -> Result<(), NumberError> {
        let r = Rounding::ToNearest;
        self.m = self.m.add(self.b.mul(qf.m, r)?, r)?;
        self.b = self.b.mul(qf.b, r)?;
        if !self.m.is_zero() {
            self.quality = None;
        }
        Ok(())
    }

    /// `outFromAvgQ`: the output at which the average quality equals
    /// `quality` — `(1/rate − b) / m`, computed with Upward rounding; None
    /// for a constant function, a zero rate, or a non-positive answer.
    pub fn out_from_avg_q(&self, quality: Quality) -> Result<Option<Number>, NumberError> {
        let rate = quality.rate();
        if self.m.is_zero() || rate.is_zero() {
            return Ok(None);
        }
        let r = Rounding::Upward;
        let one = Number::new(1, 0, r)?;
        let out = one.div(rate, r)?.sub(self.b, r)?.div(self.m, r)?;
        if out.negative || out.is_zero() {
            return Ok(None);
        }
        Ok(Some(out))
    }

    pub fn is_const(&self) -> bool {
        self.quality.is_some()
    }

    pub fn quality(&self) -> Option<Quality> {
        self.quality
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clob_functions_compose_to_the_product_of_rates() {
        // rate 2 (in per out) then rate 3: b = 1/2 · 1/3 = 1/6, still constant.
        let q2 = Quality(crate::ledger::keylet::rate_encode_native(2, 0, false, 1, 0, false).unwrap());
        let q3 = Quality(crate::ledger::keylet::rate_encode_native(3, 0, false, 1, 0, false).unwrap());
        let mut f = QualityFunction::clob_like(q2).unwrap();
        f.combine(&QualityFunction::clob_like(q3).unwrap()).unwrap();
        assert!(f.is_const());
        // rippled multiplies the two intercepts: 0.5 × 0.3333333333333333 =
        // 0.16666666666666665, which rounds half-even to …666 — not the
        // exact 1/6 (…667). The port rounds where rippled rounds.
        assert_eq!((f.b.mantissa, f.b.exponent), (1_666_666_666_666_666, -16));
        assert!(f.out_from_avg_q(q2).unwrap().is_none());
    }

    #[test]
    fn amm_function_inverts() {
        // Pool 1000 in / 2000 out, no fee: q(out) = (2000 − out) / 1000. The
        // average quality 1.5 (rate 1/1.5 in per out) sits at out = 500.
        let n = |v: i64| Number::new(v, 0, Rounding::ToNearest).unwrap();
        let f = QualityFunction::amm(n(1000), n(2000), 0).unwrap();
        assert!(!f.is_const());
        let target = Quality(crate::ledger::keylet::rate_encode_native(2, 0, false, 3, 0, false).unwrap());
        let out = f.out_from_avg_q(target).unwrap().unwrap();
        // 1/rate = 1.5 exactly; (1.5 − 2) / (−0.001) = 500.
        assert_eq!((out.mantissa, out.exponent), (5_000_000_000_000_000, -13));
    }
}
