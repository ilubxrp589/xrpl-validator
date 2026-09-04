//! Credential transaction types — CredentialCreate, CredentialDelete, CredentialAccept.
//!
//! Simple state object CRUD for on-ledger credentials.
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
use crate::shamap::hash::sha512_half;

/// Compute a deterministic key for a Credential object.
/// `key = SHA512Half(0x0044 || subject_account || issuer_account || credential_type_hash)`
///
/// Space key 0x0044 ('D') is used for credentials (simplified — not in rippled mainline yet).
/// rippled `keylet::credential` = indexHash(LedgerNameSpace::Credential,
/// subject, issuer, credType): ONE flat hash over the namespace and the three
/// fields, with credType as its RAW bytes. Hashing the type separately, or
/// hashing the hex TEXT of the type, both yield a key that can never match a
/// credential the network created — so a duplicate would go unnoticed.
fn credential_key(subject: &[u8; 20], issuer: &[u8; 20], credential_type: &[u8]) -> xrpl_core::types::Hash256 {
    let mut buf = Vec::with_capacity(2 + 20 + 20 + credential_type.len());
    buf.extend_from_slice(&[0x00, 0x44]); // 'D'
    buf.extend_from_slice(subject);
    buf.extend_from_slice(issuer);
    buf.extend_from_slice(credential_type);
    sha512_half(&buf)
}

/// CredentialType travels as hex text in our tx JSON; the keylet wants the
/// bytes it encodes.
fn cred_type_bytes(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap_or_else(|_| s.as_bytes().to_vec())
}

/// Decode an account ID that may arrive as 20-byte hex OR as a base58
/// r-address. Subject and Issuer are not among the fields callers normalise to
/// hex, so a hex-only decoder rejected every real CredentialCreate as
/// malformed before it could reach the duplicate check.
fn decode_account_id(val: &serde_json::Value) -> Option<[u8; 20]> {
    crate::tx::offer::decode20(val.as_str()?)
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
        // Self-issuance is legal: CredentialCreate::preflight checks only
        // Subject presence and the URI/CredentialType sizes, with no
        // subject != account rule. #105784451 8AA123A9 issues to itself and
        // mainnet reaches preclaim, returning tecDUPLICATE.
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

        let cred_key = credential_key(&subject, &tx.account, &cred_type_bytes(cred_type_str));

        // Check if credential already exists
        if sandbox.exists(&cred_key) {
            return TxResult::Duplicate;
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

        // The credential joins BOTH owner directories but is owned only by the
        // issuer — rippled CredentialCreate.cpp:147-174: "Added to both dirs,
        // owned only by issuer. CredentialAccept will transfer ownership to
        // subject." So the issuer's dirInsert is unconditional and carries the
        // single adjustOwnerCount(+1); the subject's insert adds no reserve.
        //
        // A SELF-issued credential (subject == account) skips the subject
        // insert entirely and is marked lsfAccepted instead.
        //
        // We wrote the credential and bumped OwnerCount but touched neither
        // directory. #105909285 68872086F0B4 issues to rPnDmSuS: mainnet
        // Modifies the subject's dir root 6F370348 AND a tail page of the
        // issuer's multi-page dir (1D0093BB) — 4 nodes to our 2.
        const LSF_ACCEPTED: u64 = 0x0001_0000;
        // Finding 150: the credential carries its directory pages —
        // `sleCred->setFieldU64(sfIssuerNode, *page)` after the issuer's
        // dirInsert, `sfSubjectNode` after the subject's (CredentialCreate.cpp
        // doApply; no SubjectNode on a self-issued credential). Both are
        // UInt64 fields with two-byte headers, ten bytes each: without them
        // our object is 20 bytes short of mainnet's — #106744392
        // 7958D8E901FF (rhUaQmNP → rsiPEXNs, SCPO_BASIC) and #106744275
        // 763A54D94565, 109 bytes against 129, the diff starting right
        // after Expiration.
        let issuer_page = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &cred_key);
        cred_obj["IssuerNode"] = serde_json::Value::String(format!("{issuer_page:x}"));
        if subject == tx.account {
            cred_obj["Flags"] = serde_json::json!(LSF_ACCEPTED);
            cred_obj["Accepted"] = serde_json::json!(true);
        } else {
            let subject_page = crate::ledger::directory::owner_dir_insert(sandbox, &subject, &cred_key);
            cred_obj["SubjectNode"] = serde_json::Value::String(format!("{subject_page:x}"));
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
        // rippled needs at least ONE of Subject/Issuer, not both
        // (CredentialDelete.cpp preflight): whichever is absent defaults to the
        // sending Account in preclaim. Requiring both rejected every real
        // self-issued delete as malformed.
        //
        // #105909624 87483CD4BCF1, #105911834 E246CD9AE20F and #105912291
        // DD6078118B99 all carry Subject + CredentialType and NO Issuer — the
        // sender IS the issuer. Mainnet answers tecNO_ENTRY (the credential is
        // simply gone); we answered temMALFORMED and never reached preclaim.
        if tx.fields.get("Subject").is_none() && tx.fields.get("Issuer").is_none() {
            return TxResult::Malformed;
        }
        // CredentialType must be present, non-empty and <= 64 bytes
        // (kMaxCredentialTypeLength, Protocol.h:221).
        match tx.fields.get("CredentialType").and_then(|v| v.as_str()) {
            None => return TxResult::Malformed,
            Some(ct) => {
                let n = cred_type_bytes(ct).len();
                if n == 0 || n > 64 {
                    return TxResult::Malformed;
                }
            }
        }
        // Not modelled: rippled's temINVALID_ACCOUNT_ID for a zeroed Subject or
        // Issuer — we have no such TxResult and no case exercises it.
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        // rippled CredentialDelete::preclaim resolves the pair with
        // `value_or(account)` and answers tecNO_ENTRY when the credential is
        // absent — BEFORE any permission test.
        let subject = tx
            .fields
            .get("Subject")
            .and_then(decode_account_id)
            .unwrap_or(tx.account);
        let issuer = tx
            .fields
            .get("Issuer")
            .and_then(decode_account_id)
            .unwrap_or(tx.account);
        let Some(ct) = tx.fields.get("CredentialType").and_then(|v| v.as_str()) else {
            return TxResult::Malformed;
        };
        if !sandbox.exists(&credential_key(&subject, &issuer, &cred_type_bytes(ct))) {
            return TxResult::NoEntry;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Same `value_or(account)` defaulting as preclaim — an absent Subject
        // or Issuer means the sender fills that role, it is not malformed.
        let subject = tx
            .fields
            .get("Subject")
            .and_then(decode_account_id)
            .unwrap_or(tx.account);
        let issuer = tx
            .fields
            .get("Issuer")
            .and_then(decode_account_id)
            .unwrap_or(tx.account);
        let cred_type_str = match tx.fields.get("CredentialType").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };

        let cred_key = credential_key(&subject, &issuer, &cred_type_bytes(cred_type_str));

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

        // A CREDENTIAL SITS IN **TWO** OWNER DIRECTORIES, and which party pays
        // its reserve depends on whether it has been ACCEPTED. `deleteSLE`
        // (CredentialHelpers.cpp:74-124):
        //     delSLE(issuer,  sfIssuerNode,  !accepted || (subject == issuer));
        //     if (subject != issuer)
        //         delSLE(subject, sfSubjectNode, accepted);
        //     view.erase(sleCredential);
        // where delSLE does `dirRemove(ownerDir(account), page, key, false)` and
        // adjusts that account's OwnerCount by -1 only when it is the OWNER.
        //
        // We deleted the object and decremented the ISSUER unconditionally —
        // no directory unlinking at all, and the wrong party once accepted.
        // The reserve MOVES to the subject on acceptance, so an accepted
        // credential must charge the subject, not the issuer.
        //
        // #105898053 F6EB8CFF: 2 mutations against mainnet's 6 — the credential
        // and one more DELETE (an emptied page), three DirectoryNode
        // modifications and one AccountRoot.
        //
        // `sfIssuerNode`/`sfSubjectNode` are the stored page hints; passing them
        // is what lets the removal find the right page instead of walking.
        let cred = sandbox
            .read(&cred_key)
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok());
        let hint = |c: &serde_json::Value, f: &str| -> Option<u64> {
            match c.get(f) {
                Some(serde_json::Value::String(h)) => u64::from_str_radix(h, 16).ok(),
                Some(serde_json::Value::Number(n)) => n.as_u64(),
                _ => None,
            }
        };
        let accepted = cred
            .as_ref()
            .map(|c| c["Flags"].as_u64().unwrap_or(0) & 0x0001_0000 != 0)
            .unwrap_or(false);

        sandbox.delete(cred_key);

        if let Some(c) = &cred {
            let ih = hint(c, "IssuerNode");
            crate::ledger::directory::owner_dir_remove(sandbox, &issuer, &cred_key, ih, false);
            if subject != issuer {
                let sh = hint(c, "SubjectNode");
                crate::ledger::directory::owner_dir_remove(sandbox, &subject, &cred_key, sh, false);
            }
        }

        // The reserve follows acceptance: the issuer carries an UNACCEPTED
        // credential (and a self-issued one), the subject carries an accepted one.
        let mut charge = |who: &[u8; 20]| {
            let k = keylet::account_root_key(who);
            if let Some(data) = sandbox.read(&k) {
                if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                    if count > 0 {
                        acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
                    }
                    sandbox.write(k, serde_json::to_vec(&acct).unwrap_or_default());
                }
            }
        };
        if !accepted || subject == issuer {
            charge(&issuer);
        }
        if accepted && subject != issuer {
            charge(&subject);
        }

        TxResult::Success
    }
}

#[cfg(test)]
mod delete_tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn state_with(id: &[u8; 20]) -> LedgerState {
        let header = LedgerHeader {
            sequence: 100, total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]), transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]), parent_close_time: 0,
            close_time: 10, close_time_resolution: 10, close_flags: 0,
        };
        let mut st = LedgerState::new_unverified(header);
        let a = serde_json::json!({
            "LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
            "Balance": "100000000", "Sequence": 1, "OwnerCount": 0,
        });
        st.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&a).unwrap()).unwrap();
        st
    }

    fn del_tx(account: [u8; 20], subject: Option<[u8; 20]>, issuer: Option<[u8; 20]>) -> TxFields {
        let mut f = serde_json::json!({ "CredentialType": "4142" });
        if let Some(s) = subject { f["Subject"] = serde_json::json!(hex::encode(s)); }
        if let Some(i) = issuer { f["Issuer"] = serde_json::json!(hex::encode(i)); }
        TxFields {
            account, tx_type: "CredentialDelete".to_string(), fee: 12, sequence: 7,
            ticket_seq: None, last_ledger_seq: None, fields: f,
        }
    }

    /// A credential joins BOTH owner directories but is owned only by the
    /// issuer — rippled CredentialCreate.cpp:147-174: "Added to both dirs,
    /// owned only by issuer. CredentialAccept will transfer ownership to
    /// subject." The issuer's dirInsert carries the single
    /// adjustOwnerCount(+1); the subject's adds no reserve. A SELF-issued
    /// credential skips the subject insert and is marked lsfAccepted.
    ///
    /// We wrote the credential and bumped OwnerCount but touched neither
    /// directory. #105909285 68872086F0B4: mainnet Modifies the subject's dir
    /// root AND a tail page of the issuer's multi-page dir — 4 nodes to our 2.
    #[test]
    fn a_credential_joins_both_owner_directories() {
        let issuer = [0x01u8; 20];
        let subject = [0x03u8; 20];
        let st = state_with(&issuer);
        let mut sb = Sandbox::new(&st);

        let tx = TxFields {
            account: issuer, tx_type: "CredentialCreate".to_string(), fee: 12, sequence: 4,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Subject": hex::encode(subject),
                "CredentialType": "4142",
            }),
        };
        assert_eq!(CredentialCreateTransactor.do_apply(&tx, &mut sb), TxResult::Success);

        let cred_key = credential_key(&subject, &issuer, &cred_type_bytes("4142"));
        assert!(sb.exists(&cred_key), "the credential is written");
        assert!(
            sb.exists(&keylet::owner_dir_key(&issuer)),
            "the ISSUER's owner directory receives it",
        );
        assert!(
            sb.exists(&keylet::owner_dir_key(&subject)),
            "and so does the SUBJECT's",
        );

        // Self-issued: no subject insert, marked accepted instead.
        let st = state_with(&issuer);
        let mut sb = Sandbox::new(&st);
        let mut tx = tx;
        tx.fields["Subject"] = serde_json::json!(hex::encode(issuer));
        assert_eq!(CredentialCreateTransactor.do_apply(&tx, &mut sb), TxResult::Success);
        let self_key = credential_key(&issuer, &issuer, &cred_type_bytes("4142"));
        let cred: serde_json::Value =
            serde_json::from_slice(&sb.read(&self_key).expect("self-issued credential")).unwrap();
        assert_eq!(
            cred["Flags"].as_u64(), Some(0x0001_0000),
            "a self-issued credential is lsfAccepted on creation",
        );
    }

    /// rippled needs at least ONE of Subject/Issuer, not both — whichever is
    /// absent defaults to the sending Account (CredentialDelete.cpp preflight
    /// and preclaim's `value_or(account)`). Requiring both rejected every real
    /// self-issued delete as malformed before preclaim could answer.
    ///
    /// #105909624 87483CD4BCF1, #105911834 E246CD9AE20F, #105912291
    /// DD6078118B99: Subject + CredentialType, NO Issuer. Mainnet tecNO_ENTRY,
    /// we said temMALFORMED.
    #[test]
    fn a_delete_naming_only_a_subject_reaches_preclaim() {
        let account = [0x01u8; 20];
        let subject = [0x03u8; 20];
        let st = state_with(&account);
        let sb = Sandbox::new(&st);

        // Subject only — the sender is the issuer. Must pass preflight and then
        // answer tecNO_ENTRY, not temMALFORMED.
        let tx = del_tx(account, Some(subject), None);
        assert_eq!(CredentialDeleteTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(CredentialDeleteTransactor.preclaim(&tx, &sb), TxResult::NoEntry);

        // Issuer only — mirror case, the sender is the subject.
        let tx = del_tx(account, None, Some(subject));
        assert_eq!(CredentialDeleteTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(CredentialDeleteTransactor.preclaim(&tx, &sb), TxResult::NoEntry);

        // NEITHER — genuinely malformed, still rejected.
        let tx = del_tx(account, None, None);
        assert_eq!(CredentialDeleteTransactor.preflight(&tx), TxResult::Malformed);
    }

    /// CredentialType must be present, non-empty and <= 64 bytes
    /// (kMaxCredentialTypeLength, Protocol.h:221).
    #[test]
    fn credential_type_size_is_bounded() {
        let account = [0x01u8; 20];
        let subject = [0x03u8; 20];

        let mut tx = del_tx(account, Some(subject), None);
        tx.fields["CredentialType"] = serde_json::json!("");
        assert_eq!(CredentialDeleteTransactor.preflight(&tx), TxResult::Malformed, "empty");

        let mut tx = del_tx(account, Some(subject), None);
        tx.fields["CredentialType"] = serde_json::json!("41".repeat(64));
        assert_eq!(CredentialDeleteTransactor.preflight(&tx), TxResult::Success, "64 bytes is the limit");

        let mut tx = del_tx(account, Some(subject), None);
        tx.fields["CredentialType"] = serde_json::json!("41".repeat(65));
        assert_eq!(CredentialDeleteTransactor.preflight(&tx), TxResult::Malformed, "65 bytes is over");
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

        let cred_key = credential_key(&subject, &issuer, &cred_type_bytes(cred_type_str));

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

        // Expired credentials are deleted even though the accept fails —
        // rippled checkExpired against parentCloseTime, then deleteSLE and
        // tecEXPIRED (CredentialAccept.cpp:110-117).
        if let Some(exp) = cred.get("Expiration").and_then(|v| v.as_u64()) {
            if exp != 0 && sandbox.base().header.close_time as u64 >= exp {
                let issuer_hint = cred.get("IssuerNode").and_then(|v| v.as_str())
                    .and_then(|h| u64::from_str_radix(h, 16).ok());
                let subject_hint = cred.get("SubjectNode").and_then(|v| v.as_str())
                    .and_then(|h| u64::from_str_radix(h, 16).ok());
                sandbox.delete(cred_key);
                crate::ledger::directory::owner_dir_remove(sandbox, &issuer, &cred_key, issuer_hint, false);
                if subject != issuer {
                    crate::ledger::directory::owner_dir_remove(sandbox, &subject, &cred_key, subject_hint, false);
                }
                // Unaccepted: the ISSUER still owns the reserve.
                crate::tx::offer::owner_count_add(sandbox, &issuer, -1);
                return TxResult::Expired;
            }
        }

        // Mark as accepted
        cred["Accepted"] = serde_json::Value::Bool(true);

        // Set the lsfAccepted flag (0x00010000)
        let flags = cred["Flags"].as_u64().unwrap_or(0);
        cred["Flags"] = serde_json::Value::Number((flags | 0x00010000).into());

        sandbox.write(cred_key, serde_json::to_vec(&cred).unwrap());

        // THE RESERVE MOVES: on acceptance the credential's owner changes
        // from issuer to subject — adjustOwnerCount(issuer, -1) then
        // (subject, +1), CredentialAccept.cpp:122-123. #106065037 11709731:
        // mainnet Modifies the issuer r9avT7NU's root (OwnerCount -1); we
        // left it untouched (2v3).
        if issuer != subject {
            crate::tx::offer::owner_count_add(sandbox, &issuer, -1);
            crate::tx::offer::owner_count_add(sandbox, &subject, 1);
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
        // Second create with same params is tecDUPLICATE, the code rippled's
        // preclaim returns when the credential keylet already exists.
        assert_eq!(CredentialCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Duplicate);
    }
}
