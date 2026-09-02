//! Byte-exact vector drills for the payment flow DRIVER (2026-09-01).
//!
//! The driver is rippled's `flow()` loop (StrandFlow.h:606-790): activate the
//! best strand, flow it, repeat while both remainders are positive. Its only
//! bounds are safety bounds — maxTries = 1000 iterations (the 1000th entry is
//! telFAILED_PROCESSING) and 1500 offers stepped. Whatever the loop is capped
//! at here is how many fills-or-AMM-slices a lone strand may take, and a cap
//! under mainnet's count turns a tesSUCCESS into a DeliverMin shortfall.
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
    assert_eq!(ter, "tesSUCCESS", "mainnet applied this Payment");

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

/// F59's regression guard (#106693003 E9919AA2, the path-TER flagship): a
/// tfPartialPayment self-payment buying BCHAMP with 1485.4 XRP of SendMax,
/// DeliverMin 737956.89. Mainnet fills it through 33 book offers interleaved
/// with 13 AMM slices — 46 driver iterations — spending the whole SendMax
/// and delivering 763166.81. The driver here was capped at 32 rounds (the
/// old multi-strand interleave cap, kept for lone strands in 4566c4e): round
/// 32 left 497 XRP unspent and 510958.66 delivered, under DeliverMin —
/// tecPATH_PARTIAL against mainnet's tesSUCCESS, the live shadow's
/// ter-mismatch signature. rippled's loop runs on its remainders alone
/// (maxTries = 1000 is a failure bound, not a fill count).
#[test]
fn payment_flow_driver_runs_on_remainders_not_a_round_cap() {
    run_bundle(include_str!("vectors/payment_rounds_106693003.json"));
}

/// Finding 81 (payments) — #106703062 AC58204A: a BTC→FLR partial payment
/// whose first round leaves 2e-14 FLR of dust. IOUAmount remainders zero
/// below 1e-96 and the flow goes dry; ours spun to the 1000-round cap and
/// returned telFAILED_PROCESSING.
#[test]
fn payment_dust_remainders_are_iou_amounts() {
    run_bundle(include_str!("vectors/payment_dust_remainder_106703062.json"));
}

/// #106704888 16B59A8C (finding 88): a path payment that drains more than 99%
/// of the LOVE/BONSAI pool binds AMMLiquidity::maxOffer's 99% cap. rippled
/// computes `out * Number{99,-2}` in Number's default NEAREST mode and only
/// the toAmount conversion rounds down: 17.93069198232003 * 0.99 =
/// 17.7513850624968297 → 17.75138506249683. Ours rounded the product down too
/// (…82), and swapAssetOut of the smaller out moved the LOVE side by 3.1e-13
/// — the rapido bot's hot pair 92DFA753/E97DC12B, receipted every ledger.
#[test]
fn payment_pool_drain_prices_the_max_offer_cap_nearest() {
    run_bundle(include_str!("vectors/payment_pool_drain_max_offer_106704888.json"));
}

/// #106705935 71D0477C (finding 89): a partial SGB→XRP payment whose sender
/// line carries a 0.3% transfer rate. The driver measured round 0's spend as
/// the line's balance DELTA (the walk's precise figure was NET, so the fee
/// made the two disagree and the guard rejected it), and at the line's
/// precision (256853.7283596457, ulp 1e-10) the gross 6379.277459070794 read
/// as 6379.2774590708. rippled subtracts the strand's own reported in — the
/// sum of per-fill mulRatio grosses — so its round-1 SendMax remainder is
/// 5334.264789309106, ours 5334.2647893091, and the maker's net credit
/// (5318.309859729916 vs …910) left its TakerPays one ulp high.
#[test]
fn payment_fee_leg_spend_is_the_walks_gross_not_the_line_delta() {
    run_bundle(include_str!("vectors/payment_fee_leg_gross_spend_106705935.json"));
}

/// #106674444 E514B993 (finding 81b): a PLX→USDC→RLUSD→589 payment with
/// DeliverMin that rippled settles in ONE flow iteration (no step limits, so
/// the reverse-pass amounts are final). Finding 81 had made every per-fill
/// remainder a 16-digit IOUAmount; the CLOB hop's residual then normalised
/// to exponent −32, the next hop's carry inherited that scale, and the pool
/// swap's forward recompute delivered 564.5944089112000 for the requested
/// …246 — a phantom 2.46e-11 second round that moved five lines and the
/// maker offer. The round loop keeps finding 81's normalisation; the fills
/// keep their exact remainders. Red at HEAD~, green with WIN98's 98 ledgers.
#[test]
fn payment_forward_fill_remainders_stay_exact() {
    run_bundle(include_str!("vectors/payment_fwd_remainders_exact_106674444.json"));
}
