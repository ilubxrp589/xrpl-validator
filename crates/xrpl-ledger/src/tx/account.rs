//! AccountSet and AccountDelete transactions.
//!
//! AccountSet: modify account flags, domain, transfer rate, etc.
//! AccountDelete: remove an account (requires OwnerCount=0, high fee).
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

/// AccountSet transactor.
pub struct AccountSetTransactor;

impl Transactor for AccountSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AccountSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
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
        let acct_key = keylet::account_root_key(&tx.account);
        let data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut acct: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Apply SetFlag
        if let Some(flag) = tx.fields.get("SetFlag").and_then(|f| f.as_u64()) {
            if flag >= 32 {
                return TxResult::Malformed;
            }
            let current = acct["Flags"].as_u64().unwrap_or(0);
            acct["Flags"] = serde_json::Value::Number((current | (1u64 << flag)).into());
        }

        // Apply ClearFlag
        if let Some(flag) = tx.fields.get("ClearFlag").and_then(|f| f.as_u64()) {
            if flag >= 32 {
                return TxResult::Malformed;
            }
            let current = acct["Flags"].as_u64().unwrap_or(0);
            acct["Flags"] = serde_json::Value::Number((current & !(1u64 << flag)).into());
        }

        // Apply optional fields
        for field in ["Domain", "EmailHash", "MessageKey", "TransferRate", "TickSize"] {
            if let Some(val) = tx.fields.get(field) {
                acct[field] = val.clone();
            }
        }

        sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
        TxResult::Success
    }
}

/// AccountDelete transactor.
pub struct AccountDeleteTransactor;

impl Transactor for AccountDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AccountDelete" {
            return TxResult::Malformed;
        }
        if tx.fee < 2_000_000 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Destination").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        let data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let acct: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Must have OwnerCount == 0
        let owner_count = acct["OwnerCount"].as_u64().unwrap_or(0);
        if owner_count > 0 {
            return TxResult::NoPermission;
        }

        // Account sequence + 256 must be <= current ledger sequence
        let acct_seq = acct["Sequence"].as_u64().unwrap_or(0) as u32;
        let ledger_seq = sandbox.base().header.sequence;
        if acct_seq.saturating_add(256) > ledger_seq {
            return TxResult::NoPermission;
        }

        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        let data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let acct: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let balance = acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // Transfer remaining balance to destination
        let dest_hex = match tx.fields.get("Destination").and_then(|d| d.as_str()) {
            Some(h) => h,
            None => return TxResult::Malformed,
        };
        let dest_bytes = match hex::decode(dest_hex) {
            Ok(b) if b.len() == 20 => b,
            _ => return TxResult::Malformed,
        };
        let mut dest_id = [0u8; 20];
        dest_id.copy_from_slice(&dest_bytes);
        let dest_key = keylet::account_root_key(&dest_id);

        // Destination must exist
        let dest_data = match sandbox.read(&dest_key) {
            Some(d) => d,
            None => return TxResult::NoDst,
        };
        let mut dest: serde_json::Value = match serde_json::from_slice(&dest_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let dest_balance = dest["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        dest["Balance"] =
            serde_json::Value::String(dest_balance.checked_add(balance).unwrap_or(u64::MAX).to_string());
        sandbox.write(dest_key, serde_json::to_vec(&dest).unwrap());

        // Delete the account
        sandbox.delete(acct_key);

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

    fn make_state(id: &[u8; 20], balance: u64) -> LedgerState {
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
            "Flags": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        state
    }

    #[test]
    fn account_set_flag() {
        let acct = [0x01u8; 20];
        let state = make_state(&acct, 50_000_000);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "AccountSet".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"SetFlag": 8}), // asfDefaultRipple
        };

        assert_eq!(AccountSetTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let key = keylet::account_root_key(&acct);
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Flags"].as_u64().unwrap() & (1 << 8), 1 << 8);
    }

    #[test]
    fn account_delete() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state(&alice, 50_000_000);
        // Add bob
        let bob_acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(bob),
            "Balance": "10000000",
            "Sequence": 1,
            "OwnerCount": 0,
            "Flags": 0,
        });
        state.state_map.insert(keylet::account_root_key(&bob), serde_json::to_vec(&bob_acct).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "AccountDelete".to_string(),
            fee: 2_000_000,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"Destination": hex::encode(bob)}),
        };

        assert_eq!(AccountDeleteTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Alice should be gone
        assert!(!sandbox.exists(&keylet::account_root_key(&alice)));

        // Bob should have Alice's balance
        let bob_data = sandbox.read(&keylet::account_root_key(&bob)).unwrap();
        let bv: serde_json::Value = serde_json::from_slice(&bob_data).unwrap();
        // Bob had 10M, Alice had 50M (fee already deducted by apply_common before do_apply)
        assert_eq!(bv["Balance"].as_str().unwrap(), "60000000");
    }
}
