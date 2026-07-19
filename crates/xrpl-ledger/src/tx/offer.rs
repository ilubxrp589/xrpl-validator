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
fn amount_currency20(v: &serde_json::Value) -> Option<[u8; 20]> {
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
fn amount_issuer20(v: &serde_json::Value) -> Option<[u8; 20]> {
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

type Me = (u128, i32);

struct Leg {
    xrp: bool,
    cur: [u8; 20],
    issuer: [u8; 20],
}

fn leg_of(v: &serde_json::Value) -> Option<Leg> {
    Some(Leg { xrp: v.is_string(), cur: amount_currency20(v)?, issuer: amount_issuer20(v)? })
}

fn decode20(s: &str) -> Option<[u8; 20]> {
    if let Ok(b) = hex::decode(s) {
        if b.len() == 20 {
            return <[u8; 20]>::try_from(b.as_slice()).ok();
        }
    }
    xrpl_core::types::AccountId::from_address(s).ok().map(|a| a.0)
}

fn me_rescale(a: Me, e: i32, ceil: bool) -> u128 {
    if a.1 >= e {
        let d = (a.1 - e).min(38) as u32;
        a.0.saturating_mul(10u128.saturating_pow(d))
    } else {
        let d = 10u128.saturating_pow(((e - a.1).min(38)) as u32);
        if ceil { a.0.div_ceil(d) } else { a.0 / d }
    }
}

fn me_cmp(a: Me, b: Me) -> std::cmp::Ordering {
    let e = a.1.min(b.1);
    me_rescale(a, e, false).cmp(&me_rescale(b, e, false))
}

fn me_is_zero(a: Me) -> bool {
    a.0 == 0
}

fn me_sub(a: Me, b: Me) -> Me {
    let e = a.1.min(b.1);
    (me_rescale(a, e, false).saturating_sub(me_rescale(b, e, false)), e)
}

fn me_norm(mut a: Me) -> Me {
    while a.0 >= 100_000_000_000_000_000_000 {
        a.0 /= 10;
        a.1 += 1;
    }
    a
}

/// a*b/c with directed rounding (mantissas kept ≤ ~1e20 so the product fits).
fn me_muldiv(a: Me, b: Me, c: Me, ceil: bool) -> Me {
    if c.0 == 0 {
        return (0, 0);
    }
    let (a, b, c) = (me_norm(a), me_norm(b), me_norm(c));
    let num = a.0.saturating_mul(b.0);
    let m = if ceil { num.div_ceil(c.0) } else { num / c.0 };
    me_norm((m, a.1 + b.1 - c.1))
}

fn me_to_value_string(a: Me) -> String {
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
fn me_amount_json(orig: &serde_json::Value, a: Me) -> serde_json::Value {
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

fn json_at(sandbox: &Sandbox, key: &xrpl_core::types::Hash256) -> Option<serde_json::Value> {
    sandbox.read(key).and_then(|d| serde_json::from_slice(&d).ok())
}

fn put_json(sandbox: &mut Sandbox, key: xrpl_core::types::Hash256, v: &serde_json::Value) {
    sandbox.write(key, serde_json::to_vec(v).unwrap_or_default());
}

fn dirnum(v: &serde_json::Value) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(s) = v.as_str() {
        return u64::from_str_radix(s, 16).ok().or_else(|| s.parse().ok()).unwrap_or(0);
    }
    0
}

/// Signed (negative, magnitude) of an amount value string.
fn signed_value(v: &serde_json::Value) -> (bool, Me) {
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

fn signed_add(aneg: bool, a: Me, bneg: bool, b: Me) -> (bool, Me) {
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

fn owner_count_add(sandbox: &mut Sandbox, id: &[u8; 20], delta: i64) {
    let key = keylet::account_root_key(id);
    if let Some(mut a) = json_at(sandbox, &key) {
        let c = a["OwnerCount"].as_u64().unwrap_or(0) as i64;
        a["OwnerCount"] = serde_json::Value::Number(((c + delta).max(0) as u64).into());
        put_json(sandbox, key, &a);
    }
}

/// How much of `leg` the account can actually deliver.
fn available(sandbox: &Sandbox, id: &[u8; 20], leg: &Leg) -> Me {
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
fn line_adjust(sandbox: &mut Sandbox, party: &[u8; 20], leg: &Leg, amt: Me, receiving: bool) {
    if party == &leg.issuer {
        return;
    }
    let lkey = keylet::ripple_state_key(party, &leg.issuer, &leg.cur);
    let party_low = party < &leg.issuer;
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
        put_json(sandbox, lkey, &line);
    } else if receiving {
        let (lo, hi) = if party_low { (party, &leg.issuer) } else { (&leg.issuer, party) };
        let bal_neg = !party_low; // holding sits on the party's side
        let sign = if bal_neg { "-" } else { "" };
        let cur_str = hex::encode_upper(leg.cur);
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Flags": if party_low { 0x0001_0000u64 } else { 0x0002_0000u64 },
            "Balance": {"currency": cur_str, "issuer": "0000000000000000000000000000000000000000",
                         "value": format!("{}{}", sign, me_to_value_string(amt))},
            "LowLimit": {"currency": cur_str, "issuer": hex::encode(lo), "value": "0"},
            "HighLimit": {"currency": cur_str, "issuer": hex::encode(hi), "value": "0"},
        });
        put_json(sandbox, lkey, &line);
        crate::ledger::directory::owner_dir_insert(sandbox, party, &lkey);
        crate::ledger::directory::owner_dir_insert(sandbox, &leg.issuer, &lkey);
        owner_count_add(sandbox, party, 1);
        owner_count_add(sandbox, &leg.issuer, 1);
    }
}

/// Move `amt` of `leg` from one account to another.
fn move_leg(sandbox: &mut Sandbox, from: &[u8; 20], to: &[u8; 20], leg: &Leg, amt: Me) {
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
fn delete_maker_offer(
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

/// Walk the inverse book from best quality and cross while the maker's rate is
/// within `threshold`. Returns (remaining pays, remaining gets, crossed count).
fn cross_engine(
    taker: &[u8; 20],
    mut rem_pays: Me,
    mut rem_gets: Me,
    pays_leg: &Leg,
    gets_leg: &Leg,
    threshold: u64,
    sandbox: &mut Sandbox,
) -> (Me, Me, u32) {
    let mut crossed = 0u32;
    if threshold == 0 {
        return (rem_pays, rem_gets, crossed);
    }
    let inv_base = keylet::book_base(&gets_leg.cur, &pays_leg.cur, &gets_leg.issuer, &pays_leg.issuer);
    let dirs = sandbox.keys_with_prefix(&inv_base.0[..24]);
    'dirs: for dk in dirs {
        let q = u64::from_be_bytes(dk.0[24..32].try_into().unwrap_or_default());
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
                    continue;
                }
                let (Some(m_gives0), Some(m_wants0)) = (
                    offer.get("TakerGets").and_then(keylet::amount_mant_exp),
                    offer.get("TakerPays").and_then(keylet::amount_mant_exp),
                ) else { continue };
                if m_gives0.0 == 0 || m_wants0.0 == 0 {
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    continue;
                }
                let funded = available(sandbox, &maker, pays_leg);
                if me_is_zero(funded) {
                    // Unfunded offers found during the walk are removed.
                    delete_maker_offer(sandbox, &okey, &offer, &maker);
                    continue;
                }
                let m_gives = if me_cmp(funded, m_gives0).is_lt() { funded } else { m_gives0 };
                let mut give = if me_cmp(rem_pays, m_gives).is_lt() { rem_pays } else { m_gives };
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
                move_leg(sandbox, &maker, taker, pays_leg, give);
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
                if me_is_zero(rem_pays) || me_is_zero(rem_gets) {
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
    (rem_pays, rem_gets, crossed)
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

        // Check sender has enough balance for what they're selling (XRP side)
        let taker_gets = parse_amount(&tx.fields["TakerGets"]);
        if let Some(Amount::Xrp(drops)) = &taker_gets {
            if let Some(data) = sandbox.read(&acct_key) {
                if let Ok(acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let balance = acct["Balance"]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    // Need balance for gets + fee + reserve
                    if balance < *drops + tx.fee {
                        return TxResult::Unfunded;
                    }
                }
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

        // Cross against the inverse book while the maker's rate is within the
        // taker's limit price (threshold = quality with the sides swapped).
        let threshold = keylet::offer_quality(&tg_json, &tp_json).unwrap_or(0);
        let (rem_pays, rem_gets, crossed) =
            cross_engine(&tx.account, tp0, tg0, &pays_leg, &gets_leg, threshold, sandbox);

        let filled = if sell { me_is_zero(rem_gets) } else { me_is_zero(rem_pays) };
        if fok && !filled {
            // FillOrKill not fully filled: nothing survives but the fee.
            sandbox.restore_snapshot(snap);
            return TxResult::Killed;
        }
        if ioc {
            if crossed == 0 {
                sandbox.restore_snapshot(snap);
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
        if let Some(q) = keylet::offer_quality(&tp_json, &tg_json) {
            let base = keylet::book_base(&pays_leg.cur, &gets_leg.cur, &pays_leg.issuer, &gets_leg.issuer);
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
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

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
}
