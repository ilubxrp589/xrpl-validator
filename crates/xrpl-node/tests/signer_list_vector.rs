//! Byte-exact vector drills for SignerListSet (2026-09-01).
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
        let ent = mods
            .get(&key32(k))
            .unwrap_or_else(|| panic!("target {k} must be written by the apply"));
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
            SandboxEntry::Deleted => panic!("target {k} deleted?"),
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

/// Finding 73 — #106699661 55F46ED0: a SignerListSet creating a one-entry
/// list. Under fixIncludeKeyletFields rippled files `Owner` on the SignerList
/// (SetSignerList.cpp:428-431); ours lacked the 22-byte field.
#[test]
fn signer_list_set_files_the_owner_keylet_field() {
    run_bundle(include_str!("vectors/signerlist_owner_106699661.json"));
}

/// Finding 83 — #106703005 F62BBA1C: a SignerListSet whose entries arrive
/// out of account order. rippled sorts them before writing the list
/// (SetSignerList.cpp:66); we filed them as sent.
#[test]
fn signer_list_set_files_entries_in_account_order() {
    run_bundle(include_str!("vectors/signerlist_sorted_entries_106703005.json"));
}

/// Finding 136 — #106739411 780DFE9BF728 (rDPdmrS5, 3-signer replace).
/// rippled's `replaceSignerList` (SignerListSet.cpp:299-355) removes the old
/// list from the owner directory and re-inserts the fresh one, so the
/// SignerList key moves to the END of the directory page; we modified the list
/// in place and never touched the page ("missing" DirectoryNode).
#[test]
fn signer_list_replace_reinserts_the_owner_directory_entry() {
    run_bundle(include_str!("vectors/signer_list_replace_reinserts_owner_dir_entry_106739411.json"));
}
