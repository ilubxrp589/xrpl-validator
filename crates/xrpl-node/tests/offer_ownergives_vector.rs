//! Byte-exact vector drill for finding 55 (2026-09-01): ownerGives — the
//! maker pays the out-issuer's TransferRate on top of what the taker gets.
//!
//! Mainnet #106693634 tx C3ECD3… (XRP→EVR, tfSell|IoC): the maker holds
//! 7173.556608098586 EVR at rate 1.002. rippled sizes the funding-limited
//! fill on funds/rate — the taker receives 7159.238131834916, the maker's
//! line drains to EXACT ZERO, the issuer burns the difference — and routes
//! the remaining 177,850,963 drops through the pool. We sized on the raw
//! balance and split the fill 874,298 drops off (four macroscopic diffs
//! wearing one-ulp costumes). BookStep.cpp:808-830 verbatim.
use serde_json::Value;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

fn key32(hex_key: &str) -> Hash256 {
    Hash256(<[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap())
}

#[test]
fn funding_limited_fill_sizes_on_owner_gives() {
    let bundle: Value =
        serde_json::from_str(include_str!("vectors/offer_ulp_106693634.json")).unwrap();
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
        let bytes = hex::decode(v.as_str().unwrap().trim()).unwrap();
        let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).unwrap();
        hexify_addresses(&mut jv);
        state
            .state_map
            .insert(key32(k), serde_json::to_vec(&jv).unwrap())
            .unwrap();
    }

    let tx = &bundle["tx"];
    let tx_hash = tx["hash"].as_str().unwrap();
    let txf = build_txfields(tx).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    assert_eq!(ter, "tesSUCCESS", "mainnet applied this OfferCreate");

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
