//! Credential transaction types — CredentialCreate, CredentialDelete, CredentialAccept.
//!
//! Simple state object CRUD for on-ledger credentials.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::shamap::hash::sha512_half;

/// Compute a deterministic key for a Credential object.
/// `key = SHA512Half(0x0044 || subject_account || issuer_account || credential_type_hash)`
///
/// Space key 0x0044 ('D') is used for credentials (simplified — not in rippled mainline yet).
fn credential_key(subject: &[u8; 20], issuer: &[u8; 20], credential_type: &[u8]) -> xrpl_core::types::Hash256 {
    let type_hash = sha512_half(credential_type);
    let mut buf = Vec::with_capacity(2 + 20 + 20 + 32);
    buf.extend_from_slice(&[0x00, 0x44]); // 'D' space
    buf.extend_from_slice(subject);
    buf.extend_from_slice(issuer);
    buf.extend_from_slice(&type_hash.0);
    sha512_half(&buf)
}

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
// CredentialCreate
// ---------------------------------------------------------------------------

/// CredentialCreate transactor — create a credential object in state.
///
/// The issuer (tx sender) creates a credential for a subject. The credential
/// is not yet accepted until the subject sends a CredentialAccept.
pub struct CredentialCreateTransactor;

impl Transactor for CredentialCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "CredentialCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // Subject is required
        if tx.fields.get("Subject").is_none() {
            return TxResult::Malformed;
        }
        // CredentialType is required
        if tx.fields.get("CredentialType").is_none() {
            return TxResult::Malformed;
        }
        // Bug 16: Issuer (tx.account) cannot issue a credential to themselves
        if let Some(subject_val) = tx.fields.get("Subject") {
            if let Some(hex_str) = subject_val.as_str() {
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
        let subject = match tx.fields.get("Subject").and_then(|v| decode_account_id(v)) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };

        let cred_type_str = match tx.fields.get("CredentialType").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };

        let cred_key = credential_key(&subject, &tx.account, cred_type_str.as_bytes());

        // Check if credential already exists
        if sandbox.exists(&cred_key) {
            return TxResult::NoPermission; // duplicate credential
        }

        let mut cred_obj = serde_json::json!({
            "LedgerEntryType": "Credential",
            "Subject": hex::encode(subject),
            "Issuer": hex::encode(tx.account),
            "CredentialType": cred_type_str,
            "Accepted": false,
            "Flags": 0,
        });

        // Optional: URI
        if let Some(uri) = tx.fields.get("URI") {
            cred_obj["URI"] = uri.clone();
        }

        // Optional: Expiration
        if let Some(exp) = tx.fields.get("Expiration") {
            cred_obj["Expiration"] = exp.clone();
        }

        sandbox.write(cred_key, serde_json::to_vec(&cred_obj).unwrap());

        // Increment OwnerCount for the issuer
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
// CredentialDelete
// ---------------------------------------------------------------------------

/// CredentialDelete transactor — delete a credential from state.
///
/// Can be deleted by the subject, the issuer, or anyone if expired.
pub struct CredentialDeleteTransactor;

impl Transactor for CredentialDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "CredentialDelete" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // Subject, Issuer, and CredentialType are required to locate the credential
        if tx.fields.get("Subject").is_none()
            || tx.fields.get("Issuer").is_none()
            || tx.fields.get("CredentialType").is_none()
        {
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
        let subject = match tx.fields.get("Subject").and_then(|v| decode_account_id(v)) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let issuer = match tx.fields.get("Issuer").and_then(|v| decode_account_id(v)) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let cred_type_str = match tx.fields.get("CredentialType").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };

        let cred_key = credential_key(&subject, &issuer, cred_type_str.as_bytes());

        if !sandbox.exists(&cred_key) {
            return TxResult::NoEntry;
        }

        // Permission check: sender must be subject or issuer
        let sender_hex = hex::encode(tx.account);
        let subject_hex = hex::encode(subject);
        let issuer_hex = hex::encode(issuer);
        if sender_hex != subject_hex && sender_hex != issuer_hex {
            return TxResult::NoPermission;
        }

        sandbox.delete(cred_key);

        // Decrement OwnerCount for the issuer
        let issuer_acct_key = keylet::account_root_key(&issuer);
        if let Some(data) = sandbox.read(&issuer_acct_key) {
            if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                if count > 0 {
                    acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
                }
                sandbox.write(issuer_acct_key, serde_json::to_vec(&acct).unwrap());
            }
        }

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// CredentialAccept
// ---------------------------------------------------------------------------

/// CredentialAccept transactor — subject accepts a credential, marking it active.
pub struct CredentialAcceptTransactor;

impl Transactor for CredentialAcceptTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "CredentialAccept" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // Issuer and CredentialType are required
        if tx.fields.get("Issuer").is_none() || tx.fields.get("CredentialType").is_none() {
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
        // The subject is the tx sender
        let subject = tx.account;

        let issuer = match tx.fields.get("Issuer").and_then(|v| decode_account_id(v)) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let cred_type_str = match tx.fields.get("CredentialType").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };

        let cred_key = credential_key(&subject, &issuer, cred_type_str.as_bytes());

        let cred_data = match sandbox.read(&cred_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };

        let mut cred: serde_json::Value = match serde_json::from_slice(&cred_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Bug 17: If already accepted, return NoPermission
        if cred["Accepted"].as_bool().unwrap_or(false) {
            return TxResult::NoPermission;
        }

        // Mark as accepted
        cred["Accepted"] = serde_json::Value::Bool(true);

        // Set the lsfAccepted flag (0x00010000)
        let flags = cred["Flags"].as_u64().unwrap_or(0);
        cred["Flags"] = serde_json::Value::Number((flags | 0x00010000).into());

        sandbox.write(cred_key, serde_json::to_vec(&cred).unwrap());

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
    fn credential_create_and_accept() {
        let issuer = [0x01u8; 20];
        let subject = [0x02u8; 20];
        let state = make_state(&[(issuer, 50_000_000), (subject, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Issuer creates credential
        let create_tx = TxFields {
            account: issuer,
            tx_type: "CredentialCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Subject": hex::encode(subject),
                "CredentialType": "KYC",
                "URI": "https://example.com/kyc",
            }),
        };

        assert_eq!(CredentialCreateTransactor.preflight(&create_tx), TxResult::Success);
        assert_eq!(CredentialCreateTransactor.preclaim(&create_tx, &sandbox), TxResult::Success);
        assert_eq!(CredentialCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        let cred_key = credential_key(&subject, &issuer, b"KYC");
        assert!(sandbox.exists(&cred_key));
        assert_eq!(read_owner_count(&sandbox, &issuer), 1);

        // Verify not yet accepted
        let data = sandbox.read(&cred_key).unwrap();
        let cred: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(cred["Accepted"].as_bool().unwrap(), false);

        // Subject accepts
        let accept_tx = TxFields {
            account: subject,
            tx_type: "CredentialAccept".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Issuer": hex::encode(issuer),
                "CredentialType": "KYC",
            }),
        };

        assert_eq!(CredentialAcceptTransactor.preflight(&accept_tx), TxResult::Success);
        assert_eq!(CredentialAcceptTransactor.preclaim(&accept_tx, &sandbox), TxResult::Success);
        assert_eq!(CredentialAcceptTransactor.do_apply(&accept_tx, &mut sandbox), TxResult::Success);

        // Verify accepted
        let data = sandbox.read(&cred_key).unwrap();
        let cred: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(cred["Accepted"].as_bool().unwrap(), true);
        assert_ne!(cred["Flags"].as_u64().unwrap() & 0x00010000, 0);
    }

    #[test]
    fn credential_delete_by_subject() {
        let issuer = [0x01u8; 20];
        let subject = [0x02u8; 20];
        let state = make_state(&[(issuer, 50_000_000), (subject, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create
        let create_tx = TxFields {
            account: issuer,
            tx_type: "CredentialCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Subject": hex::encode(subject),
                "CredentialType": "AML",
            }),
        };
        CredentialCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Subject deletes
        let delete_tx = TxFields {
            account: subject,
            tx_type: "CredentialDelete".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Subject": hex::encode(subject),
                "Issuer": hex::encode(issuer),
                "CredentialType": "AML",
            }),
        };

        assert_eq!(CredentialDeleteTransactor.preflight(&delete_tx), TxResult::Success);
        assert_eq!(CredentialDeleteTransactor.preclaim(&delete_tx, &sandbox), TxResult::Success);
        assert_eq!(CredentialDeleteTransactor.do_apply(&delete_tx, &mut sandbox), TxResult::Success);

        let cred_key = credential_key(&subject, &issuer, b"AML");
        assert!(!sandbox.exists(&cred_key));
        assert_eq!(read_owner_count(&sandbox, &issuer), 0);
    }

    #[test]
    fn credential_delete_unauthorized_fails() {
        let issuer = [0x01u8; 20];
        let subject = [0x02u8; 20];
        let stranger = [0x03u8; 20];
        let state = make_state(&[
            (issuer, 50_000_000),
            (subject, 50_000_000),
            (stranger, 50_000_000),
        ]);

        let mut sandbox = Sandbox::new(&state);

        // Create
        let create_tx = TxFields {
            account: issuer,
            tx_type: "CredentialCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Subject": hex::encode(subject),
                "CredentialType": "KYC",
            }),
        };
        CredentialCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Stranger tries to delete — should fail
        let delete_tx = TxFields {
            account: stranger,
            tx_type: "CredentialDelete".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Subject": hex::encode(subject),
                "Issuer": hex::encode(issuer),
                "CredentialType": "KYC",
            }),
        };

        assert_eq!(CredentialDeleteTransactor.do_apply(&delete_tx, &mut sandbox), TxResult::NoPermission);
    }

    #[test]
    fn credential_accept_nonexistent_fails() {
        let subject = [0x02u8; 20];
        let state = make_state(&[(subject, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: subject,
            tx_type: "CredentialAccept".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Issuer": hex::encode([0x01u8; 20]),
                "CredentialType": "KYC",
            }),
        };
        assert_eq!(CredentialAcceptTransactor.do_apply(&tx, &mut sandbox), TxResult::NoEntry);
    }

    #[test]
    fn credential_create_preflight_rejects_missing_fields() {
        let issuer = [0x01u8; 20];

        // Missing Subject
        let tx1 = TxFields {
            account: issuer,
            tx_type: "CredentialCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"CredentialType": "KYC"}),
        };
        assert_eq!(CredentialCreateTransactor.preflight(&tx1), TxResult::Malformed);

        // Missing CredentialType
        let tx2 = TxFields {
            account: issuer,
            tx_type: "CredentialCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"Subject": hex::encode([0x02u8; 20])}),
        };
        assert_eq!(CredentialCreateTransactor.preflight(&tx2), TxResult::Malformed);
    }

    #[test]
    fn credential_duplicate_create_fails() {
        let issuer = [0x01u8; 20];
        let subject = [0x02u8; 20];
        let state = make_state(&[(issuer, 50_000_000), (subject, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        let create_tx = TxFields {
            account: issuer,
            tx_type: "CredentialCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Subject": hex::encode(subject),
                "CredentialType": "KYC",
            }),
        };

        assert_eq!(CredentialCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);
        // Second create with same params should fail
        assert_eq!(CredentialCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::NoPermission);
    }
}
