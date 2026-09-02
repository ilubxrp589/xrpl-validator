//! Byte-exact vector drills for AccountSet (2026-09-01).
//!
//! The transaction-level tf* flags (tfRequireDestTag/tfOptionalDestTag,
//! tfRequireAuth/tfOptionalAuth, tfDisallowXRP/tfAllowXRP) are the old
//! spelling of SetFlag/ClearFlag and rippled honours both
//! (SetAccount.cpp:326-336).
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

/// F65's regression guard (#106698282 95AFC9A9, the first receipt on the
/// lag-fix binary): a fresh issuer's `AccountSet` with `SetFlag: 8`
/// (DefaultRipple) and transaction `Flags: 0x100000` (tfDisallowXRP).
/// Mainnet's root lands Flags 0x880000 — DefaultRipple AND DisallowXRP —
/// where we only applied the SetFlag and landed 0x800000.
#[test]
fn account_set_honours_the_transaction_level_tf_flags_is_byte_exact() {
    run_bundle(include_str!("vectors/accountset_tfflags_106698282.json"));
}

/// Finding 70 — #106699631 D842A3B1: AccountSet with `Domain: ""` (and
/// SetFlag 15). rippled `makeFieldAbsent(sfDomain)` on an empty blob; we
/// filed an empty VL field (`7700`), two bytes longer than mainnet's root.
#[test]
fn account_set_empty_domain_removes_the_field() {
    run_bundle(include_str!("vectors/accountset_clear_domain_106699631.json"));
}

/// Finding 86 — #106703565 9709366F: SetFlag 16 (asfAllowTrustLineClawback)
/// by an account that owns 96 objects. rippled refuses with tecOWNERS unless
/// the owner directory is empty (SetAccount.cpp:288-292); we set the bit.
#[test]
fn account_set_clawback_needs_an_empty_owner_directory() {
    run_bundle(include_str!("vectors/accountset_clawback_owners_106703565.json"));
}
