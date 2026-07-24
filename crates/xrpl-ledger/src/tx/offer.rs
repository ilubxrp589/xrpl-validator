//! OfferCreate and OfferCancel transaction types.
//!
//! OfferCreate: places a new order on the DEX, crossing against existing offers.
//! OfferCancel: removes an existing order.
//!
//! Offer crossing walks the order book and matches compatible offers.
//! When offers cross: balances adjust, consumed offers are deleted,
//! partial fills modify the remaining amount.
//!
//! # DEAD CODE WARNING
//!
//! This module is **not called** by the live validator. Production transaction
//! application is delegated to rippled's C++ engine via FFI — see
//! `crates/xrpl-ffi/src/lib.rs` and `crates/xrpl-node/src/ffi_engine.rs`.
//!
//! This code is retained as a reference implementation / learning artifact.
//! Tests in this module prove the code works in isolation; they do NOT prove
//! the validator is correct.
//!
//! If you are adding a new amendment or tx type: add it to the FFI path,
//! not here. See `ffi/ARCHITECTURE.md` for the architectural decision record.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use xrpl_core::types::Hash256;

/// Parse an Amount field — returns (drops, is_xrp).
/// XRP amounts are strings of drops. IOU amounts are objects with currency/issuer/value.
fn parse_amount(val: &serde_json::Value) -> Option<Amount> {
    match val {
        serde_json::Value::String(s) => {
            let drops: u64 = s.parse().ok()?;
            Some(Amount::Xrp(drops))
        }
        serde_json::Value::Number(n) => {
            Some(Amount::Xrp(n.as_u64()?))
        }
        serde_json::Value::Object(obj) => {
            let currency = obj.get("currency")?.as_str()?.to_string();
            let issuer = obj.get("issuer")?.as_str()?.to_string();
            let value: f64 = obj.get("value")?.as_str()?.parse().ok()?;
            if !value.is_finite() || value < 0.0 {
                return None;
            }
            Some(Amount::Iou { currency, issuer, value })
        }
        _ => None,
    }
}

/// 20-byte currency code of an amount (XRP = zeros; 3-char ASCII at bytes
/// 12..15; 40-hex verbatim).
pub(crate) fn amount_currency20(v: &serde_json::Value) -> Option<[u8; 20]> {
    match v {
        serde_json::Value::String(_) => Some([0u8; 20]), // XRP
        serde_json::Value::Object(o) => {
            let c = o.get("currency")?.as_str()?;
            if c == "XRP" {
                return Some([0u8; 20]);
            }
            if c.len() == 40 {
                let b = hex::decode(c).ok()?;
                return <[u8; 20]>::try_from(b.as_slice()).ok();
            }
            let cb = c.as_bytes();
            if cb.is_empty() || cb.len() > 8 {
                return None;
            }
            let mut b = [0u8; 20];
            b[12..12 + cb.len()].copy_from_slice(cb);
            Some(b)
        }
        _ => None,
    }
}

/// 20-byte issuer of an amount (XRP = account-zero; hex or base58 r-address).
pub(crate) fn amount_issuer20(v: &serde_json::Value) -> Option<[u8; 20]> {
    match v {
        serde_json::Value::String(_) => Some([0u8; 20]),
        serde_json::Value::Object(o) => {
            let s = o.get("issuer")?.as_str()?;
            if let Ok(b) = hex::decode(s) {
                if b.len() == 20 {
                    return <[u8; 20]>::try_from(b.as_slice()).ok();
                }
            }
            xrpl_core::types::AccountId::from_address(s).ok().map(|a| a.0)
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
enum Amount {
    Xrp(u64),
    Iou { currency: String, issuer: String, value: f64 },
}

impl Amount {
    fn is_xrp(&self) -> bool {
        matches!(self, Amount::Xrp(_))
    }

    /// Compute exchange rate as f64 (self per 1 unit of other).
    fn rate_against(&self, other: &Amount) -> f64 {
        let self_val = match self {
            Amount::Xrp(d) => *d as f64,
            Amount::Iou { value, .. } => *value,
        };
        let other_val = match other {
            Amount::Xrp(d) => *d as f64,
            Amount::Iou { value, .. } => *value,
        };
        if other_val == 0.0 { return f64::MAX; }
        self_val / other_val
    }
}

// ===========================================================================
// Integer crossing engine — (mantissa u128, exponent i32) arithmetic, faithful
// keylet-quality math (see keylet::offer_quality), directed rounding at fill
// boundaries (taker pays UP, receives DOWN — rippled Quality::ceil_in/out).
// The differential gate compares (key, kind) sets; exact fill values decide
// full-vs-partial maker consumption, so the integer math matters even though
// stored byte values are not compared.
// ===========================================================================

/// Mainnet owner reserve (drops): base 1 XRP + 0.2 XRP per owned object.
const XRP_RESERVE_BASE: u128 = 1_000_000;
const XRP_RESERVE_INC: u128 = 200_000;

pub(crate) type Me = (u128, i32);

pub(crate) struct Leg {
    pub(crate) xrp: bool,
    pub(crate) cur: [u8; 20],
    pub(crate) issuer: [u8; 20],
}

pub(crate) fn leg_of(v: &serde_json::Value) -> Option<Leg> {
    Some(Leg { xrp: v.is_string(), cur: amount_currency20(v)?, issuer: amount_issuer20(v)? })
}

pub(crate) fn decode20(s: &str) -> Option<[u8; 20]> {
    if let Ok(b) = hex::decode(s) {
        if b.len() == 20 {
            return <[u8; 20]>::try_from(b.as_slice()).ok();
        }
    }
    xrpl_core::types::AccountId::from_address(s).ok().map(|a| a.0)
}

pub(crate) fn me_rescale(a: Me, e: i32, ceil: bool) -> u128 {
    if a.1 >= e {
        let d = (a.1 - e).min(38) as u32;
        a.0.saturating_mul(10u128.saturating_pow(d))
    } else {
        let d = 10u128.saturating_pow(((e - a.1).min(38)) as u32);
        if ceil { a.0.div_ceil(d) } else { a.0 / d }
    }
}

pub(crate) fn me_cmp(a: Me, b: Me) -> std::cmp::Ordering {
    let e = a.1.min(b.1);
    me_rescale(a, e, false).cmp(&me_rescale(b, e, false))
}

pub(crate) fn me_is_zero(a: Me) -> bool {
    a.0 == 0
}

pub(crate) fn me_sub(a: Me, b: Me) -> Me {
    let e = a.1.min(b.1);
    (me_rescale(a, e, false).saturating_sub(me_rescale(b, e, false)), e)
}

pub(crate) fn me_norm(mut a: Me) -> Me {
    while a.0 >= 100_000_000_000_000_000_000 {
        a.0 /= 10;
        a.1 += 1;
    }
    a
}

/// a*b/c with directed rounding (mantissas kept ≤ ~1e20 so the product fits).
pub(crate) fn me_muldiv(a: Me, b: Me, c: Me, ceil: bool) -> Me {
    if c.0 == 0 {
        return (0, 0);
    }
    let (a, b, c) = (me_norm(a), me_norm(b), me_norm(c));
    let mut num = a.0.saturating_mul(b.0);
    let mut e = a.1 + b.1 - c.1;
    // Keep ~16 significant digits in the quotient: mixed-scale operands
    // (XRP drops against 1e-11-exponent IOU mantissas) otherwise truncate
    // to zero (bridge slice sizing found this the hard way).
    while num != 0
        && num < c.0.saturating_mul(1_000_000_000_000_000)
        && num < u128::MAX / 10
    {
        num = num.saturating_mul(10);
        e -= 1;
    }
    let m = if ceil { num.div_ceil(c.0) } else { num / c.0 };
    me_norm((m, e))
}

pub(crate) fn me_to_value_string(a: Me) -> String {
    if a.0 == 0 {
        return "0".into();
    }
    let (mut m, mut e) = a;
    while m % 10 == 0 && e < 0 {
        m /= 10;
        e += 1;
    }
    if e >= 0 {
        format!("{}{}", m, "0".repeat(e.min(40) as usize))
    } else {
        let s = m.to_string();
        let k = (-e) as usize;
        if s.len() > k {
            format!("{}.{}", &s[..s.len() - k], &s[s.len() - k..])
        } else {
            format!("0.{}{}", "0".repeat(k - s.len()), s)
        }
    }
}

/// Remainder amount in the same JSON shape as the original tx field.
pub(crate) fn me_amount_json(orig: &serde_json::Value, a: Me) -> serde_json::Value {
    match orig {
        serde_json::Value::String(_) => {
            serde_json::Value::String(me_rescale(a, 0, false).to_string())
        }
        serde_json::Value::Object(o) => serde_json::json!({
            "currency": o.get("currency").cloned().unwrap_or_default(),
            "issuer": o.get("issuer").cloned().unwrap_or_default(),
            "value": me_to_value_string(a),
        }),
        _ => serde_json::Value::Null,
    }
}

pub(crate) fn json_at(sandbox: &Sandbox, key: &xrpl_core::types::Hash256) -> Option<serde_json::Value> {
    sandbox.read(key).and_then(|d| serde_json::from_slice(&d).ok())
}

pub(crate) fn put_json(sandbox: &mut Sandbox, key: xrpl_core::types::Hash256, v: &serde_json::Value) {
    sandbox.write(key, serde_json::to_vec(v).unwrap_or_default());
}

pub(crate) fn dirnum(v: &serde_json::Value) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(s) = v.as_str() {
        return u64::from_str_radix(s, 16).ok().or_else(|| s.parse().ok()).unwrap_or(0);
    }
    0
}

/// Signed (negative, magnitude) of an amount value string.
pub(crate) fn signed_value(v: &serde_json::Value) -> (bool, Me) {
    let s = match v {
        serde_json::Value::Object(o) => o.get("value").and_then(|x| x.as_str()).unwrap_or("0"),
        serde_json::Value::String(s) => s.as_str(),
        _ => "0",
    };
    let neg = s.starts_with('-');
    let me = keylet::amount_mant_exp(&serde_json::Value::String(s.trim_start_matches('-').to_string()))
        .unwrap_or((0, 0));
    (neg && me.0 > 0, me)
}

pub(crate) fn signed_add(aneg: bool, a: Me, bneg: bool, b: Me) -> (bool, Me) {
    if aneg == bneg {
        let e = a.1.min(b.1);
        return (aneg, (me_rescale(a, e, false) + me_rescale(b, e, false), e));
    }
    match me_cmp(a, b) {
        std::cmp::Ordering::Equal => (false, (0, 0)),
        std::cmp::Ordering::Greater => (aneg, me_sub(a, b)),
        std::cmp::Ordering::Less => (bneg, me_sub(b, a)),
    }
}

pub(crate) fn owner_count_add(sandbox: &mut Sandbox, id: &[u8; 20], delta: i64) {
    let key = keylet::account_root_key(id);
    if let Some(mut a) = json_at(sandbox, &key) {
        let c = a["OwnerCount"].as_u64().unwrap_or(0) as i64;
        a["OwnerCount"] = serde_json::Value::Number(((c + delta).max(0) as u64).into());
        put_json(sandbox, key, &a);
    }
}

/// How much of `leg` the account can actually deliver.
pub(crate) fn available(sandbox: &Sandbox, id: &[u8; 20], leg: &Leg) -> Me {
    if leg.xrp {
        let key = keylet::account_root_key(id);
        let Some(a) = json_at(sandbox, &key) else { return (0, 0) };
        let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let oc = a["OwnerCount"].as_u64().unwrap_or(0) as u128;
        let reserve = XRP_RESERVE_BASE + XRP_RESERVE_INC * oc;
        (bal.saturating_sub(reserve), 0)
    } else if id == &leg.issuer {
        (u128::MAX / 4, 20) // issuers deliver their own IOU without limit
    } else {
        let lkey = keylet::ripple_state_key(id, &leg.issuer, &leg.cur);
        let Some(line) = json_at(sandbox, &lkey) else { return (0, 0) };
        let (neg, bal) = signed_value(&line["Balance"]);
        let holds = if id < &leg.issuer { !neg } else { neg }; // positive toward the party?
        // Balance is from the LOW account's perspective: positive = high owes
        // low. The party HOLDS the IOU when the balance points toward them.
        let party_low = id < &leg.issuer;
        let party_holds = if party_low { !neg } else { neg };
        let _ = holds;
        if party_holds && bal.0 > 0 { bal } else { (0, 0) }
    }
}

/// Adjust one party's side of an IOU movement (line balance ±amt), creating
/// the line if the receiver has none (rippled offer-crossing behavior).
pub(crate) fn line_adjust(sandbox: &mut Sandbox, party: &[u8; 20], leg: &Leg, amt: Me, receiving: bool) {
    if party == &leg.issuer {
        return;
    }
    let lkey = keylet::ripple_state_key(party, &leg.issuer, &leg.cur);
    let party_low = party < &leg.issuer;
    // rippled flag layout (per side of the line).
    const LOW_RESERVE: u64 = 0x0001_0000;
    const HIGH_RESERVE: u64 = 0x0002_0000;
    const LOW_NO_RIPPLE: u64 = 0x0010_0000;
    const HIGH_NO_RIPPLE: u64 = 0x0020_0000;
    const LOW_FREEZE: u64 = 0x0040_0000;
    const HIGH_FREEZE: u64 = 0x0080_0000;
    if let Some(mut line) = json_at(sandbox, &lkey) {
        let (lneg, lbal) = signed_value(&line["Balance"]);
        // party's holding: low holds when balance positive, high when negative
        let (hneg, h) = if party_low { (lneg, lbal) } else { (!lneg && lbal.0 > 0, lbal) };
        let hneg = if party_low { lneg } else { !lneg && lbal.0 > 0 };
        let _ = (hneg, h);
        let (pneg, pmag) = if party_low { (lneg, lbal) } else { (!lneg, lbal) };
        let (nneg, nmag) = signed_add(pneg && pmag.0 > 0, pmag, !receiving, amt);
        let (wneg, wmag) = if party_low { (nneg, nmag) } else { (!nneg, nmag) };
        let sign = if wneg && wmag.0 > 0 { "-" } else { "" };
        line["Balance"]["value"] = serde_json::Value::String(format!("{}{}", sign, me_to_value_string(wmag)));

        // rippled rippleCreditIOU: a line the SENDER just spent from positive
        // down to zero-or-below reverts to its default state — release their
        // reserve, clear their reserve flag, and delete the line outright
        // once the counterparty holds no reserve on it either. Guarded the
        // rippled way: their side must be unfrozen, NoRipple-vs-DefaultRipple
        // divergent, with a zero limit and zero quality settings.
        let spent_out = !receiving && pmag.0 > 0 && nmag.0 == 0;
        if spent_out {
            let flags = line["Flags"].as_u64().unwrap_or(0);
            let (my_reserve, my_no_ripple, my_freeze, my_limit, their_reserve) = if party_low {
                (LOW_RESERVE, LOW_NO_RIPPLE, LOW_FREEZE, "LowLimit", HIGH_RESERVE)
            } else {
                (HIGH_RESERVE, HIGH_NO_RIPPLE, HIGH_FREEZE, "HighLimit", LOW_RESERVE)
            };
            let limit_zero = line[my_limit]["value"].as_str().map(|v| v == "0").unwrap_or(true);
            let default_ripple = json_at(sandbox, &keylet::account_root_key(party))
                .and_then(|a| a["Flags"].as_u64())
                .map(|f| f & 0x0080_0000 != 0)
                .unwrap_or(false);
            let quality_zero = line.get("LowQualityIn").is_none()
                && line.get("LowQualityOut").is_none()
                && line.get("HighQualityIn").is_none()
                && line.get("HighQualityOut").is_none();
            if flags & my_reserve != 0
                && (flags & my_no_ripple != 0) != default_ripple
                && flags & my_freeze == 0
                && limit_zero
                && quality_zero
            {
                owner_count_add(sandbox, party, -1);
                line["Flags"] = serde_json::Value::from(flags & !my_reserve);
                if flags & their_reserve == 0 {
                    // Default on both sides: the line stops existing.
                    sandbox.delete(lkey);
                    crate::ledger::directory::owner_dir_remove(sandbox, party, &lkey, None);
                    crate::ledger::directory::owner_dir_remove(sandbox, &leg.issuer, &lkey, None);
                    return;
                }
            }
        }
        put_json(sandbox, lkey, &line);
    } else if receiving {
        let (lo, hi) = if party_low { (party, &leg.issuer) } else { (&leg.issuer, party) };
        let bal_neg = !party_low; // holding sits on the party's side
        let sign = if bal_neg { "-" } else { "" };
        let cur_str = hex::encode_upper(leg.cur);
        // rippled trustCreate: the RECEIVER carries the reserve, and their
        // side gets NoRipple unless their account has DefaultRipple set.
        let default_ripple = json_at(sandbox, &keylet::account_root_key(party))
            .and_then(|a| a["Flags"].as_u64())
            .map(|f| f & 0x0080_0000 != 0)
            .unwrap_or(false);
        let mut flags = if party_low { LOW_RESERVE } else { HIGH_RESERVE };
        if !default_ripple {
            flags |= if party_low { LOW_NO_RIPPLE } else { HIGH_NO_RIPPLE };
        }
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Flags": flags,
            "Balance": {"currency": cur_str, "issuer": "0000000000000000000000000000000000000000",
                         "value": format!("{}{}", sign, me_to_value_string(amt))},
            "LowLimit": {"currency": cur_str, "issuer": hex::encode(lo), "value": "0"},
            "HighLimit": {"currency": cur_str, "issuer": hex::encode(hi), "value": "0"},
        });
        put_json(sandbox, lkey, &line);
        // The line joins BOTH owner directories, but only the receiver pays
        // the reserve — the issuer's OwnerCount is untouched.
        crate::ledger::directory::owner_dir_insert(sandbox, party, &lkey);
        crate::ledger::directory::owner_dir_insert(sandbox, &leg.issuer, &lkey);
        owner_count_add(sandbox, party, 1);
    }
}

/// Move `amt` of `leg` from one account to another.
pub(crate) fn move_leg(sandbox: &mut Sandbox, from: &[u8; 20], to: &[u8; 20], leg: &Leg, amt: Me) {
    if me_is_zero(amt) {
        return;
    }
    if leg.xrp {
        let drops = me_rescale(amt, 0, false);
        for (id, add) in [(from, false), (to, true)] {
            let key = keylet::account_root_key(id);
            if let Some(mut a) = json_at(sandbox, &key) {
                let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                let nb = if add { bal.saturating_add(drops) } else { bal.saturating_sub(drops) };
                a["Balance"] = serde_json::Value::String(nb.to_string());
                put_json(sandbox, key, &a);
            }
        }
    } else {
        line_adjust(sandbox, from, leg, amt, false);
        line_adjust(sandbox, to, leg, amt, true);
    }
}

/// Fully remove a maker offer: object + owner-dir entry + book-dir entry +
/// maker OwnerCount.
pub(crate) fn delete_maker_offer(
    sandbox: &mut Sandbox,
    okey: &xrpl_core::types::Hash256,
    offer: &serde_json::Value,
    maker: &[u8; 20],
) {
    let hint = |f: &str| offer.get(f).map(dirnum).filter(|n| *n > 0);
    let owner_hint = offer.get("OwnerNode").map(dirnum);
    let book_hint = offer.get("BookNode").map(dirnum);
    let _ = hint;
    sandbox.delete(*okey);
    crate::ledger::directory::owner_dir_remove(sandbox, maker, okey, owner_hint);
    if let Some(bd) = offer
        .get("BookDirectory")
        .and_then(|v| v.as_str())
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .map(xrpl_core::types::Hash256)
    {
        crate::ledger::directory::dir_remove(sandbox, &bd, okey, book_hint);
    }
    owner_count_add(sandbox, maker, -1);
}

/// The issuer-published TickSize governing a pair: the smaller of the two
/// issuers' TickSize fields (XRP has none). 16 means "no tick rounding"
/// (rippled's Quality::maxTickSize).
pub(crate) fn tick_size_for(sandbox: &Sandbox, a: &Leg, b: &Leg) -> u32 {
    let mut ts = 16u32;
    for leg in [a, b] {
        if leg.xrp {
            continue;
        }
        if let Some(acct) = json_at(sandbox, &keylet::account_root_key(&leg.issuer)) {
            if let Some(t) = acct.get("TickSize").and_then(|v| v.as_u64()) {
                if t > 0 {
                    ts = ts.min(t as u32);
                }
            }
        }
    }
    ts
}

/// rippled Quality::round(digits): round the rate mantissa UP to `digits`
/// significant decimal digits.
fn quality_round_up(rate: u64, digits: u32) -> u64 {
    if digits >= 16 {
        return rate;
    }
    let modulus = 10u64.pow(16 - digits);
    let exp = rate >> 56;
    let man = rate & 0x00FF_FFFF_FFFF_FFFF;
    let man = man + modulus - 1;
    let man = man - (man % modulus);
    (exp << 56) | man
}

/// Normalize a mantissa into rippled's STAmount range [1e15, 1e16).
fn norm16(x: Me) -> Me {
    let (mut m, mut e) = x;
    if m == 0 {
        return (0, 0);
    }
    while m >= 10_000_000_000_000_000 {
        m /= 10;
        e += 1;
    }
    while m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    (m, e)
}

/// `num/den` rounded half-even to 16 significant digits — Number's real
/// rounding, with the division remainder folded into the tie comparison.
/// The legacy `+5`/`+7` half-adjust tricks approximate this under
/// TRUNCATING canonicalize; mixing them with a nearest pass double-rounds
/// (0EAE58BB fixed / 42071037 broken taught this the hard way).
fn div_nearest_16(num: u128, den: u128, e: i32) -> Me {
    if num == 0 || den == 0 {
        return (0, 0);
    }
    let q = num / den;
    let r = num % den;
    let mut k = 0u32;
    let mut t = q;
    while t >= 10_000_000_000_000_000 {
        t /= 10;
        k += 1;
    }
    let d = 10u128.pow(k);
    let (mut m, rr) = (q / d, q % d);
    // Discarded tail = rr + r/den (in ulps of the kept mantissa) vs d/2.
    let lhs = 2 * (rr * den + r);
    let rhs = d * den;
    if lhs > rhs || (lhs == rhs && m & 1 == 1) {
        m += 1;
    }
    let mut e = e + k as i32;
    if m >= 10_000_000_000_000_000 {
        m /= 10;
        e += 1;
    }
    while m > 0 && m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    (m, e)
}

/// rippled `divide(num, den, issue)` under Number semantics: exact quotient
/// rounded half-even at 16 digits (drops for XRP).
fn st_divide(num: Me, den: Me, xrp: bool) -> Me {
    if num.0 == 0 || den.0 == 0 {
        return (0, 0);
    }
    let (nm, ne) = norm16(num);
    let (dm, de) = norm16(den);
    if xrp {
        let v = nm * 100_000_000_000_000_000u128 / dm + 5;
        (me_rescale_nearest((v, ne - de - 17)), 0)
    } else {
        div_nearest_16(nm * 100_000_000_000_000_000u128, dm, ne - de - 17)
    }
}

/// rippled `multiply(v1, v2, issue)` under Number semantics: exact product
/// rounded half-even at 16 digits (drops for XRP).
fn st_multiply(a: Me, b: Me, xrp: bool) -> Me {
    if a.0 == 0 || b.0 == 0 {
        return (0, 0);
    }
    let (am, ae) = norm16(a);
    let (bm, be) = norm16(b);
    if xrp {
        let v = am * bm / 100_000_000_000_000u128 + 7;
        (me_rescale_nearest((v, ae + be + 14)), 0)
    } else {
        div_nearest_16(am * bm, 1, ae + be)
    }
}

/// rippled `getRate(pays, gets)` on raw mantissa/exponent pairs — the same
/// algorithm as `keylet::offer_quality`, without the JSON round-trip.
fn rate_of_me(pays: Me, gets: Me) -> Option<u64> {
    if pays.0 == 0 || gets.0 == 0 {
        return None;
    }
    let (nm, ne) = norm16(pays);
    let (dm, de) = norm16(gets);
    let v = nm * 100_000_000_000_000_000u128 / dm + 5;
    let mut e = ne - de - 17;
    let mut k = 0u32;
    let mut t = v;
    while t >= 10_000_000_000_000_000 {
        t /= 10;
        k += 1;
    }
    let d = 10u128.pow(k);
    let (mut q, r) = (v / d, v % d);
    let twice = r * 2;
    if twice > d || (twice == d && q & 1 == 1) {
        q += 1;
    }
    e += k as i32;
    if q >= 10_000_000_000_000_000 {
        q /= 10;
        e += 1;
    }
    Some((((e + 100) as u64) << 56) | q as u64)
}

/// Scale a mantissa/exponent to an integer drop count, rounding to nearest
/// (rippled's `XRPAmount{Number}` conversion under the default round mode).
fn me_rescale_nearest(x: Me) -> u128 {
    let (m, e) = x;
    if m == 0 {
        return 0;
    }
    if e >= 0 {
        return m * 10u128.pow(e.unsigned_abs().min(30));
    }
    let shift = e.unsigned_abs().min(39);
    let d = 10u128.pow(shift);
    let q = m / d;
    let r = m % d;
    let twice = r.saturating_mul(2);
    if twice > d || (twice == d && q & 1 == 1) {
        q + 1
    } else {
        q
    }
}

/// Apply issuer tick-size rounding to a requested offer, exactly as
/// rippled's CreateOffer does BEFORE crossing: round the rate up to the
/// tick, then re-derive the side that isn't held exact — TakerPays for a
/// tfSell offer, TakerGets otherwise.
pub(crate) fn apply_tick_size(pays: Me, gets: Me, sell: bool, tick: u32, pays_xrp: bool, gets_xrp: bool) -> (Me, Me) {
    if tick >= 16 {
        return (pays, gets);
    }
    let Some(rate) = rate_of_me(pays, gets) else {
        return (pays, gets);
    };
    let rounded = quality_round_up(rate, tick);
    let rate_me = ((rounded & 0x00FF_FFFF_FFFF_FFFF) as u128, ((rounded >> 56) as i32) - 100);
    if sell {
        // Hold TakerGets, re-derive TakerPays = TakerGets × rate.
        let p = st_multiply(gets, rate_me, pays_xrp);
        if p.0 == 0 { (pays, gets) } else { (p, gets) }
    } else {
        // Hold TakerPays, re-derive TakerGets = TakerPays ÷ rate.
        let g = st_divide(pays, rate_me, gets_xrp);
        if g.0 == 0 { (pays, gets) } else { (pays, g) }
    }
}

/// Quality-ordered (rate, offer key) ladder of a book — every resting offer,
/// best first, capped.
fn book_offer_ladder(sandbox: &Sandbox, base: &Hash256, cap: usize) -> Vec<(u64, Hash256)> {
    let mut out = Vec::new();
    for dk in sandbox.keys_with_prefix(&base.0[..24]) {
        let q = u64::from_be_bytes(dk.0[24..32].try_into().unwrap_or_default());
        let mut page_key = dk;
        for _ in 0..10_000 {
            let Some(page) = json_at(sandbox, &page_key) else { break };
            for ent in page.get("Indexes").and_then(|v| v.as_array()).into_iter().flatten() {
                if let Some(k) = ent.as_str().and_then(|s| hex::decode(s).ok())
                    .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                {
                    out.push((q, Hash256(k)));
                    if out.len() >= cap {
                        return out;
                    }
                }
            }
            let next = page.get("IndexNext").map(dirnum).unwrap_or(0);
            if next == 0 {
                break;
            }
            page_key = keylet::dir_page_key(&dk, next);
        }
    }
    out
}

/// Decode a u64-encoded rate into (mantissa, exponent).
fn rate_me(q: u64) -> Me {
    ((q & 0x00FF_FFFF_FFFF_FFFF) as u128, ((q >> 56) as i32) - 100)
}

/// First ladder entry whose offer still exists, is an Offer, is non-empty,
/// and is funded. With `mutate`, dead offers encountered on the way are
/// deleted exactly like the direct walk does — but ONLY when their funding
/// state is actually KNOWN (a maker whose root/line was never loaded is
/// skipped, not condemned; phantom deletions poisoned the first bridge
/// attempt). Without `mutate`, this is a pure peek: nothing is touched and
/// the caller's index is not advanced.
fn live_head(
    sandbox: &mut Sandbox,
    ladder: &[(u64, Hash256)],
    start: &mut usize,
    taker: &[u8; 20],
    maker_pays_leg: &Leg,
    mutate: bool,
    stale: &mut Vec<Hash256>,
) -> Option<(u64, Hash256, serde_json::Value, [u8; 20], Me, Me)> {
    let mut i = *start;
    let result = loop {
        if i >= ladder.len() {
            break None;
        }
        let (q, okey) = ladder[i];
        let Some(offer) = json_at(sandbox, &okey) else { i += 1; continue };
        if offer.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("Offer") {
            i += 1;
            continue;
        }
        let Some(maker) = offer.get("Account").and_then(|v| v.as_str()).and_then(decode20) else {
            i += 1;
            continue;
        };
        if &maker == taker {
            if mutate {
                delete_maker_offer(sandbox, &okey, &offer, &maker);
                stale.push(okey);
            }
            i += 1;
            continue;
        }
        let (Some(gives), Some(wants)) = (
            offer.get("TakerGets").and_then(keylet::amount_mant_exp),
            offer.get("TakerPays").and_then(keylet::amount_mant_exp),
        ) else {
            i += 1;
            continue;
        };
        // Funding is only judgeable when the backing object is in state.
        let funding_known = if maker_pays_leg.xrp {
            json_at(sandbox, &keylet::account_root_key(&maker)).is_some()
        } else {
            maker == maker_pays_leg.issuer
                || json_at(sandbox, &keylet::ripple_state_key(&maker, &maker_pays_leg.issuer, &maker_pays_leg.cur)).is_some()
        };
        if !funding_known {
            i += 1;
            continue;
        }
        if gives.0 == 0 || wants.0 == 0 || me_is_zero(available(sandbox, &maker, maker_pays_leg)) {
            if mutate {
                delete_maker_offer(sandbox, &okey, &offer, &maker);
                stale.push(okey);
            }
            i += 1;
            continue;
        }
        break Some((q, okey, offer, maker, gives, wants));
    };
    if mutate {
        *start = i;
    }
    result
}

/// Consume `give` of the maker's gives / `pay` of their wants against one
/// offer: move both legs and update or delete the offer object.
#[allow(clippy::too_many_arguments)]
fn settle_fill(
    sandbox: &mut Sandbox,
    okey: &Hash256,
    offer: &serde_json::Value,
    maker: &[u8; 20],
    from_taker: &[u8; 20],
    to_beneficiary: &[u8; 20],
    pays_leg: &Leg,
    gets_leg: &Leg,
    give: Me,
    pay: Me,
    gives0: Me,
    wants0: Me,
) {
    move_leg(sandbox, maker, to_beneficiary, pays_leg, give);
    move_leg(sandbox, from_taker, maker, gets_leg, pay);
    let funded = available(sandbox, maker, pays_leg);
    let consumed = me_cmp(give, gives0).is_ge() || me_is_zero(funded);
    if consumed {
        delete_maker_offer(sandbox, okey, offer, maker);
    } else {
        let mut off2 = offer.clone();
        off2["TakerGets"] = me_amount_json(&offer["TakerGets"], me_sub(gives0, give));
        off2["TakerPays"] = me_amount_json(&offer["TakerPays"], me_sub(wants0, pay));
        put_json(sandbox, *okey, &off2);
    }
}

/// IOU↔IOU crossing with XRP autobridging (rippled FlowCross): at every step
/// the DIRECT book competes with the two-book XRP bridge; the better maker
/// rate is consumed one offer (or one bridge slice) at a time. Engaged only
/// when both bridge books have depth — otherwise the plain direct walk runs.
/// Reaching here means two strands, i.e. rippled's `AMMContext::multiPath` is
/// true, which is why the pool competes with Fibonacci slices below.
#[allow(clippy::too_many_arguments)]
fn cross_bridged(
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    mut rem_pays: Me,
    mut rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    sell: bool,
    inv_base: &Hash256,
    amm: &Option<crate::tx::amm_swap::Amm>,
    sandbox: &mut Sandbox,
    stale: &mut Vec<Hash256>,
) -> Option<(Me, Me, u32)> {
    let xrp_leg = Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] };
    let zero = [0u8; 20];
    // Leg A: spend our gets, acquire XRP. Leg B: spend XRP, acquire our pays.
    let base_a = keylet::book_base(&gets_leg.cur, &zero, &gets_leg.issuer, &zero);
    let base_b = keylet::book_base(&zero, &pays_leg.cur, &zero, &pays_leg.issuer);
    let la = book_offer_ladder(sandbox, &base_a, 128);
    let lb = book_offer_ladder(sandbox, &base_b, 128);
    // Per-leg AMM liquidity: each bridge leg is a BookStep of its own pair.
    let amm_a = crate::tx::amm_swap::discover(sandbox, gets_leg, &xrp_leg, taker);
    let amm_b = crate::tx::amm_swap::discover(sandbox, &xrp_leg, pays_leg, taker);
    let amm_a_init = amm_a
        .as_ref()
        .map(|a| crate::tx::amm_swap::pool_balances(sandbox, a, &xrp_leg, gets_leg))
        .unwrap_or(((0, 0), (0, 0)));
    let amm_b_init = amm_b
        .as_ref()
        .map(|a| crate::tx::amm_swap::pool_balances(sandbox, a, pays_leg, &xrp_leg))
        .unwrap_or(((0, 0), (0, 0)));
    let (mut amm_a_iters, mut amm_b_iters) = (0u32, 0u32);
    if (la.is_empty() && amm_a.is_none()) || (lb.is_empty() && amm_b.is_none()) {
        if std::env::var("DX_BRIDGE").is_ok() {
            eprintln!("DX_BRIDGE bail la={} lb={} base_a={} base_b={}",
                la.len(), lb.len(), hex::encode_upper(&base_a.0[..12]), hex::encode_upper(&base_b.0[..12]));
        }
        return None; // no bridge: caller runs the direct walk
    }
    let ld = book_offer_ladder(sandbox, inv_base, 128);
    let thr = (threshold != u64::MAX).then(|| rate_me(threshold));
    let (mut di, mut ai, mut bi) = (0usize, 0usize, 0usize);
    let mut crossed = 0u32;
    // Offer crossing is always multi-strand (direct + XRP bridge), so the pool
    // competes with FIB-sequence offers off its starting balances. Single-
    // strand walks never reach here — they size the AMM through
    // `amm_swap::consume` on the direct walk instead.
    let amm_init = amm.as_ref().map(|a| crate::tx::amm_swap::pool_balances(sandbox, a, pays_leg, gets_leg));
    let mut amm_iters = 0u32;
    let done = |rp: Me, rg: Me| me_is_zero(rg) || (!sell && me_is_zero(rp));
    for _ in 0..512 {
        if done(rem_pays, rem_gets) {
            break;
        }
        // PEEK both sources (no mutation) to pick the better rate within the
        // threshold; only the chosen source is then walked with mutation, so
        // dead-offer cleanup happens exactly where rippled's walk reaches.
        // Each BRIDGE LEG is a BookStep of its own pair, so its book head
        // competes with that pair's pool (fib slice) — #105666830's XAH leg
        // filled from the XAH/XRP pool on mainnet.
        let dpeek = live_head(sandbox, &ld, &mut di, taker, pays_leg, false, stale);
        let apeek = live_head(sandbox, &la, &mut ai, taker, &xrp_leg, false, stale);
        let bpeek = live_head(sandbox, &lb, &mut bi, taker, pays_leg, false, stale);
        let a_fib = amm_a.as_ref().and_then(|am| {
            crate::tx::amm_swap::fib_slice(sandbox, am, amm_a_init, amm_a_iters, &xrp_leg, gets_leg)
                .map(|s| (crate::tx::amm_swap::slice_rate(s.0, s.1), s))
        });
        let b_fib = amm_b.as_ref().and_then(|am| {
            crate::tx::amm_swap::fib_slice(sandbox, am, amm_b_init, amm_b_iters, pays_leg, &xrp_leg)
                .map(|s| (crate::tx::amm_swap::slice_rate(s.0, s.1), s))
        });
        let qa_book = apeek.as_ref().map(|(q, ..)| rate_me(*q));
        let qb_book = bpeek.as_ref().map(|(q, ..)| rate_me(*q));
        // The pool wins a leg only when STRICTLY better than the book head.
        let a_use_amm = match (&a_fib, qa_book) {
            (Some((qf, _)), Some(qb)) => me_cmp(*qf, qb).is_lt(),
            (Some(_), None) => true,
            _ => false,
        };
        let b_use_amm = match (&b_fib, qb_book) {
            (Some((qf, _)), Some(qb)) => me_cmp(*qf, qb).is_lt(),
            (Some(_), None) => true,
            _ => false,
        };
        let qa = if a_use_amm { a_fib.as_ref().map(|(q, _)| *q) } else { qa_book };
        let qb = if b_use_amm { b_fib.as_ref().map(|(q, _)| *q) } else { qb_book };
        let dq = dpeek.as_ref().map(|(q, ..)| rate_me(*q));
        let bq = match (qa, qb) {
            (Some((am, ae)), Some((bm, be))) => Some(norm16((am * bm, ae + be))),
            _ => None,
        };
        // AMM turn: the direct-pair pool competes with the best BOOK rate
        // via multi-path FIB slices (its AVERAGE quality incl. slippage/fee).
        if let (Some(a), Some(init)) = (amm, &amm_init) {
            let best_book = match (dq, bq) {
                (Some(d), Some(b)) => Some(if me_cmp(d, b).is_le() { d } else { b }),
                (Some(d), None) => Some(d),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            if std::env::var("DX_AMM").is_ok() {
                eprintln!("DX_AMM site=bridged best_book={best_book:?}");
            }
            let (rp, rg, used) = crate::tx::amm_swap::consume_fib(
                sandbox, a, taker, beneficiary, rem_pays, rem_gets, pays_leg, gets_leg,
                threshold, sell, *init, amm_iters, best_book,
            );
            rem_pays = rp;
            rem_gets = rg;
            if used {
                amm_iters += 1;
                crossed += 1;
                if done(rem_pays, rem_gets) {
                    break;
                }
                continue;
            }
            if done(rem_pays, rem_gets) {
                break;
            }
        }
        // Pick the better (lower pays-per-gets) source within the threshold.
        let use_direct = match (dq, bq) {
            (Some(d), Some(b)) => me_cmp(d, b).is_le(),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        let spot = if use_direct { dq.unwrap() } else { bq.unwrap() };
        if std::env::var("DX_BRIDGE").is_ok() {
            eprintln!("DX_BRIDGE dq={dq:?} bq={bq:?} thr={thr:?} use_direct={use_direct} di={di} ai={ai} bi={bi} ld={} la={} lb={}", ld.len(), la.len(), lb.len());
        }
        if let Some(t) = thr {
            if me_cmp(spot, t).is_gt() {
                break;
            }
        }
        if use_direct {
            let Some((_, okey, offer, maker, gives0, wants0)) =
                live_head(sandbox, &ld, &mut di, taker, pays_leg, true, stale)
            else { break };
            let funded = available(sandbox, &maker, pays_leg);
            let m_gives = if me_cmp(funded, gives0).is_lt() { funded } else { gives0 };
            let mut give = if !sell && me_cmp(rem_pays, m_gives).is_lt() { rem_pays } else { m_gives };
            let mut pay = me_muldiv(give, wants0, gives0, true);
            if me_cmp(pay, rem_gets).is_gt() {
                pay = rem_gets;
                give = me_muldiv(pay, gives0, wants0, false);
                if me_is_zero(give) {
                    break;
                }
            }
            settle_fill(sandbox, &okey, &offer, &maker, taker, beneficiary,
                        pays_leg, gets_leg, give, pay, gives0, wants0);
            rem_pays = me_sub(rem_pays, give);
            rem_gets = me_sub(rem_gets, pay);
            crossed += 1;
        } else {
            // Resolve each leg's source: book maker offer or that pair's
            // pool fib slice. A-side capacity/rate in (XRP-out, gets-in),
            // B-side in (pays-out, XRP-in).
            let a_book = if a_use_amm {
                None
            } else {
                match live_head(sandbox, &la, &mut ai, taker, &xrp_leg, true, stale) {
                    Some(h) => Some(h),
                    None => break,
                }
            };
            let b_book = if b_use_amm {
                None
            } else {
                match live_head(sandbox, &lb, &mut bi, taker, pays_leg, true, stale) {
                    Some(h) => Some(h),
                    None => break,
                }
            };
            // (xrp capacity, gets per that xrp) for leg A.
            let (a_cap_xrp, a_in_full, a_out_full) = match (&a_book, &a_fib) {
                (Some((_, _, _, amaker, a_gives0, a_wants0)), _) => {
                    let funded = available(sandbox, amaker, &xrp_leg);
                    let a_gives = if me_cmp(funded, *a_gives0).is_lt() { funded } else { *a_gives0 };
                    (a_gives, *a_wants0, *a_gives0)
                }
                (None, Some((_, (s_in, s_out)))) => (*s_out, *s_in, *s_out),
                (None, None) => break,
            };
            let (b_cap_xrp, b_in_full, b_out_full) = match (&b_book, &b_fib) {
                (Some((_, _, _, _, b_gives0, b_wants0)), _) => (*b_wants0, *b_wants0, *b_gives0),
                (None, Some((_, (s_in, s_out)))) => (*s_in, *s_in, *s_out),
                (None, None) => break,
            };
            let mut xrp = if me_cmp(a_cap_xrp, b_cap_xrp).is_lt() { a_cap_xrp } else { b_cap_xrp };
            let mut gets_in = me_muldiv(xrp, a_in_full, a_out_full, true);
            if me_cmp(gets_in, rem_gets).is_gt() {
                gets_in = rem_gets;
                xrp = me_muldiv(gets_in, a_out_full, a_in_full, false);
            }
            let mut pays_out = me_muldiv(xrp, b_out_full, b_in_full, false);
            if !sell && me_cmp(pays_out, rem_pays).is_gt() {
                pays_out = rem_pays;
                xrp = me_muldiv(pays_out, b_in_full, b_out_full, true);
                gets_in = me_muldiv(xrp, a_in_full, a_out_full, true);
            }
            let xrp = (me_rescale(xrp, 0, false), 0);
            if std::env::var("DX_BRIDGE").is_ok() {
                eprintln!("DX_BRIDGE slice xrp={xrp:?} gets_in={gets_in:?} pays_out={pays_out:?} a_amm={a_use_amm} b_amm={b_use_amm} rem_g={rem_gets:?} rem_p={rem_pays:?}");
            }
            if me_is_zero(xrp) || me_is_zero(gets_in) || me_is_zero(pays_out) {
                break;
            }
            // Leg A: taker sells gets for XRP (XRP rides in-flight via the
            // taker and nets out of their mutation set).
            match (&a_book, &a_fib) {
                (Some((_, akey, aoffer, amaker, a_gives0, a_wants0)), _) => {
                    settle_fill(sandbox, akey, aoffer, amaker, taker, taker,
                                &xrp_leg, gets_leg, xrp, gets_in, *a_gives0, *a_wants0);
                }
                (None, Some(_)) => {
                    crate::tx::amm_swap::apply_slice(
                        sandbox, amm_a.as_ref().unwrap(), taker, taker, &xrp_leg, gets_leg, gets_in, xrp,
                    );
                    amm_a_iters += 1;
                }
                _ => break,
            }
            // Leg B: taker sells that XRP for the pays side.
            match (&b_book, &b_fib) {
                (Some((_, bkey, boffer, bmaker, b_gives0, b_wants0)), _) => {
                    settle_fill(sandbox, bkey, boffer, bmaker, taker, beneficiary,
                                pays_leg, &xrp_leg, pays_out, xrp, *b_gives0, *b_wants0);
                }
                (None, Some(_)) => {
                    crate::tx::amm_swap::apply_slice(
                        sandbox, amm_b.as_ref().unwrap(), taker, beneficiary, pays_leg, &xrp_leg, xrp, pays_out,
                    );
                    amm_b_iters += 1;
                }
                _ => break,
            }
            rem_gets = me_sub(rem_gets, gets_in);
            rem_pays = me_sub(rem_pays, pays_out);
            crossed += 1;
        }
    }
    Some((rem_pays, rem_gets, crossed))
}

/// Walk the inverse book from best quality and cross while the maker's rate is
/// within `threshold`. Returns (remaining pays, remaining gets, crossed count).
///
/// `taker` funds the gets side and owns any self-crossed offers; the acquired
/// pays side is credited to `beneficiary`. For OfferCreate they are the same
/// account, but a Payment's strand output belongs to the DESTINATION — routing
/// it through the sender would materialize an intermediate trust line that
/// rippled never creates (and, when the destination is the issuer, the IOU is
/// redeemed rather than held).
/// `sell` selects tfSell semantics: the taker sells the ENTIRE gets side
/// (`rem_gets`), accepting more of the pays side than requested. The binding
/// constraint becomes `rem_gets` alone — `rem_pays` (the minimum to acquire)
/// stops bounding each fill and no longer terminates the walk once reached.
/// `offer_crossing` distinguishes rippled's FlowCross from Flow, and decides
/// BOTH structural questions at once:
///
///  * **Autobridging.** Only offer crossing synthesizes the XRP bridge
///    (`flow()` builds the direct and bridged strands itself). A payment's
///    strands come from its explicit Paths plus the default path, so a
///    payment reaches a book only where its path names one — it never routes
///    through an XRP leg the transaction did not ask for.
///  * **The AMM offer generator** (`AMMContext::multiPath`, set from the
///    strand count at `Flow.cpp:106` `setMultiPath(strands.size() > 1)`).
///    Bridged crossing is two strands, so the pool competes with Fibonacci
///    slices; every other walk is single-strand and sizes the AMM by
///    `changeSpotPriceQuality`/`maxOffer` instead. That correspondence is
///    exact here: `cross_bridged` runs iff multiPath would be true.
///
/// The one case this does not model is a payment carrying two or more
/// explicit Paths, which rippled would treat as multi-path; we walk only the
/// first path, so such a payment stays on the single-strand generator.
pub(crate) fn cross_engine_to(
    taker: &[u8; 20],
    beneficiary: &[u8; 20],
    mut rem_pays: Me,
    mut rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    sell: bool,
    offer_crossing: bool,
    domain: Option<&Hash256>,
    sandbox: &mut Sandbox,
    stale: &mut Vec<Hash256>,
) -> (Me, Me, u32) {
    let mut crossed = 0u32;
    if threshold == 0 {
        return (rem_pays, rem_gets, crossed);
    }
    // Strand is exhausted when the gets side is spent (always) or, for a
    // buy, when the wanted pays side is fully acquired.
    let done = |rp: Me, rg: Me| me_is_zero(rg) || (!sell && me_is_zero(rp));
    // AMM for the pair competes with the book at every quality level
    // (rippled BookStep + AMMLiquidity) — EXCEPT in a permissioned-domain
    // book, which no pool participates in: `BookStep::tryAMM` returns early
    // on `book_.domain` ("amm doesn't support domain yet", BookStep.cpp:820).
    // A domain offer therefore crosses domain offers only, and rests in full
    // when none match, however good the pool's price looks (#105761560
    // C9948B9C crossed 1.238909 XRP against the EUROP pool where mainnet
    // moved no value at all).
    let amm = match domain {
        Some(_) => None,
        None => crate::tx::amm_swap::discover(sandbox, gets_leg, pays_leg, taker),
    };
    let inv_base = match domain {
        Some(d) => keylet::book_base_domain(&gets_leg.cur, &pays_leg.cur, &gets_leg.issuer, &pays_leg.issuer, d),
        None => keylet::book_base(&gets_leg.cur, &pays_leg.cur, &gets_leg.issuer, &pays_leg.issuer),
    };
    // IOU↔IOU pairs autobridge through XRP (open books; the direct-pair AMM
    // competes inside the controller).
    if offer_crossing && !pays_leg.xrp && !gets_leg.xrp && domain.is_none() {
        if let Some(r) = cross_bridged(
            taker, beneficiary, rem_pays, rem_gets, pays_leg, gets_leg, threshold, sell,
            &inv_base, &amm, sandbox, stale,
        ) {
            return r;
        }
    }
    let dirs = sandbox.keys_with_prefix(&inv_base.0[..24]);
    'dirs: for dk in dirs {
        let q = u64::from_be_bytes(dk.0[24..32].try_into().unwrap_or_default());
        // AMM turn: consume pool liquidity while its spot quality strictly
        // beats this book level (anchored so the book resumes at `q`).
        if let Some(a) = &amm {
            if std::env::var("DX_AMM").is_ok() {
                eprintln!("DX_AMM site=direct-walk q={q:x}");
            }
            let (rp, rg, used) = crate::tx::amm_swap::consume(
                sandbox, a, taker, beneficiary, rem_pays, rem_gets, pays_leg, gets_leg, threshold, sell, Some(q),
            );
            rem_pays = rp;
            rem_gets = rg;
            crossed += used as u32;
            if done(rem_pays, rem_gets) {
                break 'dirs;
            }
        }
        if std::env::var("DX_BOOK").is_ok() {
            eprintln!("DX_BOOK dir q={q:016x} threshold={threshold:016x} cross={}", q <= threshold);
        }
        if q > threshold {
            break;
        }
        let mut page_key_h = dk;
        for _ in 0..10_000 {
            let Some(page) = json_at(sandbox, &page_key_h) else { break };
            let entries: Vec<String> = page
                .get("Indexes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            for ent in entries {
                let Some(okey) = hex::decode(&ent)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                    .map(xrpl_core::types::Hash256)
                else { continue };
                let Some(offer) = json_at(sandbox, &okey) else { continue };
                if offer.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("Offer") {
                    continue;
                }
                let Some(maker) = offer.get("Account").and_then(|v| v.as_str()).and_then(decode20)
                else { continue };
                if &maker == taker {
                    // Self-crossing: rippled cancels the older own offer.
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                    continue;
                }
                // Expired offers are never crossed — the stream collects them
                // as removable and they are deleted (`hasExpired`: expiry is
                // reached once parentCloseTime >= Expiration, so the base
                // ledger's close time is the one to test).
                // #105776250 CD408C1D crosses a book whose head expired 17s
                // before the parent closed; mainnet deletes it and kills the
                // offer, we consumed it.
                if let Some(exp) = offer.get("Expiration").and_then(|v| v.as_u64()) {
                    if exp != 0 && sandbox.base().header.close_time as u64 >= exp {
                        delete_maker_offer(sandbox, &okey, &offer, &maker);
                        stale.push(okey);
                        continue;
                    }
                }
                let (Some(m_gives0), Some(m_wants0)) = (
                    offer.get("TakerGets").and_then(keylet::amount_mant_exp),
                    offer.get("TakerPays").and_then(keylet::amount_mant_exp),
                ) else { continue };
                if m_gives0.0 == 0 || m_wants0.0 == 0 {
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                    continue;
                }
                let funded = available(sandbox, &maker, pays_leg);
                if me_is_zero(funded) {
                    // Unfunded offers found during the walk are removed.
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    stale.push(okey);
                    continue;
                }
                let m_gives = if me_cmp(funded, m_gives0).is_lt() { funded } else { m_gives0 };
                // A sell takes the whole maker offer (bounded only by rem_gets
                // below); a buy stops at the wanted rem_pays.
                let mut give = if !sell && me_cmp(rem_pays, m_gives).is_lt() { rem_pays } else { m_gives };
                if pays_leg.xrp {
                    give = (me_rescale(give, 0, false), 0);
                }
                let mut pay = me_muldiv(give, m_wants0, m_gives0, true);
                if gets_leg.xrp {
                    pay = (me_rescale(pay, 0, true), 0);
                }
                if me_cmp(pay, rem_gets).is_gt() {
                    pay = rem_gets;
                    give = me_muldiv(pay, m_gives0, m_wants0, false);
                    if pays_leg.xrp {
                        give = (me_rescale(give, 0, false), 0);
                    }
                    if me_is_zero(give) {
                        break 'dirs;
                    }
                }
                // The book gate above compares ENCODED qualities, where the
                // taker's limit and a maker can tie at 16 digits while the
                // maker is really a hair worse. What actually decides it is
                // the quality the fill ACHIEVES: once the fill is clamped to
                // the taker's remaining input and the output floored to whole
                // drops, the realised rate can land well outside the limit.
                // rippled re-checks exactly that and drops the whole path,
                // forgiving only a 1e-7 relative round-off (StrandFlow.h:698
                // "Path rejected by limitQuality").
                //
                // #105780948 101AD681: an IoC offer ties with the book head at
                // 0x5503BD5CE357AF28, but it is 2.1e-9 XMusic short of buying
                // the full 4621775 drops, so the fill floors to 4621774 and
                // realises 2.16e-7 worse than its limit. Mainnet crosses
                // nothing and returns tecKILLED; we filled it.
                if threshold != u64::MAX && !me_is_zero(give) && !me_is_zero(pay) {
                    if let Some(ach) = rate_of_me(pay, give) {
                        if ach > threshold {
                            let (a, t) = (rate_me(ach), rate_me(threshold));
                            let excess = me_muldiv(me_sub(a, t), (10_000_000, 0), (1, 0), false);
                            if me_cmp(excess, t).is_ge() {
                                break 'dirs;
                            }
                        }
                    }
                }
                move_leg(sandbox, &maker, beneficiary, pays_leg, give);
                move_leg(sandbox, taker, &maker, gets_leg, pay);
                rem_pays = me_sub(rem_pays, give);
                rem_gets = me_sub(rem_gets, pay);
                crossed += 1;
                let consumed = me_cmp(give, m_gives0).is_ge() || me_cmp(give, funded).is_ge();
                if consumed {
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                } else {
                    let mut off2 = offer.clone();
                    off2["TakerGets"] = me_amount_json(&offer["TakerGets"], me_sub(m_gives0, give));
                    off2["TakerPays"] = me_amount_json(&offer["TakerPays"], me_sub(m_wants0, pay));
                    put_json(sandbox, okey, &off2);
                }
                if done(rem_pays, rem_gets) {
                    break 'dirs;
                }
            }
            let next = page.get("IndexNext").map(dirnum).unwrap_or(0);
            if next == 0 {
                break;
            }
            page_key_h = keylet::dir_page_key(&dk, next);
        }
    }
    // Final AMM turn once the book is exhausted (maxOffer sizing).
    if let Some(a) = &amm {
        if !done(rem_pays, rem_gets) {
            if std::env::var("DX_AMM").is_ok() {
                eprintln!("DX_AMM site=direct-tail");
            }
            let (rp, rg, used) = crate::tx::amm_swap::consume(
                sandbox, a, taker, beneficiary, rem_pays, rem_gets, pays_leg, gets_leg, threshold, sell, None,
            );
            rem_pays = rp;
            rem_gets = rg;
            crossed += used as u32;
        }
    }
    (rem_pays, rem_gets, crossed)
}

/// Cross with the taker as its own beneficiary (OfferCreate semantics).
pub(crate) fn cross_engine(
    taker: &[u8; 20],
    rem_pays: Me,
    rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    sell: bool,
    domain: Option<&Hash256>,
    sandbox: &mut Sandbox,
    stale: &mut Vec<Hash256>,
) -> (Me, Me, u32) {
    // FlowCross always builds both the direct and the XRP-bridged strand, so
    // offer crossing is multi-path by construction.
    cross_engine_to(taker, taker, rem_pays, rem_gets, pays_leg, gets_leg, threshold, sell, true, domain, sandbox, stale)
}

pub struct OfferCreateTransactor;

impl Transactor for OfferCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "OfferCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("TakerPays").is_none() || tx.fields.get("TakerGets").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }

        // rippled CreateOffer::preclaim: an offer is unfunded when
        // accountFunds(TakerGets) <= 0 — for XRP that is balance minus
        // reserve, NOT the full sell amount (partially funded offers still
        // cross). Returns tecUNFUNDED_OFFER, not the generic tecUNFUNDED.
        // IOU-side funding is enforced by available() in do_apply.
        if let Some(leg) = leg_of(&tx.fields["TakerGets"]) {
            if me_is_zero(available(sandbox, &tx.account, &leg)) {
                return TxResult::UnfundedOffer;
            }
        }

        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let tp_json = tx.fields["TakerPays"].clone();
        let tg_json = tx.fields["TakerGets"].clone();
        let (Some(tp0), Some(tg0)) =
            (keylet::amount_mant_exp(&tp_json), keylet::amount_mant_exp(&tg_json))
        else {
            return TxResult::Malformed;
        };
        let (Some(pays_leg), Some(gets_leg)) = (leg_of(&tp_json), leg_of(&tg_json)) else {
            return TxResult::Malformed;
        };
        if tp0.0 == 0 || tg0.0 == 0 {
            return TxResult::Malformed;
        }
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let sell = flags & 0x0008_0000 != 0;
        let ioc = flags & 0x0002_0000 != 0;
        let fok = flags & 0x0004_0000 != 0;

        // Cancel-and-replace: an OfferSequence names a prior offer to cancel
        // before crossing/placing (rippled does this first, unconditionally).
        if let Some(old_seq) = tx.fields.get("OfferSequence").and_then(|v| v.as_u64()) {
            let old_key = keylet::offer_key(&tx.account, old_seq as u32);
            if let Some(old) = json_at(sandbox, &old_key) {
                delete_maker_offer(sandbox, &old_key, &old, &tx.account);
            }
        }

        let snap = sandbox.snapshot();

        // Issuer tick size rounds the requested rate up to N significant
        // digits and re-derives the non-exact side — BEFORE crossing, so the
        // crossing and the placed remainder both use the rounded amounts
        // (rippled CreateOffer::applyGuts).
        let tick = tick_size_for(sandbox, &pays_leg, &gets_leg);
        let (tp0, tg0) = apply_tick_size(tp0, tg0, sell, tick, pays_leg.xrp, gets_leg.xrp);
        if tp0.0 == 0 || tg0.0 == 0 {
            return TxResult::Success; // rounded to nothing: fee-only
        }

        // Taker funding: the offer can only sell what the account actually
        // holds of the TakerGets asset (issuers mint freely; XRP is balance
        // minus reserve; IOU is the trust-line holding). Holding none is an
        // unfunded offer — fee-only, nothing crossed or placed — no matter
        // how willing a counterparty is (rippled CreateOffer accountFunds).
        if me_is_zero(available(sandbox, &tx.account, &gets_leg)) {
            return TxResult::UnfundedOffer;
        }

        // A DomainID (XLS-80) scopes both crossing and placement to the
        // domain's book.
        let domain: Option<Hash256> = tx
            .fields
            .get("DomainID")
            .and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s).ok())
            .filter(|b| b.len() == 32)
            .map(|b| {
                let mut d = [0u8; 32];
                d.copy_from_slice(&b);
                Hash256(d)
            });

        // Cross against the inverse book while the maker's rate is within the
        // taker's limit price (threshold = quality with the sides swapped).
        let threshold = rate_of_me(tg0, tp0).unwrap_or(0);
        // Offers the walk removes as STALE — expired, unfunded, empty or the
        // taker's own — are not part of the crossing: rippled applies its
        // removableOffers to the cancel sandbox as well, so the cleanup
        // survives a kill that rolls every fill back (OfferCreate.cpp:460).
        let mut stale: Vec<Hash256> = Vec::new();
        let (rem_pays, rem_gets, crossed) = cross_engine(
            &tx.account, tp0, tg0, &pays_leg, &gets_leg, threshold, sell, domain.as_ref(), sandbox,
            &mut stale,
        );
        // Re-run the stale removals after a rollback restores them.
        let reap = |sandbox: &mut Sandbox, stale: &[Hash256]| {
            for okey in stale {
                let Some(off) = json_at(sandbox, okey) else { continue };
                let Some(maker) = off.get("Account").and_then(|v| v.as_str()).and_then(decode20)
                else { continue };
                delete_maker_offer(sandbox, okey, &off, &maker);
            }
        };

        let filled = if sell { me_is_zero(rem_gets) } else { me_is_zero(rem_pays) };
        if fok && !filled {
            // FillOrKill not fully filled: nothing survives but the fee and
            // the stale-offer cleanup.
            sandbox.restore_snapshot(snap);
            reap(sandbox, &stale);
            return TxResult::Killed;
        }
        if ioc {
            if crossed == 0 {
                sandbox.restore_snapshot(snap);
                reap(sandbox, &stale);
                return TxResult::Killed;
            }
            return TxResult::Success; // keep fills, never place
        }
        if me_is_zero(rem_pays) || me_is_zero(rem_gets) {
            return TxResult::Success; // fully consumed
        }

        if crossed == 0 {
            // Pure-placement refusals (mainnet meta: fee-only AccountRoot).
            if me_is_zero(available(sandbox, &tx.account, &gets_leg)) {
                return TxResult::UnfundedOffer;
            }
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(a) = json_at(sandbox, &acct_key) {
                let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                let oc = a["OwnerCount"].as_u64().unwrap_or(0) as u128;
                if bal < XRP_RESERVE_BASE + XRP_RESERVE_INC * (oc + 1) {
                    return TxResult::InsufReserveOffer;
                }
            }
        }

        // Place the remainder at the taker's ORIGINAL quality (rippled
        // preserves the price for partial fills).
        let seq = if tx.uses_ticket() { tx.ticket_seq.unwrap_or(0) } else { tx.sequence };
        let offer_key = keylet::offer_key(&tx.account, seq);
        let offer_obj = serde_json::json!({
            "LedgerEntryType": "Offer",
            "Account": hex::encode(tx.account),
            "Sequence": seq,
            "TakerPays": me_amount_json(&tp_json, rem_pays),
            "TakerGets": me_amount_json(&tg_json, rem_gets),
            "Flags": flags,
        });
        sandbox.write(offer_key, serde_json::to_vec(&offer_obj).expect("serializing valid JSON Value"));
        crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &offer_key);
        // Book quality comes from the offer as REQUESTED (after tick
        // rounding), not from the residual: rippled keeps a partially
        // crossed offer at its original price (uRate is computed before
        // crossing).
        if let Some(q) = rate_of_me(tp0, tg0) {
            let base = match &domain {
                Some(d) => keylet::book_base_domain(&pays_leg.cur, &gets_leg.cur, &pays_leg.issuer, &gets_leg.issuer, d),
                None => keylet::book_base(&pays_leg.cur, &gets_leg.cur, &pays_leg.issuer, &gets_leg.issuer),
            };
            let bdir = keylet::book_dir_key(&base, q);
            crate::ledger::directory::dir_insert(sandbox, &bdir, None, &offer_key);
        }
        owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

/// OfferCancel transactor — cancel an existing DEX offer.
pub struct OfferCancelTransactor;

impl Transactor for OfferCancelTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "OfferCancel" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("OfferSequence").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let offer_seq = match tx.fields.get("OfferSequence").and_then(|s| s.as_u64()) {
            Some(s) => s as u32,
            None => return TxResult::Malformed,
        };

        let offer_key = keylet::offer_key(&tx.account, offer_seq);

        if let Some(data) = sandbox.read(&offer_key) {
            // Pull the offer's directory hints before deleting it: OwnerNode
            // (page in the owner's dir), BookDirectory (root key of the order
            // book's quality dir) and BookNode (page within it). rippled's
            // offerDelete unlinks both directories via these hints.
            let offer: Option<serde_json::Value> = serde_json::from_slice(&data).ok();
            let hint = |v: Option<&serde_json::Value>| {
                v.and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                })
            };
            let owner_node = offer.as_ref().and_then(|o| hint(o.get("OwnerNode")));
            let book_node = offer.as_ref().and_then(|o| hint(o.get("BookNode")));
            let book_dir = offer
                .as_ref()
                .and_then(|o| o.get("BookDirectory"))
                .and_then(|v| v.as_str())
                .and_then(|s| hex::decode(s).ok())
                .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                .map(xrpl_core::types::Hash256);

            sandbox.delete(offer_key);
            crate::ledger::directory::owner_dir_remove(sandbox, &tx.account, &offer_key, owner_node);
            if let Some(bd) = book_dir {
                crate::ledger::directory::dir_remove(sandbox, &bd, &offer_key, book_node);
            }

            // Decrement OwnerCount
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(data) = sandbox.read(&acct_key) {
                if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                    if count > 0 {
                        acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
                    }
                    sandbox.write(acct_key, serde_json::to_vec(&acct).expect("serializing valid JSON Value"));
                }
            }
        }

        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::{apply_modifications, Sandbox};
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn read_balance(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        json_at(sandbox, &keylet::account_root_key(id))
            .and_then(|a| a["Balance"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0)
    }

    fn make_state_with_account(id: &[u8; 20], balance: u64) -> LedgerState {
        let header = LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        let mut state = LedgerState::new_unverified(header);
        let acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": 1,
            "OwnerCount": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        state
    }

    #[test]
    fn offer_create_places_on_book() {
        let acct = [0x01u8; 20];
        let state = make_state_with_account(&acct, 50_000_000);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "10"},
                "TakerGets": "1000000",
            }),
        };
        assert_eq!(OfferCreateTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Offer should exist on the book
        let offer_key = keylet::offer_key(&acct, 5);
        assert!(sandbox.exists(&offer_key));

        // OwnerCount incremented
        let acct_key = keylet::account_root_key(&acct);
        let data = sandbox.read(&acct_key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["OwnerCount"].as_u64().unwrap(), 1);
    }

    #[test]
    fn offer_cancel_removes_from_book() {
        let acct = [0x01u8; 20];
        let state = make_state_with_account(&acct, 50_000_000);

        // First create
        let mut sandbox = Sandbox::new(&state);
        let create_tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "1000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "10"},
            }),
        };
        OfferCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Then cancel
        let cancel_tx = TxFields {
            account: acct,
            tx_type: "OfferCancel".to_string(),
            fee: 12,
            sequence: 6,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"OfferSequence": 5}),
        };
        assert_eq!(OfferCancelTransactor.do_apply(&cancel_tx, &mut sandbox), TxResult::Success);

        let offer_key = keylet::offer_key(&acct, 5);
        assert!(!sandbox.exists(&offer_key));

        // OwnerCount back to 0
        let acct_key = keylet::account_root_key(&acct);
        let data = sandbox.read(&acct_key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["OwnerCount"].as_u64().unwrap(), 0);
    }

    #[test]
    fn quality_one_matches_rippled_constant() {
        // getRate(1 XRP, 1 XRP) — rippled's QUALITY_ONE.
        let one = serde_json::Value::String("1000000".into());
        assert_eq!(keylet::offer_quality(&one, &one), Some(0x55038D7EA4C68000));
        // mainnet-verified vector (#105666725 offer 95551964FE):
        // 602250000 drops / 602.25 RLUSD-ish IOU — sanity: nonzero, monotonic
        let pays = serde_json::Value::String("3500000".into());
        let gets = serde_json::json!({"currency":"ABC","issuer":"0000000000000000000000000000000000000001","value":"7"});
        let q1 = keylet::offer_quality(&pays, &gets).unwrap();
        let gets2 = serde_json::json!({"currency":"ABC","issuer":"0000000000000000000000000000000000000001","value":"14"});
        let q2 = keylet::offer_quality(&pays, &gets2).unwrap();
        assert!(q2 < q1); // paying the same for more = better (lower) quality
    }

    #[test]
    fn immediate_or_cancel_no_place() {
        let acct = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut state = make_state_with_account(&acct, 50_000_000);
        // Fund the taker's TakerGets (10 USD) so the offer is not rejected as
        // unfunded before the IoC-crosses-nothing path is even reached.
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");
        let (lo, hi) = if acct < issuer { (acct, issuer) } else { (issuer, acct) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if acct < issuer { "10" } else { "-10" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(keylet::ripple_state_key(&acct, &issuer, &cur), serde_json::to_vec(&line).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "1000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "10"},
                "Flags": 0x00020000u64, // tfImmediateOrCancel
            }),
        };
        // Mainnet (ImmediateOfferKilled amendment): IoC that crosses nothing
        // is tecKILLED, and nothing is placed.
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Killed);

        // IOC offer should NOT be placed on the book (no crossing happened)
        let offer_key = keylet::offer_key(&acct, 5);
        assert!(!sandbox.exists(&offer_key));
    }

    /// Mainnet tx A2AED79309E6… (ledger 105035380): a tfSell FoK selling XRP
    /// from an account whose balance sits BELOW its reserve (available <= 0)
    /// is tecUNFUNDED_OFFER, not tecUNFUNDED — rippled's accountFunds for a
    /// native sell is balance-minus-reserve.
    #[test]
    fn xrp_sell_below_reserve_is_unfunded_offer() {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        // Balance 725_581 with OwnerCount 1 → reserve 1_200_000 → available < 0.
        let mut state = make_state_with_account(&taker, 725_581);
        {
            let key = keylet::account_root_key(&taker);
            let mut a = json_at(&Sandbox::new(&state), &key).unwrap();
            a["OwnerCount"] = serde_json::json!(1);
            state.state_map.insert(key, serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 5,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "468801653",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "Flags": 0x000C_0000u64,
            }),
        };
        // The probe runs preclaim first — it must agree on tecUNFUNDED_OFFER,
        // not the generic tecUNFUNDED.
        assert_eq!(OfferCreateTransactor.preclaim(&tx, &sandbox), TxResult::UnfundedOffer);
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::UnfundedOffer);
    }

    /// Mainnet tx 8A70D6556E… (ledger 105035380): a tfSell FoK offering to
    /// sell an IOU the account holds NONE of is unfunded — rippled returns
    /// tecUNFUNDED_OFFER (fee-only) without crossing anything, even though a
    /// willing maker exists. The taker's funding of the TakerGets asset caps
    /// the crossing; zero holdings means nothing to sell.
    #[test]
    fn sell_offer_unfunded_when_taker_holds_none() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 50_000_000);
        for id in [&maker, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        // Maker holds 100 USD and offers to buy it back for XRP — a willing
        // counterparty, so any spurious crossing would show up.
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
                "TakerGets": "10000000",
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Taker holds NO USD but tries to sell 50 USD for XRP, tfSell + FoK.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "50"},
                "TakerPays": "1000000",
                "Flags": 0x000C_0000u64,
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::UnfundedOffer);
        // Nothing crossed: maker's offer and taker's XRP untouched.
        assert!(sandbox.exists(&keylet::offer_key(&maker, 2)));
        assert_eq!(read_balance(&sandbox, &taker), 50_000_000);
    }

    /// tfSell means "sell the ENTIRE TakerGets, even if that acquires more
    /// than TakerPays." A FillOrKill sell against a maker offering a better
    /// rate than the taker's minimum must consume the whole TakerGets (taking
    /// the surplus), not stop once TakerPays is reached — otherwise the
    /// unsold remainder fails the fill check and the offer is wrongly killed.
    #[test]
    fn fill_or_kill_sell_takes_surplus_over_taker_pays() {
        let taker = [0x01u8; 20];
        let maker = [0x04u8; 20];
        let issuer = [0x02u8; 20];
        let mut cur = [0u8; 20];
        cur[12..15].copy_from_slice(b"USD");

        let mut state = make_state_with_account(&taker, 50_000_000);
        for id in [&maker, &issuer] {
            let a = serde_json::json!({
                "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                "Balance": "50000000", "Sequence": 1, "OwnerCount": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        }
        // Maker holds 100 USD.
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000"},
        });
        state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();

        // Maker sells 100 USD for 10 XRP (10 USD per XRP — very generous).
        let mut sandbox = Sandbox::new(&state);
        let maker_offer = TxFields {
            account: maker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "10000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "100"},
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&maker_offer, &mut sandbox), TxResult::Success);
        assert!(sandbox.exists(&keylet::offer_key(&maker, 2)), "maker offer placed");
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();
        assert!(json_at(&Sandbox::new(&state), &keylet::offer_key(&maker, 2)).is_some(), "maker offer persisted");

        // Taker: tfSell + tfFillOrKill, sell 10 XRP for at least 5 USD.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerGets": "10000000",
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "Flags": 0x000C_0000u64, // tfSell | tfFillOrKill
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Taker sold all 10 XRP and acquired ~100 USD — far above TakerPays 5.
        assert_eq!(read_balance(&sandbox, &taker), 40_000_000);
        let tkey = keylet::ripple_state_key(&taker, &issuer, &cur);
        let tl = json_at(&sandbox, &tkey).expect("taker USD line");
        let (_neg, mag) = signed_value(&tl["Balance"]);
        assert!(me_cmp(mag, (50, 0)).is_gt(), "acquired well over TakerPays 5, got {mag:?}");
    }

    /// Build a state with `holder` and `issuer` accounts (no trust line).
    fn state_for_line(holder: &[u8; 20], issuer: &[u8; 20]) -> crate::ledger::state::LedgerState {
        let mut state = make_state_with_account(holder, 50_000_000);
        let iss = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(issuer),
            "Balance": "50000000",
            "Sequence": 1,
            "OwnerCount": 0,
        });
        state
            .state_map
            .insert(keylet::account_root_key(issuer), serde_json::to_vec(&iss).unwrap())
            .unwrap();
        state
    }

    fn owner_count(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        json_at(sandbox, &keylet::account_root_key(id))
            .and_then(|a| a["OwnerCount"].as_u64())
            .unwrap_or(0)
    }

    /// rippled's trustCreate charges the reserve to the RECEIVER only: the
    /// line joins both owner directories, but just one OwnerCount moves. The
    /// receiver's side also gets NoRipple (their account lacks DefaultRipple).
    #[test]
    fn line_creation_charges_only_the_receiver() {
        let holder = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = state_for_line(&holder, &issuer);
        let mut sandbox = Sandbox::new(&state);
        let leg = Leg { xrp: false, cur: *b"USD\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", issuer };

        line_adjust(&mut sandbox, &holder, &leg, (5, 0), true);

        let line = json_at(&sandbox, &keylet::ripple_state_key(&holder, &issuer, &leg.cur))
            .expect("line created");
        let flags = line["Flags"].as_u64().unwrap();
        let holder_low = holder < issuer;
        let (reserve, no_ripple) = if holder_low {
            (0x0001_0000, 0x0010_0000)
        } else {
            (0x0002_0000, 0x0020_0000)
        };
        assert_eq!(flags & reserve, reserve, "receiver reserve flag");
        assert_eq!(flags & no_ripple, no_ripple, "receiver NoRipple flag");
        assert_eq!(owner_count(&sandbox, &holder), 1);
        assert_eq!(owner_count(&sandbox, &issuer), 0, "issuer pays no reserve");
    }

    /// Mainnet tx 0A207078B3A4… (ledger 105666725): a line spent back to
    /// exactly zero returns to its default state and rippled DELETES it,
    /// releasing the holder's reserve and unlinking both owner directories.
    #[test]
    fn line_spent_to_zero_is_deleted() {
        let holder = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = state_for_line(&holder, &issuer);
        let mut sandbox = Sandbox::new(&state);
        let leg = Leg { xrp: false, cur: *b"USD\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", issuer };
        let lkey = keylet::ripple_state_key(&holder, &issuer, &leg.cur);

        line_adjust(&mut sandbox, &holder, &leg, (5, 0), true);
        assert!(sandbox.exists(&lkey));
        assert_eq!(owner_count(&sandbox, &holder), 1);

        // Spend every unit back out.
        line_adjust(&mut sandbox, &holder, &leg, (5, 0), false);

        assert!(!sandbox.exists(&lkey), "default line deleted at zero balance");
        assert_eq!(owner_count(&sandbox, &holder), 0, "reserve released");
    }

    /// A line spent only PART of the way down survives untouched.
    #[test]
    fn line_partially_spent_survives() {
        let holder = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = state_for_line(&holder, &issuer);
        let mut sandbox = Sandbox::new(&state);
        let leg = Leg { xrp: false, cur: *b"USD\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", issuer };
        let lkey = keylet::ripple_state_key(&holder, &issuer, &leg.cur);

        line_adjust(&mut sandbox, &holder, &leg, (5, 0), true);
        line_adjust(&mut sandbox, &holder, &leg, (2, 0), false);

        assert!(sandbox.exists(&lkey));
        assert_eq!(owner_count(&sandbox, &holder), 1);
    }

    /// Mainnet tx 9870DA80… (ledger 105091579): the STX issuer publishes
    /// TickSize 6, so rippled rounds the offer rate UP to 6 significant
    /// digits and re-derives the non-exact side before placing. The tx asks
    /// to sell 8539920 drops for 813087.72688567 STX; mainnet stored an
    /// offer of 8539914 drops at book quality 5321D3536A38DBA4.
    #[test]
    fn offer_placement_honors_issuer_tick_size() {
        let acct = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let mut state = make_state_with_account(&acct, 500_000_000);
        // Issuer account publishing TickSize 6.
        let iss_acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(issuer),
            "Balance": "50000000",
            "Sequence": 1,
            "OwnerCount": 0,
            "TickSize": 6,
        });
        state
            .state_map
            .insert(keylet::account_root_key(&issuer), serde_json::to_vec(&iss_acct).unwrap())
            .unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "STX", "issuer": hex::encode(issuer), "value": "813087.72688567"},
                "TakerGets": "8539920",
            }),
        };
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let placed = json_at(&sandbox, &keylet::offer_key(&acct, 5)).expect("offer placed");
        assert_eq!(placed["TakerGets"].as_str(), Some("8539914"));
        assert_eq!(placed["TakerPays"]["value"].as_str(), Some("813087.72688567"));

        // ...and it lands in the book page for the tick-rounded quality.
        let q = keylet::offer_quality(&placed["TakerPays"], &placed["TakerGets"]).unwrap();
        assert_eq!(format!("{q:016x}"), "5321d3536a38dba4");
    }
}
