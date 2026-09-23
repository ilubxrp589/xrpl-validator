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
        // An expectation the apply did not write pins an object the
        // transaction must leave ALONE: it passes when it equals the seated
        // pre-image (finding 143's rule; finding 154's refused burn pins the
        // owner's page and the issuer's root this way).
        let Some(ent) = mods.get(&key32(k)) else {
            let pre_hex = bundle["pre"][k].as_str().unwrap_or_default().trim().to_uppercase();
            assert_eq!(
                want_hex.as_str().unwrap().trim().to_uppercase(),
                pre_hex,
                "target {k} was not written by the apply and does not pin the untouched pre-image"
            );
            continue;
        };
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
        // An EMPTY expectation is a deletion pin (finding 158): mainnet's
        // meta deleted the object in this transaction, so must the apply
        // (finding 245's expired offer is the first one this suite pins).
        let want_deleted = want_hex.as_str().unwrap().trim().is_empty();
        // An expectation the apply did not write pins an object the
        // transaction must leave ALONE: it passes when it equals the seated
        // pre-image (finding 143's rule; finding 154's refused burn pins the
        // owner's page and the issuer's root this way).
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

// Finding 154 — #106747838 EE7C95A6D70C: rDQ9jYQ1 burns a token owned by
// rBpHoJQjj (Owner set) whose flag word is 0x0008 — transferable, not
// burnable — so `NFTokenBurn::preclaim` refuses with tecNO_PERMISSION before
// looking at the issuer's minter. We had no permission check. The owner's
// last NFTokenPage and the issuer's root are pinned untouched.
#[test]
fn nft_burn_by_a_stranger_needs_a_burnable_token() {
    run_bundle(include_str!("vectors/nft_burn_by_a_stranger_needs_a_burnable_token_106747838.json"));
}

// Finding 185 — #106793245 E2BD39A88D8C (r4Tfxnu8qy sells an NFT for 1 XRP
// to rDeizxSR with Expiration 841935600, 13.6 h before the parent ledger's
// close): `hasExpired` is NFTokenCreateOffer::preclaim's first test and
// mainnet answers tecEXPIRED, fee only. We had no expiration check and
// created the offer with three directory pages.
#[test]
fn nftoken_create_offer_with_a_past_expiration_is_refused() {
    run_bundle(include_str!("vectors/nftoken_create_offer_with_a_past_expiration_is_refused_106793245.json"));
}

/// Finding 235 (#106850559 CF28C7C9E016): rwM46RCWpQ bids 10 NOIR.rhdbs6zj
/// for rQNUjxkQ's NFT while its NOIR line holds 0. rippled's
/// tokenOfferCreatePreclaim refuses with tecUNFUNDED_OFFER
/// (accountFunds(acct, amount).signum() <= 0); our preclaim had no funds
/// check at all and rested the offer, its directory page and an OwnerCount
/// unit.
#[test]
fn nft_buy_offer_from_an_unfunded_buyer_is_tec_unfunded_offer_106850559() {
    run_bundle(include_str!("vectors/nft_buy_offer_from_an_unfunded_buyer_is_tec_unfunded_offer_106850559.json"));
}

/// Finding 245 (#106898039 8C6528F472A7): rNBpHhJcev accepts rKqqb5QZXV's
/// sell offer FBA94DA4 — Amount 0, Destination the submitter — whose
/// Expiration 842038938 lies four days before the parent close 842389472.
/// `checkOffer` refuses it with tecEXPIRED before anything else is judged
/// (NFTokenAcceptOffer.cpp:75, hasExpired = parentCloseTime >= Expiration);
/// we moved the token and wrote its pages, fifteen times across
/// #106898039-#106898413.
#[test]
fn nft_accept_of_an_expired_sell_offer_is_tec_expired_106898039() {
    run_bundle(include_str!("vectors/nft_accept_of_an_expired_sell_offer_is_tec_expired_106898039.json"));
}

/// Finding 267 — brokered accept whose buy amount, less the broker's fee,
/// leaves the seller one drop short of the ask: rippled 3.3.0 refuses it in
/// preclaim with tecINSUFFICIENT_PAYMENT (NFTokenAcceptOffer.cpp:141-142),
/// claims the fee and leaves both offers standing. We brokered the sale.
#[test]
fn nft_brokered_accept_one_drop_short_after_the_broker_fee_is_tec_insufficient_payment_106945777() {
    run_bundle(include_str!("vectors/nft_brokered_accept_one_drop_short_after_the_broker_fee_is_tec_insufficient_payment_106945777.json"));
}

/// Finding 267, second specimen by the same broker 33 ledgers later: buy
/// 2000000 less fee 31781 against an ask of 1968220 — short by one drop.
#[test]
fn nft_brokered_accept_one_drop_short_after_the_broker_fee_is_tec_insufficient_payment_106945810() {
    run_bundle(include_str!("vectors/nft_brokered_accept_one_drop_short_after_the_broker_fee_is_tec_insufficient_payment_106945810.json"));
}

/// Finding 309 (#107063938 EBF238024D19): accepting a buy offer priced in an
/// IOU checks the BUYER's funds through `accountFunds(..., fhZERO_IF_FROZEN)`
/// (NFTokenAcceptOffer.cpp:212-218). rDnNmaX1 held 0.3373 of the 337.002
/// xSPECTAR it offered: tecINSUFFICIENT_FUNDS; we judged only XRP prices.
/// The bundle's pre carries the offer, the buyer's root and line, and the
/// issuer's root, fetched at the parent ledger (a tec's meta touches none).
#[test]
fn nft_accept_buy_offer_priced_in_iou_needs_the_buyer_funded_107063938() {
    run_bundle(include_str!(
        "vectors/nft_accept_buy_offer_priced_in_iou_needs_the_buyer_funded_107063938.json"
    ));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 3-2: t6 accepts t5's 4 XRP sell of a 5% transfer-fee token; 0.2 XRP goes to the issuer t4. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_resale_pays_the_issuer_its_transfer_fee_in_xrp_117af51b7531() {
    run_bundle(include_str!("vectors/nft_c13_resale_pays_the_issuer_its_transfer_fee_in_xrp_117AF51B7531.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 4-3: broker fee 0.6 XRP against a 0.5 XRP spread (sell 3 / buy 3.5). Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_brokered_accept_with_a_fee_above_the_spread_is_tec_insufficient_payment_e27546e80d1d() {
    run_bundle(include_str!("vectors/nft_c13_brokered_accept_with_a_fee_above_the_spread_is_tec_insufficient_payment_E27546E80D1D.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 4-4: broker fee 0.25 XRP, 5% royalty to the issuer, remainder to the seller t6. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_brokered_accept_splits_broker_fee_and_issuer_royalty_0bbc76237301() {
    run_bundle(include_str!("vectors/nft_c13_brokered_accept_splits_broker_fee_and_issuer_royalty_0BBC76237301.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 5-3: t5 sells B for 6 USD; preclaim reads the NFT issuer's USD line (tecNO_ISSUER without it). Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_iou_priced_sell_of_a_transfer_fee_token_needs_the_issuer_line_3537e0343c25() {
    run_bundle(include_str!("vectors/nft_c13_iou_priced_sell_of_a_transfer_fee_token_needs_the_issuer_line_3537E0343C25.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 5-4: t6 accepts B for 6 USD; 0.3 USD royalty to t4 over trust lines. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_iou_priced_accept_pays_the_royalty_in_usd_1c8636548a57() {
    run_bundle(include_str!("vectors/nft_c13_iou_priced_accept_pays_the_royalty_in_usd_1C8636548A57.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 6-2: t6 offers A after the brokered sale moved it to t5. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_sell_offer_on_a_token_one_no_longer_owns_is_tec_no_entry_fdf8b380064c() {
    run_bundle(include_str!("vectors/nft_c13_sell_offer_on_a_token_one_no_longer_owns_is_tec_no_entry_FDF8B380064C.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 6-3: t6 burns A it no longer holds. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_burn_by_a_former_owner_is_tec_no_entry_5cc2a8a01584() {
    run_bundle(include_str!("vectors/nft_c13_burn_by_a_former_owner_is_tec_no_entry_5CC2A8A01584.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 6-3b: t5 burns A while its own sell offer is open. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_owner_burn_removes_the_token_and_its_sell_offer_ab3619057e42() {
    run_bundle(include_str!("vectors/nft_c13_owner_burn_removes_the_token_and_its_sell_offer_AB3619057E42.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 6-4: t6 accepts the sell offer the burn deleted. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_accept_of_an_offer_deleted_by_a_burn_is_tec_object_not_found_770d0b750910() {
    run_bundle(include_str!("vectors/nft_c13_accept_of_an_offer_deleted_by_a_burn_is_tec_object_not_found_770D0B750910.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 7-1: t4 (issuer, tfBurnable) burns B with Owner t6. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_issuer_burns_a_burnable_token_held_by_another_account_be9e0306e1e1() {
    run_bundle(include_str!("vectors/nft_c13_issuer_burns_a_burnable_token_held_by_another_account_BE9E0306E1E1.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 8-2: t6 accepts a sell offer destined to t5. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_accept_by_someone_other_than_the_destination_is_tec_no_permission_3ce35b273c38() {
    run_bundle(include_str!("vectors/nft_c13_accept_by_someone_other_than_the_destination_is_tec_no_permission_3CE35B273C38.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 8-3: t5 accepts the issuer's sell of a non-transferable token. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_issuer_transfers_a_non_transferable_token_5ee3d06e53d1() {
    run_bundle(include_str!("vectors/nft_c13_issuer_transfers_a_non_transferable_token_5EE3D06E53D1.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 9-2: t6 (NFTokenMinter of t4) mints with Issuer t4. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_authorized_minter_mints_on_behalf_of_the_issuer_394ef4194644() {
    run_bundle(include_str!("vectors/nft_c13_authorized_minter_mints_on_behalf_of_the_issuer_394EF4194644.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 9-3: t5 mints with Issuer t4 without being its minter. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_mint_with_issuer_by_a_non_minter_is_tec_no_permission_6ca753295790() {
    run_bundle(include_str!("vectors/nft_c13_mint_with_issuer_by_a_non_minter_is_tec_no_permission_6CA753295790.json"));
}

/// Campaign 13 (testnet NFT depth, 2026-09-21) 9-5: t5 accepts a sell offer whose Expiration passed. Testnet
/// ledger's verdict, byte-exact under the port.
#[test]
fn nft_c13_accept_of_an_expired_offer_is_tec_expired_ecf3f4863cb3() {
    run_bundle(include_str!("vectors/nft_c13_accept_of_an_expired_offer_is_tec_expired_ECF3F4863CB3.json"));
}

/// Finding 397 (soak #26 receipt, mainnet #107190696 072B81D6348C): a direct accept of a sell offer whose
/// Destination names another account, by an acceptor who is also short of the price, is tecNO_PERMISSION —
/// rippled's preclaim tests the Destination before the funds (NFTokenAcceptOffer.cpp:239-263). We answered
/// tecINSUFFICIENT_FUNDS: the funds test ran in preclaim, the Destination test only in do_apply.
#[test]
fn nftaccept_destination_before_funds_107190696() {
    run_bundle(include_str!("vectors/nftaccept_destination_before_funds_107190696.json"));
}
