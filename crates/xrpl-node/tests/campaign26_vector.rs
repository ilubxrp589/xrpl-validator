//! Campaign 26 (testnet, 2026-10-03) byte-exact vectors: EDGE STATES rather than features — ordinary
//! transactions meeting a boundary, the shapes behind mainnet receipts F397-F411. Agent A: AMM deposits,
//! withdrawals and clawbacks with the account at its reserve (−1 drop / exact / +1), the issuer as LP,
//! pools named in the opposite order to the pool object's, dust withdrawals. Findings 412 (AMMDeposit's
//! non-LP reserve gate is preclaim's, on the pre-fee balance), 413 (AMMClawback's holder reserve test
//! before fixCleanup3_4_0, measured by the issuer's pre-fee balance) and 414 (single-asset withdrawals
//! whose tokens adjust to zero are refused; an asset that adjusts to zero is withdrawn as zero), all in
//! tx/amm.rs. Testnet runs mainnet's rules (no testnet-only amendments; 16-digit Number). Same harness
//! as campaign16_vector.rs.
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


/// Finding 412 — 2-1 da tfSingleAsset 1.5 USD into PX, pre-fee = R(2) exact (45BEF6F3E8C2, network tecINSUF_RESERVE_LINE).
#[test]
fn c26a_2_1_da_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_exact_testnet_21248761() {
    run_bundle(include_str!("vectors/c26a_2_1_da_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_exact_testnet_21248761.json"));
}

/// Finding 412 — 2-3 da tfSingleAsset 1.5 USD into PX, pre-fee = R(2)+1 (F5E65210AEFE, network tesSUCCESS).
#[test]
fn c26a_2_3_da_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_1_testnet_21248765() {
    run_bundle(include_str!("vectors/c26a_2_3_da_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_1_testnet_21248765.json"));
}

/// Finding 412 — 2-4 db tfSingleAsset 1.5 USD into PX, pre-fee = R(2)+fee (7F8752A9FB16, network tesSUCCESS).
#[test]
fn c26a_2_4_db_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_fee_testnet_21248774() {
    run_bundle(include_str!("vectors/c26a_2_4_db_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_fee_testnet_21248774.json"));
}

/// Finding 412 — 2-5 dc tfSingleAsset 1.5 USD into PX, pre-fee = R(2)+fee+1 (C60EC2E56AB1, network tesSUCCESS).
#[test]
fn c26a_2_5_dc_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_fee_1_testnet_21248782() {
    run_bundle(include_str!("vectors/c26a_2_5_dc_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_fee_1_testnet_21248782.json"));
}

/// Finding 412 — 2-6 dd tfSingleAsset 1.5 EUR into PI (IOU/IOU), pre-fee = (94BADF6DF696, network tesSUCCESS).
#[test]
fn c26a_2_6_dd_tfsingleasset_1_5_eur_into_pi_iou_iou_pre_fee_testnet_21248792() {
    run_bundle(include_str!("vectors/c26a_2_6_dd_tfsingleasset_1_5_eur_into_pi_iou_iou_pre_fee_testnet_21248792.json"));
}

/// Finding 412 — 2-7 de tfOneAssetLPToken PI 0.5 LP paid in USD (max 3), pr (760F4B7FD6AE, network tesSUCCESS).
#[test]
fn c26a_2_7_de_tfoneassetlptoken_pi_0_5_lp_paid_in_usd_max_3_pr_testnet_21248801() {
    run_bundle(include_str!("vectors/c26a_2_7_de_tfoneassetlptoken_pi_0_5_lp_paid_in_usd_max_3_pr_testnet_21248801.json"));
}

/// 3-2 w1 tfWithdrawAll PI, OC 1, no EUR/USD line, pre-fee R( (408073E21D3D, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_2_w1_tfwithdrawall_pi_oc_1_no_eur_usd_line_pre_fee_r_testnet_21248835() {
    run_bundle(include_str!("vectors/c26a_3_2_w1_tfwithdrawall_pi_oc_1_no_eur_usd_line_pre_fee_r_testnet_21248835.json"));
}

/// 3-4 w1 tfWithdrawAll PI, pre-fee R(3) exactly (EED4DA34EF7C, network tesSUCCESS).
#[test]
fn c26a_3_4_w1_tfwithdrawall_pi_pre_fee_r_3_exactly_testnet_21248840() {
    run_bundle(include_str!("vectors/c26a_3_4_w1_tfwithdrawall_pi_pre_fee_r_3_exactly_testnet_21248840.json"));
}

/// 3-6 w6 tfSingleAsset 0.2 USD from PI at OC 1 (balance 1.25 (9817125B8F68, network tesSUCCESS).
#[test]
fn c26a_3_6_w6_tfsingleasset_0_2_usd_from_pi_at_oc_1_balance_1_25_testnet_21248849() {
    run_bundle(include_str!("vectors/c26a_3_6_w6_tfsingleasset_0_2_usd_from_pi_at_oc_1_balance_1_25_testnet_21248849.json"));
}

/// 3-7 w6 tfSingleAsset 0.2 EUR from PI at OC 2 (needs R(3)) (40ED0CFB8344, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_7_w6_tfsingleasset_0_2_eur_from_pi_at_oc_2_needs_r_3_testnet_21248851() {
    run_bundle(include_str!("vectors/c26a_3_7_w6_tfsingleasset_0_2_eur_from_pi_at_oc_2_needs_r_3_testnet_21248851.json"));
}

/// 3-8 w6 tfLPToken 0.3 LP from PI (USD line exists, EUR miss (4F298CA2FC9C, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_8_w6_tflptoken_0_3_lp_from_pi_usd_line_exists_eur_miss_testnet_21248853() {
    run_bundle(include_str!("vectors/c26a_3_8_w6_tflptoken_0_3_lp_from_pi_usd_line_exists_eur_miss_testnet_21248853.json"));
}

/// 3-0 w2 tfSingleAsset 200000 drops into TFX pool (CDCCE8C6AAE3, network tesSUCCESS).
#[test]
fn c26a_3_0_w2_tfsingleasset_200000_drops_into_tfx_pool_testnet_21248857() {
    run_bundle(include_str!("vectors/c26a_3_0_w2_tfsingleasset_200000_drops_into_tfx_pool_testnet_21248857.json"));
}

/// 3-10 w2 tfWithdrawAll PT named TFX/XRP, live after XRP = R (B2B3CC211F2A, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_10_w2_tfwithdrawall_pt_named_tfx_xrp_live_after_xrp_r_testnet_21248864() {
    run_bundle(include_str!("vectors/c26a_3_10_w2_tfwithdrawall_pt_named_tfx_xrp_live_after_xrp_r_testnet_21248864.json"));
}

/// 3-12 w2 tfWithdrawAll PT named TFX/XRP, live after XRP = R (052E04F2C179, network tesSUCCESS).
#[test]
fn c26a_3_12_w2_tfwithdrawall_pt_named_tfx_xrp_live_after_xrp_r_testnet_21248869() {
    run_bundle(include_str!("vectors/c26a_3_12_w2_tfwithdrawall_pt_named_tfx_xrp_live_after_xrp_r_testnet_21248869.json"));
}

/// 3-0 w3 tfSingleAsset 300000 drops into USD pool (0E4E6F81D96C, network tesSUCCESS).
#[test]
fn c26a_3_0_w3_tfsingleasset_300000_drops_into_usd_pool_testnet_21248874() {
    run_bundle(include_str!("vectors/c26a_3_0_w3_tfsingleasset_300000_drops_into_usd_pool_testnet_21248874.json"));
}

/// 3-14 w3 tfTwoAsset Amount=USD(max 5) Amount2=XRP 80000: US (BB660E17FB34, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_14_w3_tftwoasset_amount_usd_max_5_amount2_xrp_80000_us_testnet_21248881() {
    run_bundle(include_str!("vectors/c26a_3_14_w3_tftwoasset_amount_usd_max_5_amount2_xrp_80000_us_testnet_21248881.json"));
}

/// 3-15 w3 tfTwoAsset Amount=XRP 80000 Amount2=USD(max 5): li (B8380CFD639F, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_15_w3_tftwoasset_amount_xrp_80000_amount2_usd_max_5_li_testnet_21248883() {
    run_bundle(include_str!("vectors/c26a_3_15_w3_tftwoasset_amount_xrp_80000_amount2_usd_max_5_li_testnet_21248883.json"));
}

/// 3-17 w3 tfTwoAsset Amount=XRP 80000 Amount2=USD(max 5): li (A1DD707FA967, network tesSUCCESS).
#[test]
fn c26a_3_17_w3_tftwoasset_amount_xrp_80000_amount2_usd_max_5_li_testnet_21248887() {
    run_bundle(include_str!("vectors/c26a_3_17_w3_tftwoasset_amount_xrp_80000_amount2_usd_max_5_li_testnet_21248887.json"));
}

/// 3-0 w4 tfSingleAsset 300000 drops into USD pool (E962666BB05D, network tesSUCCESS).
#[test]
fn c26a_3_0_w4_tfsingleasset_300000_drops_into_usd_pool_testnet_21248891() {
    run_bundle(include_str!("vectors/c26a_3_0_w4_tfsingleasset_300000_drops_into_usd_pool_testnet_21248891.json"));
}

/// 3-19 w4 tfLPToken 1/3 of its LP, named USD/XRP, live after (D6F4A564DD2D, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_19_w4_tflptoken_1_3_of_its_lp_named_usd_xrp_live_after_testnet_21248899() {
    run_bundle(include_str!("vectors/c26a_3_19_w4_tflptoken_1_3_of_its_lp_named_usd_xrp_live_after_testnet_21248899.json"));
}

/// 3-21 w4 tfLPToken 1/3 of its LP, named USD/XRP, live after (D30936200192, network tesSUCCESS).
#[test]
fn c26a_3_21_w4_tflptoken_1_3_of_its_lp_named_usd_xrp_live_after_testnet_21248904() {
    run_bundle(include_str!("vectors/c26a_3_21_w4_tflptoken_1_3_of_its_lp_named_usd_xrp_live_after_testnet_21248904.json"));
}

/// 3-0 w5 tfSingleAsset 300000 drops into USD pool (DEF9C46B59BD, network tesSUCCESS).
#[test]
fn c26a_3_0_w5_tfsingleasset_300000_drops_into_usd_pool_testnet_21248909() {
    run_bundle(include_str!("vectors/c26a_3_0_w5_tfsingleasset_300000_drops_into_usd_pool_testnet_21248909.json"));
}

/// 3-23 w5 tfSingleAsset 0.1 USD, pre-fee R(4)-1 (60CCABEDE230, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_23_w5_tfsingleasset_0_1_usd_pre_fee_r_4_1_testnet_21248917() {
    run_bundle(include_str!("vectors/c26a_3_23_w5_tfsingleasset_0_1_usd_pre_fee_r_4_1_testnet_21248917.json"));
}

/// 3-24 w5 tfOneAssetLPToken 1/4 LP as USD (min 0), pre-fee R (F3C56FD3C2D2, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_24_w5_tfoneassetlptoken_1_4_lp_as_usd_min_0_pre_fee_r_testnet_21248919() {
    run_bundle(include_str!("vectors/c26a_3_24_w5_tfoneassetlptoken_1_4_lp_as_usd_min_0_pre_fee_r_testnet_21248919.json"));
}

/// 3-25 w5 tfLimitLPToken USD min 0.01, EPrice = E(0.1 USD), (3BCE95DD37E8, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_25_w5_tflimitlptoken_usd_min_0_01_eprice_e_0_1_usd_testnet_21248921() {
    run_bundle(include_str!("vectors/c26a_3_25_w5_tflimitlptoken_usd_min_0_01_eprice_e_0_1_usd_testnet_21248921.json"));
}

/// 3-26 w5 tfOneAssetWithdrawAll as USD, pre-fee R(4)-1-3fee (89573D48C697, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_3_26_w5_tfoneassetwithdrawall_as_usd_pre_fee_r_4_1_3fee_testnet_21248923() {
    run_bundle(include_str!("vectors/c26a_3_26_w5_tfoneassetwithdrawall_as_usd_pre_fee_r_4_1_3fee_testnet_21248923.json"));
}

/// 3-28 w5 tfSingleAsset 0.1 USD, pre-fee R(4) exactly (2EC6E6DB096F, network tesSUCCESS).
#[test]
fn c26a_3_28_w5_tfsingleasset_0_1_usd_pre_fee_r_4_exactly_testnet_21248928() {
    run_bundle(include_str!("vectors/c26a_3_28_w5_tfsingleasset_0_1_usd_pre_fee_r_4_exactly_testnet_21248928.json"));
}

/// 4-1 lx tfLimitLPToken AAA (min 0) EPrice = T*f/A = 0.01 (d (6F66B30DC27B, network tecAMM_FAILED).
#[test]
fn c26a_4_1_lx_tflimitlptoken_aaa_min_0_eprice_t_f_a_0_01_d_testnet_21248938() {
    run_bundle(include_str!("vectors/c26a_4_1_lx_tflimitlptoken_aaa_min_0_eprice_t_f_a_0_01_d_testnet_21248938.json"));
}

/// 4-2 lx tfLimitLPToken AAA EPrice = E0 + 1 ulp (denom -1e-1 (C076E88BFDDE, network tecAMM_INVALID_TOKENS).
#[test]
fn c26a_4_2_lx_tflimitlptoken_aaa_eprice_e0_1_ulp_denom_1e_1_testnet_21248940() {
    run_bundle(include_str!("vectors/c26a_4_2_lx_tflimitlptoken_aaa_eprice_e0_1_ulp_denom_1e_1_testnet_21248940.json"));
}

/// 4-3 lx tfLimitLPToken AAA EPrice = E0 - 1 ulp (denom +1e-1 (6E06B6DC4976, network tecAMM_INVALID_TOKENS).
#[test]
fn c26a_4_3_lx_tflimitlptoken_aaa_eprice_e0_1_ulp_denom_1e_1_testnet_21248942() {
    run_bundle(include_str!("vectors/c26a_4_3_lx_tflimitlptoken_aaa_eprice_e0_1_ulp_denom_1e_1_testnet_21248942.json"));
}

/// 4-6 G4 tfLPToken 10 LP of its own AAA/BBB pool, pre-fee R( (A52CEB53CD22, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_4_6_g4_tflptoken_10_lp_of_its_own_aaa_bbb_pool_pre_fee_r_testnet_21248949() {
    run_bundle(include_str!("vectors/c26a_4_6_g4_tflptoken_10_lp_of_its_own_aaa_bbb_pool_pre_fee_r_testnet_21248949.json"));
}

/// 4-7 G4 tfSingleAsset 1 BBB, pre-fee R(4)-1-fee (453166493197, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_4_7_g4_tfsingleasset_1_bbb_pre_fee_r_4_1_fee_testnet_21248951() {
    run_bundle(include_str!("vectors/c26a_4_7_g4_tfsingleasset_1_bbb_pre_fee_r_4_1_fee_testnet_21248951.json"));
}

/// 4-9 G4 tfLPToken 10 LP, pre-fee R(4) (E7AFF7F0337B, network tesSUCCESS).
#[test]
fn c26a_4_9_g4_tflptoken_10_lp_pre_fee_r_4_testnet_21248955() {
    run_bundle(include_str!("vectors/c26a_4_9_g4_tflptoken_10_lp_pre_fee_r_4_testnet_21248955.json"));
}

/// 4-10 G2 (issuer) tfSingleAsset 2 TFX into PM (E81093D05E62, network tesSUCCESS).
#[test]
fn c26a_4_10_g2_issuer_tfsingleasset_2_tfx_into_pm_testnet_21248957() {
    run_bundle(include_str!("vectors/c26a_4_10_g2_issuer_tfsingleasset_2_tfx_into_pm_testnet_21248957.json"));
}

/// 4-13 G2 tfWithdrawAll PM: EUR line at OC3 (R(4) ok), then (316050B15950, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_4_13_g2_tfwithdrawall_pm_eur_line_at_oc3_r_4_ok_then_testnet_21248965() {
    run_bundle(include_str!("vectors/c26a_4_13_g2_tfwithdrawall_pm_eur_line_at_oc3_r_4_ok_then_testnet_21248965.json"));
}

/// 4-15 G2 tfWithdrawAll PM, pre-fee R(5) (D47987DFF293, network tesSUCCESS).
#[test]
fn c26a_4_15_g2_tfwithdrawall_pm_pre_fee_r_5_testnet_21248970() {
    run_bundle(include_str!("vectors/c26a_4_15_g2_tfwithdrawall_pm_pre_fee_r_5_testnet_21248970.json"));
}

/// 5-0 h1 tfSingleAsset 0.3 XRP into PC (no CLW line) (51712A429C24, network tesSUCCESS).
#[test]
fn c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976() {
    run_bundle(include_str!("vectors/c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976.json"));
}

/// Finding 413 — 5-3 G3 AMMClawback h1 partial 0.3 CLW (issuer prior R(10)- (C9AC881B76ED, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_5_3_g3_ammclawback_h1_partial_0_3_clw_issuer_prior_r_10_testnet_21248987() {
    run_bundle(include_str!("vectors/c26a_5_3_g3_ammclawback_h1_partial_0_3_clw_issuer_prior_r_10_testnet_21248987.json"));
}

/// Finding 413 — 5-5 G3 AMMClawback h1 all (issuer prior R(10)-1) (65ABA900E6D1, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26a_5_5_g3_ammclawback_h1_all_issuer_prior_r_10_1_testnet_21248992() {
    run_bundle(include_str!("vectors/c26a_5_5_g3_ammclawback_h1_all_issuer_prior_r_10_1_testnet_21248992.json"));
}

/// Finding 413 — 5-7 G3 AMMClawback h1 all (issuer prior R(10) exactly: the (636327F1935D, network tesSUCCESS).
#[test]
fn c26a_5_7_g3_ammclawback_h1_all_issuer_prior_r_10_exactly_the_testnet_21248997() {
    run_bundle(include_str!("vectors/c26a_5_7_g3_ammclawback_h1_all_issuer_prior_r_10_exactly_the_testnet_21248997.json"));
}

/// 5-8 h2 tfSingleAsset 0.2 XRP into PC (0BDE5D650227, network tesSUCCESS).
#[test]
fn c26a_5_8_h2_tfsingleasset_0_2_xrp_into_pc_testnet_21249001() {
    run_bundle(include_str!("vectors/c26a_5_8_h2_tfsingleasset_0_2_xrp_into_pc_testnet_21249001.json"));
}

/// 5-9 G3 AMMClawback h2 partial 0.2 CLW (holder OC 1) (31208F0386D9, network tesSUCCESS).
#[test]
fn c26a_5_9_g3_ammclawback_h2_partial_0_2_clw_holder_oc_1_testnet_21249003() {
    run_bundle(include_str!("vectors/c26a_5_9_g3_ammclawback_h2_partial_0_2_clw_holder_oc_1_testnet_21249003.json"));
}

/// 5-10 G3 AMMClawback h2 all (holder OC 1) (90B1AF2DE5D2, network tesSUCCESS).
#[test]
fn c26a_5_10_g3_ammclawback_h2_all_holder_oc_1_testnet_21249005() {
    run_bundle(include_str!("vectors/c26a_5_10_g3_ammclawback_h2_all_holder_oc_1_testnet_21249005.json"));
}

/// 5-12 G1 AMMClawback h5 tfClawTwoAssets PI named USD/EUR (h (4745E692B522, network tesSUCCESS).
#[test]
fn c26a_5_12_g1_ammclawback_h5_tfclawtwoassets_pi_named_usd_eur_h_testnet_21249014() {
    run_bundle(include_str!("vectors/c26a_5_12_g1_ammclawback_h5_tfclawtwoassets_pi_named_usd_eur_h_testnet_21249014.json"));
}

/// Finding 414 — 6-9 C1 tfSingleAsset withdraw 1 drop from PX (D8805B0F8AEC, network tesSUCCESS).
#[test]
fn c26a_6_9_c1_tfsingleasset_withdraw_1_drop_from_px_testnet_21249041() {
    run_bundle(include_str!("vectors/c26a_6_9_c1_tfsingleasset_withdraw_1_drop_from_px_testnet_21249041.json"));
}

/// Finding 414 — 6-10 C1 tfSingleAsset withdraw 0.000000000000001 USD from (442E190B01B5, network tecAMM_INVALID_TOKENS).
#[test]
fn c26a_6_10_c1_tfsingleasset_withdraw_0_000000000000001_usd_from_testnet_21249043() {
    run_bundle(include_str!("vectors/c26a_6_10_c1_tfsingleasset_withdraw_0_000000000000001_usd_from_testnet_21249043.json"));
}

/// 6-11 C1 tfLPToken 0.000000000001 LP from PX (FE18D56E52CD, network tecAMM_INVALID_TOKENS).
#[test]
fn c26a_6_11_c1_tflptoken_0_000000000001_lp_from_px_testnet_21249045() {
    run_bundle(include_str!("vectors/c26a_6_11_c1_tflptoken_0_000000000001_lp_from_px_testnet_21249045.json"));
}

/// Finding 414 — 6-12 C1 tfOneAssetLPToken 0.0000000000001 LP as XRP from P (05395596473C, network tecAMM_INVALID_TOKENS).
#[test]
fn c26a_6_12_c1_tfoneassetlptoken_0_0000000000001_lp_as_xrp_from_p_testnet_21249047() {
    run_bundle(include_str!("vectors/c26a_6_12_c1_tfoneassetlptoken_0_0000000000001_lp_as_xrp_from_p_testnet_21249047.json"));
}

