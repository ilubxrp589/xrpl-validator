//! OracleSet / OracleDelete — XLS-47 price oracles.
//!
//! rippled reference: `SetOracle.cpp` / `DeleteOracle.cpp`. An Oracle object
//! is keyed by (owner, OracleDocumentID) in namespace 'R' and carries up to
//! ten `PriceDataSeries` entries. OracleSet is an upsert: on update, token
//! pairs named in the tx replace matching pairs in the object (a pair sent
//! WITHOUT an AssetPrice deletes that pair); pairs the tx doesn't name
//! survive. The owner reserve charges 1 unit for ≤ 5 pairs, 2 above.
//!
//! Mutation shape (mainnet-verified against #105091578 / #105666830 Band
//! Protocol updates): Oracle Modified + AccountRoot Modified on update;
//! create adds the owner-dir insert.

use crate::ledger::directory::{owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

fn doc_id(tx: &TxFields) -> Option<u32> {
    tx.fields
        .get("OracleDocumentID")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
}

/// Owner-reserve units an oracle occupies: 1 for ≤ 5 token pairs, 2 above.
fn reserve_units(series: &serde_json::Value) -> i64 {
    let n = series.as_array().map(|a| a.len()).unwrap_or(0);
    if n > 5 { 2 } else { 1 }
}

fn pair_of(item: &serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    let pd = &item["PriceData"];
    (pd["BaseAsset"].clone(), pd["QuoteAsset"].clone())
}

fn bump_owner_count(sandbox: &mut Sandbox, account: &[u8; 20], delta: i64) {
    if delta == 0 {
        return;
    }
    let k = keylet::account_root_key(account);
    if let Some(d) = sandbox.read(&k) {
        if let Ok(mut a) = serde_json::from_slice::<serde_json::Value>(&d) {
            let oc = a["OwnerCount"].as_u64().unwrap_or(0) as i64;
            a["OwnerCount"] = serde_json::json!((oc + delta).max(0));
            sandbox.write(k, serde_json::to_vec(&a).unwrap_or_default());
        }
    }
}

pub struct OracleSetTransactor;

impl Transactor for OracleSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "OracleSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if doc_id(tx).is_none() || tx.fields.get("LastUpdateTime").is_none() {
            return TxResult::Malformed;
        }
        // 1..=10 pairs per transaction.
        let n = tx.fields["PriceDataSeries"].as_array().map(|a| a.len());
        match n {
            Some(1..=10) => TxResult::Success,
            _ => TxResult::Malformed,
        }
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let k = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&k) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = doc_id(tx) else {
            return TxResult::Malformed;
        };
        let key = keylet::oracle_key(&tx.account, id);

        let existing = sandbox
            .read(&key)
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok());

        if let Some(mut oracle) = existing {
            let old_units = reserve_units(&oracle["PriceDataSeries"]);
            let mut series = oracle["PriceDataSeries"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for item in tx.fields["PriceDataSeries"].as_array().into_iter().flatten() {
                let tx_pair = pair_of(item);
                let pos = series.iter().position(|e| pair_of(e) == tx_pair);
                if item["PriceData"].get("AssetPrice").is_none() {
                    // Pair named without a price → delete it from the object.
                    if let Some(p) = pos {
                        series.remove(p);
                    }
                } else if let Some(p) = pos {
                    series[p] = item.clone();
                } else {
                    series.push(item.clone());
                }
            }
            if series.is_empty() || series.len() > 10 {
                return TxResult::Malformed;
            }
            oracle["PriceDataSeries"] = serde_json::Value::Array(series);
            for f in ["LastUpdateTime", "Provider", "AssetClass", "URI"] {
                if let Some(v) = tx.fields.get(f) {
                    oracle[f] = v.clone();
                }
            }
            let new_units = reserve_units(&oracle["PriceDataSeries"]);
            sandbox.write(key, serde_json::to_vec(&oracle).unwrap_or_default());
            bump_owner_count(sandbox, &tx.account, new_units - old_units);
        } else {
            // Create path: Provider + AssetClass are required alongside the series.
            if tx.fields.get("Provider").is_none() || tx.fields.get("AssetClass").is_none() {
                return TxResult::Malformed;
            }
            let mut oracle = serde_json::json!({
                "LedgerEntryType": "Oracle",
                "Owner": hex::encode(tx.account),
                "OracleDocumentID": id,
                "PriceDataSeries": tx.fields["PriceDataSeries"].clone(),
                "OwnerNode": 0,
            });
            for f in ["LastUpdateTime", "Provider", "AssetClass", "URI"] {
                if let Some(v) = tx.fields.get(f) {
                    oracle[f] = v.clone();
                }
            }
            let units = reserve_units(&oracle["PriceDataSeries"]);
            sandbox.write(key, serde_json::to_vec(&oracle).unwrap_or_default());
            owner_dir_insert(sandbox, &tx.account, &key);
            bump_owner_count(sandbox, &tx.account, units);
        }

        TxResult::Success
    }
}

pub struct OracleDeleteTransactor;

impl Transactor for OracleDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "OracleDelete" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if doc_id(tx).is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let k = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&k) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = doc_id(tx) else {
            return TxResult::Malformed;
        };
        let key = keylet::oracle_key(&tx.account, id);
        let Some(data) = sandbox.read(&key) else {
            return TxResult::NoEntry;
        };
        let oracle: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };
        let units = reserve_units(&oracle["PriceDataSeries"]);
        let hint = oracle.get("OwnerNode").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
        });
        sandbox.delete(key);
        owner_dir_remove(sandbox, &tx.account, &key, hint);
        bump_owner_count(sandbox, &tx.account, -units);
        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn state_with_account(id: &[u8; 20]) -> LedgerState {
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
            "Balance": "1000000000",
            "Sequence": 5,
            "OwnerCount": 0,
            "Flags": 0,
        });
        state.state_map.insert(
            keylet::account_root_key(id),
            serde_json::to_vec(&acct).unwrap(),
        );
        state
    }

    fn pd(base: &str, price: Option<&str>) -> serde_json::Value {
        let mut inner = serde_json::json!({ "BaseAsset": base, "QuoteAsset": "USD", "Scale": 9 });
        if let Some(p) = price {
            inner["AssetPrice"] = serde_json::json!(p);
        }
        serde_json::json!({ "PriceData": inner })
    }

    fn set_tx(account: [u8; 20], series: Vec<serde_json::Value>) -> TxFields {
        TxFields {
            account,
            tx_type: "OracleSet".into(),
            fee: 10,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "OracleDocumentID": 1,
                "LastUpdateTime": 1_780_000_000u64,
                "Provider": "42616E64",
                "AssetClass": "63757272656E6379",
                "PriceDataSeries": series,
            }),
        }
    }

    #[test]
    fn create_then_update_merges_series() {
        let id = [0x51u8; 20];
        let state = state_with_account(&id);
        let tr = OracleSetTransactor;
        let key = keylet::oracle_key(&id, 1);

        let mut sb = Sandbox::new(&state);
        let create = set_tx(id, vec![pd("BTC", Some("11")), pd("ETH", Some("22"))]);
        assert_eq!(tr.preflight(&create), TxResult::Success);
        assert_eq!(tr.do_apply(&create, &mut sb), TxResult::Success);

        // Update BTC, delete ETH, add XRP.
        let update = set_tx(id, vec![pd("BTC", Some("99")), pd("ETH", None), pd("XRP", Some("3"))]);
        assert_eq!(tr.do_apply(&update, &mut sb), TxResult::Success);
        let oracle: serde_json::Value = serde_json::from_slice(&sb.read(&key).unwrap()).unwrap();
        let series = oracle["PriceDataSeries"].as_array().unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0]["PriceData"]["BaseAsset"], "BTC");
        assert_eq!(series[0]["PriceData"]["AssetPrice"], "99");
        assert_eq!(series[1]["PriceData"]["BaseAsset"], "XRP");
    }

    #[test]
    fn delete_removes_oracle_and_reserve() {
        let id = [0x52u8; 20];
        let state = state_with_account(&id);
        let mut sb = Sandbox::new(&state);
        let tr_set = OracleSetTransactor;
        tr_set.do_apply(&set_tx(id, vec![pd("BTC", Some("1"))]), &mut sb);

        let del = TxFields {
            account: id,
            tx_type: "OracleDelete".into(),
            fee: 10,
            sequence: 6,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({ "OracleDocumentID": 1 }),
        };
        let tr = OracleDeleteTransactor;
        assert_eq!(tr.do_apply(&del, &mut sb), TxResult::Success);
        assert!(sb.read(&keylet::oracle_key(&id, 1)).is_none());
        let acct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&id)).unwrap()).unwrap();
        assert_eq!(acct["OwnerCount"], 0);
    }
}
