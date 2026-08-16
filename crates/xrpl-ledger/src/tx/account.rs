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
        if tx.fields.get("Destination").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        // "The fee required for AccountDelete is one owner reserve"
        // (AccountDelete::calculateBaseFee -> calculateOwnerReserveFee). That
        // is read from the ledger's fee settings, not fixed: the 2024 vote cut
        // the increment from 2 XRP to 0.2, so a hardcoded 2_000_000 rejects
        // every present-day AccountDelete (#105764469 2A99D114 pays exactly
        // the 200000-drop increment — mainnet tesSUCCESS, we said temBAD_FEE).
        // Needs the view, so it belongs here rather than in preflight.
        if tx.fee < crate::ledger::fees::reserve_inc(sandbox) {
            return TxResult::BadFee;
        }
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

        // ...and OwnerCount ZERO IS NOT THE SAME AS OWNING NOTHING. rippled
        // walks the owner DIRECTORY and refuses on any entry whose type has no
        // `nonObligationDeleter` (AccountDelete.cpp):
        //     if (dirIsEmpty(ctx.view, ownerDirKeylet)) return tesSUCCESS;
        //     do {
        //         auto sleItem = ctx.view.read(keylet::child(dirEntry));
        //         if (nonObligationDeleter(nodeType) == nullptr)
        //             return tecHAS_OBLIGATIONS;
        //     } while (cdirNext(...));
        // Deletable: Offer, SignerList, Ticket, DepositPreauth, NFTokenOffer,
        // DID, Oracle, Credential, Delegate. Everything else is an obligation.
        //
        // #106322004 77C5E61D: the account holds OwnerCount 0 and one ESCROW.
        // An escrow that names this account as DESTINATION is linked into its
        // directory but does NOT raise its OwnerCount — the reserve sits with
        // the escrow's creator. So the count says "owns nothing" while the
        // directory says otherwise, and mainnet refuses. We deleted the account
        // and paid its balance away: 2 mutations against mainnet's 1, and an
        // account destroyed that mainnet keeps.
        const DELETABLE: [&str; 9] = [
            "Offer", "SignerList", "Ticket", "DepositPreauth", "NFTokenOffer",
            "DID", "Oracle", "Credential", "Delegate",
        ];
        let dir_root = keylet::owner_dir_key(&tx.account);
        let mut page_key = dir_root;
        for _ in 0..1000 {
            let Some(page) = sandbox
                .read(&page_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else { break };
            for idx in page.get("Indexes").and_then(|v| v.as_array()).into_iter().flatten() {
                let Some(k) = idx.as_str().and_then(|s| {
                    hex::decode(s).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                }) else { continue };
                let kind = sandbox
                    .read(&xrpl_core::types::Hash256(k))
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                    .and_then(|o| o.get("LedgerEntryType").and_then(|t| t.as_str()).map(str::to_string));
                match kind {
                    // Unreadable means unhydrated, not absent — refusing on it
                    // would invent obligations. Anything we CAN read decides.
                    None => continue,
                    Some(t) if DELETABLE.contains(&t.as_str()) => continue,
                    Some(_) => return TxResult::HasObligations,
                }
            }
            let next = page
                .get("IndexNext")
                .and_then(|v| {
                    v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                })
                .unwrap_or(0);
            if next == 0 {
                break;
            }
            page_key = keylet::dir_page_key(&dir_root, next);
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
        // `AccountDelete::preclaim` (:229-235): tecNO_DST, then
        // lsfRequireDestTag with no DestinationTag -> tecDST_TAG_NEEDED.
        // Same sweep as pay_channel above; no failing ledger pins this one
        // either, but rippled's condition is unambiguous.
        if dest["Flags"].as_u64().unwrap_or(0) & 0x0002_0000 != 0
            && tx.fields.get("DestinationTag").is_none()
        {
            return TxResult::DstTagNeeded; // lsfRequireDestTag
        }

        let dest_balance = dest["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        dest["Balance"] =
            serde_json::Value::String(dest_balance.checked_add(balance).unwrap_or(u64::MAX).to_string());
        sandbox.write(dest_key, serde_json::to_vec(&dest).unwrap());

        // The owner DIRECTORY goes too. rippled walks it deleting each owned
        // object and then removes the directory ITSELF, before erasing the
        // account (AccountDelete.cpp):
        //     Keylet const ownerDirKeylet{keylet::ownerDir(accountID_)};
        //     …
        //     if (view().exists(ownerDirKeylet) && !view().emptyDirDelete(ownerDirKeylet))
        //     view().update(dst);
        //     view().erase(src);
        //
        // An account at OwnerCount 0 still OWNS an empty root page — the count
        // tracks reserved objects, not the directory that held them — and that
        // page is a ledger object mainnet removes. #106295546 4568277964F6 is
        // exactly that and nothing else: 3 nodes to our 2, the missing one
        // being the directory F2EAB202… whose RootIndex is itself.
        //
        // Guarded on emptiness because that is what `emptyDirDelete` means;
        // preclaim already requires OwnerCount == 0, so a non-empty directory
        // here would be a state we do not understand and must not silently
        // discard.
        let dir_key = keylet::owner_dir_key(&tx.account);
        if let Some(d) = sandbox.read(&dir_key) {
            let empty = serde_json::from_slice::<serde_json::Value>(&d)
                .ok()
                .map(|v| {
                    v.get("Indexes")
                        .and_then(|i| i.as_array())
                        .is_none_or(|a| a.is_empty())
                })
                .unwrap_or(false);
            if empty {
                sandbox.delete(dir_key);
            }
        }

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
