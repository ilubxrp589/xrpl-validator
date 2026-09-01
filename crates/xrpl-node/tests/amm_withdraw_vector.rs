//! Byte-exact vector drill for finding 54 (2026-09-01): the equal-withdraw
//! fraction rounds NEAREST, in every mode.
//!
//! Mainnet #106692584 tx B27127D6… (tfWithdrawAll, Jocker/XRP): the ceiled
//! fraction the F31/32 calibration installed hands the withdrawer
//! 27065.19038319615 Jocker where mainnet's …613 demands the nearest
//! fraction — and re-analysis showed #106629211 (the ceil's justifying
//! specimen) was byte-INDIFFERENT between the two all along, its ledger
//! lines quantizing both candidates identically. One rule survives all
//! calibrators: frac = divide(tokens, lptAMMBalance) Number-nearest,
//! asset products DOWNWARD.
use serde_json::Value;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

fn key32(hex_key: &str) -> Hash256 {
    Hash256(<[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap())
}

#[test]
fn withdraw_all_line_is_byte_exact() {
    let bundle: Value =
        serde_json::from_str(include_str!("vectors/ammwithdraw_ulp_106692584.json")).unwrap();
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
        let bytes = hex::decode(v.as_str().unwrap().trim()).unwrap();
        let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).unwrap();
        hexify_addresses(&mut jv);
        state
            .state_map
            .insert(key32(k), serde_json::to_vec(&jv).unwrap())
            .unwrap();
    }

    let tx = &bundle["tx"];
    let tx_hash = tx["hash"].as_str().unwrap();
    let txf = build_txfields(tx).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    assert_eq!(ter, "tesSUCCESS", "mainnet applied this AMMWithdraw");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        let ent = mods
            .get(&key32(k))
            .unwrap_or_else(|| panic!("target {k} must be written by the apply"));
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
            SandboxEntry::Deleted => panic!("target {k} deleted?"),
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
    assert_eq!(ter, want_ter, "mainnet's result for this AMMWithdraw");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

    for (k, want_hex) in bundle["expect"].as_object().unwrap() {
        let ent = mods
            .get(&key32(k))
            .unwrap_or_else(|| panic!("target {k} must be written by the apply"));
        let bytes = match ent {
            SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b.clone(),
            SandboxEntry::Deleted => panic!("target {k} deleted?"),
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

/// F61's regression guard (#106696548 9FEB2A66, tfWithdrawAll BCFT/XRP —
/// two 211-byte PRE-OK line diffs in the live shadow right after the F45-F58
/// deploy). The LP's 10588450.34004458 tokens against a supply of
/// 94636650.79668728 is a fraction of 0.11188530290227921591…: every
/// nearest rule gives …792 and the withdrawer lands 9762090.561664651,
/// eight ulps under mainnet's 9762090.561664659. rippled's `divide` hands
/// the STAmount constructor the truncated 17-digit quotient PLUS 5, and
/// under fixUniversalNumber that constructor canonicalises through Number
/// to_nearest — so 11188530290227921 + 5 → …7926 → …793, and the product
/// rounded downward (getRoundedAsset, IsDeposit::No) is mainnet's share.
#[test]
fn withdraw_all_fraction_carries_divides_legacy_bias_is_byte_exact() {
    run_bundle(include_str!("vectors/amm_withdraw_frac_106696548.json"));
}

/// F63's regression guard (#106696868 DFC7A0F4, tfTwoAsset CHICKEN/SCRATCH
/// by the pool's ONLY LP — four 211/324-byte PRE-OK diffs in the live
/// shadow after the F45-F58 deploy). The LP's line holds 507670.4691518975
/// against an object LPTokenBalance of 507670.469151897: rounding dust of
/// the pool's life. fixAMMv1_1 `verifyAndAdjustLPTokenBalance` snaps the
/// object to the line before anything is sized (isOnlyLiquidityProvider
/// walks the AMM's owner directory; 1e-3 relative distance). Sized from the
/// object instead, tokens land …392 for …394, SCRATCH …620 for …619, and
/// the object and line end five ulps apart where mainnet's agree at …581.
#[test]
fn only_lp_withdraw_snaps_the_object_to_the_line_is_byte_exact() {
    run_bundle(include_str!("vectors/amm_withdraw_onlylp_106696868.json"));
}

/// F68's regression guard (#106699133 6B6670FD, one of five in a row from
/// rh9vsQip…): tfOneAssetWithdrawAll of 40,500,694.89 of the pool's
/// 140,528,763.31 LP tokens as XRP alone, with `Amount` 345.906940 XRP as
/// the floor. rippled's singleWithdrawTokens pays only if ammAssetOut buys
/// at least that floor — it does not, so mainnet recorded tecAMM_FAILED
/// (fee only) where we paid the position out.
#[test]
fn one_asset_withdraw_all_under_its_amount_floor_is_tec_amm_failed() {
    run_bundle(include_str!("vectors/amm_withdraw_floor_106699133.json"));
}
