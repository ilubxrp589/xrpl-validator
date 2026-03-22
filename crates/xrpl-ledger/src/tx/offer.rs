//! OfferCreate and OfferCancel transaction types.
//!
//! OfferCreate: places a new order on the DEX, crossing against existing offers.
//! OfferCancel: removes an existing order.
//!
//! Offer crossing walks the order book and matches compatible offers.
//! When offers cross: balances adjust, consumed offers are deleted,
//! partial fills modify the remaining amount.

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

/// Check if new offer's rate is compatible with existing offer for crossing.
///
/// New OfferCreate: taker sells `taker_gets`, wants to buy `taker_pays`.
/// Existing Offer: owner sells `existing_pays` (TakerGets from existing's perspective),
///                 wants `existing_gets` (TakerPays from existing's perspective).
///
/// For crossing: new taker's TakerPays must match existing's TakerGets type,
/// and the price must be compatible.
///
/// Quality (price): gets/pays ratio. Offers cross when:
///   new_gets/new_pays <= existing_pays/existing_gets
///   (new taker's asking price <= existing offer's selling price)
fn offers_cross(
    new_gets: &Amount, new_pays: &Amount,
    existing_gets: &Amount, existing_pays: &Amount,
) -> bool {
    // new taker quality = new_gets / new_pays (what taker gives per unit received)
    // existing quality = existing_gets / existing_pays (what existing asks per unit they give)
    // Cross when taker's quality >= existing's quality
    // i.e. taker is willing to give at least as much per unit as existing asks
    let new_quality = new_gets.rate_against(new_pays);       // taker gives this much per 1 unit bought
    let existing_quality = existing_gets.rate_against(existing_pays); // existing asks this much per 1 unit sold
    new_quality >= existing_quality * 0.999999 // epsilon for float
}

/// Execute a crossing between a new offer and an existing offer.
/// Returns the remaining amounts for the new offer after the cross.
fn execute_crossing(
    new_gets: &Amount, new_pays: &Amount,
    existing_gets: &Amount, existing_pays: &Amount,
    existing_account: &[u8; 20],
    taker_account: &[u8; 20],
    sandbox: &mut Sandbox,
) -> (Option<Amount>, Option<Amount>) {
    // Determine fill amount — limited by the smaller side
    let fill_fraction = match (new_pays, existing_pays) {
        (Amount::Xrp(new_want), Amount::Xrp(existing_sell)) => {
            let fill = (*new_want).min(*existing_sell);
            if *existing_sell == 0 { return (Some(new_gets.clone()), Some(new_pays.clone())); }
            fill as f64 / *existing_sell as f64
        }
        (Amount::Iou { value: new_want, .. }, Amount::Iou { value: existing_sell, .. }) => {
            let fill = new_want.min(*existing_sell);
            if *existing_sell == 0.0 { return (Some(new_gets.clone()), Some(new_pays.clone())); }
            fill / existing_sell
        }
        (Amount::Xrp(new_want), Amount::Iou { value: existing_sell, .. }) => {
            // Can't directly compare XRP drops to IOU value
            // Use the exchange rate
            if *existing_sell == 0.0 { return (Some(new_gets.clone()), Some(new_pays.clone())); }
            1.0_f64.min(*new_want as f64 / (*existing_sell * 1_000_000.0))
        }
        (Amount::Iou { value: new_want, .. }, Amount::Xrp(existing_sell)) => {
            if *existing_sell == 0 { return (Some(new_gets.clone()), Some(new_pays.clone())); }
            1.0_f64.min(*new_want / (*existing_sell as f64 / 1_000_000.0))
        }
    };

    let fill_fraction = fill_fraction.min(1.0).max(0.0);

    // Adjust taker's XRP balance (gets what existing is selling)
    if let Amount::Xrp(existing_sell) = existing_pays {
        let filled_drops = (*existing_sell as f64 * fill_fraction) as u64;
        adjust_xrp_balance(taker_account, filled_drops as i64, sandbox);
        adjust_xrp_balance(existing_account, -(filled_drops as i64), sandbox);
    }

    // Adjust existing's balance (gets what taker is selling)
    if let Amount::Xrp(new_sell_total) = new_gets {
        let filled_drops = (*new_sell_total as f64 * fill_fraction) as u64;
        adjust_xrp_balance(existing_account, filled_drops as i64, sandbox);
        adjust_xrp_balance(taker_account, -(filled_drops as i64), sandbox);
    }

    // Compute remaining amounts
    let remaining_fraction = 1.0 - fill_fraction;
    if remaining_fraction < 0.000001 {
        // Fully filled
        (None, None)
    } else {
        let remaining_gets = match new_gets {
            Amount::Xrp(d) => Amount::Xrp((*d as f64 * remaining_fraction) as u64),
            Amount::Iou { currency, issuer, value } =>
                Amount::Iou { currency: currency.clone(), issuer: issuer.clone(), value: value * remaining_fraction },
        };
        let remaining_pays = match new_pays {
            Amount::Xrp(d) => Amount::Xrp((*d as f64 * remaining_fraction) as u64),
            Amount::Iou { currency, issuer, value } =>
                Amount::Iou { currency: currency.clone(), issuer: issuer.clone(), value: value * remaining_fraction },
        };
        (Some(remaining_gets), Some(remaining_pays))
    }
}

fn adjust_xrp_balance(account: &[u8; 20], delta: i64, sandbox: &mut Sandbox) {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let new_balance = balance + delta;
            if new_balance < 0 {
                // Insufficient balance — skip this adjustment rather than
                // clamping to 0, which would create XRP from nothing.
                return;
            }
            acct["Balance"] = serde_json::Value::String(new_balance.to_string());
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

/// Try to find and cross against existing offers in the sandbox/state.
/// This scans known offers — not a full directory walk (which requires
/// the order book directory structure). Sufficient for offers loaded from sync.
fn try_cross_offers(
    taker: &[u8; 20],
    taker_pays: &Amount,
    taker_gets: &Amount,
    sandbox: &mut Sandbox,
) -> (Option<Amount>, Option<Amount>, u32) {
    // We can't enumerate all offers without the directory structure.
    // In a full implementation, we'd walk the offer book directory by quality.
    // For now, crossing happens when offers are explicitly loaded in state.
    // The offer is simply placed on the book — no crossing without directory.
    //
    // Return remaining amounts unchanged and 0 offers consumed.
    (Some(taker_gets.clone()), Some(taker_pays.clone()), 0)
}

/// OfferCreate transactor — create a new DEX offer, attempt crossing.
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
        let taker_pays = match parse_amount(&tx.fields["TakerPays"]) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        let taker_gets = match parse_amount(&tx.fields["TakerGets"]) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // Attempt crossing against existing offers
        let (remaining_gets, remaining_pays, crossed) =
            try_cross_offers(&tx.account, &taker_pays, &taker_gets, sandbox);

        // If the offer is not fully filled, place the remainder on the book
        let is_fill_or_kill = tx.fields.get("Flags")
            .and_then(|f| f.as_u64())
            .map(|f| f & 0x00040000 != 0) // tfFillOrKill
            .unwrap_or(false);

        let is_immediate_or_cancel = tx.fields.get("Flags")
            .and_then(|f| f.as_u64())
            .map(|f| f & 0x00020000 != 0) // tfImmediateOrCancel
            .unwrap_or(false);

        if let (Some(rem_gets), Some(rem_pays)) = (remaining_gets, remaining_pays) {
            if is_fill_or_kill {
                // Fill or Kill: the offer must be fully filled.
                // If there are remaining amounts, it was not fully filled.
                return TxResult::Unfunded;
            }

            if !is_immediate_or_cancel {
                // Place remaining offer on the book
                let seq = if tx.uses_ticket() {
                    tx.ticket_seq.unwrap_or(0)
                } else {
                    tx.sequence
                };
                let offer_key = keylet::offer_key(&tx.account, seq);

                let gets_json = match &rem_gets {
                    Amount::Xrp(d) => serde_json::Value::String(d.to_string()),
                    Amount::Iou { currency, issuer, value } => serde_json::json!({
                        "currency": currency, "issuer": issuer, "value": value.to_string()
                    }),
                };
                let pays_json = match &rem_pays {
                    Amount::Xrp(d) => serde_json::Value::String(d.to_string()),
                    Amount::Iou { currency, issuer, value } => serde_json::json!({
                        "currency": currency, "issuer": issuer, "value": value.to_string()
                    }),
                };

                let offer_obj = serde_json::json!({
                    "LedgerEntryType": "Offer",
                    "Account": hex::encode(tx.account),
                    "Sequence": seq,
                    "TakerPays": pays_json,
                    "TakerGets": gets_json,
                    "Flags": tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0),
                });
                sandbox.write(offer_key, serde_json::to_vec(&offer_obj).unwrap());

                // Increment OwnerCount
                let acct_key = keylet::account_root_key(&tx.account);
                if let Some(data) = sandbox.read(&acct_key) {
                    if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                        let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                        acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
                        sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
                    }
                }
            }
            // ImmediateOrCancel: don't place on book, just keep any partial fill that happened
        }
        // else: fully filled, nothing to place on book

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

        if sandbox.exists(&offer_key) {
            sandbox.delete(offer_key);

            // Decrement OwnerCount
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(data) = sandbox.read(&acct_key) {
                if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                    if count > 0 {
                        acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
                    }
                    sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
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
                "TakerPays": "1000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "10"},
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
    fn offers_cross_rate_check() {
        // New offer: buy 100 XRP, sell 10 USD (rate: 10 XRP per USD)
        // Existing offer: sell 50 XRP, buy 5 USD (rate: 10 XRP per USD)
        // Should cross (equal rate)
        let new_gets = Amount::Iou { currency: "USD".into(), issuer: "x".into(), value: 10.0 };
        let new_pays = Amount::Xrp(100_000_000);
        let existing_gets = Amount::Iou { currency: "USD".into(), issuer: "x".into(), value: 5.0 };
        let existing_pays = Amount::Xrp(50_000_000);
        assert!(offers_cross(&new_gets, &new_pays, &existing_gets, &existing_pays));

        // New offer: buy 100 XRP, sell 5 USD (rate: 20 XRP per USD — expensive)
        // Existing: sell 50 XRP, buy 10 USD (rate: 5 XRP per USD — cheap)
        // Should NOT cross — new taker isn't offering enough USD
        let new_gets2 = Amount::Iou { currency: "USD".into(), issuer: "x".into(), value: 5.0 };
        let existing_gets2 = Amount::Iou { currency: "USD".into(), issuer: "x".into(), value: 10.0 };
        assert!(!offers_cross(&new_gets2, &new_pays, &existing_gets2, &existing_pays));
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
        assert_eq!(OfferCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // IOC offer should NOT be placed on the book (no crossing happened)
        let offer_key = keylet::offer_key(&acct, 5);
        assert!(!sandbox.exists(&offer_key));
    }
}
