//! Cashing an expired check waits for the last pass.
//! - **rippled:** CheckCash::preclaim returns tecEXPIRED (CheckCash.cpp:133-136). Under tapRETRY a
//!   preclaim tec isn't likely to claim a fee, so BuildLedger's `applyTransactions` keeps it for the
//!   next pass. It is claimed only once the passes stop being certain. Only tecEXPIRED from doApply
//!   (the reset branch of Transactor::operator()) is claimed in any pass.
//! - **Us, until 2026-11-07:** CheckCash found the expiry in do_apply, so the passes claimed it in
//!   pass 0. Mainnet #108167573 7D9C347F165F and #108167575 D8719C579BDD: the same results as the
//!   network but a different order, so both ledgers went unsigned.
//!
//! The loop and its judgement are differential_probe's `run_passes`.
use std::collections::HashMap;

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::close::{
    account_key, apply_in_passes, canonical_order, consensus_salt, ApplyOutcome, SeqProxy, TransactionEntry,
    ALWAYS_CLAIMED,
};
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::apply_modifications;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, native_apply_one_staged};

/// The parent ledger; the set builds the one after it.
const PARENT: u32 = 108_167_572;
/// The parent's close time, which `hasExpired` reads.
const CLOSED: u64 = 847_342_390;

fn put(state: &mut LedgerState, key: Hash256, v: Value) {
    state.state_map.insert(key, serde_json::to_vec(&v).unwrap()).unwrap();
}

fn root(account: &[u8; 20], balance: u64, sequence: u32, owners: u32) -> Value {
    json!({
        "LedgerEntryType": "AccountRoot",
        "Account": hex::encode(account),
        "Balance": balance.to_string(),
        "Sequence": sequence,
        "OwnerCount": owners,
        "Flags": 0,
    })
}

#[test]
fn an_expired_check_is_cashed_in_the_last_pass_not_the_first() {
    // Two fixed transaction ids fix the set's salt; the casher is whichever account sorts first
    // under it, so a claim in pass 0 would put the cash ahead of the payment.
    let ids = [Hash256([0x33; 32]), Hash256([0x44; 32])];
    let salt = consensus_salt(&ids);
    let (a, b) = ([0x11u8; 20], [0x91u8; 20]);
    let (casher, payer) = if account_key(&a, &salt) < account_key(&b, &salt) { (a, b) } else { (b, a) };
    let writer = [0x55u8; 20];

    let header = LedgerHeader {
        sequence: PARENT,
        total_coins: 99_985_000_000_000_000,
        parent_hash: Hash256([0x10; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: CLOSED as u32 - 1,
        close_time: CLOSED as u32,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    put(&mut state, keylet::account_root_key(&writer), root(&writer, 100_000_000, 2, 1));
    put(&mut state, keylet::account_root_key(&casher), root(&casher, 50_000_000, 3, 0));
    put(&mut state, keylet::account_root_key(&payer), root(&payer, 100_000_000, 7, 0));
    let check = keylet::check_key(&writer, 1);
    put(
        &mut state,
        check,
        json!({
            "LedgerEntryType": "Check",
            "Flags": 0,
            "Account": hex::encode(writer),
            "Destination": hex::encode(casher),
            "SendMax": "5000000",
            "Sequence": 1,
            "Expiration": CLOSED - 100,
            "OwnerNode": "0",
            "DestinationNode": "0",
        }),
    );

    let txs: HashMap<Hash256, Value> = HashMap::from([
        (
            ids[0],
            json!({
                "TransactionType": "CheckCash",
                "Account": hex::encode(casher),
                "CheckID": hex::encode_upper(check.0),
                "Amount": "1000000",
                "Fee": "12",
                "Sequence": 3,
                "Flags": 0,
            }),
        ),
        (
            ids[1],
            json!({
                "TransactionType": "Payment",
                "Account": hex::encode(payer),
                "Destination": hex::encode(writer),
                "Amount": "1000000",
                "Fee": "12",
                "Sequence": 7,
                "Flags": 0,
            }),
        ),
    ]);
    let mut entries = vec![
        TransactionEntry { hash: ids[0], account: casher, seq_proxy: SeqProxy::of(3, None), blob: vec![] },
        TransactionEntry { hash: ids[1], account: payer, seq_proxy: SeqProxy::of(7, None), blob: vec![] },
    ];
    canonical_order(&mut entries, &salt);
    assert_eq!(entries[0].hash, ids[0], "the cash sorts ahead of the payment");

    let mut results: HashMap<Hash256, Vec<(String, bool)>> = HashMap::new();
    let passes = apply_in_passes(entries, |e, certain_retry| {
        let txf = build_txfields(&txs[&e.hash]).expect("txfields");
        let (code, mods, from_do_apply) = native_apply_one_staged(&state, &txf);
        results.entry(e.hash).or_default().push((code.clone(), from_do_apply));
        let outcome = match &code[..3.min(code.len())] {
            "tes" => ApplyOutcome::Success,
            "ter" => ApplyOutcome::Retry,
            "tec" if certain_retry && !(from_do_apply && ALWAYS_CLAIMED.contains(&code.as_str())) => {
                ApplyOutcome::Retry
            }
            "tec" => ApplyOutcome::Success,
            _ => ApplyOutcome::Fail,
        };
        if outcome == ApplyOutcome::Success {
            apply_modifications(&mut state, mods).expect("applies");
        }
        outcome
    });

    let expired = ("tecEXPIRED".to_string(), false);
    assert_eq!(results[&ids[0]], [expired.clone(), expired.clone(), expired], "refused in preclaim each pass, claimed in the last");
    let applied: Vec<(usize, Hash256)> = passes.applied.iter().map(|(pass, e)| (*pass, e.hash)).collect();
    assert_eq!(applied, [(0, ids[1]), (2, ids[0])], "the payment in pass 0, the expired cash in pass 2");
    assert!(passes.failed.is_empty() && passes.left.is_empty());
}
