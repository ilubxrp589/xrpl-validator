//! Campaign 17 (testnet, 2026-09-23) byte-exact vectors: the OfferCreate flag
//! matrix — tfPassive, tfImmediateOrCancel, tfFillOrKill (fixFillOrKill),
//! tfSell, TickSize rounding, TransferRate, self-crossing, expiration,
//! OfferSequence, unfunded / frozen makers, reserve edges, GlobalFreeze and
//! deep freeze. Finding 381 (OfferCreate tecFROZEN). Two port-only specimens
//! live in port_flow_vector.rs. Same harness as did_vector.rs.
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

/// Finding 381 — 8-2 tk sells 5 EUR for 5 USD (TakerGets globally frozen) (57ABBA764293, network tecFROZEN).
#[test]
fn c17_8_2_tk_sells_5_eur_for_5_usd_takergets_globally_frozen_testnet_20976728() {
    run_bundle(include_str!("vectors/c17_8_2_tk_sells_5_eur_for_5_usd_takergets_globally_frozen_testnet_20976728.json"));
}

/// Finding 381 — 8-3 tk buys 5 EUR for 3000000d (TakerPays globally frozen) (B8D0F0F209E4, network tecFROZEN).
#[test]
fn c17_8_3_tk_buys_5_eur_for_3000000d_takerpays_globally_frozen_testnet_20976730() {
    run_bundle(include_str!("vectors/c17_8_3_tk_buys_5_eur_for_3000000d_takerpays_globally_frozen_testnet_20976730.json"));
}

/// Finding 381 — 8-4 ib sells its own EUR for XRP while globally frozen (0B844A627E01, network tecFROZEN).
#[test]
fn c17_8_4_ib_sells_its_own_eur_for_xrp_while_globally_frozen_testnet_20976732() {
    run_bundle(include_str!("vectors/c17_8_4_ib_sells_its_own_eur_for_xrp_while_globally_frozen_testnet_20976732.json"));
}

/// Finding 381 — 8b-2 tk bids 5 EUR for 3000000d with Expiration already past (EUR globally frozen) (E9449A3E0633, network tecFROZEN).
#[test]
fn c17_8b_2_tk_bids_5_eur_for_3000000d_with_expiration_already_past_testnet_20977008() {
    run_bundle(include_str!("vectors/c17_8b_2_tk_bids_5_eur_for_3000000d_with_expiration_already_past_testnet_20977008.json"));
}

/// Finding 381 — 8b-3 rv (no EUR line at all) offers 1 EUR for 100000d (EUR globally frozen) (8E8974E1A4E0, network tecFROZEN).
#[test]
fn c17_8b_3_rv_no_eur_line_at_all_offers_1_eur_for_100000d_eur_globa_testnet_20977010() {
    run_bundle(include_str!("vectors/c17_8b_3_rv_no_eur_line_at_all_offers_1_eur_for_100000d_eur_globa_testnet_20977010.json"));
}

/// Finding 381 — 7-14 fz (deep frozen) bids 1 USD for 600000d (A7815067D548, network tecFROZEN).
#[test]
fn c17_7_14_fz_deep_frozen_bids_1_usd_for_600000d_testnet_20976694() {
    run_bundle(include_str!("vectors/c17_7_14_fz_deep_frozen_bids_1_usd_for_600000d_testnet_20976694.json"));
}

/// Finding 381 — 8b-6 dz bids 1 USD for 500000d (holder-side deep freeze on its TakerPays line) (5467E2F0F3EB, network tecFROZEN).
#[test]
fn c17_8b_6_dz_bids_1_usd_for_500000d_holder_side_deep_freeze_on_its_testnet_20977016() {
    run_bundle(include_str!("vectors/c17_8b_6_dz_bids_1_usd_for_500000d_holder_side_deep_freeze_on_its_testnet_20977016.json"));
}

/// Campaign 17 — 1-1 m1 asks 37.301075 USD for 18650537d (O1) (7529815E0554, network tesSUCCESS).
#[test]
fn c17_1_1_m1_asks_37_301075_usd_for_18650537d_o1_testnet_20976472() {
    run_bundle(include_str!("vectors/c17_1_1_m1_asks_37_301075_usd_for_18650537d_o1_testnet_20976472.json"));
}

/// Campaign 17 — 1-2 m2 asks 20.5 USD for 11275000d (O2, 550000/USD) (11DEBEE7F0A2, network tesSUCCESS).
#[test]
fn c17_1_2_m2_asks_20_5_usd_for_11275000d_o2_550000_usd_testnet_20976474() {
    run_bundle(include_str!("vectors/c17_1_2_m2_asks_20_5_usd_for_11275000d_o2_550000_usd_testnet_20976474.json"));
}

/// Campaign 17 — 1-3 tk PASSIVE bid at O1's exact quality (37.301075 USD for 18650537d) (CFA182E2ACAB, network tesSUCCESS).
#[test]
fn c17_1_3_tk_passive_bid_at_o1_s_exact_quality_37_301075_usd_for_1_testnet_20976476() {
    run_bundle(include_str!("vectors/c17_1_3_tk_passive_bid_at_o1_s_exact_quality_37_301075_usd_for_1_testnet_20976476.json"));
}

/// Campaign 17 — 1-4 tk PASSIVE bid 10 USD for 5200000d (better than O1) (75FE07DE32E8, network tesSUCCESS).
#[test]
fn c17_1_4_tk_passive_bid_10_usd_for_5200000d_better_than_o1_testnet_20976478() {
    run_bundle(include_str!("vectors/c17_1_4_tk_passive_bid_10_usd_for_5200000d_better_than_o1_testnet_20976478.json"));
}

/// Campaign 17 — 1-5 tk IOC bid 30 USD for 15300000d (510000/USD): O1 rest only (7CD41A0517A5, network tesSUCCESS).
#[test]
fn c17_1_5_tk_ioc_bid_30_usd_for_15300000d_510000_usd_o1_rest_only_testnet_20976482() {
    run_bundle(include_str!("vectors/c17_1_5_tk_ioc_bid_30_usd_for_15300000d_510000_usd_o1_rest_only_testnet_20976482.json"));
}

/// Campaign 17 — 1-6 tk IOC bid 5 USD for 2000000d (400000/USD): nothing crossable (570AE515A3F5, network tecKILLED).
#[test]
fn c17_1_6_tk_ioc_bid_5_usd_for_2000000d_400000_usd_nothing_crossab_testnet_20976484() {
    run_bundle(include_str!("vectors/c17_1_6_tk_ioc_bid_5_usd_for_2000000d_400000_usd_nothing_crossab_testnet_20976484.json"));
}

/// Campaign 17 — 1-7 tk IOC bid 5 USD for 3000000d (600000/USD): fully filled from O2 (ED4FD3E84091, network tesSUCCESS).
#[test]
fn c17_1_7_tk_ioc_bid_5_usd_for_3000000d_600000_usd_fully_filled_fr_testnet_20976487() {
    run_bundle(include_str!("vectors/c17_1_7_tk_ioc_bid_5_usd_for_3000000d_600000_usd_fully_filled_fr_testnet_20976487.json"));
}

/// Campaign 17 — 1-8 m1 asks 12.345678 USD for 6172839d (O3) (24806E76AB78, network tesSUCCESS).
#[test]
fn c17_1_8_m1_asks_12_345678_usd_for_6172839d_o3_testnet_20976489() {
    run_bundle(include_str!("vectors/c17_1_8_m1_asks_12_345678_usd_for_6172839d_o3_testnet_20976489.json"));
}

/// Campaign 17 — 1-9 tk FoK bid 10 USD for 6000000d: fillable from O3 (TakerGets not all spent) (25930E84C9CA, network tesSUCCESS).
#[test]
fn c17_1_9_tk_fok_bid_10_usd_for_6000000d_fillable_from_o3_takerget_testnet_20976491() {
    run_bundle(include_str!("vectors/c17_1_9_tk_fok_bid_10_usd_for_6000000d_fillable_from_o3_takerget_testnet_20976491.json"));
}

/// Campaign 17 — 1-10 tk FoK bid 50 USD for 26000000d (520000/USD): only 2.345678 crossable (2314A7668C20, network tecKILLED).
#[test]
fn c17_1_10_tk_fok_bid_50_usd_for_26000000d_520000_usd_only_2_345678_testnet_20976493() {
    run_bundle(include_str!("vectors/c17_1_10_tk_fok_bid_50_usd_for_26000000d_520000_usd_only_2_345678_testnet_20976493.json"));
}

/// Campaign 17 — 1-11 tk FoK|Sell 1000000d for >=1.8 USD: O3 rest takes all 1000000d (05F45E20A275, network tesSUCCESS).
#[test]
fn c17_1_11_tk_fok_sell_1000000d_for_1_8_usd_o3_rest_takes_all_10000_testnet_20976495() {
    run_bundle(include_str!("vectors/c17_1_11_tk_fok_sell_1000000d_for_1_8_usd_o3_rest_takes_all_10000_testnet_20976495.json"));
}

/// Campaign 17 — 1-14 m1 asks 30 USD for 12000000d (O4, 400000/USD) (74AA6423F836, network tesSUCCESS).
#[test]
fn c17_1_14_m1_asks_30_usd_for_12000000d_o4_400000_usd_testnet_20976498() {
    run_bundle(include_str!("vectors/c17_1_14_m1_asks_30_usd_for_12000000d_o4_400000_usd_testnet_20976498.json"));
}

/// Campaign 17 — 1-15 tk Sell 4000000d for >=8 USD: receives 10 USD (AA4FEF18D68A, network tesSUCCESS).
#[test]
fn c17_1_15_tk_sell_4000000d_for_8_usd_receives_10_usd_testnet_20976500() {
    run_bundle(include_str!("vectors/c17_1_15_tk_sell_4000000d_for_8_usd_receives_10_usd_testnet_20976500.json"));
}

/// Campaign 17 — 2-1 m1 asks 50 EUR for 55.55555 USD (6D4A8E7C15E8, network tesSUCCESS).
#[test]
fn c17_2_1_m1_asks_50_eur_for_55_55555_usd_testnet_20976528() {
    run_bundle(include_str!("vectors/c17_2_1_m1_asks_50_eur_for_55_55555_usd_testnet_20976528.json"));
}

/// Campaign 17 — 2-2 m2 asks 30 EUR for 34.5 USD (1.15) (9FE35E908EB2, network tesSUCCESS).
#[test]
fn c17_2_2_m2_asks_30_eur_for_34_5_usd_1_15_testnet_20976531() {
    run_bundle(include_str!("vectors/c17_2_2_m2_asks_30_eur_for_34_5_usd_1_15_testnet_20976531.json"));
}

/// Campaign 17 — 2-3 tk PASSIVE bid 50 EUR for 55.55555 USD (equal to m1) (FB2F1063EFF7, network tesSUCCESS).
#[test]
fn c17_2_3_tk_passive_bid_50_eur_for_55_55555_usd_equal_to_m1_testnet_20976533() {
    run_bundle(include_str!("vectors/c17_2_3_tk_passive_bid_50_eur_for_55_55555_usd_equal_to_m1_testnet_20976533.json"));
}

/// Campaign 17 — 2-4 tk IOC bid 60 EUR for 67.5 USD (1.125) (AF3FF56205E2, network tesSUCCESS).
#[test]
fn c17_2_4_tk_ioc_bid_60_eur_for_67_5_usd_1_125_testnet_20976535() {
    run_bundle(include_str!("vectors/c17_2_4_tk_ioc_bid_60_eur_for_67_5_usd_1_125_testnet_20976535.json"));
}

/// Campaign 17 — 2-5 tk FoK bid 40 EUR for 44 USD (1.1) (7C59BC90321D, network tecKILLED).
#[test]
fn c17_2_5_tk_fok_bid_40_eur_for_44_usd_1_1_testnet_20976537() {
    run_bundle(include_str!("vectors/c17_2_5_tk_fok_bid_40_eur_for_44_usd_1_1_testnet_20976537.json"));
}

/// Campaign 17 — 2-6 tk FoK|Sell 11.5 USD for >=9 EUR (D6321279CC52, network tesSUCCESS).
#[test]
fn c17_2_6_tk_fok_sell_11_5_usd_for_9_eur_testnet_20976540() {
    run_bundle(include_str!("vectors/c17_2_6_tk_fok_sell_11_5_usd_for_9_eur_testnet_20976540.json"));
}

/// Campaign 17 — 3-1 dz (holds 1e-15 USD) asks 10 USD for 11 EUR (D1) (2684C1FCCB7D, network tesSUCCESS).
#[test]
fn c17_3_1_dz_holds_1e_15_usd_asks_10_usd_for_11_eur_d1_testnet_20976549() {
    run_bundle(include_str!("vectors/c17_3_1_dz_holds_1e_15_usd_asks_10_usd_for_11_eur_d1_testnet_20976549.json"));
}

/// Campaign 17 — 3-3 tk bid 40 USD for 50 EUR (plain): dust cross (53F0B19974EF, network tesSUCCESS).
#[test]
fn c17_3_3_tk_bid_40_usd_for_50_eur_plain_dust_cross_testnet_20976553() {
    run_bundle(include_str!("vectors/c17_3_3_tk_bid_40_usd_for_50_eur_plain_dust_cross_testnet_20976553.json"));
}

/// Campaign 17 — 4-1 m1 bid 7.777777 GBP for 2345678d (tick5 on pays side, buy rounds TakerGets) (2953A211E620, network tesSUCCESS).
#[test]
fn c17_4_1_m1_bid_7_777777_gbp_for_2345678d_tick5_on_pays_side_buy_testnet_20976559() {
    run_bundle(include_str!("vectors/c17_4_1_m1_bid_7_777777_gbp_for_2345678d_tick5_on_pays_side_buy_testnet_20976559.json"));
}

/// Campaign 17 — 4-2 m1 Sell ask 9.87654321 GBP for 4938271d (tick5 on gets side, sell rounds TakerPays) (C928E6624378, network tesSUCCESS).
#[test]
fn c17_4_2_m1_sell_ask_9_87654321_gbp_for_4938271d_tick5_on_gets_si_testnet_20976561() {
    run_bundle(include_str!("vectors/c17_4_2_m1_sell_ask_9_87654321_gbp_for_4938271d_tick5_on_gets_si_testnet_20976561.json"));
}

/// Campaign 17 — 4-3 m2 bid 9.999999 GBP for 1000000d (rate 9.999999e-6 -> carry 1.0000e-5) (F3184B73E44E, network tesSUCCESS).
#[test]
fn c17_4_3_m2_bid_9_999999_gbp_for_1000000d_rate_9_999999e_6_carry_testnet_20976563() {
    run_bundle(include_str!("vectors/c17_4_3_m2_bid_9_999999_gbp_for_1000000d_rate_9_999999e_6_carry_testnet_20976563.json"));
}

/// Campaign 17 — 4-4 m2 bid 0.000000123456789 GBP for 1d (rounds to zero drops) (93A041B0E243, network tesSUCCESS).
#[test]
fn c17_4_4_m2_bid_0_000000123456789_gbp_for_1d_rounds_to_zero_drops_testnet_20976566() {
    run_bundle(include_str!("vectors/c17_4_4_m2_bid_0_000000123456789_gbp_for_1d_rounds_to_zero_drops_testnet_20976566.json"));
}

/// Campaign 17 — 4-5 m1 gives 7.654321 GBP for 123.4567 JPY (tick min(5,3)=3) (DBB2705443A9, network tesSUCCESS).
#[test]
fn c17_4_5_m1_gives_7_654321_gbp_for_123_4567_jpy_tick_min_5_3_3_testnet_20976569() {
    run_bundle(include_str!("vectors/c17_4_5_m1_gives_7_654321_gbp_for_123_4567_jpy_tick_min_5_3_3_testnet_20976569.json"));
}

/// Campaign 17 — 4-6 m2 Sell 200 JPY for >=12.3456 GBP (tick3, both rates): crosses 4-5, remainder rests (AAEEA81E03DC, network tesSUCCESS).
#[test]
fn c17_4_6_m2_sell_200_jpy_for_12_3456_gbp_tick3_both_rates_crosses_testnet_20976571() {
    run_bundle(include_str!("vectors/c17_4_6_m2_sell_200_jpy_for_12_3456_gbp_tick3_both_rates_crosses_testnet_20976571.json"));
}

/// Campaign 17 — 4-8 tk FoK|Sell 1.234567 GBP for >=100000d (rate 1.25) (A2EB74A88DD6, network tesSUCCESS).
#[test]
fn c17_4_8_tk_fok_sell_1_234567_gbp_for_100000d_rate_1_25_testnet_20976575() {
    run_bundle(include_str!("vectors/c17_4_8_tk_fok_sell_1_234567_gbp_for_100000d_rate_1_25_testnet_20976575.json"));
}

/// Campaign 17 — 4-9 tk FoK|Sell 15 GBP for >=100000d (sendMax 18.75 > bids) (80C9FB7E27A7, network tecKILLED).
#[test]
fn c17_4_9_tk_fok_sell_15_gbp_for_100000d_sendmax_18_75_bids_testnet_20976577() {
    run_bundle(include_str!("vectors/c17_4_9_tk_fok_sell_15_gbp_for_100000d_sendmax_18_75_bids_testnet_20976577.json"));
}

/// Campaign 17 — 4-10 tk bid 12.3456789 GBP for 6543210d (tick5): crosses 4-2, remainder rests (9DAC2D9EC77C, network tesSUCCESS).
#[test]
fn c17_4_10_tk_bid_12_3456789_gbp_for_6543210d_tick5_crosses_4_2_rem_testnet_20976579() {
    run_bundle(include_str!("vectors/c17_4_10_tk_bid_12_3456789_gbp_for_6543210d_tick5_crosses_4_2_rem_testnet_20976579.json"));
}

/// Campaign 17 — 4-11 m2 gives 2.718281 GBP for 3.141592 USD (tick5 on GBP gets side) (15DD0E5EC144, network tesSUCCESS).
#[test]
fn c17_4_11_m2_gives_2_718281_gbp_for_3_141592_usd_tick5_on_gbp_gets_testnet_20976581() {
    run_bundle(include_str!("vectors/c17_4_11_m2_gives_2_718281_gbp_for_3_141592_usd_tick5_on_gbp_gets_testnet_20976581.json"));
}

/// Campaign 17 — 4-12 tk Sell 2.5 USD for >=1.987654321 GBP (tick5 on pays side): crosses 4-11 (E1BDBFF303B6, network tesSUCCESS).
#[test]
fn c17_4_12_tk_sell_2_5_usd_for_1_987654321_gbp_tick5_on_pays_side_c_testnet_20976583() {
    run_bundle(include_str!("vectors/c17_4_12_tk_sell_2_5_usd_for_1_987654321_gbp_tick5_on_pays_side_c_testnet_20976583.json"));
}

/// Campaign 17 — 5-1 sx asks 10 USD for 5000000d (A1) (97FCFD300A57, network tesSUCCESS).
#[test]
fn c17_5_1_sx_asks_10_usd_for_5000000d_a1_testnet_20976600() {
    run_bundle(include_str!("vectors/c17_5_1_sx_asks_10_usd_for_5000000d_a1_testnet_20976600.json"));
}

/// Campaign 17 — 5-2 sx bids 10 USD for 5000000d (equal quality to own A1) (959FC73CDDD4, network tesSUCCESS).
#[test]
fn c17_5_2_sx_bids_10_usd_for_5000000d_equal_quality_to_own_a1_testnet_20976602() {
    run_bundle(include_str!("vectors/c17_5_2_sx_bids_10_usd_for_5000000d_equal_quality_to_own_a1_testnet_20976602.json"));
}

/// Campaign 17 — 5-4 sx IOC bid 10 USD for 5500000d (own passive ask is better) (E5A51AD1AC8C, network tecKILLED).
#[test]
fn c17_5_4_sx_ioc_bid_10_usd_for_5500000d_own_passive_ask_is_better_testnet_20976606() {
    run_bundle(include_str!("vectors/c17_5_4_sx_ioc_bid_10_usd_for_5500000d_own_passive_ask_is_better_testnet_20976606.json"));
}

/// Campaign 17 — 5-6 sx FoK bid 10 USD for 5500000d (only own ask in the book) (5E606961671A, network tecKILLED).
#[test]
fn c17_5_6_sx_fok_bid_10_usd_for_5500000d_only_own_ask_in_the_book_testnet_20976610() {
    run_bundle(include_str!("vectors/c17_5_6_sx_fok_bid_10_usd_for_5500000d_only_own_ask_in_the_book_testnet_20976610.json"));
}

/// Campaign 17 — 5-8 sx B2 asks 10 EUR for 5000000d (D75CAB747F49, network tesSUCCESS).
#[test]
fn c17_5_8_sx_b2_asks_10_eur_for_5000000d_testnet_20976614() {
    run_bundle(include_str!("vectors/c17_5_8_sx_b2_asks_10_eur_for_5000000d_testnet_20976614.json"));
}

/// Campaign 17 — 6-1 m1 asks 10 USD for 4000000d, Expiration +8 (E1) (6D761A1E9188, network tecEXPIRED).
#[test]
fn c17_6_1_m1_asks_10_usd_for_4000000d_expiration_8_e1_testnet_20976625() {
    run_bundle(include_str!("vectors/c17_6_1_m1_asks_10_usd_for_4000000d_expiration_8_e1_testnet_20976625.json"));
}

/// Campaign 17 — 6-3 tk IOC bid 5 USD for 2050000d (0.41): E1 expired, E2 above limit (B1F03330D19C, network tecKILLED).
#[test]
fn c17_6_3_tk_ioc_bid_5_usd_for_2050000d_0_41_e1_expired_e2_above_l_testnet_20976629() {
    run_bundle(include_str!("vectors/c17_6_3_tk_ioc_bid_5_usd_for_2050000d_0_41_e1_expired_e2_above_l_testnet_20976629.json"));
}

/// Campaign 17 — 6-5 tk bid 5 USD for 2250000d (0.45): E3 expired (removed, not filled), E2 fills (E6320651B44C, network tesSUCCESS).
#[test]
fn c17_6_5_tk_bid_5_usd_for_2250000d_0_45_e3_expired_removed_not_fi_testnet_20976637() {
    run_bundle(include_str!("vectors/c17_6_5_tk_bid_5_usd_for_2250000d_0_45_e3_expired_removed_not_fi_testnet_20976637.json"));
}

/// Campaign 17 — 6-6 tk bid with Expiration already past (FD12DAF1F1B8, network tecEXPIRED).
#[test]
fn c17_6_6_tk_bid_with_expiration_already_past_testnet_20976639() {
    run_bundle(include_str!("vectors/c17_6_6_tk_bid_with_expiration_already_past_testnet_20976639.json"));
}

/// Campaign 17 — 6-8 tk replaces X1 (OfferSequence) with bid 4 USD for 1240000d (X2) (93E442DE0FA7, network tesSUCCESS).
#[test]
fn c17_6_8_tk_replaces_x1_offersequence_with_bid_4_usd_for_1240000d_testnet_20976643() {
    run_bundle(include_str!("vectors/c17_6_8_tk_replaces_x1_offersequence_with_bid_4_usd_for_1240000d_testnet_20976643.json"));
}

/// Campaign 17 — 6-10 tk IOC bid 5 USD for 1500000d (nothing crossable) + OfferSequence=X2 (8BC7444A6FAC, network tecKILLED).
#[test]
fn c17_6_10_tk_ioc_bid_5_usd_for_1500000d_nothing_crossable_offerseq_testnet_20976649() {
    run_bundle(include_str!("vectors/c17_6_10_tk_ioc_bid_5_usd_for_1500000d_nothing_crossable_offerseq_testnet_20976649.json"));
}

/// Campaign 17 — 6-11 tk FoK bid 5 USD for 1500000d + OfferSequence=X2 (8859E325B00F, network tecKILLED).
#[test]
fn c17_6_11_tk_fok_bid_5_usd_for_1500000d_offersequence_x2_testnet_20976651() {
    run_bundle(include_str!("vectors/c17_6_11_tk_fok_bid_5_usd_for_1500000d_offersequence_x2_testnet_20976651.json"));
}

/// Campaign 17 — 6b-3 tk IOC bid 5 USD for 2050000d (0.41): E1 expired, E2 above limit (BD03D9C0E0C4, network tecKILLED).
#[test]
fn c17_6b_3_tk_ioc_bid_5_usd_for_2050000d_0_41_e1_expired_e2_above_l_testnet_20976768() {
    run_bundle(include_str!("vectors/c17_6b_3_tk_ioc_bid_5_usd_for_2050000d_0_41_e1_expired_e2_above_l_testnet_20976768.json"));
}

/// Campaign 17 — 7-1 m1 asks 5 USD for 2000000d (0.40) (B86F56BA8A1C, network tesSUCCESS).
#[test]
fn c17_7_1_m1_asks_5_usd_for_2000000d_0_40_testnet_20976667() {
    run_bundle(include_str!("vectors/c17_7_1_m1_asks_5_usd_for_2000000d_0_40_testnet_20976667.json"));
}

/// Campaign 17 — 7-3 m2 asks 5 USD for 2200000d (0.44) (BC5C9CC46FD0, network tesSUCCESS).
#[test]
fn c17_7_3_m2_asks_5_usd_for_2200000d_0_44_testnet_20976671() {
    run_bundle(include_str!("vectors/c17_7_3_m2_asks_5_usd_for_2200000d_0_44_testnet_20976671.json"));
}

/// Campaign 17 — 7-5 ia freezes fz's USD line (797F72643B14, network tesSUCCESS).
#[test]
fn c17_7_5_ia_freezes_fz_s_usd_line_testnet_20976676() {
    run_bundle(include_str!("vectors/c17_7_5_ia_freezes_fz_s_usd_line_testnet_20976676.json"));
}

/// Campaign 17 — 7-12 ia deep-freezes fz's USD line (7A371F660A45, network tesSUCCESS).
#[test]
fn c17_7_12_ia_deep_freezes_fz_s_usd_line_testnet_20976690() {
    run_bundle(include_str!("vectors/c17_7_12_ia_deep_freezes_fz_s_usd_line_testnet_20976690.json"));
}

/// Campaign 17 — 7-13 tk IOC asks 5 USD for 2000000d: only bid is fz's (deep frozen) (B7F5458BCF85, network tecKILLED).
#[test]
fn c17_7_13_tk_ioc_asks_5_usd_for_2000000d_only_bid_is_fz_s_deep_fro_testnet_20976692() {
    run_bundle(include_str!("vectors/c17_7_13_tk_ioc_asks_5_usd_for_2000000d_only_bid_is_fz_s_deep_fro_testnet_20976692.json"));
}

/// Campaign 17 — 7-15 fz (frozen) asks 1 USD for 400000d (BFFD15953F85, network tecUNFUNDED_OFFER).
#[test]
fn c17_7_15_fz_frozen_asks_1_usd_for_400000d_testnet_20976697() {
    run_bundle(include_str!("vectors/c17_7_15_fz_frozen_asks_1_usd_for_400000d_testnet_20976697.json"));
}

/// Campaign 17 — 7b-3 tk FoK bid 5 USD for 2025000d (0.405): only U2 (unfunded) below the limit (5189FE7900B4, network tecKILLED).
#[test]
fn c17_7b_3_tk_fok_bid_5_usd_for_2025000d_0_405_only_u2_unfunded_bel_testnet_20976780() {
    run_bundle(include_str!("vectors/c17_7b_3_tk_fok_bid_5_usd_for_2025000d_0_405_only_u2_unfunded_bel_testnet_20976780.json"));
}

/// Campaign 17 — 9-2 rv (pre-fee 1390000 < 1400000) asks 1 USD for 5000000d, nothing crosses (C6575A8C8471, network tecINSUF_RESERVE_OFFER).
#[test]
fn c17_9_2_rv_pre_fee_1390000_1400000_asks_1_usd_for_5000000d_nothi_testnet_20976704() {
    run_bundle(include_str!("vectors/c17_9_2_rv_pre_fee_1390000_1400000_asks_1_usd_for_5000000d_nothi_testnet_20976704.json"));
}

/// Campaign 17 — 9-3 m1 bids 4 USD for 2000000d (0.5) (5581C8152193, network tesSUCCESS).
#[test]
fn c17_9_3_m1_bids_4_usd_for_2000000d_0_5_testnet_20976707() {
    run_bundle(include_str!("vectors/c17_9_3_m1_bids_4_usd_for_2000000d_0_5_testnet_20976707.json"));
}

/// Campaign 17 — 9-4 rv (pre-fee 1389990) asks 10 USD for 4000000d: crosses m1 4 USD, remainder refused (EFC6E6B7A15B, network tesSUCCESS).
#[test]
fn c17_9_4_rv_pre_fee_1389990_asks_10_usd_for_4000000d_crosses_m1_4_testnet_20976709() {
    run_bundle(include_str!("vectors/c17_9_4_rv_pre_fee_1389990_asks_10_usd_for_4000000d_crosses_m1_4_testnet_20976709.json"));
}

/// Campaign 17 — 9-7 rv (pre-fee 1399990) replaces 9-6 via OfferSequence (42D1F62558DB, network tecINSUF_RESERVE_OFFER).
#[test]
fn c17_9_7_rv_pre_fee_1399990_replaces_9_6_via_offersequence_testnet_20976715() {
    run_bundle(include_str!("vectors/c17_9_7_rv_pre_fee_1399990_replaces_9_6_via_offersequence_testnet_20976715.json"));
}

/// Campaign 17 — 9-8 m1 pays rv 30 drops (820C817BEF2C, network tesSUCCESS).
#[test]
fn c17_9_8_m1_pays_rv_30_drops_testnet_20976717() {
    run_bundle(include_str!("vectors/c17_9_8_m1_pays_rv_30_drops_testnet_20976717.json"));
}

/// Campaign 17 — 9-11 rv (liquid pre-fee 10 drops) bids 1 USD for 1000d: liquid 0 after the fee (225085B50632, network tecUNFUNDED_OFFER).
#[test]
fn c17_9_11_rv_liquid_pre_fee_10_drops_bids_1_usd_for_1000d_liquid_0_testnet_20976723() {
    run_bundle(include_str!("vectors/c17_9_11_rv_liquid_pre_fee_10_drops_bids_1_usd_for_1000d_liquid_0_testnet_20976723.json"));
}

/// Campaign 17 — 8-1 ib sets GlobalFreeze (57CD714D2CA2, network tesSUCCESS).
#[test]
fn c17_8_1_ib_sets_globalfreeze_testnet_20976726() {
    run_bundle(include_str!("vectors/c17_8_1_ib_sets_globalfreeze_testnet_20976726.json"));
}

/// Campaign 17 — 8-5 ib clears GlobalFreeze (FBBFAFA6F62E, network tesSUCCESS).
#[test]
fn c17_8_5_ib_clears_globalfreeze_testnet_20976734() {
    run_bundle(include_str!("vectors/c17_8_5_ib_clears_globalfreeze_testnet_20976734.json"));
}

/// Campaign 17 — 8b-1 ib sets GlobalFreeze (FBAF2E5D7DFA, network tesSUCCESS).
#[test]
fn c17_8b_1_ib_sets_globalfreeze_testnet_20977006() {
    run_bundle(include_str!("vectors/c17_8b_1_ib_sets_globalfreeze_testnet_20977006.json"));
}

/// Campaign 17 — 8b-4 ib clears GlobalFreeze (4FA22432F8AD, network tesSUCCESS).
#[test]
fn c17_8b_4_ib_clears_globalfreeze_testnet_20977012() {
    run_bundle(include_str!("vectors/c17_8b_4_ib_clears_globalfreeze_testnet_20977012.json"));
}

/// Campaign 17 — 8b-5 dz freezes + deep-freezes ITS OWN side of its USD line (96FF833BD038, network tesSUCCESS).
#[test]
fn c17_8b_5_dz_freezes_deep_freezes_its_own_side_of_its_usd_line_testnet_20977014() {
    run_bundle(include_str!("vectors/c17_8b_5_dz_freezes_deep_freezes_its_own_side_of_its_usd_line_testnet_20977014.json"));
}
