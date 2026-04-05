//! Miscellaneous transaction types — SetRegularKey, SignerListSet, DepositPreauth, Clawback.
//!
//! Simple pass-through transactors for less common transaction types.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

/// Helper: decode a 20-byte hex account ID from a JSON value.
fn decode_account_id(val: &serde_json::Value) -> Option<[u8; 20]> {
    let hex_str = val.as_str()?;
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

// ---------------------------------------------------------------------------
// SetRegularKey
// ---------------------------------------------------------------------------

/// SetRegularKey transactor — set or clear the RegularKey on an account.
pub struct SetRegularKeyTransactor;

impl Transactor for SetRegularKeyTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "SetRegularKey" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // RegularKey is optional — if absent, clears the key
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

        if let Some(regular_key) = tx.fields.get("RegularKey") {
            // Set the regular key
            acct["RegularKey"] = regular_key.clone();
        } else {
            // Clear the regular key
            if let Some(obj) = acct.as_object_mut() {
                obj.remove("RegularKey");
            }
        }

        sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// SignerListSet
// ---------------------------------------------------------------------------

/// SignerListSet transactor — set or remove a multi-signing signer list.
pub struct SignerListSetTransactor;

impl Transactor for SignerListSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "SignerListSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // SignerQuorum is required
        let quorum = match tx.fields.get("SignerQuorum").and_then(|v| v.as_u64()) {
            Some(q) => q,
            None => return TxResult::Malformed,
        };
        // Bug 12: If quorum > 0, SignerEntries must be present and non-empty
        if quorum > 0 {
            let entries = tx.fields.get("SignerEntries");
            let is_empty = match entries {
                None => true,
                Some(v) => match v.as_array() {
                    None => true,
                    Some(arr) => arr.is_empty(),
                },
            };
            if is_empty {
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
        let quorum = match tx.fields.get("SignerQuorum").and_then(|v| v.as_u64()) {
            Some(q) => q,
            None => return TxResult::Malformed,
        };

        let acct_key = keylet::account_root_key(&tx.account);

        // Use a deterministic key for the signer list: same space as owner dir
        // but with a special marker. Simplified: hash(account + "SignerList").
        let signer_list_key = {
            use crate::shamap::hash::sha512_half;
            let mut buf = Vec::with_capacity(30);
            buf.extend_from_slice(&[0x00, 0x53]); // 'S' for SignerList
            buf.extend_from_slice(&tx.account);
            sha512_half(&buf)
        };

        if quorum == 0 {
            // Quorum of 0 means delete the signer list
            if sandbox.exists(&signer_list_key) {
                sandbox.delete(signer_list_key);
                // Decrement OwnerCount
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
        } else {
            let signers = tx.fields.get("SignerEntries")
                .cloned()
                .unwrap_or(serde_json::Value::Array(vec![]));

            let already_exists = sandbox.exists(&signer_list_key);

            let signer_list_obj = serde_json::json!({
                "LedgerEntryType": "SignerList",
                "SignerQuorum": quorum,
                "SignerEntries": signers,
                "OwnerNode": "0",
            });

            sandbox.write(signer_list_key, serde_json::to_vec(&signer_list_obj).unwrap());

            // Increment OwnerCount only if this is a new signer list
            if !already_exists {
                if let Some(data) = sandbox.read(&acct_key) {
                    if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                        let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                        acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
                        sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
                    }
                }
            }
        }

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// DepositPreauth
// ---------------------------------------------------------------------------

/// DepositPreauth transactor — authorize or deauthorize a sender for preauthorized deposits.
pub struct DepositPreauthTransactor;

impl Transactor for DepositPreauthTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "DepositPreauth" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // Must have exactly one of Authorize or Unauthorize
        let has_auth = tx.fields.get("Authorize").is_some();
        let has_unauth = tx.fields.get("Unauthorize").is_some();
        if has_auth == has_unauth {
            // Both present or both absent
            return TxResult::Malformed;
        }
        // Bug 14: Cannot authorize yourself
        if let Some(auth_val) = tx.fields.get("Authorize") {
            if let Some(hex_str) = auth_val.as_str() {
                if hex_str == hex::encode(tx.account) {
                    return TxResult::Malformed;
                }
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
        if let Some(auth_val) = tx.fields.get("Authorize") {
            // Authorize: create a DepositPreauth entry
            let authorized = match decode_account_id(auth_val) {
                Some(id) => id,
                None => return TxResult::Malformed,
            };

            let dp_key = keylet::deposit_preauth_key(&tx.account, &authorized);

            if sandbox.exists(&dp_key) {
                // Already authorized — this is a no-op in rippled but we return success
                return TxResult::Success;
            }

            let dp_obj = serde_json::json!({
                "LedgerEntryType": "DepositPreauth",
                "Account": hex::encode(tx.account),
                "Authorize": hex::encode(authorized),
            });

            sandbox.write(dp_key, serde_json::to_vec(&dp_obj).unwrap());

            // Increment OwnerCount
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(data) = sandbox.read(&acct_key) {
                if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                    acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
                    sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
                }
            }
        } else if let Some(unauth_val) = tx.fields.get("Unauthorize") {
            // Unauthorize: delete the DepositPreauth entry
            let unauthorized = match decode_account_id(unauth_val) {
                Some(id) => id,
                None => return TxResult::Malformed,
            };

            let dp_key = keylet::deposit_preauth_key(&tx.account, &unauthorized);

            if !sandbox.exists(&dp_key) {
                return TxResult::NoEntry;
            }

            sandbox.delete(dp_key);

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

// ---------------------------------------------------------------------------
// Clawback
// ---------------------------------------------------------------------------

/// Clawback transactor — issuer claws back IOU tokens from a holder.
///
/// Simplified: modifies the RippleState (trust line) balance between issuer and holder.
pub struct ClawbackTransactor;

impl Transactor for ClawbackTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "Clawback" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // Amount is required (IOU amount to claw back)
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
        let amount = match tx.fields.get("Amount") {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // Amount must be an IOU object: {currency, issuer, value}
        let currency_str = match amount.get("currency").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => return TxResult::Malformed,
        };
        let holder = match amount.get("issuer").and_then(|i| decode_account_id(i)) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let clawback_value: f64 = match amount.get("value").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
            Some(v) if v > 0.0 => v,
            _ => return TxResult::BadAmount,
        };

        // Build the currency code (20 bytes)
        let currency = {
            let mut code = [0u8; 20];
            if currency_str.len() == 3 {
                code[12] = currency_str.as_bytes()[0];
                code[13] = currency_str.as_bytes()[1];
                code[14] = currency_str.as_bytes()[2];
            } else if currency_str.len() == 40 {
                if let Ok(bytes) = hex::decode(currency_str) {
                    if bytes.len() == 20 {
                        code.copy_from_slice(&bytes);
                    }
                }
            }
            code
        };

        // The issuer (sender of clawback) and the holder define the trust line
        let line_key = keylet::ripple_state_key(&tx.account, &holder, &currency);

        let line_data = match sandbox.read(&line_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };

        let mut line: serde_json::Value = match serde_json::from_slice(&line_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // RippleState Balance is from low account's perspective.
        // Positive balance means low account holds tokens issued by high account.
        // Negative balance means high account holds tokens issued by low account.
        let balance_str = line["Balance"]["value"].as_str().unwrap_or("0");
        let balance: f64 = balance_str.parse().unwrap_or(0.0);

        // Determine direction: if issuer (tx.account) is the low account,
        // the holder's balance is negative from low's perspective.
        let issuer_is_low = tx.account < holder;
        let holder_balance = if issuer_is_low { -balance } else { balance };

        if holder_balance <= 0.0 {
            // Holder has no tokens to claw back
            return TxResult::NoEntry;
        }

        // Claw back up to the holder's balance
        let actual_clawback = clawback_value.min(holder_balance);
        let new_holder_balance = holder_balance - actual_clawback;

        let new_balance = if issuer_is_low { -new_holder_balance } else { new_holder_balance };
        line["Balance"]["value"] = serde_json::Value::String(format!("{}", new_balance));

        sandbox.write(line_key, serde_json::to_vec(&line).unwrap());

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// Stub transactors for less common / newer transaction types.
// These do fee deduction (via apply_common) but skip type-specific effects.
// The tx_engine verifies them via generic fee verification.
// ---------------------------------------------------------------------------

macro_rules! stub_transactor {
    ($name:ident, $tx_type:expr) => {
        pub struct $name;
        impl Transactor for $name {
            fn preflight(&self, tx: &TxFields) -> TxResult {
                if tx.tx_type != $tx_type { return TxResult::Malformed; }
                if tx.fee == 0 { return TxResult::BadFee; }
                TxResult::Success
            }
            fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
                let k = keylet::account_root_key(&tx.account);
                if !sandbox.exists(&k) { return TxResult::NoAccount; }
                TxResult::Success
            }
            fn do_apply(&self, _tx: &TxFields, _sandbox: &mut Sandbox) -> TxResult {
                TxResult::Success
            }
        }
    };
}

stub_transactor!(TicketCreateTransactor, "TicketCreate");
stub_transactor!(OracleSetTransactor, "OracleSet");
stub_transactor!(OracleDeleteTransactor, "OracleDelete");
stub_transactor!(DIDSetTransactor, "DIDSet");
stub_transactor!(DIDDeleteTransactor, "DIDDelete");
stub_transactor!(XChainCreateBridgeTransactor, "XChainCreateBridge");
stub_transactor!(XChainCreateClaimIDTransactor, "XChainCreateClaimID");
stub_transactor!(XChainCommitTransactor, "XChainCommit");
stub_transactor!(XChainClaimTransactor, "XChainClaim");
stub_transactor!(XChainModifyBridgeTransactor, "XChainModifyBridge");
stub_transactor!(XChainAccountCreateCommitTransactor, "XChainAccountCreateCommit");
stub_transactor!(XChainAddClaimAttestationTransactor, "XChainAddClaimAttestation");
stub_transactor!(XChainAddAccountCreateAttestationTransactor, "XChainAddAccountCreateAttestation");
stub_transactor!(PermissionedDomainSetTransactor, "PermissionedDomainSet");
stub_transactor!(PermissionedDomainDeleteTransactor, "PermissionedDomainDelete");
stub_transactor!(AMMClawbackTransactor, "AMMClawback");
stub_transactor!(MPTokenIssuanceCreateTransactor, "MPTokenIssuanceCreate");
stub_transactor!(MPTokenIssuanceDestroyTransactor, "MPTokenIssuanceDestroy");
stub_transactor!(MPTokenIssuanceSetTransactor, "MPTokenIssuanceSet");
stub_transactor!(MPTokenAuthorizeTransactor, "MPTokenAuthorize");

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

    fn read_owner_count(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        v["OwnerCount"].as_u64().unwrap_or(0)
    }

    #[test]
    fn set_regular_key() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "SetRegularKey".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "RegularKey": hex::encode([0xFFu8; 20]),
            }),
        };

        assert_eq!(SetRegularKeyTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(SetRegularKeyTransactor.preclaim(&tx, &sandbox), TxResult::Success);
        assert_eq!(SetRegularKeyTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Verify key was set
        let acct_key = keylet::account_root_key(&alice);
        let data = sandbox.read(&acct_key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["RegularKey"].as_str().unwrap(), hex::encode([0xFFu8; 20]));
    }

    #[test]
    fn clear_regular_key() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // First set
        let set_tx = TxFields {
            account: alice,
            tx_type: "SetRegularKey".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"RegularKey": hex::encode([0xAAu8; 20])}),
        };
        SetRegularKeyTransactor.do_apply(&set_tx, &mut sandbox);

        // Then clear (no RegularKey field)
        let clear_tx = TxFields {
            account: alice,
            tx_type: "SetRegularKey".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({}),
        };
        assert_eq!(SetRegularKeyTransactor.do_apply(&clear_tx, &mut sandbox), TxResult::Success);

        let acct_key = keylet::account_root_key(&alice);
        let data = sandbox.read(&acct_key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert!(v.get("RegularKey").is_none());
    }

    #[test]
    fn signer_list_set_and_remove() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Set signer list with quorum=2
        let set_tx = TxFields {
            account: alice,
            tx_type: "SignerListSet".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "SignerQuorum": 2,
                "SignerEntries": [
                    {"SignerEntry": {"Account": hex::encode([0x02u8; 20]), "SignerWeight": 1}},
                    {"SignerEntry": {"Account": hex::encode([0x03u8; 20]), "SignerWeight": 1}},
                ]
            }),
        };

        assert_eq!(SignerListSetTransactor.preflight(&set_tx), TxResult::Success);
        assert_eq!(SignerListSetTransactor.do_apply(&set_tx, &mut sandbox), TxResult::Success);
        assert_eq!(read_owner_count(&sandbox, &alice), 1);

        // Remove signer list (quorum=0)
        let remove_tx = TxFields {
            account: alice,
            tx_type: "SignerListSet".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"SignerQuorum": 0}),
        };

        assert_eq!(SignerListSetTransactor.do_apply(&remove_tx, &mut sandbox), TxResult::Success);
        assert_eq!(read_owner_count(&sandbox, &alice), 0);
    }

    #[test]
    fn deposit_preauth_authorize_and_unauthorize() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let state = make_state(&[(alice, 50_000_000), (bob, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Authorize bob
        let auth_tx = TxFields {
            account: alice,
            tx_type: "DepositPreauth".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Authorize": hex::encode(bob),
            }),
        };

        assert_eq!(DepositPreauthTransactor.preflight(&auth_tx), TxResult::Success);
        assert_eq!(DepositPreauthTransactor.do_apply(&auth_tx, &mut sandbox), TxResult::Success);

        let dp_key = keylet::deposit_preauth_key(&alice, &bob);
        assert!(sandbox.exists(&dp_key));
        assert_eq!(read_owner_count(&sandbox, &alice), 1);

        // Unauthorize bob
        let unauth_tx = TxFields {
            account: alice,
            tx_type: "DepositPreauth".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Unauthorize": hex::encode(bob),
            }),
        };

        assert_eq!(DepositPreauthTransactor.do_apply(&unauth_tx, &mut sandbox), TxResult::Success);
        assert!(!sandbox.exists(&dp_key));
        assert_eq!(read_owner_count(&sandbox, &alice), 0);
    }

    #[test]
    fn deposit_preauth_both_fields_rejected() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let tx = TxFields {
            account: alice,
            tx_type: "DepositPreauth".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Authorize": hex::encode(bob),
                "Unauthorize": hex::encode(bob),
            }),
        };
        assert_eq!(DepositPreauthTransactor.preflight(&tx), TxResult::Malformed);
    }

    #[test]
    fn clawback_reduces_trust_line_balance() {
        let issuer = [0x01u8; 20]; // low account
        let holder = [0x02u8; 20]; // high account
        let state = make_state(&[(issuer, 50_000_000), (holder, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create a trust line with holder balance of 100 USD
        // issuer < holder, so Balance is -100 from low's perspective
        // (negative means high account holds tokens from low account's issuance)
        let currency_code = {
            let mut code = [0u8; 20];
            code[12] = b'U';
            code[13] = b'S';
            code[14] = b'D';
            code
        };
        let line_key = keylet::ripple_state_key(&issuer, &holder, &currency_code);
        let line_obj = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Balance": {"currency": "USD", "issuer": "0000000000000000000000000000000000000000", "value": "-100"},
            "LowLimit": {"currency": "USD", "issuer": hex::encode(issuer), "value": "0"},
            "HighLimit": {"currency": "USD", "issuer": hex::encode(holder), "value": "1000"},
            "Flags": 0,
        });
        sandbox.write(line_key, serde_json::to_vec(&line_obj).unwrap());

        // Issuer claws back 30 USD
        let tx = TxFields {
            account: issuer,
            tx_type: "Clawback".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": {
                    "currency": "USD",
                    "issuer": hex::encode(holder),
                    "value": "30"
                }
            }),
        };

        assert_eq!(ClawbackTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(ClawbackTransactor.preclaim(&tx, &sandbox), TxResult::Success);
        assert_eq!(ClawbackTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Balance should now be -70 (holder has 70 USD left)
        let data = sandbox.read(&line_key).unwrap();
        let line: serde_json::Value = serde_json::from_slice(&data).unwrap();
        let balance: f64 = line["Balance"]["value"].as_str().unwrap().parse().unwrap();
        assert!((balance - (-70.0)).abs() < 0.001);
    }

    #[test]
    fn clawback_no_trust_line_fails() {
        let issuer = [0x01u8; 20];
        let state = make_state(&[(issuer, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: issuer,
            tx_type: "Clawback".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": {
                    "currency": "USD",
                    "issuer": hex::encode([0x02u8; 20]),
                    "value": "50"
                }
            }),
        };
        assert_eq!(ClawbackTransactor.do_apply(&tx, &mut sandbox), TxResult::NoEntry);
    }
}
