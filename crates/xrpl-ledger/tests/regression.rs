//! Transaction application regression tests.
//!
//! Edge cases, overflow scenarios, and correctness checks.

use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::apply::apply_transaction_set;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::{apply_modifications, Sandbox};
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::ledger::transactor::{apply_common, Transactor, TxFields, TxResult};
use xrpl_ledger::tx::payment::PaymentTransactor;

fn make_state(seq: u32, coins: u64) -> LedgerState {
    let header = LedgerHeader {
        sequence: seq,
        total_coins: coins,
        parent_hash: Hash256([0; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: 0,
        close_time: 700_000_000,
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
        "Flags": 0,
    });
    let key = keylet::account_root_key(id);
    state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
}

fn payment(sender: [u8; 20], dest: [u8; 20], amount: u64, fee: u64, seq: u32) -> TxFields {
    TxFields {
        account: sender,
        tx_type: "Payment".to_string(),
        fee,
        sequence: seq,
        ticket_seq: None,
        last_ledger_seq: None,
        fields: serde_json::json!({
            "Destination": hex::encode(dest),
            "Amount": amount.to_string(),
        }),
    }
}

fn read_balance(state: &LedgerState, id: &[u8; 20]) -> u64 {
    let key = keylet::account_root_key(id);
    let data = state.state_map.lookup(&key).expect("account not found");
    let v: serde_json::Value = serde_json::from_slice(data).unwrap();
    v["Balance"].as_str().unwrap().parse().unwrap()
}

fn read_seq(state: &LedgerState, id: &[u8; 20]) -> u32 {
    let key = keylet::account_root_key(id);
    let data = state.state_map.lookup(&key).expect("account not found");
    let v: serde_json::Value = serde_json::from_slice(data).unwrap();
    v["Sequence"].as_u64().unwrap() as u32
}

// === Self-payment ===

#[test]
fn self_payment_only_burns_fee() {
    let alice = [0x01u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 1);

    let tx = payment(alice, alice, 10_000_000, 12, 1);
    let (new_state, results) = apply_transaction_set(
        &state, vec![(Hash256([0xAA; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::Success);
    // Self-payment: balance goes down by fee only (amount cancels out)
    assert_eq!(read_balance(&new_state, &alice), 50_000_000 - 12);
    // Total coins reduced by fee
    assert_eq!(new_state.header.total_coins, 100_000_000_000_000_000 - 12);
}

// === Empty transaction set ===

#[test]
fn empty_tx_set_produces_next_ledger() {
    let mut state = make_state(100, 100_000_000_000_000_000);
    let alice = [0x01u8; 20];
    add_account(&mut state, &alice, 50_000_000, 1);

    let (new_state, results) = apply_transaction_set(
        &state, vec![], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results.len(), 0);
    assert_eq!(new_state.header.sequence, 101);
    assert_eq!(new_state.header.total_coins, 100_000_000_000_000_000);
    assert_eq!(read_balance(&new_state, &alice), 50_000_000);
}

// === Exact balance payment (spends everything minus fee) ===

#[test]
fn exact_balance_payment() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 20_000_012, 1); // exactly amount + fee
    add_account(&mut state, &bob, 50_000_000, 1);

    let tx = payment(alice, bob, 20_000_000, 12, 1);
    let (new_state, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::Success);
    assert_eq!(read_balance(&new_state, &alice), 0);
    assert_eq!(read_balance(&new_state, &bob), 70_000_000);
}

// === Payment exceeding balance ===

#[test]
fn payment_exceeding_balance_fails() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 1_000_000, 1); // only 1 XRP
    add_account(&mut state, &bob, 50_000_000, 1);

    let tx = payment(alice, bob, 5_000_000, 12, 1); // wants 5 XRP
    let (new_state, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    // preclaim should catch insufficient funds
    assert!(!results[0].result.is_success());
}

// === Multiple transactions from same sender ===

#[test]
fn sequential_payments_from_same_sender() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let carol = [0x03u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 100_000_000, 1);
    add_account(&mut state, &bob, 10_000_000, 1);
    add_account(&mut state, &carol, 10_000_000, 1);

    let txs = vec![
        (Hash256([0x01; 32]), payment(alice, bob, 20_000_000, 12, 1)),
        (Hash256([0x02; 32]), payment(alice, carol, 15_000_000, 12, 2)),
    ];

    let (new_state, results) = apply_transaction_set(
        &state, txs, 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::Success);
    assert_eq!(results[1].result, TxResult::Success);
    // Alice: 100M - 12 - 20M - 12 - 15M = 64,999,976
    assert_eq!(read_balance(&new_state, &alice), 64_999_976);
    assert_eq!(read_balance(&new_state, &bob), 30_000_000);
    assert_eq!(read_balance(&new_state, &carol), 25_000_000);
    assert_eq!(read_seq(&new_state, &alice), 3);
}

// === Ticket-based transaction ===

#[test]
fn ticket_based_payment() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 10);
    add_account(&mut state, &bob, 10_000_000, 1);

    let tx = TxFields {
        account: alice,
        tx_type: "Payment".to_string(),
        fee: 12,
        sequence: 0, // ticket-based
        ticket_seq: Some(5),
        last_ledger_seq: None,
        fields: serde_json::json!({
            "Destination": hex::encode(bob),
            "Amount": "5000000",
        }),
    };

    let (new_state, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::Success);
    assert_eq!(read_balance(&new_state, &alice), 50_000_000 - 12 - 5_000_000);
    assert_eq!(read_balance(&new_state, &bob), 15_000_000);
    // Sequence should NOT be incremented for ticket-based tx
    assert_eq!(read_seq(&new_state, &alice), 10);
}

// === New account creation at exact reserve ===

#[test]
fn create_account_at_exact_reserve() {
    let alice = [0x01u8; 20];
    let newbie = [0x99u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 1);

    // Send exactly 10 XRP (default reserve)
    let tx = payment(alice, newbie, 10_000_000, 12, 1);
    let (new_state, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::Success);
    assert_eq!(read_balance(&new_state, &newbie), 10_000_000);
    assert_eq!(read_seq(&new_state, &newbie), 1);
}

// === New account below reserve fails ===

#[test]
fn create_account_below_reserve_fails() {
    let alice = [0x01u8; 20];
    let newbie = [0x99u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 1);

    let tx = payment(alice, newbie, 9_999_999, 12, 1); // 1 drop below reserve
    let (new_state, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::NoDstInsufXrp);
    // Newbie should NOT exist
    let key = keylet::account_root_key(&newbie);
    assert!(new_state.state_map.lookup(&key).is_none());
}

// === Fee-only for unsupported tx type ===

#[test]
fn unsupported_type_still_burns_fee() {
    let alice = [0x01u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 1);

    let tx = TxFields {
        account: alice,
        tx_type: "BatchSubmit".to_string(),
        fee: 100,
        sequence: 1,
        ticket_seq: None,
        last_ledger_seq: None,
        fields: serde_json::json!({}),
    };

    let (new_state, results) = apply_transaction_set(
        &state, vec![(Hash256([0xFF; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    // Fee deducted, tx marked unsupported
    assert_eq!(results[0].fee, 100);
    assert_eq!(new_state.header.total_coins, 100_000_000_000_000_000 - 100);
    assert_eq!(read_balance(&new_state, &alice), 50_000_000 - 100);
}

// === tec from do_apply only commits fee (not partial state) ===

#[test]
fn tec_rollback_only_commits_fee() {
    let alice = [0x01u8; 20];
    let nonexistent = [0xDDu8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 1);
    // nonexistent account does NOT exist — payment below reserve should tec

    // Use sandbox directly to test the tec rollback
    let mods = {
        let mut sandbox = Sandbox::new(&state);
        let tx = payment(alice, nonexistent, 5_000_000, 12, 1); // below 10M reserve

        // apply_common succeeds (deducts fee)
        let common = apply_common(&tx, &mut sandbox);
        assert_eq!(common, TxResult::Success);

        // Snapshot after common
        let snapshot = sandbox.snapshot();

        // do_apply should fail with NoDstInsufXrp (tec)
        let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
        assert_eq!(result, TxResult::NoDstInsufXrp);

        // Rollback to snapshot (only fee changes persist)
        sandbox.restore_snapshot(snapshot);
        sandbox.into_modifications()
    };

    apply_modifications(&mut state, mods).unwrap();

    // Alice should only lose the fee, NOT the amount
    assert_eq!(read_balance(&state, &alice), 50_000_000 - 12);
}

// === Conservation: sum of balances + destroyed = original ===

#[test]
fn conservation_of_xrp() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let carol = [0x03u8; 20];
    let initial_coins: u64 = 100_000_000_000_000_000;
    let mut state = make_state(100, initial_coins);
    add_account(&mut state, &alice, 60_000_000, 1);
    add_account(&mut state, &bob, 30_000_000, 1);
    add_account(&mut state, &carol, 10_000_000, 1);

    let txs = vec![
        (Hash256([0x01; 32]), payment(alice, bob, 15_000_000, 12, 1)),
        (Hash256([0x02; 32]), payment(bob, carol, 10_000_000, 15, 1)),
        (Hash256([0x03; 32]), payment(carol, alice, 5_000_000, 10, 1)),
    ];

    let (new_state, results) = apply_transaction_set(
        &state, txs, 700_000_010, 10,
    ).unwrap();

    assert!(results.iter().all(|r| r.result.is_success()));

    let total_fees: u64 = results.iter().map(|r| r.fee).sum();
    let alice_bal = read_balance(&new_state, &alice);
    let bob_bal = read_balance(&new_state, &bob);
    let carol_bal = read_balance(&new_state, &carol);

    // Conservation: all account balances + fees destroyed = original balances
    let original_balances = 60_000_000u64 + 30_000_000 + 10_000_000;
    let new_balances = alice_bal + bob_bal + carol_bal;
    assert_eq!(new_balances + total_fees, original_balances,
        "XRP conservation violated! {} + {} != {}", new_balances, total_fees, original_balances);

    // total_coins should reflect fee destruction
    assert_eq!(new_state.header.total_coins, initial_coins - total_fees);
}

// === Future sequence rejected ===

#[test]
fn future_sequence_rejected() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 5); // seq=5
    add_account(&mut state, &bob, 10_000_000, 1);

    let tx = payment(alice, bob, 1_000_000, 12, 50); // seq=50, way ahead
    let (_, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::BadSequence);
}

// === Past sequence rejected ===

#[test]
fn past_sequence_rejected() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 10); // seq=10
    add_account(&mut state, &bob, 10_000_000, 1);

    let tx = payment(alice, bob, 1_000_000, 12, 5); // seq=5, behind
    let (_, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::PastSeq);
}

// === Zero amount rejected ===

#[test]
fn zero_amount_rejected() {
    let alice = [0x01u8; 20];
    let bob = [0x02u8; 20];
    let mut state = make_state(100, 100_000_000_000_000_000);
    add_account(&mut state, &alice, 50_000_000, 1);
    add_account(&mut state, &bob, 10_000_000, 1);

    let tx = payment(alice, bob, 0, 12, 1);
    let (_, results) = apply_transaction_set(
        &state, vec![(Hash256([0x01; 32]), tx)], 700_000_010, 10,
    ).unwrap();

    assert_eq!(results[0].result, TxResult::BadAmount);
}
