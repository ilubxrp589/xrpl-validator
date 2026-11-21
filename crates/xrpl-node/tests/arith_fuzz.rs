//! Track 1 — differential fuzz of the engine's arithmetic against libxrpl.
//!
//! Each case builds random operands, runs them through libxrpl via the shim
//! (`xrpl_ffi::arith`) and through our port (`xrpl_ledger::tx::arith_probe`),
//! and records every last-digit disagreement. The test prints a per-op table
//! and the first examples of each class, then fails if anything disagreed —
//! run with `--nocapture` to read the list. Deterministic: a fixed xorshift
//! seed, overridable with ARITH_FUZZ_SEED; case count with ARITH_FUZZ_N.
use xrpl_ffi::arith;
use xrpl_ffi::{XrplAmt, ARITH_ADD, ARITH_CANONICALIZE, ARITH_DIVIDE, ARITH_DIVROUND, ARITH_DIVROUND_STRICT, ARITH_MULROUND, ARITH_MULROUND_STRICT, ARITH_MULTIPLY, ARITH_SUB, NUM_TO_DROPS};
use xrpl_ledger::tx::arith_probe as ours;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
    /// A canonical IOU mantissa in [1e15, 1e16) with a mix of shapes.
    fn mantissa(&mut self) -> u64 {
        match self.next() % 6 {
            0 => 1_000_000_000_000_000,
            1 => 9_999_999_999_999_999,
            2 => self.range(1_000_000_000_000_000, 1_000_000_000_000_999), // near 1e15
            3 => (self.range(1, 9_999_999) * 1_000_000_000) + self.range(0, 999_999_999), // random 16-digit
            _ => self.range(1_000_000_000_000_000, 9_999_999_999_999_999),
        }
    }
    fn exponent(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i32
    }
    fn drops(&mut self) -> u64 {
        match self.next() % 4 {
            0 => self.range(1, 1_000),
            1 => self.range(1, 100_000_000),
            2 => self.range(1, 100_000_000_000_000),
            _ => self.range(1, 100_000_000_000_000_000),
        }
    }
}

fn iou(m: u64, e: i32) -> XrplAmt {
    arith::iou(m, e)
}

#[derive(Default)]
struct Tally {
    cases: u64,
    mismatches: u64,
    oracle_errors: u64,
    examples: Vec<String>,
}
impl Tally {
    fn note(&mut self, ok: bool, example: impl FnOnce() -> String) {
        self.cases += 1;
        if !ok {
            self.mismatches += 1;
            if self.examples.len() < 6 {
                self.examples.push(example());
            }
        }
    }
}

fn fmt_amt(a: &XrplAmt) -> String {
    if a.native != 0 { format!("{}{}drops", if a.negative != 0 { "-" } else { "" }, a.mantissa) }
    else { format!("{}{}e{}", if a.negative != 0 { "-" } else { "" }, a.mantissa, a.exponent) }
}

#[test]
fn arithmetic_agrees_with_libxrpl() {
    let seed = std::env::var("ARITH_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x9E37_79B9_7F4A_7C15u64);
    let n: u64 = std::env::var("ARITH_FUZZ_N").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    // Mainnet runs Number's 16-digit Small scale (the large scale waits on
    // SingleAssetVault / LendingProtocol); ARITH_SCALE=large fuzzes the other.
    let large = std::env::var("ARITH_SCALE").map(|v| v == "large").unwrap_or(false);
    arith::set_scale(large);
    eprintln!("[arith_fuzz] Number scale: {}", if large { "Large330 (19 digits)" } else { "Small (16 digits)" });
    let mut rng = Rng(seed);
    let mut t: std::collections::BTreeMap<&'static str, Tally> = Default::default();

    for _ in 0..n {
        // --- mulRound IOU×IOU→IOU, both roundings -------------------------------
        let (am, ae, bm, be) = (rng.mantissa(), rng.exponent(-30, 10), rng.mantissa(), rng.exponent(-30, 10));
        for round_up in [true, false] {
            let key = if round_up { "mulRound IOU up" } else { "mulRound IOU down" };
            match arith::stamount_op(ARITH_MULROUND, iou(am, ae), iou(bm, be), round_up, false) {
                Ok(r) => {
                    let o = if round_up { ours::mul_round16((am as u128, ae), (bm as u128, be), true) } else { ours::mul_round16_legacy_down((am as u128, ae), (bm as u128, be)) };
                    let o = ours::me_norm(o);
                    let ok = r.mantissa as u128 == o.0 && r.exponent == o.1;
                    t.entry(key).or_default().note(ok, || format!("{}e{} * {}e{} -> libxrpl {} ours {}e{}", am, ae, bm, be, fmt_amt(&r), o.0, o.1));
                }
                Err(_) => t.entry(key).or_default().oracle_errors += 1,
            }
        }
        // --- mulRoundStrict IOU×IOU→XRP (drops), both roundings ------------------
        // rate-shaped second operand: drops per unit, so the product is drops.
        let (rm, re) = (rng.mantissa(), rng.exponent(-12, -6));
        let (vm, ve) = (rng.mantissa(), rng.exponent(-18, -10));
        for round_up in [true, false] {
            let key = if round_up { "mulRoundStrict XRP up" } else { "mulRoundStrict XRP down" };
            match arith::stamount_op(ARITH_MULROUND_STRICT, iou(vm, ve), iou(rm, re), round_up, true) {
                Ok(r) => {
                    let o = ours::mul_round_drops_strict((vm as u128, ve), (rm as u128, re), round_up);
                    let ok = r.mantissa as u128 == o;
                    t.entry(key).or_default().note(ok, || format!("{}e{} * {}e{} -> libxrpl {} ours {}drops", vm, ve, rm, re, fmt_amt(&r), o));
                }
                Err(_) => t.entry(key).or_default().oracle_errors += 1,
            }
        }
        // --- mulRound IOU×IOU→XRP non-strict, round up ----------------------------
        match arith::stamount_op(ARITH_MULROUND, iou(vm, ve), iou(rm, re), true, true) {
            Ok(r) => {
                let o = ours::mul_round_drops((vm as u128, ve), (rm as u128, re));
                let ok = r.mantissa as u128 == o;
                t.entry("mulRound XRP up (legacy)").or_default().note(ok, || format!("{}e{} * {}e{} -> libxrpl {} ours {}drops", vm, ve, rm, re, fmt_amt(&r), o));
            }
            Err(_) => t.entry("mulRound XRP up (legacy)").or_default().oracle_errors += 1,
        }
        // --- divRound IOU/IOU→IOU round up ---------------------------------------
        match arith::stamount_op(ARITH_DIVROUND, iou(am, ae), iou(bm, be), true, false) {
            Ok(r) => {
                let o = ours::me_norm(ours::div_round16_up((am as u128, ae), (bm as u128, be)));
                let ok = r.mantissa as u128 == o.0 && r.exponent == o.1;
                t.entry("divRound IOU up").or_default().note(ok, || format!("{}e{} / {}e{} -> libxrpl {} ours {}e{}", am, ae, bm, be, fmt_amt(&r), o.0, o.1));
            }
            Err(_) => t.entry("divRound IOU up").or_default().oracle_errors += 1,
        }
        // --- Track 2 st_amount: the four rounding functions × result asset ×
        // rounding, written once from STAmount.cpp — every cell against libxrpl.
        {
            let ops = [(ARITH_MULROUND, 0u8, "mulRound"), (ARITH_MULROUND_STRICT, 1, "mulRoundStrict"), (ARITH_DIVROUND, 2, "divRound"), (ARITH_DIVROUND_STRICT, 3, "divRoundStrict")];
            let (xm, xe) = (rng.mantissa(), rng.exponent(-12, 4));
            let (ym, ye) = (rng.mantissa(), rng.exponent(-12, 4));
            let drops_a = rng.drops();
            for (shim_op, op, name) in ops {
                for result_native in [false, true] {
                    for round_up in [true, false] {
                        // IOU op IOU → IOU, or IOU op IOU → XRP (the ceil_out / ceil_in shapes);
                        // for the XRP result, the multiply's first operand is drops
                        // (limit × rate) and the divide's numerator is drops (limit / rate).
                        let (a, b) = if result_native && op <= 1 {
                            ((drops_a, 0, true), (ym, ye, false))
                        } else if result_native {
                            ((drops_a, 0, true), (ym, ye, false))
                        } else {
                            ((xm, xe, false), (ym, ye, false))
                        };
                        let key: &'static str = Box::leak(format!("st {name} {} {}", if result_native { "XRP" } else { "IOU" }, if round_up { "up" } else { "down" }).into_boxed_str());
                        let fa = if a.2 { arith::xrp(a.0) } else { iou(a.0, a.1) };
                        let fb = if b.2 { arith::xrp(b.0) } else { iou(b.0, b.1) };
                        match arith::stamount_op(shim_op, fa, fb, round_up, result_native) {
                            Ok(r) => {
                                let o = ours::st_round(op, a, b, result_native, round_up);
                                let ok = matches!(o, Ok((m, e, n)) if m == r.mantissa && (m == 0 || e == r.exponent) && n == (r.native != 0));
                                t.entry(key).or_default().note(ok, || format!("{:?} {name} {:?} -> libxrpl {} ours {:?}", a, b, fmt_amt(&r), o));
                            }
                            Err(_) => t.entry(key).or_default().oracle_errors += 1,
                        }
                    }
                }
            }
        }
        // --- STAmount multiply / divide (IOU): Number product at nearest; the
        // legacy `muldiv(num, 1e17, den) + 5` quotient ----------------------------
        match arith::stamount_op(ARITH_MULTIPLY, iou(am, ae), iou(bm, be), false, false) {
            Ok(r) => {
                let o = ours::number_op_signed(2, (am as i64, ae), (bm as i64, be), 0);
                let ok = matches!(o, Ok((m, e)) if m as u128 == r.mantissa as u128 && e == r.exponent);
                t.entry("STAmount multiply IOU").or_default().note(ok, || format!("{}e{} * {}e{} -> libxrpl {} ours {:?}", am, ae, bm, be, fmt_amt(&r), o));
            }
            Err(_) => t.entry("STAmount multiply IOU").or_default().oracle_errors += 1,
        }
        match arith::stamount_op(ARITH_DIVIDE, iou(am, ae), iou(bm, be), false, false) {
            Ok(r) => {
                let o = ours::divide16((am as u128, ae), (bm as u128, be));
                let ok = r.mantissa as u128 == o.0 && r.exponent == o.1;
                t.entry("STAmount divide IOU").or_default().note(ok, || format!("{}e{} / {}e{} -> libxrpl {} ours {}e{}", am, ae, bm, be, fmt_amt(&r), o.0, o.1));
            }
            Err(_) => t.entry("STAmount divide IOU").or_default().oracle_errors += 1,
        }
        // --- Number → drops under the three modes ---------------------------------
        {
            let (dm, de) = (rng.mantissa(), rng.exponent(-20, 2));
            for mode in [0u8, 2, 3] {
                let key: &'static str = match mode { 0 => "Number to_drops nearest", 2 => "Number to_drops down", _ => "Number to_drops up" };
                match arith::number_op(NUM_TO_DROPS, (dm as i64, de), (1, 0), mode as i32) {
                    Ok((d, _)) => {
                        let o = ours::number_to_drops((dm as i64, de), mode);
                        let ok = matches!(o, Ok(od) if od == d);
                        t.entry(key).or_default().note(ok, || format!("{}e{} mode{} -> libxrpl {} drops ours {:?}", dm, de, mode, d, o));
                    }
                    Err(_) => t.entry(key).or_default().oracle_errors += 1,
                }
            }
        }
        // --- STAmount add / sub (IOU) --------------------------------------------
        // Gaps up to twenty digits either way: the wide ones are where Number's
        // guard cannot borrow and the exact-then-round model used to disagree.
        let (cm, ce) = (rng.mantissa(), ae + if rng.next() % 2 == 0 { rng.exponent(-3, 3) } else { rng.exponent(-20, 20) });
        match arith::stamount_op(ARITH_ADD, iou(am, ae), iou(cm, ce), false, false) {
            Ok(r) => {
                let (neg, o) = ours::stamount_signed_add(false, (am as u128, ae), false, (cm as u128, ce));
                let ok = !neg && r.mantissa as u128 == o.0 && r.exponent == o.1 && r.negative == 0;
                t.entry("STAmount add").or_default().note(ok, || format!("{}e{} + {}e{} -> libxrpl {} ours {}e{}", am, ae, cm, ce, fmt_amt(&r), o.0, o.1));
            }
            Err(_) => t.entry("STAmount add").or_default().oracle_errors += 1,
        }
        match arith::stamount_op(ARITH_SUB, iou(am, ae), iou(cm, ce), false, false) {
            Ok(r) => {
                let (neg, o) = ours::stamount_signed_add(false, (am as u128, ae), true, (cm as u128, ce));
                let ok = (r.negative != 0) == neg && r.mantissa as u128 == o.0 && (r.mantissa == 0 || r.exponent == o.1);
                t.entry("STAmount sub").or_default().note(ok, || format!("{}e{} - {}e{} -> libxrpl {} ours {}{}e{}", am, ae, cm, ce, fmt_amt(&r), if neg { "-" } else { "" }, o.0, o.1));
            }
            Err(_) => t.entry("STAmount sub").or_default().oracle_errors += 1,
        }
        // --- canonicalize: an un-normalised IOU mantissa through the constructor --
        let raw_m = rng.range(1, u64::MAX / 2);
        let raw_e = rng.exponent(-40, 20);
        match arith::stamount_op(ARITH_CANONICALIZE, iou(raw_m, raw_e), iou(1, 0), false, false) {
            Ok(r) => {
                let o = ours::round16_nearest((raw_m as u128, raw_e));
                let ok = r.mantissa as u128 == o.0 && (r.mantissa == 0 || r.exponent == o.1);
                t.entry("canonicalize IOU").or_default().note(ok, || format!("{}e{} -> libxrpl {} ours {}e{}", raw_m, raw_e, fmt_amt(&r), o.0, o.1));
            }
            Err(_) => t.entry("canonicalize IOU").or_default().oracle_errors += 1,
        }
        // --- getRate: XRP pays / IOU gets and IOU / IOU ---------------------------
        let drops = rng.drops();
        let gets = (rng.mantissa(), rng.exponent(-12, 6));
        let r_ffi = arith::get_rate(iou(gets.0, gets.1), arith::xrp(drops));
        let r_ours = ours::rate_encode_native(drops as u128, 0, true, gets.0 as u128, gets.1, false).unwrap_or(0);
        t.entry("getRate XRP/IOU").or_default().note(r_ffi == r_ours, || format!("pays {}drops gets {}e{} -> libxrpl {:016x} ours {:016x}", drops, gets.0, gets.1, r_ffi, r_ours));
        let pays = (rng.mantissa(), rng.exponent(-12, 6));
        let r_ffi = arith::get_rate(iou(gets.0, gets.1), iou(pays.0, pays.1));
        let r_ours = ours::rate_encode_native(pays.0 as u128, pays.1, false, gets.0 as u128, gets.1, false).unwrap_or(0);
        t.entry("getRate IOU/IOU").or_default().note(r_ffi == r_ours, || format!("pays {}e{} gets {}e{} -> libxrpl {:016x} ours {:016x}", pays.0, pays.1, gets.0, gets.1, r_ffi, r_ours));
        // --- Number ops under the four rounding modes ------------------------------
        for (op, name) in [(0u8, "Number add"), (1, "Number sub"), (2, "Number mul"), (3, "Number div"), (4, "Number root2")] {
            for mode in [0u8, 2, 3] {
                let key: &'static str = match (name, mode) {
                    ("Number add", 0) => "Number add nearest", ("Number add", 2) => "Number add down", ("Number add", _) => "Number add up",
                    ("Number sub", 0) => "Number sub nearest", ("Number sub", 2) => "Number sub down", ("Number sub", _) => "Number sub up",
                    ("Number mul", 0) => "Number mul nearest", ("Number mul", 2) => "Number mul down", ("Number mul", _) => "Number mul up",
                    ("Number div", 0) => "Number div nearest", ("Number div", 2) => "Number div down", ("Number div", _) => "Number div up",
                    (_, 0) => "Number root2 nearest", (_, 2) => "Number root2 down", (_, _) => "Number root2 up",
                };
                // Signed, exact: both sides are 16-digit Number results at the Small scale.
                let (xa, xb) = ((am, ae), (bm, be));
                match arith::number_op(op as i32, (xa.0 as i64, xa.1), (xb.0 as i64, xb.1), mode as i32) {
                    Ok((m, e)) => {
                        let o = ours::number_op_signed(op, (xa.0 as i64, xa.1), (xb.0 as i64, xb.1), mode);
                        let ok = matches!(o, Ok((om, oe)) if om == m && (m == 0 || oe == e));
                        t.entry(key).or_default().note(ok, || format!("{}e{} op{} {}e{} mode{} -> libxrpl {}e{} ours {:?}", xa.0, xa.1, op, xb.0, xb.1, mode, m, e, o));
                    }
                    Err(_) => t.entry(key).or_default().oracle_errors += 1,
                }
            }
        }
    }

    let mut total_mismatch = 0;
    eprintln!("\n{:<28} {:>8} {:>10} {:>8}", "op", "cases", "mismatch", "oracleErr");
    for (k, v) in &t {
        eprintln!("{:<28} {:>8} {:>10} {:>8}", k, v.cases, v.mismatches, v.oracle_errors);
        total_mismatch += v.mismatches;
    }
    for (k, v) in &t {
        for ex in &v.examples {
            eprintln!("  [{k}] {ex}");
        }
    }
    assert_eq!(total_mismatch, 0, "arithmetic disagrees with libxrpl — see the table above");
}

/// One operand for the Large330 fuzz: a signed 64-bit mantissa and an
/// exponent, as `Number(rep, int)` takes them. The shapes aim at what the
/// 19-digit scale adds: the range ends (1e18, 1e19 − 1), the int64 cusp
/// (`kMaxRep` = 2^63 − 1 and the three values up to `kMaxRepUp`, which only
/// an exponent of +1 can spell), mantissas above `kMaxRep` (held as a
/// multiple of ten), and ordinary 16-digit IOU mantissas being widened.
fn large_operand(rng: &mut Rng) -> (i64, i32) {
    const K_MAX: u64 = i64::MAX as u64;
    let (m, lift): (u64, i32) = match rng.next() % 12 {
        0 => (1_000_000_000_000_000_000, 0),
        1 => (K_MAX - rng.range(0, 12), 0),
        // (kMaxRep/10 − 2 ..= kMaxRep/10 + 3) × 10: straddles kMaxRep and kMaxRepUp.
        2 => (K_MAX / 10 - 2 + rng.range(0, 5), 1),
        // 18 digits × 10 above kMaxRep: an internal mantissa past int64.
        3 => (rng.range(K_MAX / 10 + 1, 999_999_999_999_999_999), 1),
        4 => (rng.range(1_000_000_000_000_000_000, 1_000_000_000_000_000_999), 0),
        5 => (rng.range(1, 999), 0),
        // A canonical IOU mantissa (16 digits).
        6 => (rng.mantissa(), 0),
        // Any digit count.
        7 => ((rng.next() >> (rng.next() % 60)).clamp(1, K_MAX), 0),
        // Trailing zeros: Enabled330's alignment strips them first.
        8 => (rng.range(1, 9_999_999) * 1_000_000_000_000, 0),
        _ => (rng.range(1_000_000_000_000_000_000, K_MAX), 0),
    };
    let e = match rng.next() % 50 {
        0 => rng.exponent(-32768, -32700),
        1 => rng.exponent(32700, 32767),
        2..=20 => rng.exponent(-3, 3),
        _ => rng.exponent(-45, 45),
    };
    let m = m as i64;
    (if rng.next() % 4 == 0 { -m } else { m }, e + lift)
}

/// `Number` at the 19-digit Large330 scale — the scale rippled switches every
/// transaction to once SingleAssetVault or LendingProtocol is enabled
/// (Rules.cpp:35-74) — against libxrpl's own, operation by operation: add,
/// sub, mul, div, root2 and `operator rep()` under all four rounding modes,
/// plus the IOU sum and difference (`IOUAmount::operator+=`: nineteen digits
/// rounded once to sixteen). Both sides see the external mantissa and
/// exponent; a throw on one side must be a throw on the other.
///
/// Deterministic like the Small test: ARITH_FUZZ_SEED, ARITH_FUZZ_N.
#[test]
fn number_agrees_with_libxrpl_at_large330() {
    use xrpl_ledger::tx::number::{mantissa_scale, MantissaScale, ScaleGuard};
    let seed = std::env::var("ARITH_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x9E37_79B9_7F4A_7C15u64);
    let n: u64 = std::env::var("ARITH_FUZZ_N").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    // libxrpl's scale is a thread-local, as ours is: both set for this test's thread.
    arith::set_scale(true);
    let _scale = ScaleGuard::new(MantissaScale::Large330);
    assert_eq!(mantissa_scale(), MantissaScale::Large330);
    let mut rng = Rng(seed ^ 0x330);
    let mut t: std::collections::BTreeMap<String, Tally> = Default::default();
    let mut both_threw = 0u64;
    const MODE: [&str; 4] = ["nearest", "towards-zero", "down", "up"];
    const OP: [&str; 5] = ["add", "sub", "mul", "div", "root2"];

    for _ in 0..n {
        let a = large_operand(&mut rng);
        let b = large_operand(&mut rng);
        // A second partner close to `a`: the alignment and the borrow loop
        // only work when the exponents are within a few digits.
        let near = {
            let c = large_operand(&mut rng);
            (c.0, a.1 + rng.exponent(-22, 22))
        };
        let drops = (large_operand(&mut rng).0, rng.exponent(-25, 2));
        for mode in 0u8..4 {
            for (op, name) in OP.iter().enumerate() {
                for (label, y) in [("", b), (" near", near)] {
                    // root2 is unary and throws below zero: one positive run.
                    let x = if op == 4 { (a.0.abs(), a.1) } else { a };
                    if op == 4 && !label.is_empty() {
                        continue;
                    }
                    let key = format!("L330 Number {name}{label} {}", MODE[mode as usize]);
                    let theirs = arith::number_op(op as i32, x, y, mode as i32);
                    let mine = ours::number_op_signed(op as u8, x, y, mode);
                    let ok = match (&theirs, &mine) {
                        (Ok((m, e)), Ok((om, oe))) => om == m && (*m == 0 || oe == e),
                        (Err(_), Err(_)) => {
                            both_threw += 1;
                            true
                        }
                        _ => false,
                    };
                    t.entry(key).or_default().note(ok, || format!("{}e{} {name} {}e{} mode {} -> libxrpl {:?} ours {:?}", x.0, x.1, y.0, y.1, MODE[mode as usize], theirs, mine));
                }
            }
            // `operator rep()`: XRP drops and MPT units out of a Number.
            let key = format!("L330 Number to_drops {}", MODE[mode as usize]);
            let theirs = arith::number_op(NUM_TO_DROPS, drops, (1, 0), mode as i32).map(|(d, _)| d);
            let mine = ours::number_to_drops(drops, mode);
            let ok = match (&theirs, &mine) {
                (Ok(d), Ok(od)) => d == od,
                (Err(_), Err(_)) => {
                    both_threw += 1;
                    true
                }
                _ => false,
            };
            t.entry(key).or_default().note(ok, || format!("{}e{} mode {} -> libxrpl {:?} ours {:?}", drops.0, drops.1, MODE[mode as usize], theirs, mine));
        }
        // --- STAmount add / sub (IOU) at Large330: 16-digit operands, the sum
        // held at nineteen digits and rounded once (IOUAmount.cpp:28-37, 69-84).
        let (am, ae) = (rng.mantissa(), rng.exponent(-30, 10));
        let (cm, ce) = (rng.mantissa(), ae + if rng.next() % 2 == 0 { rng.exponent(-3, 3) } else { rng.exponent(-20, 20) });
        for (shim_op, negate, name) in [(ARITH_ADD, false, "L330 STAmount add"), (ARITH_SUB, true, "L330 STAmount sub")] {
            match arith::stamount_op(shim_op, iou(am, ae), iou(cm, ce), false, false) {
                Ok(r) => {
                    let (neg, o) = ours::stamount_signed_add(false, (am as u128, ae), negate, (cm as u128, ce));
                    let ok = r.mantissa as u128 == o.0 && (r.mantissa == 0 || (r.exponent == o.1 && (r.negative != 0) == neg));
                    t.entry(name.to_string()).or_default().note(ok, || format!("{}e{} {} {}e{} -> libxrpl {} ours {}{}e{}", am, ae, if negate { "-" } else { "+" }, cm, ce, fmt_amt(&r), if neg { "-" } else { "" }, o.0, o.1));
                }
                Err(_) => t.entry(name.to_string()).or_default().oracle_errors += 1,
            }
        }
    }
    // Leave the thread as the Small test expects to find it.
    arith::set_scale(false);

    let mut total_mismatch = 0;
    let mut total_cases = 0;
    eprintln!("\n[arith_fuzz] Number scale: Large330 (19 digits), {n} iterations");
    eprintln!("{:<40} {:>8} {:>10} {:>8}", "op", "cases", "mismatch", "oracleErr");
    for (k, v) in &t {
        eprintln!("{:<40} {:>8} {:>10} {:>8}", k, v.cases, v.mismatches, v.oracle_errors);
        total_mismatch += v.mismatches;
        total_cases += v.cases;
    }
    eprintln!("L330 total: {total_cases} cases, {total_mismatch} mismatches, {both_threw} where both sides threw");
    for (k, v) in &t {
        for ex in &v.examples {
            eprintln!("  [{k}] {ex}");
        }
    }
    assert_eq!(total_mismatch, 0, "Number at Large330 disagrees with libxrpl — see the table above");
}
