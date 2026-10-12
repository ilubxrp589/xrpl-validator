//! Pseudo-transactions — EnableAmendment, SetFee, UNLModify (Change.cpp).
//! Injected by consensus on FLAG ledgers (seq % 256 == 0): no Account, no
//! Fee, no Sequence, no signature. The driver bypasses fee/sequence handling
//! entirely (`dispatch::is_pseudo`). Three UNLModify specimens live in the
//! corpus (l105990912 disable+re-enable on a Modified NegativeUNL,
//! l106119168 disable CREATING it) — previously SKIP-PARSE holes.

use crate::ledger::amendments;
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

fn read_or_new(sandbox: &Sandbox, k: &xrpl_core::types::Hash256, ty: &str) -> serde_json::Value {
    sandbox
        .read(k)
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_else(|| serde_json::json!({"LedgerEntryType": ty, "Flags": 0}))
}

/// tfInnerBatchTxn (TxFlags.h).
const TF_INNER_BATCH_TXN: u64 = 0x4000_0000;
/// tfUniversal: tfFullyCanonicalSig | tfInnerBatchTxn.
const TF_UNIVERSAL: u64 = 0x8000_0000 | TF_INNER_BATCH_TXN;
const TF_GOT_MAJORITY: u64 = 0x0001_0000;
const TF_LOST_MAJORITY: u64 = 0x0002_0000;
/// tfEnableAmendmentMask: every flag but the universal ones and the two majority flags.
const TF_ENABLE_AMENDMENT_MASK: u64 = !(TF_UNIVERSAL | TF_GOT_MAJORITY | TF_LOST_MAJORITY) & 0xFFFF_FFFF;

/// `Change::preflight` (Change.cpp, 3.4.1) for all three pseudo-transactions, with the parts of `preflight0`
/// that reach them:
/// - no tfInnerBatchTxn;
/// - once LendingProtocol is enabled, no flag outside tfEnableAmendmentMask (temINVALID_FLAG);
/// - the zero account (temBAD_SRC_ACCOUNT);
/// - a native, zero fee (temBAD_FEE);
/// - no SigningPubKey, TxnSignature or Signers (temBAD_SIGNATURE);
/// - Sequence 0 and no PreviousTxnID (temBAD_SEQUENCE).
///
/// Consensus never sends a pseudo-transaction that fails one of these, so they change no mainnet result.
/// They matter for a transaction set taken from peers.
fn change_preflight(tx: &TxFields, rules: &Sandbox) -> TxResult {
    let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
    if flags & TF_INNER_BATCH_TXN != 0 {
        return TxResult::InvalidFlag;
    }
    if amendments::enabled(rules, amendments::LENDING_PROTOCOL) && flags & TF_ENABLE_AMENDMENT_MASK != 0 {
        return TxResult::InvalidFlag;
    }
    if tx.account != [0u8; 20] {
        return TxResult::BadSrcAccount;
    }
    if !tx.fields.get("Fee").and_then(|f| f.as_str()).is_some_and(|f| f.parse::<u64>() == Ok(0)) {
        return TxResult::BadFee;
    }
    let set = |k: &str| tx.fields.get(k).is_some_and(|v| v.as_str() != Some(""));
    if set("SigningPubKey") || set("TxnSignature") || tx.fields.get("Signers").is_some() {
        return TxResult::BadSignature;
    }
    if tx.sequence != 0 || tx.fields.get("PreviousTxnID").is_some() {
        return TxResult::BadSequence;
    }
    TxResult::Success
}

/// `Change::preclaim` for SetFee under XRPFees (on mainnet). It needs all three drops fields and none of the
/// four legacy ones, else temMALFORMED. It runs with the format checks because the pseudo pipeline runs no
/// preclaim, and like rippled's it reads only the transaction and the rules. The pre-XRPFees branch (legacy
/// fields required, a drops field temDISABLED) is not ported: do_apply writes the drops fields too.
fn set_fee_fields(tx: &TxFields, rules: &Sandbox) -> TxResult {
    const DROPS: [&str; 3] = ["BaseFeeDrops", "ReserveBaseDrops", "ReserveIncrementDrops"];
    const LEGACY: [&str; 4] = ["BaseFee", "ReferenceFeeUnits", "ReserveBase", "ReserveIncrement"];
    let has = |k: &&str| tx.fields.get(*k).is_some();
    if amendments::enabled(rules, amendments::XRP_FEES) && (!DROPS.iter().all(has) || LEGACY.iter().any(has)) {
        return TxResult::Malformed;
    }
    TxResult::Success
}

/// rippled's `publicKeyType`: a 33-byte ed25519 (0xED) or secp256k1 (0x02, 0x03) key.
fn is_public_key(hex_key: &str) -> bool {
    hex::decode(hex_key).is_ok_and(|k| k.len() == 33 && matches!(k[0], 0xED | 0x02 | 0x03))
}

/// UNLModify (Change::applyUNLModify): on a flag ledger, records the PENDING
/// disable/re-enable on the NegativeUNL singleton — sfValidatorToDisable /
/// sfValidatorToReEnable. (The actual DisabledValidators rotation happens at
/// the NEXT flag ledger's ledger-close, outside any transaction.)
pub struct UNLModifyTransactor;

impl Transactor for UNLModifyTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "UNLModify" {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        match self.preflight(tx) {
            TxResult::Success => change_preflight(tx, rules),
            r => r,
        }
    }
    fn preclaim(&self, _tx: &TxFields, _sandbox: &Sandbox) -> TxResult {
        TxResult::Success
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // The harness header is the PARENT's (probe convention: sequence =
        // fixture_seq − 1, close_time = parent close); the ledger being
        // applied is +1.
        let seq = sandbox.base().header.sequence as u64 + 1;
        if seq % 256 != 0 {
            return TxResult::Failure;
        }
        let disabling = match tx.fields.get("UNLModifyDisabling").and_then(|v| v.as_u64()) {
            Some(d @ (0 | 1)) => d != 0,
            _ => return TxResult::Failure,
        };
        if tx.fields.get("LedgerSequence").and_then(|v| v.as_u64()) != Some(seq) {
            return TxResult::Failure;
        }
        let Some(validator) = tx.fields.get("UNLModifyValidator").and_then(|v| v.as_str()) else {
            return TxResult::Failure;
        };
        if !is_public_key(validator) {
            return TxResult::Failure;
        }
        let k = keylet::negative_unl_key();
        let mut nunl = read_or_new(sandbox, &k, "NegativeUNL");
        let in_list = nunl
            .get("DisabledValidators")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter().any(|e| {
                    e.get("DisabledValidator")
                        .and_then(|d| d.get("PublicKey"))
                        .and_then(|p| p.as_str())
                        .is_some_and(|p| p.eq_ignore_ascii_case(validator))
                })
            })
            .unwrap_or(false);
        let same = |f: &str| {
            nunl.get(f).and_then(|v| v.as_str()).is_some_and(|v| v.eq_ignore_ascii_case(validator))
        };
        if disabling {
            if nunl.get("ValidatorToDisable").is_some()
                || same("ValidatorToReEnable")
                || in_list
            {
                return TxResult::Failure;
            }
            nunl["ValidatorToDisable"] = serde_json::Value::String(validator.to_string());
        } else {
            if nunl.get("ValidatorToReEnable").is_some() || same("ValidatorToDisable") || !in_list
            {
                return TxResult::Failure;
            }
            nunl["ValidatorToReEnable"] = serde_json::Value::String(validator.to_string());
        }
        sandbox.write(k, serde_json::to_vec(&nunl).unwrap_or_default());
        TxResult::Success
    }
}

/// SetFee (Change::applyFee), XRPFees era: BaseFeeDrops /
/// ReserveBaseDrops / ReserveIncrementDrops replace the legacy quartet.
pub struct SetFeeTransactor;

impl Transactor for SetFeeTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "SetFee" {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        match self.preflight(tx) {
            TxResult::Success => match change_preflight(tx, rules) {
                TxResult::Success => set_fee_fields(tx, rules),
                r => r,
            },
            r => r,
        }
    }
    fn preclaim(&self, _tx: &TxFields, _sandbox: &Sandbox) -> TxResult {
        TxResult::Success
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let k = keylet::fee_settings_key();
        let mut fees = read_or_new(sandbox, &k, "FeeSettings");
        for f in ["BaseFeeDrops", "ReserveBaseDrops", "ReserveIncrementDrops"] {
            if let Some(v) = tx.fields.get(f) {
                fees[f] = v.clone();
            }
        }
        // XRPFees is active on mainnet: the legacy fields are removed.
        if let Some(o) = fees.as_object_mut() {
            for f in ["BaseFee", "ReferenceFeeUnits", "ReserveBase", "ReserveIncrement"] {
                o.remove(f);
            }
        }
        sandbox.write(k, serde_json::to_vec(&fees).unwrap_or_default());
        TxResult::Success
    }
}

/// EnableAmendment (Change::applyAmendment): tfGotMajority (0x00010000)
/// records a Majority entry stamped with the parent close time;
/// tfLostMajority removes it; NO flags enables the amendment outright.
pub struct EnableAmendmentTransactor;

impl Transactor for EnableAmendmentTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "EnableAmendment" {
            return TxResult::Malformed;
        }
        if tx.fields.get("Amendment").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preflight_in(&self, tx: &TxFields, rules: &Sandbox) -> TxResult {
        match self.preflight(tx) {
            TxResult::Success => change_preflight(tx, rules),
            r => r,
        }
    }
    fn preclaim(&self, _tx: &TxFields, _sandbox: &Sandbox) -> TxResult {
        TxResult::Success
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(amendment) = tx.fields.get("Amendment").and_then(|v| v.as_str()) else {
            return TxResult::Malformed;
        };
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let (got, lost) = (flags & TF_GOT_MAJORITY != 0, flags & TF_LOST_MAJORITY != 0);
        let k = keylet::amendments_key();
        let mut obj = read_or_new(sandbox, &k, "Amendments");
        let enabled: Vec<String> = obj
            .get("Amendments")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if enabled.iter().any(|a| a.eq_ignore_ascii_case(amendment)) {
            return TxResult::Already;
        }
        if got && lost {
            return TxResult::InvalidFlag; // checked after the enabled list, as Change::applyAmendment does
        }
        let mut majorities: Vec<serde_json::Value> = obj
            .get("Majorities")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let had = majorities.iter().any(|m| {
            m.get("Majority")
                .and_then(|x| x.get("Amendment"))
                .and_then(|a| a.as_str())
                .is_some_and(|a| a.eq_ignore_ascii_case(amendment))
        });
        if got && had {
            return TxResult::Already;
        }
        if lost && !had {
            return TxResult::Already;
        }
        majorities.retain(|m| {
            !m.get("Majority")
                .and_then(|x| x.get("Amendment"))
                .and_then(|a| a.as_str())
                .is_some_and(|a| a.eq_ignore_ascii_case(amendment))
        });
        if got {
            majorities.push(serde_json::json!({"Majority": {
                "Amendment": amendment,
                "CloseTime": sandbox.base().header.close_time,
            }}));
        } else if !lost {
            // enable outright
            let mut e = enabled;
            e.push(amendment.to_string());
            obj["Amendments"] = serde_json::Value::Array(
                e.into_iter().map(serde_json::Value::String).collect(),
            );
        }
        if majorities.is_empty() {
            obj.as_object_mut().map(|o| o.remove("Majorities"));
        } else {
            obj["Majorities"] = serde_json::Value::Array(majorities);
        }
        sandbox.write(k, serde_json::to_vec(&obj).unwrap_or_default());
        TxResult::Success
    }
}

/// `Ledger::updateNegativeUNL` — the FLAG-LEDGER OPEN rotation, a
/// ledger-level action that runs BEFORE any transaction: the pending
/// ValidatorToDisable joins DisabledValidators (stamped FirstLedgerSequence =
/// this flag ledger), the pending ValidatorToReEnable's entry leaves, both
/// pending fields clear; an emptied object is erased. Without it, this
/// ledger's own UNLModify pseudo-txs see stale pending fields and refuse
/// (l105990912's disable read the parent period's ToDisable → tefFAILURE).
/// Returns the rotated object bytes (None = erase), or None-no-change.
pub fn rotate_negative_unl(nunl_bytes: &[u8], flag_seq: u32) -> Option<Option<Vec<u8>>> {
    let mut nunl: serde_json::Value = serde_json::from_slice(nunl_bytes).ok()?;
    let to_disable = nunl.get("ValidatorToDisable").and_then(|v| v.as_str()).map(str::to_string);
    let to_reenable =
        nunl.get("ValidatorToReEnable").and_then(|v| v.as_str()).map(str::to_string);
    if to_disable.is_none() && to_reenable.is_none() {
        return None;
    }
    let mut list: Vec<serde_json::Value> = nunl
        .get("DisabledValidators")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if let Some(re) = &to_reenable {
        list.retain(|e| {
            !e.get("DisabledValidator")
                .and_then(|d| d.get("PublicKey"))
                .and_then(|p| p.as_str())
                .is_some_and(|p| p.eq_ignore_ascii_case(re))
        });
    }
    if let Some(dis) = &to_disable {
        list.push(serde_json::json!({"DisabledValidator": {
            "FirstLedgerSequence": flag_seq,
            "PublicKey": dis,
        }}));
    }
    if list.is_empty() {
        return Some(None); // erase the object
    }
    let o = nunl.as_object_mut()?;
    o.remove("ValidatorToDisable");
    o.remove("ValidatorToReEnable");
    o.insert("DisabledValidators".into(), serde_json::Value::Array(list));
    Some(Some(serde_json::to_vec(&nunl).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use serde_json::{json, Value};
    use xrpl_core::types::Hash256;

    /// The state a ledger applies on: its parent at `parent_seq`, with an Amendments object enabling `ids`.
    fn state(parent_seq: u32, ids: &[&str]) -> LedgerState {
        let mut st = LedgerState::new_unverified(LedgerHeader {
            sequence: parent_seq,
            total_coins: 0,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 842_441_332,
            close_time_resolution: 10,
            close_flags: 0,
        });
        if !ids.is_empty() {
            let object = json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": ids});
            st.state_map.insert(keylet::amendments_key(), object.to_string().into_bytes()).unwrap();
        }
        st
    }

    /// A pseudo-transaction as the engine sees it: the zero account, no fee, Sequence 0.
    fn pseudo(v: Value) -> TxFields {
        TxFields {
            account: [0u8; 20],
            tx_type: v["TransactionType"].as_str().unwrap().to_string(),
            fee: 0,
            sequence: v["Sequence"].as_u64().unwrap_or(0) as u32,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: v,
            inner_batch: false,
        }
    }

    /// Mainnet #106911489's EnableAmendment (fixCleanup3_3_0).
    fn enable_amendment() -> TxFields {
        pseudo(json!({"Account": "rrrrrrrrrrrrrrrrrrrrrhoLvTp", "Amendment": amendments::FIX_CLEANUP_3_3_0, "Fee": "0",
                      "LedgerSequence": 106911489, "Sequence": 0, "SigningPubKey": "", "TransactionType": "EnableAmendment"}))
    }

    /// Mainnet #107068416's UNLModify, disabling a validator; its Account is empty.
    fn unl_modify() -> TxFields {
        pseudo(json!({"Account": "", "Fee": "0", "LedgerSequence": 107068416, "Sequence": 0, "SigningPubKey": "",
                      "TransactionType": "UNLModify", "UNLModifyDisabling": 1,
                      "UNLModifyValidator": "ED9AE4F5887BA029EB7C0884486D23CF281975F773F44BD213054219882C411CC7"}))
    }

    /// A SetFee of the XRPFees era, with `extra` fields added.
    fn set_fee(extra: Value) -> TxFields {
        let mut v = json!({"Account": "rrrrrrrrrrrrrrrrrrrrrhoLvTp", "BaseFeeDrops": "10", "Fee": "0",
                           "LedgerSequence": 107001857, "ReserveBaseDrops": "1000000",
                           "ReserveIncrementDrops": "200000", "Sequence": 0, "SigningPubKey": "",
                           "TransactionType": "SetFee"});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        pseudo(v)
    }

    fn check(tx: &TxFields, st: &LedgerState) -> TxResult {
        let rules = Sandbox::new(st);
        match tx.tx_type.as_str() {
            "EnableAmendment" => EnableAmendmentTransactor.preflight_in(tx, &rules),
            "SetFee" => SetFeeTransactor.preflight_in(tx, &rules),
            _ => UNLModifyTransactor.preflight_in(tx, &rules),
        }
    }

    fn with(mut tx: TxFields, field: &str, value: Value) -> TxFields {
        tx.fields[field] = value;
        tx
    }

    #[test]
    fn mainnet_shaped_pseudo_transactions_pass() {
        let rules = state(107068415, &[amendments::XRP_FEES]);
        assert_eq!(check(&enable_amendment(), &rules), TxResult::Success);
        assert_eq!(check(&unl_modify(), &rules), TxResult::Success);
        assert_eq!(check(&set_fee(json!({})), &rules), TxResult::Success);
        let lending = state(107068415, &[amendments::XRP_FEES, amendments::LENDING_PROTOCOL]);
        assert_eq!(check(&with(enable_amendment(), "Flags", json!(TF_GOT_MAJORITY)), &lending), TxResult::Success);
    }

    #[test]
    fn the_account_must_be_zero() {
        let mut tx = enable_amendment();
        tx.account = [1; 20];
        assert_eq!(check(&tx, &state(1, &[])), TxResult::BadSrcAccount);
    }

    #[test]
    fn the_fee_must_be_native_and_zero() {
        let rules = state(1, &[]);
        assert_eq!(check(&with(unl_modify(), "Fee", json!("10")), &rules), TxResult::BadFee);
        let iou = json!({"currency": "USD", "issuer": "rrrrrrrrrrrrrrrrrrrrBZbvji", "value": "0"});
        assert_eq!(check(&with(unl_modify(), "Fee", iou), &rules), TxResult::BadFee);
    }

    #[test]
    fn a_pseudo_transaction_carries_no_signature() {
        let rules = state(1, &[]);
        assert_eq!(check(&with(enable_amendment(), "SigningPubKey", json!("02".repeat(33))), &rules), TxResult::BadSignature);
        assert_eq!(check(&with(enable_amendment(), "TxnSignature", json!("3045")), &rules), TxResult::BadSignature);
        assert_eq!(check(&with(enable_amendment(), "Signers", json!([])), &rules), TxResult::BadSignature);
    }

    #[test]
    fn sequence_zero_and_no_previous_txn_id() {
        let rules = state(1, &[]);
        let mut tx = enable_amendment();
        tx.sequence = 5;
        assert_eq!(check(&tx, &rules), TxResult::BadSequence);
        assert_eq!(check(&with(enable_amendment(), "PreviousTxnID", json!("AA".repeat(32))), &rules), TxResult::BadSequence);
    }

    #[test]
    fn flags_are_masked_once_lending_protocol_is_enabled() {
        let odd = with(enable_amendment(), "Flags", json!(0x0008_0000u64));
        assert_eq!(check(&odd, &state(1, &[])), TxResult::Success, "no mask before LendingProtocol");
        assert_eq!(check(&odd, &state(1, &[amendments::LENDING_PROTOCOL])), TxResult::InvalidFlag);
        let inner = with(set_fee(json!({})), "Flags", json!(TF_INNER_BATCH_TXN));
        assert_eq!(check(&inner, &state(1, &[])), TxResult::InvalidFlag, "never an inner batch transaction");
    }

    #[test]
    fn set_fee_takes_the_three_drops_fields_and_none_of_the_legacy_ones() {
        let rules = state(1, &[amendments::XRP_FEES]);
        let mut short = set_fee(json!({}));
        short.fields.as_object_mut().unwrap().remove("ReserveIncrementDrops");
        assert_eq!(check(&short, &rules), TxResult::Malformed);
        assert_eq!(check(&set_fee(json!({"BaseFee": "000000000000000A"})), &rules), TxResult::Malformed);
        assert_eq!(check(&short, &state(1, &[])), TxResult::Success, "the pre-XRPFees rules are not ported");
    }

    #[test]
    fn unl_modify_needs_a_real_public_key() {
        let st = state(107068415, &[]); // the next ledger, 107068416, is a flag ledger
        let apply = |tx: &TxFields| UNLModifyTransactor.do_apply(tx, &mut Sandbox::new(&st));
        assert_eq!(apply(&unl_modify()), TxResult::Success);
        let bad_prefix = "05".to_string() + &"11".repeat(32);
        assert_eq!(apply(&with(unl_modify(), "UNLModifyValidator", json!(bad_prefix))), TxResult::Failure);
        assert_eq!(apply(&with(unl_modify(), "UNLModifyValidator", json!("ED".repeat(32)))), TxResult::Failure);
    }

    #[test]
    fn enable_amendment_already_done_is_tef_already() {
        let apply = |tx: &TxFields, st: &LedgerState| EnableAmendmentTransactor.do_apply(tx, &mut Sandbox::new(st));
        let on = state(106911488, &[amendments::FIX_CLEANUP_3_3_0]);
        assert_eq!(apply(&enable_amendment(), &on), TxResult::Already);
        let off = state(106911488, &[amendments::XRP_FEES]);
        assert_eq!(apply(&with(enable_amendment(), "Flags", json!(TF_LOST_MAJORITY)), &off), TxResult::Already);
        assert_eq!(apply(&with(enable_amendment(), "Flags", json!(TF_GOT_MAJORITY | TF_LOST_MAJORITY)), &off), TxResult::InvalidFlag);
        let mut sb = Sandbox::new(&off);
        let got = with(enable_amendment(), "Flags", json!(TF_GOT_MAJORITY));
        assert_eq!(EnableAmendmentTransactor.do_apply(&got, &mut sb), TxResult::Success);
        assert_eq!(EnableAmendmentTransactor.do_apply(&got, &mut sb), TxResult::Already, "a majority twice");
    }
}
