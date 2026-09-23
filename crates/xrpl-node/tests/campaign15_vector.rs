//! Campaign 15 (testnet, 2026-09-23) byte-exact vectors: AccountSet depth,
//! SetRegularKey, SignerListSet and the first MULTI-SIGNED transactions any
//! campaign sent. Findings 361-365 plus the SignerList read-set gap; the rest
//! pin rules that held (fee-0 key change, AccountTxnID stamps, DisallowIncoming
//! blocks, 32 signers, pre-fee reserve edges). Same harness as did_vector.rs.
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

/// Finding 361 — A-31 SetFlag 32 (out of range) (243548C0FC8C, network tesSUCCESS).
#[test]
fn c15_a_31_setflag_32_out_of_range_testnet_20976122() {
    run_bundle(include_str!("vectors/c15_a_31_setflag_32_out_of_range_testnet_20976122.json"));
}

/// Finding 361 — A-32 ClearFlag 99 (out of range) (A43E9649A176, network tesSUCCESS).
#[test]
fn c15_a_32_clearflag_99_out_of_range_testnet_20976124() {
    run_bundle(include_str!("vectors/c15_a_32_clearflag_99_out_of_range_testnet_20976124.json"));
}

/// Finding 362 — A-36 tx carrying AccountTxnID = 0^64 while the root has none (17BD6EF1A614, network tesSUCCESS).
#[test]
fn c15_a_36_tx_carrying_accounttxnid_0_64_while_the_root_has_none_testnet_20976132() {
    run_bundle(include_str!("vectors/c15_a_36_tx_carrying_accounttxnid_0_64_while_the_root_has_none_testnet_20976132.json"));
}

/// Finding 363 — R-16 tfRequireAuth + SetFlag asfNoFreeze by RK1, owner dir not empty (B2729B55BFFF, network tecOWNERS).
#[test]
fn c15_r_16_tfrequireauth_setflag_asfnofreeze_by_rk1_owner_dir_not_empty_testnet_20976195() {
    run_bundle(include_str!("vectors/c15_r_16_tfrequireauth_setflag_asfnofreeze_by_rk1_owner_dir_not_empty_testnet_20976195.json"));
}

/// Finding 363 — R-19 tfRequireAuth + SetFlag asfAllowTrustLineClawback by RK1 (NoFreeze, owns ticket) (B6030DD7D279, network tecOWNERS).
#[test]
fn c15_r_19_tfrequireauth_setflag_asfallowtrustlineclawback_by_rk1_nofreeze_owns_t_testnet_20976201() {
    run_bundle(include_str!("vectors/c15_r_19_tfrequireauth_setflag_asfallowtrustlineclawback_by_rk1_nofreeze_owns_t_testnet_20976201.json"));
}

/// Finding 363 — E-6 SetFlag asfAllowTrustLineClawback again (already set, owns a ticket) (3FD3C715789B, network tecOWNERS).
#[test]
fn c15_e_6_setflag_asfallowtrustlineclawback_again_already_set_owns_a_ticket_testnet_20976222() {
    run_bundle(include_str!("vectors/c15_e_6_setflag_asfallowtrustlineclawback_again_already_set_owns_a_ticket_testnet_20976222.json"));
}

/// Finding 364 — G-25 NFTokenMinter present WITHOUT SetFlag 10 (29318A81B45B, network tesSUCCESS).
#[test]
fn c15_g_25_nftokenminter_present_without_setflag_10_testnet_20976282() {
    run_bundle(include_str!("vectors/c15_g_25_nftokenminter_present_without_setflag_10_testnet_20976282.json"));
}

/// Finding 365 — M-16 SignerListSet delete, multi-signed, master disabled, no regular key (6E73BEFE2EAE, network tecNO_ALTERNATIVE_KEY).
#[test]
fn c15_m_16_signerlistset_delete_multi_signed_master_disabled_no_regular_key_testnet_20976356() {
    run_bundle(include_str!("vectors/c15_m_16_signerlistset_delete_multi_signed_master_disabled_no_regular_key_testnet_20976356.json"));
}

/// Campaign 15 read-set gap (SignerList) — M-9 SetFlag asfDisableMaster by master, NO regular key, signer list present (040971B3E857, network tesSUCCESS).
#[test]
fn c15_m_9_setflag_asfdisablemaster_by_master_no_regular_key_signer_list_present_testnet_20976342() {
    run_bundle(include_str!("vectors/c15_m_9_setflag_asfdisablemaster_by_master_no_regular_key_signer_list_present_testnet_20976342.json"));
}

/// Campaign 15 read-set gap (SignerList) — M-13 SetRegularKey remove, signed by RKD, master disabled, list present (FD411DDE751E, network tesSUCCESS).
#[test]
fn c15_m_13_setregularkey_remove_signed_by_rkd_master_disabled_list_present_testnet_20976350() {
    run_bundle(include_str!("vectors/c15_m_13_setregularkey_remove_signed_by_rkd_master_disabled_list_present_testnet_20976350.json"));
}

/// Campaign 15 — M-2 Payment D->B 1 XRP, 2-of-2 (ECE596B0C613, network tesSUCCESS).
#[test]
fn c15_m_2_payment_d_b_1_xrp_2_of_2_testnet_20976325() {
    run_bundle(include_str!("vectors/c15_m_2_payment_d_b_1_xrp_2_of_2_testnet_20976325.json"));
}

/// Campaign 15 — M-4 AccountSet Domain, 3 of [B,E,F] sign (over quorum) (F4D93B54179E, network tesSUCCESS).
#[test]
fn c15_m_4_accountset_domain_3_of_b_e_f_sign_over_quorum_testnet_20976329() {
    run_bundle(include_str!("vectors/c15_m_4_accountset_domain_3_of_b_e_f_sign_over_quorum_testnet_20976329.json"));
}

/// Campaign 15 — M-5 TrustSet BRV/B 50, multi-signed (35218D4EC1F2, network tesSUCCESS).
#[test]
fn c15_m_5_trustset_brv_b_50_multi_signed_testnet_20976334() {
    run_bundle(include_str!("vectors/c15_m_5_trustset_brv_b_50_multi_signed_testnet_20976334.json"));
}

/// Campaign 15 — M-7 Payment D->B: C signs with its REGULAR key (C master disabled) + phantom X master (EEC921509EA3, network tesSUCCESS).
#[test]
fn c15_m_7_payment_d_b_c_signs_with_its_regular_key_c_master_disabled_phantom_x_m_testnet_20976338() {
    run_bundle(include_str!("vectors/c15_m_7_payment_d_b_c_signs_with_its_regular_key_c_master_disabled_phantom_x_m_testnet_20976338.json"));
}

/// Campaign 15 — M-10 Payment D->B, multi-signed, master disabled (EE74CBE65D89, network tesSUCCESS).
#[test]
fn c15_m_10_payment_d_b_multi_signed_master_disabled_testnet_20976344() {
    run_bundle(include_str!("vectors/c15_m_10_payment_d_b_multi_signed_master_disabled_testnet_20976344.json"));
}

/// Campaign 15 — M-11 SetFlag asfNoFreeze, multi-signed, master disabled (FDABC000DF36, network tesSUCCESS).
#[test]
fn c15_m_11_setflag_asfnofreeze_multi_signed_master_disabled_testnet_20976346() {
    run_bundle(include_str!("vectors/c15_m_11_setflag_asfnofreeze_multi_signed_master_disabled_testnet_20976346.json"));
}

/// Campaign 15 — M-12 SetRegularKey RKD, multi-signed (1B1AE1E7BAE8, network tesSUCCESS).
#[test]
fn c15_m_12_setregularkey_rkd_multi_signed_testnet_20976348() {
    run_bundle(include_str!("vectors/c15_m_12_setregularkey_rkd_multi_signed_testnet_20976348.json"));
}

/// Campaign 15 — M-15 Payment D->B with Ticket, multi-signed (84D97503209D, network tesSUCCESS).
#[test]
fn c15_m_15_payment_d_b_with_ticket_multi_signed_testnet_20976354() {
    run_bundle(include_str!("vectors/c15_m_15_payment_d_b_with_ticket_multi_signed_testnet_20976354.json"));
}

/// Campaign 15 — M-19 SignerListSet [B,E] q1, multi-signed, with Ticket (A7F04A8F6BAF, network tesSUCCESS).
#[test]
fn c15_m_19_signerlistset_b_e_q1_multi_signed_with_ticket_testnet_20976366() {
    run_bundle(include_str!("vectors/c15_m_19_signerlistset_b_e_q1_multi_signed_with_ticket_testnet_20976366.json"));
}

/// Campaign 15 — M-20 Payment D->B signed by B alone (quorum 1) (47F1101D1917, network tesSUCCESS).
#[test]
fn c15_m_20_payment_d_b_signed_by_b_alone_quorum_1_testnet_20976369() {
    run_bundle(include_str!("vectors/c15_m_20_payment_d_b_signed_by_b_alone_quorum_1_testnet_20976369.json"));
}

/// Campaign 15 — R-1 SetRegularKey RK1, Fee 0, master (C2054124E343, network tesSUCCESS).
#[test]
fn c15_r_1_setregularkey_rk1_fee_0_master_testnet_20976169() {
    run_bundle(include_str!("vectors/c15_r_1_setregularkey_rk1_fee_0_master_testnet_20976169.json"));
}

/// Campaign 15 — R-3 B pays C 1 XRP (D0F0BCB1C9EE, network tesSUCCESS).
#[test]
fn c15_r_3_b_pays_c_1_xrp_testnet_20976172() {
    run_bundle(include_str!("vectors/c15_r_3_b_pays_c_1_xrp_testnet_20976172.json"));
}

/// Campaign 15 — R2-2 SetRegularKey RK2 signed by regular RK1, flag clear (B66180E120FD, network tesSUCCESS).
#[test]
fn c15_r2_2_setregularkey_rk2_signed_by_regular_rk1_flag_clear_testnet_20976378() {
    run_bundle(include_str!("vectors/c15_r2_2_setregularkey_rk2_signed_by_regular_rk1_flag_clear_testnet_20976378.json"));
}

/// Campaign 15 — R2-4 SetRegularKey RK1, master, NORMAL fee, flag clear (B77FCC2396A2, network tesSUCCESS).
#[test]
fn c15_r2_4_setregularkey_rk1_master_normal_fee_flag_clear_testnet_20976384() {
    run_bundle(include_str!("vectors/c15_r2_4_setregularkey_rk1_master_normal_fee_flag_clear_testnet_20976384.json"));
}

/// Campaign 15 — S-3 D 32 signers (28 phantom, WalletLocator on 11) q5 (EB567C81EC9D, network tesSUCCESS).
#[test]
fn c15_s_3_d_32_signers_28_phantom_walletlocator_on_11_q5_testnet_20976312() {
    run_bundle(include_str!("vectors/c15_s_3_d_32_signers_28_phantom_walletlocator_on_11_q5_testnet_20976312.json"));
}

/// Campaign 15 — S-5 D delete (quorum 0, no entries) (48CD05A55A31, network tesSUCCESS).
#[test]
fn c15_s_5_d_delete_quorum_0_no_entries_testnet_20976317() {
    run_bundle(include_str!("vectors/c15_s_5_d_delete_quorum_0_no_entries_testnet_20976317.json"));
}

/// Campaign 15 — F-2 F SignerListSet [B] q1 with pre-fee balance 1,199,999 (17F545C53291, network tecINSUFFICIENT_RESERVE).
#[test]
fn c15_f_2_f_signerlistset_b_q1_with_pre_fee_balance_1_199_999_testnet_20976297() {
    run_bundle(include_str!("vectors/c15_f_2_f_signerlistset_b_q1_with_pre_fee_balance_1_199_999_testnet_20976297.json"));
}

/// Campaign 15 — F-4 F SignerListSet [B] q1 with pre-fee balance exactly 1,200,000 (C12F66C5C641, network tesSUCCESS).
#[test]
fn c15_f_4_f_signerlistset_b_q1_with_pre_fee_balance_exactly_1_200_000_testnet_20976301() {
    run_bundle(include_str!("vectors/c15_f_4_f_signerlistset_b_q1_with_pre_fee_balance_exactly_1_200_000_testnet_20976301.json"));
}

/// Campaign 15 — F-5 F replaces list [B,E] q2 at pre-fee 1,199,988 (98F3081AA44A, network tecINSUFFICIENT_RESERVE).
#[test]
fn c15_f_5_f_replaces_list_b_e_q2_at_pre_fee_1_199_988_testnet_20976303() {
    run_bundle(include_str!("vectors/c15_f_5_f_replaces_list_b_e_q2_at_pre_fee_1_199_988_testnet_20976303.json"));
}

/// Campaign 15 — A-38 Payment A->B 1 XRP (stamps) (E821CCE79060, network tesSUCCESS).
#[test]
fn c15_a_38_payment_a_b_1_xrp_stamps_testnet_20976137() {
    run_bundle(include_str!("vectors/c15_a_38_payment_a_b_1_xrp_stamps_testnet_20976137.json"));
}

/// Campaign 15 — A-39 TrustSet A trusts BRV/B 100 (stamps) (8C4354620675, network tesSUCCESS).
#[test]
fn c15_a_39_trustset_a_trusts_brv_b_100_stamps_testnet_20976139() {
    run_bundle(include_str!("vectors/c15_a_39_trustset_a_trusts_brv_b_100_stamps_testnet_20976139.json"));
}

/// Campaign 15 — A-40 OfferCreate A: 1 XRP for 1 BRV (stamps, rests) (519EBEF7C609, network tesSUCCESS).
#[test]
fn c15_a_40_offercreate_a_1_xrp_for_1_brv_stamps_rests_testnet_20976141() {
    run_bundle(include_str!("vectors/c15_a_40_offercreate_a_1_xrp_for_1_brv_stamps_rests_testnet_20976141.json"));
}

/// Campaign 15 — A-41 Payment A->unfunded 0.5 XRP (5DEA0EC34872, network tecNO_DST_INSUF_XRP).
#[test]
fn c15_a_41_payment_a_unfunded_0_5_xrp_testnet_20976143() {
    run_bundle(include_str!("vectors/c15_a_41_payment_a_unfunded_0_5_xrp_testnet_20976143.json"));
}

/// Campaign 15 — G-2 B CheckCreate -> G (0BA64267F1E2, network tecNO_PERMISSION).
#[test]
fn c15_g_2_b_checkcreate_g_testnet_20976235() {
    run_bundle(include_str!("vectors/c15_g_2_b_checkcreate_g_testnet_20976235.json"));
}

/// Campaign 15 — G-12 B buy offer for G's NFT (Owner = G) (D7955530CF00, network tecNO_PERMISSION).
#[test]
fn c15_g_12_b_buy_offer_for_g_s_nft_owner_g_testnet_20976256() {
    run_bundle(include_str!("vectors/c15_g_12_b_buy_offer_for_g_s_nft_owner_g_testnet_20976256.json"));
}

/// Campaign 15 — G-13 B sell offer with Destination = G (BE5BCFADE4C8, network tecNO_PERMISSION).
#[test]
fn c15_g_13_b_sell_offer_with_destination_g_testnet_20976258() {
    run_bundle(include_str!("vectors/c15_g_13_b_sell_offer_with_destination_g_testnet_20976258.json"));
}

/// Campaign 15 — G-18 B TrustSet toward G (no line yet) (70BB33527EE1, network tecNO_PERMISSION).
#[test]
fn c15_g_18_b_trustset_toward_g_no_line_yet_testnet_20976268() {
    run_bundle(include_str!("vectors/c15_g_18_b_trustset_toward_g_no_line_yet_testnet_20976268.json"));
}

/// Campaign 15 — G-27 B mints on G's behalf after the clear (0AEED26CBC84, network tecNO_PERMISSION).
#[test]
fn c15_g_27_b_mints_on_g_s_behalf_after_the_clear_testnet_20976286() {
    run_bundle(include_str!("vectors/c15_g_27_b_mints_on_g_s_behalf_after_the_clear_testnet_20976286.json"));
}

/// Campaign 15 — E-4 SetFlag asfNoFreeze with clawback enabled (5A6DC1BB4574, network tecNO_PERMISSION).
#[test]
fn c15_e_4_setflag_asfnofreeze_with_clawback_enabled_testnet_20976218() {
    run_bundle(include_str!("vectors/c15_e_4_setflag_asfnofreeze_with_clawback_enabled_testnet_20976218.json"));
}

/// Campaign 15 — A-46 SetFlag asfRequireAuth with owner objects (74AAA7B459B8, network tecOWNERS).
#[test]
fn c15_a_46_setflag_asfrequireauth_with_owner_objects_testnet_20976151() {
    run_bundle(include_str!("vectors/c15_a_46_setflag_asfrequireauth_with_owner_objects_testnet_20976151.json"));
}

/// Campaign 15 — R-11 SetFlag asfDisableMaster, no alternative key (9AA77C30B68A, network tecNO_ALTERNATIVE_KEY).
#[test]
fn c15_r_11_setflag_asfdisablemaster_no_alternative_key_testnet_20976185() {
    run_bundle(include_str!("vectors/c15_r_11_setflag_asfdisablemaster_no_alternative_key_testnet_20976185.json"));
}

/// Campaign 15 — R-13 SetFlag asfDisableMaster signed by REGULAR key (5D60814DD502, network tecNEED_MASTER_KEY).
#[test]
fn c15_r_13_setflag_asfdisablemaster_signed_by_regular_key_testnet_20976189() {
    run_bundle(include_str!("vectors/c15_r_13_setflag_asfdisablemaster_signed_by_regular_key_testnet_20976189.json"));
}

/// Campaign 15 — R-21 SetRegularKey remove, master disabled, no signer list (70CF3A187605, network tecNO_ALTERNATIVE_KEY).
#[test]
fn c15_r_21_setregularkey_remove_master_disabled_no_signer_list_testnet_20976205() {
    run_bundle(include_str!("vectors/c15_r_21_setregularkey_remove_master_disabled_no_signer_list_testnet_20976205.json"));
}

/// Campaign 15 — A-47 ClearFlag asfRequireAuth (not set) (49EF0B32ED99, network tesSUCCESS).
#[test]
fn c15_a_47_clearflag_asfrequireauth_not_set_testnet_20976154() {
    run_bundle(include_str!("vectors/c15_a_47_clearflag_asfrequireauth_not_set_testnet_20976154.json"));
}
