//! Campaign 25 (devnet, 2026-09-25) byte-exact vectors: Batch (BatchV1_1, active on mainnet ~09-29) x
//! Clawback, AMMClawback and token escrow, all already enabled on mainnet. Trust-line and MPT clawback inside
//! batches (a holder clawed to zero, then removing its line or MPToken in a later inner; untilfailure and onlyone
//! across a holder with no line), AMMClawback (partial; a fresh LP clawed whole; the last LP clawed so the pool is
//! deleted, then recreated by a later inner; tfClawTwoAssets on a two-IOU pool), and MPT and IOU escrows created,
//! finished and cancelled inside batches, including clawback while part of the balance is locked. AllOrNothing
//! discards throughout. Same harness as campaigns 23 and 24.
//!
//! Result: 37 of 40 byte-exact under both flow engines, no engine finding. The other three differ only because devnet
//! enables SingleAssetVault and LendingProtocol, which run rippled's `Number` at 19 digits (`setCurrentTransactionRules`,
//! Rules.cpp); mainnet enables neither and the engine models the 16-digit scale. libxrpl 3.4.0 under devnet's rules
//! reproduces devnet (parity_probe). They stay pinned, ignored, for the day either amendment nears mainnet.
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

/// Campaign 25 — 0-1 I allows trust-line clawback (84E1048946FF, AccountSet tesSUCCESS).
#[test]
fn c25_0_1_i_allows_trust_line_clawback_devnet_5595217() {
    run_batch_bundle(include_str!("vectors/c25_0_1_i_allows_trust_line_clawback_devnet_5595217.json"));
}

/// Campaign 25 — 0-2 I sets DefaultRipple (BEF6C7C01F38, AccountSet tesSUCCESS).
#[test]
fn c25_0_2_i_sets_defaultripple_devnet_5595219() {
    run_batch_bundle(include_str!("vectors/c25_0_2_i_sets_defaultripple_devnet_5595219.json"));
}

/// Campaign 25 — 0-3 I allows trust-line locking (token escrow) (78D426DF0247, AccountSet tesSUCCESS).
#[test]
fn c25_0_3_i_allows_trust_line_locking_token_escrow_devnet_5595222() {
    run_batch_bundle(include_str!("vectors/c25_0_3_i_allows_trust_line_locking_token_escrow_devnet_5595222.json"));
}

/// Campaign 25 — 0-4 A trusts I for CLW (3E2CED4E2E2B, TrustSet tesSUCCESS).
#[test]
fn c25_0_4_a_trusts_i_for_clw_devnet_5595224() {
    run_batch_bundle(include_str!("vectors/c25_0_4_a_trusts_i_for_clw_devnet_5595224.json"));
}

/// Campaign 25 — 0-4 B trusts I for CLW (63EF0F7E68CC, TrustSet tesSUCCESS).
#[test]
fn c25_0_4_b_trusts_i_for_clw_devnet_5595226() {
    run_batch_bundle(include_str!("vectors/c25_0_4_b_trusts_i_for_clw_devnet_5595226.json"));
}

/// Campaign 25 — 0-4 C trusts I for CLW (E92D4CC95FCA, TrustSet tesSUCCESS).
#[test]
fn c25_0_4_c_trusts_i_for_clw_devnet_5595228() {
    run_batch_bundle(include_str!("vectors/c25_0_4_c_trusts_i_for_clw_devnet_5595228.json"));
}

/// Campaign 25 — 0-5 I pays A 1000 CLW (4707C53F176E, Payment tesSUCCESS).
#[test]
fn c25_0_5_i_pays_a_1000_clw_devnet_5595230() {
    run_batch_bundle(include_str!("vectors/c25_0_5_i_pays_a_1000_clw_devnet_5595230.json"));
}

/// Campaign 25 — 0-6 I pays B 500 CLW (5CB400E78E42, Payment tesSUCCESS).
#[test]
fn c25_0_6_i_pays_b_500_clw_devnet_5595232() {
    run_batch_bundle(include_str!("vectors/c25_0_6_i_pays_b_500_clw_devnet_5595232.json"));
}

/// Campaign 25 — 0-8 M creates the MPT issuance (escrow, trade, transfer, clawback) (FEBF12E0815D, MPTokenIssuanceCreate tesSUCCESS).
#[test]
fn c25_0_8_m_creates_the_mpt_issuance_escrow_trade_transfer_clawbac_devnet_5595234() {
    run_batch_bundle(include_str!("vectors/c25_0_8_m_creates_the_mpt_issuance_escrow_trade_transfer_clawbac_devnet_5595234.json"));
}

/// Campaign 25 — 0-9 A authorizes the MPT (D44E28CCD4B2, MPTokenAuthorize tesSUCCESS).
#[test]
fn c25_0_9_a_authorizes_the_mpt_devnet_5595236() {
    run_batch_bundle(include_str!("vectors/c25_0_9_a_authorizes_the_mpt_devnet_5595236.json"));
}

/// Campaign 25 — 0-10 B authorizes the MPT (1FA6C9DA3D10, MPTokenAuthorize tesSUCCESS).
#[test]
fn c25_0_10_b_authorizes_the_mpt_devnet_5595238() {
    run_batch_bundle(include_str!("vectors/c25_0_10_b_authorizes_the_mpt_devnet_5595238.json"));
}

/// Campaign 25 — 0-11 M pays A 10000 MPT (0305BCEA576E, Payment tesSUCCESS).
#[test]
fn c25_0_11_m_pays_a_10000_mpt_devnet_5595240() {
    run_batch_bundle(include_str!("vectors/c25_0_11_m_pays_a_10000_mpt_devnet_5595240.json"));
}

/// Campaign 25 — 0-12 M pays B 5000 MPT (93109817690F, Payment tesSUCCESS).
#[test]
fn c25_0_12_m_pays_b_5000_mpt_devnet_5595242() {
    run_batch_bundle(include_str!("vectors/c25_0_12_m_pays_b_5000_mpt_devnet_5595242.json"));
}

/// Campaign 25 — 0-13 A creates the CLW/XRP pool (DE042814DC09, AMMCreate tesSUCCESS).
#[test]
#[ignore = "devnet runs Number at 19 digits (SingleAssetVault, LendingProtocol): the initial LP mint root2(200 x 20000000) is 63245.55320336758 there, 63245.55320336755 at mainnet's 16"]
fn c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244() {
    run_batch_bundle(include_str!("vectors/c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244.json"));
}

/// Campaign 25 — 0-14 B deposits both assets (3034DA9F21C6, AMMDeposit tesSUCCESS).
#[test]
fn c25_0_14_b_deposits_both_assets_devnet_5595246() {
    run_batch_bundle(include_str!("vectors/c25_0_14_b_deposits_both_assets_devnet_5595246.json"));
}

/// Campaign 25 — 0-7 I pays C 100 CLW (retry after tefPAST_SEQ) (EEE2EA6F2DF2, Payment tesSUCCESS).
#[test]
fn c25_0_7_i_pays_c_100_clw_retry_after_tefpast_seq_devnet_5595253() {
    run_batch_bundle(include_str!("vectors/c25_0_7_i_pays_c_100_clw_retry_after_tefpast_seq_devnet_5595253.json"));
}

/// Campaign 25 — 1-1 independent: I pays A 50 CLW, then claws 30 back (6F62373CEA0C, Batch tesSUCCESS).
#[test]
fn c25_1_1_independent_i_pays_a_50_clw_then_claws_30_back_devnet_5595258() {
    run_batch_bundle(include_str!("vectors/c25_1_1_independent_i_pays_a_50_clw_then_claws_30_back_devnet_5595258.json"));
}

/// Campaign 25 — 1-2 allornothing: I claws 10 from B, unfunded XRP pay -> discarded (BD7C693FB692, Batch tesSUCCESS).
#[test]
fn c25_1_2_allornothing_i_claws_10_from_b_unfunded_xrp_pay_discarde_devnet_5595260() {
    run_batch_bundle(include_str!("vectors/c25_1_2_allornothing_i_claws_10_from_b_unfunded_xrp_pay_discarde_devnet_5595260.json"));
}

/// Campaign 25 — 1-3 independent outer C: C pays B 20 CLW, I claws all C has left, C closes the line (E918E7E8FAB2, Batch tesSUCCESS).
#[test]
fn c25_1_3_independent_outer_c_c_pays_b_20_clw_i_claws_all_c_has_le_devnet_5595262() {
    run_batch_bundle(include_str!("vectors/c25_1_3_independent_outer_c_c_pays_b_20_clw_i_claws_all_c_has_le_devnet_5595262.json"));
}

/// Campaign 25 — 1-4 untilfailure: I claws 5 from A, then from M (no CLW line), then 5 from B (7736ED4CC113, Batch tesSUCCESS).
#[test]
fn c25_1_4_untilfailure_i_claws_5_from_a_then_from_m_no_clw_line_th_devnet_5595265() {
    run_batch_bundle(include_str!("vectors/c25_1_4_untilfailure_i_claws_5_from_a_then_from_m_no_clw_line_th_devnet_5595265.json"));
}

/// Campaign 25 — 1-5 onlyone: I claws from M (no line), then 1 from A (stops), then 1 from B (72289D7240EB, Batch tesSUCCESS).
#[test]
fn c25_1_5_onlyone_i_claws_from_m_no_line_then_1_from_a_stops_then_devnet_5595267() {
    run_batch_bundle(include_str!("vectors/c25_1_5_onlyone_i_claws_from_m_no_line_then_1_from_a_stops_then_devnet_5595267.json"));
}

/// Campaign 25 — 2-1 independent: M pays A 100 MPT, then claws 60 back (8D1A3A4B7F0F, Batch tesSUCCESS).
#[test]
fn c25_2_1_independent_m_pays_a_100_mpt_then_claws_60_back_devnet_5595273() {
    run_batch_bundle(include_str!("vectors/c25_2_1_independent_m_pays_a_100_mpt_then_claws_60_back_devnet_5595273.json"));
}

/// Campaign 25 — 2-2 independent outer C: C opts in, M pays C 50, M claws all 50, C opts out (MPToken made and removed) (B5B4A019857F, Batch tesSUCCESS).
#[test]
fn c25_2_2_independent_outer_c_c_opts_in_m_pays_c_50_m_claws_all_50_devnet_5595276() {
    run_batch_bundle(include_str!("vectors/c25_2_2_independent_outer_c_c_opts_in_m_pays_c_50_m_claws_all_50_devnet_5595276.json"));
}

/// Campaign 25 — 2-3 allornothing: M claws 10 from A, then from C (no MPToken) -> discarded (C85653E83E26, Batch tesSUCCESS).
#[test]
fn c25_2_3_allornothing_m_claws_10_from_a_then_from_c_no_mptoken_di_devnet_5595279() {
    run_batch_bundle(include_str!("vectors/c25_2_3_allornothing_m_claws_10_from_a_then_from_c_no_mptoken_di_devnet_5595279.json"));
}

/// Campaign 25 — 3-1 independent: I claws 10 CLW of A's pool share, then pays A 10 CLW (6B4E1852B504, Batch tesSUCCESS).
#[test]
fn c25_3_1_independent_i_claws_10_clw_of_a_s_pool_share_then_pays_a_devnet_5595285() {
    run_batch_bundle(include_str!("vectors/c25_3_1_independent_i_claws_10_clw_of_a_s_pool_share_then_pays_a_devnet_5595285.json"));
}

/// Campaign 25 — 3-2 independent outer B: B deposits 1 XRP single-asset, I claws back all of B's LP (B90D26CC84CC, Batch tesSUCCESS).
#[test]
#[ignore = "devnet runs Number at 19 digits: B's single-asset deposit mints different LP tokens (as in 6-1), so clawing all of B's LP leaves the pool's CLW at 186.163102167096 there, 186.1631058885331 here"]
fn c25_3_2_independent_outer_b_b_deposits_1_xrp_single_asset_i_claw_devnet_5595287() {
    run_batch_bundle(include_str!("vectors/c25_3_2_independent_outer_b_b_deposits_1_xrp_single_asset_i_claw_devnet_5595287.json"));
}

/// Campaign 25 — 3-3 allornothing: I claws 5 CLW of A's share, unfunded pay -> discarded (58F20AF507E5, Batch tesSUCCESS).
#[test]
fn c25_3_3_allornothing_i_claws_5_clw_of_a_s_share_unfunded_pay_dis_devnet_5595290() {
    run_batch_bundle(include_str!("vectors/c25_3_3_allornothing_i_claws_5_clw_of_a_s_share_unfunded_pay_dis_devnet_5595290.json"));
}

/// Campaign 25 — 3-4 independent outer A: I claws all of A's LP (the last LP: pool deleted), A creates the pool again (F3F241EE479B, Batch tesSUCCESS).
#[test]
fn c25_3_4_independent_outer_a_i_claws_all_of_a_s_lp_the_last_lp_po_devnet_5595293() {
    run_batch_bundle(include_str!("vectors/c25_3_4_independent_outer_a_i_claws_all_of_a_s_lp_the_last_lp_po_devnet_5595293.json"));
}

/// Campaign 25 — 4-1 independent: A escrows 100 MPT to B (finishable), 30 MPT to B (cancelable soon), pays B 50 MPT (A0DEE2B50344, Batch tesSUCCESS).
#[test]
fn c25_4_1_independent_a_escrows_100_mpt_to_b_finishable_30_mpt_to_devnet_5595299() {
    run_batch_bundle(include_str!("vectors/c25_4_1_independent_a_escrows_100_mpt_to_b_finishable_30_mpt_to_devnet_5595299.json"));
}

/// Campaign 25 — 4-2 independent outer A (signer I): A escrows 40 CLW to B, I claws 100 CLW from A's line (C3BD9D7FFAEA, Batch tesSUCCESS).
#[test]
fn c25_4_2_independent_outer_a_signer_i_a_escrows_40_clw_to_b_i_cla_devnet_5595301() {
    run_batch_bundle(include_str!("vectors/c25_4_2_independent_outer_a_signer_i_a_escrows_40_clw_to_b_i_cla_devnet_5595301.json"));
}

/// Campaign 25 — 4-3 independent outer A (signer M): A escrows 200 MPT to B, M claws 100 MPT from A (12E4610484A4, Batch tesSUCCESS).
#[test]
fn c25_4_3_independent_outer_a_signer_m_a_escrows_200_mpt_to_b_m_cl_devnet_5595304() {
    run_batch_bundle(include_str!("vectors/c25_4_3_independent_outer_a_signer_m_a_escrows_200_mpt_to_b_m_cl_devnet_5595304.json"));
}

/// Campaign 25 — 4-4 allornothing: A escrows 20 MPT to B, unfunded pay -> discarded (1CFC00E2EDFD, Batch tesSUCCESS).
#[test]
fn c25_4_4_allornothing_a_escrows_20_mpt_to_b_unfunded_pay_discarde_devnet_5595306() {
    run_batch_bundle(include_str!("vectors/c25_4_4_allornothing_a_escrows_20_mpt_to_b_unfunded_pay_discarde_devnet_5595306.json"));
}

/// Campaign 25 — 4-5 independent outer B: B finishes the 100 MPT and the 40 CLW escrows, pays A 25 MPT (DEE3E4CCD551, Batch tesSUCCESS).
#[test]
fn c25_4_5_independent_outer_b_b_finishes_the_100_mpt_and_the_40_cl_devnet_5595313() {
    run_batch_bundle(include_str!("vectors/c25_4_5_independent_outer_b_b_finishes_the_100_mpt_and_the_40_cl_devnet_5595313.json"));
}

/// Campaign 25 — 4-6 independent: A cancels the 30 MPT escrow, then pays B 30 MPT (EE1902EEE9DD, Batch tesSUCCESS).
#[test]
fn c25_4_6_independent_a_cancels_the_30_mpt_escrow_then_pays_b_30_m_devnet_5595315() {
    run_batch_bundle(include_str!("vectors/c25_4_6_independent_a_cancels_the_30_mpt_escrow_then_pays_b_30_m_devnet_5595315.json"));
}

/// Campaign 25 — 5-0a A trusts I for CLX (FA1B939320DD, TrustSet tesSUCCESS).
#[test]
fn c25_5_0a_a_trusts_i_for_clx_devnet_5595317() {
    run_batch_bundle(include_str!("vectors/c25_5_0a_a_trusts_i_for_clx_devnet_5595317.json"));
}

/// Campaign 25 — 5-0b I pays A 500 CLX (EB23F9F69EA8, Payment tesSUCCESS).
#[test]
fn c25_5_0b_i_pays_a_500_clx_devnet_5595319() {
    run_batch_bundle(include_str!("vectors/c25_5_0b_i_pays_a_500_clx_devnet_5595319.json"));
}

/// Campaign 25 — 5-0c A creates the CLW/CLX pool (1D7DF90D33F5, AMMCreate tesSUCCESS).
#[test]
fn c25_5_0c_a_creates_the_clw_clx_pool_devnet_5595321() {
    run_batch_bundle(include_str!("vectors/c25_5_0c_a_creates_the_clw_clx_pool_devnet_5595321.json"));
}

/// Campaign 25 — 5-1 independent: I claws both assets of A's CLW/CLX share (last LP: pool deleted), pays A 10 CLW (2EC905B9BAA9, Batch tesSUCCESS).
#[test]
fn c25_5_1_independent_i_claws_both_assets_of_a_s_clw_clx_share_las_devnet_5595323() {
    run_batch_bundle(include_str!("vectors/c25_5_1_independent_i_claws_both_assets_of_a_s_clw_clx_share_las_devnet_5595323.json"));
}

/// Campaign 25 — 6-1 B deposits 1 XRP single-asset (plain) (EA54024A6C14, AMMDeposit tesSUCCESS).
#[test]
#[ignore = "devnet runs Number at 19 digits: at 16, fixAMMv1_3's adjustAssetInByTokens prices the tokens for 1 XRP over 1,000,000 drops and re-mints for 999,999 (1541.92481545477); devnet mints 1541.92632150251"]
fn c25_6_1_b_deposits_1_xrp_single_asset_plain_devnet_5596640() {
    run_batch_bundle(include_str!("vectors/c25_6_1_b_deposits_1_xrp_single_asset_plain_devnet_5596640.json"));
}

/// Campaign 25 — 6-2 I claws back all of B's LP (plain) (669A2826DB6F, AMMClawback tesSUCCESS).
#[test]
fn c25_6_2_i_claws_back_all_of_b_s_lp_plain_devnet_5596643() {
    run_batch_bundle(include_str!("vectors/c25_6_2_i_claws_back_all_of_b_s_lp_plain_devnet_5596643.json"));
}
