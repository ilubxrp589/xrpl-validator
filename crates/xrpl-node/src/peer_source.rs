//! Independence Step 4, stage A: a ledger the engine could take from peers alone.
//!
//! Today the engine applies each ledger from .39's copy: `ledger` with `expand`, every transaction as rippled's
//! JSON with its `metaData`, put in TransactionIndex order, and the header's fields, with the state root read
//! separately (ws-sync `fetch_ledger_metadata`, `fetch_account_hash`). Step 4 takes the same ledger from peers
//! ([`crate::peer_fetch`]: the 118-byte header and the transaction tree, both proven against the ledger hash).
//! [`assemble`] turns that binary copy into exactly those inputs:
//! - the header ws-sync uses, and the state root, both from the proven header;
//! - each transaction as rippled's JSON: its fields and its `hash`, with its `metaData` and the result named as
//!   rippled names it, in TransactionIndex order;
//! - the binary pairs in the same order.
//!
//! The codec skips a field it does not know, so a transaction or record that does not encode back to its own
//! bytes is refused rather than handed on short. Where rippled's JSON spells a value differently from the codec,
//! it is respelled rippled's way ([`respell`]): the engine reads these spellings (a path step's `type` decides
//! its kind). Stage A changes nothing live: [`compare`] holds the inputs made
//! from the peers' copy against .39's, field by field, for every ledger peer_fetch completes.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde_json::{json, Value};
use xrpl_ledger::ledger::header::LedgerHeader as Header;

use crate::ws_sync::LedgerHeader;

/// A ledger's transactions as the tree holds them: each transaction's bytes and its record's.
pub type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

/// One ledger's engine inputs, made from a binary copy.
#[derive(Debug, Clone)]
pub struct Assembled {
    pub header: LedgerHeader,
    pub account_hash: [u8; 32],
    /// rippled's JSON for each transaction, with `hash` and `metaData`, in TransactionIndex order.
    pub txs: Vec<Value>,
    /// The transaction and record bytes, in the same order.
    pub pairs: Pairs,
}

/// Fields rippled's JSON adds to a transaction that its bytes do not hold. The engine reads none of them
/// (it computes what it delivered itself, and `DeliverMax` is the API's second name for `Amount`).
pub const ADDED_TO_TX: &[&str] = &["DeliverMax", "ctid", "date", "inLedger", "ledger_index"];
/// The same for a transaction's `metaData`.
pub const ADDED_TO_META: &[&str] = &["delivered_amount", "nftoken_id", "nftoken_ids", "offer_id", "mpt_issuance_id"];

/// UInt64 fields rippled's JSON spells as hex without leading zeros, in lower case (`STUInt64::getJson`); the
/// codec pads them to 16 upper-case digits. The four it spells in base 10 (`kSmdBaseTen`) are left as they are.
static DEFS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../XRPL Rust SDK/tests/vectors/definitions.json")).unwrap_or_default()
});

static UINT64_HEX: LazyLock<HashSet<String>> = LazyLock::new(|| {
    const BASE_TEN: &[&str] = &["MaximumAmount", "OutstandingAmount", "MPTAmount", "LockedAmount"];
    DEFS["FIELDS"]
        .as_array()
        .map(|f| {
            f.iter()
                .filter(|x| x[1]["type"] == "UInt64")
                .filter_map(|x| x[0].as_str())
                .filter(|n| !BASE_TEN.contains(n))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
});

/// `PermissionValue` as rippled's JSON names it (`STUInt32::getJson` → `Permission::getPermissionName`): a
/// transaction type's code plus one, or a granular permission (permissions.macro, 65537 on). The codec gives the
/// number: mainnet #107804444 4A242D3BBE51 (DelegateSet), ours 28, .39's "NFTokenCreateOffer".
static PERMISSION_NAMES: LazyLock<HashMap<u64, String>> = LazyLock::new(|| {
    let mut m: HashMap<u64, String> = DEFS["TRANSACTION_TYPES"]
        .as_object()
        .map(|t| t.iter().filter_map(|(n, c)| c.as_u64().map(|c| (c + 1, n.clone()))).collect())
        .unwrap_or_default();
    for (i, n) in [
        "TrustlineAuthorize", "TrustlineFreeze", "TrustlineUnfreeze", "AccountDomainSet", "AccountEmailHashSet",
        "AccountMessageKeySet", "AccountTransferRateSet", "AccountTickSizeSet", "PaymentMint", "PaymentBurn",
        "MPTokenIssuanceLock", "MPTokenIssuanceUnlock",
    ]
    .into_iter()
    .enumerate()
    {
        m.insert(65_537 + i as u64, n.to_string());
    }
    m
});

/// The codec's JSON respelled as rippled's: UInt64 hex short and in lower case, each path step with its
/// `type` (account 0x01, currency 0x10, issuer 0x20; `STPathSet::getJson`), which the bytes hold as flags, and
/// `PermissionValue` by name.
pub fn respell(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                if k == "Paths" {
                    for step in x.as_array_mut().into_iter().flatten().filter_map(Value::as_array_mut).flatten() {
                        if let Some(e) = step.as_object_mut() {
                            let t = u64::from(e.contains_key("account"))
                                | (if e.contains_key("currency") { 0x10 } else { 0 })
                                | (if e.contains_key("issuer") { 0x20 } else { 0 });
                            e.insert("type".into(), json!(t));
                        }
                    }
                } else if k == "PermissionValue" {
                    if let Some(name) = x.as_u64().and_then(|n| PERMISSION_NAMES.get(&n)) {
                        *x = json!(name);
                    }
                } else if UINT64_HEX.contains(k.as_str()) {
                    if let Some(h) = x.as_str().filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
                        let short = h.trim_start_matches('0').to_ascii_lowercase();
                        *x = json!(if short.is_empty() { "0".to_string() } else { short });
                    }
                }
                respell(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(respell),
        _ => {}
    }
}

/// Decode a transaction or a record, refusing one that does not encode back to its own bytes.
fn decode_exact(blob: &[u8], what: &str) -> Result<Value, String> {
    let v = xrpl_core::codec::decode::decode_transaction_binary(blob).map_err(|e| format!("{what}: decode {e:?}"))?;
    let back = crate::meta_check::encode(&v).map_err(|e| format!("{what}: {e}"))?;
    if back != blob {
        return Err(format!("{what}: does not encode back to its own bytes (a field the codec does not know?)"));
    }
    Ok(v)
}

/// The engine's inputs for a ledger, from its proven header and its transaction tree's (transaction, record)
/// pairs in any order.
pub fn assemble(header: &Header, pairs: Pairs) -> Result<Assembled, String> {
    if xrpl_ledger::shamap::tx_tree::compute_tx_tree_root(&pairs) != header.transaction_hash {
        return Err("the transactions do not make the header's transaction root".into());
    }
    let mut rows = Vec::with_capacity(pairs.len());
    for (tx_blob, meta_blob) in pairs {
        let id = hex::encode_upper(xrpl_ledger::shamap::tx_tree::tx_id(&tx_blob).0);
        let short = &id[..16];
        let mut tx = decode_exact(&tx_blob, &format!("tx {short}"))?;
        let mut meta = decode_exact(&meta_blob, &format!("record of {short}"))?;
        let code = meta["TransactionResult"].as_u64().ok_or_else(|| format!("record of {short}: no TransactionResult"))?;
        let name = crate::meta_check::result_name(code).ok_or_else(|| format!("record of {short}: result {code} has no name"))?;
        meta["TransactionResult"] = json!(name);
        respell(&mut tx);
        respell(&mut meta);
        let index = meta["TransactionIndex"].as_u64().ok_or_else(|| format!("record of {short}: no TransactionIndex"))?;
        tx["hash"] = json!(id);
        tx["metaData"] = meta;
        rows.push((index, tx, (tx_blob, meta_blob)));
    }
    rows.sort_by_key(|r| r.0);
    if let Some((i, r)) = rows.iter().enumerate().find(|(i, r)| r.0 != *i as u64) {
        return Err(format!("TransactionIndex {} where {i} belongs", r.0));
    }
    let (txs, pairs) = rows.into_iter().map(|(_, tx, p)| (tx, p)).unzip();
    Ok(Assembled {
        header: LedgerHeader {
            parent_hash: header.parent_hash.0,
            parent_close_time: header.parent_close_time,
            total_drops: header.total_coins,
            transaction_hash: header.transaction_hash.0,
            close_time: header.close_time,
            close_resolution: header.close_time_resolution,
            close_flags: header.close_flags,
            ledger_hash: header.hash().0,
        },
        account_hash: header.account_hash.0,
        txs,
        pairs,
    })
}

/// .39's copy in the same form, from its `ledger` reply (`transactions`, `expand`, not binary): the header,
/// the state root (hex) and the transactions in TransactionIndex order, read as ws-sync reads them.
pub fn their_inputs(ledger: &Value) -> Result<(LedgerHeader, String, Vec<Value>), String> {
    let h32 = |k: &str| -> Result<[u8; 32], String> {
        let b = hex::decode(ledger[k].as_str().ok_or_else(|| format!("no {k}"))?).map_err(|e| format!("{k}: {e}"))?;
        <[u8; 32]>::try_from(b.as_slice()).map_err(|_| format!("{k}: not 32 bytes"))
    };
    let n = |k: &str| ledger[k].as_u64().ok_or_else(|| format!("no {k}"));
    let header = LedgerHeader {
        parent_hash: h32("parent_hash")?,
        parent_close_time: n("parent_close_time")? as u32,
        total_drops: ledger["total_coins"].as_str().and_then(|s| s.parse().ok()).ok_or("no total_coins")?,
        transaction_hash: h32("transaction_hash")?,
        close_time: n("close_time")? as u32,
        close_resolution: n("close_time_resolution")? as u8,
        close_flags: n("close_flags")? as u8,
        ledger_hash: h32("ledger_hash")?,
    };
    let account = ledger["account_hash"].as_str().ok_or("no account_hash")?.to_string();
    let mut txs = ledger["transactions"].as_array().ok_or("no transactions")?.clone();
    txs.sort_by_key(|t| meta_of(t)["TransactionIndex"].as_u64().unwrap_or(0));
    Ok((header, account, txs))
}

fn meta_of(t: &Value) -> &Value {
    if t["metaData"].is_object() {
        &t["metaData"]
    } else {
        &t["meta"]
    }
}

/// The inputs made from the peers' copy against .39's: None when the engine would read the same thing from
/// both, else the first difference, in words. `account_hash` may be empty (not read).
pub fn compare(ours: &Assembled, header: &LedgerHeader, account_hash: &str, txs: &[Value]) -> Option<String> {
    differences(ours, header, account_hash, txs, 1).into_iter().next()
}

/// Up to `max` differences between the two, in words (empty: the same).
pub fn differences(ours: &Assembled, header: &LedgerHeader, account_hash: &str, txs: &[Value], max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let (o, t) = (&ours.header, header);
    for (name, same) in [
        ("ledger hash", o.ledger_hash == t.ledger_hash),
        ("parent hash", o.parent_hash == t.parent_hash),
        ("parent close time", o.parent_close_time == t.parent_close_time),
        ("total XRP", o.total_drops == t.total_drops),
        ("transaction root", o.transaction_hash == t.transaction_hash),
        ("close time", o.close_time == t.close_time),
        ("close time resolution", o.close_resolution == t.close_resolution),
        ("close flags", o.close_flags == t.close_flags),
    ] {
        if !same {
            out.push(format!("header: the {name} differs"));
        }
    }
    if !account_hash.is_empty() && !account_hash.eq_ignore_ascii_case(&hex::encode(ours.account_hash)) {
        out.push("header: the state root differs".into());
    }
    if ours.txs.len() != txs.len() {
        out.push(format!("{} transactions, .39 has {}", ours.txs.len(), txs.len()));
        return out.into_iter().take(max).collect();
    }
    for (i, (o, t)) in ours.txs.iter().zip(txs).enumerate() {
        if out.len() >= max {
            break;
        }
        let theirs = as_read(t);
        let id = o["hash"].as_str().unwrap_or("?");
        let mut found = Vec::new();
        collect_differences(o, &theirs, "", &mut found, max - out.len());
        out.extend(found.into_iter().map(|d| format!("tx {i} ({}…) {d}", &id[..id.len().min(16)])));
    }
    out.into_iter().take(max).collect()
}

/// .39's transaction as the engine could read it from the bytes alone: `metaData` (not `meta`), without the
/// fields rippled's JSON adds.
fn as_read(t: &Value) -> Value {
    let mut t = t.clone();
    if let Some(o) = t.as_object_mut() {
        if let Some(m) = o.remove("meta") {
            o.entry("metaData").or_insert(m);
        }
        for k in ADDED_TO_TX {
            o.remove(*k);
        }
        if let Some(m) = o.get_mut("metaData").and_then(Value::as_object_mut) {
            for k in ADDED_TO_META {
                m.remove(*k);
            }
        }
    }
    t
}

/// Where two JSON values differ, as paths (`metaData.AffectedNodes[2].ModifiedNode.FinalFields.Balance`), up to
/// `max` of them.
fn collect_differences(o: &Value, t: &Value, path: &str, out: &mut Vec<String>, max: usize) {
    if out.len() >= max || o == t {
        return;
    }
    let at = |k: &str| if path.is_empty() { k.to_string() } else { format!("{path}.{k}") };
    let show = |v: &Value| {
        let s = v.to_string();
        if s.len() > 80 { format!("{}…", &s[..s.char_indices().nth(80).map_or(s.len(), |(i, _)| i)]) } else { s }
    };
    match (o, t) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                if out.len() >= max {
                    return;
                }
                match (a.get(k), b.get(k)) {
                    (Some(x), None) => out.push(format!("{}: ours only ({})", at(k), show(x))),
                    (None, Some(y)) => out.push(format!("{}: .39's only ({})", at(k), show(y))),
                    (Some(x), Some(y)) => collect_differences(x, y, &at(k), out, max),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                collect_differences(x, y, &format!("{path}[{i}]"), out, max);
            }
        }
        (Value::Array(a), Value::Array(b)) => out.push(format!("{path}: {} items, .39's has {}", a.len(), b.len())),
        _ => out.push(format!("{path}: ours {}, .39's {}", show(o), show(t))),
    }
}

/// A small ledger in bytes, and .39's JSON of it, for tests here and in peer_fetch.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use xrpl_core::types::Hash256;

    pub fn payment(seq: u32) -> Value {
        json!({
            "TransactionType": "Payment",
            "Account": "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh",
            "Destination": "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe",
            "Amount": "1000000",
            "Fee": "12",
            "Flags": 2147483648u64,
            "Sequence": seq,
            "SigningPubKey": "0330E7FC9D56BB25D6893BA3F317AE5BCF33B3291BD63DB32654A313222F7FD020",
            "TxnSignature": "3045022100D184EB4AE5956FF600E7536EE459345C7BBCF097A84CC61A93B9AF7197EDB98702201CEA8009B7BEEBAA2AACC0359B41C427C1C5B550A4CA4B80CF2174AF2D6D5DCE",
        })
    }

    pub fn record(index: u64, result: u64) -> Value {
        json!({"TransactionIndex": index, "TransactionResult": result, "AffectedNodes": []})
    }

    pub fn pair(seq: u32, index: u64, result: u64) -> (Vec<u8>, Vec<u8>) {
        (crate::meta_check::encode(&payment(seq)).unwrap(), crate::meta_check::encode(&record(index, result)).unwrap())
    }

    pub fn header_for(pairs: &[(Vec<u8>, Vec<u8>)]) -> Header {
        Header {
            sequence: 107_790_000,
            total_coins: 99_985_000_000_000_000,
            parent_hash: Hash256([7; 32]),
            transaction_hash: xrpl_ledger::shamap::tx_tree::compute_tx_tree_root(pairs),
            account_hash: Hash256([9; 32]),
            parent_close_time: 814_000_000,
            close_time: 814_000_004,
            close_time_resolution: 10,
            close_flags: 0,
        }
    }

    /// What .39 sends for the same two transactions: rippled's JSON, out of order, with the fields it adds.
    pub fn theirs(a: &Assembled) -> (LedgerHeader, String, Vec<Value>) {
        let mut txs: Vec<Value> = a.txs.iter().rev().cloned().collect();
        for t in &mut txs {
            t["DeliverMax"] = t["Amount"].clone();
            let m = t.as_object_mut().unwrap().remove("metaData").unwrap();
            t["meta"] = m;
            t["meta"]["delivered_amount"] = json!("1000000");
        }
        let mut ledger = json!({
            "parent_hash": hex::encode_upper(a.header.parent_hash), "parent_close_time": a.header.parent_close_time,
            "total_coins": a.header.total_drops.to_string(), "transaction_hash": hex::encode_upper(a.header.transaction_hash),
            "close_time": a.header.close_time, "close_time_resolution": a.header.close_resolution,
            "close_flags": a.header.close_flags, "ledger_hash": hex::encode_upper(a.header.ledger_hash),
            "account_hash": hex::encode_upper(a.account_hash),
        });
        ledger["transactions"] = json!(txs);
        their_inputs(&ledger).unwrap()
    }

    /// Two payments, filed out of order.
    pub fn ledger() -> (Header, Pairs) {
        let pairs = vec![pair(5, 1, 128), pair(4, 0, 0)];
        (header_for(&pairs), pairs)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use xrpl_core::types::Hash256;

    #[test]
    fn a_ledger_from_its_bytes_is_what_the_engine_reads_from_39s_json() {
        let pairs = vec![pair(5, 1, 128), pair(4, 0, 0)];
        let h = header_for(&pairs);
        let a = assemble(&h, pairs).unwrap();
        assert_eq!(a.txs.len(), 2);
        assert_eq!(a.txs[0]["Sequence"], 4, "TransactionIndex order");
        assert_eq!(a.txs[0]["metaData"]["TransactionResult"], "tesSUCCESS");
        assert_eq!(a.txs[1]["metaData"]["TransactionResult"], "tecPATH_DRY", "the result by its name");
        assert_eq!(a.txs[0]["hash"], hex::encode_upper(xrpl_ledger::shamap::tx_tree::tx_id(&a.pairs[0].0).0));
        assert_eq!(a.header.ledger_hash, h.hash().0);
        assert_eq!(a.account_hash, [9; 32]);
        let (th, ta, tt) = theirs(&a);
        assert_eq!(compare(&a, &th, &ta, &tt), None, "the fields rippled adds are not read");
    }

    #[test]
    fn a_difference_is_named_by_where_it_is() {
        let pairs = vec![pair(4, 0, 0), pair(5, 1, 0)];
        let a = assemble(&header_for(&pairs), pairs).unwrap();
        let (th, ta, mut tt) = theirs(&a);
        tt[1]["Fee"] = json!("15");
        assert_eq!(compare(&a, &th, &ta, &tt).unwrap(), format!("tx 1 ({}…) Fee: ours \"12\", .39's \"15\"", &a.txs[1]["hash"].as_str().unwrap()[..16]));
        let (mut th, ta, tt) = theirs(&a);
        th.close_time += 1;
        assert_eq!(compare(&a, &th, &ta, &tt).unwrap(), "header: the close time differs");
        let (th, _, tt) = theirs(&a);
        assert_eq!(compare(&a, &th, &"00".repeat(32), &tt).unwrap(), "header: the state root differs");
    }

    #[test]
    fn values_are_spelled_as_rippleds_json_spells_them() {
        let mut v = json!({
            "OwnerNode": "0000000000000E30", "IndexNext": "0000000000000000", "ExchangeRate": "4F205ADBF46F1800",
            "MaximumAmount": "100", "Sequence": 5,
            "Paths": [[{"currency": "USD", "issuer": "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe"}, {"account": "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe"}]],
            "AffectedNodes": [{"ModifiedNode": {"FinalFields": {"HighNode": "000000000000032F"}}}],
            "Permissions": [{"Permission": {"PermissionValue": 28}}, {"Permission": {"PermissionValue": 65_537}},
                            {"Permission": {"PermissionValue": 70_000}}],
        });
        respell(&mut v);
        let names: Vec<&Value> = (0..3).map(|i| &v["Permissions"][i]["Permission"]["PermissionValue"]).collect();
        assert_eq!(names, [&json!("NFTokenCreateOffer"), &json!("TrustlineAuthorize"), &json!(70_000)], "#107804444");
        assert_eq!(v["OwnerNode"], "e30");
        assert_eq!(v["IndexNext"], "0");
        assert_eq!(v["ExchangeRate"], "4f205adbf46f1800");
        assert_eq!(v["MaximumAmount"], "100", "base 10 stays");
        assert_eq!(v["Paths"][0][0]["type"], 48);
        assert_eq!(v["Paths"][0][1]["type"], 1);
        assert_eq!(v["AffectedNodes"][0]["ModifiedNode"]["FinalFields"]["HighNode"], "32f", "inside records too");
    }

    #[test]
    fn every_difference_is_listed_up_to_the_cap() {
        let pairs = vec![pair(4, 0, 0), pair(5, 1, 0)];
        let a = assemble(&header_for(&pairs), pairs).unwrap();
        let (th, ta, mut tt) = theirs(&a);
        tt[0]["Fee"] = json!("15");
        tt[1]["Fee"] = json!("16");
        tt[1]["Sequence"] = json!(9);
        assert_eq!(differences(&a, &th, &ta, &tt, 10).len(), 3);
        assert_eq!(differences(&a, &th, &ta, &tt, 2).len(), 2);
    }

    #[test]
    fn a_copy_that_is_not_the_headers_or_the_codecs_is_refused() {
        let pairs = vec![pair(4, 0, 0)];
        let mut h = header_for(&pairs);
        h.transaction_hash = Hash256([1; 32]);
        assert!(assemble(&h, pairs.clone()).unwrap_err().contains("transaction root"));
        let mut bad = pairs[0].1.clone();
        bad.extend_from_slice(&[0x20, 0xC8, 0, 0, 0, 1]); // a UInt32 field the codec's definitions lack (type 2, field 200)
        let odd = vec![(pairs[0].0.clone(), bad)];
        let err = assemble(&header_for(&odd), odd).unwrap_err();
        assert!(err.contains("encode back") || err.contains("decode"), "{err}");
    }
}
