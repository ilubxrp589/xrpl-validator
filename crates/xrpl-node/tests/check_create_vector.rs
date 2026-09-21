//! CheckCreate byte-exact vectors (bundle-driven).
//!
//! The driver is rippled's `flow()` loop (StrandFlow.h:606-790): activate the
//! best strand, flow it, repeat while both remainders are positive. Its only
//! bounds are safety bounds — maxTries = 1000 iterations (the 1000th entry is
//! telFAILED_PROCESSING) and 1500 offers stepped. Whatever the loop is capped
//! at here is how many fills-or-AMM-slices a lone strand may take, and a cap
//! under mainnet's count turns a tesSUCCESS into a DeliverMin shortfall.
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
    assert_eq!(ter, want_ter, "mainnet result for this transaction");

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

/// F59's regression guard (#106693003 E9919AA2, the path-TER flagship): a
/// tfPartialPayment self-payment buying BCHAMP with 1485.4 XRP of SendMax,
/// DeliverMin 737956.89. Mainnet fills it through 33 book offers interleaved
/// with 13 AMM slices — 46 driver iterations — spending the whole SendMax
/// and delivering 763166.81. The driver here was capped at 32 rounds (the
/// old multi-strand interleave cap, kept for lone strands in 4566c4e): round
/// 32 left 497 XRP unspent and 510958.66 delivered, under DeliverMin —
/// tecPATH_PARTIAL against mainnet's tesSUCCESS, the live shadow's
/// ter-mismatch signature. rippled's loop runs on its remainders alone
/// (maxTries = 1000 is a failure bound, not a fill count).
/// #106708057 CEDC31F6 (finding 92): a CheckCreate whose XAU SendMax is
/// frozen — CheckCreate::preclaim refuses a globally frozen currency, a
/// source line frozen by the issuer, or a destination line the destination
/// froze, with tecFROZEN. We had no freeze block at all and created the
/// Check plus its two directory entries where mainnet claims the fee alone.
#[test]
fn check_create_refuses_a_frozen_send_max() {
    run_bundle(include_str!("vectors/check_create_frozen_sendmax_106708057.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 2-9: t3 cancels an unexpired check.
/// Testnet's verdict, byte-exact.
#[test]
fn check_c14_cancel_by_a_stranger_is_tec_no_permission_fd28df79ec52() {
    run_bundle(include_str!("vectors/check_c14_cancel_by_a_stranger_is_tec_no_permission_FD28DF79EC52.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 2-10: t2 cancels.
/// Testnet's verdict, byte-exact.
#[test]
fn check_c14_cancel_by_the_destination_e6ee3c370fc9() {
    run_bundle(include_str!("vectors/check_c14_cancel_by_the_destination_E6EE3C370FC9.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 2-13: t3 cancels after Expiration.
/// Testnet's verdict, byte-exact.
#[test]
fn check_c14_stranger_cancels_an_expired_check_09ba231ab8bf() {
    run_bundle(include_str!("vectors/check_c14_stranger_cancels_an_expired_check_09BA231AB8BF.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 2-15: t1 cancels.
/// Testnet's verdict, byte-exact.
#[test]
fn check_c14_cancel_by_the_owner_038327f5e91c() {
    run_bundle(include_str!("vectors/check_c14_cancel_by_the_owner_038327F5E91C.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 2-27: t1 cancels a check already cashed.
/// Testnet's verdict, byte-exact.
#[test]
fn check_c14_cancel_of_a_cashed_check_is_tec_no_entry_591b1fa15c10() {
    run_bundle(include_str!("vectors/check_c14_cancel_of_a_cashed_check_is_tec_no_entry_591B1FA15C10.json"));
}

/// Campaign 14 (testnet PayChan / Check / DepositAuth depth, 2026-09-21) 2-18: t1 writes t2 a check without a tag.
/// Testnet's verdict, byte-exact.
#[test]
fn check_c14_create_to_a_require_dest_account_is_tec_dst_tag_needed_1d431861a64c() {
    run_bundle(include_str!("vectors/check_c14_create_to_a_require_dest_account_is_tec_dst_tag_needed_1D431861A64C.json"));
}
