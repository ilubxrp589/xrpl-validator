//! Pseudo-transactions — EnableAmendment, SetFee, UNLModify (Change.cpp).
//! Injected by consensus on FLAG ledgers (seq % 256 == 0): no Account, no
//! Fee, no Sequence, no signature. The driver bypasses fee/sequence handling
//! entirely (`dispatch::is_pseudo`). Three UNLModify specimens live in the
//! corpus (l105990912 disable+re-enable on a Modified NegativeUNL,
//! l106119168 disable CREATING it) — previously SKIP-PARSE holes.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

fn read_or_new(sandbox: &Sandbox, k: &xrpl_core::types::Hash256, ty: &str) -> serde_json::Value {
    sandbox
        .read(k)
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_else(|| serde_json::json!({"LedgerEntryType": ty, "Flags": 0}))
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
    fn preclaim(&self, _tx: &TxFields, _sandbox: &Sandbox) -> TxResult {
        TxResult::Success
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(amendment) = tx.fields.get("Amendment").and_then(|v| v.as_str()) else {
            return TxResult::Malformed;
        };
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let (got, lost) = (flags & 0x0001_0000 != 0, flags & 0x0002_0000 != 0);
        if got && lost {
            return TxResult::Malformed;
        }
        let k = keylet::amendments_key();
        let mut obj = read_or_new(sandbox, &k, "Amendments");
        let enabled: Vec<String> = obj
            .get("Amendments")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if enabled.iter().any(|a| a.eq_ignore_ascii_case(amendment)) {
            return TxResult::Failure; // tefALREADY
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
            return TxResult::Failure; // tefALREADY
        }
        if lost && !had {
            return TxResult::Failure; // tefALREADY
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
