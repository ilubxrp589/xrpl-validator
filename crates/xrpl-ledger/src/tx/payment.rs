//! Payment transaction — XRP direct payment.
//!
//! The most fundamental transaction type. Moves XRP from one account to another.
//! Can also create new accounts if the amount meets the reserve requirement.
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

/// Payment transactor.
pub struct PaymentTransactor;

impl PaymentTransactor {
    /// Extract the destination account ID from the transaction fields.
    fn destination(tx: &TxFields) -> Option<[u8; 20]> {
        let dest_hex = tx.fields.get("Destination")?.as_str()?;
        let bytes = hex::decode(dest_hex).ok()?;
        if bytes.len() != 20 {
            return None;
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Some(arr)
    }

    /// Extract the Amount in drops from the transaction fields.
    /// Amount can be a string (XRP drops) or an object (IOU — not handled here).
    fn amount_drops(tx: &TxFields) -> Option<u64> {
        match &tx.fields.get("Amount")? {
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None, // IOU object — not supported yet
        }
    }

    /// Read the reserve base from state, or use default.
    fn reserve_base(sandbox: &Sandbox) -> u64 {
        let fee_key = keylet::fee_settings_key();
        if let Some(data) = sandbox.read(&fee_key) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                if let Some(r) = v.get("ReserveBase").and_then(|r| r.as_u64()) {
                    return r;
                }
                // Newer format uses ReserveBaseDrops
                if let Some(r) = v
                    .get("ReserveBaseDrops")
                    .and_then(|r| r.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    return r;
                }
            }
        }
        // Default: 10 XRP (changed from 1 XRP in 2024)
        10_000_000
    }
}

impl Transactor for PaymentTransactor {
    /// val-060: Format validation — no state access.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        // Must be a Payment
        if tx.tx_type != "Payment" {
            return TxResult::Malformed;
        }

        // Fee must be positive
        if tx.fee == 0 {
            return TxResult::BadFee;
        }

        // Destination must be present and valid
        if Self::destination(tx).is_none() {
            return TxResult::Malformed;
        }

        // Amount: XRP drops or an IOU object.
        match Self::amount_drops(tx) {
            Some(amount) => {
                if amount == 0 || amount > 100_000_000_000_000_000 {
                    return TxResult::BadAmount;
                }
            }
            None => {
                // IOU delivery — validated by the engine in do_apply.
                let ok = tx.fields.get("Amount")
                    .and_then(crate::ledger::keylet::amount_mant_exp)
                    .is_some_and(|(m, _)| m > 0);
                if !ok {
                    return TxResult::BadAmount;
                }
            }
        }

        // Can't send to yourself (rippled allows it but it's a no-op)
        // Safe: we already checked destination is Some above
        let dest = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        if dest == tx.account {
            // rippled actually allows this — it's just a fee burn
            // We'll allow it too
        }

        TxResult::Success
    }

    /// val-061: State validation — read-only checks.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);

        // Sender must exist
        let acct_data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };

        let acct: serde_json::Value = match serde_json::from_slice(&acct_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Check sequence (skip for ticket-based txs)
        if !tx.uses_ticket() {
            let acct_seq = acct["Sequence"].as_u64().unwrap_or(0) as u32;
            if tx.sequence < acct_seq {
                return TxResult::PastSeq;
            }
            if tx.sequence > acct_seq {
                return TxResult::BadSequence;
            }
        }

        // Check LastLedgerSequence
        if let Some(max_ledger) = tx.last_ledger_seq {
            let current_seq = sandbox.base().header.sequence;
            if current_seq > max_ledger {
                return TxResult::MaxLedger;
            }
        }

        // Funding check applies only to a DIRECT full-delivery XRP payment.
        // Cross-currency (SendMax) buys the XRP via paths, and tfPartial
        // delivers what liquidity affords — both resolved in do_apply.
        let partial = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0002_0000 != 0;
        let pure_xrp = tx.fields.get("SendMax").is_none()
            && tx.fields.get("Paths").is_none()
            && tx.fields.get("Amount").map(|a| a.is_string()).unwrap_or(false);
        if pure_xrp && !partial {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let amount = Self::amount_drops(tx).unwrap_or(0);
            if balance < amount.saturating_add(tx.fee) {
                return TxResult::UnfundedPayment;
            }
        }

        // If destination doesn't exist, amount must meet reserve
        let dest = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let dest_key = keylet::account_root_key(&dest);
        if !sandbox.exists(&dest_key) {
            let reserve = Self::reserve_base(sandbox);
            let amount = Self::amount_drops(tx).unwrap_or(0);
            if amount < reserve {
                return TxResult::NoDstInsufXrp;
            }
        }

        TxResult::Success
    }

    /// val-062: Apply payment state changes — direct XRP, direct IOU, or
    /// cross-currency via the order books (the crossing engine).
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let dest_id = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let amt_json = tx.fields.get("Amount").cloned().unwrap_or_default();
        let sendmax = tx.fields.get("SendMax").cloned();
        let partial = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0002_0000 != 0;
        let cross_currency = match (&sendmax, amt_json.is_string()) {
            (Some(sm), true) => !sm.is_string(),
            (Some(sm), false) => {
                sm.is_string()
                    || sm.get("currency").and_then(|v| v.as_str())
                        != amt_json.get("currency").and_then(|v| v.as_str())
                    || sm.get("issuer").and_then(|v| v.as_str())
                        != amt_json.get("issuer").and_then(|v| v.as_str())
            }
            (None, _) => false,
        } || tx.fields.get("Paths").is_some();

        if cross_currency {
            return self.apply_path_payment(tx, sandbox, &amt_json, sendmax.as_ref(), &dest_id, partial);
        }
        if !amt_json.is_string() {
            return self.apply_iou_direct(tx, sandbox, &amt_json, &dest_id, partial);
        }

        let amount = match Self::amount_drops(tx) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // --- Sender side ---
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Deduct amount (fee is deducted by apply_common)
        let sender_balance = sender["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if sender_balance < amount {
            return TxResult::UnfundedPayment;
        }

        sender["Balance"] = serde_json::Value::String((sender_balance - amount).to_string());
        sandbox.write(sender_key, serde_json::to_vec(&sender).expect("serializing valid JSON Value"));

        // --- Destination side ---
        let dest_key = keylet::account_root_key(&dest_id);

        if let Some(dest_data) = sandbox.read(&dest_key) {
            // Destination exists — add amount
            let mut dest: serde_json::Value = match serde_json::from_slice(&dest_data) {
                Ok(v) => v,
                Err(_) => return TxResult::Malformed,
            };

            let dest_balance = dest["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            let new_dest_balance = match dest_balance.checked_add(amount) {
                Some(b) => b,
                None => return TxResult::Malformed,
            };
            dest["Balance"] = serde_json::Value::String(new_dest_balance.to_string());
            sandbox.write(dest_key, serde_json::to_vec(&dest).expect("serializing valid JSON Value"));
        } else {
            // Destination doesn't exist — create new AccountRoot
            let reserve = Self::reserve_base(sandbox);
            if amount < reserve {
                return TxResult::NoDstInsufXrp;
            }

            let new_account = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(dest_id),
                "Balance": amount.to_string(),
                "Sequence": 1,
                "OwnerCount": 0,
                "Flags": 0,
            });
            sandbox.write(dest_key, serde_json::to_vec(&new_account).expect("serializing valid JSON Value"));
        }

        TxResult::Success
    }
}

impl PaymentTransactor {
    /// Direct same-currency IOU transfer over trust lines. Key-set faithful:
    /// sender/receiver lines adjust (receiver line created when absent),
    /// issuer-side legs settle implicitly. Insufficient holdings fail the
    /// rippled way: nothing delivered -> tecPATH_DRY, partial short-fall
    /// without tfPartialPayment -> tecPATH_PARTIAL (both fee-only).
    fn apply_iou_direct(
        &self,
        tx: &TxFields,
        sandbox: &mut Sandbox,
        amt_json: &serde_json::Value,
        dest: &[u8; 20],
        partial: bool,
    ) -> TxResult {
        use crate::tx::offer as ox;
        let (Some(leg), Some(want)) = (ox::leg_of(amt_json), crate::ledger::keylet::amount_mant_exp(amt_json)) else {
            return TxResult::Malformed;
        };
        let avail = if tx.account == leg.issuer {
            want // issuers mint their own IOU
        } else {
            ox::available(sandbox, &tx.account, &leg)
        };
        if ox::me_is_zero(avail) {
            return TxResult::PathDry;
        }
        let deliver = if ox::me_cmp(avail, want).is_lt() {
            if !partial {
                return TxResult::PathPartial;
            }
            avail
        } else {
            want
        };
        ox::move_leg(sandbox, &tx.account, dest, &leg, deliver);
        TxResult::Success
    }

    /// Cross-currency delivery: spend the SendMax side across the order book
    /// to acquire the Amount side (the offer-crossing engine with payment
    /// semantics), then hand the acquisition to the destination. Arb-style
    /// self-payments (Account == Destination) skip the final hop.
    fn apply_path_payment(
        &self,
        tx: &TxFields,
        sandbox: &mut Sandbox,
        amt_json: &serde_json::Value,
        sendmax: Option<&serde_json::Value>,
        dest: &[u8; 20],
        partial: bool,
    ) -> TxResult {
        use crate::tx::offer as ox;
        let Some(sm_json) = sendmax else {
            // Paths without SendMax: spend the Amount currency itself.
            return self.apply_iou_direct(tx, sandbox, amt_json, dest, partial);
        };
        let (Some(want_leg), Some(want0)) = (ox::leg_of(amt_json), crate::ledger::keylet::amount_mant_exp(amt_json)) else {
            return TxResult::Malformed;
        };
        let (Some(spend_leg), Some(spend0)) = (ox::leg_of(sm_json), crate::ledger::keylet::amount_mant_exp(sm_json)) else {
            return TxResult::Malformed;
        };
        let snap = sandbox.snapshot();
        // Limit price = the full SendMax per full Amount; walk while makers
        // are at or better. Identical shape to an OfferCreate paying `Amount`,
        // getting `SendMax`.
        let threshold = crate::ledger::keylet::offer_quality(sm_json, amt_json).unwrap_or(u64::MAX);
        let (rem_want, _rem_spend, _crossed) = ox::cross_engine(
            &tx.account, want0, spend0, &want_leg, &spend_leg, threshold, sandbox,
        );
        let delivered = ox::me_sub(want0, rem_want);
        if ox::me_is_zero(delivered) {
            sandbox.restore_snapshot(snap);
            return TxResult::PathDry;
        }
        if !partial && !ox::me_is_zero(rem_want) {
            sandbox.restore_snapshot(snap);
            return TxResult::PathPartial;
        }
        if dest != &tx.account {
            ox::move_leg(sandbox, &tx.account, dest, &want_leg, delivered);
        }
        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::{apply_modifications, Sandbox};
    use crate::ledger::state::LedgerState;
    use crate::ledger::transactor::apply_common;
    use xrpl_core::types::Hash256;

    fn make_state() -> LedgerState {
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
        LedgerState::new_unverified(header)
    }

    fn add_account(state: &mut LedgerState, id: &[u8; 20], balance: u64, seq: u32) {
        let acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": seq,
            "OwnerCount": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
    }

    fn read_balance(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        v["Balance"].as_str().unwrap().parse().unwrap()
    }

    fn read_balance_from_state(state: &LedgerState, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = state.state_map.lookup(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(data).unwrap();
        v["Balance"].as_str().unwrap().parse().unwrap()
    }

    fn payment_tx(sender: [u8; 20], dest: [u8; 20], amount: u64, fee: u64, seq: u32) -> TxFields {
        TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee,
            sequence: seq,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": amount.to_string(),
            }),
        }
    }

    #[test]
    fn preflight_valid_payment() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::Success);
    }

    #[test]
    fn preflight_zero_amount() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 0, 12, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::BadAmount);
    }

    #[test]
    fn preflight_zero_fee() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 1_000_000, 0, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::BadFee);
    }

    #[test]
    fn preflight_no_destination() {
        let sender = [0x01u8; 20];
        let tx = TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({"Amount": "1000000"}),
        };
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::Malformed);
    }

    #[test]
    fn preclaim_sender_not_found() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state(); // no accounts
        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1);
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::NoAccount);
    }

    #[test]
    fn preclaim_insufficient_balance() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 500_000, 1); // only 0.5 XRP
        add_account(&mut state, &dest, 50_000_000, 1);

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1); // needs 1M + 12
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::UnfundedPayment);
    }

    #[test]
    fn preclaim_new_dest_below_reserve() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        // dest doesn't exist, sending 1 XRP but reserve is 10 XRP

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1);
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::NoDstInsufXrp);
    }

    #[test]
    fn preclaim_past_sequence() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 10); // seq=10
        add_account(&mut state, &dest, 50_000_000, 1);

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 5); // tx seq=5 < account seq=10
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::PastSeq);
    }

    #[test]
    fn do_apply_xrp_to_existing_account() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        add_account(&mut state, &dest, 10_000_000, 1);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            let tx = payment_tx(sender, dest, 5_000_000, 12, 1);

            // Run common (deducts fee, increments seq)
            let common = apply_common(&tx, &mut sandbox);
            assert_eq!(common, TxResult::Success);

            // Run payment apply
            let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
            assert_eq!(result, TxResult::Success);

            // Verify in sandbox
            // Sender: 50M - 12(fee) - 5M(amount) = 44,999,988
            assert_eq!(read_balance(&sandbox, &sender), 44_999_988);
            // Dest: 10M + 5M = 15M
            assert_eq!(read_balance(&sandbox, &dest), 15_000_000);

            sandbox.into_modifications()
        };

        // Commit and verify state changed
        apply_modifications(&mut state, mods).unwrap();
        assert_eq!(read_balance_from_state(&state, &sender), 44_999_988);
        assert_eq!(read_balance_from_state(&state, &dest), 15_000_000);
    }

    #[test]
    fn do_apply_creates_new_account() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 100_000_000, 1);
        // dest doesn't exist

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            let tx = payment_tx(sender, dest, 20_000_000, 12, 1); // 20 XRP > 10 XRP reserve

            let common = apply_common(&tx, &mut sandbox);
            assert_eq!(common, TxResult::Success);

            let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
            assert_eq!(result, TxResult::Success);

            // New account should exist with balance = amount
            assert_eq!(read_balance(&sandbox, &dest), 20_000_000);
            // Sender: 100M - 12 - 20M = 79,999,988
            assert_eq!(read_balance(&sandbox, &sender), 79_999_988);

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();

        // Verify new account was created in state
        let dest_key = keylet::account_root_key(&dest);
        let dest_data = state.state_map.lookup(&dest_key).expect("dest account should exist");
        let dest_obj: serde_json::Value = serde_json::from_slice(dest_data).unwrap();
        assert_eq!(dest_obj["LedgerEntryType"], "AccountRoot");
        assert_eq!(dest_obj["Sequence"], 1);
        assert_eq!(dest_obj["Balance"].as_str().unwrap(), "20000000");
    }

    #[test]
    fn do_apply_new_account_below_reserve_fails() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 100_000_000, 1);

        let mut sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 5_000_000, 12, 1); // 5 XRP < 10 XRP reserve

        let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
        assert_eq!(result, TxResult::NoDstInsufXrp);
    }

    #[test]
    fn full_pipeline_preflight_preclaim_apply() {
        let sender = [0xAAu8; 20];
        let dest = [0xBBu8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 200_000_000, 5);
        add_account(&mut state, &dest, 50_000_000, 1);

        let tx = payment_tx(sender, dest, 25_000_000, 15, 5);
        let transactor = PaymentTransactor;

        // Full pipeline
        assert_eq!(transactor.preflight(&tx), TxResult::Success);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            assert_eq!(transactor.preclaim(&tx, &sandbox), TxResult::Success);
            assert_eq!(apply_common(&tx, &mut sandbox), TxResult::Success);
            assert_eq!(transactor.do_apply(&tx, &mut sandbox), TxResult::Success);

            // Sender: 200M - 15(fee) - 25M = 174,999,985
            assert_eq!(read_balance(&sandbox, &sender), 174_999_985);
            // Dest: 50M + 25M = 75M
            assert_eq!(read_balance(&sandbox, &dest), 75_000_000);
            // Sender sequence: 5 → 6
            let sk = keylet::account_root_key(&sender);
            let sd = sandbox.read(&sk).unwrap();
            let sv: serde_json::Value = serde_json::from_slice(&sd).unwrap();
            assert_eq!(sv["Sequence"], 6);

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();
    }
}
