//! Byte-exact vector gate for AMMCreate (F46, 2026-08-31).
//!
//! Mainnet #106674486 tx 8A6C485D…: a created AMM must carry Flags 0 +
//! OwnerNode and be dirLinked into the AMM account's own owner directory
//! (AMMCreate.cpp:263), the fee/auction/vote structures must be initialized
//! (AMMCreate.cpp:260), and each non-XRP asset trustline gets lsfAMMNode
//! (AMMCreate.cpp:288-300) — the LP-token line does NOT. The vector bundle
//! carries the real transaction, every pre-image it reads (at #106674485),
//! and the canonical bytes of the four objects the live shadow flagged; the
//! whole gate runs in milliseconds where the window replay costs ~50 min.

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

#[test]
fn amm_create_objects_are_byte_exact() {
    let bundle: Value =
        serde_json::from_str(include_str!("vectors/ammcreate_106674486.json")).unwrap();
    let seq = bundle["seq"].as_u64().unwrap() as u32;
    let pct = bundle["parent_close_time"].as_u64().unwrap() as u32;
    // The AMM pseudo-account derives from the PARENT LEDGER HASH — a zeroed
    // parent_hash shifts the account and every downstream key with it.
    let parent_hash = key32(bundle["parent_hash"].as_str().unwrap());
    let header = LedgerHeader {
        sequence: seq - 1,
        total_coins: bundle["total_coins"].as_u64().unwrap(),
        parent_hash,
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
    assert_eq!(ter, "tesSUCCESS", "mainnet applied this AMMCreate");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    for (k, want_hex) in bundle["expect_created"].as_object().unwrap() {
        let ent = mods.get(&key32(k)).unwrap_or_else(|| {
            let mut wrote: Vec<String> = mods
                .keys()
                .map(|h| {
                    let ty = mods
                        .get(h)
                        .and_then(|e| match e {
                            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => {
                                serde_json::from_slice::<Value>(b).ok()
                            }
                            SandboxEntry::Deleted => None,
                        })
                        .and_then(|v| v["LedgerEntryType"].as_str().map(String::from))
                        .unwrap_or_else(|| "deleted".into());
                    format!("{}:{ty}", hex::encode_upper(&h.0[..8]))
                })
                .collect();
            wrote.sort();
            panic!("object {k} must be written by the apply; wrote: {wrote:?}")
        });
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
            SandboxEntry::Deleted => panic!("object {k} deleted?"),
        };
        let mut jv: Value = serde_json::from_slice(&bytes).unwrap();
        canon_for_encode(&mut jv);
        let enc = xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap();
        let want = hex::decode(want_hex.as_str().unwrap().trim()).unwrap();
        assert_eq!(
            hex::encode_upper(&enc),
            hex::encode_upper(&want),
            "object {k} must byte-match the mainnet post-state"
        );
    }
}
