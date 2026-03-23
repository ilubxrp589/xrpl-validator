//! Payment Channel transaction types: PaymentChannelCreate, PaymentChannelClaim,
//! PaymentChannelFund.
//!
//! Payment channels allow fast, off-ledger XRP transfers. The sender locks XRP
//! into a channel object, and the destination can claim from it using signed
//! authorizations. The sender can add more XRP to the channel.
//!
//! PaymentChannelCreate: Lock XRP into a new channel at pay_channel_key(account, sequence).
//! PaymentChannelClaim:  Claim XRP from a channel. If fully claimed, delete channel.
//! PaymentChannelFund:   Add more XRP to an existing channel.

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
// PaymentChannelCreate
// ---------------------------------------------------------------------------

/// PaymentChannelCreate transactor — lock XRP into a new payment channel.
pub struct PaymentChannelCreateTransactor;

impl Transactor for PaymentChannelCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PaymentChannelCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Destination").is_none() {
            return TxResult::Malformed;
        }
        let amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        if amount == 0 {
            return TxResult::BadAmount;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }

        // Check sender has enough balance for Amount + fee
        if let Some(data) = sandbox.read(&acct_key) {
            if let Ok(acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                let balance = acct["Balance"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let amount = tx.fields.get("Amount").and_then(|a| parse_drops(a)).unwrap_or(0);
                if balance < amount.checked_add(tx.fee).unwrap_or(u64::MAX) {
                    return TxResult::Unfunded;
                }
            }
        }

        // Destination must exist
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
        let amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        let dest = match tx.fields.get("Destination").and_then(|d| parse_account_id(d)) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };

        // Deduct Amount from sender
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender_acct: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let sender_balance = sender_acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if sender_balance < amount {
            return TxResult::Unfunded;
        }

        sender_acct["Balance"] =
            serde_json::Value::String((sender_balance - amount).to_string());

        // Increment OwnerCount
        let owner_count = sender_acct["OwnerCount"].as_u64().unwrap_or(0);
        sender_acct["OwnerCount"] = serde_json::Value::Number((owner_count + 1).into());
        sandbox.write(sender_key, serde_json::to_vec(&sender_acct).unwrap());

        // Create PayChannel object
        let seq = if tx.uses_ticket() {
            tx.ticket_seq.unwrap_or(0)
        } else {
            tx.sequence
        };
        let channel_key = keylet::pay_channel_key(&tx.account, seq);

        let channel_obj = serde_json::json!({
            "LedgerEntryType": "PayChannel",
            "Account": hex::encode(tx.account),
            "Destination": hex::encode(dest),
            "Amount": amount.to_string(),
            "Balance": "0",
            "Sequence": seq,
        });
        sandbox.write(channel_key, serde_json::to_vec(&channel_obj).unwrap());

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// PaymentChannelClaim
// ---------------------------------------------------------------------------

/// PaymentChannelClaim transactor — claim XRP from a payment channel.
/// If the channel is fully claimed, it is deleted.
pub struct PaymentChannelClaimTransactor;

impl Transactor for PaymentChannelClaimTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PaymentChannelClaim" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Channel").is_none() {
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
        // Decode Channel (hex of the 32-byte keylet)
        let channel_hex = match tx.fields.get("Channel").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let channel_bytes = match hex::decode(channel_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => return TxResult::Malformed,
        };
        let mut channel_key_arr = [0u8; 32];
        channel_key_arr.copy_from_slice(&channel_bytes);
        let channel_key = xrpl_core::types::Hash256(channel_key_arr);

        // Read the PayChannel object
        let channel_data = match sandbox.read(&channel_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let mut channel: serde_json::Value = match serde_json::from_slice(&channel_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Get channel details
        let dest = match channel.get("Destination").and_then(|d| parse_account_id(d)) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let creator = match channel.get("Account").and_then(|a| parse_account_id(a)) {
            Some(c) => c,
            None => return TxResult::Malformed,
        };

        // Only destination or creator may claim
        if tx.account != dest && tx.account != creator {
            return TxResult::NoPermission;
        }

        // Parse the claim Balance (new total claimed so far)
        let claim_balance = match tx.fields.get("Balance").and_then(|b| parse_drops(b)) {
            Some(b) => b,
            None => return TxResult::Malformed,
        };

        let channel_amount = channel.get("Amount")
            .and_then(|a| parse_drops(a))
            .unwrap_or(0);

        let current_balance = channel.get("Balance")
            .and_then(|b| parse_drops(b))
            .unwrap_or(0);

        // New claim must be >= current claimed balance and <= channel amount
        if claim_balance < current_balance {
            return TxResult::Malformed;
        }
        if claim_balance > channel_amount {
            return TxResult::Unfunded;
        }

        // Amount to transfer = new claim - current balance
        let transfer = claim_balance - current_balance;

        // Credit destination — fail if destination account can't be read
        let dest_key = keylet::account_root_key(&dest);
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
            serde_json::Value::String((dest_balance + transfer).to_string());
        sandbox.write(dest_key, serde_json::to_vec(&dest_acct).unwrap());

        // Check if channel is fully claimed
        if claim_balance == channel_amount {
            // Delete the channel
            sandbox.delete(channel_key);

            // Decrement OwnerCount on creator
            let creator_key = keylet::account_root_key(&creator);
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
        } else {
            // Update channel Balance
            channel["Balance"] = serde_json::Value::String(claim_balance.to_string());
            sandbox.write(channel_key, serde_json::to_vec(&channel).unwrap());
        }

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// PaymentChannelFund
// ---------------------------------------------------------------------------

/// PaymentChannelFund transactor — add more XRP to an existing payment channel.
pub struct PaymentChannelFundTransactor;

impl Transactor for PaymentChannelFundTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PaymentChannelFund" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Channel").is_none() {
            return TxResult::Malformed;
        }
        let amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        if amount == 0 {
            return TxResult::BadAmount;
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
        let add_amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // Decode Channel
        let channel_hex = match tx.fields.get("Channel").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let channel_bytes = match hex::decode(channel_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => return TxResult::Malformed,
        };
        let mut channel_key_arr = [0u8; 32];
        channel_key_arr.copy_from_slice(&channel_bytes);
        let channel_key = xrpl_core::types::Hash256(channel_key_arr);

        // Read channel
        let channel_data = match sandbox.read(&channel_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let mut channel: serde_json::Value = match serde_json::from_slice(&channel_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Only the channel creator can fund it
        let creator = match channel.get("Account").and_then(|a| parse_account_id(a)) {
            Some(c) => c,
            None => return TxResult::Malformed,
        };
        if creator != tx.account {
            return TxResult::NoPermission;
        }

        // Deduct from sender's balance
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender_acct: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let sender_balance = sender_acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if sender_balance < add_amount {
            return TxResult::Unfunded;
        }

        sender_acct["Balance"] =
            serde_json::Value::String((sender_balance - add_amount).to_string());
        sandbox.write(sender_key, serde_json::to_vec(&sender_acct).unwrap());

        // Increase channel Amount (checked to prevent overflow)
        let channel_amount = channel.get("Amount")
            .and_then(|a| parse_drops(a))
            .unwrap_or(0);
        let new_channel_amount = match channel_amount.checked_add(add_amount) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        channel["Amount"] =
            serde_json::Value::String(new_channel_amount.to_string());
        sandbox.write(channel_key, serde_json::to_vec(&channel).unwrap());

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
    fn pay_channel_create_and_partial_claim() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create a payment channel with 10 XRP
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "10000000",
            }),
        };

        assert_eq!(PaymentChannelCreateTransactor.preflight(&create_tx), TxResult::Success);
        assert_eq!(PaymentChannelCreateTransactor.preclaim(&create_tx, &sandbox), TxResult::Success);
        assert_eq!(PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        // Sender balance: 50M - 10M = 40M
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 40_000_000);
        // OwnerCount should be 1
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 1);

        // Channel should exist
        let channel_key = keylet::pay_channel_key(&sender, 1);
        assert!(sandbox.exists(&channel_key));

        // Partial claim — claim 3 XRP from the 10 XRP channel
        let channel_hex = hex::encode(channel_key.0);
        let claim_tx = TxFields {
            account: dest,
            tx_type: "PaymentChannelClaim".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Balance": "3000000",
            }),
        };

        assert_eq!(PaymentChannelClaimTransactor.preflight(&claim_tx), TxResult::Success);
        assert_eq!(PaymentChannelClaimTransactor.preclaim(&claim_tx, &sandbox), TxResult::Success);
        assert_eq!(PaymentChannelClaimTransactor.do_apply(&claim_tx, &mut sandbox), TxResult::Success);

        // Dest balance: 10M + 3M = 13M
        assert_eq!(read_field_u64(&sandbox, &dest, "Balance"), 13_000_000);
        // Channel should still exist (partially claimed)
        assert!(sandbox.exists(&channel_key));

        // Read channel to verify Balance updated
        let ch_data = sandbox.read(&channel_key).unwrap();
        let ch: serde_json::Value = serde_json::from_slice(&ch_data).unwrap();
        assert_eq!(ch["Balance"].as_str().unwrap(), "3000000");
        assert_eq!(ch["Amount"].as_str().unwrap(), "10000000");
    }

    #[test]
    fn pay_channel_full_claim_deletes_channel() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create channel with 5 XRP
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "5000000",
            }),
        };
        PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox);

        let channel_key = keylet::pay_channel_key(&sender, 1);
        let channel_hex = hex::encode(channel_key.0);

        // Fully claim the channel
        let claim_tx = TxFields {
            account: dest,
            tx_type: "PaymentChannelClaim".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Balance": "5000000",
            }),
        };

        assert_eq!(PaymentChannelClaimTransactor.do_apply(&claim_tx, &mut sandbox), TxResult::Success);

        // Channel should be deleted
        assert!(!sandbox.exists(&channel_key));
        // Dest balance: 10M + 5M = 15M
        assert_eq!(read_field_u64(&sandbox, &dest, "Balance"), 15_000_000);
        // Sender OwnerCount back to 0
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 0);
    }

    #[test]
    fn pay_channel_fund_increases_amount() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create channel with 5 XRP
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "5000000",
            }),
        };
        PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Sender balance: 50M - 5M = 45M
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 45_000_000);

        let channel_key = keylet::pay_channel_key(&sender, 1);
        let channel_hex = hex::encode(channel_key.0);

        // Fund the channel with 3 more XRP
        let fund_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelFund".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Amount": "3000000",
            }),
        };

        assert_eq!(PaymentChannelFundTransactor.preflight(&fund_tx), TxResult::Success);
        assert_eq!(PaymentChannelFundTransactor.preclaim(&fund_tx, &sandbox), TxResult::Success);
        assert_eq!(PaymentChannelFundTransactor.do_apply(&fund_tx, &mut sandbox), TxResult::Success);

        // Sender balance: 45M - 3M = 42M
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 42_000_000);

        // Channel Amount should be 5M + 3M = 8M
        let ch_data = sandbox.read(&channel_key).unwrap();
        let ch: serde_json::Value = serde_json::from_slice(&ch_data).unwrap();
        assert_eq!(ch["Amount"].as_str().unwrap(), "8000000");
    }

    #[test]
    fn pay_channel_fund_wrong_party() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create channel
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "5000000",
            }),
        };
        PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox);

        let channel_key = keylet::pay_channel_key(&sender, 1);
        let channel_hex = hex::encode(channel_key.0);

        // Destination tries to fund — should fail (only creator can fund)
        let fund_tx = TxFields {
            account: dest,
            tx_type: "PaymentChannelFund".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Amount": "3000000",
            }),
        };

        assert_eq!(
            PaymentChannelFundTransactor.do_apply(&fund_tx, &mut sandbox),
            TxResult::NoPermission
        );
    }
}
