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
    for w in seq.windows(2) {
        let hop = DirectHop { src: w[0], dst: w[1], cur: *cur };
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
fn hop_qualities(
    sandbox: &Sandbox,
    hops: &[DirectHop],
    i: usize,
    is_last: bool,
) -> (u128, u128) {
    let hop = &hops[i];
    if src_redeems(sandbox, hop) {
        // qualitiesSrcRedeems: no previous step ⇒ (1, 1); otherwise the
        // larger of the previous hop's lineQualityIn and our own QualityOut.
        if i == 0 {
            return (QUALITY_ONE, QUALITY_ONE);
        }
        let prev_qin = line_quality(sandbox, &hops[i - 1], true);
        let own_qout = line_quality(sandbox, hop, false);
        (prev_qin.max(own_qout), QUALITY_ONE)
    } else {
        // qualitiesSrcIssues: a strand-head issuer charges nothing (prev
        // defaults to Issues).
        let prev_redeems = i > 0 && src_redeems(sandbox, &hops[i - 1]);
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

/// One PASS over a pure-direct strand: reverse plan right-to-left
/// (DirectStep.cpp:503-568), forward application left-to-right (:617-700)
/// capped by the reverse plan (`setCacheLimiting`), mutations per hop.
/// Returns (spent at the head, delivered at the tail).
pub(crate) fn direct_strand_pass(
    sandbox: &mut Sandbox,
    hops: &[DirectHop],
    rem_in: ox::Me,
    rem_out: ox::Me,
) -> (ox::Me, ox::Me) {
    let n = hops.len();
    if n == 0 || ox::me_is_zero(rem_in) || ox::me_is_zero(rem_out) {
        return ((0, 0), (0, 0));
    }
    // ---- reverse: how much must each hop carry for the tail to emit
    // rem_out? All reads, no writes — rippled's rev-pass writes land on a
    // sandbox the fwd pass rebuilds anyway, and within one strand no two
    // hops share a line (the loop dedup guarantees it), so the plan is
    // identical.
    let mut rev_src_to_dst = vec![(0u128, 0i32); n];
    let mut need = rem_out;
    for i in (0..n).rev() {
        let (src_q_out, dst_q_in) = hop_qualities(sandbox, hops, i, i + 1 == n);
        let max = max_src_to_dst(sandbox, &hops[i]);
        if ox::me_is_zero(max) {
            return ((0, 0), (0, 0)); // dry — rippled: "DirectStepI::rev: dry"
        }
        let mut src_to_dst = mul_ratio(need, QUALITY_ONE, dst_q_in, true);
        if ox::me_cmp(src_to_dst, max).is_gt() {
            src_to_dst = max; // limiting node
        }
        rev_src_to_dst[i] = src_to_dst;
        need = mul_ratio(src_to_dst, src_q_out, QUALITY_ONE, true);
    }
    // ---- forward from min(reverse head requirement, what remains of the
    // SendMax): flow() runs the fwd pass whenever maxIn caps the reverse
    // answer (StrandFlow.h), and the fwd never exceeds the reverse plan.
    let mut carry = if ox::me_cmp(need, rem_in).is_gt() { rem_in } else { need };
    let spent = carry;
    for (i, hop) in hops.iter().enumerate() {
        let (src_q_out, dst_q_in) = hop_qualities(sandbox, hops, i, i + 1 == n);
        let mut src_to_dst = mul_ratio(carry, QUALITY_ONE, src_q_out, false);
        if ox::me_cmp(src_to_dst, rev_src_to_dst[i]).is_gt() {
            src_to_dst = rev_src_to_dst[i];
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
    }
    (spent, carry)
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
    for i in 0..n {
        let (src_q_out, dst_q_in) = hop_qualities(sandbox, hops, i, i + 1 == n);
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
