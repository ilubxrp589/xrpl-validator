//! Check transaction types: CheckCreate, CheckCash, CheckCancel.
//!
//! Checks are deferred payments — the sender creates a Check object that the
//! destination can cash later. The sender's funds are not locked; they must
//! have sufficient balance when the Check is cashed.
//!
//! CheckCreate: Create a Check object at check_key(account, sequence).
//! CheckCash:   Cash a Check — transfer funds from sender to destination, delete Check.
//! CheckCancel: Cancel a Check — delete the object with no fund transfer.
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
        let Some(dest) = tx.fields.get("Destination").and_then(|d| parse_account_id(d)) else {
            return TxResult::Malformed;
        };
        // Destination must be a valid account
        let Some(dst) = sandbox
            .read(&keylet::account_root_key(&dest))
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
        else {
            return TxResult::NoDst;
        };
        // The rest of rippled's CheckCreate::preclaim, in its order: a
        // destination may refuse checks outright, may be a pseudo-account, or
        // may insist on a tag.
        let dflags = dst["Flags"].as_u64().unwrap_or(0);
        if dflags & 0x0800_0000 != 0 {
            return TxResult::NoPermission; // lsfDisallowIncomingCheck
        }
        // Pseudo-accounts cannot cash checks — same designator fields as the
        // Payment rule (`3a718aa`): sfAMMID / sfVaultID / sfLoanBrokerID.
        if ["AMMID", "VaultID", "LoanBrokerID"].iter().any(|f| dst.get(f).is_some()) {
            return TxResult::NoPermission;
        }
        // #105846674 F6CC9A594B17: a VRTY airdrop check to rnabZzjg, whose
        // flags 0x120000 carry lsfRequireDestTag, with no DestinationTag on the
        // transaction. Mainnet claims the fee in one mutation; we created the
        // Check and both directory entries in seven.
        if dflags & 0x0002_0000 != 0 && tx.fields.get("DestinationTag").is_none() {
            return TxResult::DstTagNeeded; // lsfRequireDestTag
        }
        // NOT modelled: the non-native SendMax freeze block (global freeze, and
        // either party's line frozen by the issuer → tecFROZEN) and the
        // Expiration → tecEXPIRED check. No failing ledger for either yet.
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

        // Create the Check ledger object — canonical shape: Flags always,
        // both dir hints, optional passthroughs. Only the WRITER pays the
        // reserve (byte census: the old both-sides bump left r6zw at
        // OwnerCount 45 where mainnet has 44 — the dest-root touch in the
        // meta is a threadOwners refresh, not a count change).
        let mut check_obj = serde_json::json!({
            "LedgerEntryType": "Check",
            "Flags": 0,
            "Account": hex::encode(tx.account),
            "Destination": hex::encode(dest),
            "SendMax": send_max,
            "Sequence": seq,
        });
        for f in ["Expiration", "DestinationTag", "SourceTag", "InvoiceID"] {
            if let Some(v) = tx.fields.get(f) {
                check_obj[f] = v.clone();
            }
        }
        let owner_node =
            crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &check_key);
        let dest_node = crate::ledger::directory::owner_dir_insert(sandbox, &dest, &check_key);
        check_obj["OwnerNode"] = serde_json::Value::String(format!("{owner_node:x}"));
        check_obj["DestinationNode"] = serde_json::Value::String(format!("{dest_node:x}"));
        sandbox.write(check_key, serde_json::to_vec(&check_obj).unwrap());
        crate::tx::offer::owner_count_add(sandbox, &tx.account, 1);

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
        // EXACTLY ONE of Amount / DeliverMin (CheckCash.cpp:57
        // `bool(optAmount) == bool(optDeliverMin) => temMALFORMED`): Amount is
        // a fixed cash-out, DeliverMin a floor for a partial cash-out.
        // Requiring Amount alone rejected every DeliverMin check
        // (#105798519 8FBBA125 cashes with DeliverMin only — mainnet
        // tesSUCCESS, 8 mutations, we said temMALFORMED).
        if tx.fields.get("Amount").is_some() == tx.fields.get("DeliverMin").is_some() {
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

        // Cash via the shared transfer engine (XRP or IOU). The check's
        // SendMax fixes the currency; the tx Amount is the requested delivery.
        // Writer shortfall fails the rippled way: tecPATH_PARTIAL, fee-only.
        use crate::tx::offer as ox;
        let sm_json = check.get("SendMax").cloned().unwrap_or_default();
        let amt_json = tx.fields.get("Amount")
            .or(tx.fields.get("DeliverMin"))
            .cloned()
            .unwrap_or_else(|| sm_json.clone());
        let (Some(leg), Some(want)) = (ox::leg_of(&sm_json), crate::ledger::keylet::amount_mant_exp(&amt_json)) else {
            return TxResult::Malformed;
        };
        let cap = crate::ledger::keylet::amount_mant_exp(&sm_json).unwrap_or(want);
        if ox::me_cmp(want, cap).is_gt() {
            return TxResult::PathPartial; // asking beyond the check's SendMax
        }
        // Cashing an IOU into a line the CASHER doesn't have first requires
        // the reserve for creating it: `checkReserve` fires BEFORE the flow —
        // "Trust line does not exist. Insufficient reserve to create line."
        // ⇒ tecNO_LINE_INSUF_RESERVE (CheckCash.cpp:391-401, preFeeBalance
        // vs accountReserve(ownerCount + 1)). #106233366 E7419B07: we fell
        // through to the writer-funds check and said tecPATH_PARTIAL.
        if !leg.xrp && tx.account != leg.issuer {
            let line_key = crate::ledger::keylet::ripple_state_key(&tx.account, &leg.issuer, &leg.cur);
            if !sandbox.exists(&line_key) {
                let acct_key = crate::ledger::keylet::account_root_key(&tx.account);
                if let Some(a) = sandbox
                    .read(&acct_key)
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                {
                    let oc = a["OwnerCount"].as_u64().unwrap_or(0);
                    let bal = a["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    // do_apply runs post-fee; rippled compares preFeeBalance_.
                    let pre_fee = bal.saturating_add(tx.fee);
                    if pre_fee < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
                        return TxResult::NoLineInsufReserve;
                    }
                }
            }
        }
        let mut avail = if creator == leg.issuer { want } else { ox::available(sandbox, &creator, &leg) };
        // CASHING THE CHECK RELEASES THE CHECK'S OWN RESERVE, and that reserve
        // counts toward the payment. rippled says so twice:
        //     if (value.native())
        //         availableFunds += XRPAmount{ctx.view.fees().increment};
        //   "src will have one reserve's worth of additional XRP once the check
        //    is cashed, since the check's reserve will no longer be required"
        //   (CheckCash.cpp:175-181), and doApply spends it through
        //   `xrpLiquid(psb, srcId, -1, viewJ)` — "Hence the -1" (:333-339).
        // XRP ONLY: the test is `value.native()`, since an IOU check's reserve
        // is not what funds the transfer.
        //
        // #106375426 0932105B: the writer holds 60741227 drops at OwnerCount 49,
        // so a full 10800000 reserve leaves 49941227 against a 50000000 cash —
        // short by 58773. Release the check's own 200000 and it clears with
        // 141227 to spare. Mainnet succeeds in 7 nodes; we claimed the fee with
        // tecPATH_PARTIAL in 3.
        if leg.xrp && creator != leg.issuer {
            avail = (avail.0.saturating_add(ox::XRP_RESERVE_INC), avail.1);
        }
        if ox::me_cmp(avail, want).is_lt() {
            return TxResult::PathPartial; // writer cannot cover — fee-only
        }
        ox::move_leg(sandbox, &creator, &tx.account, &leg, want);

        // Delete the Check and unlink it from BOTH owner directories
        // (writer via OwnerNode, casher/destination via DestinationNode).
        let owner_hint = check.get("OwnerNode").map(|v| ox::dirnum(v));
        let dest_hint = check.get("DestinationNode").map(|v| ox::dirnum(v));
        sandbox.delete(check_key);
        crate::ledger::directory::owner_dir_remove(sandbox, &creator, &check_key, owner_hint, true);
        crate::ledger::directory::owner_dir_remove(sandbox, &tx.account, &check_key, dest_hint, true);
        // Only the check WRITER ever paid the reserve.
        ox::owner_count_add(sandbox, &creator, -1);

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

        // A Check is linked into TWO owner directories — the writer's (via
        // OwnerNode) and the destination's (via DestinationNode) — so
        // cancelling must unlink both, each through its stored page hint.
        // Only the destination link is conditional: rippled skips it when the
        // check is written to self, which it "shouldn't be"
        // (CheckCancel.cpp:71-93). The reserve is the writer's alone, so only
        // the writer's OwnerCount moves (adjustOwnerCount(sleSrc, -1), line 97)
        // — the destination's AccountRoot appears in the metadata only when it
        // is also the account paying the fee.
        use crate::tx::offer as ox;
        let owner_hint = check.get("OwnerNode").map(|v| ox::dirnum(v));
        let dest_hint = check.get("DestinationNode").map(|v| ox::dirnum(v));
        if let (Some(creator_id), Some(dest_id)) = (creator, dest) {
            if creator_id != dest_id {
                crate::ledger::directory::owner_dir_remove(
                    sandbox, &dest_id, &check_key, dest_hint, true,
                );
            }
        }
        if let Some(creator_id) = creator {
            crate::ledger::directory::owner_dir_remove(
                sandbox, &creator_id, &check_key, owner_hint, true,
            );
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

    /// rippled's CheckCreate::preclaim refuses a check whose destination has
    /// set `lsfRequireDestTag` (0x00020000) when the transaction carries no
    /// DestinationTag. #105846674 F6CC9A594B17: a VRTY airdrop check to
    /// rnabZzjg (flags 0x120000, no tag on the tx) — mainnet claims the fee in
    /// one mutation, we created the Check and both dir entries in seven.
    #[test]
    fn a_check_needs_a_tag_when_the_destination_requires_one() {
        let acct = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state_with_accounts(&[(&acct, 500_000_000), (&dest, 500_000_000)]);

        let mut fields = serde_json::json!({
            "Destination": hex::encode(dest),
            "SendMax": "1000000",
        });
        let mut tx = TxFields {
            account: acct,
            tx_type: "CheckCreate".into(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: fields.clone(),
        };
        assert_eq!(
            CheckCreateTransactor.preclaim(&tx, &Sandbox::new(&state)),
            TxResult::Success,
            "an untagged check to an ordinary destination stands",
        );

        // The destination now requires a tag — nothing else changes.
        let dkey = keylet::account_root_key(&dest);
        let mut d: serde_json::Value =
            serde_json::from_slice(&Sandbox::new(&state).read(&dkey).unwrap()).unwrap();
        d["Flags"] = serde_json::json!(0x0002_0000u64);
        state.state_map.insert(dkey, serde_json::to_vec(&d).unwrap()).unwrap();

        assert_eq!(
            CheckCreateTransactor.preclaim(&tx, &Sandbox::new(&state)),
            TxResult::DstTagNeeded,
            "but not without a DestinationTag",
        );

        // Supplying the tag makes it good again.
        fields["DestinationTag"] = serde_json::json!(12345u64);
        tx.fields = fields;
        assert_eq!(
            CheckCreateTransactor.preclaim(&tx, &Sandbox::new(&state)),
            TxResult::Success,
            "a tagged check satisfies the requirement",
        );
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

    /// Mainnet #105797946 (AD75E19EE0AC, 4B8AF3399363, 0AD4F6597D90): a Check
    /// is linked into BOTH the writer's and the destination's owner directory,
    /// so CheckCancel must unlink both — rippled CheckCancel.cpp:71-93 issues
    /// one dirRemove per directory. We deleted the object and adjusted the
    /// reserve but left two dangling directory entries, emitting 2 mutations
    /// where mainnet emits 4.
    #[test]
    fn check_cancel_unlinks_both_owner_directories() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);
        let mut sandbox = Sandbox::new(&state);

        let create_tx = TxFields {
            account: sender, tx_type: "CheckCreate".to_string(), fee: 12, sequence: 1,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "SendMax": "5000000",
            }),
        };
        assert_eq!(CheckCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        let check_key = keylet::check_key(&sender, 1);
        let entry = hex::encode_upper(check_key.0);
        let listed = |sandbox: &Sandbox, owner: &[u8; 20]| -> bool {
            let root = keylet::owner_dir_key(owner);
            sandbox
                .read(&root)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                .and_then(|p| p.get("Indexes").and_then(|v| v.as_array()).cloned())
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(entry.as_str())))
        };
        // CheckCreate links the check into both directories.
        assert!(listed(&sandbox, &sender), "writer's dir should list the check");
        assert!(listed(&sandbox, &dest), "destination's dir should list the check");

        let cancel_tx = TxFields {
            account: sender, tx_type: "CheckCancel".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({ "CheckID": hex::encode(check_key.0) }),
        };
        assert_eq!(CheckCancelTransactor.do_apply(&cancel_tx, &mut sandbox), TxResult::Success);

        assert!(!sandbox.exists(&check_key));
        assert!(!listed(&sandbox, &sender), "writer's dir still lists a cancelled check");
        assert!(!listed(&sandbox, &dest), "destination's dir still lists a cancelled check");
        // The reserve is the writer's alone (adjustOwnerCount(sleSrc, -1)).
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 0);
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

        assert_eq!(CheckCashTransactor.do_apply(&cash_tx, &mut sandbox), TxResult::PathPartial);

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
