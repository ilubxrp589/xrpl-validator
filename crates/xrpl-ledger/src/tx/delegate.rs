//! DelegateSet — grant, change or withdraw another account's permission to
//! sign transactions on this one's behalf (rippled 3.4.0
//! `transactors/delegate/DelegateSet.cpp`, PermissionDelegationV1_1).
//!
//! Finding 360: the amendment reached majority on mainnet 2026-09-21 (active
//! ~10-05) and the engine had neither the transactor nor the delegated fee
//! path. Three parts, all here or wired from here:
//!   * DelegateSet: one `Delegate` object per (account, authorized) pair,
//!     keyed `keylet::delegate`, linked into BOTH owner directories
//!     (`OwnerNode` in the delegator's, `DestinationNode` in the delegate's,
//!     so AccountDelete finds inbound delegations), reserve and OwnerCount on
//!     the delegator only; an empty `Permissions` array deletes it.
//!   * the delegated transaction: fee from the delegate, sequence from the
//!     account (`apply_common`), `preFeeBalance_` = the account's untouched
//!     balance (`TxFields::account_fee`).
//!   * AccountDelete removes a delegation from either side through
//!     [`delete_delegate_object`] (rippled `DelegateSet::deleteDelegate`).
//! Permission checks (`checkPermission`, granular sandboxes) end in
//! terNO_DELEGATE_PERMISSION or tem codes — never in a ledger — so a
//! delegated transaction that landed has passed them.

use crate::ledger::directory::{owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use serde_json::{json, Value};
use xrpl_core::types::Hash256;

/// `kPermissionMaxSize` (Protocol.h).
const PERMISSION_MAX: usize = 10;

fn authorize(tx: &TxFields) -> Option<[u8; 20]> {
    crate::tx::offer::decode20(tx.fields.get("Authorize")?.as_str()?)
}

fn permissions(tx: &TxFields) -> Vec<Value> {
    tx.fields.get("Permissions").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// A `PermissionValue` as its u32: a number, or a transaction-type /
/// granular-permission name (tx type code + 1; granular 65537+).
fn permission_value(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => xrpl_core::codec::definitions::permission_value_code(s).ok().map(u64::from),
        _ => None,
    }
}

fn node_of(obj: &Value, field: &str) -> Option<u64> {
    let v = obj.get(field)?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
}

pub struct DelegateSetTransactor;

impl Transactor for DelegateSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        let perms = permissions(tx);
        if perms.len() > PERMISSION_MAX {
            return TxResult::TemArrayTooLarge; // never lands
        }
        let Some(auth) = authorize(tx) else { return TxResult::Malformed };
        if auth == tx.account {
            return TxResult::Malformed; // cannot authorize self
        }
        let mut seen = std::collections::HashSet::new();
        for p in &perms {
            // rippled's JSON names the value ("Payment", "TrustlineAuthorize");
            // a decoded object carries the number. Both mean the same u32.
            let Some(v) = permission_value(&p["Permission"]["PermissionValue"]) else { return TxResult::Malformed };
            if !seen.insert(v) {
                return TxResult::Malformed; // duplicate permission
            }
        }
        // `isDelegable` refusals are temMALFORMED too, and a transaction that
        // landed has passed them, so they are not re-judged here.
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let Some(auth) = authorize(tx) else { return TxResult::Malformed };
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        if !sandbox.exists(&keylet::account_root_key(&auth)) {
            return TxResult::NoTarget;
        }
        if crate::tx::misc::is_pseudo_account(sandbox, &auth) {
            return TxResult::TecPseudoAccount;
        }
        if permissions(tx).is_empty() && !sandbox.exists(&keylet::delegate_key(&tx.account, &auth)) {
            return TxResult::NoEntry;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(auth) = authorize(tx) else { return TxResult::Malformed };
        let key = keylet::delegate_key(&tx.account, &auth);
        let perms = permissions(tx);
        if let Some(mut obj) = crate::tx::offer::json_at(sandbox, &key) {
            if perms.is_empty() {
                return delete_delegate_object(sandbox, &key);
            }
            obj["Permissions"] = Value::Array(perms);
            sandbox.write(key, serde_json::to_vec(&obj).unwrap_or_default());
            return TxResult::Success;
        }
        if perms.is_empty() {
            return TxResult::Malformed; // tecINTERNAL: preclaim refused this
        }
        // checkReserve(sleOwner, preFeeBalance_, {ownerCountDelta = 1}).
        let Some(owner) = crate::tx::offer::json_at(sandbox, &keylet::account_root_key(&tx.account)) else {
            return TxResult::NoAccount;
        };
        let bal = owner["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let oc = owner["OwnerCount"].as_u64().unwrap_or(0);
        if bal.saturating_add(tx.account_fee()) < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
            return TxResult::InsufficientReserve;
        }
        let owner_node = owner_dir_insert(sandbox, &tx.account, &key);
        let dest_node = owner_dir_insert(sandbox, &auth, &key);
        let obj = json!({
            "LedgerEntryType": "Delegate",
            "Flags": 0,
            "Account": hex::encode(tx.account),
            "Authorize": hex::encode(auth),
            "Permissions": perms,
            "OwnerNode": format!("{owner_node:x}"),
            "DestinationNode": format!("{dest_node:x}"),
        });
        sandbox.write(key, serde_json::to_vec(&obj).unwrap_or_default());
        crate::tx::offer::owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

/// rippled `DelegateSet::deleteDelegate`: unlink the object from the
/// delegator's directory (`OwnerNode`) and, when present, the delegate's
/// (`DestinationNode`) — both with keepRoot FALSE — release one owner count
/// from the DELEGATOR only, and erase it. Shared with AccountDelete, which
/// meets a delegation from either side.
pub fn delete_delegate_object(sandbox: &mut Sandbox, key: &Hash256) -> TxResult {
    let Some(obj) = crate::tx::offer::json_at(sandbox, key) else { return TxResult::NoEntry };
    let (Some(delegator), Some(delegatee)) = (
        obj["Account"].as_str().and_then(crate::tx::offer::decode20),
        obj["Authorize"].as_str().and_then(crate::tx::offer::decode20),
    ) else {
        return TxResult::Malformed;
    };
    owner_dir_remove(sandbox, &delegator, key, node_of(&obj, "OwnerNode"), false);
    if obj.get("DestinationNode").is_some() {
        owner_dir_remove(sandbox, &delegatee, key, node_of(&obj, "DestinationNode"), false);
    }
    crate::tx::offer::owner_count_add(sandbox, &delegator, -1);
    sandbox.delete(*key);
    TxResult::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet;
    use crate::ledger::state::LedgerState;
    use crate::tx::dispatch::apply_on_sandbox;
    use serde_json::{json, Value};

    const A: [u8; 20] = [0xA1; 20]; // delegating account
    const B: [u8; 20] = [0xB2; 20]; // delegate (authorized)
    const C: [u8; 20] = [0xC3; 20]; // bystander
    const FEE: u64 = 12;
    const BASE: u64 = 1_000_000;
    const INC: u64 = 200_000;

    fn root(id: [u8; 20], balance: u64, oc: u64) -> Value {
        json!({"LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": balance.to_string(),
               "Sequence": 10, "OwnerCount": oc, "Flags": 0})
    }

    fn state(accounts: &[([u8; 20], u64, u64)]) -> LedgerState {
        let mut st = LedgerState::new_unverified(LedgerHeader {
            sequence: 10_000, // AccountDelete needs Sequence + 256 behind it
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        });
        let fees = json!({"LedgerEntryType": "FeeSettings", "Flags": 0, "BaseFeeDrops": "10",
                          "ReserveBaseDrops": BASE.to_string(), "ReserveIncrementDrops": INC.to_string()});
        st.state_map.insert(keylet::fee_settings_key(), serde_json::to_vec(&fees).unwrap()).unwrap();
        for (id, bal, oc) in accounts {
            st.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&root(*id, *bal, *oc)).unwrap()).unwrap();
        }
        st
    }

    fn txf(account: [u8; 20], tx_type: &str, fields: Value) -> TxFields {
        let mut f = json!({"TransactionType": tx_type, "Account": hex::encode(account), "Fee": FEE.to_string()});
        for (k, v) in fields.as_object().unwrap() {
            f[k] = v.clone();
        }
        TxFields {
            account,
            tx_type: tx_type.into(),
            fee: FEE,
            sequence: 10,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: f,
            inner_batch: false,
        }
    }

    fn read(sb: &Sandbox, k: &Hash256) -> Option<Value> {
        sb.read(k).map(|b| serde_json::from_slice(&b).unwrap())
    }
    fn bal(sb: &Sandbox, id: [u8; 20]) -> u64 {
        read(sb, &keylet::account_root_key(&id)).unwrap()["Balance"].as_str().unwrap().parse().unwrap()
    }
    fn field(sb: &Sandbox, id: [u8; 20], f: &str) -> u64 {
        read(sb, &keylet::account_root_key(&id)).unwrap()[f].as_u64().unwrap_or(0)
    }
    fn dir_has(sb: &Sandbox, owner: [u8; 20], k: &Hash256) -> bool {
        read(sb, &keylet::owner_dir_key(&owner)).is_some_and(|d| {
            d["Indexes"].as_array().unwrap().iter().any(|x| x.as_str().unwrap().eq_ignore_ascii_case(&hex::encode(k.0)))
        })
    }
    fn perms(vals: &[u32]) -> Value {
        Value::Array(vals.iter().map(|v| json!({"Permission": {"PermissionValue": v}})).collect())
    }
    fn set(sb: &mut Sandbox, vals: &[u32]) -> String {
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(B), "Permissions": perms(vals)})), sb);
        r.code_str().to_string()
    }

    #[test]
    fn the_delegate_key_is_the_e_space_hash_of_both_accounts() {
        // indexHash(LedgerNameSpace::Delegate = 'E', account, authorized).
        let mut buf = vec![0x00, b'E'];
        buf.extend_from_slice(&A);
        buf.extend_from_slice(&B);
        assert_eq!(keylet::delegate_key(&A, &B), crate::shamap::hash::sha512_half(&buf));
    }

    #[test]
    fn delegate_set_creates_the_object_in_both_directories_and_charges_the_delegator() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1, 65537]), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        let o = read(&sb, &k).expect("the Delegate object");
        assert_eq!(o["LedgerEntryType"], "Delegate");
        assert_eq!(o["Account"].as_str().unwrap().to_lowercase(), hex::encode(A));
        assert_eq!(o["Authorize"].as_str().unwrap().to_lowercase(), hex::encode(B));
        assert_eq!(o["Permissions"], perms(&[1, 65537]));
        assert!(o.get("OwnerNode").is_some() && o.get("DestinationNode").is_some(), "{o}");
        assert!(dir_has(&sb, A, &k) && dir_has(&sb, B, &k), "linked into both owner directories");
        assert_eq!((field(&sb, A, "OwnerCount"), field(&sb, B, "OwnerCount")), (1, 0), "only the delegator pays reserve");
    }

    #[test]
    fn permission_values_arrive_as_names_in_live_json() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let named = json!([{"Permission": {"PermissionValue": "Payment"}}, {"Permission": {"PermissionValue": "TrustlineAuthorize"}}]);
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(B), "Permissions": named})), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert!(read(&sb, &keylet::delegate_key(&A, &B)).is_some());
        // "Payment" is tx type 0 → 1, and the name and the number are the same permission.
        let dup = json!([{"Permission": {"PermissionValue": "Payment"}}, {"Permission": {"PermissionValue": 1}}]);
        let mut sb = Sandbox::new(&st);
        let (r, applied) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(B), "Permissions": dup})), &mut sb);
        assert!(!applied && !r.is_claimed(), "a duplicate by name and number is temMALFORMED: {r:?}");
    }

    #[test]
    fn delegate_set_replaces_the_permissions_of_an_existing_object() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS");
        assert_eq!(set(&mut sb, &[8, 12]), "tesSUCCESS");
        let o = read(&sb, &keylet::delegate_key(&A, &B)).unwrap();
        assert_eq!(o["Permissions"], perms(&[8, 12]));
        assert_eq!(field(&sb, A, "OwnerCount"), 1);
    }

    #[test]
    fn delegate_set_with_no_permissions_deletes_it_from_both_directories() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS");
        assert_eq!(set(&mut sb, &[]), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        assert!(read(&sb, &k).is_none());
        // dirRemove(…, keepRoot = false): the emptied roots go too.
        assert!(read(&sb, &keylet::owner_dir_key(&A)).is_none() && read(&sb, &keylet::owner_dir_key(&B)).is_none());
        assert_eq!(field(&sb, A, "OwnerCount"), 0);
    }

    #[test]
    fn delegate_set_refusals_are_claimed_tecs() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        // Nothing to delete.
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[]), "tecNO_ENTRY");
        assert_eq!(bal(&sb, A), 50_000_000 - FEE);
        // Authorized account missing.
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(C), "Permissions": perms(&[1])})), &mut sb);
        assert_eq!(r.code_str(), "tecNO_TARGET");
        // Authorized account is a pseudo-account (an AMM's).
        let mut st2 = state(&[(A, 50_000_000, 0)]);
        let mut amm = root(C, 1, 1);
        amm["AMMID"] = json!("AB".repeat(32));
        st2.state_map.insert(keylet::account_root_key(&C), serde_json::to_vec(&amm).unwrap()).unwrap();
        let mut sb = Sandbox::new(&st2);
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(C), "Permissions": perms(&[1])})), &mut sb);
        assert_eq!(r.code_str(), "tecPSEUDO_ACCOUNT");
        // Reserve for one more object, judged on the pre-fee balance.
        let st3 = state(&[(A, BASE + INC - 1, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st3);
        assert_eq!(set(&mut sb, &[1]), "tecINSUFFICIENT_RESERVE");
        let st4 = state(&[(A, BASE + INC, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st4);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS", "exactly the reserve pre-fee is enough");
    }

    #[test]
    fn delegate_set_malformations_never_land() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        for f in [
            json!({"Authorize": hex::encode(A), "Permissions": perms(&[1])}),         // self
            json!({"Authorize": hex::encode(B), "Permissions": perms(&[1, 1])}),      // duplicate
            json!({"Authorize": hex::encode(B), "Permissions": perms(&(1..=11).collect::<Vec<_>>())}), // > 10
        ] {
            let mut sb = Sandbox::new(&st);
            let (r, applied) = apply_on_sandbox(&txf(A, "DelegateSet", f.clone()), &mut sb);
            assert!(!applied && !r.is_claimed(), "{f} → {r:?}");
        }
    }

    // ---- a delegated transaction: the delegate pays, the account's sequence moves ----

    #[test]
    fn a_delegated_transaction_charges_the_fee_to_the_delegate() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        let mut t = txf(A, "Payment", json!({"Destination": hex::encode(C), "Amount": "1000000", "Delegate": hex::encode(B)}));
        t.fields["DeliverMax"] = json!("1000000");
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert_eq!(bal(&sb, A), 50_000_000 - 1_000_000, "the account pays the amount, not the fee");
        assert_eq!(bal(&sb, B), 50_000_000 - FEE, "the delegate pays the fee");
        assert_eq!(field(&sb, A, "Sequence"), 11, "the account's sequence is consumed");
        assert_eq!(field(&sb, B, "Sequence"), 10, "the delegate's is not");
        assert_eq!(bal(&sb, C), 51_000_000);
    }

    #[test]
    fn a_delegated_tec_still_charges_the_delegate() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        // DepositPreauth of an account that does not exist: tecNO_TARGET.
        let t = txf(A, "DepositPreauth", json!({"Authorize": hex::encode(C), "Delegate": hex::encode(B)}));
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tecNO_TARGET");
        assert_eq!((bal(&sb, A), bal(&sb, B)), (50_000_000, 50_000_000 - FEE));
        assert_eq!(field(&sb, A, "Sequence"), 11);
    }

    #[test]
    fn a_delegated_reserve_test_uses_the_accounts_untouched_balance() {
        // preFeeBalance_ is the ACCOUNT's balance before the fee; a delegate
        // paid it, so the account's balance IS that pre-fee balance. Adding
        // the fee back (the non-delegated reconstruction) would admit this.
        let st = state(&[(A, BASE + INC - 1, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        let t = txf(A, "DepositPreauth", json!({"Authorize": hex::encode(C), "Delegate": hex::encode(B)}));
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tecINSUFFICIENT_RESERVE");
        let st = state(&[(A, BASE + INC, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert_eq!(bal(&sb, A), BASE + INC, "and the account still pays no fee");
    }

    // ---- AccountDelete removes inbound and outbound delegations ----

    /// AccountDelete's fee is one owner reserve (temBAD_FEE below it).
    fn account_delete(who: [u8; 20]) -> TxFields {
        let mut t = txf(who, "AccountDelete", json!({"Destination": hex::encode(C)}));
        t.fee = INC;
        t.fields["Fee"] = json!(INC.to_string());
        t
    }

    fn with_delegation() -> LedgerState {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS");
        let mods = sb.into_modifications();
        let mut out = st.clone();
        crate::ledger::sandbox::apply_modifications(&mut out, mods).unwrap();
        out
    }

    #[test]
    fn deleting_the_delegator_removes_its_delegation_from_the_delegates_directory() {
        let st = with_delegation();
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&account_delete(A), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        assert!(read(&sb, &k).is_none());
        assert!(read(&sb, &keylet::owner_dir_key(&B)).is_none(), "B's directory held only the delegation");
    }

    #[test]
    fn deleting_the_delegate_removes_the_delegation_and_frees_the_delegators_reserve() {
        let st = with_delegation();
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&account_delete(B), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        assert!(read(&sb, &k).is_none());
        assert!(read(&sb, &keylet::owner_dir_key(&A)).is_none(), "A's directory held only the delegation");
        assert_eq!(field(&sb, A, "OwnerCount"), 0, "the delegator's reserve is released");
    }
}
