//! Campaign 24 (devnet, 2026-09-23) byte-exact vectors: Batch (BatchV1_1, active on mainnet
//! ~2026-10-09) x the inner types campaigns 8 and 23 never put inside a Batch — NFToken, OfferCreate,
//! Check, PaymentChannel, Escrow, Oracle, DID, AMM, TrustSet, AccountSet. Each shape has an inner
//! create or change an object a LATER inner uses or deletes (the per-inner threading and
//! owner-threading rules, F392 / F394), plus AllOrNothing discards of complex creations. No engine
//! finding: 27/27 byte-exact under both flow engines on first run. Same harness as campaign 23.
use serde_json::Value;
use std::collections::HashMap;
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

/// Hydrate the pre-state, apply the bundle's transaction, and pin its TER.
/// Returns the state as of BEFORE the apply (the threading stamps need it to
/// tell a real change from a write-back) together with the raw, unstamped
/// mutation map.
fn apply_bundle(bundle: &Value) -> (LedgerState, String, HashMap<Hash256, SandboxEntry>) {
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

    let txf = build_txfields(&bundle["tx"]).expect("txfields");
    let (ter, mods) = native_apply_one(&state, &txf);
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet recorded this transaction result");
    (state, ter, mods)
}

/// Every `expect` entry byte-for-byte against the stamped mutation map.
fn assert_expect(bundle: &Value, mods: &HashMap<Hash256, SandboxEntry>) {
    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        // An EMPTY expectation is a deletion pin (finding 158): mainnet's
        // meta deleted the object in this transaction, so must the apply.
        let want_deleted = want_hex.as_str().unwrap().trim().is_empty();
        // An expectation the apply did not write is legitimate only when it
        // pins an object the transaction must leave ALONE: its post-image is
        // then its pre-image, and the vector says so by expecting exactly
        // the seated bytes (finding 143 — the taker's own bid beyond the
        // ask's limit, which mainnet never names).
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

fn run_batch_bundle(bundle_json: &str) {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
    let (state, _ter, mut mods) = apply_bundle(&bundle);
    let pre = |k: &Hash256| state.state_map.lookup(k).map(|b| b.to_vec());
    let tx_hash = bundle["tx"]["hash"].as_str().unwrap();
    let seq = bundle["seq"].as_u64().unwrap() as u32;
    if bundle["tx"]["TransactionType"].as_str() != Some("Batch") {
        xrpl_ledger::ledger::threading::stamp_threading(&mut mods, &pre, tx_hash, seq);
        assert_expect(&bundle, &mods);
        return;
    }
    let ours = xrpl_ledger::tx::batch::take_inner_results();
    let touched = xrpl_ledger::tx::batch::take_inner_touched();
    let raw: Vec<String> = bundle["raw_inner_hashes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_uppercase())
        .collect();
    assert!(ours.len() <= raw.len() && touched.len() <= raw.len(), "more attempted inners than RawTransactions");
    xrpl_ledger::ledger::threading::stamp_batch_threading(&mut mods, &pre, tx_hash, seq, &raw[..touched.len()], &touched);
    let filed: HashMap<String, String> = bundle["filed_results"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.to_uppercase(), v.as_str().unwrap().to_string()))
        .collect();
    let aon = xrpl_node::native_apply::batch_all_or_nothing(&bundle["tx"]);
    for (i, id, want, mismatch) in xrpl_node::native_apply::pair_inner_verdicts(&raw, &ours, &filed, aon) {
        let got = ours.get(i).map(String::as_str).unwrap_or(xrpl_node::native_apply::INNER_NOT_ATTEMPTED);
        assert!(!mismatch, "inner {i} {id}: ours {got} vs ledger {want}");
    }
    assert_expect(&bundle, &mods);
}

/// Campaign 24 — 0-1 I sets DefaultRipple (054D232E1308, AccountSet tesSUCCESS).
#[test]
fn c24_0_1_i_sets_defaultripple_devnet_5536977() {
    run_batch_bundle(include_str!("vectors/c24_0_1_i_sets_defaultripple_devnet_5536977.json"));
}

/// Campaign 24 — 0-2 A trusts I for USD (7CA1FC277160, TrustSet tesSUCCESS).
#[test]
fn c24_0_2_a_trusts_i_for_usd_devnet_5536979() {
    run_batch_bundle(include_str!("vectors/c24_0_2_a_trusts_i_for_usd_devnet_5536979.json"));
}

/// Campaign 24 — 0-3 B trusts I for USD (16D6E36E95A5, TrustSet tesSUCCESS).
#[test]
fn c24_0_3_b_trusts_i_for_usd_devnet_5536981() {
    run_batch_bundle(include_str!("vectors/c24_0_3_b_trusts_i_for_usd_devnet_5536981.json"));
}

/// Campaign 24 — 0-4 I pays A 1000 USD (47B4DA38BAAF, Payment tesSUCCESS).
#[test]
fn c24_0_4_i_pays_a_1000_usd_devnet_5536983() {
    run_batch_bundle(include_str!("vectors/c24_0_4_i_pays_a_1000_usd_devnet_5536983.json"));
}

/// Campaign 24 — 0-5 I pays B 100 USD (F6519AA59EDE, Payment tesSUCCESS).
#[test]
fn c24_0_5_i_pays_b_100_usd_devnet_5536985() {
    run_batch_bundle(include_str!("vectors/c24_0_5_i_pays_b_100_usd_devnet_5536985.json"));
}

/// Campaign 24 — 0-6 A mints NFT #0 (sets FirstNFTokenSequence) (BC0F56676354, NFTokenMint tesSUCCESS).
#[test]
fn c24_0_6_a_mints_nft_0_sets_firstnftokensequence_devnet_5536987() {
    run_batch_bundle(include_str!("vectors/c24_0_6_a_mints_nft_0_sets_firstnftokensequence_devnet_5536987.json"));
}

/// Campaign 24 — 0-7 A mints NFT #1 (for 1-4) (9954AD131075, NFTokenMint tesSUCCESS).
#[test]
fn c24_0_7_a_mints_nft_1_for_1_4_devnet_5536989() {
    run_batch_bundle(include_str!("vectors/c24_0_7_a_mints_nft_1_for_1_4_devnet_5536989.json"));
}

/// Campaign 24 — 2-1 independent: A offers 10 USD for 10 XRP, B crosses it fully (9E832A3F13CF, Batch tesSUCCESS).
#[test]
fn c24_2_1_independent_a_offers_10_usd_for_10_xrp_b_crosses_it_full_devnet_5536996() {
    run_batch_bundle(include_str!("vectors/c24_2_1_independent_a_offers_10_usd_for_10_xrp_b_crosses_it_full_devnet_5536996.json"));
}

/// Campaign 24 — 3-1 independent: A checks 5 XRP to B, B cashes it (93E72630C986, Batch tesSUCCESS).
#[test]
fn c24_3_1_independent_a_checks_5_xrp_to_b_b_cashes_it_devnet_5536998() {
    run_batch_bundle(include_str!("vectors/c24_3_1_independent_a_checks_5_xrp_to_b_b_cashes_it_devnet_5536998.json"));
}

/// Campaign 24 — 3-2 independent: A checks 5 USD to B, A cancels it (DD2F2F76AE09, Batch tesSUCCESS).
#[test]
fn c24_3_2_independent_a_checks_5_usd_to_b_a_cancels_it_devnet_5537000() {
    run_batch_bundle(include_str!("vectors/c24_3_2_independent_a_checks_5_usd_to_b_a_cancels_it_devnet_5537000.json"));
}

/// Campaign 24 — 4-1 independent: A opens a 10 XRP channel to B and funds it +5 (30F963DB9F42, Batch tesSUCCESS).
#[test]
fn c24_4_1_independent_a_opens_a_10_xrp_channel_to_b_and_funds_it_5_devnet_5537002() {
    run_batch_bundle(include_str!("vectors/c24_4_1_independent_a_opens_a_10_xrp_channel_to_b_and_funds_it_5_devnet_5537002.json"));
}

/// Campaign 24 — 5-1 independent: A escrows 1 XRP to B twice (finishable; cancelable after +30) (6D1231F36BB0, Batch tesSUCCESS).
#[test]
fn c24_5_1_independent_a_escrows_1_xrp_to_b_twice_finishable_cancel_devnet_5537005() {
    run_batch_bundle(include_str!("vectors/c24_5_1_independent_a_escrows_1_xrp_to_b_twice_finishable_cancel_devnet_5537005.json"));
}

/// Campaign 24 — 5-2 independent: B finishes escrow 1, A cancels escrow 2 (18DD413771AE, Batch tesSUCCESS).
#[test]
fn c24_5_2_independent_b_finishes_escrow_1_a_cancels_escrow_2_devnet_5537017() {
    run_batch_bundle(include_str!("vectors/c24_5_2_independent_b_finishes_escrow_1_a_cancels_escrow_2_devnet_5537017.json"));
}

/// Campaign 24 — 6-1 independent: A creates oracle 1, then updates it (95E3E37B16DD, Batch tesSUCCESS).
#[test]
fn c24_6_1_independent_a_creates_oracle_1_then_updates_it_devnet_5537019() {
    run_batch_bundle(include_str!("vectors/c24_6_1_independent_a_creates_oracle_1_then_updates_it_devnet_5537019.json"));
}

/// Campaign 24 — 6-2 independent: A deletes oracle 1, creates oracle 2 (DDC89DDED335, Batch tesSUCCESS).
#[test]
fn c24_6_2_independent_a_deletes_oracle_1_creates_oracle_2_devnet_5537021() {
    run_batch_bundle(include_str!("vectors/c24_6_2_independent_a_deletes_oracle_1_creates_oracle_2_devnet_5537021.json"));
}

/// Campaign 24 — 7-1 independent: DIDSet URI, DIDSet Data, DIDDelete (EDC902739CF7, Batch tesSUCCESS).
#[test]
fn c24_7_1_independent_didset_uri_didset_data_diddelete_devnet_5537023() {
    run_batch_bundle(include_str!("vectors/c24_7_1_independent_didset_uri_didset_data_diddelete_devnet_5537023.json"));
}

/// Campaign 24 — 7-2 allornothing: DIDSet, unfunded pay -> discarded (41DB524EAD92, Batch tesSUCCESS).
#[test]
fn c24_7_2_allornothing_didset_unfunded_pay_discarded_devnet_5537025() {
    run_batch_bundle(include_str!("vectors/c24_7_2_allornothing_didset_unfunded_pay_discarded_devnet_5537025.json"));
}

/// Campaign 24 — 9-1 independent: C opens a USD line, I pays C 50 USD on it (0958ED3FE62E, Batch tesSUCCESS).
#[test]
fn c24_9_1_independent_c_opens_a_usd_line_i_pays_c_50_usd_on_it_devnet_5537029() {
    run_batch_bundle(include_str!("vectors/c24_9_1_independent_c_opens_a_usd_line_i_pays_c_50_usd_on_it_devnet_5537029.json"));
}

/// Campaign 24 — 10-1 independent: B sets RequireDest, A pays B untagged, B clears RequireDest (671E4645EF80, Batch tesSUCCESS).
#[test]
fn c24_10_1_independent_b_sets_requiredest_a_pays_b_untagged_b_clear_devnet_5537031() {
    run_batch_bundle(include_str!("vectors/c24_10_1_independent_b_sets_requiredest_a_pays_b_untagged_b_clear_devnet_5537031.json"));
}

/// Campaign 24 — 1-1 independent: A mints, A offers it to B for 0, B accepts (page created, moved) (15A1C85CEE62, Batch tesSUCCESS).
#[test]
fn c24_1_1_independent_a_mints_a_offers_it_to_b_for_0_b_accepts_pag_devnet_5537041() {
    run_batch_bundle(include_str!("vectors/c24_1_1_independent_a_mints_a_offers_it_to_b_for_0_b_accepts_pag_devnet_5537041.json"));
}

/// Campaign 24 — 1-2 untilfailure: A mints, burns it, mints again (79A226A1D332, Batch tesSUCCESS).
#[test]
fn c24_1_2_untilfailure_a_mints_burns_it_mints_again_devnet_5537043() {
    run_batch_bundle(include_str!("vectors/c24_1_2_untilfailure_a_mints_burns_it_mints_again_devnet_5537043.json"));
}

/// Campaign 24 — 1-3 allornothing: mint, sell offer, cancel it, unfunded pay -> all discarded (59A34195C6DB, Batch tesSUCCESS).
#[test]
fn c24_1_3_allornothing_mint_sell_offer_cancel_it_unfunded_pay_all_devnet_5537045() {
    run_batch_bundle(include_str!("vectors/c24_1_3_allornothing_mint_sell_offer_cancel_it_unfunded_pay_all_devnet_5537045.json"));
}

/// Campaign 24 — 1-4 independent: sell offer for NFT #1, then cancel it (created+deleted in the batch) (9ECC5158CA6C, Batch tesSUCCESS).
#[test]
fn c24_1_4_independent_sell_offer_for_nft_1_then_cancel_it_created_devnet_5537047() {
    run_batch_bundle(include_str!("vectors/c24_1_4_independent_sell_offer_for_nft_1_then_cancel_it_created_devnet_5537047.json"));
}

/// Campaign 24 — 4-2 independent: B claims 3 XRP with A's signature, A asks to close (873FDEFFAC32, Batch tesSUCCESS).
#[test]
fn c24_4_2_independent_b_claims_3_xrp_with_a_s_signature_a_asks_to_devnet_5537050() {
    run_batch_bundle(include_str!("vectors/c24_4_2_independent_b_claims_3_xrp_with_a_s_signature_a_asks_to_devnet_5537050.json"));
}

/// Campaign 24 — 8-1 independent: AMMCreate XRP/USD, single-asset deposit 1 XRP, single-asset withdraw 0.5 XRP (5EF5FFA3A25F, Batch tesSUCCESS).
#[test]
fn c24_8_1_independent_ammcreate_xrp_usd_single_asset_deposit_1_xrp_devnet_5537053() {
    run_batch_bundle(include_str!("vectors/c24_8_1_independent_ammcreate_xrp_usd_single_asset_deposit_1_xrp_devnet_5537053.json"));
}

/// Campaign 24 — 8-2 independent: B deposits 1 USD, B bids with its fresh LP, A votes 600 (33E64FB3DCA5, Batch tesSUCCESS).
#[test]
fn c24_8_2_independent_b_deposits_1_usd_b_bids_with_its_fresh_lp_a_devnet_5537055() {
    run_batch_bundle(include_str!("vectors/c24_8_2_independent_b_deposits_1_usd_b_bids_with_its_fresh_lp_a_devnet_5537055.json"));
}

/// Campaign 24 — 8-3 allornothing: A deposits 1 XRP, unfunded pay -> discarded (D46125A9D279, Batch tesSUCCESS).
#[test]
fn c24_8_3_allornothing_a_deposits_1_xrp_unfunded_pay_discarded_devnet_5537057() {
    run_batch_bundle(include_str!("vectors/c24_8_3_allornothing_a_deposits_1_xrp_unfunded_pay_discarded_devnet_5537057.json"));
}
