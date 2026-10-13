//! Independence 3.2: a new account's first transaction, sorted ahead of the payment that creates it
//! in the same set, waits a pass instead of failing.
//! - **rippled:** the account doesn't exist yet, so `checkSeqProxy` returns terNO_ACCOUNT, a ter.
//!   BuildLedger's `applyTransactions` keeps a ter for the next pass, and by then the payment has
//!   created the account.
//! - **Us, until 2026-10-12:** our code read "tefNO_ACCOUNT", so the passes dropped the transaction
//!   as a failure.
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
const PARENT: u32 = 107_600_000;

fn ledger_with(funder: &[u8; 20], balance: u64) -> LedgerState {
    let header = LedgerHeader {
        sequence: PARENT,
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
    let root = json!({
        "LedgerEntryType": "AccountRoot",
        "Account": hex::encode(funder),
        "Balance": balance.to_string(),
        "Sequence": 7,
        "OwnerCount": 0,
        "Flags": 0,
    });
    state.state_map.insert(keylet::account_root_key(funder), serde_json::to_vec(&root).unwrap()).unwrap();
    state
}

#[test]
fn a_new_accounts_first_transaction_waits_a_pass_for_the_payment_that_creates_it() {
    // Two fixed transaction ids fix the set's salt. Of two accounts that differ in their first bit,
    // the new account is whichever sorts first under that salt, so its transaction comes first.
    let ids = [Hash256([0x11; 32]), Hash256([0x22; 32])];
    let salt = consensus_salt(&ids);
    let (a, b) = ([0x11u8; 20], [0x91u8; 20]);
    let (newbie, funder) = if account_key(&a, &salt) < account_key(&b, &salt) { (a, b) } else { (b, a) };

    let mut state = ledger_with(&funder, 100_000_000);
    let txs: HashMap<Hash256, Value> = HashMap::from([
        // The new account's first transaction. A created account starts at the sequence of the
        // ledger it's created in.
        (
            ids[0],
            json!({
                "TransactionType": "AccountSet",
                "Account": hex::encode(newbie),
                "Fee": "12",
                "Sequence": PARENT + 1,
                "Flags": 0,
            }),
        ),
        // The payment that creates it, with 20 XRP.
        (
            ids[1],
            json!({
                "TransactionType": "Payment",
                "Account": hex::encode(funder),
                "Destination": hex::encode(newbie),
                "Amount": "20000000",
                "Fee": "12",
                "Sequence": 7,
                "Flags": 0,
            }),
        ),
    ]);
    let mut entries = vec![
        TransactionEntry { hash: ids[0], account: newbie, seq_proxy: SeqProxy::of(PARENT + 1, None), blob: vec![] },
        TransactionEntry { hash: ids[1], account: funder, seq_proxy: SeqProxy::of(7, None), blob: vec![] },
    ];
    canonical_order(&mut entries, &salt);
    assert_eq!(entries[0].hash, ids[0], "the new account's transaction sorts ahead of the payment");

    let mut results: HashMap<Hash256, Vec<String>> = HashMap::new();
    let passes = apply_in_passes(entries, |e, certain_retry| {
        let txf = build_txfields(&txs[&e.hash]).expect("txfields");
        let (code, mods, from_do_apply) = native_apply_one_staged(&state, &txf);
        results.entry(e.hash).or_default().push(code.clone());
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

    assert_eq!(results[&ids[0]], ["terNO_ACCOUNT", "tesSUCCESS"], "it waited a pass, then applied");
    assert_eq!(results[&ids[1]], ["tesSUCCESS"]);
    let applied: Vec<(usize, Hash256)> = passes.applied.iter().map(|(pass, e)| (*pass, e.hash)).collect();
    assert_eq!(applied, [(0, ids[1]), (1, ids[0])], "the payment in pass 0, the new account's transaction in pass 1");
    assert!(passes.failed.is_empty() && passes.left.is_empty());
}
