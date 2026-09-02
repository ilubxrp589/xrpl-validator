//! Byte-exact vector drill for finding 48 (2026-08-31).
//!
//! Mainnet #106677548 tx D15BC4CE… (NFTokenAcceptOffer): the live shadow
//! flagged RippleState A7924882… one ULP off at offset 68 (Balance region),
//! PRE-OK — canonical inputs, divergent write. This test applies the REAL
//! transaction against its REAL pre-images (fetch_tx_bundle.py) and demands
//! the canonical post bytes for the flagged line. RED = the offline
//! reproducer that names the bytes; after the fix it stays as the
//! regression pin. (Pre-images are ledger-start; if an earlier tx of the
//! ledger touched the specimen's objects the repro can shift — upgrade the
//! bundle to meta-merged pre-images in that case.)

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
fn nft_accept_line_is_byte_exact() {
    let bundle: Value =
        serde_json::from_str(include_str!("vectors/nftaccept_106677548.json")).unwrap();
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
    assert_eq!(ter, "tesSUCCESS", "mainnet applied this NFTokenAcceptOffer");

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
    // The ledger's verdict — a tec specimen (fee-only) is pinned against
    // THIS, not against tesSUCCESS (fetch_tx_bundle records it as `result`).
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "the ledger's verdict for this transaction");

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

/// F64's regression guard (#106697665 37030A48, the second live-shadow
/// receipt after the F45-F60 deploy): an account's FIRST NFTokenMint, self-
/// minted at Sequence 106697663. rippled's `sfFirstNFTokenSequence` is the
/// issuer root's Sequence as-is only for an authorized-minter mint (Issuer
/// named) or a ticketed one; a plain self-mint takes `acctSeq - 1`, because
/// the minter's own root has already been bumped past the tx's Sequence
/// when doApply runs (NFTokenMint.cpp:263-275). We stored 106697664 and the
/// minted NFTokenID's token sequence followed — the AccountRoot and the
/// NFTokenPage both off by one.
#[test]
fn first_self_mint_first_nftoken_sequence_is_the_tx_sequence_is_byte_exact() {
    run_bundle(include_str!("vectors/nft_mint_firstseq_106697665.json"));
}

/// Finding 94 (2026-09-02): mainnet #106711435 tx DC521081D420… — a brokered
/// accept whose BUYER is the token's minter. rippled pays the issuer's cut
/// only when `seller != issuer && buyer != issuer`
/// (NFTokenAcceptOffer.cpp:542); our seller-only gate carved 500000 drops
/// (the 5 % TransferFee) off the seller and credited the buyer with them.
/// RED at HEAD~ (two AccountRoots one balance apart), GREEN with the gate.
#[test]
fn nft_accept_brokered_issuer_buyer_pays_no_royalty() {
    run_bundle(include_str!("vectors/nftaccept_brokered_issuer_buyer_106711435.json"));
}

/// #106714409 DB792CF854A2 (finding 100): broker rpx9JT brokers r3DuY4i5's
/// buy offer against r3DuY4i5's OWN sell offer. rippled's preclaim refuses
/// the loop — "a broker may not sell the token to the current owner"
/// (NFTokenAcceptOffer.cpp:105-108) — with tecCANT_ACCEPT_OWN_NFTOKEN_OFFER
/// and claims the fee; we ran the sale. The two offers are injected into
/// the bundle by hand (a tec meta carries only the AccountRoot); the target
/// is the broker's fee-only AccountRoot and the bundle's tec result.
#[test]
fn nft_accept_brokered_loop_is_cant_accept_own() {
    run_bundle(include_str!("vectors/nftaccept_brokered_loop_106714409.json"));
}
