//! Campaign 16 (testnet, 2026-09-23) byte-exact vectors: AMM deposit /
//! withdraw sub-modes (tfLPToken, tfOneAssetLPToken, tfLimitLPToken,
//! tfTwoAssetIfEmpty, tfWithdrawAll, tfOneAssetWithdrawAll), AMMBid with
//! AuthAccounts, AMMVote slot rotation, freeze and deep freeze on pool and
//! LP lines, and trades through the pools, against rippled 3.4.0. Findings
//! 383-388 (tx/amm.rs), plus three fetcher hydration pins. Same harness as
//! did_vector.rs.
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
    assert_eq!(ter, want_ter, "mainnet result for this transaction");

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

/// Finding 383 — 2-5 l5 tfLPToken LPTokenOut 333.333 only (proportional both sides) (5C9A096BB7B3, network tesSUCCESS).
#[test]
fn c16_2_5_l5_tflptoken_lptokenout_333_333_only_proportional_both_sides_testnet_20976174() {
    run_bundle(include_str!("vectors/c16_2_5_l5_tflptoken_lptokenout_333_333_only_proportional_both_sides_testnet_20976174.json"));
}

/// Finding 383 — 2-6 l5 tfLPToken 100 LP with minima 50 USD / 1 drop (USD minimum binds) (F5A5214E7BE6, network tecAMM_FAILED).
#[test]
fn c16_2_6_l5_tflptoken_100_lp_with_minima_50_usd_1_drop_usd_minimum_bi_testnet_20976176() {
    run_bundle(include_str!("vectors/c16_2_6_l5_tflptoken_100_lp_with_minima_50_usd_1_drop_usd_minimum_bi_testnet_20976176.json"));
}

/// Finding 383 — 2-7 l5 tfLPToken 100 LP with minima 0.001 USD / 1000 drops (not binding) (068BC607D581, network tesSUCCESS).
#[test]
fn c16_2_7_l5_tflptoken_100_lp_with_minima_0_001_usd_1000_drops_not_bin_testnet_20976178() {
    run_bundle(include_str!("vectors/c16_2_7_l5_tflptoken_100_lp_with_minima_0_001_usd_1000_drops_not_bin_testnet_20976178.json"));
}

/// Finding 383 — 2-25 l6 tfLPToken P2 LPTokenOut 0.777 (108BC7777241, network tesSUCCESS).
#[test]
fn c16_2_25_l6_tflptoken_p2_lptokenout_0_777_testnet_20976216() {
    run_bundle(include_str!("vectors/c16_2_25_l6_tflptoken_p2_lptokenout_0_777_testnet_20976216.json"));
}

/// Finding 383 — 2-31 l3 tfLPToken P3 LPTokenOut 12.345 (F8E3221220E0, network tesSUCCESS).
#[test]
fn c16_2_31_l3_tflptoken_p3_lptokenout_12_345_testnet_20976230() {
    run_bundle(include_str!("vectors/c16_2_31_l3_tflptoken_p3_lptokenout_12_345_testnet_20976230.json"));
}

/// Finding 384 — 2-11 l8 tfLimitLPToken 2.5 XRP, EPrice = price of ~0.8 XRP (binds: quadratic) (39255C5387B6, network tesSUCCESS).
#[test]
fn c16_2_11_l8_tflimitlptoken_2_5_xrp_eprice_price_of_0_8_xrp_binds_quad_testnet_20976187() {
    run_bundle(include_str!("vectors/c16_2_11_l8_tflimitlptoken_2_5_xrp_eprice_price_of_0_8_xrp_binds_quad_testnet_20976187.json"));
}

/// Finding 384 — 2-12 l7 tfLimitLPToken 1.111111 USD, EPrice = price of ~0.4 USD (binds, IOU side) (60ECA6680868, network tesSUCCESS).
#[test]
fn c16_2_12_l7_tflimitlptoken_1_111111_usd_eprice_price_of_0_4_usd_binds_testnet_20976189() {
    run_bundle(include_str!("vectors/c16_2_12_l7_tflimitlptoken_1_111111_usd_eprice_price_of_0_4_usd_binds_testnet_20976189.json"));
}

/// Finding 384 — 2-27 l6 tfLimitLPToken P2 0.5 EUR, EPrice = price of ~0.2 EUR (binds) (93F8BFDC1CDF, network tesSUCCESS).
#[test]
fn c16_2_27_l6_tflimitlptoken_p2_0_5_eur_eprice_price_of_0_2_eur_binds_testnet_20976220() {
    run_bundle(include_str!("vectors/c16_2_27_l6_tflimitlptoken_p2_0_5_eur_eprice_price_of_0_2_eur_binds_testnet_20976220.json"));
}

/// Finding 385 — 2-13 l6 tfTwoAssetIfEmpty 1 XRP + 1 USD into non-empty P1 (7949F6C5FC6B, network tecAMM_NOT_EMPTY).
#[test]
fn c16_2_13_l6_tftwoassetifempty_1_xrp_1_usd_into_non_empty_p1_testnet_20976191() {
    run_bundle(include_str!("vectors/c16_2_13_l6_tftwoassetifempty_1_xrp_1_usd_into_non_empty_p1_testnet_20976191.json"));
}

/// Finding 385 — 2-14 l9 tfTwoAssetIfEmpty 900 XRP + 900 USD into non-empty P1 (unfunded too) (49F7207A646F, network tecAMM_NOT_EMPTY).
#[test]
fn c16_2_14_l9_tftwoassetifempty_900_xrp_900_usd_into_non_empty_p1_unfun_testnet_20976193() {
    run_bundle(include_str!("vectors/c16_2_14_l9_tftwoassetifempty_900_xrp_900_usd_into_non_empty_p1_unfun_testnet_20976193.json"));
}

/// Finding 386 — 6-6 l10 withdraws 0.2 USD (deep frozen) (935F0BA9D554, network tecFROZEN).
#[test]
fn c16_6_6_l10_withdraws_0_2_usd_deep_frozen_testnet_20976318() {
    run_bundle(include_str!("vectors/c16_6_6_l10_withdraws_0_2_usd_deep_frozen_testnet_20976318.json"));
}

/// Findings 386/387 — 6-8 l10 tfLPToken 1 LP (deep frozen: both pool assets checked) (7440618478C3, network tecFROZEN).
#[test]
fn c16_6_8_l10_tflptoken_1_lp_deep_frozen_both_pool_assets_checked_testnet_20976324() {
    run_bundle(include_str!("vectors/c16_6_8_l10_tflptoken_1_lp_deep_frozen_both_pool_assets_checked_testnet_20976324.json"));
}

/// Finding 387 — 6-13 l5 tfLPToken 5 LP from frozen P1 (F5FEFEF3A863, network tecFROZEN).
#[test]
fn c16_6_13_l5_tflptoken_5_lp_from_frozen_p1_testnet_20976336() {
    run_bundle(include_str!("vectors/c16_6_13_l5_tflptoken_5_lp_from_frozen_p1_testnet_20976336.json"));
}

/// Finding 388 — 7-8 l10 tfLimitLPToken min 0.1 XRP, EPrice = E(0.3 XRP) (24F48BDDD861, network tecAMM_INVALID_TOKENS).
#[test]
fn c16_7_8_l10_tflimitlptoken_min_0_1_xrp_eprice_e_0_3_xrp_testnet_20976360() {
    run_bundle(include_str!("vectors/c16_7_8_l10_tflimitlptoken_min_0_1_xrp_eprice_e_0_3_xrp_testnet_20976360.json"));
}

/// Finding 388 — 7-9 l10 tfLimitLPToken min 2 XRP, EPrice = E(0.3 XRP) (64AE8BC3CFE8, network tecAMM_FAILED).
#[test]
fn c16_7_9_l10_tflimitlptoken_min_2_xrp_eprice_e_0_3_xrp_testnet_20976363() {
    run_bundle(include_str!("vectors/c16_7_9_l10_tflimitlptoken_min_2_xrp_eprice_e_0_3_xrp_testnet_20976363.json"));
}

/// Finding 388 — 7-19 l6 tfLimitLPToken P2 min 0.05 EUR, EPrice = E(0.15 EUR) (8626B1D3935D, network tesSUCCESS).
#[test]
fn c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384() {
    run_bundle(include_str!("vectors/c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384.json"));
}

/// Fetcher hydration (campaign 16) — 1-5 l4 AMMCreate duplicate XRP/USD (l4 holds no USD) (8A61CF0CF55C, network tecDUPLICATE): the fetcher now hydrates the pool an AMMCreate names by Amount/Amount2 (preclaim reads it first).
#[test]
fn c16_1_5_l4_ammcreate_duplicate_xrp_usd_l4_holds_no_usd_testnet_20976145() {
    run_bundle(include_str!("vectors/c16_1_5_l4_ammcreate_duplicate_xrp_usd_l4_holds_no_usd_testnet_20976145.json"));
}

/// Fetcher hydration (campaign 16) — 6-15 s pays l5 0.1 USD via frozen P1 (092BB64D2C93, network tecPATH_PARTIAL): the fetcher now hydrates the destination's line with the Amount issuer (DirectStepI::check reads it).
#[test]
fn c16_6_15_s_pays_l5_0_1_usd_via_frozen_p1_testnet_20976340() {
    run_bundle(include_str!("vectors/c16_6_15_s_pays_l5_0_1_usd_via_frozen_p1_testnet_20976340.json"));
}

/// Fetcher hydration (campaign 16) — 6-2 l10 tfSingleAsset 0.3 XRP into P1 (its USD line frozen) (6CF8257C40B0, network tecFROZEN): the fetcher now hydrates the depositor's line for an asset named only in Asset/Asset2.
#[test]
fn c16_6_2_l10_tfsingleasset_0_3_xrp_into_p1_its_usd_line_frozen_testnet_20976309() {
    run_bundle(include_str!("vectors/c16_6_2_l10_tfsingleasset_0_3_xrp_into_p1_its_usd_line_frozen_testnet_20976309.json"));
}

/// Campaign 16 — 1-1 l1 AMMCreate P1 XRP/USD 25.123456 XRP + 100.123456 USD fee 500 (B58FFF18935F, network tesSUCCESS).
#[test]
fn c16_1_1_l1_ammcreate_p1_xrp_usd_25_123456_xrp_100_123456_usd_fee_500_testnet_20976137() {
    run_bundle(include_str!("vectors/c16_1_1_l1_ammcreate_p1_xrp_usd_25_123456_xrp_100_123456_usd_fee_500_testnet_20976137.json"));
}

/// Campaign 16 — 1-2 l2 AMMCreate P2 USD/EUR 50.5 USD + 40.25 EUR fee 1000 (CEA3DE91EBA7, network tesSUCCESS).
#[test]
fn c16_1_2_l2_ammcreate_p2_usd_eur_50_5_usd_40_25_eur_fee_1000_testnet_20976139() {
    run_bundle(include_str!("vectors/c16_1_2_l2_ammcreate_p2_usd_eur_50_5_usd_40_25_eur_fee_1000_testnet_20976139.json"));
}

/// Campaign 16 — 1-3 l3 AMMCreate P3 XRP/TFX 20.5 XRP + 60.60606 TFX fee 300 (TransferRate issuer: waived) (63AFE521FB81, network tesSUCCESS).
#[test]
fn c16_1_3_l3_ammcreate_p3_xrp_tfx_20_5_xrp_60_60606_tfx_fee_300_transf_testnet_20976141() {
    run_bundle(include_str!("vectors/c16_1_3_l3_ammcreate_p3_xrp_tfx_20_5_xrp_60_60606_tfx_fee_300_transf_testnet_20976141.json"));
}

/// Campaign 16 — 2-1 l4 tfSingleAsset 1.234567 XRP into P1 (no LP line yet) (A82D667FA4D6, network tesSUCCESS).
#[test]
fn c16_2_1_l4_tfsingleasset_1_234567_xrp_into_p1_no_lp_line_yet_testnet_20976165() {
    run_bundle(include_str!("vectors/c16_2_1_l4_tfsingleasset_1_234567_xrp_into_p1_no_lp_line_yet_testnet_20976165.json"));
}

/// Campaign 16 — 2-2 l5 tfSingleAsset 1.234567 USD into P1 (0BBB23BD0A0D, network tesSUCCESS).
#[test]
fn c16_2_2_l5_tfsingleasset_1_234567_usd_into_p1_testnet_20976167() {
    run_bundle(include_str!("vectors/c16_2_2_l5_tfsingleasset_1_234567_usd_into_p1_testnet_20976167.json"));
}

/// Campaign 16 — 2-3 l6 tfTwoAsset max 10.101 USD + 5 XRP (USD side binds) (BF2D4A7A82EF, network tesSUCCESS).
#[test]
fn c16_2_3_l6_tftwoasset_max_10_101_usd_5_xrp_usd_side_binds_testnet_20976169() {
    run_bundle(include_str!("vectors/c16_2_3_l6_tftwoasset_max_10_101_usd_5_xrp_usd_side_binds_testnet_20976169.json"));
}

/// Campaign 16 — 2-4 l6 tfTwoAsset max 50 USD + 1.5 XRP (XRP side binds) (61262FB5240B, network tesSUCCESS).
#[test]
fn c16_2_4_l6_tftwoasset_max_50_usd_1_5_xrp_xrp_side_binds_testnet_20976171() {
    run_bundle(include_str!("vectors/c16_2_4_l6_tftwoasset_max_50_usd_1_5_xrp_xrp_side_binds_testnet_20976171.json"));
}

/// Campaign 16 — 2-8 l7 tfOneAssetLPToken 250.5 LP paid in XRP, max 3 XRP (789008B67AC9, network tesSUCCESS).
#[test]
fn c16_2_8_l7_tfoneassetlptoken_250_5_lp_paid_in_xrp_max_3_xrp_testnet_20976181() {
    run_bundle(include_str!("vectors/c16_2_8_l7_tfoneassetlptoken_250_5_lp_paid_in_xrp_max_3_xrp_testnet_20976181.json"));
}

/// Campaign 16 — 2-9 l7 tfOneAssetLPToken 250 LP paid in USD, max 0.01 USD (17526B6CA75D, network tecAMM_FAILED).
#[test]
fn c16_2_9_l7_tfoneassetlptoken_250_lp_paid_in_usd_max_0_01_usd_testnet_20976183() {
    run_bundle(include_str!("vectors/c16_2_9_l7_tfoneassetlptoken_250_lp_paid_in_usd_max_0_01_usd_testnet_20976183.json"));
}

/// Campaign 16 — 2-10 l8 tfLimitLPToken 2.5 XRP, EPrice 1.5x its own price (not binding) (81229AA9F1C6, network tesSUCCESS).
#[test]
fn c16_2_10_l8_tflimitlptoken_2_5_xrp_eprice_1_5x_its_own_price_not_bind_testnet_20976185() {
    run_bundle(include_str!("vectors/c16_2_10_l8_tflimitlptoken_2_5_xrp_eprice_1_5x_its_own_price_not_bind_testnet_20976185.json"));
}

/// Campaign 16 — 2-15 l9 tfSingleAsset 1000 USD (holds 5) (0B96F221439D, network tecUNFUNDED_AMM).
#[test]
fn c16_2_15_l9_tfsingleasset_1000_usd_holds_5_testnet_20976195() {
    run_bundle(include_str!("vectors/c16_2_15_l9_tfsingleasset_1000_usd_holds_5_testnet_20976195.json"));
}

/// Campaign 16 — 2-16 l9 tfSingleAsset 500 XRP, no LP line (D1D34EC48FD0, network tecINSUF_RESERVE_LINE).
#[test]
fn c16_2_16_l9_tfsingleasset_500_xrp_no_lp_line_testnet_20976197() {
    run_bundle(include_str!("vectors/c16_2_16_l9_tfsingleasset_500_xrp_no_lp_line_testnet_20976197.json"));
}

/// Campaign 16 — 2-19 l4 tfSingleAsset 1 drop into P1 (B0F9EFBD2025, network tecAMM_INVALID_TOKENS).
#[test]
fn c16_2_19_l4_tfsingleasset_1_drop_into_p1_testnet_20976204() {
    run_bundle(include_str!("vectors/c16_2_19_l4_tfsingleasset_1_drop_into_p1_testnet_20976204.json"));
}

/// Campaign 16 — 2-21 g1 (issuer) tfSingleAsset 5.5 USD into P1 (6EB911122BFA, network tesSUCCESS).
#[test]
fn c16_2_21_g1_issuer_tfsingleasset_5_5_usd_into_p1_testnet_20976208() {
    run_bundle(include_str!("vectors/c16_2_21_g1_issuer_tfsingleasset_5_5_usd_into_p1_testnet_20976208.json"));
}

/// Campaign 16 — 2-24 l6 tfSingleAsset P2 2.345678 EUR (C9481D21E1D5, network tesSUCCESS).
#[test]
fn c16_2_24_l6_tfsingleasset_p2_2_345678_eur_testnet_20976214() {
    run_bundle(include_str!("vectors/c16_2_24_l6_tfsingleasset_p2_2_345678_eur_testnet_20976214.json"));
}

/// Campaign 16 — 2-28 l6 tfOneAssetLPToken P2 0.4321 LP paid in USD, max 2 USD (05439CCAFA12, network tesSUCCESS).
#[test]
fn c16_2_28_l6_tfoneassetlptoken_p2_0_4321_lp_paid_in_usd_max_2_usd_testnet_20976223() {
    run_bundle(include_str!("vectors/c16_2_28_l6_tfoneassetlptoken_p2_0_4321_lp_paid_in_usd_max_2_usd_testnet_20976223.json"));
}

/// Campaign 16 — 2-29 l3 tfSingleAsset P3 5.555555 TFX (transfer fee waived) (4F34B5BA9928, network tesSUCCESS).
#[test]
fn c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225() {
    run_bundle(include_str!("vectors/c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225.json"));
}

/// Campaign 16 — 2-32 l6 tfSingleAsset P4 1.5 XRP (fee 0 pool) (EBBC0E851C36, network tesSUCCESS).
#[test]
fn c16_2_32_l6_tfsingleasset_p4_1_5_xrp_fee_0_pool_testnet_20976232() {
    run_bundle(include_str!("vectors/c16_2_32_l6_tfsingleasset_p4_1_5_xrp_fee_0_pool_testnet_20976232.json"));
}

/// Campaign 16 — 6-3 l10 tfSingleAsset 0.3 USD into P1 (frozen) (F1B35AC64479, network tecFROZEN).
#[test]
fn c16_6_3_l10_tfsingleasset_0_3_usd_into_p1_frozen_testnet_20976312() {
    run_bundle(include_str!("vectors/c16_6_3_l10_tfsingleasset_0_3_usd_into_p1_frozen_testnet_20976312.json"));
}

/// Campaign 16 — 6-11 l6 tfSingleAsset 0.1 XRP into frozen P1 (75E2B49FE16D, network tecFROZEN).
#[test]
fn c16_6_11_l6_tfsingleasset_0_1_xrp_into_frozen_p1_testnet_20976331() {
    run_bundle(include_str!("vectors/c16_6_11_l6_tfsingleasset_0_1_xrp_into_frozen_p1_testnet_20976331.json"));
}

/// Campaign 16 — 2-33 l6 AMMBid P2 BidMin = 1.5x computed (pays BidMin; refund 0 to creator l2) (EEFF36577072, network tesSUCCESS).
#[test]
fn c16_2_33_l6_ammbid_p2_bidmin_1_5x_computed_pays_bidmin_refund_0_to_cr_testnet_20976235() {
    run_bundle(include_str!("vectors/c16_2_33_l6_ammbid_p2_bidmin_1_5x_computed_pays_bidmin_refund_0_to_cr_testnet_20976235.json"));
}

/// Campaign 16 — 3-1 l4 AMMBid P1 no BidMin/BidMax (outbids creator l1 at price 0) (7E2C08F9DCFD, network tesSUCCESS).
#[test]
fn c16_3_1_l4_ammbid_p1_no_bidmin_bidmax_outbids_creator_l1_at_price_0_testnet_20976238() {
    run_bundle(include_str!("vectors/c16_3_1_l4_ammbid_p1_no_bidmin_bidmax_outbids_creator_l1_at_price_0_testnet_20976238.json"));
}

/// Campaign 16 — 3-3 l6 AMMBid P1 BidMax half the computed price (C99E2330A7AD, network tecAMM_FAILED).
#[test]
fn c16_3_3_l6_ammbid_p1_bidmax_half_the_computed_price_testnet_20976243() {
    run_bundle(include_str!("vectors/c16_3_3_l6_ammbid_p1_bidmax_half_the_computed_price_testnet_20976243.json"));
}

/// Campaign 16 — 3-5 l7 re-bids P1 AuthAccounts [l8,l9,l10,l2] (holder refunds itself) (231564557FCA, network tesSUCCESS).
#[test]
fn c16_3_5_l7_re_bids_p1_authaccounts_l8_l9_l10_l2_holder_refunds_itsel_testnet_20976247() {
    run_bundle(include_str!("vectors/c16_3_5_l7_re_bids_p1_authaccounts_l8_l9_l10_l2_holder_refunds_itsel_testnet_20976247.json"));
}

/// Campaign 16 — 3-6 s AMMBid P1 (not an LP) (3FF5C901C3EE, network tecAMM_INVALID_TOKENS).
#[test]
fn c16_3_6_s_ammbid_p1_not_an_lp_testnet_20976249() {
    run_bundle(include_str!("vectors/c16_3_6_s_ammbid_p1_not_an_lp_testnet_20976249.json"));
}

/// Campaign 16 — 8-1 l2 AMMBid P2 in time slot >= 1 (decayed price, refund to l6) (318B21D86779, network tesSUCCESS).
#[test]
fn c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570() {
    run_bundle(include_str!("vectors/c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570.json"));
}

/// Campaign 16 — 5-4 l7 votes 0 (TradingFee omitted in VoteEntry) (74388688BB7D, network tesSUCCESS).
#[test]
fn c16_5_4_l7_votes_0_tradingfee_omitted_in_voteentry_testnet_20976285() {
    run_bundle(include_str!("vectors/c16_5_4_l7_votes_0_tradingfee_omitted_in_voteentry_testnet_20976285.json"));
}

/// Campaign 16 — 5-8 l2 votes 600 (9th, more LP than the smallest: evicts it) (2B87CCFC422E, network tesSUCCESS).
#[test]
fn c16_5_8_l2_votes_600_9th_more_lp_than_the_smallest_evicts_it_testnet_20976294() {
    run_bundle(include_str!("vectors/c16_5_8_l2_votes_600_9th_more_lp_than_the_smallest_evicts_it_testnet_20976294.json"));
}

/// Campaign 16 — 5-9 l3 votes 50 (fewer LP than the smallest: slots refreshed only) (4DE217A631CA, network tesSUCCESS).
#[test]
fn c16_5_9_l3_votes_50_fewer_lp_than_the_smallest_slots_refreshed_only_testnet_20976296() {
    run_bundle(include_str!("vectors/c16_5_9_l3_votes_50_fewer_lp_than_the_smallest_slots_refreshed_only_testnet_20976296.json"));
}

/// Campaign 16 — 5-11 s votes 500 (not an LP) (7E0ADCB46F87, network tecAMM_INVALID_TOKENS).
#[test]
fn c16_5_11_s_votes_500_not_an_lp_testnet_20976300() {
    run_bundle(include_str!("vectors/c16_5_11_s_votes_500_not_an_lp_testnet_20976300.json"));
}

/// Campaign 16 — 7-15 l6 re-votes 260 (l4 and l8 now hold 0 LP: their entries drop) (220B29524EC5, network tesSUCCESS).
#[test]
fn c16_7_15_l6_re_votes_260_l4_and_l8_now_hold_0_lp_their_entries_drop_testnet_20976376() {
    run_bundle(include_str!("vectors/c16_7_15_l6_re_votes_260_l4_and_l8_now_hold_0_lp_their_entries_drop_testnet_20976376.json"));
}

/// Campaign 16 — 4-1 l8 (AuthAccount) pays l5 0.5 USD, SendMax 0.3 XRP via P1 (944D5C6F5157, network tesSUCCESS).
#[test]
fn c16_4_1_l8_authaccount_pays_l5_0_5_usd_sendmax_0_3_xrp_via_p1_testnet_20976260() {
    run_bundle(include_str!("vectors/c16_4_1_l8_authaccount_pays_l5_0_5_usd_sendmax_0_3_xrp_via_p1_testnet_20976260.json"));
}

/// Campaign 16 — 4-3 l8 OfferCreate gets 0.2 USD for up to 0.25 XRP (crosses P1) (F6D8BEC6EFB4, network tesSUCCESS).
#[test]
fn c16_4_3_l8_offercreate_gets_0_2_usd_for_up_to_0_25_xrp_crosses_p1_testnet_20976264() {
    run_bundle(include_str!("vectors/c16_4_3_l8_offercreate_gets_0_2_usd_for_up_to_0_25_xrp_crosses_p1_testnet_20976264.json"));
}

/// Campaign 16 — 4-6 l9 pays l3 1 TFX, SendMax 1 XRP via P3 (TransferRate 1.2) (C79D4A3E6FCA, network tesSUCCESS).
#[test]
fn c16_4_6_l9_pays_l3_1_tfx_sendmax_1_xrp_via_p3_transferrate_1_2_testnet_20976270() {
    run_bundle(include_str!("vectors/c16_4_6_l9_pays_l3_1_tfx_sendmax_1_xrp_via_p3_transferrate_1_2_testnet_20976270.json"));
}

/// Campaign 16 — 8-3 s pays l5 0.1 USD, SendMax 0.1 XRP via P1 (new slot, no AuthAccounts) (8F60EBF09F33, network tesSUCCESS).
#[test]
fn c16_8_3_s_pays_l5_0_1_usd_sendmax_0_1_xrp_via_p1_new_slot_no_authacc_testnet_20977575() {
    run_bundle(include_str!("vectors/c16_8_3_s_pays_l5_0_1_usd_sendmax_0_1_xrp_via_p1_new_slot_no_authacc_testnet_20977575.json"));
}

/// Campaign 16 — 6-4 l10 withdraws 0.2 USD from P1 (regular freeze, self-withdraw) (C8B79F796446, network tesSUCCESS).
#[test]
fn c16_6_4_l10_withdraws_0_2_usd_from_p1_regular_freeze_self_withdraw_testnet_20976314() {
    run_bundle(include_str!("vectors/c16_6_4_l10_withdraws_0_2_usd_from_p1_regular_freeze_self_withdraw_testnet_20976314.json"));
}

/// Campaign 16 — 6-7 l10 withdraws 0.05 XRP (deep frozen USD line, XRP only) (25F835618181, network tesSUCCESS).
#[test]
fn c16_6_7_l10_withdraws_0_05_xrp_deep_frozen_usd_line_xrp_only_testnet_20976322() {
    run_bundle(include_str!("vectors/c16_6_7_l10_withdraws_0_05_xrp_deep_frozen_usd_line_xrp_only_testnet_20976322.json"));
}

/// Campaign 16 — 6-14 g1 (issuer) tfLPToken 3 LP from frozen P1 (783186D68BFD, network tesSUCCESS).
#[test]
fn c16_6_14_g1_issuer_tflptoken_3_lp_from_frozen_p1_testnet_20976338() {
    run_bundle(include_str!("vectors/c16_6_14_g1_issuer_tflptoken_3_lp_from_frozen_p1_testnet_20976338.json"));
}

/// Campaign 16 — 7-1 l4 tfLPToken 100 LP (l4 has no USD line: created) (32FDDFF22591, network tesSUCCESS).
#[test]
fn c16_7_1_l4_tflptoken_100_lp_l4_has_no_usd_line_created_testnet_20976345() {
    run_bundle(include_str!("vectors/c16_7_1_l4_tflptoken_100_lp_l4_has_no_usd_line_created_testnet_20976345.json"));
}

/// Campaign 16 — 7-2 l4 tfWithdrawAll P1 (F3649AEB5916, network tesSUCCESS).
#[test]
fn c16_7_2_l4_tfwithdrawall_p1_testnet_20976347() {
    run_bundle(include_str!("vectors/c16_7_2_l4_tfwithdrawall_p1_testnet_20976347.json"));
}

/// Campaign 16 — 7-3 l6 tfTwoAsset max 1.5 USD + 1 XRP (52CA199FFEEE, network tesSUCCESS).
#[test]
fn c16_7_3_l6_tftwoasset_max_1_5_usd_1_xrp_testnet_20976349() {
    run_bundle(include_str!("vectors/c16_7_3_l6_tftwoasset_max_1_5_usd_1_xrp_testnet_20976349.json"));
}

/// Campaign 16 — 7-4 l7 tfSingleAsset 0.4321 XRP (8471B1E0E0E4, network tecAMM_INVALID_TOKENS).
#[test]
fn c16_7_4_l7_tfsingleasset_0_4321_xrp_testnet_20976351() {
    run_bundle(include_str!("vectors/c16_7_4_l7_tfsingleasset_0_4321_xrp_testnet_20976351.json"));
}

/// Campaign 16 — 7-6 l8 tfOneAssetWithdrawAll as XRP (min 0) (6C4DED5BE7DA, network tesSUCCESS).
#[test]
fn c16_7_6_l8_tfoneassetwithdrawall_as_xrp_min_0_testnet_20976355() {
    run_bundle(include_str!("vectors/c16_7_6_l8_tfoneassetwithdrawall_as_xrp_min_0_testnet_20976355.json"));
}

/// Campaign 16 — 7-7 l9 tfOneAssetLPToken 50 LP as USD (min 0.001) (0A7730F95DA6, network tesSUCCESS).
#[test]
fn c16_7_7_l9_tfoneassetlptoken_50_lp_as_usd_min_0_001_testnet_20976357() {
    run_bundle(include_str!("vectors/c16_7_7_l9_tfoneassetlptoken_50_lp_as_usd_min_0_001_testnet_20976357.json"));
}

/// Campaign 16 — 7-10 l9 tfLPToken more LP than held (7BCA63FD178A, network tecAMM_INVALID_TOKENS).
#[test]
fn c16_7_10_l9_tflptoken_more_lp_than_held_testnet_20976366() {
    run_bundle(include_str!("vectors/c16_7_10_l9_tflptoken_more_lp_than_held_testnet_20976366.json"));
}

/// Campaign 16 — 7-13 l5 tfLPToken 0.00001 LP (XRP side rounds to 0 drops) (E1643C26BCD6, network tecAMM_FAILED).
#[test]
fn c16_7_13_l5_tflptoken_0_00001_lp_xrp_side_rounds_to_0_drops_testnet_20976372() {
    run_bundle(include_str!("vectors/c16_7_13_l5_tflptoken_0_00001_lp_xrp_side_rounds_to_0_drops_testnet_20976372.json"));
}

/// Campaign 16 — 7-14 l5 tfLPToken 0.000000000001 LP (adjusts to 0) (4B9DC2C19489, network tecAMM_INVALID_TOKENS).
#[test]
fn c16_7_14_l5_tflptoken_0_000000000001_lp_adjusts_to_0_testnet_20976374() {
    run_bundle(include_str!("vectors/c16_7_14_l5_tflptoken_0_000000000001_lp_adjusts_to_0_testnet_20976374.json"));
}

/// Campaign 16 — 7-17 l6 tfSingleAsset P2 0.333333 EUR (1FDAAB3C7A28, network tesSUCCESS).
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380() {
    run_bundle(include_str!("vectors/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380.json"));
}

/// Campaign 16 — 7-25 l2 (sole LP) tfOneAssetWithdrawAll P4 as EUR (AD1FB2A1073B, network tecAMM_BALANCE).
#[test]
fn c16_7_25_l2_sole_lp_tfoneassetwithdrawall_p4_as_eur_testnet_20976398() {
    run_bundle(include_str!("vectors/c16_7_25_l2_sole_lp_tfoneassetwithdrawall_p4_as_eur_testnet_20976398.json"));
}

/// Campaign 16 — 7-26 l2 tfWithdrawAll P4 (last LP out) (75EC3BE26402, network tesSUCCESS).
#[test]
fn c16_7_26_l2_tfwithdrawall_p4_last_lp_out_testnet_20976400() {
    run_bundle(include_str!("vectors/c16_7_26_l2_tfwithdrawall_p4_last_lp_out_testnet_20976400.json"));
}
