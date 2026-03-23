//! Check transaction types: CheckCreate, CheckCash, CheckCancel.
//!
//! Checks are deferred payments — the sender creates a Check object that the
//! destination can cash later. The sender's funds are not locked; they must
//! have sufficient balance when the Check is cashed.
//!
//! CheckCreate: Create a Check object at check_key(account, sequence).
//! CheckCash:   Cash a Check — transfer funds from sender to destination, delete Check.
//! CheckCancel: Cancel a Check — delete the object with no fund transfer.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

/// Extract a 20-byte account ID from a hex string field.
fn parse_account_id(val: &serde_json::Value) -> Option<[u8; 20]> {
    let hex_str = val.as_str()?;
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

/// Extract an XRP amount in drops from a string or number value.
fn parse_drops(val: &serde_json::Value) -> Option<u64> {
    match val {
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// CheckCreate
// ---------------------------------------------------------------------------

/// CheckCreate transactor — create a new Check object.
pub struct CheckCreateTransactor;

impl Transactor for CheckCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "CheckCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Destination").is_none() {
            return TxResult::Malformed;
        }
        if tx.fields.get("SendMax").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        // Destination must be a valid account
        if let Some(dest) = tx.fields.get("Destination").and_then(|d| parse_account_id(d)) {
            let dest_key = keylet::account_root_key(&dest);
            if !sandbox.exists(&dest_key) {
                return TxResult::NoDst;
            }
        } else {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let dest = match tx.fields.get("Destination").and_then(|d| parse_account_id(d)) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };

        let send_max = match tx.fields.get("SendMax") {
            Some(v) => v.clone(),
            None => return TxResult::Malformed,
        };

        let seq = if tx.uses_ticket() {
            tx.ticket_seq.unwrap_or(0)
        } else {
            tx.sequence
        };
        let check_key = keylet::check_key(&tx.account, seq);

        // Create the Check ledger object
        let check_obj = serde_json::json!({
            "LedgerEntryType": "Check",
            "Account": hex::encode(tx.account),
            "Destination": hex::encode(dest),
            "SendMax": send_max,
            "Sequence": seq,
        });
        sandbox.write(check_key, serde_json::to_vec(&check_obj).unwrap());

        // Increment OwnerCount on sender
        let acct_key = keylet::account_root_key(&tx.account);
        if let Some(data) = sandbox.read(&acct_key) {
            if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
                sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
            }
        }

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// CheckCash
// ---------------------------------------------------------------------------

/// CheckCash transactor — cash a Check, transferring funds from the Check
/// creator to the destination.
pub struct CheckCashTransactor;

impl Transactor for CheckCashTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "CheckCash" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // Must specify CheckID to identify the check
        if tx.fields.get("CheckID").is_none() {
            return TxResult::Malformed;
        }
        // Must specify Amount (how much to cash)
        if tx.fields.get("Amount").is_none() {
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
        // Decode CheckID (hex of the 32-byte keylet)
        let check_id_hex = match tx.fields.get("CheckID").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let check_id_bytes = match hex::decode(check_id_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => return TxResult::Malformed,
        };
        let mut check_key_arr = [0u8; 32];
        check_key_arr.copy_from_slice(&check_id_bytes);
        let check_key = xrpl_core::types::Hash256(check_key_arr);

        // Read the Check object
        let check_data = match sandbox.read(&check_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let check: serde_json::Value = match serde_json::from_slice(&check_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Verify the casher is the destination of the check
        let dest = match check.get("Destination").and_then(|d| parse_account_id(d)) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        if dest != tx.account {
            return TxResult::NoPermission;
        }

        // Get the check creator
        let creator = match check.get("Account").and_then(|a| parse_account_id(a)) {
            Some(c) => c,
            None => return TxResult::Malformed,
        };

        // Parse the cash amount
        let amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // Check SendMax — the amount cashed cannot exceed SendMax
        if let Some(send_max_drops) = check.get("SendMax").and_then(|s| parse_drops(s)) {
            if amount > send_max_drops {
                return TxResult::Unfunded;
            }
        }

        // Deduct from creator's balance
        let creator_key = keylet::account_root_key(&creator);
        let creator_data = match sandbox.read(&creator_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut creator_acct: serde_json::Value = match serde_json::from_slice(&creator_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let creator_balance = creator_acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if creator_balance < amount {
            return TxResult::Unfunded;
        }

        creator_acct["Balance"] =
            serde_json::Value::String((creator_balance - amount).to_string());

        // Decrement creator's OwnerCount
        let owner_count = creator_acct["OwnerCount"].as_u64().unwrap_or(0);
        if owner_count > 0 {
            creator_acct["OwnerCount"] =
                serde_json::Value::Number((owner_count - 1).into());
        }
        sandbox.write(creator_key, serde_json::to_vec(&creator_acct).unwrap());

        // Credit destination (casher) — fail if destination account can't be read
        let dest_key = keylet::account_root_key(&tx.account);
        let dest_data = match sandbox.read(&dest_key) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let mut dest_acct: serde_json::Value = match serde_json::from_slice(&dest_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };
        let dest_balance = dest_acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        dest_acct["Balance"] =
            serde_json::Value::String((dest_balance + amount).to_string());
        sandbox.write(dest_key, serde_json::to_vec(&dest_acct).unwrap());

        // Delete the Check object
        sandbox.delete(check_key);

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// CheckCancel
// ---------------------------------------------------------------------------

/// CheckCancel transactor — cancel a Check with no fund transfer.
pub struct CheckCancelTransactor;

impl Transactor for CheckCancelTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "CheckCancel" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("CheckID").is_none() {
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
        // Decode CheckID
        let check_id_hex = match tx.fields.get("CheckID").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let check_id_bytes = match hex::decode(check_id_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => return TxResult::Malformed,
        };
        let mut check_key_arr = [0u8; 32];
        check_key_arr.copy_from_slice(&check_id_bytes);
        let check_key = xrpl_core::types::Hash256(check_key_arr);

        // Read the Check object
        let check_data = match sandbox.read(&check_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let check: serde_json::Value = match serde_json::from_slice(&check_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Only the creator or destination may cancel
        let creator = check.get("Account").and_then(|a| parse_account_id(a));
        let dest = check.get("Destination").and_then(|d| parse_account_id(d));

        let is_creator = creator.map_or(false, |c| c == tx.account);
        let is_dest = dest.map_or(false, |d| d == tx.account);

        if !is_creator && !is_dest {
            return TxResult::NoPermission;
        }

        // Decrement OwnerCount on the creator
        if let Some(creator_id) = creator {
            let creator_key = keylet::account_root_key(&creator_id);
            if let Some(data) = sandbox.read(&creator_key) {
                if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                    if count > 0 {
                        acct["OwnerCount"] =
                            serde_json::Value::Number((count - 1).into());
                    }
                    sandbox.write(creator_key, serde_json::to_vec(&acct).unwrap());
                }
            }
        }

        // Delete the Check
        sandbox.delete(check_key);

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

    fn make_state_with_accounts(
        accounts: &[(&[u8; 20], u64)],
    ) -> LedgerState {
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
        for (id, balance) in accounts {
            let acct = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(id),
                "Balance": balance.to_string(),
                "Sequence": 1,
                "OwnerCount": 0,
            });
            let key = keylet::account_root_key(id);
            state
                .state_map
                .insert(key, serde_json::to_vec(&acct).unwrap())
                .unwrap();
        }
        state
    }

    fn read_field_u64(sandbox: &Sandbox, account: &[u8; 20], field: &str) -> u64 {
        let key = keylet::account_root_key(account);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        v[field]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| v[field].as_u64())
            .unwrap_or(0)
    }

    #[test]
    fn check_create_and_cash() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create a check
        let create_tx = TxFields {
            account: sender,
            tx_type: "CheckCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "SendMax": "5000000",
            }),
        };

        assert_eq!(CheckCreateTransactor.preflight(&create_tx), TxResult::Success);
        assert_eq!(CheckCreateTransactor.preclaim(&create_tx, &sandbox), TxResult::Success);
        assert_eq!(CheckCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        // Check object should exist
        let check_key = keylet::check_key(&sender, 1);
        assert!(sandbox.exists(&check_key));

        // OwnerCount should be 1
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 1);

        // Cash the check
        let check_id_hex = hex::encode(check_key.0);
        let cash_tx = TxFields {
            account: dest,
            tx_type: "CheckCash".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "CheckID": check_id_hex,
                "Amount": "3000000",
            }),
        };

        assert_eq!(CheckCashTransactor.preflight(&cash_tx), TxResult::Success);
        assert_eq!(CheckCashTransactor.preclaim(&cash_tx, &sandbox), TxResult::Success);
        assert_eq!(CheckCashTransactor.do_apply(&cash_tx, &mut sandbox), TxResult::Success);

        // Check should be deleted
        assert!(!sandbox.exists(&check_key));

        // Sender balance: 50M - 3M = 47M
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 47_000_000);
        // Dest balance: 10M + 3M = 13M
        assert_eq!(read_field_u64(&sandbox, &dest, "Balance"), 13_000_000);
        // Sender OwnerCount back to 0
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 0);
    }

    #[test]
    fn check_create_and_cancel() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create a check
        let create_tx = TxFields {
            account: sender,
            tx_type: "CheckCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "SendMax": "5000000",
            }),
        };
        assert_eq!(CheckCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        let check_key = keylet::check_key(&sender, 1);
        assert!(sandbox.exists(&check_key));
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 1);

        // Cancel the check (as creator)
        let check_id_hex = hex::encode(check_key.0);
        let cancel_tx = TxFields {
            account: sender,
            tx_type: "CheckCancel".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "CheckID": check_id_hex,
            }),
        };

        assert_eq!(CheckCancelTransactor.preflight(&cancel_tx), TxResult::Success);
        assert_eq!(CheckCancelTransactor.preclaim(&cancel_tx, &sandbox), TxResult::Success);
        assert_eq!(CheckCancelTransactor.do_apply(&cancel_tx, &mut sandbox), TxResult::Success);

        // Check should be deleted
        assert!(!sandbox.exists(&check_key));
        // OwnerCount back to 0
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 0);
        // Balances unchanged (no fund transfer)
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 50_000_000);
        assert_eq!(read_field_u64(&sandbox, &dest, "Balance"), 10_000_000);
    }

    #[test]
    fn check_cash_exceeds_send_max() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create a check with SendMax of 5 XRP
        let create_tx = TxFields {
            account: sender,
            tx_type: "CheckCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "SendMax": "5000000",
            }),
        };
        CheckCreateTransactor.do_apply(&create_tx, &mut sandbox);

        let check_key = keylet::check_key(&sender, 1);
        let check_id_hex = hex::encode(check_key.0);

        // Try to cash more than SendMax
        let cash_tx = TxFields {
            account: dest,
            tx_type: "CheckCash".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "CheckID": check_id_hex,
                "Amount": "10000000",
            }),
        };

        assert_eq!(CheckCashTransactor.do_apply(&cash_tx, &mut sandbox), TxResult::Unfunded);

        // Check should still exist
        assert!(sandbox.exists(&check_key));
    }

    #[test]
    fn check_cancel_wrong_party() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let outsider = [0x03u8; 20];
        let state = make_state_with_accounts(&[
            (&sender, 50_000_000),
            (&dest, 10_000_000),
            (&outsider, 10_000_000),
        ]);

        let mut sandbox = Sandbox::new(&state);

        // Create a check
        let create_tx = TxFields {
            account: sender,
            tx_type: "CheckCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "SendMax": "5000000",
            }),
        };
        CheckCreateTransactor.do_apply(&create_tx, &mut sandbox);

        let check_key = keylet::check_key(&sender, 1);
        let check_id_hex = hex::encode(check_key.0);

        // An outsider tries to cancel — should fail
        let cancel_tx = TxFields {
            account: outsider,
            tx_type: "CheckCancel".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "CheckID": check_id_hex,
            }),
        };

        assert_eq!(
            CheckCancelTransactor.do_apply(&cancel_tx, &mut sandbox),
            TxResult::NoPermission
        );
        // Check should still exist
        assert!(sandbox.exists(&check_key));
    }
}
