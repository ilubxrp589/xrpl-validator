//! Byte-exact vector drills for the pseudo-transactions — UNLModify, EnableAmendment (2026-09-20).
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


/// Like the other vector drills, but a flag ledger first rotates the
/// NegativeUNL object (ValidatorToDisable / ValidatorToReEnable become
/// DisabledValidators) BEFORE the ledger's transactions apply — rippled's
/// `Ledger::updateNegativeUNL`, our `tx::pseudo::rotate_negative_unl`, and
/// the shadow's ledger-level step in native_shadow.rs. The bundle's
/// pre-images are the parent's, so the rotation is applied here.
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
    // UNLModify lands IN the flag ledger (rotation first); EnableAmendment and
    // SetFee land in the ledger AFTER it (no rotation).
    let nk = xrpl_ledger::ledger::keylet::negative_unl_key();
    if let (0, Some(bytes)) = (seq % 256, state.state_map.lookup(&nk).map(|b| b.to_vec())) {
        match xrpl_ledger::tx::pseudo::rotate_negative_unl(&bytes, seq) {
            Some(Some(nb)) => {
                state.state_map.insert(nk, nb).unwrap();
            }
            Some(None) => {
                state.state_map.delete(&nk).unwrap();
            }
            None => {}
        }
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
            continue;
        };
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
            SandboxEntry::Deleted => {
                assert!(want_deleted, "target {k} deleted?");
                continue;
            }
        };
        let mut jv: Value = serde_json::from_slice(&bytes).unwrap();
        canon_for_encode(&mut jv);
        let enc = xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap();
        let want = hex::decode(want_hex.as_str().unwrap().trim()).unwrap();
        assert_eq!(hex::encode_upper(&enc), hex::encode_upper(&want), "target {k} must byte-match the mainnet post-state");
    }
}

/// Coverage pin (2026-09-20 inventory): the pseudo-transactions had no
/// vector. #107068416 923E027D: the UNL marks a validator to disable —
/// ValidatorToDisable set on the NegativeUNL singleton.
#[test]
fn unlmodify_disable_a_validator_107068416() {
    run_bundle(include_str!("vectors/unlmodify_disable_a_validator_107068416.json"));
}

/// #107068672 524654E6: the next flag ledger. The rotation moves that
/// validator into DisabledValidators first; the UNLModify then re-enables
/// it (ValidatorToReEnable). Without the rotation the apply is tefFAILURE
/// because the validator is not yet in DisabledValidators.
#[test]
fn unlmodify_reenable_after_the_flag_rotation_107068672() {
    run_bundle(include_str!("vectors/unlmodify_reenable_after_the_flag_rotation_107068672.json"));
}

/// #106911489 5749CFD2: the ledger after the flag ledger that carried
/// fixCleanup3_3_0 over the threshold — EnableAmendment moves the amendment
/// out of Majorities and into the Amendments singleton's enabled list.
#[test]
fn enable_amendment_fixcleanup330_106911489() {
    run_bundle(include_str!("vectors/enable_amendment_fixcleanup330_106911489.json"));
}
