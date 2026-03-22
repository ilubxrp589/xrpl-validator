//! Ledger close — apply a set of transactions to produce a new ledger.
//!
//! Takes the current LedgerState and a list of transactions,
//! applies them in canonical order, and returns the new LedgerState.

use xrpl_core::types::Hash256;

use super::header::LedgerHeader;
use super::sandbox::{apply_modifications, Sandbox};
use super::state::LedgerState;
use super::transactor::{apply_common, TxFields, TxResult};
use crate::tx::dispatch::get_transactor;
use crate::LedgerError;

/// Result of applying a single transaction.
#[derive(Debug)]
pub struct AppliedTx {
    pub tx_hash: Hash256,
    pub result: TxResult,
    pub fee: u64,
}

/// Apply a transaction set to produce a new ledger state.
///
/// Steps:
/// 1. Sort transactions in canonical order (handled by caller or here)
/// 2. For each transaction: preflight → preclaim → apply_common → do_apply
/// 3. If success: commit sandbox changes
/// 4. If failure (tec): still deduct fee, discard other changes
/// 5. If failure (tem/tef): discard entirely
/// 6. Sum all fees → subtract from total_coins
/// 7. Build new LedgerHeader
pub fn apply_transaction_set(
    current: &LedgerState,
    transactions: Vec<(Hash256, TxFields)>,
    close_time: u32,
    close_time_resolution: u8,
) -> Result<(LedgerState, Vec<AppliedTx>), LedgerError> {
    // Clone the state so we can mutate it
    let mut new_state = current.clone();
    let mut results = Vec::with_capacity(transactions.len());
    let mut total_fees: u64 = 0;

    for (tx_hash, tx) in &transactions {
        // Dispatch to the correct transactor
        let transactor = match get_transactor(&tx.tx_type) {
            Some(t) => t,
            None => {
                // Unsupported tx type — deduct fee but skip apply
                let fee_result = {
                    let mut sandbox = Sandbox::new(&new_state);
                    let r = apply_common(tx, &mut sandbox);
                    if r.is_success() {
                        let mods = sandbox.into_modifications();
                        apply_modifications(&mut new_state, mods)?;
                        total_fees += tx.fee;
                    } else {
                        sandbox.discard();
                    }
                    r
                };
                results.push(AppliedTx {
                    tx_hash: *tx_hash,
                    result: if fee_result.is_success() {
                        TxResult::Unsupported
                    } else {
                        fee_result
                    },
                    fee: if fee_result.is_success() { tx.fee } else { 0 },
                });
                continue;
            }
        };

        // Phase 1: Preflight (no state)
        let preflight = transactor.preflight(tx);
        if !preflight.is_success() {
            if preflight.is_claimed() {
                // Claimed preflight failure (e.g. Unsupported IOU) — deduct fee
                let mut sandbox = Sandbox::new(&new_state);
                let common = apply_common(tx, &mut sandbox);
                if common.is_success() {
                    let mods = sandbox.into_modifications();
                    apply_modifications(&mut new_state, mods)?;
                    total_fees += tx.fee;
                    results.push(AppliedTx {
                        tx_hash: *tx_hash,
                        result: preflight,
                        fee: tx.fee,
                    });
                } else {
                    sandbox.discard();
                    results.push(AppliedTx {
                        tx_hash: *tx_hash,
                        result: common,
                        fee: 0,
                    });
                }
            } else {
                results.push(AppliedTx {
                    tx_hash: *tx_hash,
                    result: preflight,
                    fee: 0,
                });
            }
            continue;
        }

        // Phase 2+3+4: Preclaim + Common + Apply in sandbox
        let (result, fee_charged) = {
            let mut sandbox = Sandbox::new(&new_state);

            // Preclaim (read-only check)
            let preclaim = transactor.preclaim(tx, &sandbox);
            if !preclaim.is_success() && !preclaim.is_claimed() {
                // tem/tef — not claimed, discard entirely
                sandbox.discard();
                (preclaim, 0u64)
            } else if !preclaim.is_success() {
                // tec — claimed, deduct fee only
                let common = apply_common(tx, &mut sandbox);
                if common.is_success() {
                    let mods = sandbox.into_modifications();
                    apply_modifications(&mut new_state, mods)?;
                    (preclaim, tx.fee)
                } else {
                    sandbox.discard();
                    (common, 0)
                }
            } else {
                // Preclaim passed — apply common then do_apply
                let common = apply_common(tx, &mut sandbox);
                if !common.is_success() {
                    sandbox.discard();
                    (common, 0)
                } else {
                    // Snapshot after apply_common so we can rollback do_apply
                    // changes on tec results (only fee + sequence should persist).
                    let common_snapshot = sandbox.snapshot();

                    let apply_result = transactor.do_apply(tx, &mut sandbox);
                    if apply_result.is_success() {
                        let mods = sandbox.into_modifications();
                        apply_modifications(&mut new_state, mods)?;
                        (TxResult::Success, tx.fee)
                    } else if apply_result.is_claimed() {
                        // tec from do_apply — rollback to just apply_common
                        // modifications (fee deduction + sequence increment).
                        // do_apply's partial state changes are discarded.
                        sandbox.restore_snapshot(common_snapshot);
                        let mods = sandbox.into_modifications();
                        apply_modifications(&mut new_state, mods)?;
                        (apply_result, tx.fee)
                    } else {
                        sandbox.discard();
                        (apply_result, 0)
                    }
                }
            }
        };

        total_fees += fee_charged;
        results.push(AppliedTx {
            tx_hash: *tx_hash,
            result,
            fee: fee_charged,
        });
    }

    // Destroy fees: total_coins -= sum(all_fees)
    let new_total_coins = new_state
        .header
        .total_coins
        .checked_sub(total_fees)
        .ok_or(LedgerError::Invariant(
            "total_coins underflow: fees exceed total coin supply".into(),
        ))?;

    // Build new header
    let new_header = LedgerHeader {
        sequence: current.header.sequence + 1,
        total_coins: new_total_coins,
        parent_hash: current.ledger_hash(),
        account_hash: new_state.state_map.root_hash(),
        transaction_hash: new_state.tx_map.root_hash(),
        parent_close_time: current.header.close_time,
        close_time,
        close_time_resolution,
        close_flags: 0,
    };

    new_state.header = new_header;

    Ok((new_state, results))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::keylet;

    fn make_state() -> LedgerState {
        let header = LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000, // 100B XRP
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
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
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
    fn apply_single_payment() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &alice, 50_000_000, 1);
        add_account(&mut state, &bob, 10_000_000, 1);

        let tx_hash = Hash256([0xAA; 32]);
        let tx = payment_tx(alice, bob, 5_000_000, 12, 1);

        let (new_state, results) = apply_transaction_set(
            &state, vec![(tx_hash, tx)], 700_000_010, 10,
        ).unwrap();

        // Check result
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].result, TxResult::Success);
        assert_eq!(results[0].fee, 12);

        // Check new header
        assert_eq!(new_state.header.sequence, 101);
        assert_eq!(new_state.header.total_coins, 100_000_000_000_000_000 - 12);
        assert_eq!(new_state.header.parent_close_time, 700_000_000);
        assert_eq!(new_state.header.close_time, 700_000_010);

        // Check balances
        let alice_key = keylet::account_root_key(&alice);
        let alice_data = new_state.state_map.lookup(&alice_key).unwrap();
        let alice_obj: serde_json::Value = serde_json::from_slice(alice_data).unwrap();
        // 50M - 12(fee) - 5M(amount) = 44,999,988
        assert_eq!(alice_obj["Balance"].as_str().unwrap(), "44999988");

        let bob_key = keylet::account_root_key(&bob);
        let bob_data = new_state.state_map.lookup(&bob_key).unwrap();
        let bob_obj: serde_json::Value = serde_json::from_slice(bob_data).unwrap();
        // 10M + 5M = 15M
        assert_eq!(bob_obj["Balance"].as_str().unwrap(), "15000000");

        // Root hashes should differ from the original
        assert_ne!(new_state.state_map.root_hash(), state.state_map.root_hash());
    }

    #[test]
    fn apply_multiple_payments() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let carol = [0x03u8; 20];
        let mut state = make_state();
        add_account(&mut state, &alice, 100_000_000, 1);
        add_account(&mut state, &bob, 50_000_000, 1);
        add_account(&mut state, &carol, 20_000_000, 1);

        let txs = vec![
            (Hash256([0x01; 32]), payment_tx(alice, bob, 10_000_000, 12, 1)),
            (Hash256([0x02; 32]), payment_tx(bob, carol, 5_000_000, 10, 1)),
        ];

        let (new_state, results) = apply_transaction_set(
            &state, txs, 700_000_010, 10,
        ).unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.result == TxResult::Success));

        // Total fees = 12 + 10 = 22
        assert_eq!(new_state.header.total_coins, 100_000_000_000_000_000 - 22);

        // Alice: 100M - 12 - 10M = 89,999,988
        let ak = keylet::account_root_key(&alice);
        let av: serde_json::Value = serde_json::from_slice(
            new_state.state_map.lookup(&ak).unwrap()
        ).unwrap();
        assert_eq!(av["Balance"].as_str().unwrap(), "89999988");

        // Bob: 50M + 10M - 10 - 5M = 54,999,990
        let bk = keylet::account_root_key(&bob);
        let bv: serde_json::Value = serde_json::from_slice(
            new_state.state_map.lookup(&bk).unwrap()
        ).unwrap();
        assert_eq!(bv["Balance"].as_str().unwrap(), "54999990");

        // Carol: 20M + 5M = 25M
        let ck = keylet::account_root_key(&carol);
        let cv: serde_json::Value = serde_json::from_slice(
            new_state.state_map.lookup(&ck).unwrap()
        ).unwrap();
        assert_eq!(cv["Balance"].as_str().unwrap(), "25000000");
    }

    #[test]
    fn unsupported_tx_type_deducts_fee() {
        let alice = [0x01u8; 20];
        let mut state = make_state();
        add_account(&mut state, &alice, 50_000_000, 1);

        let tx = TxFields {
            account: alice,
            tx_type: "EscrowCreate".to_string(), // not implemented yet
            fee: 15,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({}),
        };

        let (new_state, results) = apply_transaction_set(
            &state, vec![(Hash256([0xFF; 32]), tx)], 700_000_010, 10,
        ).unwrap();

        assert_eq!(results[0].result, TxResult::Unsupported);
        assert_eq!(results[0].fee, 15);
        // Fee should still be deducted
        assert_eq!(new_state.header.total_coins, 100_000_000_000_000_000 - 15);
    }

    #[test]
    fn new_ledger_has_correct_sequence() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &alice, 50_000_000, 1);
        add_account(&mut state, &bob, 50_000_000, 1);

        let (new_state, _) = apply_transaction_set(
            &state,
            vec![(Hash256([0x01; 32]), payment_tx(alice, bob, 1_000_000, 12, 1))],
            700_000_010, 10,
        ).unwrap();

        assert_eq!(new_state.header.sequence, 101);
        assert_eq!(new_state.header.parent_hash, state.ledger_hash());
    }
}
