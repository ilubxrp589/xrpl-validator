//! Byte-exact vector drills for AMMClawback and Clawback (testnet campaign 9, 2026-09-21).
//!
//! Each test replays one mainnet transaction against its same-ledger
//! pre-images and compares every touched object byte-for-byte with the
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


/// Finding 352 (AMMClawback port) / Finding 351 — Amount 20 CLW of a two-LP pool: tokensAdj from the UPWARD product, both sides rounded DOWN, XRP side to the holder.
#[test]
fn amm_clawback_partial_by_amount_testnet_20936598() {
    run_bundle(include_str!("vectors/amm_clawback_partial_by_amount_testnet_20936598.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — Amount 9999 CLW: lpt×frac > holding → the whole position, divide(hold, lpt).
#[test]
fn amm_clawback_amount_past_the_holding_is_withdraw_all_testnet_20936600() {
    run_bundle(include_str!("vectors/amm_clawback_amount_past_the_holding_is_withdraw_all_testnet_20936600.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — Amount 1e-12 CLW: pre-fixCleanup3_4_0 a zero side is still a withdraw (tesSUCCESS).
#[test]
fn amm_clawback_dust_amount_pre_fixcleanup340_testnet_20936602() {
    run_bundle(include_str!("vectors/amm_clawback_dust_amount_pre_fixcleanup340_testnet_20936602.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — no Amount: the holder's remaining position.
#[test]
fn amm_clawback_no_amount_is_withdraw_all_testnet_20936604() {
    run_bundle(include_str!("vectors/amm_clawback_no_amount_is_withdraw_all_testnet_20936604.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — the holder has no LP tokens left: tecAMM_BALANCE.
#[test]
fn amm_clawback_holder_without_lp_is_amm_balance_testnet() {
    run_bundle(include_str!("vectors/amm_clawback_holder_without_lp_is_amm_balance_testnet.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — CLW/CLX pool with tfClawTwoAssets: both sides holder → issuer.
#[test]
fn amm_clawback_two_asset_pool_tfclawtwoassets_testnet_20936609() {
    run_bundle(include_str!("vectors/amm_clawback_two_asset_pool_tfclawtwoassets_testnet_20936609.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — CLW/CLX pool, no flag: CLX stays with the holder.
#[test]
fn amm_clawback_two_asset_pool_without_the_flag_testnet_20936611() {
    run_bundle(include_str!("vectors/amm_clawback_two_asset_pool_without_the_flag_testnet_20936611.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — Clawback of the entire CLX balance: line survives at zero.
#[test]
fn clawback_of_the_whole_balance_leaves_the_line_testnet_20936574() {
    run_bundle(include_str!("vectors/clawback_of_the_whole_balance_leaves_the_line_testnet_20936574.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — Clawback from a zero line: tecINSUFFICIENT_FUNDS (Finding 351).
#[test]
fn clawback_with_nothing_held_is_insufficient_funds_testnet() {
    run_bundle(include_str!("vectors/clawback_with_nothing_held_is_insufficient_funds_testnet.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — Clawback 999999 of ~1000: clamps to the balance.
#[test]
fn clawback_more_than_held_clamps_to_the_balance_testnet_20936578() {
    run_bundle(include_str!("vectors/clawback_more_than_held_clamps_to_the_balance_testnet_20936578.json"));
}

/// Finding 352 (AMMClawback port) / Finding 351 — Clawback of a holder with no line: tecNO_LINE (Finding 351).
#[test]
fn clawback_from_a_holder_with_no_line_is_no_line_testnet() {
    run_bundle(include_str!("vectors/clawback_from_a_holder_with_no_line_is_no_line_testnet.json"));
}
