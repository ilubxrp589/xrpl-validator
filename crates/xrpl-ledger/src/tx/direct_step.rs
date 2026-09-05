//! DirectStepI — rippling through accounts. Stage 1 of
//! `docs/DIRECTSTEP-DESIGN.md`: PURE-account strands (no book hops), the
//! `Payment tecPATH_DRY-v-tesSUCCESS` family's #106102038 and #106373989.
//!
//! Every rule here is a port of `libxrpl/tx/paths/DirectStep.cpp` (3.2.1)
//! or `PaySteps.cpp::toStrand`, and the rev/fwd rounding is pinned by the
//! FFI traces: #106373989's reverse pass asks 0.01672194676077877 (round
//! UP through the 1.003 transfer rate) and its forward pass, capped by the
//! SendMax one ulp below that, delivers 0.01667193096787513 (round DOWN).

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::tx::offer as ox;

pub(crate) const QUALITY_ONE: u128 = 1_000_000_000;

/// One DirectStepI: value moves across the MUTUAL line `line(src, dst,
/// cur)` — never via each party's issuer line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectHop {
    pub src: [u8; 20],
    pub dst: [u8; 20],
    pub cur: [u8; 20],
}

/// The account SEQUENCE of a pure-account path, implied elements included:
/// `src, [SendMax issuer], path accounts…, [deliver issuer], dst`
/// (toStrand's normPath, PaySteps.cpp:262-330). `None` when any element
/// carries a currency/issuer bit — that path has book hops and belongs to
/// the existing pipeline (stage 2 unifies them).
///
/// Shared by the ENGINE's strand construction and the PROBE's hydration so
/// the two can never drift — the institutionalized lesson from the
/// unhydrated-line regressions of 2026-08-18.
pub fn pure_account_sequence(
    src: &[u8; 20],
    dst: &[u8; 20],
    deliver_issuer: &[u8; 20],
    sendmax_issuer: &[u8; 20],
    els: &[serde_json::Value],
) -> Option<Vec<[u8; 20]>> {
    let mut seq: Vec<[u8; 20]> = vec![*src];
    // Implied SendMax-issuer element, "unless the path starts at an
    // address which is the issuer of SendMax" (PaySteps.cpp:283-289).
    let first_el_acct = els
        .first()
        .filter(|e| e.get("type").and_then(|v| v.as_u64()).unwrap_or(0) & 0x01 != 0)
        .and_then(|e| e.get("account"))
        .and_then(|v| v.as_str())
        .and_then(ox::decode20);
    if sendmax_issuer != src && first_el_acct.as_ref() != Some(sendmax_issuer) {
        seq.push(*sendmax_issuer);
    }
    for el in els {
        let t = el.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
        if t != 0x01 {
            return None; // currency/issuer bits ⇒ book hops ⇒ not this stage
        }
        let a = el.get("account").and_then(|v| v.as_str()).and_then(ox::decode20)?;
        seq.push(a);
    }
    // Implied deliver-issuer element unless already the tail or the
    // destination itself (PaySteps.cpp:315-321).
    if seq.last() != Some(deliver_issuer) && dst != deliver_issuer {
        seq.push(*deliver_issuer);
    }
    // The destination tail (PaySteps.cpp:324-329).
    if seq.last() != Some(dst) {
        seq.push(*dst);
    }
    (seq.len() >= 2).then_some(seq)
}

/// Signed holding of `who` on the mutual line with `other`: `(neg, mag)`,
/// `neg` = who OWES other. `None` when the line is not in the sandbox.
fn holding_toward(
    sandbox: &Sandbox,
    who: &[u8; 20],
    other: &[u8; 20],
    cur: &[u8; 20],
) -> Option<(bool, ox::Me)> {
    let line = ox::json_at(sandbox, &keylet::ripple_state_key(who, other, cur))?;
    let (neg, mag) = ox::signed_value(&line["Balance"]);
    // Balance is stored from the LOW account's perspective.
    let who_low = who < other;
    Some((if who_low { neg } else { !neg && mag.0 > 0 }, mag))
}

/// `party`'s limit toward the other side: the limit field on PARTY's side.
fn limit_of(sandbox: &Sandbox, party: &[u8; 20], other: &[u8; 20], cur: &[u8; 20]) -> ox::Me {
    let Some(line) = ox::json_at(sandbox, &keylet::ripple_state_key(party, other, cur)) else {
        return (0, 0);
    };
    let field = if party < other { "LowLimit" } else { "HighLimit" };
    keylet::amount_mant_exp(&line[field]).unwrap_or((0, 0))
}

/// Trust-line QualityIn/QualityOut for a hop (DirectStep.cpp:342-380):
/// In is read from DST's side of the mutual line, Out from SRC's; absent
/// or zero means QUALITY_ONE.
fn line_quality(sandbox: &Sandbox, hop: &DirectHop, q_in: bool) -> u128 {
    if hop.src == hop.dst {
        return QUALITY_ONE;
    }
    let Some(line) = ox::json_at(sandbox, &keylet::ripple_state_key(&hop.src, &hop.dst, &hop.cur))
    else {
        return QUALITY_ONE;
    };
    let field = if q_in {
        if hop.dst < hop.src { "LowQualityIn" } else { "HighQualityIn" }
    } else if hop.src < hop.dst {
        "LowQualityOut"
    } else {
        "HighQualityOut"
    };
    match line.get(field).and_then(|v| v.as_u64()) {
        Some(q) if q > 0 => q as u128,
        _ => QUALITY_ONE,
    }
}

/// The account's TransferRate, QUALITY_ONE when unset/unreadable.
fn transfer_rate_of(sandbox: &Sandbox, account: &[u8; 20]) -> u128 {
    ox::json_at(sandbox, &keylet::account_root_key(account))
        .and_then(|a| a.get("TransferRate").and_then(|v| v.as_u64()))
        .map(|r| r as u128)
        .filter(|r| *r > 0)
        .unwrap_or(QUALITY_ONE)
}

/// Does `src` REDEEM on this hop right now? (DirectStep.cpp:492-500 —
/// positive holding toward dst means redeeming its own debt back.)
fn src_redeems(sandbox: &Sandbox, hop: &DirectHop) -> bool {
    matches!(holding_toward(sandbox, &hop.src, &hop.dst, &hop.cur),
             Some((false, m)) if m.0 > 0)
}

/// Each hop's debt direction, read ONCE from the current (pre-flow) state.
/// rippled stamps `cache_->srcDebtDir` in the REV pass and `debtDirection`
/// RETURNS THE CACHE in the forward direction (DirectStep.cpp:492-499) —
/// so a fwd hop's fee decision sees the balances as they stood BEFORE the
/// pass moved anything. Re-probing live state instead loses the fee
/// exactly when a hop drains its line: #106455081 D2A4F725 spends its
/// whole 3.56084675020209e-6 BTC.rvYA holding at hop 0, the live re-probe
/// then reads hop 0 as Issues, and hop 1's 1.0015 redeem-charge vanishes
/// (delivered 193862 drops where mainnet nets 193574).
fn hop_dirs(sandbox: &Sandbox, hops: &[DirectHop]) -> Vec<bool> {
    hops.iter().map(|h| src_redeems(sandbox, h)).collect()
}

/// maxSrcToDst (DirectStep.cpp:476-490): what src can still push to dst.
/// Redeeming ⇒ the holding itself; issuing ⇒ dst's limit minus what dst
/// already holds.
fn max_src_to_dst(sandbox: &Sandbox, hop: &DirectHop) -> ox::Me {
    match holding_toward(sandbox, &hop.src, &hop.dst, &hop.cur) {
        Some((false, m)) if m.0 > 0 => m,
        held => {
            let limit = limit_of(sandbox, &hop.dst, &hop.src, &hop.cur);
            // srcOwed ≤ 0 ⇒ dst holds −srcOwed already.
            let dst_holds = match held {
                Some((true, m)) => m,
                _ => (0, 0),
            };
            if ox::me_cmp(dst_holds, limit) != std::cmp::Ordering::Less {
                (0, 0)
            } else {
                ox::me_sub(limit, dst_holds)
            }
        }
    }
}

/// Construction-time checks, per DirectIPaymentStep::check
/// (DirectStep.cpp:418-464). Any failure DROPS the strand — the same
/// path-drop plumbing that maps a flow with no strands to tecPATH_DRY.
/// A pure-account strand has no book steps, so the NoRipple-after-book
/// rule has nothing to test here (stage 2's concern).
/// Finding 157 — `checkFreeze` (StepChecks.h:19-46), run on every direct hop
/// that is not the strand's only step ("pure issue/redeem can't be frozen"):
/// the hop is dry when the DESTINATION account carries lsfGlobalFreeze — the
/// holder's own flag, whoever the issuer is — when the (src, dst) line
/// carries the destination's freeze bit, or when either side deep-froze it.
/// #106753769 DD2CD0BC4C81 (again at #106753771): rARKjtjX pays 0.005461 ASC
/// to r37rYnxT, whose root has lsfGlobalFreeze set; the issuer → destination
/// hop is terNO_LINE, the strand is dry and mainnet returns tecPATH_DRY. We
/// paid — the destination's line and the sender's moved on a transaction
/// that only charged a fee.
pub(crate) fn hop_frozen(sandbox: &Sandbox, hop: &DirectHop) -> bool {
    hop_frozen_parts(sandbox, &hop.src, &hop.dst, &hop.cur)
}

/// Finding 162 — `checkNoRipple` (DirectStep.cpp:859, the rule for a direct
/// step whose previous step is a direct step): rippling THROUGH `cur` is
/// refused when `cur`'s own NoRipple flag is set on BOTH of its lines — the
/// one from `prev` and the one to `next` — or when either line is missing:
///
///     if (!sleIn || !sleOut) return terNO_LINE;
///     if (sleIn->isFlag((cur > prev) ? lsfHighNoRipple : lsfLowNoRipple) &&
///         sleOut->isFlag((cur > next) ? lsfHighNoRipple : lsfLowNoRipple))
///         return terNO_RIPPLE;
///
/// The default path of a holder → holder IOU payment ripples through the
/// ISSUER, so an issuer that set NoRipple on its side of both holders' lines
/// cannot have its token moved between them — the strand fails to build and
/// the payment is tecPATH_DRY.
///
/// #106779252 3AD7EA863312 (and six more the same morning): rJEvC4cuk pays
/// 2,000,000 XRPFLORIDAGATORS to rpFYjv6SF; both lines exist, the sender
/// holds 99,999,999,999, and the issuer rMpjR1oh carries NoRipple on both.
/// Mainnet: tecPATH_DRY, fee only. We delivered and moved both lines.
pub(crate) fn check_no_ripple(
    sandbox: &Sandbox,
    prev: &[u8; 20],
    cur: &[u8; 20],
    next: &[u8; 20],
    currency: &[u8; 20],
) -> bool {
    const LSF_LOW_NO_RIPPLE: u64 = 0x0010_0000;
    const LSF_HIGH_NO_RIPPLE: u64 = 0x0020_0000;
    let flags = |a: &[u8; 20], b: &[u8; 20]| -> Option<u64> {
        ox::json_at(sandbox, &keylet::ripple_state_key(a, b, currency))
            .map(|l| l["Flags"].as_u64().unwrap_or(0))
    };
    let (Some(fin), Some(fout)) = (flags(prev, cur), flags(cur, next)) else {
        return true; // terNO_LINE
    };
    let bit_in = if cur > prev { LSF_HIGH_NO_RIPPLE } else { LSF_LOW_NO_RIPPLE };
    let bit_out = if cur > next { LSF_HIGH_NO_RIPPLE } else { LSF_LOW_NO_RIPPLE };
    fin & bit_in != 0 && fout & bit_out != 0
}

/// `hop_frozen` on a (src, dst, currency) triple — the simple holder → issuer
/// → holder payment in payment.rs has no `DirectHop`.
pub(crate) fn hop_frozen_parts(sandbox: &Sandbox, src: &[u8; 20], dst: &[u8; 20], cur: &[u8; 20]) -> bool {
    let hop = DirectHop { src: *src, dst: *dst, cur: *cur };
    let hop = &hop;
    const LSF_GLOBAL_FREEZE: u64 = 0x0040_0000; // AccountRoot
    const LSF_LOW_FREEZE: u64 = 0x0040_0000; // RippleState
    const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
    const LSF_LOW_DEEP_FREEZE: u64 = 0x0200_0000;
    const LSF_HIGH_DEEP_FREEZE: u64 = 0x0400_0000;
    if ox::json_at(sandbox, &keylet::account_root_key(&hop.dst))
        .is_some_and(|a| a["Flags"].as_u64().unwrap_or(0) & LSF_GLOBAL_FREEZE != 0)
    {
        return true;
    }
    if let Some(line) = ox::json_at(sandbox, &keylet::ripple_state_key(&hop.src, &hop.dst, &hop.cur)) {
        let f = line["Flags"].as_u64().unwrap_or(0);
        let dst_bit = if hop.dst > hop.src { LSF_HIGH_FREEZE } else { LSF_LOW_FREEZE };
        if f & dst_bit != 0 || f & (LSF_LOW_DEEP_FREEZE | LSF_HIGH_DEEP_FREEZE) != 0 {
            return true;
        }
    }
    false
}

pub(crate) fn build_direct_strand(
    sandbox: &Sandbox,
    seq: &[[u8; 20]],
    cur: &[u8; 20],
) -> Option<Vec<DirectHop>> {
    const LSF_REQUIRE_AUTH: u64 = 0x0004_0000; // AccountRoot
    const LOW_AUTH: u64 = 0x0004_0000; // RippleState
    const HIGH_AUTH: u64 = 0x0008_0000;
    // "A strand may not include the same account node more than once in
    // the same currency … at most twice: once as src and once as dst"
    // (PaySteps.cpp:333-341). Track per-role appearances.
    let mut seen_src: Vec<[u8; 20]> = Vec::new();
    let mut seen_dst: Vec<[u8; 20]> = Vec::new();
    let mut hops = Vec::with_capacity(seq.len() - 1);
    let mut prev_src: Option<[u8; 20]> = None;
    for w in seq.windows(2) {
        let hop = DirectHop { src: w[0], dst: w[1], cur: *cur };
        // Finding 162: a direct step after a direct step runs checkNoRipple
        // on the account it ripples through.
        if let Some(prev) = prev_src.replace(hop.src) {
            if check_no_ripple(sandbox, &prev, &hop.src, &hop.dst, cur) {
                if std::env::var("DX_PAY").is_ok() {
                    eprintln!("DX_PAY direct drop: NO RIPPLE through {}", hex::encode(&hop.src[..4]));
                }
                return None; // terNO_RIPPLE
            }
        }
        if seen_src.contains(&hop.src) || seen_dst.contains(&hop.dst) {
            return None; // temBAD_PATH_LOOP ⇒ path dropped
        }
        seen_src.push(hop.src);
        seen_dst.push(hop.dst);
        // "Since this is a payment a trust line must be present" ⇒
        // terNO_LINE. The probe hydrates every mutual line on the
        // sequence, so absence is mainnet truth, not a fixture gap.
        let lkey = keylet::ripple_state_key(&hop.src, &hop.dst, &hop.cur);
        let Some(line) = ox::json_at(sandbox, &lkey) else {
            if std::env::var("DX_PAY").is_ok() {
                eprintln!("DX_PAY direct drop: NO LINE {}~{}", hex::encode(&hop.src[..4]), hex::encode(&hop.dst[..4]));
            }
            return None;
        };
        // Finding 157: checkFreeze — skipped only when this hop is the whole
        // strand (`ctx.isFirst && ctx.isLast`).
        if seq.len() > 2 && hop_frozen(sandbox, &hop) {
            if std::env::var("DX_PAY").is_ok() {
                eprintln!("DX_PAY direct drop: FROZEN {}~{}", hex::encode(&hop.src[..4]), hex::encode(&hop.dst[..4]));
            }
            return None;
        }
        // Issuer-side auth: src requires auth, the line is unauthorized on
        // src's side, and the balance is zero ⇒ terNO_AUTH.
        let src_requires_auth = ox::json_at(sandbox, &keylet::account_root_key(&hop.src))
            .map(|a| a["Flags"].as_u64().unwrap_or(0) & LSF_REQUIRE_AUTH != 0)
            .unwrap_or(false);
        if src_requires_auth {
            let auth_bit = if hop.src > hop.dst { HIGH_AUTH } else { LOW_AUTH };
            let authed = line["Flags"].as_u64().unwrap_or(0) & auth_bit != 0;
            let zero = ox::signed_value(&line["Balance"]).1 .0 == 0;
            if !authed && zero {
                if std::env::var("DX_PAY").is_ok() {
                    eprintln!("DX_PAY direct drop: NO AUTH {}", hex::encode(&hop.src[..4]));
                }
                return None;
            }
        }
        // Dry precheck (:449-461). Operationally it is `maxPaymentFlow ==
        // 0`: a REDEEMING source is never dry (the first-cut port asked
        // whether DST holds anything and condemned every holder→gateway hop
        // — the trace's own hop 0, src redeeming 21911 CNY, was the
        // refutation); an ISSUING source is dry exactly when dst's holding
        // has already consumed dst's limit toward src.
        if !src_redeems(sandbox, &hop) && ox::me_is_zero(max_src_to_dst(sandbox, &hop)) {
            if std::env::var("DX_PAY").is_ok() {
                eprintln!(
                    "DX_PAY direct drop: DRY {}~{}",
                    hex::encode(&hop.src[..4]),
                    hex::encode(&hop.dst[..4])
                );
            }
            return None;
        }
        hops.push(hop);
    }
    Some(hops)
}

/// The qualities of hop `i` in a strand (DirectStep.cpp:735-790):
/// (srcQOut, dstQIn). The transfer rate is charged exactly when the source
/// ISSUES and the step before it REDEEMS — the traced 1.002 (rhub8) and
/// 1.003 (rcEG) charges.
///
/// `prev_book` — a BOOK segment precedes this run (mixed strand). A book's
/// debtDirection is Redeems whenever `ownerPaysTransferFee` is off
/// (BookStep.cpp:146), and OwnerPaysFee is a dormant amendment, so on
/// mainnet a book ALWAYS redeems into the run: hop 0's issuer charges its
/// transfer rate exactly as if an interior redeeming hop stood before it.
/// #106455079 B1017CAA is the specimen: XRP →(BTC/XRP pool)→ BTC.rvYA →
/// rNyMZc → rchGBx; sizing hop 0 at parity buys the pool slice 1.0015 too
/// small and under-spends the sender 19,262 drops. A book's lineQualityIn
/// is the base Step's QUALITY_ONE (Steps.h:153).
fn hop_qualities(
    sandbox: &Sandbox,
    hops: &[DirectHop],
    dirs: &[bool],
    i: usize,
    is_last: bool,
    prev_book: bool,
) -> (u128, u128) {
    let hop = &hops[i];
    if dirs[i] {
        // qualitiesSrcRedeems: no previous step ⇒ (1, 1); otherwise the
        // larger of the previous step's lineQualityIn and our own QualityOut.
        if i == 0 {
            if prev_book {
                let own_qout = line_quality(sandbox, hop, false);
                return (own_qout.max(QUALITY_ONE), QUALITY_ONE);
            }
            return (QUALITY_ONE, QUALITY_ONE);
        }
        let prev_qin = line_quality(sandbox, &hops[i - 1], true);
        let own_qout = line_quality(sandbox, hop, false);
        (prev_qin.max(own_qout), QUALITY_ONE)
    } else {
        // qualitiesSrcIssues: a strand-head issuer charges nothing (prev
        // defaults to Issues); a book before the run redeems (above).
        let prev_redeems = if i == 0 { prev_book } else { dirs[i - 1] };
        let src_q_out = if prev_redeems { transfer_rate_of(sandbox, &hop.src) } else { QUALITY_ONE };
        let mut dst_q_in = line_quality(sandbox, hop, true);
        if is_last && dst_q_in > QUALITY_ONE {
            dst_q_in = QUALITY_ONE;
        }
        (src_q_out, dst_q_in)
    }
}

/// Move `amt` from src to dst across their mutual line — rippled's
/// `directSendNoFee`. `line_adjust` with the COUNTERPARTY standing as the
/// leg's issuer writes the one shared object with the calibrated 16-digit
/// rounding and inert-write suppression.
fn direct_ripple_credit(sandbox: &mut Sandbox, hop: &DirectHop, amt: ox::Me) {
    let as_leg = ox::Leg { xrp: false, cur: hop.cur, issuer: hop.dst };
    ox::line_adjust(sandbox, &hop.src, &as_leg, amt, false);
}


/// rippled's `mulRatio(IOUAmount, num, den, roundUp)`
/// (libxrpl/protocol/IOUAmount.cpp:183-307), digit-exact. The shape that
/// matters — and that a plain exact-ceil misses by one ulp: the quotient is
/// carried at up to 19 digits, truncation residues are TRACKED, the
/// constructor normalizes to 16 digits HALF-EVEN (Number's to_nearest), and
/// only then does roundUp add one ulp — when ANY residue survived. The
/// traces pin it twice: 0.01667193096787513 × 1.003 rounds UP to
/// …877 (exact-ceil says …876), and 1.661184758819387 × 1.002 to …027.
fn mul_ratio(amt: ox::Me, num: u128, den: u128, round_up: bool) -> ox::Me {
    if amt.0 == 0 || num == 0 || den == 0 {
        return (0, 0);
    }
    // number of decimal digits
    fn digits(mut v: u128) -> i32 {
        let mut d = 0;
        while v > 0 {
            v /= 10;
            d += 1;
        }
        d
    }
    // kLoG10Ceil: index of the first power of ten ≥ v
    fn ceil_log10(v: u128) -> i32 {
        let d = digits(v);
        if v == 10u128.pow((d - 1) as u32) { d - 1 } else { d }
    }
    const KFL64: i32 = 18; // floor(log10(i64::MAX))
    let (m, mut e) = amt;
    let mul = m.saturating_mul(num); // ≤ 1e16 × ~2e9 — fits u128
    let mut low = mul / den;
    let mut rem = mul % den;
    if rem != 0 {
        let room = KFL64 - ceil_log10(low.max(1));
        if room > 0 {
            let p = 10u128.pow(room as u32);
            low *= p;
            rem *= p;
            e -= room;
        }
        let add = rem / den;
        low += add;
        rem -= add * den;
    }
    let mut has_rem = rem != 0;
    let shrink = ceil_log10(low.max(1)) - KFL64;
    if shrink > 0 {
        let p = 10u128.pow(shrink as u32);
        let sav = low;
        low /= p;
        e += shrink;
        has_rem |= sav != low * p;
    }
    // IOUAmount constructor: normalize to [1e15, 1e16) — HALF-EVEN on the
    // way down (Number::to_nearest), exact upscaling on the way up. The
    // has_rem verdict was fixed BEFORE this normalization, as in rippled.
    if low > 0 {
        let over = digits(low) - 16;
        if over > 0 {
            let p = 10u128.pow(over as u32);
            let (q, r) = (low / p, low % p);
            let half = p / 2;
            low = q + if r > half {
                1
            } else if r == half {
                q & 1
            } else {
                0
            };
            e += over;
            if digits(low) > 16 {
                low /= 10;
                e += 1;
            }
        }
        while digits(low) < 16 {
            low *= 10;
            e -= 1;
        }
    }
    if round_up && has_rem && low > 0 {
        low += 1;
        if digits(low) > 16 {
            low /= 10;
            e += 1;
        }
    }
    (low, e)
}

/// Reverse plan for a run: what must enter hop 0 for the tail to emit
/// `need_out`, and each hop's planned srcToDst (DirectStep.cpp:503-568).
/// `None` when any hop is dry. Reads only.
pub(crate) fn run_rev(
    sandbox: &Sandbox,
    hops: &[DirectHop],
    need_out: ox::Me,
    prev_book: bool,
) -> Option<(ox::Me, Vec<ox::Me>, Vec<bool>)> {
    let n = hops.len();
    if n == 0 || ox::me_is_zero(need_out) {
        return None;
    }
    // Debt directions stamped ONCE at rev time (see `hop_dirs`); the fwd
    // pass reuses them exactly as rippled's fwd `debtDirection` returns the
    // rev-stamped cache.
    let dirs = hop_dirs(sandbox, hops);
    let mut plan = vec![(0u128, 0i32); n];
    let mut need = need_out;
    for i in (0..n).rev() {
        let (src_q_out, dst_q_in) = hop_qualities(sandbox, hops, &dirs, i, i + 1 == n, prev_book);
        let max = max_src_to_dst(sandbox, &hops[i]);
        if ox::me_is_zero(max) {
            return None; // dry — rippled: "DirectStepI::rev: dry"
        }
        let mut src_to_dst = mul_ratio(need, QUALITY_ONE, dst_q_in, true);
        if ox::me_cmp(src_to_dst, max).is_gt() {
            src_to_dst = max; // limiting node
        }
        plan[i] = src_to_dst;
        need = mul_ratio(src_to_dst, src_q_out, QUALITY_ONE, true);
    }
    if std::env::var("DX_RUN").is_ok() {
        eprintln!("DX_RUN rev need_out={need_out:?} -> in={need:?} plan={plan:?} dirs={dirs:?} prev_book={prev_book}");
    }
    Some((need, plan, dirs))
}

/// Forward pass with writes: flow `in_amt` left-to-right, capped by the
/// reverse `plan` and the live maxes (DirectStep.cpp:617-700 +
/// setCacheLimiting). Returns (spent at the head, delivered at the tail).
pub(crate) fn run_fwd(
    sandbox: &mut Sandbox,
    hops: &[DirectHop],
    in_amt: ox::Me,
    plan: &[ox::Me],
    dirs: &[bool],
    prev_book: bool,
) -> (ox::Me, ox::Me) {
    let n = hops.len();
    if n == 0 || ox::me_is_zero(in_amt) {
        return ((0, 0), (0, 0));
    }
    let spent = in_amt;
    let mut carry = in_amt;
    for (i, hop) in hops.iter().enumerate() {
        let (src_q_out, dst_q_in) = hop_qualities(sandbox, hops, dirs, i, i + 1 == n, prev_book);
        let carry_pre = carry;
        let mut src_to_dst = mul_ratio(carry, QUALITY_ONE, src_q_out, false);
        if ox::me_cmp(src_to_dst, plan[i]).is_gt() {
            src_to_dst = plan[i];
        }
        let max = max_src_to_dst(sandbox, hop);
        if ox::me_cmp(src_to_dst, max).is_gt() {
            src_to_dst = max;
        }
        if ox::me_is_zero(src_to_dst) {
            return ((0, 0), (0, 0));
        }
        direct_ripple_credit(sandbox, hop, src_to_dst);
        carry = mul_ratio(src_to_dst, dst_q_in, QUALITY_ONE, false);
        if std::env::var("DX_RUN").is_ok() {
            eprintln!(
                "DX_RUN fwd hop={i} carry_in={carry_pre:?} sq={src_q_out} dq={dst_q_in} s2d={src_to_dst:?} plan_i={:?} out_carry={carry:?}",
                plan[i]
            );
        }
    }
    (spent, carry)
}

/// One PASS over a pure-direct strand: reverse plan, then forward from
/// min(reverse ask, remaining SendMax), mutations per hop.
pub(crate) fn direct_strand_pass(
    sandbox: &mut Sandbox,
    hops: &[DirectHop],
    rem_in: ox::Me,
    rem_out: ox::Me,
) -> (ox::Me, ox::Me) {
    if ox::me_is_zero(rem_in) || ox::me_is_zero(rem_out) {
        return ((0, 0), (0, 0));
    }
    let Some((need, plan, dirs)) = run_rev(sandbox, hops, rem_out, false) else {
        return ((0, 0), (0, 0));
    };
    let head = if ox::me_cmp(need, rem_in).is_gt() { rem_in } else { need };
    run_fwd(sandbox, hops, head, &plan, &dirs, false)
}

/// The strand's quality upper bound as an in-per-out rate (for the round
/// loop's best-first ordering): the product of every hop's srcQOut over
/// its dstQIn. All-default lines bound at exactly 1.
pub(crate) fn direct_upper_bound(sandbox: &Sandbox, hops: &[DirectHop]) -> Option<ox::Me> {
    let n = hops.len();
    if n == 0 {
        return None;
    }
    let one = (QUALITY_ONE, 0i32);
    let mut ub: ox::Me = (1, 0);
    let dirs = hop_dirs(sandbox, hops);
    for i in 0..n {
        let (src_q_out, dst_q_in) = hop_qualities(sandbox, hops, &dirs, i, i + 1 == n, false);
        ub = ox::me_muldiv(ub, (src_q_out, 0), (dst_q_in, 0), true);
    }
    let _ = one;
    Some(ub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn state() -> LedgerState {
        LedgerState::new_unverified(LedgerHeader {
            sequence: 100,
            total_coins: 0,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        })
    }

    fn add_account(st: &mut LedgerState, id: &[u8; 20], rate: Option<u64>) {
        let mut a = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
            "Balance": "1000000000", "Sequence": 1, "OwnerCount": 0, "Flags": 0,
        });
        if let Some(r) = rate {
            a["TransferRate"] = serde_json::json!(r);
        }
        st.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
    }

    /// A mutual line where `holder` HOLDS `(mant, exp)` toward `other`, and
    /// `other` grants `holder`… — limits: each side's limit is `limit`.
    /// A mutual line where `holder` HOLDS `held` toward `other`, with
    /// holder's limit `holder_limit` and other's limit `other_limit` — a
    /// real gateway extends NO limit back, which is exactly what the dry
    /// precheck's first, wrong port tripped over.
    fn add_line_limits(
        st: &mut LedgerState,
        holder: &[u8; 20],
        other: &[u8; 20],
        cur: &[u8; 20],
        held: (u128, i32),
        holder_limit: &str,
        other_limit: &str,
    ) {
        let holder_low = holder < other;
        let sign = if holder_low || held.0 == 0 { "" } else { "-" };
        let val = format!("{}{}e{}", sign, held.0, held.1);
        let (lo, hi) = if holder_low { (holder, other) } else { (other, holder) };
        let (lo_lim, hi_lim) =
            if holder_low { (holder_limit, other_limit) } else { (other_limit, holder_limit) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur),
                        "issuer": "0000000000000000000000000000000000000000", "value": val},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": lo_lim},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": hi_lim},
        });
        st.state_map
            .insert(keylet::ripple_state_key(holder, other, cur), serde_json::to_vec(&line).unwrap())
            .unwrap();
    }

    fn cur20(name: &[u8]) -> [u8; 20] {
        let mut c = [0u8; 20];
        c[12..12 + name.len()].copy_from_slice(name);
        c
    }

    /// #106102038 5B97B89E: CNY through one gateway. Two hops — the sender
    /// REDEEMS its 21911.42902424973 CNY holding at rKiCet, the gateway
    /// ISSUES to the destination under a wide limit. 10050 in, 10050 out,
    /// no rate (a redeem's head and an issue whose predecessor redeems at
    /// the SAME account charge nothing — the gateway's rate is unset).
    #[test]
    fn cny_two_hop_ripple_matches_the_trace() {
        let (s, g, d) = ([0x01u8; 20], [0x02u8; 20], [0x03u8; 20]);
        let cny = cur20(b"CNY");
        let mut st = state();
        for id in [&s, &g, &d] {
            add_account(&mut st, id, None);
        }
        add_line_limits(&mut st, &s, &g, &cny, (2191142902424973, -11), "1000000000", "0");
        add_line_limits(&mut st, &d, &g, &cny, (7225499, -3), "1000000000", "0");

        let seq = pure_account_sequence(&s, &d, &d, &s, &[serde_json::json!({"account": hex::encode(g), "type": 1})])
            .expect("sequence");
        assert_eq!(seq, vec![s, g, d], "implied issuer elements collapse onto the endpoints");
        let hops = build_direct_strand(&Sandbox::new(&st), &seq, &cny).expect("checks pass");
        assert_eq!(hops.len(), 2);

        let mut sb = Sandbox::new(&st);
        let (sin, sout) =
            direct_strand_pass(&mut sb, &hops, (1_0150_5, -1), (10050, 0));
        assert_eq!(ox::me_cmp(sin, (10050, 0)), std::cmp::Ordering::Equal);
        assert_eq!(ox::me_cmp(sout, (10050, 0)), std::cmp::Ordering::Equal);
        // The mutations land on the two MUTUAL lines: the sender's holding
        // fell, the destination's rose — by exactly the flow.
        let held_s = holding_toward(&sb, &s, &g, &cny).unwrap();
        assert!(!held_s.0);
        assert_eq!(ox::me_cmp(held_s.1, (1186142902424973, -11)), std::cmp::Ordering::Equal);
        let held_d = holding_toward(&sb, &d, &g, &cny).unwrap();
        assert_eq!(ox::me_cmp(held_d.1, (17275499, -3)), std::cmp::Ordering::Equal);
    }

    /// #106373989 8CAD0435: four hops, and the deliver issuer's 1.003
    /// TransferRate is charged where it ISSUES after a redeeming hop. The
    /// traced digits, both directions: the reverse plan asks
    /// 0.01672194676077877 (round UP), the forward pass capped by the
    /// SendMax one ulp lower delivers 0.01667193096787513 (round DOWN).
    #[test]
    fn four_hop_rate_charge_matches_the_trace() {
        let (src, g1, m, e) = ([0x01u8; 20], [0x02u8; 20], [0x03u8; 20], [0x04u8; 20]);
        let usdc = cur20(b"USD");
        let mut st = state();
        add_account(&mut st, &src, None);
        add_account(&mut st, &g1, None);
        add_account(&mut st, &m, None);
        add_account(&mut st, &e, Some(1_003_000_000));
        // src holds 5.534315704294562 toward g1 (redeems at hop 0).
        add_line_limits(&mut st, &src, &g1, &usdc, (5534315704294562, -15), "1000000", "0");
        // g1 issues to m: m holds 0.0011534, wide limit.
        add_line_limits(&mut st, &m, &g1, &usdc, (11534, -7), "999999999999", "0");
        // m holds 1.162506813527889 toward e (redeems at hop 2).
        add_line_limits(&mut st, &m, &e, &usdc, (1162506813527889, -15), "1000000", "0");
        // e issues to src on the last hop: huge limit.
        add_line_limits(&mut st, &src, &e, &usdc, (0, 0), "99999999999999999", "0");

        let els: Vec<serde_json::Value> = [g1, m, e]
            .iter()
            .map(|a| serde_json::json!({"account": hex::encode(a), "type": 1}))
            .collect();
        // Circular: dst == src; deliver issuer e is already the path tail.
        let seq = pure_account_sequence(&src, &src, &e, &src, &els).expect("sequence");
        assert_eq!(seq, vec![src, g1, m, e, src]);
        let hops = build_direct_strand(&Sandbox::new(&st), &seq, &usdc).expect("checks pass");
        assert_eq!(hops.len(), 4);

        // Reverse alone (no SendMax cap): the UP-rounded input.
        let mut sb = Sandbox::new(&st);
        let (sin, sout) =
            direct_strand_pass(&mut sb, &hops, (1, 10), (1667193096787513, -17));
        assert_eq!(sin, (1672194676077877, -17), "reverse rounds UP through 1.003");
        assert_eq!(sout, (1667193096787513, -17));

        // Forward capped by the real SendMax, one ulp below the reverse ask.
        let mut sb2 = Sandbox::new(&st);
        let (sin2, sout2) =
            direct_strand_pass(&mut sb2, &hops, (1672194676077876, -17), (1667193096787513, -17));
        assert_eq!(sin2, (1672194676077876, -17));
        assert_eq!(sout2, (1667193096787513, -17), "forward rounds DOWN through 1.003");
    }
}

// ---------------------------------------------------------------------------
// Stage 2 — mixed strands: direct runs composed with book hops
// ---------------------------------------------------------------------------

/// One segment of a mixed strand.
#[derive(Clone, Debug, PartialEq)]
pub enum SegLayout {
    Run(Vec<DirectHop>),
    Book { from: ox::Leg, to: ox::Leg },
}

/// toStrand's normalization for a MIXED path — account elements become
/// DirectHop runs, currency/issuer elements become book hops, `curAsset`
/// re-anchoring exactly as PaySteps.cpp:380-505 does it. Check-free and
/// sandbox-free: the PROBE calls this to learn what to hydrate, the engine
/// wraps it with the construction checks — one normalizer, no drift.
///
/// The metas pinned the re-anchor question: #106311829 D2EB36BA's consumed
/// offer TakerPays USD.rvYAfWj — the book AFTER an account run is keyed by
/// the run's LAST account, and those accounts are real gateways.
///
/// Tail rule: a strand whose value ends in a RUN materializes the terminal
/// deliver-issuer→destination hops (stage 1's shape, fees inside); one that
/// ends in a BOOK stops there — the round loop's want_rate model owns that
/// delivery, as it always has for book chains.
pub fn mixed_layout(
    src: &[u8; 20],
    dst: &[u8; 20],
    spend_leg: &ox::Leg,
    want_leg: &ox::Leg,
    els: &[serde_json::Value],
) -> Option<Vec<SegLayout>> {
    let mut segs: Vec<SegLayout> = Vec::new();
    let mut run: Vec<DirectHop> = Vec::new();
    let mut cur = spend_leg.clone();
    if !cur.xrp {
        cur.issuer = *src;
    }
    // pos = Some(account) when the value sits at an account, None when it
    // is a book's output in flight.
    let mut pos: Option<[u8; 20]> = Some(*src);

    let first_el_acct = els
        .first()
        .filter(|e| e.get("type").and_then(|v| v.as_u64()).unwrap_or(0) == 0x01)
        .and_then(|e| e.get("account"))
        .and_then(|v| v.as_str())
        .and_then(ox::decode20);
    if !spend_leg.xrp
        && spend_leg.issuer != *src
        && first_el_acct.as_ref() != Some(&spend_leg.issuer)
    {
        run.push(DirectHop { src: *src, dst: spend_leg.issuer, cur: cur.cur });
        cur.issuer = spend_leg.issuer;
        pos = Some(spend_leg.issuer);
    }

    // Only a path whose OWN elements ripple through an account belongs to
    // the mixed pipeline — implied head/tail hops (SendMax issuer, deliver
    // issuer, destination) exist on EVERY strand and are what the classic
    // model's spend_rate/want_rate bookkeeping already represents. Without
    // this gate every IOU payment built a duplicate competing strand next
    // to its classic chain: the dstep2 gate's 180 census hits, #105709221's
    // false tecPATH_PARTIAL and #105795329's three extra offers were all
    // one path flowing TWICE.
    let mut real_run = false;
    for el in els {
        let t = el.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
        match t {
            0x01 => {
                let a = el.get("account").and_then(|v| v.as_str()).and_then(ox::decode20)?;
                if cur.xrp {
                    return None; // XRP cannot ripple through accounts
                }
                match pos {
                    Some(p) => {
                        if p != a {
                            run.push(DirectHop { src: p, dst: a, cur: cur.cur });
                            // Finding 140: the head hop src → SendMax issuer is
                            // implied on EVERY strand (toStrand's normalization);
                            // an element that merely names it does not make this
                            // a mixed strand — the classic chain already is it.
                            if !(p == *src && a == spend_leg.issuer) {
                                real_run = true;
                            }
                        }
                    }
                    None => {
                        // offer→account: the implied issuer→account hop,
                        // unless the account IS the current issuer (a pure
                        // re-anchor, PaySteps.cpp:458-486).
                        if cur.issuer != a {
                            run.push(DirectHop { src: cur.issuer, dst: a, cur: cur.cur });
                            real_run = true;
                        }
                    }
                }
                cur.issuer = a;
                pos = Some(a);
            }
            0x10 | 0x20 | 0x30 => {
                let has_cur = t & 0x10 != 0;
                let has_iss = t & 0x20 != 0;
                let to = if has_cur {
                    let c = el.get("currency").and_then(|v| v.as_str())?;
                    if c == "XRP" {
                        ox::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] }
                    } else {
                        let mut c20 = [0u8; 20];
                        if c.len() == 40 {
                            c20.copy_from_slice(&hex::decode(c).ok()?);
                        } else if c.len() == 3 {
                            c20[12..15].copy_from_slice(c.as_bytes());
                        } else {
                            return None;
                        }
                        let iss = if has_iss {
                            el.get("issuer").and_then(|v| v.as_str()).and_then(ox::decode20)?
                        } else {
                            cur.issuer
                        };
                        ox::Leg { xrp: false, cur: c20, issuer: iss }
                    }
                } else {
                    // issuer-only element: same currency, new issuer.
                    let iss = el.get("issuer").and_then(|v| v.as_str()).and_then(ox::decode20)?;
                    ox::Leg { xrp: cur.xrp, cur: cur.cur, issuer: iss }
                };
                if !run.is_empty() {
                    segs.push(SegLayout::Run(std::mem::take(&mut run)));
                }
                segs.push(SegLayout::Book { from: cur.clone(), to: to.clone() });
                cur = to;
                pos = None;
            }
            _ => return None, // MPT and malformed types
        }
    }

    // Terminal book when the currency still differs from the delivery
    // (PaySteps.cpp:291-302 — payments compare CURRENCY only).
    if cur.xrp != want_leg.xrp || cur.cur != want_leg.cur {
        if !run.is_empty() {
            segs.push(SegLayout::Run(std::mem::take(&mut run)));
        }
        segs.push(SegLayout::Book { from: cur.clone(), to: want_leg.clone() });
        cur = want_leg.clone();
        pos = None;
    }

    // Run-tail: carry the value to the deliver issuer and the destination,
    // fees inside (stage 1's shape). Book-tail: stop — the round loop's
    // want_rate model delivers.
    if let Some(mut p) = pos {
        if cur.xrp {
            return None;
        }
        if p != want_leg.issuer && *dst != want_leg.issuer {
            run.push(DirectHop { src: p, dst: want_leg.issuer, cur: cur.cur });
            p = want_leg.issuer;
        }
        if p != *dst {
            run.push(DirectHop { src: p, dst: *dst, cur: cur.cur });
        }
    }
    if !run.is_empty() {
        segs.push(SegLayout::Run(run));
    }

    // A MIXED strand has at least one of each; pure shapes belong to the
    // existing pipelines (all-book → leg chains, all-run → stage 1).
    let books = segs.iter().filter(|s| matches!(s, SegLayout::Book { .. })).count();
    (books > 0 && real_run).then_some(segs)
}

/// Engine-side validation of a mixed layout: every run hop passes the
/// DirectIPaymentStep::check battery, including the NoRipple-after-book
/// rule for the first hop of a post-book run (DirectStep.cpp:440-445), and
/// the strand-wide loop dedup. `None` drops the strand — the same
/// path-drop plumbing as everywhere else.
pub(crate) fn check_mixed_strand(sandbox: &Sandbox, segs: &[SegLayout]) -> bool {
    const LSF_REQUIRE_AUTH: u64 = 0x0004_0000;
    const LOW_AUTH: u64 = 0x0004_0000;
    const HIGH_AUTH: u64 = 0x0008_0000;
    let mut seen_src: Vec<[u8; 20]> = Vec::new();
    let mut seen_dst: Vec<[u8; 20]> = Vec::new();
    let mut prev_was_book = false;
    for seg in segs {
        match seg {
            SegLayout::Book { .. } => prev_was_book = true,
            SegLayout::Run(hops) => {
                for (i, hop) in hops.iter().enumerate() {
                    if seen_src.contains(&hop.src) || seen_dst.contains(&hop.dst) {
                        return false; // temBAD_PATH_LOOP
                    }
                    seen_src.push(hop.src);
                    seen_dst.push(hop.dst);
                    let lkey = keylet::ripple_state_key(&hop.src, &hop.dst, &hop.cur);
                    let Some(line) = ox::json_at(sandbox, &lkey) else {
                        if std::env::var("DX_PAY").is_ok() {
                            eprintln!(
                                "DX_PAY mixed drop: NO LINE {}~{}",
                                hex::encode(&hop.src[..4]),
                                hex::encode(&hop.dst[..4])
                            );
                        }
                        return false; // terNO_LINE
                    };
                    // Finding 157: checkFreeze — a mixed strand always has more
                    // than one step, so every hop is subject to it.
                    if hop_frozen(sandbox, hop) {
                        if std::env::var("DX_PAY").is_ok() {
                            eprintln!(
                                "DX_PAY mixed drop: FROZEN {}~{}",
                                hex::encode(&hop.src[..4]),
                                hex::encode(&hop.dst[..4])
                            );
                        }
                        return false; // terNO_LINE
                    }
                    // Finding 162: a direct step after a direct step runs
                    // checkNoRipple on the account it ripples through.
                    if i > 0 && check_no_ripple(sandbox, &hops[i - 1].src, &hop.src, &hop.dst, &hop.cur) {
                        if std::env::var("DX_PAY").is_ok() {
                            eprintln!("DX_PAY mixed drop: NO RIPPLE through {}", hex::encode(&hop.src[..4]));
                        }
                        return false; // terNO_RIPPLE
                    }
                    // The step OUT of a book refuses when the source side of
                    // its line carries NoRipple.
                    if i == 0 && prev_was_book {
                        let bit = if hop.src > hop.dst { 0x0020_0000 } else { 0x0010_0000 };
                        if line["Flags"].as_u64().unwrap_or(0) & bit != 0 {
                            if std::env::var("DX_PAY").is_ok() {
                                eprintln!(
                                    "DX_PAY mixed drop: NO RIPPLE after book {}",
                                    hex::encode(&hop.src[..4])
                                );
                            }
                            return false; // terNO_RIPPLE
                        }
                    }
                    let src_requires_auth =
                        ox::json_at(sandbox, &keylet::account_root_key(&hop.src))
                            .map(|a| a["Flags"].as_u64().unwrap_or(0) & LSF_REQUIRE_AUTH != 0)
                            .unwrap_or(false);
                    if src_requires_auth {
                        let auth_bit = if hop.src > hop.dst { HIGH_AUTH } else { LOW_AUTH };
                        let authed = line["Flags"].as_u64().unwrap_or(0) & auth_bit != 0;
                        let zero = ox::signed_value(&line["Balance"]).1 .0 == 0;
                        if !authed && zero {
                            return false; // terNO_AUTH
                        }
                    }
                    if !src_redeems(sandbox, hop) && ox::me_is_zero(max_src_to_dst(sandbox, hop)) {
                        if std::env::var("DX_PAY").is_ok() {
                            eprintln!(
                                "DX_PAY mixed drop: DRY {}~{}",
                                hex::encode(&hop.src[..4]),
                                hex::encode(&hop.dst[..4])
                            );
                        }
                        return false; // the maxPaymentFlow == 0 precheck
                    }
                }
                prev_was_book = false;
            }
        }
    }
    true
}

/// The quality bound contribution of one run (in-per-out, ≥ 1 with default
/// lines): the product of every hop's srcQOut over dstQIn.
pub(crate) fn run_upper_bound(sandbox: &Sandbox, hops: &[DirectHop], prev_book: bool) -> ox::Me {
    let n = hops.len();
    let mut ub: ox::Me = (1_000_000_000_000_000, -15);
    let dirs = hop_dirs(sandbox, hops);
    for i in 0..n {
        let (src_q_out, dst_q_in) = hop_qualities(sandbox, hops, &dirs, i, i + 1 == n, prev_book);
        ub = ox::me_muldiv(ub, (src_q_out, 0), (dst_q_in, 0), true);
    }
    ub
}

/// Base58check r-address for a 20-byte account id — the probe's RPC params
/// (`ledger_entry amm`, `book_offers`) take addresses, and mixed-strand
/// book legs carry re-anchored issuers that exist nowhere in the tx JSON
/// as strings.
pub fn encode_address(id: &[u8; 20]) -> String {
    use sha2::{Digest, Sha256};
    const ALPHABET: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let mut payload = Vec::with_capacity(25);
    payload.push(0u8); // account-id type prefix
    payload.extend_from_slice(id);
    let check = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&check[..4]);
    // big-number base58
    let mut digits: Vec<u8> = vec![0];
    for byte in &payload {
        let mut carry = *byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    for byte in &payload {
        if *byte == 0 {
            digits.push(0);
        } else {
            break;
        }
    }
    digits.iter().rev().map(|d| ALPHABET[*d as usize] as char).collect()
}

#[cfg(test)]
mod addr_tests {
    #[test]
    fn encode_matches_known_address() {
        // rvYAfWj5gh67oV6fW32ZzP3Aw4Eubs59B (Bitstamp) — a fixed vector.
        let id = hex::decode("0A20B3C85F482532A9578DBB3950B85CA06594D1").unwrap();
        let id: [u8; 20] = id.as_slice().try_into().unwrap();
        assert_eq!(super::encode_address(&id), "rvYAfWj5gh67oV6fW32ZzP3Aw4Eubs59B");
    }
}
