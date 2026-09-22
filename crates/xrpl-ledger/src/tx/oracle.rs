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

/// fixPriceOracleOrder (mainnet-enabled): a created oracle's series is sorted
/// by token pair, as an updated one always is.
const FIX_PRICE_ORACLE_ORDER: &str = "FF2D1E13CF6D22427111B967BD504917F63A900CECD320D6FD3AC9FA90344631";

/// rippled `tokenPairKey`: the (BaseAsset, QuoteAsset) Currency pair, as the
/// 160-bit values a `std::map` orders by.
fn pair_key(item: &serde_json::Value) -> ([u8; 20], [u8; 20]) {
    let pd = &item["PriceData"];
    let c = |v: &serde_json::Value| {
        crate::tx::offer::amount_currency20(&serde_json::json!({"currency": v.as_str().unwrap_or("")})).unwrap_or([0u8; 20])
    };
    (c(&pd["BaseAsset"]), c(&pd["QuoteAsset"]))
}

/// Finding 358 (soak #24 receipt, #107159929 B88A96E280D3 — an OracleSet
/// naming 9 of the oracle's 10 pairs): rippled's OracleSet::doApply rebuilds
/// the series from scratch — every pair the oracle already holds is collected
/// as BaseAsset + QuoteAsset ONLY ("the token pair that doesn't have their
/// price updated will not include neither price nor scale"), then the
/// transaction's entries delete (no AssetPrice), update (AssetPrice, and Scale
/// only when sent) or add pairs, and the result is emitted in std::map order
/// of (BaseAsset, QuoteAsset). We kept the unnamed pair's old price and scale
/// (13 bytes mainnet dropped) and appended new pairs unsorted.
fn merge_series(existing: &[serde_json::Value], tx_series: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut pairs: std::collections::BTreeMap<([u8; 20], [u8; 20]), serde_json::Value> = std::collections::BTreeMap::new();
    for e in existing {
        let pd = &e["PriceData"];
        pairs.insert(pair_key(e), serde_json::json!({"PriceData": {"BaseAsset": pd["BaseAsset"], "QuoteAsset": pd["QuoteAsset"]}}));
    }
    for item in tx_series {
        let key = pair_key(item);
        let pd = &item["PriceData"];
        match pd.get("AssetPrice") {
            None => {
                pairs.remove(&key);
            }
            Some(price) => {
                if let Some(cur) = pairs.get_mut(&key) {
                    cur["PriceData"]["AssetPrice"] = price.clone();
                    if let Some(sc) = pd.get("Scale") {
                        cur["PriceData"]["Scale"] = sc.clone();
                    }
                } else {
                    pairs.insert(key, item.clone());
                }
            }
        }
    }
    pairs.into_values().collect()
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
        if tx.fee_missing() {
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
        // OracleSet::preclaim time rules (OracleSet.cpp:68-81, :128-130), in
        // rippled's order. LastUpdateTime is UNIX epoch in the tx but the
        // ledger clock is RIPPLE epoch, hence the 946684800 offset.
        //   1. below the epoch offset → tecINVALID_UPDATE_TIME
        //   2. outside closeTime ± kMaxLastUpdateTimeDelta (300s) →
        //      tecINVALID_UPDATE_TIME. The harness header's close_time IS the
        //      parent's close ("the last closed ledger", rippled's comment).
        //      rippled bails tecINTERNAL when closeTime < 300 — unreachable
        //      on a real ledger — so a degenerate header skips the window.
        //   3. updating an EXISTING oracle, the new time must be STRICTLY
        //      newer than the stored one. Absent SLE = the create path — no
        //      staleness to judge — so absence skips, never condemns.
        //
        // The specimens are one bot (rsNvoAZ9, doc 1) resubmitting the same
        // update: #106122429 45AEB189 carries LastUpdateTime equal to the
        // stored value (rule 3; its −283s sits inside the window), while
        // #106323095 0D01197E and #106323126 0B3AE2F8 repeat a value 365s
        // and 485s behind close, so rule 2 takes them first — same verdict.
        const EPOCH_OFFSET: u64 = 946_684_800;
        const MAX_DELTA: u64 = 300;
        let Some(lut) = tx.fields.get("LastUpdateTime").and_then(|v| v.as_u64()) else {
            return TxResult::Malformed;
        };
        if lut < EPOCH_OFFSET {
            return TxResult::InvalidUpdateTime;
        }
        let lut_ripple = lut - EPOCH_OFFSET;
        let close = sandbox.base().close_time() as u64;
        if close >= MAX_DELTA
            && (lut_ripple < close - MAX_DELTA || lut_ripple > close + MAX_DELTA)
        {
            return TxResult::InvalidUpdateTime;
        }
        let existing: Option<serde_json::Value> = doc_id(tx)
            .and_then(|id| sandbox.read(&keylet::oracle_key(&tx.account, id)))
            .and_then(|d| serde_json::from_slice(&d).ok());
        if let Some(sle) = &existing {
            if let Some(stored) = sle.get("LastUpdateTime").and_then(|v| v.as_u64()) {
                if lut <= stored {
                    return TxResult::InvalidUpdateTime;
                }
            }
        }
        // Finding 358 — OracleSet::preclaim (OracleSet.cpp:86-150): the pair
        // bookkeeping. A pair sent WITHOUT AssetPrice must exist in the oracle
        // (tecTOKEN_PAIR_NOT_FOUND); the merged set may not be empty
        // (tecARRAY_EMPTY) nor exceed ten (tecARRAY_TOO_LARGE); and the
        // pre-fee balance must cover the reserve at the new pair count
        // (tecINSUFFICIENT_RESERVE). Before this the empty merge was a tem.
        let tx_series: Vec<serde_json::Value> = tx.fields["PriceDataSeries"].as_array().cloned().unwrap_or_default();
        let old_series: Vec<serde_json::Value> = existing
            .as_ref()
            .and_then(|o| o["PriceDataSeries"].as_array().cloned())
            .unwrap_or_default();
        if existing.is_some() {
            let held: std::collections::BTreeSet<([u8; 20], [u8; 20])> = old_series.iter().map(pair_key).collect();
            for item in &tx_series {
                if item["PriceData"].get("AssetPrice").is_none() && !held.contains(&pair_key(item)) {
                    return TxResult::TokenPairNotFound;
                }
            }
        }
        let merged = merge_series(&old_series, &tx_series);
        if merged.is_empty() {
            return TxResult::TecArrayEmpty;
        }
        if merged.len() > 10 {
            return TxResult::ArrayTooLarge;
        }
        let adjust = reserve_units(&serde_json::Value::Array(merged)) - if existing.is_some() { reserve_units(&serde_json::Value::Array(old_series)) } else { 0 };
        if adjust > 0 {
            if let Some(a) = sandbox.read(&k).and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok()) {
                let bal = a["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let oc = a["OwnerCount"].as_u64().unwrap_or(0);
                if bal < crate::ledger::fees::account_reserve(sandbox, oc + adjust as u64) {
                    return TxResult::InsufficientReserve;
                }
            }
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
            // Finding 358: rippled rebuilds the series (merge_series) — unnamed
            // pairs keep only their asset names; output in token-pair order.
            let series = merge_series(
                oracle["PriceDataSeries"].as_array().map(Vec::as_slice).unwrap_or(&[]),
                tx.fields["PriceDataSeries"].as_array().map(Vec::as_slice).unwrap_or(&[]),
            );
            if series.is_empty() {
                return TxResult::TecArrayEmpty;
            }
            // Overflowing the ten-entry limit by MERGING is a tec, not a tem:
            // the transaction is well formed and only the resulting object is
            // too big, so the fee is claimed (OracleSet.cpp:168
            // tecARRAY_TOO_LARGE). temARRAY_TOO_LARGE is the separate
            // preflight check on the transaction's own array (line 46).
            // #105792173 F2572F55 submits ten entries that merge past the cap.
            if series.len() > 10 {
                return TxResult::ArrayTooLarge;
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
            // `sfFlags` is soeREQUIRED on the Oracle ledger format, so the
            // created object carries `Flags: 0` right after its type (finding
            // 293, #107052956 B1EB9B20: ours serialized five bytes short).
            let mut oracle = serde_json::json!({
                "LedgerEntryType": "Oracle",
                "Flags": 0,
                "Owner": hex::encode(tx.account),
                "OracleDocumentID": id,
                "PriceDataSeries": if crate::ledger::amendments::enabled(sandbox, FIX_PRICE_ORACLE_ORDER) {
                    // fixPriceOracleOrder: the created series is emitted in
                    // token-pair order (OracleSet.cpp:307-318), like an update.
                    serde_json::Value::Array(merge_series(&[], tx.fields["PriceDataSeries"].as_array().map(Vec::as_slice).unwrap_or(&[])))
                } else {
                    tx.fields["PriceDataSeries"].clone()
                },
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
        if tx.fee_missing() {
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
        owner_dir_remove(sandbox, &tx.account, &key, hint, true);
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
            inner_batch: false,
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
        // Finding 358: token-pair order — XRP (the zero currency) sorts first.
        assert_eq!(series[0]["PriceData"]["BaseAsset"], "XRP");
        assert_eq!(series[0]["PriceData"]["AssetPrice"], "3");
        assert_eq!(series[1]["PriceData"]["BaseAsset"], "BTC");
        assert_eq!(series[1]["PriceData"]["AssetPrice"], "99");
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
            inner_batch: false,
        };
        let tr = OracleDeleteTransactor;
        assert_eq!(tr.do_apply(&del, &mut sb), TxResult::Success);
        assert!(sb.read(&keylet::oracle_key(&id, 1)).is_none());
        let acct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&id)).unwrap()).unwrap();
        assert_eq!(acct["OwnerCount"], 0);
    }

    /// Finding 358: pairs the update does not name keep only their asset
    /// names; the series comes out in token-pair order; deleting a pair the
    /// oracle does not hold is tecTOKEN_PAIR_NOT_FOUND; an empty merge is
    /// tecARRAY_EMPTY (a tec, the fee is claimed).
    #[test]
    fn update_strips_unnamed_pairs_and_sorts() {
        let id = [0x53u8; 20];
        let state = state_with_account(&id);
        let tr = OracleSetTransactor;
        let key = keylet::oracle_key(&id, 1);
        let mut sb = Sandbox::new(&state);
        let create = set_tx(id, vec![pd("ETH", Some("22")), pd("BTC", Some("11"))]);
        assert_eq!(tr.do_apply(&create, &mut sb), TxResult::Success);
        // Name only BTC (new price, no Scale) and add AAA: ETH keeps its
        // names alone, BTC's Scale is gone, order is AAA, BTC, ETH.
        let mut update = set_tx(id, vec![pd("BTC", Some("99")), pd("AAA", Some("1"))]);
        update.fields["PriceDataSeries"][0]["PriceData"].as_object_mut().unwrap().remove("Scale");
        update.fields["LastUpdateTime"] = serde_json::json!(1_780_000_100u64);
        assert_eq!(tr.preclaim(&update, &sb), TxResult::Success);
        assert_eq!(tr.do_apply(&update, &mut sb), TxResult::Success);
        let oracle: serde_json::Value = serde_json::from_slice(&sb.read(&key).unwrap()).unwrap();
        let series = oracle["PriceDataSeries"].as_array().unwrap();
        let bases: Vec<&str> = series.iter().map(|e| e["PriceData"]["BaseAsset"].as_str().unwrap()).collect();
        assert_eq!(bases, ["AAA", "BTC", "ETH"]);
        assert_eq!(series[1]["PriceData"]["AssetPrice"], "99");
        assert!(series[1]["PriceData"].get("Scale").is_none(), "Scale only when the update sends it");
        assert!(series[2]["PriceData"].get("AssetPrice").is_none() && series[2]["PriceData"].get("Scale").is_none(), "an unnamed pair keeps its names only");
        let mut bad = set_tx(id, vec![pd("ZZZ", None)]);
        bad.fields["LastUpdateTime"] = serde_json::json!(1_780_000_200u64);
        assert_eq!(tr.preclaim(&bad, &sb), TxResult::TokenPairNotFound);
        let mut empty = set_tx(id, vec![pd("AAA", None), pd("BTC", None), pd("ETH", None)]);
        empty.fields["LastUpdateTime"] = serde_json::json!(1_780_000_200u64);
        assert_eq!(tr.preclaim(&empty, &sb), TxResult::TecArrayEmpty);
    }
}
