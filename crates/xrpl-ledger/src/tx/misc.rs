//! Miscellaneous transaction types — SetRegularKey, SignerListSet, DepositPreauth, Clawback.
//!
//! Simple pass-through transactors for less common transaction types.
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

/// Helper: decode a 20-byte account ID from a JSON value.
///
/// Accepts 40-char hex (as the probe emits for hex-normalised ACCOUNT_FIELDS)
/// and base58 r-addresses. Clawback's holder is the nested `Amount.issuer`,
/// which the probe never hex-normalises -- decoding it hex-only returned None
/// and wrongly rejected the tx as Malformed.
fn decode_account_id(val: &serde_json::Value) -> Option<[u8; 20]> {
    crate::tx::offer::decode20(val.as_str()?)
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
        // No fee floor: a master-signed SetRegularKey on an account whose
        // lsfPasswordSpent is clear has a base fee of ZERO
        // (SetRegularKey::calculateBaseFee, SetRegularKey.cpp:29-49), so
        // Fee: "0" is valid for it. Any other fee-0 SetRegularKey is
        // telINSUF_FEE_P at the gate and never reaches a validated ledger.
        // RegularKey is optional — if absent, clears the key.
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

        const LSF_PASSWORD_SPENT: u64 = 0x0001_0000;
        const LSF_DISABLE_MASTER: u64 = 0x0010_0000;
        let flags = acct["Flags"].as_u64().unwrap_or(0);

        // `if (!minimumFee(app, ctx_.baseFee, fees, flags)) setFlag(
        // lsfPasswordSpent)` (SetRegularKey.cpp:71): the flag follows the
        // CALCULATED base fee, not the fee paid — zero iff the tx is signed
        // with the sender's master key and the flag is still clear.
        // #106696325 5B7E148E (finding 60) paid 12 drops and mainnet set it
        // all the same; we never set it (Flags 0 vs 65536, PRE-OK).
        if flags & LSF_PASSWORD_SPENT == 0 && Self::master_signed(tx) {
            acct["Flags"] = serde_json::Value::Number((flags | LSF_PASSWORD_SPENT).into());
        }

        if let Some(regular_key) = tx.fields.get("RegularKey") {
            // Set the regular key
            acct["RegularKey"] = regular_key.clone();
        } else {
            // Clearing the key with the master disabled needs a signer list
            // to fall back on (SetRegularKey.cpp:81-83).
            if flags & LSF_DISABLE_MASTER != 0
                && !sandbox.exists(&keylet::signers_key(&tx.account))
            {
                return TxResult::NoAlternativeKey;
            }
            // Clear the regular key
            if let Some(obj) = acct.as_object_mut() {
                obj.remove("RegularKey");
            }
        }

        sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
        TxResult::Success
    }
}

impl SetRegularKeyTransactor {
    /// Whether the transaction is signed with the sender's MASTER key:
    /// `calcAccountID(PublicKey(SigningPubKey)) == Account`
    /// (SetRegularKey.cpp:34-36). A multi-signed tx carries an empty
    /// SigningPubKey and a regular-key-signed one derives another account;
    /// neither is the free, password-spending form.
    fn master_signed(tx: &TxFields) -> bool {
        let Some(spk) = tx.fields.get("SigningPubKey").and_then(|v| v.as_str()) else {
            return false;
        };
        let Ok(bytes) = hex::decode(spk) else {
            return false;
        };
        // publicKeyType(): 33 bytes, secp256k1 (0x02/0x03) or ed25519 (0xED).
        if bytes.len() != 33 || !matches!(bytes[0], 0x02 | 0x03 | 0xED) {
            return false;
        }
        xrpl_core::crypto::signing::public_key_to_account_id(&bytes) == tx.account
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
        // The REAL keylet (rippled keylet::signers): 0x0053 || account || u32 0.
        // The previous fabricated key (no trailing SignerListID) pointed at a
        // key mainnet never touches — #106069820 C9DBE048 deletes a list and
        // we probed sandbox.exists at the wrong key, did nothing, fee-only
        // 1v3 (mainnet deletes the SignerList AND its owner-dir entry).
        let signer_list_key = keylet::signers_key(&tx.account);

        if quorum == 0 {
            // Quorum of 0 means delete the signer list
            if let Some(data) = sandbox.read(&signer_list_key) {
                let node_hint = serde_json::from_slice::<serde_json::Value>(&data)
                    .ok()
                    .and_then(|l| {
                        l.get("OwnerNode")
                            .and_then(|v| v.as_str())
                            .and_then(|h| u64::from_str_radix(h, 16).ok())
                    });
                sandbox.delete(signer_list_key);
                crate::ledger::directory::owner_dir_remove(
                    sandbox, &tx.account, &signer_list_key, node_hint, false,
                );
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

            // Mainnet shape: SignerListID 0 and lsfOneOwnerCount (65536,
            // featureMultiSignReserve — every list charges ONE owner unit).
            let signer_list_obj = serde_json::json!({
                "LedgerEntryType": "SignerList",
                "Flags": 65536,
                "SignerQuorum": quorum,
                "SignerEntries": signers,
                "SignerListID": 0,
                "OwnerNode": "0",
            });

            sandbox.write(signer_list_key, serde_json::to_vec(&signer_list_obj).unwrap());

            // Increment OwnerCount + owner-dir entry only for a NEW list
            if !already_exists {
                crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &signer_list_key);
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
        // Amount is required (IOU or MPT amount to claw back)
        let Some(amount) = tx.fields.get("Amount") else {
            return TxResult::Malformed;
        };
        // MPT arm (Clawback.cpp preflightHelper<MPTIssue>): Holder required,
        // may not be the issuer itself, value positive within the signed cap.
        if let Some((_, v)) = crate::tx::mpt::parse_mpt_amount(amount) {
            let Some(holder) = tx.fields.get("Holder").and_then(|h| decode_account_id(h)) else {
                return TxResult::Malformed;
            };
            if holder == tx.account {
                return TxResult::Malformed;
            }
            if v == 0 || v > crate::tx::mpt::MAX_MPT_AMOUNT {
                return TxResult::BadAmount;
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
        let amount = match tx.fields.get("Amount") {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // MPT arm (Clawback.cpp preclaimHelper<MPTIssue> + applyHelper).
        // rippled's preclaim order decides the code: missing issuance or
        // missing holder MPToken → tecOBJECT_NOT_FOUND; no lsfMPTCanClawback
        // or wrong issuer → tecNO_PERMISSION; a holder whose spendable
        // balance is zero → tecINSUFFICIENT_FUNDS (3EC225FD, l106259185 —
        // the tx still consumes its Ticket, which is all its meta shows).
        // Unported, no specimen: tecPSEUDO_ACCOUNT / tecAMM_ACCOUNT holders.
        if let Some((mptid, value)) = crate::tx::mpt::parse_mpt_amount(amount) {
            let holder = match tx.fields.get("Holder").and_then(|h| decode_account_id(h)) {
                Some(h) => h,
                None => return TxResult::Malformed,
            };
            if !sandbox.exists(&keylet::account_root_key(&holder)) {
                return TxResult::NoAccount;
            }
            let issuance_key = keylet::mpt_issuance_key(&mptid);
            let Some(mut issuance) = sandbox
                .read(&issuance_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else {
                return TxResult::ObjectNotFound;
            };
            let iflags = issuance.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
            if iflags & crate::tx::mpt::LSF_MPT_CAN_CLAWBACK == 0 {
                return TxResult::NoPermission;
            }
            let issuer_ok = issuance
                .get("Issuer")
                .and_then(|v| v.as_str())
                .and_then(crate::tx::offer::decode20)
                .map(|i| i == tx.account)
                .unwrap_or(false);
            if !issuer_ok {
                return TxResult::NoPermission;
            }
            let token_key = keylet::mptoken_key(&issuance_key, &holder);
            let Some(mut token) = sandbox
                .read(&token_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else {
                return TxResult::ObjectNotFound;
            };
            let spendable = token
                .get("MPTAmount")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if spendable == 0 {
                return TxResult::InsufficientFunds;
            }
            // Claw min(spendable, amount): holder MPTAmount down (zero is
            // omitted — SoeDefault), issuance OutstandingAmount down
            // (SoeRequired — "0" stays written).
            let claw = spendable.min(value);
            let rest = spendable - claw;
            if rest == 0 {
                if let Some(o) = token.as_object_mut() {
                    o.remove("MPTAmount");
                }
            } else {
                token["MPTAmount"] = serde_json::Value::String(rest.to_string());
            }
            sandbox.write(token_key, serde_json::to_vec(&token).unwrap_or_default());
            let outstanding = issuance
                .get("OutstandingAmount")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            issuance["OutstandingAmount"] =
                serde_json::Value::String(outstanding.saturating_sub(claw).to_string());
            sandbox.write(issuance_key, serde_json::to_vec(&issuance).unwrap_or_default());
            return TxResult::Success;
        }

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

    /// A master-signed SetRegularKey on a fresh account is the free form:
    /// its base fee is zero, so `!minimumFee(...)` SPENDS the password
    /// (lsfPasswordSpent) whatever fee the sender actually paid
    /// (SetRegularKey.cpp:71; #106696325 5B7E148E paid 12 drops, finding 60).
    #[test]
    fn master_signed_regular_key_spends_the_password_whatever_fee_is_paid() {
        let master_pk = [0x02u8; 33];
        let alice = xrpl_core::crypto::signing::public_key_to_account_id(&master_pk);
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
                "SigningPubKey": hex::encode_upper(master_pk),
            }),
        };
        assert_eq!(SetRegularKeyTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
        let data = sandbox.read(&keylet::account_root_key(&alice)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Flags"].as_u64().unwrap_or(0) & 0x0001_0000, 0x0001_0000);
        assert_eq!(v["RegularKey"].as_str().unwrap(), hex::encode([0xFFu8; 20]));

        // Fee 0 is the canonical free form — valid, same effect.
        let free = TxFields { fee: 0, ..tx.clone() };
        assert_eq!(SetRegularKeyTransactor.preflight(&free), TxResult::Success);
    }

    /// Signed with a regular key (SigningPubKey derives someone else) or
    /// multi-signed (empty SigningPubKey): the base fee is the normal one and
    /// the password stays armed.
    #[test]
    fn regular_or_multi_signed_regular_key_keeps_the_password_armed() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);
        for spk in [hex::encode_upper([0x03u8; 33]), String::new()] {
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
                    "SigningPubKey": spk,
                }),
            };
            assert_eq!(SetRegularKeyTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
            let data = sandbox.read(&keylet::account_root_key(&alice)).unwrap();
            let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
            assert_eq!(v["Flags"].as_u64().unwrap_or(0) & 0x0001_0000, 0);
        }
    }

    /// Clearing the key with the master disabled and no signer list is
    /// tecNO_ALTERNATIVE_KEY (SetRegularKey.cpp:81-83).
    #[test]
    fn clear_regular_key_with_master_disabled_needs_a_signer_list() {
        let alice = [0x01u8; 20];
        let mut state = make_state(&[(alice, 50_000_000)]);
        let key = keylet::account_root_key(&alice);
        let mut acct: serde_json::Value =
            serde_json::from_slice(&state.state_map.lookup(&key).unwrap()).unwrap();
        acct["Flags"] = serde_json::json!(0x0010_0000u64);
        acct["RegularKey"] = serde_json::json!(hex::encode([0xAAu8; 20]));
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "SetRegularKey".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"SigningPubKey": hex::encode_upper([0x03u8; 33])}),
        };
        assert_eq!(SetRegularKeyTransactor.do_apply(&tx, &mut sandbox), TxResult::NoAlternativeKey);
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
    fn clawback_decodes_base58_holder_issuer() {
        // The probe never hex-normalises the nested Amount.issuer, so Clawback
        // sees the holder as a base58 r-address. A hex-only decode returned
        // None -> Malformed, wrongly dropping a tx mainnet applies (a spurious
        // reject is consensus-relevant). offer::decode20 handles base58.
        let issuer = [0x01u8; 20];
        let holder_addr = "rNR6vtb85KWJyTs86mcHB2UqNVwgnaBGRF";
        let holder =
            crate::tx::offer::decode20(holder_addr).expect("base58 holder must decode");
        let state = make_state(&[(issuer, 50_000_000), (holder, 50_000_000)]);
        let mut sandbox = Sandbox::new(&state);

        let currency_code = {
            let mut code = [0u8; 20];
            code[12] = b'U';
            code[13] = b'S';
            code[14] = b'D';
            code
        };
        let line_key = keylet::ripple_state_key(&issuer, &holder, &currency_code);
        let (low, high) = if issuer < holder {
            (hex::encode(issuer), hex::encode(holder))
        } else {
            (hex::encode(holder), hex::encode(issuer))
        };
        let bal = if issuer < holder { "-100" } else { "100" };
        let line_obj = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Balance": {"currency": "USD", "issuer": "0000000000000000000000000000000000000000", "value": bal},
            "LowLimit": {"currency": "USD", "issuer": low, "value": "0"},
            "HighLimit": {"currency": "USD", "issuer": high, "value": "1000"},
            "Flags": 0,
        });
        sandbox.write(line_key, serde_json::to_vec(&line_obj).unwrap());

        let tx = TxFields {
            account: issuer,
            tx_type: "Clawback".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            // base58 issuer, exactly as the probe feeds it.
            fields: serde_json::json!({
                "Amount": { "currency": "USD", "issuer": holder_addr, "value": "30" }
            }),
        };
        // Was Malformed before the fix (base58 holder failed the hex-only decode).
        assert_eq!(
            ClawbackTransactor.do_apply(&tx, &mut sandbox),
            TxResult::Success
        );
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

// ---------------------------------------------------------------------------
// DID — DIDSet / DIDDelete (DIDSet.cpp / DIDDelete.cpp; ⚠ no mainnet
// specimen in any fixture window — blind source port, the scout is the
// verifier when one lands).
// ---------------------------------------------------------------------------

/// Shared create tail (DIDSet.cpp addSLE): reserve on the CURRENT (post-fee)
/// balance, insert, owner-dir link, OwnerCount+1.
fn add_owned_object(
    sandbox: &mut Sandbox,
    owner: &[u8; 20],
    key: xrpl_core::types::Hash256,
    mut obj: serde_json::Value,
) -> TxResult {
    let acct_key = keylet::account_root_key(owner);
    let Some(acct) = crate::tx::offer::json_at(sandbox, &acct_key) else {
        return TxResult::NoAccount;
    };
    let balance = acct["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
    if balance < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
        return TxResult::InsufficientReserve;
    }
    let node = crate::ledger::directory::owner_dir_insert(sandbox, owner, &key);
    obj["OwnerNode"] = serde_json::Value::String(format!("{node:x}"));
    sandbox.write(key, serde_json::to_vec(&obj).unwrap_or_default());
    crate::tx::offer::owner_count_add(sandbox, owner, 1);
    TxResult::Success
}

/// Shared delete tail (DIDDelete.cpp deleteSLE): dir unlink (keep_root),
/// OwnerCount-1, erase. tecNO_ENTRY when the object is absent.
fn delete_owned_object(
    sandbox: &mut Sandbox,
    owner: &[u8; 20],
    key: xrpl_core::types::Hash256,
) -> TxResult {
    let Some(obj) = crate::tx::offer::json_at(sandbox, &key) else {
        return TxResult::NoEntry;
    };
    let hint = obj
        .get("OwnerNode")
        .and_then(|v| v.as_str())
        .and_then(|h| u64::from_str_radix(h, 16).ok());
    crate::ledger::directory::owner_dir_remove(sandbox, owner, &key, hint, true);
    crate::tx::offer::owner_count_add(sandbox, owner, -1);
    sandbox.delete(key);
    TxResult::Success
}

pub struct DIDSetTransactor;

impl Transactor for DIDSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "DIDSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // At least one of the three payload fields must appear (preflight
        // temEMPTY_DID is a tem; the tec arm is handled in do_apply).
        if ["URI", "DIDDocument", "Data"].iter().all(|f| tx.fields.get(f).is_none()) {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let key = keylet::did_key(&tx.account);
        let update = |obj: &mut serde_json::Value| {
            for f in ["URI", "DIDDocument", "Data"] {
                if let Some(v) = tx.fields.get(f).and_then(|v| v.as_str()) {
                    if v.is_empty() {
                        obj.as_object_mut().map(|o| o.remove(f));
                    } else {
                        obj[f] = serde_json::Value::String(v.to_string());
                    }
                }
            }
        };
        if let Some(mut did) = crate::tx::offer::json_at(sandbox, &key) {
            update(&mut did);
            if ["URI", "DIDDocument", "Data"].iter().all(|f| did.get(f).is_none()) {
                return TxResult::EmptyDid;
            }
            sandbox.write(key, serde_json::to_vec(&did).unwrap_or_default());
            return TxResult::Success;
        }
        let mut did = serde_json::json!({
            "LedgerEntryType": "DID",
            "Account": hex::encode(tx.account),
        });
        update(&mut did);
        if ["URI", "DIDDocument", "Data"].iter().all(|f| did.get(f).is_none()) {
            return TxResult::EmptyDid; // fixEmptyDID
        }
        add_owned_object(sandbox, &tx.account, key, did)
    }
}

pub struct DIDDeleteTransactor;

impl Transactor for DIDDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "DIDDelete" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        delete_owned_object(sandbox, &tx.account, keylet::did_key(&tx.account))
    }
}

// ---------------------------------------------------------------------------
// PermissionedDomain — Set / Delete (⚠ blind source port, no specimen).
// ---------------------------------------------------------------------------

pub struct PermissionedDomainSetTransactor;

impl Transactor for PermissionedDomainSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PermissionedDomainSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("AcceptedCredentials").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // credentials::makeSorted orders by (issuer, credential type).
        let mut creds: Vec<serde_json::Value> = tx
            .fields
            .get("AcceptedCredentials")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        creds.sort_by_key(|c| {
            let inner = c.get("Credential").unwrap_or(c);
            (
                inner
                    .get("Issuer")
                    .and_then(|v| v.as_str())
                    .and_then(crate::tx::offer::decode20)
                    .unwrap_or([0u8; 20]),
                inner
                    .get("CredentialType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase(),
            )
        });
        let sorted = serde_json::Value::Array(creds);

        if let Some(domain_hex) = tx.fields.get("DomainID").and_then(|v| v.as_str()) {
            // Modify: replace the credential list on the existing domain.
            let Ok(kb) = hex::decode(domain_hex) else { return TxResult::Malformed };
            let Ok(karr) = <[u8; 32]>::try_from(kb.as_slice()) else {
                return TxResult::Malformed;
            };
            let key = xrpl_core::types::Hash256(karr);
            let Some(mut pd) = crate::tx::offer::json_at(sandbox, &key) else {
                // preclaim in rippled: missing domain -> tecNO_ENTRY
                return TxResult::NoEntry;
            };
            let owner_ok = pd
                .get("Owner")
                .and_then(|v| v.as_str())
                .and_then(crate::tx::offer::decode20)
                .map(|o| o == tx.account)
                .unwrap_or(false);
            if !owner_ok {
                return TxResult::NoPermission;
            }
            pd["AcceptedCredentials"] = sorted;
            sandbox.write(key, serde_json::to_vec(&pd).unwrap_or_default());
            return TxResult::Success;
        }
        let seq = if tx.uses_ticket() { tx.ticket_seq.unwrap_or(0) } else { tx.sequence };
        let key = keylet::permissioned_domain_key(&tx.account, seq);
        let pd = serde_json::json!({
            "LedgerEntryType": "PermissionedDomain",
            "Owner": hex::encode(tx.account),
            "Sequence": seq,
            "AcceptedCredentials": sorted,
        });
        add_owned_object(sandbox, &tx.account, key, pd)
    }
}

pub struct PermissionedDomainDeleteTransactor;

impl Transactor for PermissionedDomainDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PermissionedDomainDelete" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("DomainID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(kb) = tx
            .fields
            .get("DomainID")
            .and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s).ok())
        else {
            return TxResult::Malformed;
        };
        let Ok(karr) = <[u8; 32]>::try_from(kb.as_slice()) else { return TxResult::Malformed };
        let key = xrpl_core::types::Hash256(karr);
        let Some(pd) = crate::tx::offer::json_at(sandbox, &key) else {
            return TxResult::NoEntry;
        };
        let owner_ok = pd
            .get("Owner")
            .and_then(|v| v.as_str())
            .and_then(crate::tx::offer::decode20)
            .map(|o| o == tx.account)
            .unwrap_or(false);
        if !owner_ok {
            return TxResult::NoPermission;
        }
        delete_owned_object(sandbox, &tx.account, key)
    }
}
