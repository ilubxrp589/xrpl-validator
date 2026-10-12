//! Independence: what a transaction's fee costs when its payer can't cover it, through the live
//! native pipeline (`native_apply_one`), against rippled 3.4.1.
//! - **`Transactor::checkFee`** (Transactor.cpp:605-617) runs ahead of the transactor's own
//!   preclaim (applySteps.cpp:204). In a closed ledger, a payer holding less than the Fee is
//!   tecINSUFF_FEE when it holds some XRP, and terINSUF_FEE_B when it holds none.
//! - **`Transactor::reset`** (Transactor.cpp:1347-1365): the claim takes what the payer holds and
//!   still consumes the sequence. The ledger's total XRP falls by that, not by the Fee.
//!
//! Before this test we left a short payer's transaction out with "tecINSUFFICIENT_FEE" (not a
//! rippled code), charged nothing and kept the sequence.
use std::collections::HashMap;

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, native_apply_one};

const ALICE: [u8; 20] = [0xA1; 20];
const BOB: [u8; 20] = [0xB0; 20];

/// A ledger holding the given accounts, each at Sequence 7. It has no FeeSettings, so the engine
/// uses the 1 XRP base reserve.
fn ledger(accounts: &[([u8; 20], u64)]) -> LedgerState {
    let header = LedgerHeader {
        sequence: 107_600_000,
        total_coins: 99_986_000_000_000_000,
        parent_hash: Hash256([0x10; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: 813_000_000,
        close_time: 813_000_000,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    for (id, balance) in accounts {
        let root = json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": 7,
            "OwnerCount": 0,
            "Flags": 0,
        });
        state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&root).unwrap()).unwrap();
    }
    state
}

fn pay_bob(fee: u64, drops: u64) -> Value {
    json!({
        "TransactionType": "Payment",
        "Account": hex::encode(ALICE),
        "Destination": hex::encode(BOB),
        "Amount": drops.to_string(),
        "Fee": fee.to_string(),
        "Sequence": 7,
        "Flags": 0,
    })
}

fn apply(state: &LedgerState, tx: &Value) -> (String, HashMap<Hash256, SandboxEntry>) {
    native_apply_one(state, &build_txfields(tx).expect("txfields"))
}

fn root_after(mods: &HashMap<Hash256, SandboxEntry>, id: &[u8; 20]) -> Value {
    match mods.get(&keylet::account_root_key(id)) {
        Some(SandboxEntry::Modified(bytes)) => serde_json::from_slice(bytes).unwrap(),
        other => panic!("the AccountRoot was not modified: {other:?}"),
    }
}

#[test]
fn a_payer_short_of_the_fee_is_charged_what_it_holds() {
    let state = ledger(&[(ALICE, 5), (BOB, 50_000_000)]);
    let (ter, mods) = apply(&state, &pay_bob(12, 1_000_000));
    assert_eq!(ter, "tecINSUFF_FEE");
    let alice = root_after(&mods, &ALICE);
    assert_eq!(alice["Balance"], "0", "the claim took all 5 drops");
    assert_eq!(alice["Sequence"], 8, "and consumed the sequence");
    assert_eq!(mods.len(), 1, "nothing else changes: {mods:?}");
}

#[test]
fn a_payer_with_nothing_is_not_applied() {
    let state = ledger(&[(ALICE, 0), (BOB, 50_000_000)]);
    let (ter, mods) = apply(&state, &pay_bob(12, 1_000_000));
    assert_eq!(ter, "terINSUF_FEE_B");
    assert!(mods.is_empty(), "{mods:?}");
}

/// checkFee answers before the transactor's own preclaim. Bob doesn't exist and 1,000 drops is
/// below the reserve, so the Payment alone would be tecNO_DST_INSUF_XRP; the fee check comes
/// first.
#[test]
fn the_fee_is_checked_before_the_payments_own_preclaim() {
    let state = ledger(&[(ALICE, 5)]);
    let (ter, mods) = apply(&state, &pay_bob(12, 1_000));
    assert_eq!(ter, "tecINSUFF_FEE");
    assert_eq!(root_after(&mods, &ALICE)["Balance"], "0");
}

/// A payer that can cover the Fee pays all of it: the cap never bites.
#[test]
fn a_payer_that_can_cover_the_fee_pays_all_of_it() {
    let state = ledger(&[(ALICE, 50_000_000), (BOB, 50_000_000)]);
    let (ter, mods) = apply(&state, &pay_bob(12, 1_000_000));
    assert_eq!(ter, "tesSUCCESS");
    assert_eq!(root_after(&mods, &ALICE)["Balance"], (50_000_000u64 - 12 - 1_000_000).to_string());
}
