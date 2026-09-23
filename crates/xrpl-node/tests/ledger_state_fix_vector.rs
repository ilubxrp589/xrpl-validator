//! LedgerStateFix byte-exact vectors (bundle-driven), Finding 359.
//!
//! Campaign 21 (testnet, 2026-09-23): every path a healthy ledger can reach,
//! both fix types. The live engine answered tecUNSUPPORTED on all eight (the
//! type was never dispatched). Same harness as did_vector.rs.
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


/// Finding 359 — 1-1 NfTokenPageLink on f2 (3 NFTs, one healthy page) (6956E29EF07C).
#[test]
fn ledger_state_fix_nft_links_healthy_page_is_failed_processing_testnet_20976315() {
    run_bundle(include_str!("vectors/ledger_state_fix_nft_links_healthy_page_is_failed_processing_testnet_20976315.json"));
}

/// Finding 359 — 1-2 NfTokenPageLink on f3 (no NFT pages at all) (6E92326DD1BD).
#[test]
fn ledger_state_fix_nft_links_owner_without_pages_is_failed_processing_testnet_20976319() {
    run_bundle(include_str!("vectors/ledger_state_fix_nft_links_owner_without_pages_is_failed_processing_testnet_20976319.json"));
}

/// Finding 359 — 1-3 NfTokenPageLink on an account that does not exist (9D7C37059B0B).
#[test]
fn ledger_state_fix_nft_links_missing_owner_is_object_not_found_testnet_20976323() {
    run_bundle(include_str!("vectors/ledger_state_fix_nft_links_missing_owner_is_object_not_found_testnet_20976323.json"));
}

/// Finding 359 — 1-4 NfTokenPageLink on itself (f1, no NFTs) (53DED894C8B9).
#[test]
fn ledger_state_fix_nft_links_on_the_sender_itself_is_failed_processing_testnet_20976325() {
    run_bundle(include_str!("vectors/ledger_state_fix_nft_links_on_the_sender_itself_is_failed_processing_testnet_20976325.json"));
}

/// Finding 359 — 2-1 BookExchangeRate on a correct book root (87BAD487FA06).
#[test]
fn ledger_state_fix_book_rate_correct_root_is_no_permission_testnet_20976329() {
    run_bundle(include_str!("vectors/ledger_state_fix_book_rate_correct_root_is_no_permission_testnet_20976329.json"));
}

/// Finding 359 — 2-2 BookExchangeRate on an owner directory (no ExchangeRate) (2D9692E6CE0A).
#[test]
fn ledger_state_fix_book_rate_owner_directory_is_no_permission_testnet_20976332() {
    run_bundle(include_str!("vectors/ledger_state_fix_book_rate_owner_directory_is_no_permission_testnet_20976332.json"));
}

/// Finding 359 — 2-3 BookExchangeRate on an AccountRoot key (D78FC0CD4AA6).
#[test]
fn ledger_state_fix_book_rate_account_root_key_is_object_not_found_testnet_20976334() {
    run_bundle(include_str!("vectors/ledger_state_fix_book_rate_account_root_key_is_object_not_found_testnet_20976334.json"));
}

/// Finding 359 — 2-4 BookExchangeRate on an empty key (A8AD3E918D78).
#[test]
fn ledger_state_fix_book_rate_empty_key_is_object_not_found_testnet_20976336() {
    run_bundle(include_str!("vectors/ledger_state_fix_book_rate_empty_key_is_object_not_found_testnet_20976336.json"));
}
