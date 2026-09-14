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
