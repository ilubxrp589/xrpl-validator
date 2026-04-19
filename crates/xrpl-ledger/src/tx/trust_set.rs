//! TrustSet transaction — create or modify trust lines (RippleState).
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

/// TrustSet transactor.
pub struct TrustSetTransactor;

impl TrustSetTransactor {
    fn extract_limit_amount(tx: &TxFields) -> Option<(&str, [u8; 20])> {
        let limit = tx.fields.get("LimitAmount")?;
        let currency = limit.get("currency")?.as_str()?;
        let issuer_hex = limit.get("issuer")?.as_str()?;
        let issuer_bytes = hex::decode(issuer_hex).ok()?;
        if issuer_bytes.len() != 20 {
            return None;
        }
        let mut issuer = [0u8; 20];
        issuer.copy_from_slice(&issuer_bytes);
        Some((currency, issuer))
    }

    /// Build the 20-byte currency code from a 3-char ISO code.
    fn currency_code(iso: &str) -> [u8; 20] {
        let mut code = [0u8; 20];
        if iso.len() == 3 {
            // Standard currency: 12 zero bytes + 3 ASCII bytes + 5 zero bytes
            code[12] = iso.as_bytes()[0];
            code[13] = iso.as_bytes()[1];
            code[14] = iso.as_bytes()[2];
        } else if iso.len() == 40 {
            // Hex currency code
            if let Ok(bytes) = hex::decode(iso) {
                if bytes.len() == 20 {
                    code.copy_from_slice(&bytes);
                }
            }
        }
        code
    }
}

impl Transactor for TrustSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "TrustSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("LimitAmount").is_none() {
            return TxResult::Malformed;
        }
        // Reject self-trust-lines: account cannot trust itself
        if let Some((_, issuer)) = Self::extract_limit_amount(tx) {
            if tx.account == issuer {
                return TxResult::Malformed;
            }
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
        let (currency_str, issuer) = match Self::extract_limit_amount(tx) {
            Some(v) => v,
            None => return TxResult::Malformed,
        };

        let currency = Self::currency_code(currency_str);
        let line_key = keylet::ripple_state_key(&tx.account, &issuer, &currency);

        if let Some(data) = sandbox.read(&line_key) {
            // Trust line exists — update the correct side (LowLimit or HighLimit).
            // RippleState has LowLimit (lower account) and HighLimit (higher account).
            // Determine which side the sender is on by lexicographic comparison.
            if let Ok(mut line) = serde_json::from_slice::<serde_json::Value>(&data) {
                let new_value = tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0");
                if tx.account < issuer {
                    // Sender is the low side
                    line["LowLimit"]["value"] = serde_json::Value::String(new_value.to_string());
                } else {
                    // Sender is the high side
                    line["HighLimit"]["value"] = serde_json::Value::String(new_value.to_string());
                }
                if let Some(qin) = tx.fields.get("QualityIn") {
                    line["QualityIn"] = qin.clone();
                }
                if let Some(qout) = tx.fields.get("QualityOut") {
                    line["QualityOut"] = qout.clone();
                }
                sandbox.write(line_key, serde_json::to_vec(&line).expect("serializing valid JSON Value"));
            }
        } else {
            // Create new trust line
            let line_obj = serde_json::json!({
                "LedgerEntryType": "RippleState",
                "Balance": {
                    "currency": currency_str,
                    "issuer": "0000000000000000000000000000000000000000",
                    "value": "0"
                },
                "LowLimit": {
                    "currency": currency_str,
                    "issuer": hex::encode(if tx.account < issuer { tx.account } else { issuer }),
                    "value": if tx.account < issuer {
                        tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0").to_string()
                    } else {
                        "0".to_string()
                    }
                },
                "HighLimit": {
                    "currency": currency_str,
                    "issuer": hex::encode(if tx.account < issuer { issuer } else { tx.account }),
                    "value": if tx.account >= issuer {
                        tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0").to_string()
                    } else {
                        "0".to_string()
                    }
                },
                "Flags": 0,
            });
            sandbox.write(line_key, serde_json::to_vec(&line_obj).expect("serializing valid JSON Value"));

            // Increment OwnerCount for both accounts
            for id in [&tx.account, &issuer] {
                let acct_key = keylet::account_root_key(id);
                if let Some(data) = sandbox.read(&acct_key) {
                    if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                        let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                        acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
                        sandbox.write(acct_key, serde_json::to_vec(&acct).expect("serializing valid JSON Value"));
                    }
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

    fn make_state(accounts: &[([u8; 20], u64)]) -> LedgerState {
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
            state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        }
        state
    }

    #[test]
    fn create_trust_line() {
        let alice = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = make_state(&[(alice, 50_000_000), (issuer, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "TrustSet".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "LimitAmount": {
                    "currency": "USD",
                    "issuer": hex::encode(issuer),
                    "value": "1000"
                }
            }),
        };

        assert_eq!(TrustSetTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(TrustSetTransactor.preclaim(&tx, &sandbox), TxResult::Success);
        assert_eq!(TrustSetTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Trust line should exist
        let currency = TrustSetTransactor::currency_code("USD");
        let line_key = keylet::ripple_state_key(&alice, &issuer, &currency);
        assert!(sandbox.exists(&line_key));
    }
}
