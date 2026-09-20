//! Port-only byte-exact vectors (2026-09-20, testnet campaign 6).
//!
//! These bundles are what the ported flow engine (`flow/`, Track 2) gets
//! right and the modelled engine (`tx::payment` / `tx::offer`) gets wrong.
//! Every test selects the port before applying; the model's misses are
//! recorded as findings in the harness notes, not fixed — the model is the
//! fallback now, the port is the engine.
//! ledger, honouring the recorded TransactionResult.
use serde_json::Value;
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

fn run_bundle(bundle_json: &str) {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
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

    let tx = &bundle["tx"];
    let tx_hash = tx["hash"].as_str().unwrap();
    let txf = build_txfields(tx).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet recorded this transaction result");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        let want_deleted = want_hex.as_str().unwrap().trim().is_empty();
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


fn port() {
    std::env::set_var("XRPL_FLOW_ENGINE", "port");
}

/// #20910362 17C85005: BST (8% transfer fee) → USD, tfPartialPayment with a
/// DeliverMin, one explicit path. The model leaves one maker line untouched
/// that mainnet modifies (a strand it never walked).
#[test]
fn payment_partial_delivermin_bst_to_usd_over_a_fee_issuer_testnet_20910362() {
    port();
    run_bundle(include_str!("vectors/payment_partial_delivermin_bst_to_usd_over_a_fee_issuer_testnet_20910362.json"));
}

/// #20910374 A7B61CD3: EUR → USD with tfLimitQuality across ladders on both
/// sides and the direct book. The model misses one modified node.
#[test]
fn payment_limit_quality_eur_to_usd_testnet_20910374() {
    port();
    run_bundle(include_str!("vectors/payment_limit_quality_eur_to_usd_testnet_20910374.json"));
}

/// #20910392 F9323063: EUR → XRP with tfLimitQuality. The model misses one
/// modified node.
#[test]
fn payment_limit_quality_eur_to_xrp_testnet_20910392() {
    port();
    run_bundle(include_str!("vectors/payment_limit_quality_eur_to_xrp_testnet_20910392.json"));
}

/// #20910241 58A1750A: EUR → USD partial with DeliverMin through an explicit
/// path. The model misses one modified node.
#[test]
fn payment_partial_delivermin_eur_to_usd_through_paths_testnet_20910241() {
    port();
    run_bundle(include_str!("vectors/payment_partial_delivermin_eur_to_usd_through_paths_testnet_20910241.json"));
}

/// #20910434 4EA250F1: BST (8% fee) → USD, tfPartialPayment, one path, no
/// DeliverMin. Model misses a modified node.
#[test]
fn payment_partial_bst_to_usd_over_a_fee_issuer_testnet_20910434() {
    port();
    run_bundle(include_str!("vectors/payment_partial_bst_to_usd_over_a_fee_issuer_testnet_20910434.json"));
}

/// #20910299 9949C9FA: EUR → USD, no flags, no explicit paths — the default
/// path only, with the direct book and the XRP bridge competing. Model
/// misses a modified node.
#[test]
fn payment_eur_to_usd_default_paths_testnet_20910299() {
    port();
    run_bundle(include_str!("vectors/payment_eur_to_usd_default_paths_testnet_20910299.json"));
}
