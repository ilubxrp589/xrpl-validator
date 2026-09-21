//! Byte-exact vector drills for Batch (BatchV1_1, 2026-09-17).
//!
//! Each vector replays one devnet Batch OUTER against the union of its and
//! its inners' pre-images and compares every touched object byte-for-byte
//! with the ledger; the inners are applied inside the outer by
//! `BatchTransactor::do_apply`, exactly as rippled's
//! `applyBatchTransactions` does. `inner_results` pins each inner's recorded
//! TransactionResult.
use serde_json::Value;
use std::collections::HashMap;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

fn key32(hex_key: &str) -> Hash256 {
    Hash256(<[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap())
}

fn hydrate(state: &mut LedgerState, key_hex: &str, entry_hex: &str) {
    let bytes = hex::decode(entry_hex.trim()).unwrap();
    let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).unwrap();
    hexify_addresses(&mut jv);
    state
        .state_map
        .insert(key32(key_hex), serde_json::to_vec(&jv).unwrap())
        .unwrap();
}

/// Hydrate the pre-state, apply the bundle's transaction, and pin its TER.
/// Returns the state as of BEFORE the apply (the threading stamps need it to
/// tell a real change from a write-back) together with the raw, unstamped
/// mutation map.
fn apply_bundle(bundle: &Value) -> (LedgerState, String, HashMap<Hash256, SandboxEntry>) {
    let seq = bundle["seq"].as_u64().unwrap() as u32;
    let pct = bundle["parent_close_time"].as_u64().unwrap() as u32;
    let header = LedgerHeader {
        sequence: seq - 1,
        total_coins: bundle["total_coins"].as_u64().unwrap(),
        parent_hash: key32(bundle["parent_hash"].as_str().unwrap()),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: pct,
        close_time: pct,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    for (k, v) in bundle["pre"].as_object().unwrap() {
        hydrate(&mut state, k, v.as_str().unwrap());
    }

    let txf = build_txfields(&bundle["tx"]).expect("txfields");
    let (ter, mods) = native_apply_one(&state, &txf);
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet recorded this transaction result");
    (state, ter, mods)
}

/// Every `expect` entry byte-for-byte against the stamped mutation map.
fn assert_expect(bundle: &Value, mods: &HashMap<Hash256, SandboxEntry>) {
    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        // An EMPTY expectation is a deletion pin (finding 158): mainnet's
        // meta deleted the object in this transaction, so must the apply.
        let want_deleted = want_hex.as_str().unwrap().trim().is_empty();
        // An expectation the apply did not write is legitimate only when it
        // pins an object the transaction must leave ALONE: its post-image is
        // then its pre-image, and the vector says so by expecting exactly
        // the seated bytes (finding 143 — the taker's own bid beyond the
        // ask's limit, which mainnet never names).
        let Some(ent) = mods.get(&key32(k)) else {
            assert!(!want_deleted, "target {k} must be deleted by the apply, which never wrote it");
            let pre_hex = bundle["pre"][k].as_str().unwrap_or_default().trim().to_uppercase();
            assert_eq!(
                want_hex.as_str().unwrap().trim().to_uppercase(),
                pre_hex,
                "target {k} was not written by the apply and does not pin the untouched pre-image"
            );
            continue;
        };
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                assert!(!want_deleted, "target {k} must be deleted by the apply, which wrote it instead");
                b.clone()
            }
            SandboxEntry::Deleted => {
                assert!(want_deleted, "target {k} deleted?");
                continue;
            }
        };
        let mut jv: Value = serde_json::from_slice(&bytes).unwrap();
        canon_for_encode(&mut jv);
        let enc = xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap();
        let want = hex::decode(want_hex.as_str().unwrap().trim()).unwrap();
        assert_eq!(
            hex::encode_upper(&enc),
            hex::encode_upper(&want),
            "target {k} must byte-match the mainnet post-state"
        );
    }
}

// The non-batch path of the shared harness shape; kept so a non-Batch bundle
// can join this file without re-copying the harness.
#[allow(dead_code)]
fn run_bundle(bundle_json: &str) {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
    let (state, _ter, mut mods) = apply_bundle(&bundle);
    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        bundle["tx"]["hash"].as_str().unwrap(),
        bundle["seq"].as_u64().unwrap() as u32,
    );
    assert_expect(&bundle, &mods);
}

/// A Batch outer: the inners run inside the outer's `do_apply`, but rippled
/// applies each of them as its own transaction, so every object an inner
/// touched threads the INNER's hash (the outer's own fee/sequence changes
/// keep the outer's). The ledger's `inner_hashes` pair, in RawTransactions
/// order, with the engine's per-inner touched-key sets.
fn run_batch_bundle(bundle_json: &str) {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
    let want_inner: Vec<String> = bundle["inner_results"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default();
    let inner_hashes: Vec<String> = bundle["inner_hashes"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default();

    let (state, _ter, mut mods) = apply_bundle(&bundle);
    let ours = xrpl_ledger::tx::batch::take_inner_results();
    let inner_touched = xrpl_ledger::tx::batch::take_inner_touched();

    xrpl_ledger::ledger::threading::stamp_batch_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        bundle["tx"]["hash"].as_str().unwrap(),
        bundle["seq"].as_u64().unwrap() as u32,
        &inner_hashes,
        &inner_touched,
    );

    // The inner results first: a wrong one is a TER receipt naming the inner,
    // a far more useful failure than the byte-diff it would also cause.
    assert_eq!(ours, want_inner, "each inner's TransactionResult, in RawTransactions order");
    assert_expect(&bundle, &mods);
}

#[test]
fn batch_until_failure_two_payments_devnet_5309670() {
    run_batch_bundle(include_str!("vectors/batch_until_failure_two_payments_devnet_5309670.json"));
}

#[test]
fn batch_until_failure_two_payments_devnet_5309584() {
    run_batch_bundle(include_str!("vectors/batch_until_failure_two_payments_devnet_5309584.json"));
}


/// Campaign 8 (devnet, 2026-09-21): tfOnlyOne: inner 1 tesSUCCESS then stop; inners 2 and 3 never filed.
#[test]
fn batch_onlyone_fails_in_the_middle_devnet_5489437() {
    run_batch_bundle(include_str!("vectors/batch_onlyone_fails_in_the_middle_devnet_5489437.json"));
}

/// Campaign 8 (devnet, 2026-09-21): tfUntilFailure: inner 1 tes, inner 2 tecUNFUNDED_PAYMENT filed, inner 3 not attempted.
#[test]
fn batch_untilfailure_fails_in_the_middle_devnet_5489449() {
    run_batch_bundle(include_str!("vectors/batch_untilfailure_fails_in_the_middle_devnet_5489449.json"));
}

/// Campaign 8 (devnet, 2026-09-21): tfIndependent: inner 2 tecUNFUNDED_PAYMENT, inners 1 and 3 tes.
#[test]
fn batch_independent_fails_in_the_middle_devnet_5489460() {
    run_batch_bundle(include_str!("vectors/batch_independent_fails_in_the_middle_devnet_5489460.json"));
}

/// Campaign 8 (devnet, 2026-09-21): two accounts, BatchSigners: both inners tes.
#[test]
fn batch_multi_account_allornothing_with_batchsigners_devnet_5489526() {
    run_batch_bundle(include_str!("vectors/batch_multi_account_allornothing_with_batchsigners_devnet_5489526.json"));
}

/// Campaign 8 (devnet, 2026-09-21): OfferCreate, OfferCancel of that inner, OfferCancel of an old offer.
#[test]
fn batch_untilfailure_offer_create_then_cancel_devnet_5489473() {
    run_batch_bundle(include_str!("vectors/batch_untilfailure_offer_create_then_cancel_devnet_5489473.json"));
}

/// Campaign 8 (devnet, 2026-09-21): tfAllOrNothing: three payments, all tes.
#[test]
fn batch_allornothing_three_payments_devnet_5489422() {
    run_batch_bundle(include_str!("vectors/batch_allornothing_three_payments_devnet_5489422.json"));
}

/// Campaign 8 (devnet, 2026-09-21): tfAllOrNothing: inner 1 tecUNFUNDED_PAYMENT → no inner filed, outer tes with its fee alone.
#[test]
fn batch_allornothing_first_inner_fails_everything_reverts_devnet_5489428() {
    run_batch_bundle(include_str!("vectors/batch_allornothing_first_inner_fails_everything_reverts_devnet_5489428.json"));
}
