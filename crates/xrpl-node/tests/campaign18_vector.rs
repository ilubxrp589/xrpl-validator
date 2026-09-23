//! Campaign 18 (testnet, 2026-09-23) byte-exact vectors: MPT depth — every
//! issuance flag, authorization, transfer-fee payments, locks, clawback,
//! escrow, destroy, reserve edges. Findings 370-373 (fee arithmetic through
//! Number, legacy divideRound on escrow, IssuanceSet without CanLock, Clawback
//! from an AMM account) plus the MPT payment/clawback read-set gap. Same
//! harness as did_vector.rs.
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

/// Finding 370 — 3-2 h1->h2 41 A, no SendMax (41.5125 -> 42 > 41) (5D0ADEF1C05D, network tecPATH_PARTIAL).
#[test]
fn c18_3_2_h1_h2_41_a_no_sendmax_41_5125_42_41_testnet_20976385() {
    run_bundle(include_str!("vectors/c18_3_2_h1_h2_41_a_no_sendmax_41_5125_42_41_testnet_20976385.json"));
}

/// Finding 370 — 3-3 h1->h2 41 A, SendMax 42 (56C3099F0BDC, network tesSUCCESS).
#[test]
fn c18_3_3_h1_h2_41_a_sendmax_42_testnet_20976387() {
    run_bundle(include_str!("vectors/c18_3_3_h1_h2_41_a_sendmax_42_testnet_20976387.json"));
}

/// Finding 370 — 3-4 h1->h2 120 A, SendMax 122 (121.5 -> 122 even) (CB22E31CE70F, network tesSUCCESS).
#[test]
fn c18_3_4_h1_h2_120_a_sendmax_122_121_5_122_even_testnet_20976389() {
    run_bundle(include_str!("vectors/c18_3_4_h1_h2_120_a_sendmax_122_121_5_122_even_testnet_20976389.json"));
}

/// Finding 370 — 3-6 h1->h2 Amount 1000 A, SendMax 500, partial (E5BE38B4BE4B, network tesSUCCESS).
#[test]
fn c18_3_6_h1_h2_amount_1000_a_sendmax_500_partial_testnet_20976393() {
    run_bundle(include_str!("vectors/c18_3_6_h1_h2_amount_1000_a_sendmax_500_partial_testnet_20976393.json"));
}

/// Finding 370 — 3-7 h1->h2 Amount 1000 A, SendMax 500, DeliverMin 494, partial (E3651777091F, network tesSUCCESS).
#[test]
fn c18_3_7_h1_h2_amount_1000_a_sendmax_500_delivermin_494_partial_testnet_20976395() {
    run_bundle(include_str!("vectors/c18_3_7_h1_h2_amount_1000_a_sendmax_500_delivermin_494_partial_testnet_20976395.json"));
}

/// Finding 370 — 3-12 h1->h2 1 B, no SendMax (1.5 -> 2) (72EE8CC822BE, network tecPATH_PARTIAL).
#[test]
fn c18_3_12_h1_h2_1_b_no_sendmax_1_5_2_testnet_20976405() {
    run_bundle(include_str!("vectors/c18_3_12_h1_h2_1_b_no_sendmax_1_5_2_testnet_20976405.json"));
}

/// Finding 370 — 3-13 h1->h2 5 B, SendMax 8 (7.5 -> 8) (E3AFEEF93616, network tesSUCCESS).
#[test]
fn c18_3_13_h1_h2_5_b_sendmax_8_7_5_8_testnet_20976407() {
    run_bundle(include_str!("vectors/c18_3_13_h1_h2_5_b_sendmax_8_7_5_8_testnet_20976407.json"));
}

/// Finding 370 — 3-14 h1->h2 Amount 100 B, SendMax 11, partial (CC6BC83D5B53, network tesSUCCESS).
#[test]
fn c18_3_14_h1_h2_amount_100_b_sendmax_11_partial_testnet_20976409() {
    run_bundle(include_str!("vectors/c18_3_14_h1_h2_amount_100_b_sendmax_11_partial_testnet_20976409.json"));
}

/// Finding 370 — 3-15 h1->h2 9876543210987655 B, SendMax 14814814816481483 (620B87608393, network tesSUCCESS).
#[test]
fn c18_3_15_h1_h2_9876543210987655_b_sendmax_14814814816481483_testnet_20976411() {
    run_bundle(include_str!("vectors/c18_3_15_h1_h2_9876543210987655_b_sendmax_14814814816481483_testnet_20976411.json"));
}

/// Finding 370 — 3-19 h1->h2 Amount 100 E, SendMax 3, partial (C758F61DB7F1, network tesSUCCESS).
#[test]
fn c18_3_19_h1_h2_amount_100_e_sendmax_3_partial_testnet_20976419() {
    run_bundle(include_str!("vectors/c18_3_19_h1_h2_amount_100_e_sendmax_3_partial_testnet_20976419.json"));
}

/// Finding 372 — 4-19 i1 IssuanceSet Flags 0 on C (no CanLock) (6F5A4B7BDA4B, network tecNO_PERMISSION).
#[test]
fn c18_4_19_i1_issuanceset_flags_0_on_c_no_canlock_testnet_20976484() {
    run_bundle(include_str!("vectors/c18_4_19_i1_issuanceset_flags_0_on_c_no_canlock_testnet_20976484.json"));
}

/// Finding 373 — 5-10 i1 claws 5 A from an AMM account (A6F33689C9FF, network tecAMM_ACCOUNT).
#[test]
fn c18_5_10_i1_claws_5_a_from_an_amm_account_testnet_20976517() {
    run_bundle(include_str!("vectors/c18_5_10_i1_claws_5_a_from_an_amm_account_testnet_20976517.json"));
}

/// Finding 373 — 9-1 i1 claws 1 USD (IOU) from the AMM account (i1 lacks AllowTrustLineClawback) (EB4860F45631, network tecAMM_ACCOUNT).
#[test]
fn c18_9_1_i1_claws_1_usd_iou_from_the_amm_account_i1_lacks_allowtrustl_testnet_20976751() {
    run_bundle(include_str!("vectors/c18_9_1_i1_claws_1_usd_iou_from_the_amm_account_i1_lacks_allowtrustl_testnet_20976751.json"));
}

/// Finding 371 — 6-12 h2 finishes the 73 A escrow (BA70BD7E2120, network tesSUCCESS).
#[test]
fn c18_6_12_h2_finishes_the_73_a_escrow_testnet_20976552() {
    run_bundle(include_str!("vectors/c18_6_12_h2_finishes_the_73_a_escrow_testnet_20976552.json"));
}

/// Finding 371 — 6-14 h2 finishes the 1.5e15 B escrow (25388CC9D991, network tesSUCCESS).
#[test]
fn c18_6_14_h2_finishes_the_1_5e15_b_escrow_testnet_20976556() {
    run_bundle(include_str!("vectors/c18_6_14_h2_finishes_the_1_5e15_b_escrow_testnet_20976556.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 2-10 h1 pays h3 100 A (h3 unauthorized) (55142C1F45A3, network tecNO_AUTH).
#[test]
fn c18_2_10_h1_pays_h3_100_a_h3_unauthorized_testnet_20976365() {
    run_bundle(include_str!("vectors/c18_2_10_h1_pays_h3_100_a_h3_unauthorized_testnet_20976365.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 2-2 i1 mints 100 A to h1 (not yet authorized) (7406835DA36E, network tecNO_AUTH).
#[test]
fn c18_2_2_i1_mints_100_a_to_h1_not_yet_authorized_testnet_20976349() {
    run_bundle(include_str!("vectors/c18_2_2_i1_mints_100_a_to_h1_not_yet_authorized_testnet_20976349.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 3-1 h1->h2 40 A, no SendMax (40.5 rounds to even 40: fee vanishes) (2A6F1581A94B, network tesSUCCESS).
#[test]
fn c18_3_1_h1_h2_40_a_no_sendmax_40_5_rounds_to_even_40_fee_vanishes_testnet_20976383() {
    run_bundle(include_str!("vectors/c18_3_1_h1_h2_40_a_no_sendmax_40_5_rounds_to_even_40_fee_vanishes_testnet_20976383.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 3-23 i1 mints 41 C to h4 (60+41 > max 100) (C1A016FAD4E8, network tecPATH_PARTIAL).
#[test]
fn c18_3_23_i1_mints_41_c_to_h4_60_41_max_100_testnet_20976430() {
    run_bundle(include_str!("vectors/c18_3_23_i1_mints_41_c_to_h4_60_41_max_100_testnet_20976430.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 3-25 h4->h5 10 C (C lacks CanTransfer) (CD4FCFF27BF5, network tecNO_AUTH).
#[test]
fn c18_3_25_h4_h5_10_c_c_lacks_cantransfer_testnet_20976434() {
    run_bundle(include_str!("vectors/c18_3_25_h4_h5_10_c_c_lacks_cantransfer_testnet_20976434.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 3-27 i1 mints 20 C to h5, partial (over the cap by 10) (FCEAEF487B00, network tecPATH_PARTIAL).
#[test]
fn c18_3_27_i1_mints_20_c_to_h5_partial_over_the_cap_by_10_testnet_20976438() {
    run_bundle(include_str!("vectors/c18_3_27_i1_mints_20_c_to_h5_partial_over_the_cap_by_10_testnet_20976438.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 3-29 i1 mints 5 C to h5 (DepositAuth, not preauthorized) (641187079B50, network tecNO_PERMISSION).
#[test]
fn c18_3_29_i1_mints_5_c_to_h5_depositauth_not_preauthorized_testnet_20976442() {
    run_bundle(include_str!("vectors/c18_3_29_i1_mints_5_c_to_h5_depositauth_not_preauthorized_testnet_20976442.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 4-14 h1->h2 10 D (sender locked) (091BB6748126, network tecLOCKED).
#[test]
fn c18_4_14_h1_h2_10_d_sender_locked_testnet_20976474() {
    run_bundle(include_str!("vectors/c18_4_14_h1_h2_10_d_sender_locked_testnet_20976474.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 4-15 h2->h1 10 D (receiver locked) (7DFF5CD5647C, network tecLOCKED).
#[test]
fn c18_4_15_h2_h1_10_d_receiver_locked_testnet_20976476() {
    run_bundle(include_str!("vectors/c18_4_15_h2_h1_10_d_receiver_locked_testnet_20976476.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 4-7 h1->h2 10 D (global lock) (89232527C3E2, network tecLOCKED).
#[test]
fn c18_4_7_h1_h2_10_d_global_lock_testnet_20976460() {
    run_bundle(include_str!("vectors/c18_4_7_h1_h2_10_d_global_lock_testnet_20976460.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 5-3 h2->h1 10 A (h2 unauthorized) (DD3823526E74, network tecNO_AUTH).
#[test]
fn c18_5_3_h2_h1_10_a_h2_unauthorized_testnet_20976503() {
    run_bundle(include_str!("vectors/c18_5_3_h2_h1_10_a_h2_unauthorized_testnet_20976503.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 5-4 h2->i1 10 A (unauthorized holder burning) (6FACF85F2B31, network tecNO_AUTH).
#[test]
fn c18_5_4_h2_i1_10_a_unauthorized_holder_burning_testnet_20976505() {
    run_bundle(include_str!("vectors/c18_5_4_h2_i1_10_a_unauthorized_holder_burning_testnet_20976505.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 5-6 i1 claws 1 A from h2 (now zero) (FB9AD785B414, network tecINSUFFICIENT_FUNDS).
#[test]
fn c18_5_6_i1_claws_1_a_from_h2_now_zero_testnet_20976509() {
    run_bundle(include_str!("vectors/c18_5_6_i1_claws_1_a_from_h2_now_zero_testnet_20976509.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 5-7 i1 claws 5 C from h4 (C lacks CanClawback) (F8BD5A763509, network tecNO_PERMISSION).
#[test]
fn c18_5_7_i1_claws_5_c_from_h4_c_lacks_canclawback_testnet_20976511() {
    run_bundle(include_str!("vectors/c18_5_7_i1_claws_5_c_from_h4_c_lacks_canclawback_testnet_20976511.json"));
}

/// Campaign 18 read-set gap (MPT issuance / tokens) — 5-8 i2 claws 5 A from h1 (not the issuer) (47FC72572557, network tecNO_PERMISSION).
#[test]
fn c18_5_8_i2_claws_5_a_from_h1_not_the_issuer_testnet_20976513() {
    run_bundle(include_str!("vectors/c18_5_8_i2_claws_5_a_from_h1_not_the_issuer_testnet_20976513.json"));
}

/// Campaign 18 — 1-1 i1 A: all six flags (+tfFullyCanonicalSig), fee 1250, max 1e6, scale 2, metadata (DE365EC749D1, network tesSUCCESS).
#[test]
fn c18_1_1_i1_a_all_six_flags_tffullycanonicalsig_fee_1250_max_1e6_scal_testnet_20976332() {
    run_bundle(include_str!("vectors/c18_1_1_i1_a_all_six_flags_tffullycanonicalsig_fee_1250_max_1e6_scal_testnet_20976332.json"));
}

/// Campaign 18 — 1-3 i1 C: no flags, max 100, explicit AssetScale 0 + TransferFee 0 (93860A8CA363, network tesSUCCESS).
#[test]
fn c18_1_3_i1_c_no_flags_max_100_explicit_assetscale_0_transferfee_0_testnet_20976336() {
    run_bundle(include_str!("vectors/c18_1_3_i1_c_no_flags_max_100_explicit_assetscale_0_transferfee_0_testnet_20976336.json"));
}

/// Campaign 18 — 2-11 h1 opts in to A again (ADB68CF32108, network tecDUPLICATE).
#[test]
fn c18_2_11_h1_opts_in_to_a_again_testnet_20976367() {
    run_bundle(include_str!("vectors/c18_2_11_h1_opts_in_to_a_again_testnet_20976367.json"));
}

/// Campaign 18 — 2-14 i1 authorizes h1 on C (C lacks RequireAuth; h1 has no C token) (27339BD4102E, network tecNO_AUTH).
#[test]
fn c18_2_14_i1_authorizes_h1_on_c_c_lacks_requireauth_h1_has_no_c_token_testnet_20976373() {
    run_bundle(include_str!("vectors/c18_2_14_i1_authorizes_h1_on_c_c_lacks_requireauth_h1_has_no_c_token_testnet_20976373.json"));
}

/// Campaign 18 — 2-18 h1 unauthorizes A holding 5000 (AB4FB0331CF2, network tecHAS_OBLIGATIONS).
#[test]
fn c18_2_18_h1_unauthorizes_a_holding_5000_testnet_20976381() {
    run_bundle(include_str!("vectors/c18_2_18_h1_unauthorizes_a_holding_5000_testnet_20976381.json"));
}

/// Campaign 18 — 3-5 h1->h2 1000 A, SendMax 1012 (1012.5 -> 1012 even) (5B85DB983B31, network tesSUCCESS).
#[test]
fn c18_3_5_h1_h2_1000_a_sendmax_1012_1012_5_1012_even_testnet_20976391() {
    run_bundle(include_str!("vectors/c18_3_5_h1_h2_1000_a_sendmax_1012_1012_5_1012_even_testnet_20976391.json"));
}

/// Campaign 18 — 3-8 h1->i1 100 A (burn) (4CF03CF09ED9, network tesSUCCESS).
#[test]
fn c18_3_8_h1_i1_100_a_burn_testnet_20976397() {
    run_bundle(include_str!("vectors/c18_3_8_h1_i1_100_a_burn_testnet_20976397.json"));
}

/// Campaign 18 — 3-11 i1 mints 3e16 B to h1 (EDC5C0664A6A, network tesSUCCESS).
#[test]
fn c18_3_11_i1_mints_3e16_b_to_h1_testnet_20976403() {
    run_bundle(include_str!("vectors/c18_3_11_i1_mints_3e16_b_to_h1_testnet_20976403.json"));
}

/// Campaign 18 — 3-18 i2 mints 1000 E to h1 (4343AFCA39CE, network tesSUCCESS).
#[test]
fn c18_3_18_i2_mints_1000_e_to_h1_testnet_20976417() {
    run_bundle(include_str!("vectors/c18_3_18_i2_mints_1000_e_to_h1_testnet_20976417.json"));
}

/// Campaign 18 — 3-31 i1 pays 5 A to an unfunded account (5A71E2C2640A, network tecNO_DST).
#[test]
fn c18_3_31_i1_pays_5_a_to_an_unfunded_account_testnet_20976446() {
    run_bundle(include_str!("vectors/c18_3_31_i1_pays_5_a_to_an_unfunded_account_testnet_20976446.json"));
}

/// Campaign 18 — 4-6 i2 locks D globally (53EBD76DBA12, network tesSUCCESS).
#[test]
fn c18_4_6_i2_locks_d_globally_testnet_20976458() {
    run_bundle(include_str!("vectors/c18_4_6_i2_locks_d_globally_testnet_20976458.json"));
}

/// Campaign 18 — 4-13 i2 locks h1 individually (9EB0880E7182, network tesSUCCESS).
#[test]
fn c18_4_13_i2_locks_h1_individually_testnet_20976472() {
    run_bundle(include_str!("vectors/c18_4_13_i2_locks_h1_individually_testnet_20976472.json"));
}

/// Campaign 18 — 4-18 i2 IssuanceSet Flags 0 on D (CanLock) (DE84A64282BE, network tesSUCCESS).
#[test]
fn c18_4_18_i2_issuanceset_flags_0_on_d_canlock_testnet_20976482() {
    run_bundle(include_str!("vectors/c18_4_18_i2_issuanceset_flags_0_on_d_canlock_testnet_20976482.json"));
}

/// Campaign 18 — 4-20 i1 locks C (no CanLock) (7206B9265567, network tecNO_PERMISSION).
#[test]
fn c18_4_20_i1_locks_c_no_canlock_testnet_20976486() {
    run_bundle(include_str!("vectors/c18_4_20_i1_locks_c_no_canlock_testnet_20976486.json"));
}

/// Campaign 18 — 4-25 i2 claws 999999 D from locked h1 (BDDD7F4D7533, network tesSUCCESS).
#[test]
fn c18_4_25_i2_claws_999999_d_from_locked_h1_testnet_20976495() {
    run_bundle(include_str!("vectors/c18_4_25_i2_claws_999999_d_from_locked_h1_testnet_20976495.json"));
}

/// Campaign 18 — 4-26 h1 unauthorizes D: locked, zero balance (46A472613092, network tesSUCCESS).
#[test]
fn c18_4_26_h1_unauthorizes_d_locked_zero_balance_testnet_20976497() {
    run_bundle(include_str!("vectors/c18_4_26_h1_unauthorizes_d_locked_zero_balance_testnet_20976497.json"));
}

/// Campaign 18 — 5-1 i1 claws 100 A from h1 (4550E025C43A, network tesSUCCESS).
#[test]
fn c18_5_1_i1_claws_100_a_from_h1_testnet_20976499() {
    run_bundle(include_str!("vectors/c18_5_1_i1_claws_100_a_from_h1_testnet_20976499.json"));
}

/// Campaign 18 — 5-5 i1 claws 999999999 A from unauthorized h2 (C0B642E45ACE, network tesSUCCESS).
#[test]
fn c18_5_5_i1_claws_999999999_a_from_unauthorized_h2_testnet_20976507() {
    run_bundle(include_str!("vectors/c18_5_5_i1_claws_999999999_a_from_unauthorized_h2_testnet_20976507.json"));
}

/// Campaign 18 — 5-9 i1 claws 5 A from h4 (no A token) (0FD848B6CCCD, network tecOBJECT_NOT_FOUND).
#[test]
fn c18_5_9_i1_claws_5_a_from_h4_no_a_token_testnet_20976515() {
    run_bundle(include_str!("vectors/c18_5_9_i1_claws_5_a_from_h4_no_a_token_testnet_20976515.json"));
}

/// Campaign 18 — 6-1 h1 escrows 73 A to h2 (TransferRate snapshot) (BBBD5CF273FC, network tesSUCCESS).
#[test]
fn c18_6_1_h1_escrows_73_a_to_h2_transferrate_snapshot_testnet_20976521() {
    run_bundle(include_str!("vectors/c18_6_1_h1_escrows_73_a_to_h2_transferrate_snapshot_testnet_20976521.json"));
}

/// Campaign 18 — 6-8 h3 finishes the D escrow BEFORE FinishAfter while locked (CA7330027DC3, network tecLOCKED).
#[test]
fn c18_6_8_h3_finishes_the_d_escrow_before_finishafter_while_locked_testnet_20976535() {
    run_bundle(include_str!("vectors/c18_6_8_h3_finishes_the_d_escrow_before_finishafter_while_locked_testnet_20976535.json"));
}

/// Campaign 18 — 6-9 h2 escrows 10 D to locked h3 (D1C3FF325CB1, network tecLOCKED).
#[test]
fn c18_6_9_h2_escrows_10_d_to_locked_h3_testnet_20976537() {
    run_bundle(include_str!("vectors/c18_6_9_h2_escrows_10_d_to_locked_h3_testnet_20976537.json"));
}

/// Campaign 18 — 6-13 h2 finishes the 100 A escrow (33E4B07758F8, network tesSUCCESS).
#[test]
fn c18_6_13_h2_finishes_the_100_a_escrow_testnet_20976554() {
    run_bundle(include_str!("vectors/c18_6_13_h2_finishes_the_100_a_escrow_testnet_20976554.json"));
}

/// Campaign 18 — 6-15 h1 (third party) finishes the D escrow to token-less h5 (6955DB479CA2, network tecNO_PERMISSION).
#[test]
fn c18_6_15_h1_third_party_finishes_the_d_escrow_to_token_less_h5_testnet_20976558() {
    run_bundle(include_str!("vectors/c18_6_15_h1_third_party_finishes_the_d_escrow_to_token_less_h5_testnet_20976558.json"));
}

/// Campaign 18 — 6-16 h5 finishes it itself (creates its D token) (ED41F8770420, network tesSUCCESS).
#[test]
fn c18_6_16_h5_finishes_it_itself_creates_its_d_token_testnet_20976560() {
    run_bundle(include_str!("vectors/c18_6_16_h5_finishes_it_itself_creates_its_d_token_testnet_20976560.json"));
}

/// Campaign 18 — 6-17 h3 finishes the D escrow after FinishAfter, still locked (41D3839D1251, network tecLOCKED).
#[test]
fn c18_6_17_h3_finishes_the_d_escrow_after_finishafter_still_locked_testnet_20976562() {
    run_bundle(include_str!("vectors/c18_6_17_h3_finishes_the_d_escrow_after_finishafter_still_locked_testnet_20976562.json"));
}

/// Campaign 18 — 6-19 h3 finishes the D escrow (54DA9B54191E, network tesSUCCESS).
#[test]
fn c18_6_19_h3_finishes_the_d_escrow_testnet_20976566() {
    run_bundle(include_str!("vectors/c18_6_19_h3_finishes_the_d_escrow_testnet_20976566.json"));
}

/// Campaign 18 — 6-20 h2 cancels the 30 D escrow after CancelAfter (0A9505F989CD, network tesSUCCESS).
#[test]
fn c18_6_20_h2_cancels_the_30_d_escrow_after_cancelafter_testnet_20976568() {
    run_bundle(include_str!("vectors/c18_6_20_h2_cancels_the_30_d_escrow_after_cancelafter_testnet_20976568.json"));
}

/// Campaign 18 — 7-1 i1 destroys A (outstanding) (F6944EECF5D6, network tecHAS_OBLIGATIONS).
#[test]
fn c18_7_1_i1_destroys_a_outstanding_testnet_20976570() {
    run_bundle(include_str!("vectors/c18_7_1_i1_destroys_a_outstanding_testnet_20976570.json"));
}

/// Campaign 18 — 7-4 i1 destroys C (outstanding 0) (1108D702A34D, network tesSUCCESS).
#[test]
fn c18_7_4_i1_destroys_c_outstanding_0_testnet_20976577() {
    run_bundle(include_str!("vectors/c18_7_4_i1_destroys_c_outstanding_0_testnet_20976577.json"));
}

/// Campaign 18 — 7-6 h5 opts in to destroyed C (still holds its empty token) (3067EF061C59, network tecOBJECT_NOT_FOUND).
#[test]
fn c18_7_6_h5_opts_in_to_destroyed_c_still_holds_its_empty_token_testnet_20976581() {
    run_bundle(include_str!("vectors/c18_7_6_h5_opts_in_to_destroyed_c_still_holds_its_empty_token_testnet_20976581.json"));
}

/// Campaign 18 — 7-7 i1 mints 1 C to h5 after the destroy (5DB26CCE82E6, network tecOBJECT_NOT_FOUND).
#[test]
fn c18_7_7_i1_mints_1_c_to_h5_after_the_destroy_testnet_20976583() {
    run_bundle(include_str!("vectors/c18_7_7_i1_mints_1_c_to_h5_after_the_destroy_testnet_20976583.json"));
}

/// Campaign 18 — 8-4 h6 opts in to B (OwnerCount 2: reserve 1.6 > ~1.5) (5E7AFAD013E1, network tecINSUFFICIENT_RESERVE).
#[test]
fn c18_8_4_h6_opts_in_to_b_ownercount_2_reserve_1_6_1_5_testnet_20976593() {
    run_bundle(include_str!("vectors/c18_8_4_h6_opts_in_to_b_ownercount_2_reserve_1_6_1_5_testnet_20976593.json"));
}

/// Campaign 18 — 8-5 h6 creates an issuance (reserve 1.6) (DC3242DA14BF, network tecINSUFFICIENT_RESERVE).
#[test]
fn c18_8_5_h6_creates_an_issuance_reserve_1_6_testnet_20976595() {
    run_bundle(include_str!("vectors/c18_8_5_h6_creates_an_issuance_reserve_1_6_testnet_20976595.json"));
}
