//! Byte-exact vector drills for AMMDeposit (2026-09-01).
//!
//! Sizing reads rippled's `getTradingFee(view, amm, account)`: the pool's
//! TradingFee, or the AuctionSlot's DiscountedFee for the slot holder (or an
//! AuthAccount) while the slot is unexpired — AMMDeposit.cpp:393.
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
    // The ledger's verdict — a tec specimen (fee-only) is pinned against
    // THIS, not against tesSUCCESS (fetch_tx_bundle records it as `result`).
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "the ledger's verdict for this transaction");

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

/// F66's regression guard (#106698295 337D655A, a fresh Gta6/XRP pool on the
/// lag-fix binary): the auction-slot holder's single-asset deposit of
/// 970,200,000 Gta6 into a 9,800,000 pool whose TradingFee is 810 (0.81%)
/// with a DiscountedFee of 81. Equation 3 at 810 mints 198,490,216.18 (ours);
/// at the discounted 81 it mints 199,218,448.9833358 — mainnet's LPTokenBalance
/// delta to all sixteen digits.
#[test]
fn slot_holder_single_asset_deposit_uses_the_discounted_fee_is_byte_exact() {
    run_bundle(include_str!("vectors/amm_deposit_slotfee_106698295.json"));
}

/// Finding 82 — #106702459 F648B73A: a tfLimitLPToken deposit (11.172545 XRP
/// into Gta6/XRP, EPrice 4 drops per LP token). The full amount prices at
/// 3.365 drops per token, inside the ceiling, so rippled mints Equation 3's
/// 3320034.6897731 tokens; the mode used to fall through to a placeholder.
#[test]
fn amm_deposit_limit_lp_token_within_eprice_takes_the_full_amount() {
    run_bundle(include_str!("vectors/amm_deposit_eprice_106702459.json"));
}

/// Finding 87 — #106704718 DC1B6BD3: a tfOneAssetLPToken deposit (Amount
/// as the maximum, LPTokenOut as the target). rippled adjusts the tokens,
/// derives the asset with ammAssetIn and deposits exactly that; the mode
/// used to fall through to the placeholder mover.
#[test]
fn amm_deposit_one_asset_lp_token_derives_the_asset_from_the_tokens() {
    run_bundle(include_str!("vectors/amm_deposit_one_asset_lp_token_106704718.json"));
}

/// #106721484 3F14213E0C76 (finding 104): a single-asset 5 XRP deposit into
/// the XRP/BFOOT pool by an account with NO BFOOT line while the BFOOT
/// issuer has lsfRequireAuth. AMMDeposit::preclaim runs requireAuth
/// (WeakAuth) on BOTH pool assets before any funding arithmetic
/// (AMMDeposit.cpp:263, RippleStateHelpers.cpp requireAuth): no line to a
/// RequireAuth issuer is tecNO_LINE and mainnet claims the fee. We minted
/// LP tokens. Fee-only AccountRoot, the bundle's tec result.
#[test]
fn amm_deposit_no_line_to_a_require_auth_issuer_is_tec_no_line() {
    run_bundle(include_str!("vectors/amm_deposit_require_auth_no_line_106721484.json"));
}

/// Finding 108 (#106724081 8DC704493796): a tfSingleAsset deposit of ONE drop
/// into the XRP/SPY pool. The drop is worth 0.00117353 LP tokens; rippled's
/// adjustAssetInByTokens recomputes the asset-in for those tokens (rounded
/// up: 2 drops), the adjusted amount is 0, the tokens it buys are 0, and
/// AMMDeposit answers tecAMM_INVALID_TOKENS with the fee alone. We fell back
/// to the unadjusted tokens and minted them.
#[test]
fn amm_deposit_one_drop_whose_tokens_round_away_is_invalid_tokens() {
    run_bundle(include_str!("vectors/amm_deposit_single_asset_dust_106724081.json"));
}


// Finding 155 — #106748884 089332F8CBDA (rn5eEXkk, tfTwoAsset: 27 XRP +
// 62,780,683.698714 SEAL with LPTokenOut 12,238,246.4983469): on the two-asset
// path the field is only a minimum; rippled mints the rounded proportional
// 12,299,745.22446 tokens. We minted the minimum, leaving the pool's
// LPTokenBalance and the depositor's LP line 61,498 tokens short.
#[test]
fn amm_two_asset_deposit_lptokenout_is_a_minimum() {
    run_bundle(include_str!("vectors/amm_two_asset_deposit_lptokenout_is_a_minimum_106748884.json"));
}
