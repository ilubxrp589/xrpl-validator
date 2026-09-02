//! Byte-exact vector gate for EscrowCreate (F45, 2026-08-31).
//!
//! Mainnet #106674441 tx 938FDBDF…: the created Escrow must carry the
//! creator's Sequence (rippled Escrow.cpp:544, getSeqValue). The live shadow
//! caught ours at 114B vs canonical 119B (diff @8 — exactly the missing
//! 0x24 UInt32). This test replays the REAL transaction through the full
//! native pipeline (build_txfields → native_apply_one → stamp_threading →
//! canon → encode) against the REAL pre-state and demands the canonical
//! bytes back — the millisecond gate for creation-completeness fixes, where
//! a window replay costs the better part of an hour and parity_probe is
//! byte-blind on created nodes.

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

fn key32(hex_key: &str) -> Hash256 {
    Hash256(<[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap())
}

/// Load a canonical ledger-entry binary into the state exactly as the live
/// mirror does: decode → hexify → JSON bytes.
fn hydrate(state: &mut LedgerState, key_hex: &str, entry_hex: &str) {
    let bytes = hex::decode(entry_hex.trim()).unwrap();
    let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).unwrap();
    hexify_addresses(&mut jv);
    state
        .state_map
        .insert(key32(key_hex), serde_json::to_vec(&jv).unwrap())
        .unwrap();
}

const ESCROW_KEY: &str = "4D3D9DE32157E2D238CBFA2CF9D83C7273FE0AC028F482862D234B243E6BEB38";
const TX_HASH: &str = "938FDBDF31EF65D75BED3476E5246A58B4A876C0D12BB8B9A557C37D3DC2C665";

#[test]
fn escrow_create_object_is_byte_exact() {
    // The replay's exact per-ledger header recipe for applying #106674441.
    let header = LedgerHeader {
        sequence: 106_674_440,
        total_coins: 99_985_621_983_547_049,
        parent_hash: Hash256([0; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: 841_523_980,
        close_time: 841_523_980,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    hydrate(
        &mut state,
        "9C6ACBEBA4FA2FB55732A04095507ABDC984C8ECD7D9E71259E68EA9CBAA5CF9",
        include_str!("vectors/escrow_sender_ar.hex"),
    );
    hydrate(
        &mut state,
        "F68F4218FB64D360A23C3F3D7F1A34B18513DA617D1DC65376F26B8CACDD4AE3",
        include_str!("vectors/escrow_owner_dir.hex"),
    );

    let tx: Value = json!({
        "Account": "rqzuKcL4yfmGDu3nR1TceXL8v8TsnG7B4",
        "Amount": "15000000000",
        "Destination": "rqzuKcL4yfmGDu3nR1TceXL8v8TsnG7B4",
        "Fee": "12",
        "FinishAfter": 883_587_600u64,
        "LastLedgerSequence": 106_674_459u64,
        "Sequence": 65_816_638u64,
        "SigningPubKey": "029F462302BA47914B89AA3D3A01FA1203D2FF113B30CCF745DE7C809C75957649",
        "TransactionType": "EscrowCreate",
        "TxnSignature": "304402200B8CA5A0B066FD377AC8ECAC4CF4A7F7E8014D2DC60AC6EBE9CAA962CE5FC760022013B3773D0D8ECFAAD7BF5E9CC423E4137726C71A96986050BF919A1DAB66EEB0",
        "hash": TX_HASH,
    });
    let txf = build_txfields(&tx).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    assert_eq!(ter, "tesSUCCESS", "mainnet applied this escrow");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        TX_HASH,
        106_674_441,
    );

    let ent = mods.get(&key32(ESCROW_KEY)).expect("escrow object created");
    let bytes = match ent {
        SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
        SandboxEntry::Deleted => panic!("escrow deleted?"),
    };
    let mut jv: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(jv["Sequence"].as_u64(), Some(65_816_638), "Escrow.cpp:544 stamps getSeqValue");
    canon_for_encode(&mut jv);
    let enc = xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap();
    let want = hex::decode(include_str!("vectors/escrow_4D3D9DE3.hex").trim()).unwrap();
    assert_eq!(
        hex::encode_upper(&enc),
        hex::encode_upper(&want),
        "created Escrow must byte-match the mainnet object"
    );
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

/// Finding 85 — #106703533 F84D3EB3: an EscrowCreate of 1000 drops by an
/// account with 96 objects and 20274654 drops. rippled reserves for
/// OwnerCount + 1 (20400000) and refuses with tecINSUFFICIENT_RESERVE; we
/// reserved for the current count and succeeded.
#[test]
fn escrow_create_reserves_for_the_new_object() {
    run_bundle(include_str!("vectors/escrow_create_reserve_plus_one_106703533.json"));
}
