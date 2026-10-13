//! A1 of the 3.3 NoAccount audit (10-13): rippled answers a missing AMM pool with terNO_AMM, a retry
//! (AMMDeposit.cpp:186, AMMWithdraw.cpp:192, AMMVote.cpp:65, AMMBid.cpp:111, AMMClawback.cpp:116).
//! - **Complete state** (`LedgerState::complete`, the native shadow's hydrated mirror): an absent pool
//!   is absent from the ledger, and we answer terNO_AMM.
//! - **Partial pre-state** (the probe's): a pool that reads as missing may just not be loaded, so each
//!   transactor keeps its older answer.
use std::collections::HashMap;

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, native_apply_one};

const ALICE: [u8; 20] = [0xA1; 20];
const GATE: [u8; 20] = [0x6A; 20];

/// ALICE and GATE, and no AMM pool anywhere.
fn ledger() -> LedgerState {
    let mut state = LedgerState::new_unverified(LedgerHeader {
        sequence: 107_600_000,
        total_coins: 99_986_000_000_000_000,
        parent_hash: Hash256([0x10; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: 812_999_996,
        close_time: 813_000_000,
        close_time_resolution: 10,
        close_flags: 0,
    });
    for id in [ALICE, GATE] {
        let root = json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": "100000000",
            "Sequence": 7,
            "OwnerCount": 0,
            "Flags": 0,
        });
        state.state_map.insert(keylet::account_root_key(&id), serde_json::to_vec(&root).unwrap()).unwrap();
    }
    state
}

/// One transaction of `kind` on the XRP / GATE-USD pool that doesn't exist.
fn on_the_missing_pool(kind: &str) -> ([u8; 20], Value) {
    let mut tx = json!({
        "TransactionType": kind,
        "Account": hex::encode(ALICE),
        "Asset": { "currency": "XRP" },
        "Asset2": { "currency": "USD", "issuer": hex::encode(GATE) },
        "Fee": "12",
        "Sequence": 7,
        "Flags": 0,
    });
    let mut sender = ALICE;
    match kind {
        "AMMDeposit" => {
            tx["Flags"] = json!(0x0008_0000); // tfSingleAsset
            tx["Amount"] = json!("1000000");
        }
        "AMMWithdraw" => tx["Flags"] = json!(0x0002_0000), // tfWithdrawAll
        "AMMVote" => tx["TradingFee"] = json!(500),
        "AMMClawback" => {
            // GATE claws its USD back from ALICE's share of the pool.
            sender = GATE;
            tx["Account"] = json!(hex::encode(GATE));
            tx["Asset"] = json!({ "currency": "USD", "issuer": hex::encode(GATE) });
            tx["Asset2"] = json!({ "currency": "XRP" });
            tx["Holder"] = json!(hex::encode(ALICE));
        }
        _ => {}
    }
    (sender, tx)
}

fn apply(state: &LedgerState, tx: &Value) -> (String, HashMap<Hash256, SandboxEntry>) {
    native_apply_one(state, &build_txfields(tx).expect("txfields"))
}

#[test]
fn a_missing_pool_is_ter_no_amm_on_a_complete_state_and_unchanged_on_a_partial_one() {
    for (kind, partial) in [
        ("AMMDeposit", "tecNO_ENTRY"),
        ("AMMWithdraw", "tecNO_ENTRY"),
        ("AMMVote", "tesSUCCESS"),
        ("AMMBid", "tesSUCCESS"),
        ("AMMClawback", "tecNO_ENTRY"),
    ] {
        let (sender, tx) = on_the_missing_pool(kind);
        let mut state = ledger();
        assert!(!state.complete, "a fresh LedgerState is partial");

        let (ter, mods) = apply(&state, &tx);
        assert_eq!(ter, partial, "{kind} on a partial state keeps its older answer");
        assert!(mods.contains_key(&keylet::account_root_key(&sender)), "{kind}: that answer claims the fee");

        state.complete = true;
        let (ter, mods) = apply(&state, &tx);
        assert_eq!(ter, "terNO_AMM", "{kind} on a complete state");
        assert!(mods.is_empty(), "{kind}: a ter changes nothing: {mods:?}");
    }
}
