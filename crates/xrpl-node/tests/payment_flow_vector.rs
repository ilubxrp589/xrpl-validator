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
