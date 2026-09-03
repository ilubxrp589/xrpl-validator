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
    // A bundle pinned on a tec carries mainnet's result; the fee-only
    // AccountRoot in `expect` then checks the refusal's shape too.
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet's result for this Payment");

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

/// #106711980 81340311C30C (finding 96): a direct 190.36 EUR payment between
/// two GateHub holders (TransferRate 1.002) under tfPartialPayment. The fee
/// step's forward pass is rippled's IOUAmount `mulRatio(in, QUALITY_ONE,
/// rate, roundUp=false)` (DirectStep.cpp:646), whose "round down" is
/// Number's half-even nearest of the 18-digit quotient: 190.36 / 1.002 =
/// 189.98003992015968… delivers 189.9800399201597. The truncating
/// `me_muldiv` credited …596 on the destination's line. Byte-exact on both
/// trust lines and the sender's AccountRoot.
#[test]
fn payment_direct_transfer_fee_delivery_rounds_like_mul_ratio() {
    run_bundle(include_str!("vectors/payment_direct_fee_mulratio_106711980.json"));
}

/// #106712861 1A6A9DA7AD0A (finding 98): a self-payment XAH → PAX → XRP whose
/// sender holds no PAX line. rippled's strand hands the first book's PAX
/// straight to the second book's makers (through the issuer's fee); the
/// sender's own PAX line is never touched. Our hop chain parked the
/// intermediate on the sender — creating the line (both owner directories,
/// OwnerCount + 1) — and took back one ulp more a hop later, leaving a
/// phantom directory entry and OwnerCount 730 for mainnet's 729. The target
/// is the sender's AccountRoot rebuilt from the meta's FinalFields; the fix
/// registers the hop's pass-through legs so no taker-side move lands on
/// the sender.
#[test]
fn payment_intermediate_hop_never_lands_on_the_senders_line() {
    run_bundle(include_str!("vectors/payment_passthrough_intermediate_106712861.json"));
}

/// Finding 105 (#106722089 819A6BBC): a tfPartialPayment self-conversion,
/// SendMax 200000 XA3 for 15.976242 XRP through the XA3/XRP pool the sender
/// dominates. The sender's XA3 line carries NoRipple on the ISSUER's (low)
/// side, so rippled's BookStep::check refuses the book that follows the
/// sender -> issuer DirectStepI (terNO_RIPPLE), the default path fails to
/// build and Payment reports tecPATH_DRY with the fee alone. We swapped
/// 199995.93 XA3 through the pool and delivered the whole Amount.
#[test]
fn payment_book_behind_an_issuer_no_ripple_line_is_path_dry() {
    run_bundle(include_str!("vectors/payment_noripple_into_book_106722089.json"));
}

/// Finding 107 (#106723025 62498920E4FE): a tfPartialPayment self-conversion,
/// SendMax 910,314,466.5 TIME → [FUZZY, XRP] → 263.0198 RLUSD (DeliverMin
/// 261.517), whose TIME→FUZZY hop is a pool with no book behind it. rippled's
/// AMMContext allows a flow 30 iterations that consume pool liquidity —
/// in either offer mode — and then `getOffer` answers nullopt for the rest of
/// the flow. Its trace runs exactly 30 "Best path" iterations, each identical
/// to ours to the last digit, then "All strands dry": 101.4 RLUSD delivered,
/// tecPATH_PARTIAL, fee only. We kept iterating (47 rounds on this bundle),
/// reached DeliverMin and wrote 127 nodes. The bundle carries the 70 XRP/RLUSD
/// book levels, their makers and the three pools (EXTRA_KEYS), seated through
/// the in-ledger replay, so the walk here is the live walk.
#[test]
fn payment_pool_liquidity_stops_after_thirty_amm_iterations() {
    run_bundle(include_str!("vectors/payment_amm_iteration_cap_106723025.json"));
}

/// Finding 109 (#106723438 23BA5CD66ACE): a tfPartialPayment self-conversion,
/// SendMax 5 RLUSD → [XRP] → 1248.0865 xSPECTAR (DeliverMin 1223.3719), with
/// a direct RLUSD/xSPECTAR pool and an XRP bridge. rippled takes eight direct
/// fib rounds and then the bridge (4.93944527 RLUSD → 1211.67226294),
/// delivering 1226.858 — tesSUCCESS through ten nodes. Our strand bound sized
/// the direct pool's fib slice from the pool's CURRENT balances instead of the
/// flow's origin (AmmFib.init, rippled's initialBalances_), drifted 1e-4 from
/// the slice that would actually fill, won a ninth direct round on a near-tie
/// and came up 2.8 xSPECTAR under DeliverMin: tecPATH_PARTIAL.
#[test]
fn payment_strand_bound_prices_the_fib_slice_from_the_flows_origin() {
    run_bundle(include_str!("vectors/payment_strand_tie_bridge_wins_106723438.json"));
}

/// Finding 113 (#106730304 B666C9B462C6): a tfPartialPayment self-conversion,
/// SendMax 25,579 LIQUIDX for 2 drops, walking LIQUIDX → BOOT → BITx → FLR →
/// XRP. The FLR→XRP hop crosses a maker holding no FLR line, so the crossing
/// creates one (8EFF74ED) and appends it to the FLR issuer's last owner
/// directory page 0x6ab. Our walk had also carried the sender's in-flight
/// FLR through a temporary line, and the cleanup that erases that line's
/// traces restored the issuer's directory page from its pre-hop image —
/// dropping the maker's legitimate entry with it. rippled never materialises
/// the sender's in-flight line, so its page keeps the entry. Twelve targets
/// byte-pinned, page CD88D854 among them.
#[test]
fn payment_inflight_line_cleanup_keeps_other_directory_entries() {
    run_bundle(include_str!("vectors/payment_inflight_line_cleanup_keeps_dir_entries_106730304.json"));
}


/// Finding 119 (#106732759 34694521561A): a self-payment of 16.45 UNI for
/// SendMax 100.32 USDT, tfPartialPayment|tfLimitQuality, six paths, two of
/// them through pools (USDT/XRPS, XRPS/UNI). Our rounds 0–4 match rippled's
/// iterations exactly; at round 5 only the two-pool strand is active and
/// rippled's `limitOut` sizes it from the strand's composed quality function
/// to 9.790866331963915 UNI. That function carries 1/1.001 twice — the USDT
/// issuer's transfer rate at the first book hop (the sender REDEEMS into the
/// pool: BookPaymentStep::adjustQualityWithFees composes trIn) and the UNI
/// issuer's at the closing DirectStep — while our fold skipped hop 0, priced
/// the strand at 11.64, asked the full 10.62 remainder and was rejected:
/// 3.81 UNI through the pools against mainnet's 13.61. The bundle carries the
/// four issuer roots (EXTRA_KEYS) so the rates are visible; twelve targets
/// byte-pinned.
#[test]
fn payment_first_book_hop_composes_its_in_transfer_rate_into_the_quality_function() {
    run_bundle(include_str!("vectors/payment_first_hop_trin_in_the_quality_function_106732759.json"));
}

/// Finding 126 (#106735554 9BCDD090): rapido's partial payment RLUSD → USD
/// → XRP. The reverse pass is limited by rpxqUyf's 0.837973427575079 USD
/// line; the forward pass feeds rvYAfWj's USD/XRP book 0.8379734275750757
/// gross, and rippled's `limitStepIn` nets it at mulRatio-nearest over the
/// 1.0015 transfer rate: quotient …006|98, so the maker rPrDM69j receives
/// 0.8367183500500007 and its offer 3CDA2E79 keeps 0.7018076499499993.
/// The mixed engine's book segment truncated to …006 — the fifth floor of
/// the net-division family — and the residual read …994. Ten targets
/// byte-pinned.
#[test]
fn payment_mixed_book_segment_nets_its_carry_at_mulratio_nearest() {
    run_bundle(include_str!("vectors/payment_mixed_book_segment_nets_at_mulratio_nearest_106735554.json"));
}

/// Finding 126, second specimen (#106733664 37F54060): rapido again, a
/// partial payment of 1.605472 XRP for USDT through the mixed engine, the
/// maker's residual TakerPays one ULP high in ours (95.81228262477523 for
/// mainnet's …522) from the same truncating book-segment netting. Nine
/// targets byte-pinned.
#[test]
fn payment_mixed_book_segment_nets_its_carry_at_mulratio_nearest_second_specimen() {
    run_bundle(include_str!("vectors/payment_mixed_book_segment_nets_at_mulratio_nearest_second_specimen_106733664.json"));
}

/// Finding 129 (#106736593 3D845265): r9Vf7UMf's partial payment of
/// 0.27976441545926 MXR into the MXR/PLX pool, two strands built so the pool
/// offers the fib slice 0.499490147013714 → 3931.92254433. rippled's
/// `getRate` is `muldiv(num, 1e17, den) + 5` canonicalized by Number at
/// to_nearest: 127034584578478051|29 + 5 → …781. Our division truncated
/// after the +5 and read …780, so the SendMax-limited fill priced at that
/// quality delivered 2202.269692049335 for mainnet's …333. Five targets
/// byte-pinned.
#[test]
fn payment_encoded_rate_rounds_to_nearest_after_the_legacy_half_up() {
    run_bundle(include_str!("vectors/payment_encoded_rate_rounds_nearest_after_the_legacy_half_up_106736593.json"));
}

/// Finding 129, second specimen (#106732893 F6F7F340): a DeliverMin partial
/// payment walking the XRP/TPR pool through ~30 fib iterations — parked
/// since morning as a "deep fold-order drill"; one line one ULP off, and the
/// encoded rate of one slice was the whole of it. Byte-pinned.
#[test]
fn payment_encoded_rate_rounds_to_nearest_thirty_fib_iterations() {
    run_bundle(include_str!("vectors/payment_encoded_rate_rounds_nearest_thirty_fib_iterations_106732893.json"));
}

/// Finding 129, third specimen (#106736591 2AC6273B): rGdBUkZe's partial
/// payment, a five-round fib chain, the destination line …108 for
/// mainnet's …107. Byte-pinned.
#[test]
fn payment_encoded_rate_rounds_to_nearest_fib_chain() {
    run_bundle(include_str!("vectors/payment_encoded_rate_rounds_nearest_fib_chain_106736591.json"));
}

/// Finding 130 (#106734110 790B4EA7): r9Vf7UMf again, 0.14133821372812 MXR
/// into the MXR/PLX pool with two strands built. rippled's fib seed is
/// `toAmount(kInitialFibSeqPct * initialBalances.in, Upward)` — the multiply
/// under Number's nearest, Upward only on the conversion — and 0.00025 ×
/// 1994.997505611309 is an exact tie that lands …272; our product rounded
/// up to …273, the slice's encoded rate moved from …277 to …278, and the
/// SendMax-limited fill delivered 1115.898338689759 for mainnet's
/// 1115.89833868976. Byte-pinned.
#[test]
fn payment_fib_seed_multiplies_at_nearest_and_converts_upward() {
    run_bundle(include_str!("vectors/payment_fib_seed_multiplies_at_nearest_106734110.json"));
}
