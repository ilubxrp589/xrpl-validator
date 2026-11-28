//! fixCleanup3_5_0 in EscrowFinish.
//!
//! The condition — EscrowFinish::preflightSigValidated (3.5.0-rc1
//! EscrowFinish.cpp:137-153): when the Condition / Fulfillment pair fails
//! `checkCondition` and the Condition itself does not deserialize
//! (`Condition::deserialize`, Condition.cpp:144-214 — any type but
//! PREIMAGE-SHA-256 included), the transaction is temMALFORMED: not applied,
//! no fee. Before it, preflight passes and doApply answers
//! tecCRYPTOCONDITION_ERROR (EscrowFinish.cpp:331), claiming the fee.
//! Escrow_test.cpp:943-950, :1047-1053 and :1102-1135 pin all three shapes.
//!
//! The MPT fee — divRound with an MPT operand runs on `Number` under
//! fixCleanup3_5_0 as under MPTokensV2 (3.5.0-rc1 STAmount.cpp:1670-1685),
//! which reaches an MPT escrow's transfer fee while fixCleanup3_4_0 is off
//! (EscrowHelpers.h:549-553, `divideRound(amount, lockedRate, asset, up)`).
use super::*;
use crate::ledger::amendments as am;
use sha2::{Digest, Sha256};

const ALICE: [u8; 20] = [0x01; 20];
const BOB: [u8; 20] = [0x02; 20];
const CAROL: [u8; 20] = [0x03; 20];

/// The empty preimage's fulfillment and condition (cost 0).
fn fulfillment() -> Vec<u8> {
    vec![0xA0, 0x02, 0x80, 0x00]
}

fn condition() -> Vec<u8> {
    let fp: [u8; 32] = Sha256::digest(b"").into();
    let mut c = vec![0xA0, 0x25, 0x80, 0x20];
    c.extend_from_slice(&fp);
    c.extend_from_slice(&[0x81, 0x01, 0x00]);
    c
}

/// ALICE's escrow (sequence 1) to CAROL locked by `condition()`; the ledger's
/// rules are mainnet's plus fixCleanup3_5_0 when `fix`.
fn escrow_state(fix: bool) -> LedgerState {
    let mut state = make_state();
    add_account(&mut state, &ALICE, 75_000_000, 2);
    add_account(&mut state, &BOB, 50_000_000, 1);
    add_account(&mut state, &CAROL, 10_000_000, 1);
    let key = keylet::account_root_key(&ALICE);
    let mut acct: serde_json::Value = serde_json::from_slice(state.state_map.lookup(&key).unwrap()).unwrap();
    acct["OwnerCount"] = serde_json::json!(1);
    state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
    let escrow = serde_json::json!({
        "LedgerEntryType": "Escrow", "Account": hex::encode(ALICE), "Destination": hex::encode(CAROL),
        "Amount": "1000000", "CancelAfter": 100, "Condition": hex::encode_upper(condition()), "OwnerNode": "0",
    });
    state.state_map.insert(keylet::escrow_key(&ALICE, 1), serde_json::to_vec(&escrow).unwrap()).unwrap();
    let mut rules = vec![am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0];
    if fix {
        rules.push(am::FIX_CLEANUP_3_5_0);
    }
    let amendments = serde_json::json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": rules});
    state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&amendments).unwrap()).unwrap();
    state
}

/// BOB finishes ALICE's escrow presenting `condition` and `fulfillment`.
fn finish(condition: &[u8], fulfillment: &[u8]) -> TxFields {
    TxFields {
        account: BOB, tx_type: "EscrowFinish".to_string(), fee: 150, sequence: 1, last_ledger_seq: None, ticket_seq: None,
        fields: serde_json::json!({
            "Owner": hex::encode(ALICE), "OfferSequence": 1,
            "Condition": hex::encode_upper(condition), "Fulfillment": hex::encode_upper(fulfillment),
        }),
        inner_batch: false,
    }
}

/// The whole pipeline: (result, whether it was applied — fee claimed).
fn apply(state: &LedgerState, tx: &TxFields) -> (TxResult, bool) {
    let mut sb = Sandbox::new(state);
    crate::tx::dispatch::apply_on_sandbox(tx, &mut sb)
}

#[test]
fn a_condition_that_does_not_deserialize_is_malformed_under_fixcleanup350() {
    // Escrow_test.cpp:1117: three bytes that are no condition at all.
    let garbage = [0x78u8, 0x78, 0x78];
    // Escrow_test.cpp:943-950: the condition shifted by one byte.
    let shifted = condition()[1..].to_vec();
    // An ED25519-SHA-256 condition ([4]): well-formed DER, an unsupported type.
    let mut ed25519 = condition();
    ed25519[0] = 0xA4;
    // Escrow_test.cpp:1047-1053: both fields present and empty.
    for (name, c, f) in [
        ("garbage", garbage.to_vec(), fulfillment()),
        ("shifted", shifted, fulfillment()),
        ("ed25519", ed25519, fulfillment()),
        ("empty", Vec::new(), Vec::new()),
    ] {
        let tx = finish(&c, &f);
        assert_eq!(apply(&escrow_state(false), &tx), (TxResult::CryptoConditionError, true), "{name}: 3.4.1");
        assert_eq!(apply(&escrow_state(true), &tx), (TxResult::Malformed, false), "{name}: fixCleanup3_5_0");
    }
}

#[test]
fn a_well_formed_condition_still_fails_in_do_apply_under_fixcleanup350() {
    // The condition deserializes; only the fulfillment is wrong: preflight
    // passes and doApply answers, as before the fix.
    let wrong = vec![0xA0, 0x03, 0x80, 0x01, 0x42];
    let tx = finish(&condition(), &wrong);
    for fix in [false, true] {
        assert_eq!(apply(&escrow_state(fix), &tx), (TxResult::CryptoConditionError, true), "fix {fix}");
    }
    // The matching pair finishes.
    let tx = finish(&condition(), &fulfillment());
    for fix in [false, true] {
        assert_eq!(apply(&escrow_state(fix), &tx).0, TxResult::Success, "fix {fix}");
    }
}

/// 73 units escrowed at a 1.25% fee. The legacy divRound's
/// canonicalizeRound delivers 72 (finding 371); Number's quotient,
/// 72.0987…, rounded up delivers 73 — no fee. fixCleanup3_4_0's floor
/// (EscrowHelpers.h:540-548) is 72 whatever fixCleanup3_5_0 says.
#[test]
fn an_mpt_escrow_fee_divides_on_number_under_fixcleanup350() {
    let issuer = [0x0Au8; 20];
    let holder = [0x0Bu8; 20];
    let dest = [0x0Cu8; 20];
    let mut state = make_state();
    add_account(&mut state, &issuer, 100_000_000, 5);
    add_account(&mut state, &holder, 100_000_000, 3);
    add_account(&mut state, &dest, 100_000_000, 9);
    let id = mpt_fixture(&mut state, &issuer, &holder, &dest, Some(1_250));
    let ikey = keylet::mpt_issuance_key(&id);
    let create = TxFields {
        account: holder, tx_type: "EscrowCreate".into(), fee: 12, sequence: 3, last_ledger_seq: None, ticket_seq: None,
        fields: serde_json::json!({
            "Amount": {"mpt_issuance_id": hex::encode_upper(id), "value": "73"},
            "Destination": hex::encode(dest), "FinishAfter": 15,
        }),
        inner_batch: false,
    };
    let (r, sb) = run_tx(&state, &EscrowCreateTransactor, &create);
    assert_eq!(r, TxResult::Success);
    let mods = sb.into_modifications();
    apply_modifications(&mut state, mods).unwrap();
    state.header.close_time = 20;
    let finish = TxFields {
        account: dest, tx_type: "EscrowFinish".into(), fee: 12, sequence: 9, last_ledger_seq: None, ticket_seq: None,
        fields: serde_json::json!({"Owner": hex::encode(holder), "OfferSequence": 3}),
        inner_batch: false,
    };
    for (rules, delivered) in [
        (vec![], "72"),
        (vec![am::FIX_CLEANUP_3_5_0], "73"),
        (vec![am::FIX_CLEANUP_3_4_0], "72"),
        (vec![am::FIX_CLEANUP_3_4_0, am::FIX_CLEANUP_3_5_0], "72"),
    ] {
        let amendments = serde_json::json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": rules});
        state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&amendments).unwrap()).unwrap();
        let (r, sb) = run_tx(&state, &EscrowFinishTransactor, &finish);
        assert_eq!(r, TxResult::Success, "{rules:?}");
        assert_eq!(mpt_json(&sb, &keylet::mptoken_key(&ikey, &dest))["MPTAmount"], delivered, "{rules:?}");
    }
}
