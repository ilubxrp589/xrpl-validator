//! Campaign 20 (testnet, 2026-09-23) byte-exact vectors: DepositAuth by
//! CREDENTIALS across Payment (XRP / IOU / MPT), EscrowFinish,
//! PaymentChannelClaim, CheckCash and AccountDelete; DepositPreauth credential
//! sets; CredentialCreate / Accept / Delete edges. Findings 375-380 plus three
//! read-set gaps. Same harness as did_vector.rs.
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

/// Finding 375 — M-17 (b) s1 pays dst 10 MPT [iss/KYC] (E33A9793F637, network tesSUCCESS).
#[test]
fn c20_m_17_b_s1_pays_dst_10_mpt_iss_kyc_testnet_20976569() {
    run_bundle(include_str!("vectors/c20_m_17_b_s1_pays_dst_10_mpt_iss_kyc_testnet_20976569.json"));
}

/// Finding 375 — X-3 (d) s1 pays dst 10 MPT [expired EXP-M] (D25509AD2D3D, network tecEXPIRED).
#[test]
fn c20_x_3_d_s1_pays_dst_10_mpt_expired_exp_m_testnet_20976641() {
    run_bundle(include_str!("vectors/c20_x_3_d_s1_pays_dst_10_mpt_expired_exp_m_testnet_20976641.json"));
}

/// Finding 376 — X-9 (carve-out) s1 funds NEW with exactly 1 XRP (= base reserve) [expired EXP-N] (3A88EBE4F58B, network tesSUCCESS).
#[test]
fn c20_x_9_carve_out_s1_funds_new_with_exactly_1_xrp_base_reserve_expir_testnet_20976653() {
    run_bundle(include_str!("vectors/c20_x_9_carve_out_s1_funds_new_with_exactly_1_xrp_base_reserve_expir_testnet_20976653.json"));
}

/// Finding 376 — X-10 (carve-out) s1 pays NEW 0.5 XRP [expired EXP-N] (NEW holds exactly 1 XRP) (EFCD8E552A36, network tesSUCCESS).
#[test]
fn c20_x_10_carve_out_s1_pays_new_0_5_xrp_expired_exp_n_new_holds_exactl_testnet_20976655() {
    run_bundle(include_str!("vectors/c20_x_10_carve_out_s1_pays_new_0_5_xrp_expired_exp_n_new_holds_exactl_testnet_20976655.json"));
}

/// Finding 377 — X-13 stg deletes s3 STALE (expired, never accepted) (F5D3209D0338, network tesSUCCESS).
#[test]
fn c20_x_13_stg_deletes_s3_stale_expired_never_accepted_testnet_20976661() {
    run_bundle(include_str!("vectors/c20_x_13_stg_deletes_s3_stale_expired_never_accepted_testnet_20976661.json"));
}

/// Finding 377 — E-5 stg deletes the expired, un-accepted RSV (7D1515D87CF4, network tesSUCCESS).
#[test]
fn c20_e_5_stg_deletes_the_expired_un_accepted_rsv_testnet_20977022() {
    run_bundle(include_str!("vectors/c20_e_5_stg_deletes_the_expired_un_accepted_rsv_testnet_20977022.json"));
}

/// Finding 378 — D-1 del1 deletes into dst (DepositAuth), no CredentialIDs (26715285ED60, network tecNO_PERMISSION).
#[test]
fn c20_d_1_del1_deletes_into_dst_depositauth_no_credentialids_testnet_20976670() {
    run_bundle(include_str!("vectors/c20_d_1_del1_deletes_into_dst_depositauth_no_credentialids_testnet_20976670.json"));
}

/// Finding 378 — D-2 del1 deletes into dst [del1 iss/KYC] (set A gone) (58AA68E3AAB9, network tecNO_PERMISSION).
#[test]
fn c20_d_2_del1_deletes_into_dst_del1_iss_kyc_set_a_gone_testnet_20976673() {
    run_bundle(include_str!("vectors/c20_d_2_del1_deletes_into_dst_del1_iss_kyc_set_a_gone_testnet_20976673.json"));
}

/// Finding 378 — D-3 del1 deletes into dst [expired EXP-D] (61CE5312CE89, network tecEXPIRED).
#[test]
fn c20_d_3_del1_deletes_into_dst_expired_exp_d_testnet_20976676() {
    run_bundle(include_str!("vectors/c20_d_3_del1_deletes_into_dst_expired_exp_d_testnet_20976676.json"));
}

/// Finding 379 — E-1 s2 accepts a KYC credential from an issuer account that does not exist (967546A20D4D, network tecNO_ISSUER).
#[test]
fn c20_e_1_s2_accepts_a_kyc_credential_from_an_issuer_account_that_does_testnet_20977008() {
    run_bundle(include_str!("vectors/c20_e_1_s2_accepts_a_kyc_credential_from_an_issuer_account_that_does_testnet_20977008.json"));
}

/// Finding 380 — E-4 NEW (short of the reserve for OwnerCount+1) accepts the EXPIRED RSV (2BF90D2E1DB3, network tecINSUFFICIENT_RESERVE).
#[test]
fn c20_e_4_new_short_of_the_reserve_for_ownercount_1_accepts_the_expire_testnet_20977020() {
    run_bundle(include_str!("vectors/c20_e_4_new_short_of_the_reserve_for_ownercount_1_accepts_the_expire_testnet_20977020.json"));
}

/// Campaign 20 read-set gap — M-16 (a) s1 pays dst 10 MPT, no CredentialIDs (6945B79A3FF9, network tecNO_PERMISSION).
#[test]
fn c20_m_16_a_s1_pays_dst_10_mpt_no_credentialids_testnet_20976489() {
    run_bundle(include_str!("vectors/c20_m_16_a_s1_pays_dst_10_mpt_no_credentialids_testnet_20976489.json"));
}

/// Campaign 20 read-set gap — M-18 (f) s1 pays dst 10 MPT [iss/KYC, iss/AML] (C8E769A834E0, network tecNO_PERMISSION).
#[test]
fn c20_m_18_f_s1_pays_dst_10_mpt_iss_kyc_iss_aml_testnet_20976571() {
    run_bundle(include_str!("vectors/c20_m_18_f_s1_pays_dst_10_mpt_iss_kyc_iss_aml_testnet_20976571.json"));
}

/// Campaign 20 read-set gap — M-23 (b) s1 finishes E1 [iss/KYC] (BC59E355E6FF, network tesSUCCESS).
#[test]
fn c20_m_23_b_s1_finishes_e1_iss_kyc_testnet_20976579() {
    run_bundle(include_str!("vectors/c20_m_23_b_s1_finishes_e1_iss_kyc_testnet_20976579.json"));
}

/// Campaign 20 read-set gap — M-24 (b) s2 (third party) finishes E2 [s2 iss/KYC] (CBD5F596E483, network tesSUCCESS).
#[test]
fn c20_m_24_b_s2_third_party_finishes_e2_s2_iss_kyc_testnet_20976581() {
    run_bundle(include_str!("vectors/c20_m_24_b_s2_third_party_finishes_e2_s2_iss_kyc_testnet_20976581.json"));
}

/// Campaign 20 read-set gap — M-29 (b) s1 pushes Balance 1 XRP [iss/KYC] (9E4F338E5364, network tesSUCCESS).
#[test]
fn c20_m_29_b_s1_pushes_balance_1_xrp_iss_kyc_testnet_20976587() {
    run_bundle(include_str!("vectors/c20_m_29_b_s1_pushes_balance_1_xrp_iss_kyc_testnet_20976587.json"));
}

/// Campaign 20 read-set gap — M-36 dst authorizes set B again, ids in ASCending order (E5C2AB1D123A, network tecDUPLICATE).
#[test]
fn c20_m_36_dst_authorizes_set_b_again_ids_in_ascending_order_testnet_20976602() {
    run_bundle(include_str!("vectors/c20_m_36_dst_authorizes_set_b_again_ids_in_ascending_order_testnet_20976602.json"));
}

/// Campaign 20 read-set gap — X-5 (b) s1 finishes E4 [set B, reversed] (B521E0605227, network tesSUCCESS).
#[test]
fn c20_x_5_b_s1_finishes_e4_set_b_reversed_testnet_20976645() {
    run_bundle(include_str!("vectors/c20_x_5_b_s1_finishes_e4_set_b_reversed_testnet_20976645.json"));
}

/// Campaign 20 read-set gap — X-8 (b) s1 pushes Balance 3 XRP [set B] (4E30535E2D52, network tesSUCCESS).
#[test]
fn c20_x_8_b_s1_pushes_balance_3_xrp_set_b_testnet_20976651() {
    run_bundle(include_str!("vectors/c20_x_8_b_s1_pushes_balance_3_xrp_set_b_testnet_20976651.json"));
}

/// Campaign 20 — M-1 (a) s1 pays dst 2 XRP, no CredentialIDs (CDF27E9F0CF1, network tecNO_PERMISSION).
#[test]
fn c20_m_1_a_s1_pays_dst_2_xrp_no_credentialids_testnet_20976477() {
    run_bundle(include_str!("vectors/c20_m_1_a_s1_pays_dst_2_xrp_no_credentialids_testnet_20976477.json"));
}

/// Campaign 20 — M-2 (b) s1 pays dst 2 XRP [s1 iss/KYC] = set A (ACF2339ED2AA, network tesSUCCESS).
#[test]
fn c20_m_2_b_s1_pays_dst_2_xrp_s1_iss_kyc_set_a_testnet_20976543() {
    run_bundle(include_str!("vectors/c20_m_2_b_s1_pays_dst_2_xrp_s1_iss_kyc_set_a_testnet_20976543.json"));
}

/// Campaign 20 — M-3 (b) s1 pays dst 2 XRP set B, ids in keylet order (0C3ACB26D9EF, network tesSUCCESS).
#[test]
fn c20_m_3_b_s1_pays_dst_2_xrp_set_b_ids_in_keylet_order_testnet_20976545() {
    run_bundle(include_str!("vectors/c20_m_3_b_s1_pays_dst_2_xrp_set_b_ids_in_keylet_order_testnet_20976545.json"));
}

/// Campaign 20 — M-4 (b) s1 pays dst 2 XRP set B, ids reversed (D61854F4B30E, network tesSUCCESS).
#[test]
fn c20_m_4_b_s1_pays_dst_2_xrp_set_b_ids_reversed_testnet_20976547() {
    run_bundle(include_str!("vectors/c20_m_4_b_s1_pays_dst_2_xrp_set_b_ids_reversed_testnet_20976547.json"));
}

/// Campaign 20 — M-5 (c) s3 pays dst 2 XRP [its UN-accepted iss/KYC] (B59CAFDBDB27, network tecBAD_CREDENTIALS).
#[test]
fn c20_m_5_c_s3_pays_dst_2_xrp_its_un_accepted_iss_kyc_testnet_20976549() {
    run_bundle(include_str!("vectors/c20_m_5_c_s3_pays_dst_2_xrp_its_un_accepted_iss_kyc_testnet_20976549.json"));
}

/// Campaign 20 — M-6 (e) s2 pays dst 2 XRP [s1 iss/KYC] (not its own) (0CEBD04A3002, network tecBAD_CREDENTIALS).
#[test]
fn c20_m_6_e_s2_pays_dst_2_xrp_s1_iss_kyc_not_its_own_testnet_20976551() {
    run_bundle(include_str!("vectors/c20_m_6_e_s2_pays_dst_2_xrp_s1_iss_kyc_not_its_own_testnet_20976551.json"));
}

/// Campaign 20 — M-7 (f) s1 pays dst 2 XRP [iss/KYC, iss/AML] (superset of A) (AA1837219813, network tecNO_PERMISSION).
#[test]
fn c20_m_7_f_s1_pays_dst_2_xrp_iss_kyc_iss_aml_superset_of_a_testnet_20976553() {
    run_bundle(include_str!("vectors/c20_m_7_f_s1_pays_dst_2_xrp_iss_kyc_iss_aml_superset_of_a_testnet_20976553.json"));
}

/// Campaign 20 — M-8 (f) s1 pays dst 2 XRP [iss2/KYC] (subset of B) (F782EEDAB94E, network tecNO_PERMISSION).
#[test]
fn c20_m_8_f_s1_pays_dst_2_xrp_iss2_kyc_subset_of_b_testnet_20976556() {
    run_bundle(include_str!("vectors/c20_m_8_f_s1_pays_dst_2_xrp_iss2_kyc_subset_of_b_testnet_20976556.json"));
}

/// Campaign 20 — M-9 (g) s1 pays dst2 (no DepositAuth) 2 XRP [s1 iss/KYC] (C75BF784154B, network tesSUCCESS).
#[test]
fn c20_m_9_g_s1_pays_dst2_no_depositauth_2_xrp_s1_iss_kyc_testnet_20976558() {
    run_bundle(include_str!("vectors/c20_m_9_g_s1_pays_dst2_no_depositauth_2_xrp_s1_iss_kyc_testnet_20976558.json"));
}

/// Campaign 20 — M-10 (i) s1 pays dst2 2 XRP [s1 AccountRoot key as a CredentialID] (E30C9EA00AA3, network tecBAD_CREDENTIALS).
#[test]
fn c20_m_10_i_s1_pays_dst2_2_xrp_s1_accountroot_key_as_a_credentialid_testnet_20976560() {
    run_bundle(include_str!("vectors/c20_m_10_i_s1_pays_dst2_2_xrp_s1_accountroot_key_as_a_credentialid_testnet_20976560.json"));
}

/// Campaign 20 — M-13 (a) s1 pays dst 5 USD, no CredentialIDs (3316B11B9ACD, network tecNO_PERMISSION).
#[test]
fn c20_m_13_a_s1_pays_dst_5_usd_no_credentialids_testnet_20976563() {
    run_bundle(include_str!("vectors/c20_m_13_a_s1_pays_dst_5_usd_no_credentialids_testnet_20976563.json"));
}

/// Campaign 20 — M-14 (b) s1 pays dst 5 USD [iss/KYC] (D36B18440C23, network tesSUCCESS).
#[test]
fn c20_m_14_b_s1_pays_dst_5_usd_iss_kyc_testnet_20976565() {
    run_bundle(include_str!("vectors/c20_m_14_b_s1_pays_dst_5_usd_iss_kyc_testnet_20976565.json"));
}

/// Campaign 20 — M-15 (f) s1 pays dst 5 USD [iss/KYC, iss/AML] (ED585E475C8F, network tecNO_PERMISSION).
#[test]
fn c20_m_15_f_s1_pays_dst_5_usd_iss_kyc_iss_aml_testnet_20976567() {
    run_bundle(include_str!("vectors/c20_m_15_f_s1_pays_dst_5_usd_iss_kyc_iss_aml_testnet_20976567.json"));
}

/// Campaign 20 — M-19 (a) s1 finishes E1, no CredentialIDs (6303D14ED4E9, network tecNO_PERMISSION).
#[test]
fn c20_m_19_a_s1_finishes_e1_no_credentialids_testnet_20976492() {
    run_bundle(include_str!("vectors/c20_m_19_a_s1_finishes_e1_no_credentialids_testnet_20976492.json"));
}

/// Campaign 20 — M-20 (c) s3 finishes E1 [its UN-accepted KYC] (B1B5BF3A09EE, network tecBAD_CREDENTIALS).
#[test]
fn c20_m_20_c_s3_finishes_e1_its_un_accepted_kyc_testnet_20976573() {
    run_bundle(include_str!("vectors/c20_m_20_c_s3_finishes_e1_its_un_accepted_kyc_testnet_20976573.json"));
}

/// Campaign 20 — M-21 (e) s2 finishes E1 [s1 iss/KYC] (A94B77896202, network tecBAD_CREDENTIALS).
#[test]
fn c20_m_21_e_s2_finishes_e1_s1_iss_kyc_testnet_20976575() {
    run_bundle(include_str!("vectors/c20_m_21_e_s2_finishes_e1_s1_iss_kyc_testnet_20976575.json"));
}

/// Campaign 20 — M-22 (f) s1 finishes E1 [iss/KYC, iss/AML] (73B7CE518274, network tecNO_PERMISSION).
#[test]
fn c20_m_22_f_s1_finishes_e1_iss_kyc_iss_aml_testnet_20976577() {
    run_bundle(include_str!("vectors/c20_m_22_f_s1_finishes_e1_iss_kyc_iss_aml_testnet_20976577.json"));
}

/// Campaign 20 — M-25 (h) dst finishes E3 itself, no CredentialIDs (1C4BCF6AA168, network tesSUCCESS).
#[test]
fn c20_m_25_h_dst_finishes_e3_itself_no_credentialids_testnet_20976497() {
    run_bundle(include_str!("vectors/c20_m_25_h_dst_finishes_e3_itself_no_credentialids_testnet_20976497.json"));
}

/// Campaign 20 — M-26 (a) s1 (source) pushes Balance 1 XRP, no CredentialIDs (88A7CDF473DA, network tecNO_PERMISSION).
#[test]
fn c20_m_26_a_s1_source_pushes_balance_1_xrp_no_credentialids_testnet_20976499() {
    run_bundle(include_str!("vectors/c20_m_26_a_s1_source_pushes_balance_1_xrp_no_credentialids_testnet_20976499.json"));
}

/// Campaign 20 — M-27 (f) s1 pushes Balance 1 XRP [iss/KYC, iss/AML] (44B25BDF158D, network tecNO_PERMISSION).
#[test]
fn c20_m_27_f_s1_pushes_balance_1_xrp_iss_kyc_iss_aml_testnet_20976583() {
    run_bundle(include_str!("vectors/c20_m_27_f_s1_pushes_balance_1_xrp_iss_kyc_iss_aml_testnet_20976583.json"));
}

/// Campaign 20 — M-28 (e) s2 claims Balance 1 XRP with signature [s1 iss/KYC] (C4361E4F3CF0, network tecBAD_CREDENTIALS).
#[test]
fn c20_m_28_e_s2_claims_balance_1_xrp_with_signature_s1_iss_kyc_testnet_20976585() {
    run_bundle(include_str!("vectors/c20_m_28_e_s2_claims_balance_1_xrp_with_signature_s1_iss_kyc_testnet_20976585.json"));
}

/// Campaign 20 — M-30 (h) dst claims Balance 2 XRP with signature, no CredentialIDs (F0F4A437530A, network tesSUCCESS).
#[test]
fn c20_m_30_h_dst_claims_balance_2_xrp_with_signature_no_credentialids_testnet_20976589() {
    run_bundle(include_str!("vectors/c20_m_30_h_dst_claims_balance_2_xrp_with_signature_no_credentialids_testnet_20976589.json"));
}

/// Campaign 20 — M-32 dst cashes the USD check (DepositAuth, no preauth for s1 needed) (31CAA0CEE689, network tesSUCCESS).
#[test]
fn c20_m_32_dst_cashes_the_usd_check_depositauth_no_preauth_for_s1_neede_testnet_20976593() {
    run_bundle(include_str!("vectors/c20_m_32_dst_cashes_the_usd_check_depositauth_no_preauth_for_s1_neede_testnet_20976593.json"));
}

/// Campaign 20 — M-35 dst cashes the XRP check (1188AF7BC437, network tesSUCCESS).
#[test]
fn c20_m_35_dst_cashes_the_xrp_check_testnet_20976600() {
    run_bundle(include_str!("vectors/c20_m_35_dst_cashes_the_xrp_check_testnet_20976600.json"));
}

/// Campaign 20 — M-37 dst authorizes an 8-entry set C (max), shuffled (27D79CA542AF, network tesSUCCESS).
#[test]
fn c20_m_37_dst_authorizes_an_8_entry_set_c_max_shuffled_testnet_20976604() {
    run_bundle(include_str!("vectors/c20_m_37_dst_authorizes_an_8_entry_set_c_max_shuffled_testnet_20976604.json"));
}

/// Campaign 20 — M-40 dst authorizes {(unfunded issuer, KYC)} (1720970D4476, network tecNO_ISSUER).
#[test]
fn c20_m_40_dst_authorizes_unfunded_issuer_kyc_testnet_20976607() {
    run_bundle(include_str!("vectors/c20_m_40_dst_authorizes_unfunded_issuer_kyc_testnet_20976607.json"));
}

/// Campaign 20 — M-41 dst unauthorizes {(iss, NOPE)} (never authorized) (1BC0471F6362, network tecNO_ENTRY).
#[test]
fn c20_m_41_dst_unauthorizes_iss_nope_never_authorized_testnet_20976609() {
    run_bundle(include_str!("vectors/c20_m_41_dst_unauthorizes_iss_nope_never_authorized_testnet_20976609.json"));
}

/// Campaign 20 — M-42 dst unauthorizes set C given in another order (FB3CC6B0DF90, network tesSUCCESS).
#[test]
fn c20_m_42_dst_unauthorizes_set_c_given_in_another_order_testnet_20976611() {
    run_bundle(include_str!("vectors/c20_m_42_dst_unauthorizes_set_c_given_in_another_order_testnet_20976611.json"));
}

/// Campaign 20 — M-43 stg deletes s1 iss/AML (not expired) (ACF75E62F855, network tecNO_PERMISSION).
#[test]
fn c20_m_43_stg_deletes_s1_iss_aml_not_expired_testnet_20976613() {
    run_bundle(include_str!("vectors/c20_m_43_stg_deletes_s1_iss_aml_not_expired_testnet_20976613.json"));
}

/// Campaign 20 — M-44 iss (issuer) deletes s1 iss/AML (accepted) (E7A3DDE5B191, network tesSUCCESS).
#[test]
fn c20_m_44_iss_issuer_deletes_s1_iss_aml_accepted_testnet_20976615() {
    run_bundle(include_str!("vectors/c20_m_44_iss_issuer_deletes_s1_iss_aml_accepted_testnet_20976615.json"));
}

/// Campaign 20 — M-45 s1 pays dst2 2 XRP [the deleted iss/AML] (A952CBD85806, network tecBAD_CREDENTIALS).
#[test]
fn c20_m_45_s1_pays_dst2_2_xrp_the_deleted_iss_aml_testnet_20976617() {
    run_bundle(include_str!("vectors/c20_m_45_s1_pays_dst2_2_xrp_the_deleted_iss_aml_testnet_20976617.json"));
}

/// Campaign 20 — M-47 s1 pays dst 2 XRP [iss/KYC] (set A gone) (ED819762702F, network tecNO_PERMISSION).
#[test]
fn c20_m_47_s1_pays_dst_2_xrp_iss_kyc_set_a_gone_testnet_20976621() {
    run_bundle(include_str!("vectors/c20_m_47_s1_pays_dst_2_xrp_iss_kyc_set_a_gone_testnet_20976621.json"));
}

/// Campaign 20 — M-49 (h) s2 pays dst 2 XRP [s2 iss/KYC] (no set matches; account preauth) (EC06DDD3AEEC, network tesSUCCESS).
#[test]
fn c20_m_49_h_s2_pays_dst_2_xrp_s2_iss_kyc_no_set_matches_account_preaut_testnet_20976626() {
    run_bundle(include_str!("vectors/c20_m_49_h_s2_pays_dst_2_xrp_s2_iss_kyc_no_set_matches_account_preaut_testnet_20976626.json"));
}

/// Campaign 20 — M-50 s2 (subject) deletes its own iss/KYC (5FDA89748189, network tesSUCCESS).
#[test]
fn c20_m_50_s2_subject_deletes_its_own_iss_kyc_testnet_20976628() {
    run_bundle(include_str!("vectors/c20_m_50_s2_subject_deletes_its_own_iss_kyc_testnet_20976628.json"));
}

/// Campaign 20 — M-51 s2 deletes it again (A0B5E7A0979B, network tecNO_ENTRY).
#[test]
fn c20_m_51_s2_deletes_it_again_testnet_20976630() {
    run_bundle(include_str!("vectors/c20_m_51_s2_deletes_it_again_testnet_20976630.json"));
}

/// Campaign 20 — X-1 (d) s1 pays dst 2 XRP [expired EXP-P] (4EF21D38FE6C, network tecEXPIRED).
#[test]
fn c20_x_1_d_s1_pays_dst_2_xrp_expired_exp_p_testnet_20976636() {
    run_bundle(include_str!("vectors/c20_x_1_d_s1_pays_dst_2_xrp_expired_exp_p_testnet_20976636.json"));
}

/// Campaign 20 — X-2 (d) s1 pays dst 5 USD [expired EXP-I] (9DFC5D12EF41, network tecEXPIRED).
#[test]
fn c20_x_2_d_s1_pays_dst_5_usd_expired_exp_i_testnet_20976639() {
    run_bundle(include_str!("vectors/c20_x_2_d_s1_pays_dst_5_usd_expired_exp_i_testnet_20976639.json"));
}

/// Campaign 20 — X-4 (d) s1 finishes E4 [expired EXP-E] (27F720E5DFB6, network tecEXPIRED).
#[test]
fn c20_x_4_d_s1_finishes_e4_expired_exp_e_testnet_20976643() {
    run_bundle(include_str!("vectors/c20_x_4_d_s1_finishes_e4_expired_exp_e_testnet_20976643.json"));
}

/// Campaign 20 — X-6 (d) s1 pushes Balance 3 XRP [expired EXP-C] (439A1A446ACC, network tecEXPIRED).
#[test]
fn c20_x_6_d_s1_pushes_balance_3_xrp_expired_exp_c_testnet_20976647() {
    run_bundle(include_str!("vectors/c20_x_6_d_s1_pushes_balance_3_xrp_expired_exp_c_testnet_20976647.json"));
}

/// Campaign 20 — X-7 (d,h) dst claims Balance 3 XRP with signature [its own expired EXP-X] (BAE87FB5B53E, network tecEXPIRED).
#[test]
fn c20_x_7_d_h_dst_claims_balance_3_xrp_with_signature_its_own_expired_testnet_20976649() {
    run_bundle(include_str!("vectors/c20_x_7_d_h_dst_claims_balance_3_xrp_with_signature_its_own_expired_testnet_20976649.json"));
}

/// Campaign 20 — X-11 (d,g) s1 pays NEW 0.5 XRP [expired EXP-N] (NEW now holds 1.5 XRP) (9255B02174C6, network tecEXPIRED).
#[test]
fn c20_x_11_d_g_s1_pays_new_0_5_xrp_expired_exp_n_new_now_holds_1_5_xrp_testnet_20976657() {
    run_bundle(include_str!("vectors/c20_x_11_d_g_s1_pays_new_0_5_xrp_expired_exp_n_new_now_holds_1_5_xrp_testnet_20976657.json"));
}

/// Campaign 20 — X-12 s2 accepts the expired LATE (41BE2C974E90, network tecEXPIRED).
#[test]
fn c20_x_12_s2_accepts_the_expired_late_testnet_20976659() {
    run_bundle(include_str!("vectors/c20_x_12_s2_accepts_the_expired_late_testnet_20976659.json"));
}

/// Campaign 20 — X-14 iss issues s2 PAST with Expiration in the past (E668C6A65F59, network tecEXPIRED).
#[test]
fn c20_x_14_iss_issues_s2_past_with_expiration_in_the_past_testnet_20976663() {
    run_bundle(include_str!("vectors/c20_x_14_iss_issues_s2_past_with_expiration_in_the_past_testnet_20976663.json"));
}

/// Campaign 20 — D-4 dst re-authorizes set A (B647144D3BCC, network tesSUCCESS).
#[test]
fn c20_d_4_dst_re_authorizes_set_a_testnet_20976678() {
    run_bundle(include_str!("vectors/c20_d_4_dst_re_authorizes_set_a_testnet_20976678.json"));
}

/// Campaign 20 — D-5 del1 deletes into dst [del1 iss/KYC] = set A (66E3A0AC909F, network tesSUCCESS).
#[test]
fn c20_d_5_del1_deletes_into_dst_del1_iss_kyc_set_a_testnet_20976680() {
    run_bundle(include_str!("vectors/c20_d_5_del1_deletes_into_dst_del1_iss_kyc_set_a_testnet_20976680.json"));
}

/// Campaign 20 — E-2 NEW pays dst2 0.4 XRP (leaves ~1.09999 XRP at OwnerCount 0) (E1EDF2872745, network tesSUCCESS).
#[test]
fn c20_e_2_new_pays_dst2_0_4_xrp_leaves_1_09999_xrp_at_ownercount_0_testnet_20977010() {
    run_bundle(include_str!("vectors/c20_e_2_new_pays_dst2_0_4_xrp_leaves_1_09999_xrp_at_ownercount_0_testnet_20977010.json"));
}

/// Campaign 20 — E-3 iss issues NEW RSV expiring in ~25 s (6729E8DED6ED, network tesSUCCESS).
#[test]
fn c20_e_3_iss_issues_new_rsv_expiring_in_25_s_testnet_20977012() {
    run_bundle(include_str!("vectors/c20_e_3_iss_issues_new_rsv_expiring_in_25_s_testnet_20977012.json"));
}
