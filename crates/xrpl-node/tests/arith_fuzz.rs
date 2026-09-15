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
