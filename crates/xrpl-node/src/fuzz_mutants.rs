//! Mutation operators for the differential transaction fuzzer.
//!
//! The fuzzer (`differential_probe --fuzz K`) takes every real transaction of
//! a fixture ledger, derives K mutants of it, and applies each with libxrpl
//! and with our engine on the same pre-state. This module is the pure half:
//! given a transaction's JSON (API form, base58 addresses) it returns
//! labelled variants. Every variant is UNSIGNED — `TxnSignature` and
//! `Signers` removed, `SigningPubKey` emptied — which libxrpl accepts under
//! `tapDRY_RUN` (Transactor.cpp:719, the `simulate` path) and our engine
//! never checked. Deterministic for a seed, so a disagreement is reproducible
//! from (fixture, seed, tx index, mutant index) alone.
use serde_json::{json, Value};
use xrpl_ledger::ledger::keylet::amount_mant_exp;

/// xorshift64 — the same generator arith_fuzz uses.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() % items.len() as u64) as usize]
    }
    pub fn chance(&mut self, one_in: u64) -> bool {
        self.next() % one_in == 0
    }
}

pub const TF_PARTIAL_PAYMENT: u64 = 0x0002_0000;
pub const TF_NO_RIPPLE_DIRECT: u64 = 0x0001_0000;
pub const TF_LIMIT_QUALITY: u64 = 0x0004_0000;
pub const TF_PASSIVE: u64 = 0x0001_0000;
pub const TF_IMMEDIATE_OR_CANCEL: u64 = 0x0002_0000;
pub const TF_FILL_OR_KILL: u64 = 0x0004_0000;
pub const TF_SELL: u64 = 0x0008_0000;

/// Render a canonical IOU (mantissa, exponent) as the decimal string the API
/// uses: no exponent notation, no trailing zeros, "0" for zero.
pub fn iou_string(mant: u128, exp: i32) -> String {
    if mant == 0 {
        return "0".to_string();
    }
    let digits = mant.to_string();
    let mut s = if exp >= 0 {
        format!("{digits}{}", "0".repeat(exp as usize))
    } else {
        let point = digits.len() as i32 + exp;
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else {
            let (a, b) = digits.split_at(point as usize);
            format!("{a}.{b}")
        }
    };
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Nudge an amount: `Ulp(±1)` moves the last of the 16 digits (or one
/// drop); `Ppm(±n)` scales by (1 ± n·1e-6); `Halve`/`Double` are coarse.
#[derive(Clone, Copy, Debug)]
pub enum Nudge {
    Ulp(i8),
    Ppm(i32),
    Halve,
    Double,
}

pub fn nudge_amount(v: &Value, how: Nudge) -> Option<Value> {
    if let Some(drops) = v.as_str().and_then(|s| s.parse::<u64>().ok()) {
        let n = match how {
            Nudge::Ulp(d) => (drops as i128 + d as i128).max(1) as u64,
            Nudge::Ppm(p) => ((drops as i128) + (drops as i128) * p as i128 / 1_000_000).max(1) as u64,
            Nudge::Halve => (drops / 2).max(1),
            Nudge::Double => drops.saturating_mul(2),
        };
        return Some(Value::String(n.to_string()));
    }
    let obj = v.as_object()?;
    let (mant, exp) = amount_mant_exp(v)?;
    if mant == 0 {
        return None;
    }
    // `amount_mant_exp` keeps the digits as written; the ledger's canonical
    // form is a 16-digit mantissa, and an ulp is the last of THOSE digits.
    let (mut m, mut e) = (mant, exp);
    while m < 1_000_000_000_000_000 {
        m *= 10;
        e -= 1;
    }
    while m >= 10_000_000_000_000_000 {
        m /= 10;
        e += 1;
    }
    match how {
        Nudge::Ulp(d) => {
            if d > 0 {
                m += 1;
                if m >= 10_000_000_000_000_000 {
                    m /= 10;
                    e += 1;
                }
            } else {
                m -= 1;
                if m < 1_000_000_000_000_000 {
                    m = 9_999_999_999_999_999;
                    e -= 1;
                }
            }
        }
        Nudge::Ppm(p) => {
            let delta = (m as i128) * (p as i128) / 1_000_000;
            let n = (m as i128 + delta).max(1) as u128;
            m = n;
            while m >= 10_000_000_000_000_000 {
                m /= 10;
                e += 1;
            }
            while m < 1_000_000_000_000_000 {
                m *= 10;
                e -= 1;
            }
        }
        Nudge::Halve => {
            m /= 2;
            while m < 1_000_000_000_000_000 {
                m *= 10;
                e -= 1;
            }
        }
        Nudge::Double => {
            m *= 2;
            while m >= 10_000_000_000_000_000 {
                m /= 10;
                e += 1;
            }
        }
    }
    let mut out = obj.clone();
    out.insert("value".to_string(), Value::String(iou_string(m, e)));
    Some(Value::Object(out))
}

fn unsign(tx: &Value) -> Value {
    let mut v = tx.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("TxnSignature");
        o.remove("Signers");
        o.remove("hash");
        o.remove("metaData");
        o.remove("meta");
        o.insert("SigningPubKey".to_string(), Value::String(String::new()));
    }
    v
}

fn with_flags(tx: &Value, xor: u64) -> Value {
    let mut v = tx.clone();
    let f = v.get("Flags").and_then(|x| x.as_u64()).unwrap_or(0);
    v["Flags"] = json!(f ^ xor);
    v
}

fn set_field(tx: &Value, name: &str, val: Value) -> Value {
    let mut v = tx.clone();
    v[name] = val;
    v
}

fn drop_field(tx: &Value, name: &str) -> Option<Value> {
    let mut v = tx.clone();
    v.as_object_mut()?.remove(name)?;
    Some(v)
}

/// The candidate operators for one transaction, each a (label, mutant)
/// pair. `pct` is the parent ledger's close time (for Expiration). The
/// caller samples K of them with the seeded generator.
pub fn candidates(tx: &Value, pct: u32) -> Vec<(String, Value)> {
    let base = unsign(tx);
    let tt = base.get("TransactionType").and_then(|v| v.as_str()).unwrap_or("");
    let mut out: Vec<(String, Value)> = Vec::new();
    let nudges = [
        ("ulp+", Nudge::Ulp(1)),
        ("ulp-", Nudge::Ulp(-1)),
        ("ppm+1", Nudge::Ppm(1)),
        ("ppm-1", Nudge::Ppm(-1)),
        ("ppm+100", Nudge::Ppm(100)),
        ("halve", Nudge::Halve),
        ("double", Nudge::Double),
    ];
    let amount_fields: &[&str] = match tt {
        "Payment" => &["Amount", "SendMax", "DeliverMin"],
        "OfferCreate" => &["TakerPays", "TakerGets"],
        "CheckCash" => &["Amount", "DeliverMin"],
        "CheckCreate" => &["SendMax"],
        "EscrowCreate" | "PaymentChannelCreate" | "PaymentChannelFund" => &["Amount"],
        "AMMDeposit" | "AMMWithdraw" => &["Amount", "Amount2", "LPTokenOut", "LPTokenIn"],
        "Clawback" => &["Amount"],
        "TrustSet" => &["LimitAmount"],
        _ => &[],
    };
    for f in amount_fields {
        if let Some(a) = base.get(*f) {
            for (lbl, n) in nudges {
                if let Some(nv) = nudge_amount(a, n) {
                    out.push((format!("{f}:{lbl}"), set_field(&base, f, nv)));
                }
            }
        }
    }
    match tt {
        "Payment" => {
            for (lbl, bit) in [
                ("flag:partial", TF_PARTIAL_PAYMENT),
                ("flag:norippledirect", TF_NO_RIPPLE_DIRECT),
                ("flag:limitquality", TF_LIMIT_QUALITY),
            ] {
                out.push((lbl.to_string(), with_flags(&base, bit)));
            }
            if base.get("DeliverMin").is_none() {
                if let Some(a) = base.get("Amount") {
                    if let Some(half) = nudge_amount(a, Nudge::Halve) {
                        out.push((
                            "delivermin:half+partial".to_string(),
                            set_field(&with_flags(&base, TF_PARTIAL_PAYMENT), "DeliverMin", half),
                        ));
                    }
                }
            } else if let Some(v) = drop_field(&base, "DeliverMin") {
                out.push(("delivermin:drop".to_string(), v));
            }
            if let Some(v) = drop_field(&base, "Paths") {
                out.push(("paths:drop".to_string(), v));
            }
            if let Some(acct) = base.get("Account").cloned() {
                if base.get("Destination") != Some(&acct) && base.get("SendMax").is_some() {
                    out.push(("dest:self".to_string(), set_field(&base, "Destination", acct)));
                }
            }
        }
        "OfferCreate" => {
            for (lbl, bit) in [
                ("flag:sell", TF_SELL),
                ("flag:ioc", TF_IMMEDIATE_OR_CANCEL),
                ("flag:fok", TF_FILL_OR_KILL),
                ("flag:passive", TF_PASSIVE),
            ] {
                out.push((lbl.to_string(), with_flags(&base, bit)));
            }
            out.push(("expiration:past".to_string(), set_field(&base, "Expiration", json!(pct.saturating_sub(1)))));
            out.push(("expiration:future".to_string(), set_field(&base, "Expiration", json!(pct + 3600))));
            if let Some(v) = drop_field(&base, "OfferSequence") {
                out.push(("offersequence:drop".to_string(), v));
            }
        }
        _ => {}
    }
    if let Some(fee) = base.get("Fee") {
        if let Some(nv) = nudge_amount(fee, Nudge::Double) {
            out.push(("fee:double".to_string(), set_field(&base, "Fee", nv)));
        }
    }
    out
}

/// K mutants sampled from the candidates, distinct operators when possible.
pub fn mutants(tx: &Value, pct: u32, rng: &mut Rng, k: usize) -> Vec<(String, Value)> {
    let mut cands = candidates(tx, pct);
    let mut out = Vec::new();
    while !cands.is_empty() && out.len() < k {
        let i = (rng.next() % cands.len() as u64) as usize;
        out.push(cands.swap_remove(i));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iou_strings_round_trip_the_canonical_form() {
        assert_eq!(iou_string(1_177_327_080_000_000, -13), "117.732708");
        assert_eq!(iou_string(4_340_278_009_960_062, -10), "434027.8009960062");
        assert_eq!(iou_string(1_000_000_000_000_000, -15), "1");
        assert_eq!(iou_string(2_150_833_293_491_000, -31), "0.0000000000000002150833293491");
        assert_eq!(iou_string(5_000_000_000_000_000, -13), "500");
    }

    #[test]
    fn ulp_nudges_move_the_last_digit_and_carry() {
        let a = json!({"currency": "USD", "issuer": "rrrrrrrrrrrrrrrrrrrrBZbvji", "value": "117.732708"});
        assert_eq!(nudge_amount(&a, Nudge::Ulp(1)).unwrap()["value"], "117.7327080000001");
        assert_eq!(nudge_amount(&a, Nudge::Ulp(-1)).unwrap()["value"], "117.7327079999999");
        let top = json!({"currency": "USD", "issuer": "rrrrrrrrrrrrrrrrrrrrBZbvji", "value": "9.999999999999999"});
        assert_eq!(nudge_amount(&top, Nudge::Ulp(1)).unwrap()["value"], "10");
        assert_eq!(nudge_amount(&json!("6000000"), Nudge::Ulp(-1)).unwrap(), "5999999");
        assert_eq!(nudge_amount(&json!("6000000"), Nudge::Ppm(100)).unwrap(), "6000600");
        assert_eq!(nudge_amount(&a, Nudge::Halve).unwrap()["value"], "58.866354");
    }

    #[test]
    fn mutants_are_unsigned_and_deterministic() {
        let tx = json!({
            "TransactionType": "Payment", "Account": "rhTsmUJFpiju7syo8V5UbCQoaJjKWSvZju",
            "Destination": "rhTsmUJFpiju7syo8V5UbCQoaJjKWSvZju", "Amount": {"currency": "RVR", "issuer": "rPmtwX9uB4UnU1WxvFwufSVHF1iNaQXrKN", "value": "499998178.2856869"},
            "SendMax": "6000000", "Fee": "12", "Flags": 131072, "Sequence": 1, "SigningPubKey": "ED01", "TxnSignature": "AB", "hash": "X"
        });
        let a = mutants(&tx, 842985542, &mut Rng(7), 5);
        let b = mutants(&tx, 842985542, &mut Rng(7), 5);
        assert_eq!(a.len(), 5);
        assert_eq!(a.iter().map(|x| x.0.clone()).collect::<Vec<_>>(), b.iter().map(|x| x.0.clone()).collect::<Vec<_>>());
        for (_, m) in &a {
            assert_eq!(m["SigningPubKey"], "");
            assert!(m.get("TxnSignature").is_none() && m.get("hash").is_none());
            assert_ne!(m, &unsign(&tx), "a mutant must differ from its base");
        }
        let labels: std::collections::HashSet<_> = a.iter().map(|x| x.0.clone()).collect();
        assert_eq!(labels.len(), 5, "distinct operators: {labels:?}");
        assert!(candidates(&tx, 0).iter().any(|(l, _)| l == "flag:partial"));
    }
}
