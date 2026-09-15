//! Track 1 — test-facing surface over the engine's arithmetic, so a
//! differential harness (crates/xrpl-node/tests/arith_fuzz.rs, `--features
//! ffi`) can put the same operands through libxrpl and through us. Every
//! function here is a thin `pub` wrapper; the semantics live in `offer.rs`,
//! `amm_swap.rs` and `keylet.rs`. Not for engine use.
use super::amm_swap::{self, Rnd};
use super::offer;

/// Mantissa/exponent pair, the engine's `Me` (an alias for the same tuple).
pub type Me = (u128, i32);

/// `mulRound(v1, v2, IOU, roundUp)` — the lossy legacy form (see offer.rs).
pub fn mul_round16(a: Me, b: Me, round_up: bool) -> Me {
    if round_up { offer::mul_round16_up(a, b) } else { offer::mul_round16_down(a, b) }
}
/// `mulRoundStrict(v1, v2, XRP, roundUp)` → whole drops.
pub fn mul_round_drops_strict(a: Me, b: Me, round_up: bool) -> u128 {
    offer::mul_round_drops_strict(a, b, round_up)
}
/// `mulRound(v1, v2, XRP, roundUp = true)` — non-strict, whole drops.
pub fn mul_round_drops(a: Me, b: Me) -> u128 {
    offer::mul_round_drops(a, b)
}
/// `divRound(a, rate, IOU, roundUp = true)`.
pub fn div_round16_up(a: Me, rate: Me) -> Me {
    offer::div_round16_up(a, rate)
}
/// `a * b / c` with floor or ceiling at 16 digits.
pub fn me_muldiv(a: Me, b: Me, c: Me, ceil: bool) -> Me {
    offer::me_muldiv(a, b, c, ceil)
}
/// STAmount signed add: (aneg, a) + (bneg, b) → (neg, magnitude).
pub fn stamount_signed_add(aneg: bool, a: Me, bneg: bool, b: Me) -> (bool, Me) {
    offer::stamount_signed_add(aneg, a, bneg, b)
}
/// 16-digit canonicalisation.
pub fn me_norm(a: Me) -> Me {
    offer::me_norm(a)
}
/// `getRate(offerOut = gets, offerIn = pays)`.
pub fn rate_encode_native(pays_m: u128, pays_e: i32, pays_native: bool, gets_m: u128, gets_e: i32, gets_native: bool) -> Option<u64> {
    crate::ledger::keylet::rate_encode_native(pays_m, pays_e, pays_native, gets_m, gets_e, gets_native)
}
fn rnd(mode: u8) -> Rnd {
    match mode { 2 | 1 => Rnd::Down, 3 => Rnd::Up, _ => Rnd::Near }
}
/// `Number` ops: 0 add, 1 sub, 2 mul, 3 div, 4 root2; mode 0 nearest, 1 towards-zero, 2 down, 3 up.
pub fn number_op(op: u8, a: Me, b: Me, mode: u8) -> Me {
    match op {
        0 => amm_swap::n_add(a, b, rnd(mode)),
        1 => amm_swap::n_sub(a, b, rnd(mode)),
        2 => amm_swap::n_mul(a, b, rnd(mode)),
        3 => amm_swap::n_div(a, b, rnd(mode)),
        _ => amm_swap::n_sqrt(a),
    }
}
/// `Number` ops through the literal port (`tx::number`), signed: 0 add, 1 sub,
/// 2 mul, 3 div, 4 root2 (unary; `b` ignored); mode 0 nearest, 1 towards-zero, 2 down, 3 up. `Err` where
/// rippled throws (overflow, divide by zero).
pub fn number_op_signed(op: u8, a: (i64, i32), b: (i64, i32), mode: u8) -> Result<(i64, i32), String> {
    use super::number::{Number, Rounding};
    let mode = match mode { 1 => Rounding::TowardsZero, 2 => Rounding::Downward, 3 => Rounding::Upward, _ => Rounding::ToNearest };
    let x = Number::new(a.0, a.1, mode).map_err(|e| format!("{e:?}"))?;
    let y = Number::new(b.0, b.1, mode).map_err(|e| format!("{e:?}"))?;
    let r = match op {
        0 => x.add(y, mode),
        1 => x.sub(y, mode),
        2 => x.mul(y, mode),
        3 => x.div(y, mode),
        _ => x.root2(mode),
    }
    .map_err(|e| format!("{e:?}"))?;
    Ok((r.signed_mantissa(), r.exponent))
}
/// `XRPAmount{Number}` — `Number::operator rep()` under `mode` (0 nearest,
/// 1 towards-zero, 2 down, 3 up), through the port.
pub fn number_to_drops(a: (i64, i32), mode: u8) -> Result<i64, String> {
    use super::number::{Number, Rounding};
    let mode = match mode { 1 => Rounding::TowardsZero, 2 => Rounding::Downward, 3 => Rounding::Upward, _ => Rounding::ToNearest };
    Number::new(a.0, a.1, mode).and_then(|n| n.to_drops(mode)).map_err(|e| format!("{e:?}"))
}
/// STAmount `divide(num, den, IOU)` — `muldiv(num, 1e17, den) + 5` at
/// offset −17, canonicalised at nearest (the engine's `n_div_rate`).
pub fn divide16(a: Me, b: Me) -> Me {
    amm_swap::n_div_rate(a, b)
}
/// Round an arbitrary (mantissa, exponent) to 16 significant digits, nearest
/// (ties to even) — `STAmount`'s constructor canonicalisation under Number's
/// default ToNearest mode.
pub fn round16_nearest(a: Me) -> Me {
    if a.0 == 0 {
        return (0, 0);
    }
    super::amm_swap::round16(a.0, a.1, false, super::amm_swap::Rnd::Near)
}
/// `mulRound(a, b, IOU, roundUp = false)` — legacy: nearest of the truncated product.
pub fn mul_round16_legacy_down(a: Me, b: Me) -> Me {
    offer::mul_round16_legacy_down(a, b)
}
/// 16-digit truncation (TowardsZero) of an arbitrary pair.
pub fn norm16_trunc(a: Me) -> Me {
    offer::norm16(a)
}
/// 16-digit ceiling of an arbitrary pair.
pub fn round16_up(a: Me) -> Me {
    if a.0 == 0 { return (0, 0); }
    super::amm_swap::round16(a.0, a.1, false, super::amm_swap::Rnd::Up)
}

/// Track 2's `flow::st_amount` — the four rippled rounding functions written
/// once from STAmount.cpp. `op`: 0 mulRound, 1 mulRoundStrict, 2 divRound,
/// 3 divRoundStrict; operands are IOU (mantissa, exponent) unless `*_native`.
pub fn st_round(op: u8, a: (u64, i32, bool), b: (u64, i32, bool), result_native: bool, round_up: bool) -> Result<(u64, i32, bool), String> {
    use crate::flow::st_amount::{div_round, div_round_strict, mul_round, mul_round_strict, StAmount};
    // The shim builds each operand with `STAmount(asset, mantissa, exponent,
    // negative)`, which canonicalises a short IOU mantissa; mirror that here
    // (f375: every mismatch was a 10–15 digit operand fed raw).
    let mk = |v: (u64, i32, bool)| if v.2 { StAmount::drops(v.0) } else { StAmount::iou_me(false, (v.0 as u128, v.1)) };
    let (x, y) = (mk(a), mk(b));
    let r = match op {
        0 => mul_round(&x, &y, result_native, round_up),
        1 => mul_round_strict(&x, &y, result_native, round_up),
        2 => div_round(&x, &y, result_native, round_up),
        _ => div_round_strict(&x, &y, result_native, round_up),
    }
    .map_err(|e| format!("{e:?}"))?;
    Ok((r.mantissa, r.exponent, r.native))
}
