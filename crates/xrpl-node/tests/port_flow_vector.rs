//! Port-only byte-exact vectors (2026-09-20, testnet campaign 6).
//!
//! These bundles are what the ported flow engine (`flow/`, Track 2) gets
//! right and the modelled engine (`tx::payment` / `tx::offer`) gets wrong.
//! Every test selects the port before applying; the model's misses are
//! recorded as findings in the harness notes, not fixed — the model is the
//! fallback now, the port is the engine.
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


fn port() {
    std::env::set_var("XRPL_FLOW_ENGINE", "port");
}

/// #20910362 17C85005: BST (8% transfer fee) → USD, tfPartialPayment with a
/// DeliverMin, one explicit path. The model leaves one maker line untouched
/// that mainnet modifies (a strand it never walked).
#[test]
fn payment_partial_delivermin_bst_to_usd_over_a_fee_issuer_testnet_20910362() {
    port();
    run_bundle(include_str!("vectors/payment_partial_delivermin_bst_to_usd_over_a_fee_issuer_testnet_20910362.json"));
}

/// #20910374 A7B61CD3: EUR → USD with tfLimitQuality across ladders on both
/// sides and the direct book. The model misses one modified node.
#[test]
fn payment_limit_quality_eur_to_usd_testnet_20910374() {
    port();
    run_bundle(include_str!("vectors/payment_limit_quality_eur_to_usd_testnet_20910374.json"));
}

/// #20910392 F9323063: EUR → XRP with tfLimitQuality. The model misses one
/// modified node.
#[test]
fn payment_limit_quality_eur_to_xrp_testnet_20910392() {
    port();
    run_bundle(include_str!("vectors/payment_limit_quality_eur_to_xrp_testnet_20910392.json"));
}

/// #20910241 58A1750A: EUR → USD partial with DeliverMin through an explicit
/// path. The model misses one modified node.
#[test]
fn payment_partial_delivermin_eur_to_usd_through_paths_testnet_20910241() {
    port();
    run_bundle(include_str!("vectors/payment_partial_delivermin_eur_to_usd_through_paths_testnet_20910241.json"));
}

/// #20910434 4EA250F1: BST (8% fee) → USD, tfPartialPayment, one path, no
/// DeliverMin. Model misses a modified node.
#[test]
fn payment_partial_bst_to_usd_over_a_fee_issuer_testnet_20910434() {
    port();
    run_bundle(include_str!("vectors/payment_partial_bst_to_usd_over_a_fee_issuer_testnet_20910434.json"));
}

/// #20910299 9949C9FA: EUR → USD, no flags, no explicit paths — the default
/// path only, with the direct book and the XRP bridge competing. Model
/// misses a modified node.
#[test]
fn payment_eur_to_usd_default_paths_testnet_20910299() {
    port();
    run_bundle(include_str!("vectors/payment_eur_to_usd_default_paths_testnet_20910299.json"));
}

/// #20910353 15773BE3: BST (8% fee) → USD, tfPartialPayment with DeliverMin
/// and no explicit paths. Model misses a modified node.
#[test]
fn payment_partial_delivermin_bst_to_usd_default_paths_testnet_20910353() {
    port();
    run_bundle(include_str!("vectors/payment_partial_delivermin_bst_to_usd_default_paths_testnet_20910353.json"));
}

/// Finding 357 (soak #23 receipt, #107155576 E70334ED9A24): rKVBZwTW buys 175
/// XLM for 37.301075 USD (both GateHub) with 0.958 XRP against a 1.6 XRP
/// reserve. flowCross moved nothing the ledger could see, but reported a
/// dust output — rippled's `crossed` is `takerAmount != placeOffer` (the
/// amounts, OfferCreate.cpp:794), so it claimed tecINSUF_RESERVE_OFFER; we
/// read `actual_out > 0` and answered tesSUCCESS. Pre-images are the
/// in-ledger state (ledger-start pre merged with the metas of txs 0..137 —
/// at ledger start the book still held a crossable offer). Port-only: the
/// fallback model keeps its own crossed test.
#[test]
fn offer_crossed_is_judged_by_the_amounts_not_by_a_dust_output_107155576() {
    port();
    run_bundle(include_str!("vectors/offer_crossed_is_judged_by_the_amounts_not_by_a_dust_output_107155576.json"));
}

/// Campaign 17 7b-7 (B345C16754F2, network tecKILLED): the maker whose line for the asset it receives is deep-frozen is removed (OfferStream.cpp:255); read-set gap fixed too. The model misses it by policy.
#[test]
fn c17_port_7b_7_tk_ioc_asks_5_usd_for_1500000d_0_30_only_bid_is_f2_owner_testnet_20976788() {
    port();
    run_bundle(include_str!("vectors/c17_port_7b_7_tk_ioc_asks_5_usd_for_1500000d_0_30_only_bid_is_f2_owner_testnet_20976788.json"));
}

/// Campaign 17 3-2 (D564FAE830A2, network tecKILLED): an IoC whose only cross is a 1e-15 fill is tecKILLED (F357 shape). The model misses it by policy.
#[test]
fn c17_port_3_2_tk_ioc_bid_40_usd_for_50_eur_dust_cross_only_testnet_20976551() {
    port();
    run_bundle(include_str!("vectors/c17_port_3_2_tk_ioc_bid_40_usd_for_50_eur_dust_cross_only_testnet_20976551.json"));
}

/// Finding 398 (soak #27 receipt, mainnet #107194228 16D371B9DA8A): an IoC OfferCreate sells 942.58 XRP for
/// RLUSD. Iteration 0 consumed four of the maker's five offers at one quality and left the fifth's funds at dust;
/// iteration 1 finds it tiny (`shouldRmSmallIncreasedQOffer`). rippled compares `ownerFunds_` with
/// `accountFundsHelper(cancelView_)`, and the cancelView is the strand's `afView` — a PaymentSandbox over the
/// flow's sandbox, so `accountHolds` ends in `balanceHook` over the flow's deferred-credit tables. Both reads saw
/// the same hooked 1e-14, so the offer was "found" tiny and removed (OwnerCount 7 to 2). We read the afView's raw
/// line balance (1.5e-14) without the hook, judged it "became" tiny, and kept the offer.
#[test]
fn offer_found_tiny_is_judged_through_the_af_view_balance_hook_107194228() {
    port();
    run_bundle(include_str!("vectors/offer_found_tiny_is_judged_through_the_af_view_balance_hook_107194228.json"));
}

/// rp2_partial_DeliverMin=delivered+1ulp Payment (3D77A347F556, network tecPATH_PARTIAL). Campaign 26 (testnet, 2026-10-03):
/// 16-digit rounding through QualityIn/QualityOut and DeliverMin; the model misses it, the port matches.
#[test]
fn c26b_rp2_partial_delivermin_delivered_1ulp_testnet_21249225() {
    port();
    run_bundle(include_str!("vectors/c26b_rp2_partial_delivermin_delivered_1ulp_testnet_21249225.json"));
}

/// rq2_G1->H3_direct_7.777777777777777_(QIn_0.95) Payment (98910B4C79B0, network tecPATH_PARTIAL). Campaign 26 (testnet, 2026-10-03):
/// 16-digit rounding through QualityIn/QualityOut and DeliverMin; the model misses it, the port matches.
#[test]
fn c26b_rq2_g1_h3_direct_7_777777777777777_qin_0_95_testnet_21249327() {
    port();
    run_bundle(include_str!("vectors/c26b_rq2_g1_h3_direct_7_777777777777777_qin_0_95_testnet_21249327.json"));
}

/// rq1_H2->H3_123.4567890123456_(QOut_1.05,_rate,_Q Payment (C7E966379F38, network tesSUCCESS). Campaign 26 (testnet, 2026-10-03):
/// 16-digit rounding through QualityIn/QualityOut and DeliverMin; the model misses it, the port matches.
#[test]
fn c26b_rq1_h2_h3_123_4567890123456_qout_1_05_rate_q_testnet_21249325() {
    port();
    run_bundle(include_str!("vectors/c26b_rq1_h2_h3_123_4567890123456_qout_1_05_rate_q_testnet_21249325.json"));
}



/// p10.b.x2.sell_ioc_finishes_C093 OfferCreate (2FFCAD60A143, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p10_b_x2_sell_ioc_finishes_c093_testnet_21249526() {
    port();
    run_bundle(include_str!("vectors/c26c_p10_b_x2_sell_ioc_finishes_c093_testnet_21249526.json"));
}

/// p11.a.x2.fok_buy33_page1_and_new_page2_behind_empty_root OfferCreate (5CED2B19A59D, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p11_a_x2_fok_buy33_page1_and_new_page2_behind_empty_root_testnet_21250269() {
    port();
    run_bundle(include_str!("vectors/c26c_p11_a_x2_fok_buy33_page1_and_new_page2_behind_empty_root_testnet_21250269.json"));
}

/// p11e.x.plain_buy27_empty_root_issuer_page_then_last_page OfferCreate (EE8EE9BE45A2, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p11e_x_plain_buy27_empty_root_issuer_page_then_last_page_testnet_21250348() {
    port();
    run_bundle(include_str!("vectors/c26c_p11e_x_plain_buy27_empty_root_issuer_page_then_last_page_testnet_21250348.json"));
}

/// p1.x10.payment_xrp_to_usd_deletes_A100_midpath Payment (EE80C8829290, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p1_x10_payment_xrp_to_usd_deletes_a100_midpath_testnet_21248759() {
    port();
    run_bundle(include_str!("vectors/c26c_p1_x10_payment_xrp_to_usd_deletes_a100_midpath_testnet_21248759.json"));
}

/// p1.x2.ioc_buy1_walks_empty_root_into_page1 OfferCreate (6843BC725880, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p1_x2_ioc_buy1_walks_empty_root_into_page1_testnet_21248719() {
    port();
    run_bundle(include_str!("vectors/c26c_p1_x2_ioc_buy1_walks_empty_root_into_page1_testnet_21248719.json"));
}

/// p1.x3.sell_ioc_36_empties_page1_relinks_root_into_page2 OfferCreate (E4E5BC3A1EAB, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p1_x3_sell_ioc_36_empties_page1_relinks_root_into_page2_testnet_21248721() {
    port();
    run_bundle(include_str!("vectors/c26c_p1_x3_sell_ioc_36_empties_page1_relinks_root_into_page2_testnet_21248721.json"));
}

/// p1.x4.fok_buy20_finishes_A100_then_A101 OfferCreate (E4C300B808E5, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p1_x4_fok_buy20_finishes_a100_then_a101_testnet_21248723() {
    port();
    run_bundle(include_str!("vectors/c26c_p1_x4_fok_buy20_finishes_a100_then_a101_testnet_21248723.json"));
}

/// p10.b.x.ioc_buy38_dust_funded_offer_at_page_boundary OfferCreate (7ED482125FD5, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p10_b_x_ioc_buy38_dust_funded_offer_at_page_boundary_testnet_21249524() {
    port();
    run_bundle(include_str!("vectors/c26c_p10_b_x_ioc_buy38_dust_funded_offer_at_page_boundary_testnet_21249524.json"));
}

/// p1.x9.ioc_buy5_crosses_relinked_state OfferCreate (967345615AD4, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p1_x9_ioc_buy5_crosses_relinked_state_testnet_21248757() {
    port();
    run_bundle(include_str!("vectors/c26c_p1_x9_ioc_buy5_crosses_relinked_state_testnet_21248757.json"));
}

/// p3.x3.partial_limitquality_payment_takes_rest_delivermin Payment (1A7149FF9614, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p3_x3_partial_limitquality_payment_takes_rest_delivermin_testnet_21248863() {
    port();
    run_bundle(include_str!("vectors/c26c_p3_x3_partial_limitquality_payment_takes_rest_delivermin_testnet_21248863.json"));
}

/// p2.x2.ioc_buy3_empty_root_to_page2 OfferCreate (AF29B659AF37, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p2_x2_ioc_buy3_empty_root_to_page2_testnet_21248805() {
    port();
    run_bundle(include_str!("vectors/c26c_p2_x2_ioc_buy3_empty_root_to_page2_testnet_21248805.json"));
}

/// p4.d.x.sell_usd_40_through_deepfrozen_page1 OfferCreate (C3387367EC99, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p4_d_x_sell_usd_40_through_deepfrozen_page1_testnet_21248984() {
    port();
    run_bundle(include_str!("vectors/c26c_p4_d_x_sell_usd_40_through_deepfrozen_page1_testnet_21248984.json"));
}

/// p2.x3.plain_buy67_deletes_A098_and_A099_whole OfferCreate (20B9558CB510, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p2_x3_plain_buy67_deletes_a098_and_a099_whole_testnet_21248807() {
    port();
    run_bundle(include_str!("vectors/c26c_p2_x3_plain_buy67_deletes_a098_and_a099_whole_testnet_21248807.json"));
}

/// p4.c.x.sell_fok_40_through_no_line_page1 OfferCreate (5E2288E4DB01, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p4_c_x_sell_fok_40_through_no_line_page1_testnet_21248953() {
    port();
    run_bundle(include_str!("vectors/c26c_p4_c_x_sell_fok_40_through_no_line_page1_testnet_21248953.json"));
}

/// p6.x2.payment_usd_eur_xrp_path_partial_delivermin Payment (88DF3D48A9B1, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p6_x2_payment_usd_eur_xrp_path_partial_delivermin_testnet_21249109() {
    port();
    run_bundle(include_str!("vectors/c26c_p6_x2_payment_usd_eur_xrp_path_partial_delivermin_testnet_21249109.json"));
}

/// p4.e.x.sell_usd_through_reserve_bound_xrp_maker_line_created OfferCreate (51953186B7A7, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p4_e_x_sell_usd_through_reserve_bound_xrp_maker_line_created_testnet_21249011() {
    port();
    run_bundle(include_str!("vectors/c26c_p4_e_x_sell_usd_through_reserve_bound_xrp_maker_line_created_testnet_21249011.json"));
}

/// p7.x1.ioc_buy30_limit_at_book_quality_pool_beside_book OfferCreate (7121B69E6187, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p7_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249146() {
    port();
    run_bundle(include_str!("vectors/c26c_p7_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249146.json"));
}

/// p9.d.x2.payment_xrp_eur_4_on_relinked_state Payment (F5DC9A610C0E, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p9_d_x2_payment_xrp_eur_4_on_relinked_state_testnet_21249359() {
    port();
    run_bundle(include_str!("vectors/c26c_p9_d_x2_payment_xrp_eur_4_on_relinked_state_testnet_21249359.json"));
}

/// p7b.x1.ioc_buy30_limit_at_book_quality_pool_beside_book OfferCreate (8E1741A00A9E, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p7b_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249238() {
    port();
    run_bundle(include_str!("vectors/c26c_p7b_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249238.json"));
}

/// p5.x1.sell_across_three_multipage_levels_expired_and_own_offers OfferCreate (2E20461BAEEA, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p5_x1_sell_across_three_multipage_levels_expired_and_own_offers_testnet_21249058() {
    port();
    run_bundle(include_str!("vectors/c26c_p5_x1_sell_across_three_multipage_levels_expired_and_own_offers_testnet_21249058.json"));
}

/// p6.x1.autobridged_sell_usd_for_eur_bridge_then_direct_pages0_1_into_2 OfferCreate (6AEBBDBFCC68, network tesSUCCESS).
/// Campaign 26 agent C (testnet, 2026-10-03): a multi-page book level the model's walk misses; the port matches.
#[test]
fn c26c_p6_x1_autobridged_sell_usd_for_eur_bridge_then_direct_pages0_1_into_2_testnet_21249107() {
    port();
    run_bundle(include_str!("vectors/c26c_p6_x1_autobridged_sell_usd_for_eur_bridge_then_direct_pages0_1_into_2_testnet_21249107.json"));
}
