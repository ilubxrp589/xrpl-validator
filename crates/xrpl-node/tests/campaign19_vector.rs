//! Campaign 19 (testnet, 2026-09-23) byte-exact vectors: NFTokenModify,
//! NFTokenCancelOffer, Oracle and DID depth. Findings 366-369 plus two read-set
//! gaps (the Oracle object; the NFT issuer's root); the rest pin rules that held
//! (keepRoot on the last object, offer-directory cleanup, no-op modifies, the
//! F358 series shapes, pre- vs post-fee reserve edges). Same harness as did_vector.rs.
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

/// Finding 366 — 3-25 oO creates doc10 LastUpdateTime = Cv-295 (34EAA88840C0, network tecINVALID_UPDATE_TIME).
#[test]
fn c19_3_25_oo_creates_doc10_lastupdatetime_cv_295_testnet_20976431() {
    run_bundle(include_str!("vectors/c19_3_25_oo_creates_doc10_lastupdatetime_cv_295_testnet_20976431.json"));
}

/// Finding 366 — 3-26 oO creates doc11 LastUpdateTime = Cv+309 (012EB6081351, network tesSUCCESS).
#[test]
fn c19_3_26_oo_creates_doc11_lastupdatetime_cv_309_testnet_20976433() {
    run_bundle(include_str!("vectors/c19_3_26_oo_creates_doc11_lastupdatetime_cv_309_testnet_20976433.json"));
}

/// Finding 367 — 4-3 oR doc1 price update, 6 pairs (adjust 0, balance < reserve(OC)) (A43A0B3BC882, network tecINSUFFICIENT_RESERVE).
#[test]
fn c19_4_3_or_doc1_price_update_6_pairs_adjust_0_balance_reserve_oc_testnet_20976446() {
    run_bundle(include_str!("vectors/c19_4_3_or_doc1_price_update_6_pairs_adjust_0_balance_reserve_oc_testnet_20976446.json"));
}

/// Finding 367 — 4-5 oR doc1 deletes a pair 6->5 (adjust -1, balance < reserve(OC-1)) (A28A4B0E5137, network tecINSUFFICIENT_RESERVE).
#[test]
fn c19_4_5_or_doc1_deletes_a_pair_6_5_adjust_1_balance_reserve_oc_1_testnet_20976450() {
    run_bundle(include_str!("vectors/c19_4_5_or_doc1_deletes_a_pair_6_5_adjust_1_balance_reserve_oc_1_testnet_20976450.json"));
}

/// Finding 367 — 4-6 oR doc1 adds a 7th pair (adjust 0, 2 units either way) (6A4298FE5581, network tecINSUFFICIENT_RESERVE).
#[test]
fn c19_4_6_or_doc1_adds_a_7th_pair_adjust_0_2_units_either_way_testnet_20976452() {
    run_bundle(include_str!("vectors/c19_4_6_or_doc1_adds_a_7th_pair_adjust_0_2_units_either_way_testnet_20976452.json"));
}

/// Finding 368 — 1-14 nA modifies T2 (not mutable) naming Owner=nB who does not hold it (1F074790F703, network tecNO_ENTRY).
#[test]
fn c19_1_14_na_modifies_t2_not_mutable_naming_owner_nb_who_does_not_hold_it_testnet_20976408() {
    run_bundle(include_str!("vectors/c19_1_14_na_modifies_t2_not_mutable_naming_owner_nb_who_does_not_hold_it_testnet_20976408.json"));
}

/// Finding 368 — 1-15 nX modifies T3 (mutable) naming Owner=nB who does not hold it (FB47610B1139, network tecNO_ENTRY).
#[test]
fn c19_1_15_nx_modifies_t3_mutable_naming_owner_nb_who_does_not_hold_it_testnet_20976410() {
    run_bundle(include_str!("vectors/c19_1_15_nx_modifies_t3_mutable_naming_owner_nb_who_does_not_hold_it_testnet_20976410.json"));
}

/// Finding 368 — 1-29 nA modifies burned T2 (not mutable, gone) (46C9265E592B, network tecNO_ENTRY).
#[test]
fn c19_1_29_na_modifies_burned_t2_not_mutable_gone_testnet_20976438() {
    run_bundle(include_str!("vectors/c19_1_29_na_modifies_burned_t2_not_mutable_gone_testnet_20976438.json"));
}

/// Finding 369 — 3-21 oO creates doc5 [noCurrency/USD, XRP/usd] (856F2F37D316, network tesSUCCESS).
#[test]
fn c19_3_21_oo_creates_doc5_nocurrency_usd_xrp_usd_testnet_20976422() {
    run_bundle(include_str!("vectors/c19_3_21_oo_creates_doc5_nocurrency_usd_xrp_usd_testnet_20976422.json"));
}

/// Finding 369 — 3-22 oO doc5 updates the noCurrency pair (0717E2C38424, network tesSUCCESS).
#[test]
fn c19_3_22_oo_doc5_updates_the_nocurrency_pair_testnet_20976424() {
    run_bundle(include_str!("vectors/c19_3_22_oo_doc5_updates_the_nocurrency_pair_testnet_20976424.json"));
}

/// Campaign 19 read-set gap (the Oracle object) — 3-5 oO doc1 adds AVX -> 11 pairs (AA255100C5C0, network tecARRAY_TOO_LARGE).
#[test]
fn c19_3_5_oo_doc1_adds_avx_11_pairs_testnet_20976387() {
    run_bundle(include_str!("vectors/c19_3_5_oo_doc1_adds_avx_11_pairs_testnet_20976387.json"));
}

/// Campaign 19 read-set gap (the Oracle object) — 3-7 oO doc1 LastUpdateTime == stored (6694EC01F4C9, network tecINVALID_UPDATE_TIME).
#[test]
fn c19_3_7_oo_doc1_lastupdatetime_stored_testnet_20976393() {
    run_bundle(include_str!("vectors/c19_3_7_oo_doc1_lastupdatetime_stored_testnet_20976393.json"));
}

/// Campaign 19 read-set gap (the Oracle object) — 3-11 oO doc1 names ZZZ/USD without price (2073AD6A9EF6, network tecTOKEN_PAIR_NOT_FOUND).
#[test]
fn c19_3_11_oo_doc1_names_zzz_usd_without_price_testnet_20976401() {
    run_bundle(include_str!("vectors/c19_3_11_oo_doc1_names_zzz_usd_without_price_testnet_20976401.json"));
}

/// Campaign 19 read-set gap (the NFT issuer's root) — 1-19 nB (holder, not issuer/minter) modifies T4 (DE9F978F2113, network tecNO_PERMISSION).
#[test]
fn c19_1_19_nb_holder_not_issuer_minter_modifies_t4_testnet_20976418() {
    run_bundle(include_str!("vectors/c19_1_19_nb_holder_not_issuer_minter_modifies_t4_testnet_20976418.json"));
}

/// Campaign 19 read-set gap (the NFT issuer's root) — 1-26 nM modifies T6 it holds after revocation (C554925EECB9, network tecNO_PERMISSION).
#[test]
fn c19_1_26_nm_modifies_t6_it_holds_after_revocation_testnet_20976432() {
    run_bundle(include_str!("vectors/c19_1_26_nm_modifies_t6_it_holds_after_revocation_testnet_20976432.json"));
}

/// Campaign 19 — 1-7 nA modifies T1 URI (change) in a 5-NFT page (3EEFDDD0063B, network tesSUCCESS).
#[test]
fn c19_1_7_na_modifies_t1_uri_change_in_a_5_nft_page_testnet_20976394() {
    run_bundle(include_str!("vectors/c19_1_7_na_modifies_t1_uri_change_in_a_5_nft_page_testnet_20976394.json"));
}

/// Campaign 19 — 1-8 nA sets a 256-byte URI on T3 (had none; 2-byte VL) (4312D255345E, network tesSUCCESS).
#[test]
fn c19_1_8_na_sets_a_256_byte_uri_on_t3_had_none_2_byte_vl_testnet_20976396() {
    run_bundle(include_str!("vectors/c19_1_8_na_sets_a_256_byte_uri_on_t3_had_none_2_byte_vl_testnet_20976396.json"));
}

/// Campaign 19 — 1-10 nA clears T1 URI again (already absent: page unchanged) (863DE667CB16, network tesSUCCESS).
#[test]
fn c19_1_10_na_clears_t1_uri_again_already_absent_page_unchanged_testnet_20976400() {
    run_bundle(include_str!("vectors/c19_1_10_na_clears_t1_uri_again_already_absent_page_unchanged_testnet_20976400.json"));
}

/// Campaign 19 — 1-13 nM (authorized minter) modifies T3 Owner=nA (FA5AE4287EF9, network tesSUCCESS).
#[test]
fn c19_1_13_nm_authorized_minter_modifies_t3_owner_na_testnet_20976406() {
    run_bundle(include_str!("vectors/c19_1_13_nm_authorized_minter_modifies_t3_owner_na_testnet_20976406.json"));
}

/// Campaign 19 — 1-18 nA (issuer) modifies T4 held by nB (Owner=nB) (A9E905694CAA, network tesSUCCESS).
#[test]
fn c19_1_18_na_issuer_modifies_t4_held_by_nb_owner_nb_testnet_20976416() {
    run_bundle(include_str!("vectors/c19_1_18_na_issuer_modifies_t4_held_by_nb_owner_nb_testnet_20976416.json"));
}

/// Campaign 19 — 1-30 nA modifies T3 back to a short URI (page with T1,T3,T7) (3D6113F58796, network tesSUCCESS).
#[test]
fn c19_1_30_na_modifies_t3_back_to_a_short_uri_page_with_t1_t3_t7_testnet_20976440() {
    run_bundle(include_str!("vectors/c19_1_30_na_modifies_t3_back_to_a_short_uri_page_with_t1_t3_t7_testnet_20976440.json"));
}

/// Campaign 19 — 2-16 nX (stranger) cancels unexpired S1 (F29DB5E281A3, network tecNO_PERMISSION).
#[test]
fn c19_2_16_nx_stranger_cancels_unexpired_s1_testnet_20976474() {
    run_bundle(include_str!("vectors/c19_2_16_nx_stranger_cancels_unexpired_s1_testnet_20976474.json"));
}

/// Campaign 19 — 2-18 nA (NFT holder) cancels nC buy B2 (no Destination) (D38909EDBA09, network tecNO_PERMISSION).
#[test]
fn c19_2_18_na_nft_holder_cancels_nc_buy_b2_no_destination_testnet_20976478() {
    run_bundle(include_str!("vectors/c19_2_18_na_nft_holder_cancels_nc_buy_b2_no_destination_testnet_20976478.json"));
}

/// Campaign 19 — 2-20 nA cancels [S1 own, B3 nD unexpired] (3807D83D939A, network tecNO_PERMISSION).
#[test]
fn c19_2_20_na_cancels_s1_own_b3_nd_unexpired_testnet_20976482() {
    run_bundle(include_str!("vectors/c19_2_20_na_cancels_s1_own_b3_nd_unexpired_testnet_20976482.json"));
}

/// Campaign 19 — 2-21 nA cancels [S1, nA AccountRoot index] (8B41272535BA, network tecNO_PERMISSION).
#[test]
fn c19_2_21_na_cancels_s1_na_accountroot_index_testnet_20976484() {
    run_bundle(include_str!("vectors/c19_2_21_na_cancels_s1_na_accountroot_index_testnet_20976484.json"));
}

/// Campaign 19 — 2-22 nX (stranger) cancels EXPIRED S3 (2AFD64566579, network tesSUCCESS).
#[test]
fn c19_2_22_nx_stranger_cancels_expired_s3_testnet_20976487() {
    run_bundle(include_str!("vectors/c19_2_22_nx_stranger_cancels_expired_s3_testnet_20976487.json"));
}

/// Campaign 19 — 2-24 nA cancels [S1, B4 (already gone), nonexistent id] (C868871E815E, network tesSUCCESS).
#[test]
fn c19_2_24_na_cancels_s1_b4_already_gone_nonexistent_id_testnet_20976491() {
    run_bundle(include_str!("vectors/c19_2_24_na_cancels_s1_b4_already_gone_nonexistent_id_testnet_20976491.json"));
}

/// Campaign 19 — 2-29 nC cancels [B2] (last buy offer: buy dir removed) (DCD88FD157CC, network tesSUCCESS).
#[test]
fn c19_2_29_nc_cancels_b2_last_buy_offer_buy_dir_removed_testnet_20976501() {
    run_bundle(include_str!("vectors/c19_2_29_nc_cancels_b2_last_buy_offer_buy_dir_removed_testnet_20976501.json"));
}

/// Campaign 19 — 3-6 oO doc1 deletes BTC ETH (no price), XRP/USD price w/o Scale, XRP/EUR Scale 5, new URI -> 8 (004779AF7165, network tesSUCCESS).
#[test]
fn c19_3_6_oo_doc1_deletes_btc_eth_no_price_xrp_usd_price_w_o_scale_xrp_eur_testnet_20976390() {
    run_bundle(include_str!("vectors/c19_3_6_oo_doc1_deletes_btc_eth_no_price_xrp_usd_price_w_o_scale_xrp_eur_testnet_20976390.json"));
}

/// Campaign 19 — 3-12 oO doc1 same Provider+AssetClass, ADA price, BTC re-added -> 9 (3467017C3E17, network tesSUCCESS).
#[test]
fn c19_3_12_oo_doc1_same_provider_assetclass_ada_price_btc_re_added_9_testnet_20976403() {
    run_bundle(include_str!("vectors/c19_3_12_oo_doc1_same_provider_assetclass_ada_price_btc_re_added_9_testnet_20976403.json"));
}

/// Campaign 19 — 3-14 oO creates doc2 with 6 pairs (XRP/USD EUR BTC/btc MYTOKEN/$$$ GBP JPY) (B26E37E9AEF5, network tesSUCCESS).
#[test]
fn c19_3_14_oo_creates_doc2_with_6_pairs_xrp_usd_eur_btc_btc_mytoken_gbp_jpy_testnet_20976406() {
    run_bundle(include_str!("vectors/c19_3_14_oo_creates_doc2_with_6_pairs_xrp_usd_eur_btc_btc_mytoken_gbp_jpy_testnet_20976406.json"));
}

/// Campaign 19 — 3-15 oO doc2 names all 6 without price (25C9EF7A004B, network tecARRAY_EMPTY).
#[test]
fn c19_3_15_oo_doc2_names_all_6_without_price_testnet_20976408() {
    run_bundle(include_str!("vectors/c19_3_15_oo_doc2_names_all_6_without_price_testnet_20976408.json"));
}

/// Campaign 19 — 3-17 oO deletes doc1 (9 pairs, 2 units) (EC6002337C5B, network tesSUCCESS).
#[test]
fn c19_3_17_oo_deletes_doc1_9_pairs_2_units_testnet_20976414() {
    run_bundle(include_str!("vectors/c19_3_17_oo_deletes_doc1_9_pairs_2_units_testnet_20976414.json"));
}

/// Campaign 19 — 3-24 oO deletes doc2 (7 pairs; last object, root kept) (0A3674FC5F77, network tesSUCCESS).
#[test]
fn c19_3_24_oo_deletes_doc2_7_pairs_last_object_root_kept_testnet_20976429() {
    run_bundle(include_str!("vectors/c19_3_24_oo_deletes_doc2_7_pairs_last_object_root_kept_testnet_20976429.json"));
}

/// Campaign 19 — 4-9 oR creates doc2 1 pair (balance < reserve(1)) (D78927AD36B8, network tecINSUFFICIENT_RESERVE).
#[test]
fn c19_4_9_or_creates_doc2_1_pair_balance_reserve_1_testnet_20976458() {
    run_bundle(include_str!("vectors/c19_4_9_or_creates_doc2_1_pair_balance_reserve_1_testnet_20976458.json"));
}

/// Campaign 19 — 5-2 dJ DIDSet create: pre-fee >= reserve, post-fee < reserve (1507FE6E0DAD, network tecINSUFFICIENT_RESERVE).
#[test]
fn c19_5_2_dj_didset_create_pre_fee_reserve_post_fee_reserve_testnet_20976462() {
    run_bundle(include_str!("vectors/c19_5_2_dj_didset_create_pre_fee_reserve_post_fee_reserve_testnet_20976462.json"));
}

/// Campaign 19 — 5-4 dJ OracleSet create: pre-fee balance == reserve(OC+1) (92B605BAFB05, network tesSUCCESS).
#[test]
fn c19_5_4_dj_oracleset_create_pre_fee_balance_reserve_oc_1_testnet_20976466() {
    run_bundle(include_str!("vectors/c19_5_4_dj_oracleset_create_pre_fee_balance_reserve_oc_1_testnet_20976466.json"));
}

/// Campaign 19 — 6-6 dI DIDSet DIDDocument="" Data="" would leave nothing (CC2A61B11DAB, network tecEMPTY_DID).
#[test]
fn c19_6_6_di_didset_diddocument_data_would_leave_nothing_testnet_20976390() {
    run_bundle(include_str!("vectors/c19_6_6_di_didset_diddocument_data_would_leave_nothing_testnet_20976390.json"));
}

/// Campaign 19 — 6-9 dI DIDSet Data = current value (no-op) (8DDC9D557C5A, network tesSUCCESS).
#[test]
fn c19_6_9_di_didset_data_current_value_no_op_testnet_20976395() {
    run_bundle(include_str!("vectors/c19_6_9_di_didset_data_current_value_no_op_testnet_20976395.json"));
}

/// Campaign 19 — 6-10 dI DIDDelete (only object; owner dir root kept) (4C9C11542AEE, network tesSUCCESS).
#[test]
fn c19_6_10_di_diddelete_only_object_owner_dir_root_kept_testnet_20976397() {
    run_bundle(include_str!("vectors/c19_6_10_di_diddelete_only_object_owner_dir_root_kept_testnet_20976397.json"));
}

/// Campaign 19 — 6-12 dI DIDSet URI="" only (create path) (4D96A44279CB, network tecEMPTY_DID).
#[test]
fn c19_6_12_di_didset_uri_only_create_path_testnet_20976401() {
    run_bundle(include_str!("vectors/c19_6_12_di_didset_uri_only_create_path_testnet_20976401.json"));
}

/// Campaign 19 — 6-13 dI DIDSet URI="" + Data (create: Data only) (B1122BD867E0, network tesSUCCESS).
#[test]
fn c19_6_13_di_didset_uri_data_create_data_only_testnet_20976403() {
    run_bundle(include_str!("vectors/c19_6_13_di_didset_uri_data_create_data_only_testnet_20976403.json"));
}

/// Campaign 19 — 6-16 dI DIDSet all three (create) (01593C727A15, network tesSUCCESS).
#[test]
fn c19_6_16_di_didset_all_three_create_testnet_20976409() {
    run_bundle(include_str!("vectors/c19_6_16_di_didset_all_three_create_testnet_20976409.json"));
}
