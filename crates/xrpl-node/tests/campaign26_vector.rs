//! Campaign 26 (testnet, 2026-10-03) byte-exact vectors: EDGE STATES rather than features — ordinary
//! transactions meeting a boundary, the shapes behind mainnet receipts F397-F411. Agent A: AMM deposits,
//! withdrawals and clawbacks with the account at its reserve (−1 drop / exact / +1), the issuer as LP,
//! pools named in the opposite order to the pool object's, dust withdrawals. Findings 412 (AMMDeposit's
//! non-LP reserve gate is preclaim's, on the pre-fee balance), 413 (AMMClawback's holder reserve test
//! before fixCleanup3_4_0, measured by the issuer's pre-fee balance) and 414 (single-asset withdrawals
//! whose tokens adjust to zero are refused; an asset that adjusts to zero is withdrawn as zero), all in
//! tx/amm.rs. Agent B: reserve edges outside AMM — ticket-paid transactions of every object-creating type
//! at the reserve (−1 / exact), multisigned variants, AccountDelete with a full owner directory — and 16-digit
//! rounding on long-mantissa IOU balances (CheckCash DeliverMin, partial payments, SendMax at the gross,
//! clawback remainders, NFT royalties with transfer rates, full trust lines, QualityIn/Out, IOU escrows).
//! Findings 415 (TrustSet) and 416 (EscrowCreate): doApply's reserve counts OwnerCount after the spending
//! Ticket is gone; we judge in preclaim and charged the ticket. Agent C: book quality levels spanning
//! pages (32/32/16, 64, 65, 80 offers) crossed so pages empty, the root relinks and the next crossing
//! starts from the relinked chain (F411's shape and its relatives), middle pages emptied by cancels,
//! AccountDelete, freezes, deep freeze and unfunding; expired and self-owned offers inside a level;
//! auto-bridged crossings over many flow passes; one pool beside a deep level — 358 of 358 byte-exact,
//! no finding. Testnet runs mainnet's rules (no testnet-only amendments; 16-digit Number). Same harness
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

/// tc1_ticketTC_cnt2_at_R(oc-1+2)-1 TicketCreate (F84FAC18E7D5, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_tc1_tickettc_cnt2_at_r_oc_1_2_1_testnet_21248756() {
    run_bundle(include_str!("vectors/c26b_tc1_tickettc_cnt2_at_r_oc_1_2_1_testnet_21248756.json"));
}

/// tc2_ticketTC_cnt2_at_R(oc-1+2)_exact TicketCreate (D6566EA56F37, network tesSUCCESS).
#[test]
fn c26b_tc2_tickettc_cnt2_at_r_oc_1_2_exact_testnet_21248760() {
    run_bundle(include_str!("vectors/c26b_tc2_tickettc_cnt2_at_r_oc_1_2_exact_testnet_21248760.json"));
}

/// tc3_seqTC_cnt2_at_R(oc+2)-1 TicketCreate (49617E417B65, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_tc3_seqtc_cnt2_at_r_oc_2_1_testnet_21248764() {
    run_bundle(include_str!("vectors/c26b_tc3_seqtc_cnt2_at_r_oc_2_1_testnet_21248764.json"));
}

/// tc4_seqTC_cnt2_at_R(oc+2)_exact TicketCreate (069512E943F7, network tesSUCCESS).
#[test]
fn c26b_tc4_seqtc_cnt2_at_r_oc_2_exact_testnet_21248768() {
    run_bundle(include_str!("vectors/c26b_tc4_seqtc_cnt2_at_r_oc_2_exact_testnet_21248768.json"));
}

/// pay1_ticketPay_at_amt+R(oc-1)-1 Payment (0175B15546F5, network tecUNFUNDED_PAYMENT).
#[test]
fn c26b_pay1_ticketpay_at_amt_r_oc_1_1_testnet_21248776() {
    run_bundle(include_str!("vectors/c26b_pay1_ticketpay_at_amt_r_oc_1_1_testnet_21248776.json"));
}

/// pay2_ticketPay_at_amt+R(oc-1)_exact Payment (83440314781C, network tesSUCCESS).
#[test]
fn c26b_pay2_ticketpay_at_amt_r_oc_1_exact_testnet_21248780() {
    run_bundle(include_str!("vectors/c26b_pay2_ticketpay_at_amt_r_oc_1_exact_testnet_21248780.json"));
}

/// pay3 Payment (1F1A94B8137F, network tecUNFUNDED_PAYMENT).
#[test]
fn c26b_pay3_testnet_21248786() {
    run_bundle(include_str!("vectors/c26b_pay3_testnet_21248786.json"));
}

/// pay4_Fee1.5XRP>reserve_at_amt+fee_exact Payment (0E21756A9AE4, network tesSUCCESS).
#[test]
fn c26b_pay4_fee1_5xrp_reserve_at_amt_fee_exact_testnet_21248790() {
    run_bundle(include_str!("vectors/c26b_pay4_fee1_5xrp_reserve_at_amt_fee_exact_testnet_21248790.json"));
}

/// pay5 Payment (3B7CAD770BE0, network tecUNFUNDED_PAYMENT).
#[test]
fn c26b_pay5_testnet_21248799() {
    run_bundle(include_str!("vectors/c26b_pay5_testnet_21248799.json"));
}

/// Finding 415 — ts1_ticketTrust_OC2->1_free-tier_at_1.2XRP TrustSet (30D7811B8169, network tesSUCCESS).
#[test]
fn c26b_ts1_tickettrust_oc2_1_free_tier_at_1_2xrp_testnet_21248810() {
    run_bundle(include_str!("vectors/c26b_ts1_tickettrust_oc2_1_free_tier_at_1_2xrp_testnet_21248810.json"));
}

/// Finding 415 — ts2_ticketTrust_new_line_at_R(oc)-1 TrustSet (C104194ECD48, network tecNO_LINE_INSUF_RESERVE).
#[test]
fn c26b_ts2_tickettrust_new_line_at_r_oc_1_testnet_21248818() {
    run_bundle(include_str!("vectors/c26b_ts2_tickettrust_new_line_at_r_oc_1_testnet_21248818.json"));
}

/// Finding 415 — ts3_ticketTrust_new_line_at_R(oc)_exact TrustSet (1BD8DCCABB0A, network tesSUCCESS).
#[test]
fn c26b_ts3_tickettrust_new_line_at_r_oc_exact_testnet_21248822() {
    run_bundle(include_str!("vectors/c26b_ts3_tickettrust_new_line_at_r_oc_exact_testnet_21248822.json"));
}

/// Finding 415 — ts4_ticketTrust_reserveIncrease_at_R(oc)-1 TrustSet (3F83871B35DC, network tecINSUF_RESERVE_LINE).
#[test]
fn c26b_ts4_tickettrust_reserveincrease_at_r_oc_1_testnet_21248832() {
    run_bundle(include_str!("vectors/c26b_ts4_tickettrust_reserveincrease_at_r_oc_1_testnet_21248832.json"));
}

/// Finding 415 — ts5_ticketTrust_reserveIncrease_at_R(oc)_exact TrustSet (DAA3DDC1AB96, network tesSUCCESS).
#[test]
fn c26b_ts5_tickettrust_reserveincrease_at_r_oc_exact_testnet_21248836() {
    run_bundle(include_str!("vectors/c26b_ts5_tickettrust_reserveincrease_at_r_oc_exact_testnet_21248836.json"));
}

/// ts6_seqTrust_at_R(oc+1)-1 TrustSet (DE3522863211, network tecNO_LINE_INSUF_RESERVE).
#[test]
fn c26b_ts6_seqtrust_at_r_oc_1_1_testnet_21248844() {
    run_bundle(include_str!("vectors/c26b_ts6_seqtrust_at_r_oc_1_1_testnet_21248844.json"));
}

/// ts7_seqTrust_at_R(oc+1)_exact TrustSet (3F3F0838B4A7, network tesSUCCESS).
#[test]
fn c26b_ts7_seqtrust_at_r_oc_1_exact_testnet_21248849() {
    run_bundle(include_str!("vectors/c26b_ts7_seqtrust_at_r_oc_1_exact_testnet_21248849.json"));
}

/// Finding 415 — ts8_msig_ticketTrust_at_R(oc)_exact TrustSet (4762026B61AC, network tesSUCCESS).
#[test]
fn c26b_ts8_msig_tickettrust_at_r_oc_exact_testnet_21248860() {
    run_bundle(include_str!("vectors/c26b_ts8_msig_tickettrust_at_r_oc_exact_testnet_21248860.json"));
}

/// Finding 415 — ts9_msig_ticketTrust_at_R(oc)-1 TrustSet (90007F00BF90, network tecNO_LINE_INSUF_RESERVE).
#[test]
fn c26b_ts9_msig_tickettrust_at_r_oc_1_testnet_21248865() {
    run_bundle(include_str!("vectors/c26b_ts9_msig_tickettrust_at_r_oc_1_testnet_21248865.json"));
}

/// Finding 416 — esc1_ticketEscrow_XRP_at_R(oc)+amt+fee-1 EscrowCreate (9C8CE34B48A0, network tecUNFUNDED).
#[test]
fn c26b_esc1_ticketescrow_xrp_at_r_oc_amt_fee_1_testnet_21248878() {
    run_bundle(include_str!("vectors/c26b_esc1_ticketescrow_xrp_at_r_oc_amt_fee_1_testnet_21248878.json"));
}

/// Finding 416 — esc2_ticketEscrow_XRP_at_R(oc)+amt+fee_exact EscrowCreate (2E52F2F7FB6B, network tesSUCCESS).
#[test]
fn c26b_esc2_ticketescrow_xrp_at_r_oc_amt_fee_exact_testnet_21248882() {
    run_bundle(include_str!("vectors/c26b_esc2_ticketescrow_xrp_at_r_oc_amt_fee_exact_testnet_21248882.json"));
}

/// Finding 416 — esc3_ticketEscrow_IOU_at_R(oc)+fee_exact EscrowCreate (8C653BC843A2, network tesSUCCESS).
#[test]
fn c26b_esc3_ticketescrow_iou_at_r_oc_fee_exact_testnet_21248887() {
    run_bundle(include_str!("vectors/c26b_esc3_ticketescrow_iou_at_r_oc_fee_exact_testnet_21248887.json"));
}

/// Finding 416 — esc4_ticketEscrow_IOU_at_R(oc)+fee-1 EscrowCreate (029E72F0FA41, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_esc4_ticketescrow_iou_at_r_oc_fee_1_testnet_21248892() {
    run_bundle(include_str!("vectors/c26b_esc4_ticketescrow_iou_at_r_oc_fee_1_testnet_21248892.json"));
}

/// Finding 416 — esc5_seqEscrow_XRP_at_R(oc+1)+amt+fee_exact EscrowCreate (B97BC997BC84, network tesSUCCESS).
#[test]
fn c26b_esc5_seqescrow_xrp_at_r_oc_1_amt_fee_exact_testnet_21248897() {
    run_bundle(include_str!("vectors/c26b_esc5_seqescrow_xrp_at_r_oc_1_amt_fee_exact_testnet_21248897.json"));
}

/// pc1_ticketPayChan_at_R(oc)+amt_(ticket_excluded) PaymentChannelCreate (ED4E9EC91AA2, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_pc1_ticketpaychan_at_r_oc_amt_ticket_excluded_testnet_21248907() {
    run_bundle(include_str!("vectors/c26b_pc1_ticketpaychan_at_r_oc_amt_ticket_excluded_testnet_21248907.json"));
}

/// pc2_ticketPayChan_at_R(oc+1)+amt-1 PaymentChannelCreate (EF0EF33B2204, network tecUNFUNDED).
#[test]
fn c26b_pc2_ticketpaychan_at_r_oc_1_amt_1_testnet_21248912() {
    run_bundle(include_str!("vectors/c26b_pc2_ticketpaychan_at_r_oc_1_amt_1_testnet_21248912.json"));
}

/// pc3_ticketPayChan_at_R(oc+1)+amt_exact PaymentChannelCreate (DF25B4663919, network tesSUCCESS).
#[test]
fn c26b_pc3_ticketpaychan_at_r_oc_1_amt_exact_testnet_21248917() {
    run_bundle(include_str!("vectors/c26b_pc3_ticketpaychan_at_r_oc_1_amt_exact_testnet_21248917.json"));
}

/// or1_ticketOracle_create_at_R(oc)_(ticket_exclude OracleSet (BDFC7B8785AE, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_or1_ticketoracle_create_at_r_oc_ticket_exclude_testnet_21248926() {
    run_bundle(include_str!("vectors/c26b_or1_ticketoracle_create_at_r_oc_ticket_exclude_testnet_21248926.json"));
}

/// or2_ticketOracle_create_at_R(oc+1)_exact OracleSet (CFE8100CCF1D, network tesSUCCESS).
#[test]
fn c26b_or2_ticketoracle_create_at_r_oc_1_exact_testnet_21248931() {
    run_bundle(include_str!("vectors/c26b_or2_ticketoracle_create_at_r_oc_1_exact_testnet_21248931.json"));
}

/// or3_ticketOracle_update_1->6_pairs_at_R(oc+1)-1 OracleSet (83861C4265AB, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_or3_ticketoracle_update_1_6_pairs_at_r_oc_1_1_testnet_21248936() {
    run_bundle(include_str!("vectors/c26b_or3_ticketoracle_update_1_6_pairs_at_r_oc_1_1_testnet_21248936.json"));
}

/// ob1_ticketCheckCreate_at_R(oc)_exact CheckCreate (5D833C6BFBD0, network tesSUCCESS).
#[test]
fn c26b_ob1_ticketcheckcreate_at_r_oc_exact_testnet_21248954() {
    run_bundle(include_str!("vectors/c26b_ob1_ticketcheckcreate_at_r_oc_exact_testnet_21248954.json"));
}

/// ob2_ticketDIDSet_at_R(oc)+fee-1_(post-fee) DIDSet (DAB59589964C, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_ob2_ticketdidset_at_r_oc_fee_1_post_fee_testnet_21248959() {
    run_bundle(include_str!("vectors/c26b_ob2_ticketdidset_at_r_oc_fee_1_post_fee_testnet_21248959.json"));
}

/// ob3_ticketDIDSet_at_R(oc)+fee_exact_(post-fee) DIDSet (9C54ABC0AD1F, network tesSUCCESS).
#[test]
fn c26b_ob3_ticketdidset_at_r_oc_fee_exact_post_fee_testnet_21248963() {
    run_bundle(include_str!("vectors/c26b_ob3_ticketdidset_at_r_oc_fee_exact_post_fee_testnet_21248963.json"));
}

/// ob4_ticketNFTokenMint_newpage_at_R(oc)-1 NFTokenMint (3D358698BF9B, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_ob4_ticketnftokenmint_newpage_at_r_oc_1_testnet_21248970() {
    run_bundle(include_str!("vectors/c26b_ob4_ticketnftokenmint_newpage_at_r_oc_1_testnet_21248970.json"));
}

/// ob5_ticketNFTokenMint_newpage_at_R(oc)_exact NFTokenMint (E541D3A2F945, network tesSUCCESS).
#[test]
fn c26b_ob5_ticketnftokenmint_newpage_at_r_oc_exact_testnet_21248974() {
    run_bundle(include_str!("vectors/c26b_ob5_ticketnftokenmint_newpage_at_r_oc_exact_testnet_21248974.json"));
}

/// ob6_ticketNFTokenMint_samepage_at_R(oc)-1_(no_re NFTokenMint (F456DF196592, network tesSUCCESS).
#[test]
fn c26b_ob6_ticketnftokenmint_samepage_at_r_oc_1_no_re_testnet_21248980() {
    run_bundle(include_str!("vectors/c26b_ob6_ticketnftokenmint_samepage_at_r_oc_1_no_re_testnet_21248980.json"));
}

/// ob7_ticketNFTokenCreateOffer_sell_at_R(oc)_exact NFTokenCreateOffer (E253AA29CD7B, network tesSUCCESS).
#[test]
fn c26b_ob7_ticketnftokencreateoffer_sell_at_r_oc_exact_testnet_21248984() {
    run_bundle(include_str!("vectors/c26b_ob7_ticketnftokencreateoffer_sell_at_r_oc_exact_testnet_21248984.json"));
}

/// ob8_ticketSignerListSet_at_R(oc)-1 SignerListSet (048956264FC0, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_ob8_ticketsignerlistset_at_r_oc_1_testnet_21248989() {
    run_bundle(include_str!("vectors/c26b_ob8_ticketsignerlistset_at_r_oc_1_testnet_21248989.json"));
}

/// ob9_ticketSignerListSet_at_R(oc)_exact SignerListSet (46529215AFB3, network tesSUCCESS).
#[test]
fn c26b_ob9_ticketsignerlistset_at_r_oc_exact_testnet_21248993() {
    run_bundle(include_str!("vectors/c26b_ob9_ticketsignerlistset_at_r_oc_exact_testnet_21248993.json"));
}

/// ob10_ticketSignerListSet_replace_at_R(oc-1) SignerListSet (42E0572A6FF4, network tesSUCCESS).
#[test]
fn c26b_ob10_ticketsignerlistset_replace_at_r_oc_1_testnet_21248997() {
    run_bundle(include_str!("vectors/c26b_ob10_ticketsignerlistset_replace_at_r_oc_1_testnet_21248997.json"));
}

/// ob11_ticketDepositPreauth_at_R(oc)_exact DepositPreauth (BB7F6DB570DF, network tesSUCCESS).
#[test]
fn c26b_ob11_ticketdepositpreauth_at_r_oc_exact_testnet_21249003() {
    run_bundle(include_str!("vectors/c26b_ob11_ticketdepositpreauth_at_r_oc_exact_testnet_21249003.json"));
}

/// ob12_ticketCredentialCreate_at_R(oc)-1 CredentialCreate (0E7066A42D52, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_ob12_ticketcredentialcreate_at_r_oc_1_testnet_21249009() {
    run_bundle(include_str!("vectors/c26b_ob12_ticketcredentialcreate_at_r_oc_1_testnet_21249009.json"));
}

/// ob13_ticketCredentialCreate_at_R(oc)_exact CredentialCreate (8FC642EDB15A, network tesSUCCESS).
#[test]
fn c26b_ob13_ticketcredentialcreate_at_r_oc_exact_testnet_21249013() {
    run_bundle(include_str!("vectors/c26b_ob13_ticketcredentialcreate_at_r_oc_exact_testnet_21249013.json"));
}

/// ob14_ticketCredentialAccept_at_R(oc)_exact CredentialAccept (2CEC259D7D9A, network tesSUCCESS).
#[test]
fn c26b_ob14_ticketcredentialaccept_at_r_oc_exact_testnet_21249018() {
    run_bundle(include_str!("vectors/c26b_ob14_ticketcredentialaccept_at_r_oc_exact_testnet_21249018.json"));
}

/// ob15_ticketMPTokenAuthorize_at_R(oc)-1 MPTokenAuthorize (58315889A6C0, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_ob15_ticketmptokenauthorize_at_r_oc_1_testnet_21249023() {
    run_bundle(include_str!("vectors/c26b_ob15_ticketmptokenauthorize_at_r_oc_1_testnet_21249023.json"));
}

/// ob16_ticketMPTokenAuthorize_at_R(oc)_exact MPTokenAuthorize (A531D1E5563D, network tesSUCCESS).
#[test]
fn c26b_ob16_ticketmptokenauthorize_at_r_oc_exact_testnet_21249027() {
    run_bundle(include_str!("vectors/c26b_ob16_ticketmptokenauthorize_at_r_oc_exact_testnet_21249027.json"));
}

/// ob2_tickets TicketCreate (7BE089EA9CD5, network tesSUCCESS).
#[test]
fn c26b_ob2_tickets_testnet_21249031() {
    run_bundle(include_str!("vectors/c26b_ob2_tickets_testnet_21249031.json"));
}

/// ob17_ticketOffer_own-IOU_at_R(oc)-1 OfferCreate (D0268C03194C, network tecINSUF_RESERVE_OFFER).
#[test]
fn c26b_ob17_ticketoffer_own_iou_at_r_oc_1_testnet_21249035() {
    run_bundle(include_str!("vectors/c26b_ob17_ticketoffer_own_iou_at_r_oc_1_testnet_21249035.json"));
}

/// ob18_ticketOffer_own-IOU_at_R(oc)_exact OfferCreate (A223B19B5880, network tesSUCCESS).
#[test]
fn c26b_ob18_ticketoffer_own_iou_at_r_oc_exact_testnet_21249039() {
    run_bundle(include_str!("vectors/c26b_ob18_ticketoffer_own_iou_at_r_oc_exact_testnet_21249039.json"));
}

/// ob19_ticketOffer_sellXRP_at_R(oc)_(preclaim_liqu OfferCreate (1D1FC7CFBF90, network tecUNFUNDED_OFFER).
#[test]
fn c26b_ob19_ticketoffer_sellxrp_at_r_oc_preclaim_liqu_testnet_21249044() {
    run_bundle(include_str!("vectors/c26b_ob19_ticketoffer_sellxrp_at_r_oc_preclaim_liqu_testnet_21249044.json"));
}

/// ob20_ticketOffer_sellXRP_at_R(oc)+1 OfferCreate (EA789177BD44, network tesSUCCESS).
#[test]
fn c26b_ob20_ticketoffer_sellxrp_at_r_oc_1_testnet_21249048() {
    run_bundle(include_str!("vectors/c26b_ob20_ticketoffer_sellxrp_at_r_oc_1_testnet_21249048.json"));
}

/// ob21_seqOffer_sellXRP_at_R(oc)+fee_(post-fee_liq OfferCreate (C669C10480FC, network tecUNFUNDED_OFFER).
#[test]
fn c26b_ob21_seqoffer_sellxrp_at_r_oc_fee_post_fee_liq_testnet_21249052() {
    run_bundle(include_str!("vectors/c26b_ob21_seqoffer_sellxrp_at_r_oc_fee_post_fee_liq_testnet_21249052.json"));
}

/// ob22_ticketMPTIssuanceCreate_at_R(oc)_exact MPTokenIssuanceCreate (4EE0393152D4, network tesSUCCESS).
#[test]
fn c26b_ob22_ticketmptissuancecreate_at_r_oc_exact_testnet_21249054() {
    run_bundle(include_str!("vectors/c26b_ob22_ticketmptissuancecreate_at_r_oc_exact_testnet_21249054.json"));
}

/// ob23_ticketNFTokenMint+Amount_samepage_at_R(oc)- NFTokenMint (65FAE6F36F58, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_ob23_ticketnftokenmint_amount_samepage_at_r_oc_testnet_21249059() {
    run_bundle(include_str!("vectors/c26b_ob23_ticketnftokenmint_amount_samepage_at_r_oc_testnet_21249059.json"));
}

/// mp1_ticketMPTokenAuthorize_OC2->1_free-tier_at_R MPTokenAuthorize (2A65F544438D, network tesSUCCESS).
#[test]
fn c26b_mp1_ticketmptokenauthorize_oc2_1_free_tier_at_r_testnet_21249069() {
    run_bundle(include_str!("vectors/c26b_mp1_ticketmptokenauthorize_oc2_1_free_tier_at_r_testnet_21249069.json"));
}

/// of1_tickets TicketCreate (FC60DBD9D38E, network tesSUCCESS).
#[test]
fn c26b_of1_tickets_testnet_21249079() {
    run_bundle(include_str!("vectors/c26b_of1_tickets_testnet_21249079.json"));
}

/// of1_seqOffer_cross+newline,_remainder_at_R(oc+2) OfferCreate (C04C35792264, network tesSUCCESS).
#[test]
fn c26b_of1_seqoffer_cross_newline_remainder_at_r_oc_2_testnet_21249084() {
    run_bundle(include_str!("vectors/c26b_of1_seqoffer_cross_newline_remainder_at_r_oc_2_testnet_21249084.json"));
}

/// of2_tickets TicketCreate (449581D35661, network tesSUCCESS).
#[test]
fn c26b_of2_tickets_testnet_21249089() {
    run_bundle(include_str!("vectors/c26b_of2_tickets_testnet_21249089.json"));
}

/// of2_seqOffer_cross+newline,_remainder_at_R(oc+2) OfferCreate (F282F4F22442, network tesSUCCESS).
#[test]
fn c26b_of2_seqoffer_cross_newline_remainder_at_r_oc_2_testnet_21249094() {
    run_bundle(include_str!("vectors/c26b_of2_seqoffer_cross_newline_remainder_at_r_oc_2_testnet_21249094.json"));
}

/// of3_tickets TicketCreate (FEB1986CB6C7, network tesSUCCESS).
#[test]
fn c26b_of3_tickets_testnet_21249098() {
    run_bundle(include_str!("vectors/c26b_of3_tickets_testnet_21249098.json"));
}

/// of3_ticketOffer_cross+newline,_remainder_at_R(oc OfferCreate (609F60FA5A22, network tesSUCCESS).
#[test]
fn c26b_of3_ticketoffer_cross_newline_remainder_at_r_oc_testnet_21249103() {
    run_bundle(include_str!("vectors/c26b_of3_ticketoffer_cross_newline_remainder_at_r_oc_testnet_21249103.json"));
}

/// ck1_CheckCash_0.5XRP_writer_at_R(oc-1)+v-1 CheckCash (600FEEABD17E, network tecPATH_PARTIAL).
#[test]
fn c26b_ck1_checkcash_0_5xrp_writer_at_r_oc_1_v_1_testnet_21249114() {
    run_bundle(include_str!("vectors/c26b_ck1_checkcash_0_5xrp_writer_at_r_oc_1_v_1_testnet_21249114.json"));
}

/// ck2_CheckCash_0.5XRP_writer_at_R(oc-1)+v_exact CheckCash (CD041AADCCB4, network tesSUCCESS).
#[test]
fn c26b_ck2_checkcash_0_5xrp_writer_at_r_oc_1_v_exact_testnet_21249119() {
    run_bundle(include_str!("vectors/c26b_ck2_checkcash_0_5xrp_writer_at_r_oc_1_v_exact_testnet_21249119.json"));
}

/// ck3_CheckCash_DeliverMin_writer_liquid_0.3XRP CheckCash (BC1F403F3BED, network tesSUCCESS).
#[test]
fn c26b_ck3_checkcash_delivermin_writer_liquid_0_3xrp_testnet_21249123() {
    run_bundle(include_str!("vectors/c26b_ck3_checkcash_delivermin_writer_liquid_0_3xrp_testnet_21249123.json"));
}

/// cl1_CheckCreate CheckCreate (1BA9C89EE90A, network tesSUCCESS).
#[test]
fn c26b_cl1_checkcreate_testnet_21249130() {
    run_bundle(include_str!("vectors/c26b_cl1_checkcreate_testnet_21249130.json"));
}

/// cl1_CheckCash_newline_at_R(oc+1)-1 CheckCash (24F67D114680, network tecNO_LINE_INSUF_RESERVE).
#[test]
fn c26b_cl1_checkcash_newline_at_r_oc_1_1_testnet_21249135() {
    run_bundle(include_str!("vectors/c26b_cl1_checkcash_newline_at_r_oc_1_1_testnet_21249135.json"));
}

/// cl2_CheckCash_newline_at_R(oc+1)_exact CheckCash (E191E4CC35A7, network tesSUCCESS).
#[test]
fn c26b_cl2_checkcash_newline_at_r_oc_1_exact_testnet_21249140() {
    run_bundle(include_str!("vectors/c26b_cl2_checkcash_newline_at_r_oc_1_exact_testnet_21249140.json"));
}

/// cl2_tickets TicketCreate (2798AA303B8A, network tesSUCCESS).
#[test]
fn c26b_cl2_tickets_testnet_21249145() {
    run_bundle(include_str!("vectors/c26b_cl2_tickets_testnet_21249145.json"));
}

/// cl3_CheckCreate CheckCreate (A94FBCC0ADA9, network tesSUCCESS).
#[test]
fn c26b_cl3_checkcreate_testnet_21249147() {
    run_bundle(include_str!("vectors/c26b_cl3_checkcreate_testnet_21249147.json"));
}

/// cl3_ticketCheckCash_newline_at_R(oc)_exact CheckCash (FD2B4E39816C, network tesSUCCESS).
#[test]
fn c26b_cl3_ticketcheckcash_newline_at_r_oc_exact_testnet_21249151() {
    run_bundle(include_str!("vectors/c26b_cl3_ticketcheckcash_newline_at_r_oc_exact_testnet_21249151.json"));
}

/// cl4_CheckCreate CheckCreate (647D53BD4D9D, network tesSUCCESS).
#[test]
fn c26b_cl4_checkcreate_testnet_21249156() {
    run_bundle(include_str!("vectors/c26b_cl4_checkcreate_testnet_21249156.json"));
}

/// cl4_CheckCash_newline_OC0_at_R(1)-1_(no_free_tie CheckCash (9D03CC9F8C1B, network tecNO_LINE_INSUF_RESERVE).
#[test]
fn c26b_cl4_checkcash_newline_oc0_at_r_1_1_no_free_tie_testnet_21249160() {
    run_bundle(include_str!("vectors/c26b_cl4_checkcash_newline_oc0_at_r_1_1_no_free_tie_testnet_21249160.json"));
}

/// ob3_tickets TicketCreate (D93F51B9B4E8, network tesSUCCESS).
#[test]
fn c26b_ob3_tickets_testnet_21249173() {
    run_bundle(include_str!("vectors/c26b_ob3_tickets_testnet_21249173.json"));
}

/// ob24_ticketNFTokenMint+Amount_samepage_at_R(oc)_ NFTokenMint (7DE534E47B66, network tesSUCCESS).
#[test]
fn c26b_ob24_ticketnftokenmint_amount_samepage_at_r_oc_testnet_21249177() {
    run_bundle(include_str!("vectors/c26b_ob24_ticketnftokenmint_amount_samepage_at_r_oc_testnet_21249177.json"));
}

/// del1_AccountDelete_seq_(tickets+list+offer+preau AccountDelete (7F6BD9A7B0B5, network tesSUCCESS).
#[test]
fn c26b_del1_accountdelete_seq_tickets_list_offer_preau_testnet_21249180() {
    run_bundle(include_str!("vectors/c26b_del1_accountdelete_seq_tickets_list_offer_preau_testnet_21249180.json"));
}

/// del2_AccountDelete_ticket-funded AccountDelete (29708098178F, network tesSUCCESS).
#[test]
fn c26b_del2_accountdelete_ticket_funded_testnet_21249182() {
    run_bundle(include_str!("vectors/c26b_del2_accountdelete_ticket_funded_testnet_21249182.json"));
}

/// rc0_issue_H1_CLW_11.24387575898908 Payment (1ACB5DF223FA, network tesSUCCESS).
#[test]
fn c26b_rc0_issue_h1_clw_11_24387575898908_testnet_21249184() {
    run_bundle(include_str!("vectors/c26b_rc0_issue_h1_clw_11_24387575898908_testnet_21249184.json"));
}

/// rc0_issue_H2_CLW Payment (31A5F9FA9BF0, network tesSUCCESS).
#[test]
fn c26b_rc0_issue_h2_clw_testnet_21249186() {
    run_bundle(include_str!("vectors/c26b_rc0_issue_h2_clw_testnet_21249186.json"));
}

/// rc0_issue_H2_RND Payment (FE594C8305A0, network tesSUCCESS).
#[test]
fn c26b_rc0_issue_h2_rnd_testnet_21249188() {
    run_bundle(include_str!("vectors/c26b_rc0_issue_h2_rnd_testnet_21249188.json"));
}

/// rc0_issue_H1_RND Payment (D6158D45C8CC, network tesSUCCESS).
#[test]
fn c26b_rc0_issue_h1_rnd_testnet_21249190() {
    run_bundle(include_str!("vectors/c26b_rc0_issue_h1_rnd_testnet_21249190.json"));
}

/// rc1_CheckCreate CheckCreate (01A407299E30, network tesSUCCESS).
#[test]
fn c26b_rc1_checkcreate_testnet_21249192() {
    run_bundle(include_str!("vectors/c26b_rc1_checkcreate_testnet_21249192.json"));
}

/// rc1_CheckCash_DeliverMin=998.8_onto_11.243875758 CheckCash (B08D69F1DBB7, network tesSUCCESS).
#[test]
fn c26b_rc1_checkcash_delivermin_998_8_onto_11_243875758_testnet_21249194() {
    run_bundle(include_str!("vectors/c26b_rc1_checkcash_delivermin_998_8_onto_11_243875758_testnet_21249194.json"));
}

/// rc2_CheckCreate CheckCreate (F93E4EA1AD88, network tesSUCCESS).
#[test]
fn c26b_rc2_checkcreate_testnet_21249196() {
    run_bundle(include_str!("vectors/c26b_rc2_checkcreate_testnet_21249196.json"));
}

/// rc2_CheckCash_Amount=0.1234567890123456_onto_101 CheckCash (2A7590DD0EA1, network tesSUCCESS).
#[test]
fn c26b_rc2_checkcash_amount_0_1234567890123456_onto_101_testnet_21249198() {
    run_bundle(include_str!("vectors/c26b_rc2_checkcash_amount_0_1234567890123456_onto_101_testnet_21249198.json"));
}

/// rc3_CheckCreate CheckCreate (EBF159413A24, network tesSUCCESS).
#[test]
fn c26b_rc3_checkcreate_testnet_21249200() {
    run_bundle(include_str!("vectors/c26b_rc3_checkcreate_testnet_21249200.json"));
}

/// rc3_CheckCash_Amount_1.23456789e-11_onto_1010.16 CheckCash (542EFFC59374, network tesSUCCESS).
#[test]
fn c26b_rc3_checkcash_amount_1_23456789e_11_onto_1010_16_testnet_21249203() {
    run_bundle(include_str!("vectors/c26b_rc3_checkcash_amount_1_23456789e_11_onto_1010_16_testnet_21249203.json"));
}

/// rc4_CheckCreate CheckCreate (0449D4ABD85E, network tesSUCCESS).
#[test]
fn c26b_rc4_checkcreate_testnet_21249205() {
    run_bundle(include_str!("vectors/c26b_rc4_checkcreate_testnet_21249205.json"));
}

/// rc4_CheckCash_RND_DeliverMin=1_(observe_delivera CheckCash (28C06858D1D1, network tesSUCCESS).
#[test]
fn c26b_rc4_checkcash_rnd_delivermin_1_observe_delivera_testnet_21249207() {
    run_bundle(include_str!("vectors/c26b_rc4_checkcash_rnd_delivermin_1_observe_delivera_testnet_21249207.json"));
}

/// rc5_CheckCreate CheckCreate (87366EC0FE50, network tesSUCCESS).
#[test]
fn c26b_rc5_checkcreate_testnet_21249209() {
    run_bundle(include_str!("vectors/c26b_rc5_checkcreate_testnet_21249209.json"));
}

/// rc5_CheckCash_RND_DeliverMin=deliverable+1ulp CheckCash (A59B9532FDBA, network tecPATH_PARTIAL).
#[test]
fn c26b_rc5_checkcash_rnd_delivermin_deliverable_1ulp_testnet_21249211() {
    run_bundle(include_str!("vectors/c26b_rc5_checkcash_rnd_delivermin_deliverable_1ulp_testnet_21249211.json"));
}

/// rc6_CheckCash_RND_DeliverMin=deliverable_exact CheckCash (3B9ACA9A0D5E, network tesSUCCESS).
#[test]
fn c26b_rc6_checkcash_rnd_delivermin_deliverable_exact_testnet_21249213() {
    run_bundle(include_str!("vectors/c26b_rc6_checkcash_rnd_delivermin_deliverable_exact_testnet_21249213.json"));
}

/// rc7_CheckCreate CheckCreate (F62B03075A84, network tesSUCCESS).
#[test]
fn c26b_rc7_checkcreate_testnet_21249215() {
    run_bundle(include_str!("vectors/c26b_rc7_checkcreate_testnet_21249215.json"));
}

/// rc7_CheckCash_RND_Amount=deliverable_(gross_must CheckCash (BB645C2FCA1B, network tesSUCCESS).
#[test]
fn c26b_rc7_checkcash_rnd_amount_deliverable_gross_must_testnet_21249217() {
    run_bundle(include_str!("vectors/c26b_rc7_checkcash_rnd_amount_deliverable_gross_must_testnet_21249217.json"));
}

/// rc8_CheckCreate CheckCreate (D992D9CF0707, network tesSUCCESS).
#[test]
fn c26b_rc8_checkcreate_testnet_21249219() {
    run_bundle(include_str!("vectors/c26b_rc8_checkcreate_testnet_21249219.json"));
}

/// rc8_CheckCash_RND_Amount=deliverable+1ulp CheckCash (527C2C0F5F57, network tecPATH_PARTIAL).
#[test]
fn c26b_rc8_checkcash_rnd_amount_deliverable_1ulp_testnet_21249221() {
    run_bundle(include_str!("vectors/c26b_rc8_checkcash_rnd_amount_deliverable_1ulp_testnet_21249221.json"));
}

/// rp1_partial_SendMax_777.77.._(observe_delivered) Payment (9CAB9D25CCAE, network tesSUCCESS).
#[test]
fn c26b_rp1_partial_sendmax_777_77_observe_delivered_testnet_21249223() {
    run_bundle(include_str!("vectors/c26b_rp1_partial_sendmax_777_77_observe_delivered_testnet_21249223.json"));
}

/// rp3_partial_DeliverMin=delivered_exact Payment (CBF047CE7ABE, network tesSUCCESS).
#[test]
fn c26b_rp3_partial_delivermin_delivered_exact_testnet_21249227() {
    run_bundle(include_str!("vectors/c26b_rp3_partial_delivermin_delivered_exact_testnet_21249227.json"));
}

/// rp4_pay_0.3333333333333333_SendMax_1_(observe_gr Payment (9354DD71F03E, network tesSUCCESS).
#[test]
fn c26b_rp4_pay_0_3333333333333333_sendmax_1_observe_gr_testnet_21249229() {
    run_bundle(include_str!("vectors/c26b_rp4_pay_0_3333333333333333_sendmax_1_observe_gr_testnet_21249229.json"));
}

/// rp5_pay_same_SendMax=gross_exact Payment (118BBA9C3E60, network tesSUCCESS).
#[test]
fn c26b_rp5_pay_same_sendmax_gross_exact_testnet_21249231() {
    run_bundle(include_str!("vectors/c26b_rp5_pay_same_sendmax_gross_exact_testnet_21249231.json"));
}

/// rp6_pay_same_SendMax=gross-1ulp Payment (8CB2069C5EF1, network tesSUCCESS).
#[test]
fn c26b_rp6_pay_same_sendmax_gross_1ulp_testnet_21249233() {
    run_bundle(include_str!("vectors/c26b_rp6_pay_same_sendmax_gross_1ulp_testnet_21249233.json"));
}

/// rp7_pay_9999.999999999999_through_G1 Payment (18BBD49CA65A, network tesSUCCESS).
#[test]
fn c26b_rp7_pay_9999_999999999999_through_g1_testnet_21249235() {
    run_bundle(include_str!("vectors/c26b_rp7_pay_9999_999999999999_through_g1_testnet_21249235.json"));
}

/// rw0_issue_H3_200000.0000000005 Payment (CA0267A08488, network tesSUCCESS).
#[test]
fn c26b_rw0_issue_h3_200000_0000000005_testnet_21249237() {
    run_bundle(include_str!("vectors/c26b_rw0_issue_h3_200000_0000000005_testnet_21249237.json"));
}

/// rw1_claw_200000_of_200000.0000000005_(F403) Clawback (CC73CD51A332, network tesSUCCESS).
#[test]
fn c26b_rw1_claw_200000_of_200000_0000000005_f403_testnet_21249239() {
    run_bundle(include_str!("vectors/c26b_rw1_claw_200000_of_200000_0000000005_f403_testnet_21249239.json"));
}

/// rw0_issue_H4_9999999999.999999 Payment (DA61E72FD9F9, network tesSUCCESS).
#[test]
fn c26b_rw0_issue_h4_9999999999_999999_testnet_21249241() {
    run_bundle(include_str!("vectors/c26b_rw0_issue_h4_9999999999_999999_testnet_21249241.json"));
}

/// rw2_claw_9999999999.999998_of_9999999999.999999 Clawback (98A3EB3B80EC, network tesSUCCESS).
#[test]
fn c26b_rw2_claw_9999999999_999998_of_9999999999_999999_testnet_21249244() {
    run_bundle(include_str!("vectors/c26b_rw2_claw_9999999999_999998_of_9999999999_999999_testnet_21249244.json"));
}

/// rw0_issue_H4_1e9_onto_0.000001 Payment (1230C01CFC55, network tesSUCCESS).
#[test]
fn c26b_rw0_issue_h4_1e9_onto_0_000001_testnet_21249246() {
    run_bundle(include_str!("vectors/c26b_rw0_issue_h4_1e9_onto_0_000001_testnet_21249246.json"));
}

/// rw3_claw_1e-8_of_1000000000.000001_(below_16_dig Clawback (0487B65B8170, network tesSUCCESS).
#[test]
fn c26b_rw3_claw_1e_8_of_1000000000_000001_below_16_dig_testnet_21249248() {
    run_bundle(include_str!("vectors/c26b_rw3_claw_1e_8_of_1000000000_000001_below_16_dig_testnet_21249248.json"));
}

/// rw4_claw_0.0000005_of_1000000000.000001 Clawback (2732E5723263, network tesSUCCESS).
#[test]
fn c26b_rw4_claw_0_0000005_of_1000000000_000001_testnet_21249250() {
    run_bundle(include_str!("vectors/c26b_rw4_claw_0_0000005_of_1000000000_000001_testnet_21249250.json"));
}

/// rp8_P_pays_0.3333333333333333_from_exactly_1_(ob Payment (61B28BC52F57, network tesSUCCESS).
#[test]
fn c26b_rp8_p_pays_0_3333333333333333_from_exactly_1_ob_testnet_21249269() {
    run_bundle(include_str!("vectors/c26b_rp8_p_pays_0_3333333333333333_from_exactly_1_ob_testnet_21249269.json"));
}

/// rp9_SendMax=gross-1ulp Payment (4CA088206F7B, network tesSUCCESS).
#[test]
fn c26b_rp9_sendmax_gross_1ulp_testnet_21249273() {
    run_bundle(include_str!("vectors/c26b_rp9_sendmax_gross_1ulp_testnet_21249273.json"));
}

/// rp10_SendMax=gross_exact Payment (87E55652D2DD, network tesSUCCESS).
#[test]
fn c26b_rp10_sendmax_gross_exact_testnet_21249275() {
    run_bundle(include_str!("vectors/c26b_rp10_sendmax_gross_exact_testnet_21249275.json"));
}

/// rn0_issue_r3JDbW_RND Payment (9BB1F999A013, network tesSUCCESS).
#[test]
fn c26b_rn0_issue_r3jdbw_rnd_testnet_21249277() {
    run_bundle(include_str!("vectors/c26b_rn0_issue_r3jdbw_rnd_testnet_21249277.json"));
}

/// rn0_issue_rGGXCC_RND Payment (329B34539F6F, network tesSUCCESS).
#[test]
fn c26b_rn0_issue_rggxcc_rnd_testnet_21249279() {
    run_bundle(include_str!("vectors/c26b_rn0_issue_rggxcc_rnd_testnet_21249279.json"));
}

/// rn0_issue_rHEZjx_RND Payment (6D5F743DFFE5, network tesSUCCESS).
#[test]
fn c26b_rn0_issue_rhezjx_rnd_testnet_21249281() {
    run_bundle(include_str!("vectors/c26b_rn0_issue_rhezjx_rnd_testnet_21249281.json"));
}

/// rn0_issue_rBprTM_RND Payment (30DF52F07EB2, network tesSUCCESS).
#[test]
fn c26b_rn0_issue_rbprtm_rnd_testnet_21249283() {
    run_bundle(include_str!("vectors/c26b_rn0_issue_rbprtm_rnd_testnet_21249283.json"));
}

/// rn1_mint_TransferFee_3333 NFTokenMint (E37182D99438, network tesSUCCESS).
#[test]
fn c26b_rn1_mint_transferfee_3333_testnet_21249285() {
    run_bundle(include_str!("vectors/c26b_rn1_mint_transferfee_3333_testnet_21249285.json"));
}

/// rn2_M_sells_211299.0477462831 NFTokenCreateOffer (1E3635A655CA, network tesSUCCESS).
#[test]
fn c26b_rn2_m_sells_211299_0477462831_testnet_21249287() {
    run_bundle(include_str!("vectors/c26b_rn2_m_sells_211299_0477462831_testnet_21249287.json"));
}

/// rn3_H1_accepts_(issuer_seller,_rate_grossing) NFTokenAcceptOffer (B4C484B4A54A, network tesSUCCESS).
#[test]
fn c26b_rn3_h1_accepts_issuer_seller_rate_grossing_testnet_21249289() {
    run_bundle(include_str!("vectors/c26b_rn3_h1_accepts_issuer_seller_rate_grossing_testnet_21249289.json"));
}

/// rn4_H1_sells_98765.43210987654 NFTokenCreateOffer (E8C7C0B06596, network tesSUCCESS).
#[test]
fn c26b_rn4_h1_sells_98765_43210987654_testnet_21249291() {
    run_bundle(include_str!("vectors/c26b_rn4_h1_sells_98765_43210987654_testnet_21249291.json"));
}

/// rn5_H2_accepts_(royalty_3.333__+_rate) NFTokenAcceptOffer (B5BBB914749A, network tesSUCCESS).
#[test]
fn c26b_rn5_h2_accepts_royalty_3_333_rate_testnet_21249294() {
    run_bundle(include_str!("vectors/c26b_rn5_h2_accepts_royalty_3_333_rate_testnet_21249294.json"));
}

/// rn6_H3_buy_offer_12345.67890123456 NFTokenCreateOffer (E01C0E838B9E, network tesSUCCESS).
#[test]
fn c26b_rn6_h3_buy_offer_12345_67890123456_testnet_21249296() {
    run_bundle(include_str!("vectors/c26b_rn6_h3_buy_offer_12345_67890123456_testnet_21249296.json"));
}

/// rn7_H2_accepts_buy_offer NFTokenAcceptOffer (0A394C012C81, network tesSUCCESS).
#[test]
fn c26b_rn7_h2_accepts_buy_offer_testnet_21249298() {
    run_bundle(include_str!("vectors/c26b_rn7_h2_accepts_buy_offer_testnet_21249298.json"));
}

/// rn8_H3_sells_1000.000000000001 NFTokenCreateOffer (E70E516D9C3A, network tesSUCCESS).
#[test]
fn c26b_rn8_h3_sells_1000_000000000001_testnet_21249300() {
    run_bundle(include_str!("vectors/c26b_rn8_h3_sells_1000_000000000001_testnet_21249300.json"));
}

/// rn9_H4_buy_offer_1234.567890123457 NFTokenCreateOffer (7EDBAF7523C1, network tesSUCCESS).
#[test]
fn c26b_rn9_h4_buy_offer_1234_567890123457_testnet_21249302() {
    run_bundle(include_str!("vectors/c26b_rn9_h4_buy_offer_1234_567890123457_testnet_21249302.json"));
}

/// rn10_H1_brokers_fee_0.1234567890123456 NFTokenAcceptOffer (60A1F318AB27, network tesSUCCESS).
#[test]
fn c26b_rn10_h1_brokers_fee_0_1234567890123456_testnet_21249304() {
    run_bundle(include_str!("vectors/c26b_rn10_h1_brokers_fee_0_1234567890123456_testnet_21249304.json"));
}

/// rl0_H4_RND_limit_1000.000000000001 TrustSet (F61F2E146519, network tesSUCCESS).
#[test]
fn c26b_rl0_h4_rnd_limit_1000_000000000001_testnet_21249306() {
    run_bundle(include_str!("vectors/c26b_rl0_h4_rnd_limit_1000_000000000001_testnet_21249306.json"));
}

/// rl0_H4_returns_RND Payment (3B656098EA2F, network tesSUCCESS).
#[test]
fn c26b_rl0_h4_returns_rnd_testnet_21249308() {
    run_bundle(include_str!("vectors/c26b_rl0_h4_returns_rnd_testnet_21249308.json"));
}

/// rl0_issue_H4_0.1234567890123456 Payment (219CDF0AFF99, network tesSUCCESS).
#[test]
fn c26b_rl0_issue_h4_0_1234567890123456_testnet_21249310() {
    run_bundle(include_str!("vectors/c26b_rl0_issue_h4_0_1234567890123456_testnet_21249310.json"));
}

/// rl1_issuer_pays_room+1ulp Payment (98BD2423EB44, network tecPATH_PARTIAL).
#[test]
fn c26b_rl1_issuer_pays_room_1ulp_testnet_21249312() {
    run_bundle(include_str!("vectors/c26b_rl1_issuer_pays_room_1ulp_testnet_21249312.json"));
}

/// rl2_issuer_pays_round16(room) Payment (0548ED91BB89, network tesSUCCESS).
#[test]
fn c26b_rl2_issuer_pays_round16_room_testnet_21249314() {
    run_bundle(include_str!("vectors/c26b_rl2_issuer_pays_round16_room_testnet_21249314.json"));
}

/// rl3_H2_pays_1e-12_into_full_line_(via_G1,_fee) Payment (A46B7E963708, network tecPATH_DRY).
#[test]
fn c26b_rl3_h2_pays_1e_12_into_full_line_via_g1_fee_testnet_21249316() {
    run_bundle(include_str!("vectors/c26b_rl3_h2_pays_1e_12_into_full_line_via_g1_fee_testnet_21249316.json"));
}

/// rl4_H2_partial_into_full_line Payment (80C868ED84BC, network tecPATH_DRY).
#[test]
fn c26b_rl4_h2_partial_into_full_line_testnet_21249319() {
    run_bundle(include_str!("vectors/c26b_rl4_h2_partial_into_full_line_testnet_21249319.json"));
}

/// rq0_H3_QualityIn_0.95 TrustSet (818F48A50DDD, network tesSUCCESS).
#[test]
fn c26b_rq0_h3_qualityin_0_95_testnet_21249321() {
    run_bundle(include_str!("vectors/c26b_rq0_h3_qualityin_0_95_testnet_21249321.json"));
}

/// rq0_H2_QualityOut_1.05 TrustSet (4129FDA9EFA6, network tesSUCCESS).
#[test]
fn c26b_rq0_h2_qualityout_1_05_testnet_21249323() {
    run_bundle(include_str!("vectors/c26b_rq0_h2_qualityout_1_05_testnet_21249323.json"));
}

/// rq3_H3->H1_partial_SendMax_0.1111111111111111 Payment (F27F83132A74, network tesSUCCESS).
#[test]
fn c26b_rq3_h3_h1_partial_sendmax_0_1111111111111111_testnet_21249329() {
    run_bundle(include_str!("vectors/c26b_rq3_h3_h1_partial_sendmax_0_1111111111111111_testnet_21249329.json"));
}

/// rq4_H2->G1_redeem_3.141592653589793_(QOut_1.05) Payment (AAF832BE9910, network tesSUCCESS).
#[test]
fn c26b_rq4_h2_g1_redeem_3_141592653589793_qout_1_05_testnet_21249331() {
    run_bundle(include_str!("vectors/c26b_rq4_h2_g1_redeem_3_141592653589793_qout_1_05_testnet_21249331.json"));
}

/// re1_H1_escrows_1234.567890123456_RND_to_H2 EscrowCreate (1E4EED19D02A, network tesSUCCESS).
#[test]
fn c26b_re1_h1_escrows_1234_567890123456_rnd_to_h2_testnet_21249333() {
    run_bundle(include_str!("vectors/c26b_re1_h1_escrows_1234_567890123456_rnd_to_h2_testnet_21249333.json"));
}

/// re2_H1_escrows_0.9999999999999999_RND_to_H2 EscrowCreate (3C2A95ED22BC, network tesSUCCESS).
#[test]
fn c26b_re2_h1_escrows_0_9999999999999999_rnd_to_h2_testnet_21249335() {
    run_bundle(include_str!("vectors/c26b_re2_h1_escrows_0_9999999999999999_rnd_to_h2_testnet_21249335.json"));
}

/// re3_H1_escrows_777.7777777777777_RND_to_H2 EscrowCreate (03A1CFD1F79F, network tesSUCCESS).
#[test]
fn c26b_re3_h1_escrows_777_7777777777777_rnd_to_h2_testnet_21249337() {
    run_bundle(include_str!("vectors/c26b_re3_h1_escrows_777_7777777777777_rnd_to_h2_testnet_21249337.json"));
}

/// re4_H2_finishes__1_(rate_1.001234567_locked) EscrowFinish (44C730E987B9, network tesSUCCESS).
#[test]
fn c26b_re4_h2_finishes_1_rate_1_001234567_locked_testnet_21249341() {
    run_bundle(include_str!("vectors/c26b_re4_h2_finishes_1_rate_1_001234567_locked_testnet_21249341.json"));
}

/// re5_H1_(owner)_finishes__2_0.9999999999999999 EscrowFinish (8914F8B79F1D, network tesSUCCESS).
#[test]
fn c26b_re5_h1_owner_finishes_2_0_9999999999999999_testnet_21249343() {
    run_bundle(include_str!("vectors/c26b_re5_h1_owner_finishes_2_0_9999999999999999_testnet_21249343.json"));
}

/// re6_G1_lowers_TransferRate_to_1.000000001 AccountSet (F18729BD9412, network tesSUCCESS).
#[test]
fn c26b_re6_g1_lowers_transferrate_to_1_000000001_testnet_21249345() {
    run_bundle(include_str!("vectors/c26b_re6_g1_lowers_transferrate_to_1_000000001_testnet_21249345.json"));
}

/// re7_H2_finishes__3_at_the_lower_rate EscrowFinish (E19A9741B551, network tesSUCCESS).
#[test]
fn c26b_re7_h2_finishes_3_at_the_lower_rate_testnet_21249347() {
    run_bundle(include_str!("vectors/c26b_re7_h2_finishes_3_at_the_lower_rate_testnet_21249347.json"));
}

/// re8_G1_restores_TransferRate AccountSet (42AC000BA814, network tesSUCCESS).
#[test]
fn c26b_re8_g1_restores_transferrate_testnet_21249349() {
    run_bundle(include_str!("vectors/c26b_re8_g1_restores_transferrate_testnet_21249349.json"));
}

/// rw5_claw_1e16_(more_than_held)_->_all Clawback (9CA7756882AE, network tesSUCCESS).
#[test]
fn c26b_rw5_claw_1e16_more_than_held_all_testnet_21249378() {
    run_bundle(include_str!("vectors/c26b_rw5_claw_1e16_more_than_held_all_testnet_21249378.json"));
}

/// rw6_claw_0.0000000005_remainder_exactly Clawback (14368FE1F21D, network tesSUCCESS).
#[test]
fn c26b_rw6_claw_0_0000000005_remainder_exactly_testnet_21249380() {
    run_bundle(include_str!("vectors/c26b_rw6_claw_0_0000000005_remainder_exactly_testnet_21249380.json"));
}

/// ofc1_ticketOffer+cancel_own_offer_at_R(oc-1-canc OfferCreate (98FFA83B5062, network tecINSUF_RESERVE_OFFER).
#[test]
fn c26b_ofc1_ticketoffer_cancel_own_offer_at_r_oc_1_canc_testnet_21249392() {
    run_bundle(include_str!("vectors/c26b_ofc1_ticketoffer_cancel_own_offer_at_r_oc_1_canc_testnet_21249392.json"));
}

/// ofc2_ticketOffer+cancel_own_offer_at_R(oc-1-canc OfferCreate (74DF2C4739AB, network tesSUCCESS).
#[test]
fn c26b_ofc2_ticketoffer_cancel_own_offer_at_r_oc_1_canc_testnet_21249396() {
    run_bundle(include_str!("vectors/c26b_ofc2_ticketoffer_cancel_own_offer_at_r_oc_1_canc_testnet_21249396.json"));
}

/// pd1_ticketPermissionedDomainSet_at_R(oc)+fee-1 PermissionedDomainSet (F81E6E7A85D3, network tecINSUFFICIENT_RESERVE).
#[test]
fn c26b_pd1_ticketpermissioneddomainset_at_r_oc_fee_1_testnet_21249405() {
    run_bundle(include_str!("vectors/c26b_pd1_ticketpermissioneddomainset_at_r_oc_fee_1_testnet_21249405.json"));
}

/// pd2_ticketPermissionedDomainSet_at_R(oc)+fee_exa PermissionedDomainSet (F5FE6DACAD23, network tesSUCCESS).
#[test]
fn c26b_pd2_ticketpermissioneddomainset_at_r_oc_fee_exa_testnet_21249410() {
    run_bundle(include_str!("vectors/c26b_pd2_ticketpermissioneddomainset_at_r_oc_fee_exa_testnet_21249410.json"));
}

/// nb1_ticketAccept_liquid(price)_counts_ticket:_R( NFTokenAcceptOffer (7189A19DEAE7, network tecINSUFFICIENT_FUNDS).
#[test]
fn c26b_nb1_ticketaccept_liquid_price_counts_ticket_r_testnet_21249422() {
    run_bundle(include_str!("vectors/c26b_nb1_ticketaccept_liquid_price_counts_ticket_r_testnet_21249422.json"));
}

/// nb2_ticketAccept_newpage_post-fee-post-price_at_ NFTokenAcceptOffer (0E37F468C776, network tesSUCCESS).
#[test]
fn c26b_nb2_ticketaccept_newpage_post_fee_post_price_at_testnet_21249426() {
    run_bundle(include_str!("vectors/c26b_nb2_ticketaccept_newpage_post_fee_post_price_at_testnet_21249426.json"));
}

/// p1.x1.plain_buy32_empties_page0_exactly OfferCreate (855E7EF0A002, network tesSUCCESS).
#[test]
fn c26c_p1_x1_plain_buy32_empties_page0_exactly_testnet_21248717() {
    run_bundle(include_str!("vectors/c26c_p1_x1_plain_buy32_empties_page0_exactly_testnet_21248717.json"));
}

/// p1.x2.ioc_buy1_walks_empty_root_into_page1 OfferCreate (6843BC725880, network tesSUCCESS).
#[test]
fn c26c_p1_x2_ioc_buy1_walks_empty_root_into_page1_testnet_21248719() {
    run_bundle(include_str!("vectors/c26c_p1_x2_ioc_buy1_walks_empty_root_into_page1_testnet_21248719.json"));
}

/// p1.x3.sell_ioc_36_empties_page1_relinks_root_into_page2 OfferCreate (E4E5BC3A1EAB, network tesSUCCESS).
#[test]
fn c26c_p1_x3_sell_ioc_36_empties_page1_relinks_root_into_page2_testnet_21248721() {
    run_bundle(include_str!("vectors/c26c_p1_x3_sell_ioc_36_empties_page1_relinks_root_into_page2_testnet_21248721.json"));
}

/// p1.x4.fok_buy20_finishes_A100_then_A101 OfferCreate (E4C300B808E5, network tesSUCCESS).
#[test]
fn c26c_p1_x4_fok_buy20_finishes_a100_then_a101_testnet_21248723() {
    run_bundle(include_str!("vectors/c26c_p1_x4_fok_buy20_finishes_a100_then_a101_testnet_21248723.json"));
}

/// p1.x5.fok_buy30_killed_24_left OfferCreate (F07F4345667B, network tecKILLED).
#[test]
fn c26c_p1_x5_fok_buy30_killed_24_left_testnet_21248725() {
    run_bundle(include_str!("vectors/c26c_p1_x5_fok_buy30_killed_24_left_testnet_21248725.json"));
}

/// p1.x6.passive_at_equal_quality_rests OfferCreate (23EA6EBF370A, network tesSUCCESS).
#[test]
fn c26c_p1_x6_passive_at_equal_quality_rests_testnet_21248727() {
    run_bundle(include_str!("vectors/c26c_p1_x6_passive_at_equal_quality_rests_testnet_21248727.json"));
}

/// p1.x7.passive_better_limit_crosses_A101 OfferCreate (A3C2136F249E, network tesSUCCESS).
#[test]
fn c26c_p1_x7_passive_better_limit_crosses_a101_testnet_21248731() {
    run_bundle(include_str!("vectors/c26c_p1_x7_passive_better_limit_crosses_a101_testnet_21248731.json"));
}

/// p1.x8.F411_replica_sell_ioc_70_pages0and1_into_page2 OfferCreate (9E3E8D066CD6, network tesSUCCESS).
#[test]
fn c26c_p1_x8_f411_replica_sell_ioc_70_pages0and1_into_page2_testnet_21248755() {
    run_bundle(include_str!("vectors/c26c_p1_x8_f411_replica_sell_ioc_70_pages0and1_into_page2_testnet_21248755.json"));
}

/// p1.x9.ioc_buy5_crosses_relinked_state OfferCreate (967345615AD4, network tesSUCCESS).
#[test]
fn c26c_p1_x9_ioc_buy5_crosses_relinked_state_testnet_21248757() {
    run_bundle(include_str!("vectors/c26c_p1_x9_ioc_buy5_crosses_relinked_state_testnet_21248757.json"));
}

/// p1.x10.payment_xrp_to_usd_deletes_A100_midpath Payment (EE80C8829290, network tesSUCCESS).
#[test]
fn c26c_p1_x10_payment_xrp_to_usd_deletes_a100_midpath_testnet_21248759() {
    run_bundle(include_str!("vectors/c26c_p1_x10_payment_xrp_to_usd_deletes_a100_midpath_testnet_21248759.json"));
}

/// p2.x1.sell_exactly64_empties_pages0and1_root_kept_relinked_to_page2 OfferCreate (33270FC4451A, network tesSUCCESS).
#[test]
fn c26c_p2_x1_sell_exactly64_empties_pages0and1_root_kept_relinked_to_page2_testnet_21248798() {
    run_bundle(include_str!("vectors/c26c_p2_x1_sell_exactly64_empties_pages0and1_root_kept_relinked_to_page2_testnet_21248798.json"));
}

/// p2.x2.ioc_buy3_empty_root_to_page2 OfferCreate (AF29B659AF37, network tesSUCCESS).
#[test]
fn c26c_p2_x2_ioc_buy3_empty_root_to_page2_testnet_21248805() {
    run_bundle(include_str!("vectors/c26c_p2_x2_ioc_buy3_empty_root_to_page2_testnet_21248805.json"));
}

/// p2.x3.plain_buy67_deletes_A098_and_A099_whole OfferCreate (20B9558CB510, network tesSUCCESS).
#[test]
fn c26c_p2_x3_plain_buy67_deletes_a098_and_a099_whole_testnet_21248807() {
    run_bundle(include_str!("vectors/c26c_p2_x3_plain_buy67_deletes_a098_and_a099_whole_testnet_21248807.json"));
}

/// p2.x4.cancel_the_33rd_deletes_page1_zeroes_root_links OfferCancel (8DD7934529B2, network tesSUCCESS).
#[test]
fn c26c_p2_x4_cancel_the_33rd_deletes_page1_zeroes_root_links_testnet_21248820() {
    run_bundle(include_str!("vectors/c26c_p2_x4_cancel_the_33rd_deletes_page1_zeroes_root_links_testnet_21248820.json"));
}

/// p2.x5.fok_buy34_killed_33_at_level OfferCreate (EF4B9D96F624, network tecKILLED).
#[test]
fn c26c_p2_x5_fok_buy34_killed_33_at_level_testnet_21248826() {
    run_bundle(include_str!("vectors/c26c_p2_x5_fok_buy34_killed_33_at_level_testnet_21248826.json"));
}

/// p2.x6.ioc_buy33_deletes_both_pages OfferCreate (1E4D20E81A9D, network tesSUCCESS).
#[test]
fn c26c_p2_x6_ioc_buy33_deletes_both_pages_testnet_21248828() {
    run_bundle(include_str!("vectors/c26c_p2_x6_ioc_buy33_deletes_both_pages_testnet_21248828.json"));
}

/// p3.x1.payment_xrp_usd_35_page0_then_relinked_page2 Payment (B26ED60414FA, network tesSUCCESS).
#[test]
fn c26c_p3_x1_payment_xrp_usd_35_page0_then_relinked_page2_testnet_21248859() {
    run_bundle(include_str!("vectors/c26c_p3_x1_payment_xrp_usd_35_page0_then_relinked_page2_testnet_21248859.json"));
}

/// p3.x2.cancel_one_in_middle_of_last_page OfferCancel (0BB79FD461D3, network tesSUCCESS).
#[test]
fn c26c_p3_x2_cancel_one_in_middle_of_last_page_testnet_21248861() {
    run_bundle(include_str!("vectors/c26c_p3_x2_cancel_one_in_middle_of_last_page_testnet_21248861.json"));
}

/// p3.x3.partial_limitquality_payment_takes_rest_delivermin Payment (1A7149FF9614, network tesSUCCESS).
#[test]
fn c26c_p3_x3_partial_limitquality_payment_takes_rest_delivermin_testnet_21248863() {
    run_bundle(include_str!("vectors/c26c_p3_x3_partial_limitquality_payment_takes_rest_delivermin_testnet_21248863.json"));
}

/// p4.f.accountdelete_mx_deletes_middle_page_of_B140 AccountDelete (CEC5847F847E, network tesSUCCESS).
#[test]
fn c26c_p4_f_accountdelete_mx_deletes_middle_page_of_b140_testnet_21248865() {
    run_bundle(include_str!("vectors/c26c_p4_f_accountdelete_mx_deletes_middle_page_of_b140_testnet_21248865.json"));
}

/// p4.f.x.sell_usd_40_crosses_B140_root_then_relinked_page2 OfferCreate (6A560AB792DE, network tesSUCCESS).
#[test]
fn c26c_p4_f_x_sell_usd_40_crosses_b140_root_then_relinked_page2_testnet_21248868() {
    run_bundle(include_str!("vectors/c26c_p4_f_x_sell_usd_40_crosses_b140_root_then_relinked_page2_testnet_21248868.json"));
}

/// p4.a.x.ioc_buy40_frozen_middle_page_reaped OfferCreate (48380F1CA885, network tesSUCCESS).
#[test]
fn c26c_p4_a_x_ioc_buy40_frozen_middle_page_reaped_testnet_21248894() {
    run_bundle(include_str!("vectors/c26c_p4_a_x_ioc_buy40_frozen_middle_page_reaped_testnet_21248894.json"));
}

/// p4.b.x.payment_xrp_usd_40_through_unfunded_page1 Payment (48B5CB2630D0, network tesSUCCESS).
#[test]
fn c26c_p4_b_x_payment_xrp_usd_40_through_unfunded_page1_testnet_21248923() {
    run_bundle(include_str!("vectors/c26c_p4_b_x_payment_xrp_usd_40_through_unfunded_page1_testnet_21248923.json"));
}

/// p4.c.x.sell_fok_40_through_no_line_page1 OfferCreate (5E2288E4DB01, network tesSUCCESS).
#[test]
fn c26c_p4_c_x_sell_fok_40_through_no_line_page1_testnet_21248953() {
    run_bundle(include_str!("vectors/c26c_p4_c_x_sell_fok_40_through_no_line_page1_testnet_21248953.json"));
}

/// p4.d.x.sell_usd_40_through_deepfrozen_page1 OfferCreate (C3387367EC99, network tesSUCCESS).
#[test]
fn c26c_p4_d_x_sell_usd_40_through_deepfrozen_page1_testnet_21248984() {
    run_bundle(include_str!("vectors/c26c_p4_d_x_sell_usd_40_through_deepfrozen_page1_testnet_21248984.json"));
}

/// p4.e.x.sell_usd_through_reserve_bound_xrp_maker_line_created OfferCreate (51953186B7A7, network tesSUCCESS).
#[test]
fn c26c_p4_e_x_sell_usd_through_reserve_bound_xrp_maker_line_created_testnet_21249011() {
    run_bundle(include_str!("vectors/c26c_p4_e_x_sell_usd_through_reserve_bound_xrp_maker_line_created_testnet_21249011.json"));
}

/// p5.x1.sell_across_three_multipage_levels_expired_and_own_offers OfferCreate (2E20461BAEEA, network tesSUCCESS).
#[test]
fn c26c_p5_x1_sell_across_three_multipage_levels_expired_and_own_offers_testnet_21249058() {
    run_bundle(include_str!("vectors/c26c_p5_x1_sell_across_three_multipage_levels_expired_and_own_offers_testnet_21249058.json"));
}

/// p5.x2.passive_ioc_buy_at_A091_equal_quality_crosses_nothing OfferCreate (0B7A2A357956, network tecKILLED).
#[test]
fn c26c_p5_x2_passive_ioc_buy_at_a091_equal_quality_crosses_nothing_testnet_21249060() {
    run_bundle(include_str!("vectors/c26c_p5_x2_passive_ioc_buy_at_a091_equal_quality_crosses_nothing_testnet_21249060.json"));
}

/// p5.x3.passive_ioc_buy_strictly_better_crosses_A091 OfferCreate (AF938B4A4D20, network tesSUCCESS).
#[test]
fn c26c_p5_x3_passive_ioc_buy_strictly_better_crosses_a091_testnet_21249062() {
    run_bundle(include_str!("vectors/c26c_p5_x3_passive_ioc_buy_strictly_better_crosses_a091_testnet_21249062.json"));
}

/// p6.x1.autobridged_sell_usd_for_eur_bridge_then_direct_pages0_1_into_2 OfferCreate (6AEBBDBFCC68, network tesSUCCESS).
#[test]
fn c26c_p6_x1_autobridged_sell_usd_for_eur_bridge_then_direct_pages0_1_into_2_testnet_21249107() {
    run_bundle(include_str!("vectors/c26c_p6_x1_autobridged_sell_usd_for_eur_bridge_then_direct_pages0_1_into_2_testnet_21249107.json"));
}

/// p6.x2.payment_usd_eur_xrp_path_partial_delivermin Payment (88DF3D48A9B1, network tesSUCCESS).
#[test]
fn c26c_p6_x2_payment_usd_eur_xrp_path_partial_delivermin_testnet_21249109() {
    run_bundle(include_str!("vectors/c26c_p6_x2_payment_usd_eur_xrp_path_partial_delivermin_testnet_21249109.json"));
}

/// p6.x3.payment_partial_below_delivermin Payment (D6A14738BF0A, network tecPATH_PARTIAL).
#[test]
fn c26c_p6_x3_payment_partial_below_delivermin_testnet_21249123() {
    run_bundle(include_str!("vectors/c26c_p6_x3_payment_partial_below_delivermin_testnet_21249123.json"));
}

/// p6.x4.payment_exact_33_deletes_both_pages Payment (F32348D4351C, network tesSUCCESS).
#[test]
fn c26c_p6_x4_payment_exact_33_deletes_both_pages_testnet_21249125() {
    run_bundle(include_str!("vectors/c26c_p6_x4_payment_exact_33_deletes_both_pages_testnet_21249125.json"));
}

/// p7.x1.ioc_buy30_limit_at_book_quality_pool_beside_book OfferCreate (7121B69E6187, network tesSUCCESS).
#[test]
fn c26c_p7_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249146() {
    run_bundle(include_str!("vectors/c26c_p7_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249146.json"));
}

/// p7.x2.sell_ioc_at_book_quality_pool_beside_book OfferCreate (B731139CB7BC, network tecKILLED).
#[test]
fn c26c_p7_x2_sell_ioc_at_book_quality_pool_beside_book_testnet_21249148() {
    run_bundle(include_str!("vectors/c26c_p7_x2_sell_ioc_at_book_quality_pool_beside_book_testnet_21249148.json"));
}

/// p7.x3.payment_xrp_usd_through_pool_and_book Payment (E20023102251, network tecPATH_PARTIAL).
#[test]
fn c26c_p7_x3_payment_xrp_usd_through_pool_and_book_testnet_21249150() {
    run_bundle(include_str!("vectors/c26c_p7_x3_payment_xrp_usd_through_pool_and_book_testnet_21249150.json"));
}

/// p8.x1.ioc_buy_killed_reaps_frozen_and_expired_across_pages OfferCreate (58D7280FF123, network tecKILLED).
#[test]
fn c26c_p8_x1_ioc_buy_killed_reaps_frozen_and_expired_across_pages_testnet_21249179() {
    run_bundle(include_str!("vectors/c26c_p8_x1_ioc_buy_killed_reaps_frozen_and_expired_across_pages_testnet_21249179.json"));
}

/// p8.x2.ioc_buy3_funded_crosses_root_then_relinked_page OfferCreate (EC0573B0114D, network tesSUCCESS).
#[test]
fn c26c_p8_x2_ioc_buy3_funded_crosses_root_then_relinked_page_testnet_21249181() {
    run_bundle(include_str!("vectors/c26c_p8_x2_ioc_buy3_funded_crosses_root_then_relinked_page_testnet_21249181.json"));
}

/// p7b.x1.ioc_buy30_limit_at_book_quality_pool_beside_book OfferCreate (8E1741A00A9E, network tesSUCCESS).
#[test]
fn c26c_p7b_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249238() {
    run_bundle(include_str!("vectors/c26c_p7b_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249238.json"));
}

/// p7b.x2.sell_ioc_at_book_quality_pool_tied_with_tip OfferCreate (43B08C5F0F40, network tesSUCCESS).
#[test]
fn c26c_p7b_x2_sell_ioc_at_book_quality_pool_tied_with_tip_testnet_21249240() {
    run_bundle(include_str!("vectors/c26c_p7b_x2_sell_ioc_at_book_quality_pool_tied_with_tip_testnet_21249240.json"));
}

/// p7b.x3.payment_xrp_usd_through_pool_and_book Payment (84A7C262D0DB, network tesSUCCESS).
#[test]
fn c26c_p7b_x3_payment_xrp_usd_through_pool_and_book_testnet_21249242() {
    run_bundle(include_str!("vectors/c26c_p7b_x3_payment_xrp_usd_through_pool_and_book_testnet_21249242.json"));
}

/// p7b.x4.payment_partial_limitquality_at_book_quality Payment (0C55E6170537, network tesSUCCESS).
#[test]
fn c26c_p7b_x4_payment_partial_limitquality_at_book_quality_testnet_21249244() {
    run_bundle(include_str!("vectors/c26c_p7b_x4_payment_partial_limitquality_at_book_quality_testnet_21249244.json"));
}

/// p9.a.x.ioc_buy64_root_then_page1_with_explicit_zero_next OfferCreate (7D151251A643, network tesSUCCESS).
#[test]
fn c26c_p9_a_x_ioc_buy64_root_then_page1_with_explicit_zero_next_testnet_21249268() {
    run_bundle(include_str!("vectors/c26c_p9_a_x_ioc_buy64_root_then_page1_with_explicit_zero_next_testnet_21249268.json"));
}

/// p9.b.x.ioc_buy98_crosses_C096_and_C0965_whole OfferCreate (2F4F040E6DC8, network tesSUCCESS).
#[test]
fn c26c_p9_b_x_ioc_buy98_crosses_c096_and_c0965_whole_testnet_21249304() {
    run_bundle(include_str!("vectors/c26c_p9_b_x_ioc_buy98_crosses_c096_and_c0965_whole_testnet_21249304.json"));
}

/// p9.c.x.ioc_buy3_reaps_unfunded_root_and_page1_crosses_page2 OfferCreate (79F212FEB983, network tesSUCCESS).
#[test]
fn c26c_p9_c_x_ioc_buy3_reaps_unfunded_root_and_page1_crosses_page2_testnet_21249330() {
    run_bundle(include_str!("vectors/c26c_p9_c_x_ioc_buy3_reaps_unfunded_root_and_page1_crosses_page2_testnet_21249330.json"));
}

/// p9.d.x.sell_fok_xrp_for_eur_70_pages0and1_into_page2 OfferCreate (2EE2CFE5580A, network tesSUCCESS).
#[test]
fn c26c_p9_d_x_sell_fok_xrp_for_eur_70_pages0and1_into_page2_testnet_21249357() {
    run_bundle(include_str!("vectors/c26c_p9_d_x_sell_fok_xrp_for_eur_70_pages0and1_into_page2_testnet_21249357.json"));
}

/// p9.d.x2.payment_xrp_eur_4_on_relinked_state Payment (F5DC9A610C0E, network tesSUCCESS).
#[test]
fn c26c_p9_d_x2_payment_xrp_eur_4_on_relinked_state_testnet_21249359() {
    run_bundle(include_str!("vectors/c26c_p9_d_x2_payment_xrp_eur_4_on_relinked_state_testnet_21249359.json"));
}

/// p10.a.x.F411_shape_sell_ioc_pages0and1_relinked_unfunded_page2 OfferCreate (F8C4956A89A8, network tecKILLED).
#[test]
fn c26c_p10_a_x_f411_shape_sell_ioc_pages0and1_relinked_unfunded_page2_testnet_21249501() {
    run_bundle(include_str!("vectors/c26c_p10_a_x_f411_shape_sell_ioc_pages0and1_relinked_unfunded_page2_testnet_21249501.json"));
}

/// p10.b.x.ioc_buy38_dust_funded_offer_at_page_boundary OfferCreate (7ED482125FD5, network tesSUCCESS).
#[test]
fn c26c_p10_b_x_ioc_buy38_dust_funded_offer_at_page_boundary_testnet_21249524() {
    run_bundle(include_str!("vectors/c26c_p10_b_x_ioc_buy38_dust_funded_offer_at_page_boundary_testnet_21249524.json"));
}

/// p10.b.x2.sell_ioc_finishes_C093 OfferCreate (2FFCAD60A143, network tesSUCCESS).
#[test]
fn c26c_p10_b_x2_sell_ioc_finishes_c093_testnet_21249526() {
    run_bundle(include_str!("vectors/c26c_p10_b_x2_sell_ioc_finishes_c093_testnet_21249526.json"));
}

/// p10c.x.F411_shape_sell_ioc_90usd_pool_and_pages0_1_relinked_unfunded_page2 OfferCreate (2B09015DF49C, network tesSUCCESS).
#[test]
fn c26c_p10c_x_f411_shape_sell_ioc_90usd_pool_and_pages0_1_relinked_unfunded_pag_testnet_21249576() {
    run_bundle(include_str!("vectors/c26c_p10c_x_f411_shape_sell_ioc_90usd_pool_and_pages0_1_relinked_unfunded_pag_testnet_21249576.json"));
}

/// p11.a.x1.ioc_buy32_empties_root OfferCreate (4325F32A3632, network tesSUCCESS).
#[test]
fn c26c_p11_a_x1_ioc_buy32_empties_root_testnet_21250263() {
    run_bundle(include_str!("vectors/c26c_p11_a_x1_ioc_buy32_empties_root_testnet_21250263.json"));
}

/// p11.a.x2.fok_buy33_page1_and_new_page2_behind_empty_root OfferCreate (5CED2B19A59D, network tesSUCCESS).
#[test]
fn c26c_p11_a_x2_fok_buy33_page1_and_new_page2_behind_empty_root_testnet_21250269() {
    run_bundle(include_str!("vectors/c26c_p11_a_x2_fok_buy33_page1_and_new_page2_behind_empty_root_testnet_21250269.json"));
}

/// p11.b.x.sell_eur_ioc_40_through_one_drop_funded_maker OfferCreate (362432293C46, network tesSUCCESS).
#[test]
fn c26c_p11_b_x_sell_eur_ioc_40_through_one_drop_funded_maker_testnet_21250288() {
    run_bundle(include_str!("vectors/c26c_p11_b_x_sell_eur_ioc_40_through_one_drop_funded_maker_testnet_21250288.json"));
}

/// p11.c.x.ioc_buy36_transfer_fee_funds_exactly_3_of_4 OfferCreate (65BAE35A1709, network tesSUCCESS).
#[test]
fn c26c_p11_c_x_ioc_buy36_transfer_fee_funds_exactly_3_of_4_testnet_21250311() {
    run_bundle(include_str!("vectors/c26c_p11_c_x_ioc_buy36_transfer_fee_funds_exactly_3_of_4_testnet_21250311.json"));
}

/// p11.d.x.sell_ioc_through_issuer_offers_middle_page OfferCreate (AAE2D71FEA58, network tesSUCCESS).
#[test]
fn c26c_p11_d_x_sell_ioc_through_issuer_offers_middle_page_testnet_21250334() {
    run_bundle(include_str!("vectors/c26c_p11_d_x_sell_ioc_through_issuer_offers_middle_page_testnet_21250334.json"));
}

/// p11e.x.plain_buy27_empty_root_issuer_page_then_last_page OfferCreate (EE8EE9BE45A2, network tesSUCCESS).
#[test]
fn c26c_p11e_x_plain_buy27_empty_root_issuer_page_then_last_page_testnet_21250348() {
    run_bundle(include_str!("vectors/c26c_p11e_x_plain_buy27_empty_root_issuer_page_then_last_page_testnet_21250348.json"));
}

/// p12.x.autobridged_buy80_gbp_six_iterations_over_one_multipage_eur_xrp_level OfferCreate (F690075FDE40, network tesSUCCESS).
#[test]
fn c26c_p12_x_autobridged_buy80_gbp_six_iterations_over_one_multipage_eur_xrp_le_testnet_21250773() {
    run_bundle(include_str!("vectors/c26c_p12_x_autobridged_buy80_gbp_six_iterations_over_one_multipage_eur_xrp_le_testnet_21250773.json"));
}
