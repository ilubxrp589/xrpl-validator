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
pub const TF_SETF_AUTH: u64 = 0x0001_0000;
pub const TF_SET_NO_RIPPLE: u64 = 0x0002_0000;
pub const TF_CLEAR_NO_RIPPLE: u64 = 0x0004_0000;
pub const TF_SET_FREEZE: u64 = 0x0010_0000;
pub const TF_CLEAR_FREEZE: u64 = 0x0020_0000;
pub const TF_SET_DEEP_FREEZE: u64 = 0x0040_0000;
pub const TF_CLEAR_DEEP_FREEZE: u64 = 0x0080_0000;
pub const TF_LP_TOKEN: u64 = 0x0001_0000;
pub const TF_WITHDRAW_ALL: u64 = 0x0002_0000;
pub const TF_ONE_ASSET_WITHDRAW_ALL: u64 = 0x0004_0000;
pub const TF_SINGLE_ASSET: u64 = 0x0008_0000;
pub const TF_TWO_ASSET: u64 = 0x0010_0000;
pub const TF_ONE_ASSET_LP_TOKEN: u64 = 0x0020_0000;
pub const TF_LIMIT_LP_TOKEN: u64 = 0x0040_0000;
pub const TF_TWO_ASSET_IF_EMPTY: u64 = 0x0080_0000;
pub const TF_SELL_NFTOKEN: u64 = 0x0000_0001;

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
    /// The same asset at value zero (a TrustSet limit of 0 deletes the line
    /// when nothing else holds it; an amount of 0 is temBAD_AMOUNT in most
    /// preflights).
    Zero,
}

pub fn nudge_amount(v: &Value, how: Nudge) -> Option<Value> {
    if let Some(drops) = v.as_str().and_then(|s| s.parse::<u64>().ok()) {
        let n = match how {
            Nudge::Ulp(d) => (drops as i128 + d as i128).max(1) as u64,
            Nudge::Ppm(p) => ((drops as i128) + (drops as i128) * p as i128 / 1_000_000).max(1) as u64,
            Nudge::Halve => (drops / 2).max(1),
            Nudge::Double => drops.saturating_mul(2),
            Nudge::Zero => 0,
        };
        return Some(Value::String(n.to_string()));
    }
    let obj = v.as_object()?;
    if let Nudge::Zero = how {
        let mut out = obj.clone();
        out.insert("value".to_string(), Value::String("0".to_string()));
        return Some(Value::Object(out));
    }
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
        Nudge::Zero => unreachable!("handled above"),
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
pub fn candidates(tx: &Value, pct: u32, seq: u32) -> Vec<(String, Value)> {
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
        "TrustSet" => {
            // Set/clear pairs are exclusive in preflight (temINVALID_FLAG);
            // each toggle alone exercises the line's flag bookkeeping, the
            // freeze family the frozen-holder rules downstream.
            for (lbl, bit) in [
                ("flag:setfauth", TF_SETF_AUTH),
                ("flag:setnoripple", TF_SET_NO_RIPPLE),
                ("flag:clearnoripple", TF_CLEAR_NO_RIPPLE),
                ("flag:setfreeze", TF_SET_FREEZE),
                ("flag:clearfreeze", TF_CLEAR_FREEZE),
                ("flag:setdeepfreeze", TF_SET_DEEP_FREEZE),
                ("flag:cleardeepfreeze", TF_CLEAR_DEEP_FREEZE),
            ] {
                out.push((lbl.to_string(), with_flags(&base, bit)));
            }
            if let Some(a) = base.get("LimitAmount") {
                if let Some(z) = nudge_amount(a, Nudge::Zero) {
                    out.push(("limit:zero".to_string(), set_field(&base, "LimitAmount", z)));
                }
            }
            for (f, v) in [("QualityIn", 990_000_000u64), ("QualityOut", 1_010_000_000u64)] {
                out.push((format!("{}:set", f.to_lowercase()), set_field(&base, f, json!(v))));
            }
        }
        "AccountSet" => {
            // asf* values; every one a distinct preclaim/apply path.
            for asf in [1u64, 2, 3, 4, 6, 7, 8, 9, 10, 12, 13, 14, 15, 16] {
                out.push((format!("setflag:{asf}"), set_field(&drop_field_or(&base, "ClearFlag"), "SetFlag", json!(asf))));
                out.push((format!("clearflag:{asf}"), set_field(&drop_field_or(&base, "SetFlag"), "ClearFlag", json!(asf))));
            }
            for (lbl, v) in [
                ("transferrate:zero", 0u64),
                ("transferrate:one", 1_000_000_000),
                ("transferrate:1pct", 1_010_000_000),
                ("transferrate:max", 2_000_000_000),
                ("transferrate:overmax", 2_000_000_001),
            ] {
                out.push((lbl.to_string(), set_field(&base, "TransferRate", json!(v))));
            }
            for (lbl, v) in [("ticksize:zero", 0u64), ("ticksize:3", 3), ("ticksize:15", 15), ("ticksize:16", 16)] {
                out.push((lbl.to_string(), set_field(&base, "TickSize", json!(v))));
            }
        }
        "AMMDeposit" => {
            // Modes are exclusive: replace, do not toggle.
            for (lbl, bits) in [
                ("mode:lptoken", TF_LP_TOKEN),
                ("mode:singleasset", TF_SINGLE_ASSET),
                ("mode:twoasset", TF_TWO_ASSET),
                ("mode:oneassetlptoken", TF_ONE_ASSET_LP_TOKEN),
                ("mode:limitlptoken", TF_LIMIT_LP_TOKEN),
                ("mode:twoassetifempty", TF_TWO_ASSET_IF_EMPTY),
            ] {
                out.push((lbl.to_string(), set_field(&base, "Flags", json!(bits))));
            }
        }
        "AMMWithdraw" => {
            for (lbl, bits) in [
                ("mode:lptoken", TF_LP_TOKEN),
                ("mode:withdrawall", TF_WITHDRAW_ALL),
                ("mode:oneassetwithdrawall", TF_ONE_ASSET_WITHDRAW_ALL),
                ("mode:singleasset", TF_SINGLE_ASSET),
                ("mode:twoasset", TF_TWO_ASSET),
                ("mode:oneassetlptoken", TF_ONE_ASSET_LP_TOKEN),
                ("mode:limitlptoken", TF_LIMIT_LP_TOKEN),
            ] {
                out.push((lbl.to_string(), set_field(&base, "Flags", json!(bits))));
            }
        }
        "NFTokenCreateOffer" => {
            out.push(("flag:sellnftoken".to_string(), with_flags(&base, TF_SELL_NFTOKEN)));
            out.push(("expiration:past".to_string(), set_field(&base, "Expiration", json!(pct.saturating_sub(1)))));
            if let Some(v) = drop_field(&base, "Destination") {
                out.push(("destination:drop".to_string(), v));
            }
            if let Some(acct) = base.get("Account").cloned() {
                out.push(("destination:self".to_string(), set_field(&base, "Destination", acct)));
            }
        }
        "NFTokenAcceptOffer" => {
            if let Some(v) = drop_field(&base, "NFTokenBrokerFee") {
                out.push(("brokerfee:drop".to_string(), v));
            }
            for f in ["NFTokenSellOffer", "NFTokenBuyOffer"] {
                if let Some(v) = drop_field(&base, f) {
                    out.push((format!("{}:drop", f.to_lowercase()), v));
                }
            }
        }
        "OfferCancel" | "TicketCreate" => {
            let f = if tt == "OfferCancel" { "OfferSequence" } else { "TicketCount" };
            if let Some(n) = base.get(f).and_then(|v| v.as_u64()) {
                out.push((format!("{}:+1", f.to_lowercase()), set_field(&base, f, json!(n + 1))));
                if n > 0 {
                    out.push((format!("{}:-1", f.to_lowercase()), set_field(&base, f, json!(n - 1))));
                }
            }
        }
        "EscrowCreate" => {
            out.push(("finishafter:past".to_string(), set_field(&base, "FinishAfter", json!(pct.saturating_sub(1)))));
            out.push(("cancelafter:past".to_string(), set_field(&base, "CancelAfter", json!(pct.saturating_sub(1)))));
            if let Some(v) = drop_field(&base, "Condition") {
                out.push(("condition:drop".to_string(), v));
            }
        }
        "SignerListSet" => {
            if let Some(q) = base.get("SignerQuorum").and_then(|v| v.as_u64()) {
                out.push(("quorum:+1".to_string(), set_field(&base, "SignerQuorum", json!(q + 1))));
                out.push(("quorum:zero".to_string(), set_field(&drop_field_or(&base, "SignerEntries"), "SignerQuorum", json!(0))));
            }
        }
        _ => {}
    }
    // Every type: the sequence gates. A sequence one past the account's is
    // terPRE_SEQ, one before it tefPAST_SEQ; a LastLedgerSequence already
    // behind the ledger is tefMAX_LEDGER — each a distinct path in
    // preclaim/checkSeqProxy that no mainnet transaction ever takes.
    if base.get("TicketSequence").is_none() {
        if let Some(n) = base.get("Sequence").and_then(|v| v.as_u64()) {
            out.push(("seq:+1".to_string(), set_field(&base, "Sequence", json!(n + 1))));
            if n > 1 {
                out.push(("seq:-1".to_string(), set_field(&base, "Sequence", json!(n - 1))));
            }
        }
    }
    out.push(("lls:past".to_string(), set_field(&base, "LastLedgerSequence", json!(seq.saturating_sub(1)))));
    if let Some(fee) = base.get("Fee") {
        if let Some(nv) = nudge_amount(fee, Nudge::Double) {
            out.push(("fee:double".to_string(), set_field(&base, "Fee", nv)));
        }
        out.push(("fee:zero".to_string(), set_field(&base, "Fee", json!("0"))));
    }
    out
}

/// `drop_field` that hands the transaction back unchanged when the field is
/// absent.
fn drop_field_or(tx: &Value, name: &str) -> Value {
    drop_field(tx, name).unwrap_or_else(|| tx.clone())
}

/// K mutants sampled from the candidates, distinct operators when possible.
pub fn mutants(tx: &Value, pct: u32, seq: u32, rng: &mut Rng, k: usize) -> Vec<(String, Value)> {
    let mut cands = candidates(tx, pct, seq);
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
        let a = mutants(&tx, 842985542, 107_000_000, &mut Rng(7), 5);
        let b = mutants(&tx, 842985542, 107_000_000, &mut Rng(7), 5);
        assert_eq!(a.len(), 5);
        assert_eq!(a.iter().map(|x| x.0.clone()).collect::<Vec<_>>(), b.iter().map(|x| x.0.clone()).collect::<Vec<_>>());
        for (_, m) in &a {
            assert_eq!(m["SigningPubKey"], "");
            assert!(m.get("TxnSignature").is_none() && m.get("hash").is_none());
            assert_ne!(m, &unsign(&tx), "a mutant must differ from its base");
        }
        let labels: std::collections::HashSet<_> = a.iter().map(|x| x.0.clone()).collect();
        assert_eq!(labels.len(), 5, "distinct operators: {labels:?}");
        assert!(candidates(&tx, 0, 107_000_000).iter().any(|(l, _)| l == "flag:partial"));
        let labels: Vec<String> = candidates(&tx, 0, 107_000_000).into_iter().map(|(l, _)| l).collect();
        for want in ["seq:+1", "lls:past", "fee:zero"] {
            assert!(labels.iter().any(|l| l == want), "missing {want} in {labels:?}");
        }
        // Sequence 1 has no predecessor and a self-payment has no self mutant.
        assert!(!labels.iter().any(|l| l == "seq:-1" || l == "dest:self"), "{labels:?}");
        let mut tx5 = tx.clone();
        tx5["Sequence"] = json!(5);
        tx5["Destination"] = json!("rvYAfWj5gh67oV6fW32ZzP3Aw4Eubs59B");
        let l5: Vec<String> = candidates(&tx5, 0, 107_000_000).into_iter().map(|(l, _)| l).collect();
        for want in ["seq:-1", "dest:self"] {
            assert!(l5.iter().any(|l| l == want), "missing {want} in {l5:?}");
        }
        let lls = candidates(&tx, 0, 107_000_000).into_iter().find(|(l, _)| l == "lls:past").unwrap().1;
        assert_eq!(lls["LastLedgerSequence"], 106_999_999);
        let ts = json!({"TransactionType": "TrustSet", "Account": "rhTsmUJFpiju7syo8V5UbCQoaJjKWSvZju", "Fee": "12", "Sequence": 5,
            "LimitAmount": {"currency": "USD", "issuer": "rvYAfWj5gh67oV6fW32ZzP3Aw4Eubs59B", "value": "100"}});
        let tl: Vec<String> = candidates(&ts, 0, 1).into_iter().map(|(l, _)| l).collect();
        for want in ["flag:setfreeze", "flag:setdeepfreeze", "limit:zero", "qualityin:set"] {
            assert!(tl.iter().any(|l| l == want), "missing {want} in {tl:?}");
        }
        let z = candidates(&ts, 0, 1).into_iter().find(|(l, _)| l == "limit:zero").unwrap().1;
        assert_eq!(z["LimitAmount"]["value"], "0");
        let am = json!({"TransactionType": "AMMDeposit", "Account": "rhTsmUJFpiju7syo8V5UbCQoaJjKWSvZju", "Fee": "12", "Sequence": 5, "Flags": 1048576,
            "Asset": {"currency": "XRP"}, "Asset2": {"currency": "USD", "issuer": "rvYAfWj5gh67oV6fW32ZzP3Aw4Eubs59B"}, "Amount": "1000000"});
        let modes = candidates(&am, 0, 1).into_iter().find(|(l, _)| l == "mode:singleasset").unwrap().1;
        assert_eq!(modes["Flags"], 0x0008_0000);
        let acc = json!({"TransactionType": "AccountSet", "Account": "rhTsmUJFpiju7syo8V5UbCQoaJjKWSvZju", "Fee": "12", "Sequence": 5, "SetFlag": 8});
        let cf = candidates(&acc, 0, 1).into_iter().find(|(l, _)| l == "clearflag:7").unwrap().1;
        assert_eq!(cf["ClearFlag"], 7);
        assert!(cf.get("SetFlag").is_none(), "clearflag mutant must not carry SetFlag: {cf}");
    }
}
