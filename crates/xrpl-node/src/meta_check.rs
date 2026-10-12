//! Independence task 2.6: our transaction metadata against mainnet's, byte for byte.
//!
//! The differential probe (`DX_META=<dir>`) builds each transaction's metadata from the engine's own
//! changes (native_apply::native_apply_one_changes, tx_meta::affected_nodes), encodes it with the network
//! codec, and compares the bytes with mainnet's. A mismatch names the first thing that differs: the result,
//! the delivered amount, a node missing or extra, or one field of one node. The answer key is the s2 ledger
//! store `scripts/fetch_meta_ledgers.py` writes: `<dir>/bin/<seq>.json`, each transaction's `tx_blob` and
//! `meta` in hex.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

/// Mainnet's metadata for one ledger, by transaction hash (upper hex).
pub fn load_ledger(dir: &str, seq: u64) -> Option<HashMap<String, Vec<u8>>> {
    let text = std::fs::read_to_string(format!("{dir}/bin/{seq}.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let txs = v.pointer("/result/ledger/transactions").or_else(|| v.pointer("/ledger/transactions"))?.as_array()?;
    let mut out = HashMap::with_capacity(txs.len());
    for t in txs {
        let (Some(blob), Some(meta)) = (t["tx_blob"].as_str(), t["meta"].as_str()) else { continue };
        let (Ok(blob), Ok(meta)) = (hex::decode(blob), hex::decode(meta)) else { continue };
        let mut pre = b"TXN\0".to_vec();
        pre.extend_from_slice(&blob);
        out.insert(hex::encode_upper(xrpl_core::crypto::signing::sha512_half(&pre)), meta);
    }
    Some(out)
}

/// Our metadata for one transaction, in engine JSON: the AffectedNodes our engine's changes give, our result
/// and delivered amount, and mainnet's TransactionIndex (Step 3 makes the position ours).
pub fn our_meta(tc: &crate::native_apply::TxChanges, tx_index: u64) -> Value {
    let mut m = json!({
        "AffectedNodes": xrpl_ledger::ledger::tx_meta::affected_nodes(&tc.changes),
        "TransactionIndex": tx_index,
        "TransactionResult": tc.result,
    });
    if let Some(d) = &tc.delivered {
        m["DeliveredAmount"] = d.clone();
    }
    m
}

/// The TransactionIndex in mainnet's metadata bytes.
pub fn tx_index(theirs: &[u8]) -> Option<u64> {
    xrpl_core::codec::decode::decode_transaction_binary(theirs).ok()?["TransactionIndex"].as_u64()
}

/// Ok when our metadata encodes to mainnet's bytes; else the first difference.
pub fn compare(ours: &Value, theirs: &[u8]) -> Result<(), String> {
    let mut canon = ours.clone();
    xrpl_ledger::ledger::canon::canon_for_encode(&mut canon);
    let our_bytes = xrpl_core::codec::encode::encode_transaction_json(&canon, false).map_err(|e| format!("encode: {e:?}"))?;
    if our_bytes == theirs {
        return Ok(());
    }
    let dec = |b: &[u8]| xrpl_core::codec::decode::decode_transaction_binary(b).map_err(|e| format!("decode: {e:?}"));
    let (o, t) = (dec(&our_bytes)?, dec(theirs)?);
    Err(first_difference(&o, &t).unwrap_or_else(|| "same JSON, different bytes (field or node order)".into()))
}

fn nodes(meta: &Value) -> BTreeMap<String, (String, Value)> {
    let mut out = BTreeMap::new();
    for n in meta["AffectedNodes"].as_array().into_iter().flatten() {
        let Some((kind, body)) = n.as_object().and_then(|o| o.iter().next()) else { continue };
        out.insert(body["LedgerIndex"].as_str().unwrap_or("").to_uppercase(), (kind.clone(), body.clone()));
    }
    out
}

/// The first difference between two decoded metadata objects, both spelled by the codec.
fn first_difference(o: &Value, t: &Value) -> Option<String> {
    for f in ["TransactionResult", "DeliveredAmount", "ParentBatchID", "TransactionIndex"] {
        if o.get(f) != t.get(f) {
            return Some(format!("{f} ours={} theirs={}", o.get(f).unwrap_or(&Value::Null), t.get(f).unwrap_or(&Value::Null)));
        }
    }
    let (on, tn) = (nodes(o), nodes(t));
    for (k, (tk, tb)) in &tn {
        let ty = tb["LedgerEntryType"].as_str().unwrap_or("?");
        let Some((ok, ob)) = on.get(k) else { return Some(format!("{tk} {ty} {} missing", &k[..8.min(k.len())])) };
        if ok != tk {
            return Some(format!("{ty} {} is our {ok}, mainnet's {tk}", &k[..8.min(k.len())]));
        }
        for sec in ["FinalFields", "PreviousFields", "NewFields"] {
            let (os, ts) = (ob.get(sec), tb.get(sec));
            if os == ts {
                continue;
            }
            let (oo, to) = (os.and_then(Value::as_object), ts.and_then(Value::as_object));
            let (Some(oo), Some(to)) = (oo, to) else {
                return Some(format!("{tk} {ty} {} {sec} ours {} theirs {}", &k[..8.min(k.len())], present(os), present(ts)));
            };
            let mut names: Vec<&String> = oo.keys().chain(to.keys()).collect();
            names.sort();
            names.dedup();
            for f in names {
                if oo.get(f) != to.get(f) {
                    return Some(format!(
                        "{tk} {ty} {} {sec}.{f} ours={} theirs={}",
                        &k[..8.min(k.len())],
                        oo.get(f).unwrap_or(&Value::Null),
                        to.get(f).unwrap_or(&Value::Null)
                    ));
                }
            }
        }
        for f in ["PreviousTxnID", "PreviousTxnLgrSeq"] {
            if ob.get(f) != tb.get(f) {
                return Some(format!("{tk} {ty} {} {f} ours={} theirs={}", &k[..8.min(k.len())], ob.get(f).unwrap_or(&Value::Null), tb.get(f).unwrap_or(&Value::Null)));
            }
        }
    }
    if let Some((k, (ok, ob))) = on.iter().find(|(k, _)| !tn.contains_key(*k)) {
        return Some(format!("{ok} {} {} extra", ob["LedgerEntryType"].as_str().unwrap_or("?"), &k[..8.min(k.len())]));
    }
    None
}

fn present(v: Option<&Value>) -> &'static str {
    match v {
        None => "absent",
        Some(Value::Object(o)) if o.is_empty() => "empty",
        Some(_) => "present",
    }
}

/// Per-type tallies for the probe's META SUMMARY line.
#[derive(Default)]
pub struct Tally {
    pub by_type: BTreeMap<String, (u64, u64)>,
}

impl Tally {
    pub fn add(&mut self, tx_type: &str, matched: bool) {
        let e = self.by_type.entry(tx_type.to_string()).or_default();
        if matched {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }

    pub fn summary(&self) -> String {
        let (m, d) = self.by_type.values().fold((0, 0), |a, v| (a.0 + v.0, a.1 + v.1));
        let per: Vec<String> = self.by_type.iter().map(|(t, (m, d))| format!("{t} {m}/{}", m + d)).collect();
        format!("META SUMMARY: {m}/{} records byte-exact; {}", m + d, per.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_difference_names_the_node_and_field() {
        let t = json!({"TransactionResult": "tesSUCCESS", "TransactionIndex": 3, "AffectedNodes": [
            {"ModifiedNode": {"LedgerEntryType": "AccountRoot", "LedgerIndex": "AB12CD34EE", "FinalFields": {"Balance": "10"}, "PreviousFields": {"Balance": "12"}}},
            {"CreatedNode": {"LedgerEntryType": "Offer", "LedgerIndex": "FF00", "NewFields": {"Sequence": 5}}}]});
        assert_eq!(first_difference(&t, &t), None);
        let mut o = t.clone();
        o["AffectedNodes"][0]["ModifiedNode"]["PreviousFields"]["Balance"] = json!("11");
        assert_eq!(first_difference(&o, &t).unwrap(), r#"ModifiedNode AccountRoot AB12CD34 PreviousFields.Balance ours="11" theirs="12""#);
        let mut o = t.clone();
        o["AffectedNodes"].as_array_mut().unwrap().pop();
        assert_eq!(first_difference(&o, &t).unwrap(), "CreatedNode Offer FF00 missing");
        let mut o = t.clone();
        o["TransactionResult"] = json!("tecPATH_DRY");
        assert!(first_difference(&o, &t).unwrap().starts_with("TransactionResult"));
    }

    #[test]
    fn the_tally_reads_per_type() {
        let mut t = Tally::default();
        t.add("Payment", true);
        t.add("Payment", false);
        t.add("OfferCreate", true);
        assert_eq!(t.summary(), "META SUMMARY: 2/3 records byte-exact; OfferCreate 1/1, Payment 1/2");
    }
}
