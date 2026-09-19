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
        // An expectation the apply did not write pins an object the
        // transaction must leave ALONE: it passes when it equals the seated
        // pre-image (finding 143's rule; finding 157's dry payment pins both
        // trust lines this way).
        // An EMPTY expectation is a deletion pin (finding 158): mainnet's
        // meta deleted the object in this transaction, so must the apply.
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

/// Finding 131 (#106734485 83E5899A): a partial payment of 5090 drops for
/// 3979.37885397957 Sketch through two single-path pools, XRP/X then
/// X/Sketch. The reverse pass sizes the X/Sketch pool at 60.69318969933 X
/// and the XRP/X pool beyond the SendMax, so rippled resets and runs
/// forwards: 5090 drops swap to 60.6931897 X, and the X/Sketch pool is
/// driven by THAT input — 60.6931897 for 3979.3788540239 — while the
/// terminal DirectStep delivers the reverse request (within 1e-9) and the
/// issuer keeps the sliver. We consumed the second pool at its reverse
/// amounts and left 6.7e-10 X on the sender's line. Byte-pinned.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_at_the_carried_input() {
    run_bundle(include_str!("vectors/payment_forward_pass_reswaps_the_downstream_pool_106734485.json"));
}

/// Finding 134 (#106738595 94693121): r4kSEsvD's partial payment of
/// 241.520105 RLUSD for XRP down the default book. Maker rB2f945fb3 holds
/// 3.2 XRP against a reserve of 2.6 (OwnerCount 8): its first offer fills
/// for the 0.6 XRP of funds and is stepped past and deleted. rippled's
/// PaymentSandbox remembers the owner count at that first adjustment and
/// `ownerCountHook` keeps the reserve at 2.6, so the owner's second offer is
/// unfunded and skipped; we let the deletion free 0.2 XRP and crossed it,
/// then took one XRP less from rAB4F's deeper offer. Twenty-two targets
/// byte-pinned.
#[test]
fn payment_makers_reserve_holds_at_the_flows_original_owner_count() {
    run_bundle(include_str!("vectors/payment_makers_reserve_holds_the_original_owner_count_106738595.json"));
}

/// Finding 140 — #106739814 741DD630E126 (rEEGpeYc, 2.9101496 RLUSD → XRP to
/// self, tfPartialPayment, paths `[rMxCK, XRP]` and `[XRP]`). The first path
/// only names the SendMax issuer at its head — the hop every strand starts
/// with — so rippled's `toStrands` keeps ONE strand (`hasStrand`) and the
/// flow runs single-path: the RLUSD/XRP pool is sized by the anchored offer
/// and delivers 1981581 drops. We flowed it twice (two strands, `multiPath`,
/// a Fibonacci slice) and delivered 1981089.
#[test]
fn payment_path_naming_the_sendmax_issuer_is_the_default_strand() {
    run_bundle(include_str!("vectors/payment_path_naming_sendmax_issuer_is_default_strand_106739814.json"));
}

/// Finding 147 — #106743109 8D712D436C1D (rGEEkK5, 544.290782 XRP for up to
/// 100,000 ARMY through the ARMY/XRP pool, a single hop). The reverse pass
/// sizes the in from the want (99999.99992362 ARMY) and 7.638e-5 ARMY stays
/// with the sender; our tiny-sliver sweep — finding 131's rule for a pool that
/// feeds a NEXT step — spent the whole SendMax into the pool.
#[test]
fn payment_first_pool_hop_is_sized_by_the_reverse_pass() {
    run_bundle(include_str!("vectors/payment_first_pool_hop_is_sized_by_the_reverse_pass_106743109.json"));
}

// Finding 151 — #106743104 F80847602E68 (rLpnXUyv, 1.9M XRPH → RLUSD, 54
// flow iterations): rippled's `ActiveStrands` drops a strand for good once it
// flows nothing (it is never pushed to `next_`), so iterations 50-53 run a
// LONE strand under single-path pricing — the pool leg along the curve. We
// re-admitted the dry direct strand every other round and priced the pool
// leg multi-path at the anchored offer's ratio (13500.15 → 21.7125 where
// mainnet takes 118197.49 → 969.962495443).
#[test]
fn payment_dry_strand_never_returns() {
    run_bundle(include_str!("vectors/payment_dry_strand_never_returns_106743104.json"));
}

// Finding 157 — #106753769 DD2CD0BC4C81 (rARKjtjX pays 0.005461 ASC to
// r37rYnxT, whose account root carries lsfGlobalFreeze): `checkFreeze` makes
// the issuer → destination hop terNO_LINE, the default strand is dry and
// mainnet returns tecPATH_DRY. We paid. Both trust lines pinned untouched.
#[test]
fn payment_direct_hop_into_a_globally_frozen_account_is_dry() {
    run_bundle(include_str!("vectors/payment_direct_hop_into_a_globally_frozen_account_is_dry_106753769.json"));
}

/// Finding 162 (#106779252 3AD7EA863312): rJEvC4cuk pays 2,000,000
/// XRPFLORIDAGATORS to rpFYjv6SF. Both lines exist and the sender is funded,
/// but the issuer's NoRipple flag sits on both holders' lines, so rippled's
/// checkNoRipple refuses to ripple through it: no strand, tecPATH_DRY, fee
/// only. We delivered and moved both lines. Seven receipts that morning.
#[test]
fn payment_through_an_issuer_with_no_ripple_on_both_lines_is_dry_106779252() {
    run_bundle(include_str!(
        "vectors/payment_through_an_issuer_with_no_ripple_on_both_lines_is_dry_106779252.json"
    ));
}

/// Finding 165 (#106772946 C9E92CF8532F): rLc1HmTpWg's circular tfPartialPayment
/// crosses its own FUZZY offers, so one line is credited as the destination
/// and debited as the maker. rippled funds the maker from the ORIGINAL line
/// less the sixteen-digit fold of its debits (PaymentSandbox::balanceHookIOU,
/// deferred credits invisible); the last self-fill overshoots the live line
/// by 4e-11 and the line ends at −4e-11. We drained the live line to zero.
#[test]
fn payment_circular_self_fill_funds_from_the_original_balance_106772946() {
    run_bundle(include_str!("vectors/payment_circular_self_fill_funds_from_the_original_balance_s432.json"));
}

#[test]
fn payment_sweep_last_fill_is_bounded_by_the_deferred_debits_fold_106777783() {
    run_bundle(include_str!("vectors/payment_sweep_last_fill_is_bounded_by_the_deferred_debits_fold_106777783.json"));
}

#[test]
fn payment_sweep_last_fill_is_bounded_by_the_deferred_debits_fold_106773978() {
    run_bundle(include_str!("vectors/payment_sweep_last_fill_is_bounded_by_the_deferred_debits_fold_106773978.json"));
}

#[test]
fn payment_funds_limited_fill_clamps_to_the_offers_own_taker_pays_106759499() {
    run_bundle(include_str!("vectors/payment_funds_limited_fill_clamps_to_the_offers_own_taker_pays_106759499.json"));
}

#[test]
fn payment_forward_surplus_keeps_the_reverse_cache_106763015() {
    run_bundle(include_str!("vectors/payment_forward_surplus_keeps_the_reverse_cache_106763015.json"));
}

#[test]
fn payment_bound_sender_pays_the_line_bound_gross_verbatim_106782303() {
    run_bundle(include_str!("vectors/payment_bound_sender_pays_the_line_bound_gross_verbatim_106782303.json"));
}

#[test]
fn payment_parked_carry_take_back_is_capped_at_the_park_106733664() {
    run_bundle(include_str!("vectors/payment_parked_carry_take_back_is_capped_at_the_park_106733664.json"));
}

#[test]
fn payment_park_undo_reverts_the_creators_owner_count_106735554() {
    run_bundle(include_str!("vectors/payment_park_undo_reverts_the_creators_owner_count_106735554.json"));
}

#[test]
fn payment_anchored_pool_slice_and_its_level_share_one_pass_106770629() {
    run_bundle(include_str!("vectors/payment_anchored_pool_slice_and_its_level_share_one_pass_106770629.json"));
}

// Finding 181 — #106784160 A7D5C4E4960A: rKN4dh8q sends 10,000,000,000
// EverBurnX to its issuer rGKWaLgD holding NONE of it; the issuer's side of
// the line trusts rKN4dh8q for exactly that, so the one-step strand ISSUES
// (DirectStepI::maxPaymentFlow's `creditLimit2(dst, src) + srcOwed`) and
// mainnet moves the line 0 → 10B. We read the sender's holdings alone and
// answered tecPATH_DRY.
#[test]
fn payment_sender_issues_its_own_iou_to_the_issuer_within_its_limit_106784160() {
    run_bundle(include_str!("vectors/payment_sender_issues_its_own_iou_to_the_issuer_within_its_limit_106784160.json"));
}

// #106743104 F8084760 again, now with the book state rippled actually saw
// (200-offer seeding): the XRPH/XRP levels 5103CE…5111805A are consumed by
// iteration 49, the direct strand's pool slice overflows and it goes
// unboundable — dropped, since two strands are pending — while the bridge
// strand is still bounded by the XRPH/XRP level 511FF98B the shallow bundle
// lacked; iterations 50-53 run it alone (single-path anchored pool offers)
// and the payment clears DeliverMin: tesSUCCESS, 181 mutations.
#[test]
fn payment_lone_strand_runs_on_after_its_rival_goes_unboundable_106743104() {
    run_bundle(include_str!("vectors/payment_lone_strand_runs_on_after_its_rival_goes_unboundable_106743104.json"));
}

// Finding 183 — #106788656 1B9CE050C5DB (rfcoGE3E59, 200 RLUSD → USDC,
// tfPartialPayment, six strands): iteration 0 spends 0.0002455730745894072
// RLUSD through the CNY strand. rippled's `remainingIn` falls by the strand's
// OWN reported in; we measured the spend by differencing the sender's RLUSD
// line, whose 16-digit balance resolves 1e-12, and read 0.000245573075 — so
// iteration 1's in-limited fill of maker A3F9DCB7 was 199.999754426925 for
// mainnet's 199.9997544269254 and the offer's TakerPays rested 4 ulp high.
// The walk's own figure is trusted whenever it sits within the line's quantum
// of the balance delta.
#[test]
fn payment_strand_in_beats_the_lines_quantised_delta_106788656() {
    run_bundle(include_str!("vectors/payment_strand_in_beats_the_lines_quantised_delta_106788656.json"));
}

// Finding 190 — #106800968 13567222623C (rMF4Tg8Sm3, circular XRP → SGB.rctA
// under tfPartialPayment; second path [SGB/rhrFfvzZ, account rhrFfvzZ]):
// toStrand appends the deliver issuer as an account element, so the strand
// closes with DirectSteps rhrFfvzZ → rctArjqV → sender across the gateways'
// mutual line (rhrFfvzZ holds 6.32M SGB.rctA). rippled keeps it ACTIVE —
// it delivers nothing, but two active strands put the XRP/SGB pool in
// multi-path mode and the first fib slice prices 994000 drops linearly at
// 1256.908860292824 SGB. We dropped the path, ran single-path and re-curved
// the anchored offer to 1257.12637193.
#[test]
fn payment_inter_gateway_tail_keeps_the_strand_active_106800968() {
    run_bundle(include_str!("vectors/payment_inter_gateway_tail_keeps_the_strand_active_106800968.json"));
}

// Finding 192 — #106804064 335BC2814C3A (r9nKwYtEbe, circular 100M XQK → XRP
// under tfPartialPayment): iteration 0 takes 16217439 drops from the XQK/XRP
// pool into the sender; iteration 1 meets the sender's OWN XRP-for-XQK offer.
// PaymentSandbox defers the credit, so rippled funds that offer from the
// original balance alone (10489581 drops after the reserve), fills exactly
// that, finds the offer unfunded and removes it, then takes the pool again
// and rGW67HJbw's offer. We let the fresh 16217439 drops fund it too and
// filled 23473874 in one go, ending the flow early.
#[test]
fn payment_own_offer_is_funded_by_the_original_xrp_balance_106804064() {
    run_bundle(include_str!("vectors/payment_own_offer_is_funded_by_the_original_xrp_balance_106804064.json"));
}

// Finding 200 — #106807323 9D5AD40FD56E (rw6khHVtjd, tfPartialPayment 1400 XRP
// → 33567640.000661 FUZZY, third round through the XRP/FUZZY pool):
// swapAssetOut(21496358.22437994) asks 897232553 drops, one over the 897232552
// left, so the pass is in-limited and swapAssetIn yields 21496358.22438000 —
// 6e-8 over the want. rippled's forward pass never delivers that surplus: the
// strand's last DirectStep keeps its REVERSE cache when the forward input
// exceeds it (setCacheLimiting), crediting the want while the pool parts with
// the whole swap. Mainnet's line lands on …9397072; ours credited …073.
#[test]
fn payment_in_limited_pool_slice_delivers_no_more_than_the_want_106807323() {
    run_bundle(include_str!("vectors/payment_in_limited_pool_slice_delivers_no_more_than_the_want_106807323.json"));
}

// Finding 203 — #106811381 8279411B2B5A (rBf5SF3p3U, tfPartialPayment|
// tfLimitQuality 250 XRP → 1902.542098 XLM through RLUSD and CNY; #106815274
// 937E7F418B92 is the same bot again): the strand's quality function takes a
// pool's shape at a hop only when rippled's `tipOfferQualityF` would — the
// pool's fee-inclusive spot must beat the tip; a payment step's
// `qualityThreshold` is the tip itself. A clause compared the STRAND's limit
// (drops per XLM) with the HOP's tip (drops per RLUSD), unit-blind, and folded
// the XRP/RLUSD pool's slope in where rippled folds the CLOB tip. The
// function's spot sat 0.14% low, `limitOut` trimmed iteration 2 to 4.297 XLM
// where rippled's 149.1506565923698 filled: 57188990 drops → 435.65 XLM on
// mainnet against our 38153979 → 290.8.
#[test]
fn payment_strand_quality_function_takes_the_clob_tip_where_the_pools_spot_misses_it_106811381() {
    run_bundle(include_str!("vectors/payment_strand_quality_function_takes_the_clob_tip_where_the_pools_spot_misses_it_106811381.json"));
}

// Finding 204 — #106813814 BAB4F7ABBA64 (rwUx1Zgz7U, 100000 drops of
// tfPartialPayment|tfLimitQuality through the XRP/BTC pool, the BTC/RLUSD book
// and the RLUSD/FUZZY pool): the reverse pass asks 100001 drops of a 100000
// SendMax, so rippled's forward pass is driven by the INPUT at every book
// step — `fwdImp` hands `limitStepIn` the whole carry — and only the closing
// DirectStep clamps the delivery to the reverse want. Each hop here bought
// exactly the reverse want and left the carry's rounding surplus (3.3e-14 BTC)
// with the sender; mainnet pushed it through the BTC/RLUSD offer and the
// RLUSD/FUZZY pool. Intermediate hops of an input-limited strand now walk in
// sell mode and hand their surplus on; the last pool parts with the whole
// swap and delivers the want (finding 200).
#[test]
fn payment_input_limited_strand_drives_its_middle_hops_by_the_carry_106813814() {
    run_bundle(include_str!("vectors/payment_input_limited_strand_drives_its_middle_hops_by_the_carry_106813814.json"));
}

// Finding 209 — #106823197 4332E5712967 (rapido5rxP, a circular LTC → XRP
// arbitrage paying 12727272727272.72 LTC through two offers): rippled
// subtracts IOUs through `Number` at the sixteen-digit scale, where a
// difference whose coarse mantissa lands exactly on 10^15 with digits left in
// the guard is rounded there instead of receiving its sixteenth digit.
// 12727272727272.72 − 2727272727272.727 is exactly 9999999999999.993; the
// ledger's line holds …990, the next DirectStep is bounded by it, and the
// second maker's line and offer residual follow (…990 and 0.01 against our
// …993 and 0.007).
#[test]
fn payment_iou_subtraction_at_the_cusp_rounds_the_coarse_mantissa_106823197() {
    run_bundle(include_str!("vectors/payment_iou_subtraction_at_the_cusp_rounds_the_coarse_mantissa_106823197.json"));
}

// Finding 210 — #106823210 813F674F2C38 (rapido5rxP, 0.20682 XRP → WETH →
// LAWAS → LTC, tfPartialPayment|tfLimitQuality): rippled's `BookStep::fwdImp`
// re-prices an offer whose forward output exceeds the reverse pass's cached
// out (`limitStepOut(cache_->out)`, for a single-path pool `swapAssetOut`),
// and when the input that requires equals the input provided it consumes
// that input and produces exactly the cached output — the surplus stays in
// the pool. The forward swap of the exact reverse input 0.0001165445495622757
// WETH yields 14360.3597828 LAWAS against the reverse's 14360.35978279596
// (Number's cancellation in `swapAssetIn` keeps twelve digits); we carried
// the 4e-9 into the LAWAS/LTC pool and both pools' lines moved.
#[test]
fn payment_forward_pool_hop_reanchors_to_the_cached_out_106823210() {
    run_bundle(include_str!("vectors/payment_forward_pool_hop_reanchors_to_the_cached_out_106823210.json"));
}

// Finding 210, second specimen — #106823225 1699E047B113 (the same bot,
// 0.035827 XRP → TWINS → BITx → LTC): one line two ulps off the same way.
#[test]
fn payment_forward_pool_hop_reanchors_to_the_cached_out_second_specimen_106823225() {
    run_bundle(include_str!("vectors/payment_forward_pool_hop_reanchors_to_the_cached_out_second_specimen_106823225.json"));
}

// Finding 217 — #106824781 8B1596BFACC9 (rapido5rxP selling XWLF for
// 59.934028 XRP with SendMax 3526.095273376751 against a line of
// 2350.730182251167, eleven book fills): rippled's `remainingIn` counts down
// from SendMax — `sendMax − sum(savedIns)` — and the sender's holding enters
// only through the DirectStep's per-iteration bound, min(live line, original
// − Σdebits). Clamping the pot to the holding up front made the fold's
// remainder the exhausting fill's size, 206.6261549223710 against a line
// holding …718: the line kept 8e-16 and the last maker's offer and line sat
// eight and one ulps off, where mainnet's last DirectStep is "Limiting … in:
// 206.6261549223718" and the line closes at zero.
#[test]
fn payment_pot_counts_down_from_sendmax_and_hop_zero_is_bounded_by_the_holding_106824781() {
    run_bundle(include_str!("vectors/payment_pot_counts_down_from_sendmax_and_hop_zero_is_bounded_by_the_holding_106824781.json"));
}

// Finding 218 — #106824803 539C94786AED (rnXYxidJfj: 0.05 XRP → CSC → BITx
// → RLUSD, tfPartialPayment|tfLimitQuality with DeliverMin, SendMax of
// exactly the reverse-required 50000 drops): a strand whose reverse pass finds
// nothing limiting runs no forward pass — `flow()` walks forward only from
// the limiting step, and a spend that exactly meets the reverse in is not
// limiting — so the reverse-sized amounts execute verbatim: one iteration, in
// 50000, out 0.0702782901285746. Hop 0 spending its whole input is
// forward-driving only when it also left its reverse want unmet; an exact
// meet treated as input-limited re-swapped hops 1 and 2 and Number's
// cancellation dropped the tails, two pool lines an ulp off.
#[test]
fn payment_exact_spend_executes_the_reverse_pass_verbatim_106824803() {
    run_bundle(include_str!("vectors/payment_exact_spend_executes_the_reverse_pass_verbatim_106824803.json"));
}

// Finding 219 — #106826590 DDE6F9E974AB (raRBY29mxK paying USD, issuer rate
// 1.0015, for XRP under tfPartialPayment with SendMax 0.0144553203466773
// against a line of 0.01417191457932401): the holding is gross. Compared
// with the net budget it never bound, and the verbatim gross cap the
// exhausting fill debits was SendMax itself, so the line went to −2.8e-4
// where mainnet's DirectStep spends exactly the line and closes it at zero.
// The holding now nets down for the budget and caps the gross cap.
#[test]
fn payment_hop_zero_gross_cap_is_bounded_by_the_holding_106826590() {
    run_bundle(include_str!("vectors/payment_hop_zero_gross_cap_is_bounded_by_the_holding_106826590.json"));
}

// Finding 220 — #106825938 D97A404AA9BF (r9tcGwSyYP: 0.1 XRP → WETH → USDC →
// RLUSD under tfPartialPayment|tfLimitQuality, the reverse asking 100001
// drops of a 100000 SendMax): an input-limited strand's forward pass drives
// its LAST book step by input as well — `fwdImp` hands `limitStepIn` the
// whole carry, the RLUSD/USDC offer gives 0.1384312667324735 for it, 33 ulps
// over the want — and the closing DirectStep clamps the delivery to the
// reverse want; the excess is redeemed against the issuer. Out-limiting the
// last fill to the want left 3.257e-13 USDC of carry flushed into a pool and
// the maker's residual 31 and 33 ulps off.
#[test]
fn payment_input_driven_last_book_hop_consumes_the_carry_106825938() {
    run_bundle(include_str!("vectors/payment_input_driven_last_book_hop_consumes_the_carry_106825938.json"));
}

/// Finding 223 (#106831931 202239C6B762): rKLpjpCoXg's tfPartialPayment +
/// tfLimitQuality self-payment, 1106798 XRPS → 1984.11 CNY through the
/// XRPS/CNY pool. StrandFlow's limitOut trims the ask to 0.09999996787867368
/// CNY via the pool's quality function (adjustedRemOut = true), and the fill
/// lands 1.3e-9 over the limit — inside the 1e-7 forgiveness rippled grants
/// a TRIMMED request. Our pool judge recomputed the trim, found it equal to
/// the already-trimmed ask, called it "not adjusted" and refused: tecPATH_DRY
/// against mainnet's 0.09999996787867368 CNY delivered.
#[test]
fn payment_limit_quality_trimmed_ask_keeps_the_forgiveness_106831931() {
    run_bundle(include_str!("vectors/payment_limit_quality_trimmed_ask_keeps_the_forgiveness_106831931.json"));
}

/// Finding 227 (#106842419 9450A8A0F31A): rHgg35915, the CNY gateway, pays
/// ITSELF 4000 CNY/rHgg for XRP over [CNY/rKiCet8, rKiCet8]. The destination
/// is the deliver issuer, so toStrand appends no issuer hop and the strand
/// ends with DirectStepI(rKiCet8 → rHgg) — kept because that trust line
/// exists. Mainnet buys 4000 CNY.rKiCet8 into rHgg's line; we dropped the path
/// by shape and refused tecPATH_PARTIAL.
#[test]
fn payment_self_payment_keeps_the_lined_terminal_ripple_step_106842419() {
    run_bundle(include_str!("vectors/payment_self_payment_keeps_the_lined_terminal_ripple_step_106842419.json"));
}

/// Finding 228 (#106842607 DB75971543DE): rPGurZ522z's tfPartialPayment +
/// tfLimitQuality self-payment CNY.rKiCet8 → USD.rKiCet8 through the pool.
/// A payment step's qualityThreshold IS the book tip, so rippled anchors the
/// pool's offer on the tip even though the tip sits beyond the limit and is
/// never crossed: iteration 0 fills the tip-anchored 0.000280601767372 USD,
/// iteration 1 is rejected by limitQuality. Our tail turn dropped the anchor
/// by the offer-crossing rule and consumed the whole trimmed ask.
#[test]
fn payment_tail_turn_anchors_on_the_raw_tip_106842607() {
    run_bundle(include_str!("vectors/payment_tail_turn_anchors_on_the_raw_tip_106842607.json"));
}

/// Finding 231 (#106847596 7212BA04A61F): rapido5rxP's self-payment, RLUSD →
/// CNY.rKiCet8 → XRP, is in-limited on the CNY offer C499497938D8
/// (13.17986826120811 CNY for 1.990916655771713 RLUSD) with 1.990916655771711
/// RLUSD to spend. rippled's ceilIn derives the output from the input at the
/// offer's stored quality, 13.17986826120872, and then CLAMPS it to the
/// offer's own 13.17986826120811 (Quality.cpp ceilInImpl); we handed the
/// maker's line the unclamped figure, 61 ulps more than the offer held.
#[test]
fn payment_in_limited_fill_clamps_to_the_offers_out_106847596() {
    run_bundle(include_str!("vectors/payment_in_limited_fill_clamps_to_the_offers_out_106847596.json"));
}

/// Finding 248 (#106889388 2817B7C6DBDD): rww2AZLgG3's partial payment of
/// 500 XRP into XPM takes the 3460-drops/XPM level whole — raFN5J's
/// 1312.16098265896 and rKnEWE's 363.3858381502891 — and the level cannot
/// fill the 145457 XPM want, so the pass is LIMITING: rippled re-runs the
/// book step's rev with the level's 16-digit fold, 1675.546820809249, and
/// that re-run sizes rKnEWE by limitStepOut at the page rate:
/// 1675.546820809249 − 1312.16098265896 = 363.385838150289 for its whole
/// 1257315 drops (StrandFlow.h:176-184, BookStep.cpp revImp). Finding 149
/// ported that for crossings; a payment hop takes it now. rKnEWE's line
/// ends at 0.0006124416636 (we wrote …635); the offer is deleted either way.
#[test]
fn payment_limiting_book_hop_re_runs_with_the_levels_sixteen_digit_fold_106889388() {
    run_bundle(include_str!("vectors/payment_limiting_book_hop_re_runs_with_the_levels_sixteen_digit_fold_106889388.json"));
}

/// Finding 252 (#106905925 A2A90EE3FC9A): rH4krKvBq pays itself 6.399968
/// USD.rvYAfWj5 for up to 9.106178 XRP with NO tfPartialPayment, holding
/// 7.99995 XRP against an 8 XRP reserve — nothing to spend. rippled builds
/// the strand (an XRP source needs no line), the XRP/USD pool offers, the
/// source's `accountHolds` reads 0 and the strand is found dry in rev: "All
/// strands dry. Total flow: in: 0 out: 0" — and the driver's ending makes a
/// dry, non-partial flow tecPATH_PARTIAL (StrandFlow.h: `!partialPayment` is
/// judged before `actualOut == 0`). Our pre-driver "sender has nothing to
/// spend" guard said tecPATH_DRY, the verdict rippled reserves for a
/// tfPartialPayment that delivers nothing. Fee-only either way.
#[test]
fn payment_dry_source_without_partial_payment_is_tec_path_partial_106905925() {
    run_bundle(include_str!("vectors/payment_dry_source_without_partial_payment_is_tec_path_partial_106905925.json"));
}

/// Finding 255 — #106908423 2F90DCC6EDD9: rJyPE3eyHb pays 243.26322084 ArcX
/// to rPHuB6xke4 from a line holding 0 (the issuer extends it no credit),
/// no tfPartialPayment. The sender→issuer DirectIPaymentStep's `check`
/// (DirectStep.cpp:450-460, `owed <= 0 && -owed >= limit`) refuses the
/// strand at BUILD — libxrpl: "DirectStepI: dry: owed: 0/ArcX limit:
/// 0/ArcX" — so tecPATH_DRY whatever the flag; finding 252's flow-time
/// reading claimed tecPATH_PARTIAL. (The bundle seats the destination's
/// line by hand: the fetch does not gather a fee-only tx's far line, and
/// without it the dest-no-line guard would answer DRY for the wrong reason.)
#[test]
fn payment_iou_sender_holding_nothing_is_tec_path_dry_at_strand_build() {
    run_bundle(include_str!("vectors/payment_iou_sender_holding_nothing_is_tec_path_dry_at_build_106908423.json"));
}

/// Finding 258 — #106908905 94F7CFBF7ADB: 34322054.3 USDCAllow→USDC through
/// the bridge offer 575275AC whose TakerPays = TakerGets = 9999999999999999e80.
/// The fill leaves both amounts untouched in STAmount arithmetic and rippled
/// drops the identical node (ApplyStateTable.cpp:152) — the offer is pinned
/// to its pre-image here. `me_to_value_string` capped the exponent at 40
/// zeros, so we wrote the offer back as 9999999999999999e40 and threaded it.
#[test]
fn payment_book_fill_leaves_a_1e96_offer_untouched() {
    run_bundle(include_str!("vectors/payment_book_fill_leaves_a_1e96_offer_untouched_106908905.json"));
}

/// Finding 260 — #106913409 993BFC235125: r9nKwYtEbe's partial circular
/// RNTB→XRP payment (DeliverMin) crosses the pool, then its OWN offer
/// C0C94607. rippled's `accountSend` is a no-op for sender == receiver, so
/// the RNTB line never moves on that fill while the strand's actualIn
/// (741178.0257434814) still exhausts SendMax. We credited and debited the
/// same line — 2 ulp of drift — which made the driver distrust the walk's
/// spend, keep crossing, and over-consume the offer (6218711 for 7264340).
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409() {
    run_bundle(include_str!("vectors/payment_self_owned_offer_cross_moves_no_line_106913409.json"));
}

/// Finding 262 — #106913870 B63308CA10E3: rapido's partial XRP→USDT
/// self-payment with DeliverMin 0.0777 against a USDT line whose limit is
/// 9999999999999999e79. `dest_receivable` computed limit − held through
/// `me_sub`, whose u128 rescale saturates 104 orders down, and the room
/// read 0.034 USDT: the strand was sized to it and the payment answered
/// tecPATH_PARTIAL where mainnet delivers 0.07781135333001987.
#[test]
fn payment_dest_limit_at_stamount_ceiling_is_not_a_cap_106913870() {
    run_bundle(include_str!("vectors/payment_dest_limit_at_stamount_ceiling_is_not_a_cap_106913870.json"));
}

/// Finding 265 — #106920314 BD1C16580721: rGPdpPN2 redeems its whole
/// 16.5947297373 JPY to the issuer rB3gZey7, which has lsfGlobalFreeze set.
/// A one-step strand skips `checkFreeze` and sizes from the raw holding
/// (`accountHolds(IgnoreFreeze)`), so mainnet moves it — tesSUCCESS; our
/// frozen-holder-reads-zero funding made finding 255 call it tecPATH_DRY.
#[test]
fn payment_redeems_globally_frozen_iou_to_its_issuer_106920314() {
    run_bundle(include_str!("vectors/payment_redeems_globally_frozen_iou_to_its_issuer_106920314.json"));
}

/// Finding 271 — the reverse-pass sizing ladder escalates past a rung that
/// yields nothing (below the hop's quantum) instead of reading it as a
/// liquidity bound: five pools, 81 drops out, and the FACE/XRP pool must be
/// sized from the drops, not consumed at its maxOffer.
#[test]
fn payment_five_pool_path_rev_pass_escalates_past_a_sub_drop_rung_106921383() {
    run_bundle(include_str!("vectors/payment_five_pool_path_rev_pass_escalates_past_a_sub_drop_rung_106921383.json"));
}

/// Finding 283 (#106983394 5B4E06B079D4, rapido5rxP RLUSD → USD → CNY → XRP):
/// the CNY.rKiCet8/XRP book carries an EMPTY root page at 4D0ADF550834D180,
/// better than every resting offer and than the pool's spot. rippled's
/// `BookTip::step` passes empty pages, so its tip is 4F21342332122000 and
/// the pool's `changeSpotPriceQuality` slice carries the whole 519.24 CNY;
/// our level list took the empty page as the tip, refused the pool and
/// consumed two resting offers instead. The page appears in no meta, so this
/// bundle carries its parent-ledger image by hand.
#[test]
fn payment_empty_book_page_is_no_level_the_pool_slice_beats_the_real_tip_106983394() {
    run_bundle(include_str!("vectors/payment_empty_book_page_is_no_level_the_pool_slice_beats_the_real_tip_106983394.json"));
}

/// Finding 285 (#106983955 D4DD62F77155): the pool's slice takes everything
/// the taker holds, but the rev pass is sized by the want — it consumes the
/// tip whole, steps to the next level and reaps the expired offer there
/// (offer, emptied page, owner count) before the DirectStep limits the strand.
#[test]
fn payment_pool_served_pass_with_output_still_wanted_steps_the_book_and_reaps_the_expired_next_level_106983955() {
    run_bundle(include_str!("vectors/payment_pool_served_pass_with_output_still_wanted_steps_the_book_and_reaps_the_expired_next_level_106983955.json"));
}

/// Finding 286 (#106988696 D9DC48A444E4): a Payment carrying CredentialIDs is
/// judged by rippled's `credentials::valid` in preclaim — every id must exist,
/// name the sender as Subject and be accepted — before any deposit-auth test.
/// One of rwc9Dqir's five ids is a credential it ISSUED, not one it holds:
/// tecBAD_CREDENTIALS, fee only.
#[test]
fn payment_credential_ids_must_name_the_sender_as_subject_and_be_accepted_106988696() {
    run_bundle(include_str!("vectors/payment_credential_ids_must_name_the_sender_as_subject_and_be_accepted_106988696.json"));
}

/// Finding 289 (#106991212 0B722FF6EA7F): r9tcGwSyYP pays itself 0.071 RLUSD
/// for 50000 drops, XRP → XUSD → USDC.axl → RLUSD, the first two hops
/// pool-served. The last hop's forward pass carries 0.0710127 USDC.axl onto
/// rpkHXWZu's 97.3 head offer: `fwdImp` trims the fill to the remaining
/// input (`limitStepIn`, processMore = false) and the stream never steps.
/// Our in-driven walk took no in-cap on its rev extent and trailed past the
/// head after the fill, reaping rDeXHakZ's three dead offers and their pages
/// — 17 mutations against mainnet's 9. The seven pins are untouched-object
/// pins: the offers, their pages and the owner's root must stay as seated.
#[test]
fn payment_in_driven_fwd_pass_stops_at_a_trimmed_head_offer_106991212() {
    run_bundle(include_str!("vectors/payment_in_driven_fwd_pass_stops_at_a_trimmed_head_offer_106991212.json"));
}

/// Finding 290 (#106992486 1E418A4F5724): rDireAucG's partial self-payment
/// of a sentinel XRP amount for 226662 ATM through the ATM/XRP pool. The
/// pool's changeSpotPriceQuality offer is priced between the spot and the
/// tip, so `execOffer(tip)` fails `*ofrQ != offer.quality()` right after it
/// and the pass ends at the pool with nothing stepped — and the next
/// iteration's pool offers again, so the stream never reaches the expired
/// FC1110C8 behind the tip. Finding 285's rule (step the book after a
/// pool-served pass while the SendMax budget lasts) fired on the budget
/// alone and reaped it: the three pins hold the offer, its page and the
/// owner's root untouched.
#[test]
fn payment_pool_that_offers_again_ends_the_pass_at_the_pool_no_stepping_106992486() {
    run_bundle(include_str!("vectors/payment_pool_that_offers_again_ends_the_pass_at_the_pool_no_stepping_106992486.json"));
}

/// Findings 294 + 295 — #107052630 32386DDEB6B8: rUnRkdr pays 117.732708 USDT
/// with FIL through the explicit issuer hop [rsL5Y] and the XRP bridge, and
/// strand 1 crosses its OWN two FIL/USDT offers (100 FIL, then 9.48 of 186).
/// (294) rippled's DirectStep debits the sender the gross once and
/// `consumeOffer`'s issuer→owner send credits the owner the net per fill — the
/// same line, so only the 0.1% fee stays: −(35.169 × 1.001) − 109.4805 × 0.001.
/// The mixed strand's run-fed fiction restore erased the owner credits and
/// left the FIL line 109.48 low. (295) The destination's USDT line took five
/// gross credits and one fee trim from the full-precision accumulator and
/// rested at …7079999999; a completed delivery lands on pre + Amount exactly.
#[test]
fn payment_own_offers_behind_an_issuer_hop_are_credited_107052630() {
    run_bundle(include_str!("vectors/payment_own_offers_behind_an_issuer_hop_are_credited_107052630.json"));
}

/// Finding 296 — #107009438 2877CBCC88C9: a deliver-max partial payment of
/// 166716 drops for RLUSD through USDC.axl. rippled's REVERSE pass, asked for
/// everything, walks the whole USDC.axl/RLUSD book: past the one funded tip it
/// steps over rDeXHa's three unfunded offers and marks them `ofrsToRm`; the
/// forward pass buys 0.2129 RLUSD from the tip alone, and the driver still
/// deletes the three (plus their book pages, the owner page, OwnerCount) after
/// the iteration — 17 mutations. Our reverse sizing ran in a snapshot, so
/// those reaps were rolled back and the ledger showed 9.
#[test]
fn payment_reverse_pass_reaps_survive_the_snapshot_107009438() {
    run_bundle(include_str!("vectors/payment_reverse_pass_reaps_survive_the_snapshot_107009438.json"));
}

/// Finding 297 — #107056200 BD9C7473B84F: rhTsmUJ's 6 XRP partial self-payment
/// (DeliverMin) into RVR meets rMBPaL7's fresh offer at the tip and a 0.506%
/// pool. `AMMLiquidity::getOffer` stands the pool aside only when the RAW pool
/// quality (`Quality{balances}`, no fee) is not strictly better than the tip
/// or sits within 1e-7 of it; the fee enters only in the anchored offer
/// `changeSpotPriceQuality` then generates. We judged the fee-inclusive spot,
/// 5.5e-8 inside the tip, and let the offer take all 6 XRP; mainnet's raw spot
/// is 0.5% better, the pool's anchored slice is 52 drops for 0.428107876 RVR
/// (shim trace: "changeSpotPriceQuality succeeded … 52 0.428107876") and the
/// tip fills the other 5999948 — seven mutations, ours had five.
#[test]
fn payment_pool_stands_aside_on_raw_quality_not_fee_spot_107056200() {
    run_bundle(include_str!("vectors/payment_pool_stands_aside_on_raw_quality_not_fee_spot_107056200.json"));
}

/// Findings 299–303 — the first vectors from the differential fuzzer
/// (`differential_probe --fuzz`): unsigned mutants of real #107009438
/// transactions, judged by libxrpl 3.4.0 on the same pre-state. The
/// expectation is libxrpl's verdict; there is no mainnet metadata because
/// a tem never reaches a ledger.
#[test]
fn payment_xrp_direct_with_partial_flag_is_malformed_fuzz_107009438() {
    run_bundle(include_str!("vectors/payment_xrp_direct_with_partial_flag_is_malformed_fuzz_107009438.json"));
}
#[test]
fn payment_no_ripple_direct_without_paths_is_ripple_empty_fuzz_107009438() {
    run_bundle(include_str!("vectors/payment_no_ripple_direct_without_paths_is_ripple_empty_fuzz_107009438.json"));
}
#[test]
fn payment_deliver_min_without_partial_is_bad_amount_fuzz_107009438() {
    run_bundle(include_str!("vectors/payment_deliver_min_without_partial_is_bad_amount_fuzz_107009438.json"));
}
#[test]
fn payment_ninety_six_digit_amount_parses_fuzz_107009438() {
    run_bundle(include_str!("vectors/payment_ninety_six_digit_amount_parses_fuzz_107009438.json"));
}

/// Finding 304 — from the differential fuzzer: tfLimitQuality on a
/// deliver-max partial payment (Amount 9999999999999990e79 RLUSD for 200000
/// drops). rippled's limit is getRate(Amount, SendMax); a ratio below the
/// STAmount floor files rate 0, Quality(0) is the best quality there is, and
/// every strand is "rejected by limitQuality": tecPATH_DRY. Our encoder
/// wrapped the exponent into a rate no strand could fail.
#[test]
fn payment_limit_quality_below_the_stamount_floor_rejects_every_strand_fuzz_107009438() {
    run_bundle(include_str!("vectors/payment_limit_quality_below_the_stamount_floor_rejects_every_strand_fuzz_107009438.json"));
}

/// Finding 307 — from the differential fuzzer (rogue5Hn's PLX→GALLOWS
/// payment with tfLimitQuality added): rippled's `limitOut` hands back the
/// remainder UNTRIMMED when the solved out is within 1e-9 relative of it
/// ("A tiny difference could be due to the round off"), so `adjustedRemOut`
/// stays false and the 1e-7 judge forgiveness never applies. We trimmed by
/// 1.2e-13 relative, called the ask adjusted, and forgave a pass rippled
/// rejects: tecPATH_DRY.
#[test]
fn payment_limit_out_within_a_billionth_is_not_a_trim_fuzz_107009438() {
    run_bundle(include_str!(
        "vectors/payment_limit_out_within_a_billionth_is_not_a_trim_fuzz_107009438.json"
    ));
}

/// Finding 311 (#107064266 12C8A416327C, soak-17 receipt): rogue5Hn's
/// PLX→LHT→CSC payment met hop 1 with one book offer; the CSC/LHT pool
/// merely existed, yet the 5.3e-11 LHT carry overshoot was flushed into it
/// and its LHT line rounded up one ulp — a ninth mutation mainnet never
/// wrote. The flush goes through the pool only when the hop's own walk took
/// the pool. The pool's LHT line is pinned untouched in `expect`.
#[test]
fn payment_overshoot_flushes_only_through_a_pool_the_hop_took_107064266() {
    run_bundle(include_str!(
        "vectors/payment_overshoot_flushes_only_through_a_pool_the_hop_took_107064266.json"
    ));
}

/// Findings 312-316 — from the structural differential fuzzer (sweep 3 on
/// 107060755); libxrpl's result is the expectation.
#[test]
fn payment_last_ledger_behind_the_ledger_is_tefmax_ledger_fuzz_107060755() {
    run_bundle(include_str!("vectors/payment_last_ledger_behind_the_ledger_is_tefmax_ledger_fuzz_107060755.json"));
}

/// Findings 312-316 — from the structural differential fuzzer (sweep 3 on
/// 107060755); libxrpl's result is the expectation.
#[test]
fn payment_future_sequence_is_terpre_seq_fuzz_107060755() {
    run_bundle(include_str!("vectors/payment_future_sequence_is_terpre_seq_fuzz_107060755.json"));
}

/// Findings 312-316 — from the structural differential fuzzer (sweep 3 on
/// 107060755); libxrpl's result is the expectation.
#[test]
fn payment_zero_fee_is_valid_and_applies_fuzz_107060755() {
    run_bundle(include_str!("vectors/payment_zero_fee_is_valid_and_applies_fuzz_107060755.json"));
}

/// Findings 317/318 — from the structural differential fuzzer; libxrpl's
/// result is the expectation.
#[test]
fn payment_mpt_direct_with_no_ripple_direct_is_invalid_flag_fuzz_testnet_20863937() {
    run_bundle(include_str!("vectors/payment_mpt_direct_with_no_ripple_direct_is_invalid_flag_fuzz_testnet_20863937.json"));
}

/// Findings 320-326 — from the testnet campaign's differential fuzz (libxrpl's
/// result is the expectation).
#[test]
fn escrow_create_cancel_after_at_or_before_finish_after_is_bad_expiration_fuzz_testnet_20864035() {
    run_bundle(include_str!("vectors/escrow_create_cancel_after_at_or_before_finish_after_is_bad_expiration_fuzz_testnet_20864035.json"));
}

/// Finding 334 — the devnet campaign's permissioned-DEX hybrid offer.
#[test]
fn payment_consuming_a_hybrid_offer_unlinks_its_open_book_entry_devnet_5419040() {
    run_bundle(include_str!("vectors/payment_consuming_a_hybrid_offer_unlinks_its_open_book_entry_devnet_5419040.json"));
}

/// Finding 333 — devnet 5422959 FE1A0200A515: a domain payment (rQNx to
/// itself, SendMax 5 USD for 1 XRP) whose domain book cannot fill it →
/// tecPATH_PARTIAL, fee only.
#[test]
fn payment_in_domain_short_of_liquidity_is_path_partial_devnet_5422959() {
    run_bundle(include_str!("vectors/payment_in_domain_short_of_liquidity_is_path_partial_devnet_5422959.json"));
}

/// Finding 342 — #107093460 D125FEC040CD: a CCR payment into a DepositAuth
/// destination carrying one accepted, unexpired CredentialID; the
/// destination pre-authorised that (Issuer, CredentialType) — the
/// DepositPreauth-by-credentials object is in the pre-state. We refused it.
#[test]
fn payment_with_credentials_into_a_deposit_auth_destination_107093460() {
    run_bundle(include_str!("vectors/payment_with_credentials_into_a_deposit_auth_destination_107093460.json"));
}

/// Finding 342 — #107093959 1A036B0C6D6F: the sibling bot account, same shape.
#[test]
fn payment_with_credentials_into_a_deposit_auth_destination_107093959() {
    run_bundle(include_str!("vectors/payment_with_credentials_into_a_deposit_auth_destination_107093959.json"));
/// Finding 341 — #107093372 BE1B5D257244: a partial XAH→RLUSD payment whose
/// first iteration leaves a 1e-14 remainder. The pool wins iteration two
/// anchored at the next tip's quality, its forward swap of the 1.55e-12 XAH
/// left yields nothing, the strand is dry and rippled's flow ends. We filled
/// the remainder from the tip behind the pool (its owner's XAH line pinned).
#[test]
fn payment_pool_dry_iteration_ends_the_flow_107093372() {
    run_bundle(include_str!("vectors/payment_pool_dry_iteration_ends_the_flow_107093372.json"));
}
