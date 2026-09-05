//! Byte-exact vector drills for the class-B line-ULP family (2026-09-01).
//!
//! One bot (rJfVTbJs…, tfSell|tfIoC, always selling Bitstamp IOUs, issuer
//! rate 1.0015) generated the six-fix era's dominant byte-diff family. The
//! shadow flags a one-ULP Balance divergence on the maker/taker lines the
//! fill credits — ours-len == ffi-len, PRE-OK, tx-tagged. Root cause (F50):
//! rippled's flowCross grosses sendMax ONCE (multiplyRound roundUp) and the
//! in-limited final fill consumes the remaining GROSS verbatim, deriving
//! its net by division (limitStepIn) — our walk tracked the NET budget and
//! re-grossed per leg, one conversion too many.
//!
//! - 106679738 DA3C22D8: USD→XRP, real offer + AMM; maker line 0339209F…
//!   must land …759 (net chain said …760). The founding specimen.
//! - 106679743 AA7BF8C5: same key five ledgers later — the recurrence pin.
//! - 106688646 5524F0F2: ETH→RLUSD (IOU/IOU) — the 4-hot-key cluster
//!   specimen (A6203B55/77AC9AC2/A46F22FC/27577E3A), every touched node
//!   byte-compared.
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
    // The bundle carries the ledger's verdict ("result"); a tec pin compares
    // the fee-only root exactly like a success pin compares its targets.
    let want_ter = bundle["result"].as_str().unwrap_or("tesSUCCESS");
    assert_eq!(ter, want_ter, "mainnet's result for this OfferCreate");

    xrpl_ledger::ledger::threading::stamp_threading(
        &mut mods,
        &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
        tx_hash,
        seq,
    );

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

#[test]
fn offer_fill_line_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_ulp_106679738.json"));
}

/// Finding 158 (#106755558 15B9744CE01E): rw7nJtEN sells 256225 BBRL for
/// 50000 RLUSD; the direct tip is its own 20000 RLUSD bid at 5.1232 and the
/// next level its own bid at 5.1238, both inside the 5.1243 limit. rippled's
/// `execOffer` removes the first and, with nothing attempted yet, resets the
/// level quality and removes the second as well — two offers and two book
/// pages gone, nothing crossed, the rest placed. We removed the tip alone.
#[test]
fn offer_self_cross_removes_every_self_offer_inside_the_limit_106755558() {
    run_bundle(include_str!(
        "vectors/offer_self_cross_removes_every_self_offer_inside_the_limit_106755558.json"
    ));
}

/// Finding 159 (#106755996 D84E769E7583): rhhh49pF's tfIoC sells 21.88 XRP
/// for 210.047995624 CNY. The XRP/CNY pool's anchored slice (6980735 drops
/// for 67.015056) sits a hair inside the limit and the tip's partial fill
/// (14899265 drops for 143.032939624, the drop ceiling) a hair outside it;
/// together they are exactly the taker's amounts. rippled judges the
/// iteration's totals and fills; judged on the fill alone we crossed the
/// pool and left the maker's offer, line and root untouched.
#[test]
fn offer_pass_quality_is_judged_with_its_pool_slice_106755996() {
    run_bundle(include_str!("vectors/offer_pass_quality_is_judged_with_its_pool_slice_106755996.json"));
}

/// Finding 160 (#106755998 A3D3541FD33F): rURtT5MM's tfSell 0.12393 BTC →
/// RLUSD crosses one offer for 0.05880101336798101 BTC gross (rate 1.0015).
/// The residual subtracts `divideRound(gross, rate, up)` — STAmount's lossy
/// `divRound`, which truncates an 18-digit quotient to 17 before its
/// ceiling — leaving 0.06521705604794707 to rest, repriced 5274.658319744447.
/// The nearest-and-bump `mulRatio` model rested one ulp low on both sides.
#[test]
fn offer_sell_residual_uses_divround_106755998() {
    run_bundle(include_str!("vectors/offer_sell_residual_uses_divround_106755998.json"));
}

#[test]
fn offer_fill_line_recurrence_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_ulp_106679743.json"));
}

#[test]
fn offer_fill_cluster_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_ulp_106688646.json"));
}

/// F56's regression guard (#106674447 2049BE47, XAH→RLUSD tfIoC through the
/// XRP bridge, buying EXACTLY 1.0 RLUSD). The second slice's raw pool offer
/// exceeds the remaining budget, so the in-clamp marks it IN-EXHAUSTED — and
/// then the out-cap clamp binds tighter and re-derives it. With the gross
/// cap armed for every fully-funded IOU-in taker (F56), the stale flag made
/// an OUT-limited slice floor its pool mid (5253 for 5254), skip the
/// whole-drop reprice and debit the taker's WHOLE remaining 929.66 XAH as
/// the leg-A gross: six nodes off, forty ledgers of soak-98 cascade.
/// rippled sizes an out-limited iteration by `ceil_out` and its in is the
/// derived amount — the budget is not exhausted, whatever the raw offer
/// said before the limit applied.
#[test]
fn offer_fill_out_limited_bridge_under_armed_cap_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_outlimit_106674447.json"));
}

/// F62's regression guard (#106696774 12AF1556 — the 183B@108 family: the
/// XAH→RLUSD tfIoC bot through the XRP bridge, hot key 61C1E7D1, still
/// bleeding after the F45-F60 deploy). Slice 1 crosses book leg A
/// (A1A868D4) with a book leg B (8A50B0EF) whose maker holds only 0.32275
/// of its 0.47025 RLUSD. rippled's BookStep sizes an owner-FUNDS-limited
/// offer with `limitOut(…, roundUp=false)` (fixReducedOffersV1, "prevent
/// order book blocking by strictly rounding down"), so the mid-leg XRP is
/// floor(0.32275 × 348573 / 0.47025) = 239238 drops — the FFI trace's
/// `accountSendIOU … 239238/XRP`. Our ceil paid 239239, and one drop's
/// worth of XAH more on leg A: five nodes off, roots ±1 drop, both lines
/// and the rested offer.
#[test]
fn offer_fill_funds_limited_bridge_leg_floors_the_mid_leg_is_byte_exact() {
    run_bundle(include_str!("vectors/offer_bridge_funds_106696774.json"));
}

/// F67's regression guard (#106698333 314D2290): an OfferCreate buying GTA6
/// from a RequireAuth issuer (rhgbv9S3…) with a trust line the issuer has
/// not authorized. CreateOffer::preclaim's checkAcceptAsset on the TakerPays
/// side refuses with tecNO_AUTH (fee only); we crossed it. The pinned
/// target is the sender's fee-only root.
#[test]
fn offer_create_unauthorized_taker_pays_line_is_tec_no_auth() {
    run_bundle(include_str!("vectors/offer_noauth_106698333.json"));
}

/// Finding 69 — #106698812 5D50FA86: tfSell|tfIoC selling 9.08905 QUWAGI for
/// 193960 drops. The taker's QUWAGI line carries NoRipple on the ISSUER's
/// side (Flags 0x330000), so rippled's `BookStep::check` refuses the strand
/// (`toStep failed: -90` = terNO_RIPPLE), nothing crosses although the book's
/// tip is a funds-limited offer at a better rate, and the IoC is tecKILLED
/// with one mutation. We crossed the tip and reported tesSUCCESS.
#[test]
fn offer_create_issuer_side_noripple_refuses_the_book_step() {
    run_bundle(include_str!("vectors/offer_ioc_noripple_106698812.json"));
}

/// Finding 75 — #106700231 4CEDDA54: a funds-limited tfSell|tfIoC selling
/// XRP for RLUSD, filled by an AMM slice, two CLOB offers and a tail AMM
/// slice. The XRP CLOB fills were never added to the gross-spent
/// accumulator, so the tail slice's remaining cap was stale (11.69M drops
/// for a 5.18M slice); the oversized debit was dropped and the taker kept
/// 5,181,445 drops the pool had already received.
#[test]
fn offer_create_xrp_fills_feed_the_gross_cap_of_the_tail_slice() {
    run_bundle(include_str!("vectors/offer_xrp_tail_gross_106700231.json"));
}

/// Findings 76 + 78 — #106701467 DAE80780: an FLR/USD offer crossed through
/// the direct FLR/USD pool and the XRP bridge (both legs pools) with a
/// 2-drop dust tip on the XRP→USD book. rippled ranks the pool slice against
/// the tip AS FILED (76) and grosses the direct-pool slice by the in-issuer's
/// rate (78); red with either missing.
#[test]
fn offer_create_multipath_pools_rank_on_the_raw_tip_and_gross_the_direct_slice() {
    run_bundle(include_str!("vectors/offer_multipath_rawtip_fee_106701467.json"));
}

/// Findings 77 + 79 — #106701372 8BFFFACE: a passive RLUSD/BTC offer whose
/// bridge leg B is the XRP/BTC pool with no book (77), resting a remainder
/// that rippled re-derives from the offer's own rate with round-up (79):
/// 1728.598227693783 RLUSD, one ULP above the walk's net remainder.
#[test]
fn offer_create_pool_only_leg_fills_and_the_remainder_follows_the_rate() {
    run_bundle(include_str!("vectors/offer_pool_leg_remainder_106701372.json"));
}

/// Finding 79b — #106644297 2B6E3D5A: a tfSell XRP→BTC offer partially
/// crossed. Under fixReducedOffersV1 the resting out is
/// divRoundStrict(in, rate, roundUp = false): 0.007216356069340599 BTC on
/// mainnet; the first cut of finding 79 rounded it up to …600.
#[test]
fn offer_create_sell_remainder_rounds_down_under_fix_reduced_offers() {
    run_bundle(include_str!("vectors/offer_sell_remainder_106644297.json"));
}

/// #106703173 AE6C73F9 (finding 90): FLR→BTC OfferCreate against the FLR/BTC
/// pool (fee 969) where FLR carries a 1.003 transfer rate. The single-path
/// fill is sized by StrandFlow::limitOut from the strand's QualityFunction;
/// BookStep::getQualityFunc folds the in-side rate into that function (m and
/// b scaled by 1/trIn at Nearest) and the limit is the crossing's inflated
/// limitQuality (Quality{out, sendMax}). Solving the net form instead lands
/// out at 5.509588100530126e-8 for rippled's 5.509588100528479e-8 and both
/// pool lines two ulps off.
#[test]
fn offer_amm_limit_solve_folds_the_transfer_rate_into_the_quality_function() {
    run_bundle(include_str!("vectors/offer_amm_limit_qf_transfer_rate_106703173.json"));
}

/// #106702066 344657FD (finding 91): a tfSell|tfImmediateOrCancel XSPECTAR→XRP
/// crossing that runs nine flow iterations. rippled re-derives the remaining
/// input each iteration as sendMax minus the ascending 16-digit fold of every
/// saved iteration in (StrandFlow.h:724), never a running subtraction; the
/// fold budgets the ninth iteration at 7963.16183528205 where the running
/// remainder reads …206. Its last fill therefore pays …205, the maker's
/// TakerPays residual sits one ulp higher, and the taker keeps 1e-11 on its
/// line where we drained it to zero.
#[test]
fn offer_sell_remaining_input_is_the_fold_of_saved_iteration_ins() {
    run_bundle(include_str!("vectors/offer_sell_in_fold_106702066.json"));
}

/// #106708835 74C9F462 (finding 93): a tfSell|tfFillOrKill EUR→ETH crossing
/// that autobridges through XRP with a BOOK leg A and a pool leg B, and whose
/// single slice exhausts the taker's 100 EUR. rippled's forward pass is
/// in-limited — `limitStepIn` → `ceilInStrict(…, roundUp = false)` — so the
/// XRP mid-leg FLOORS for any offer type: 86946030 drops, not the ceil's
/// 86946031, and the pool then pays 0.048037377799289 ETH instead of
/// …378344132. Byte-exact on the maker's XRP offer, both trust lines and
/// both XRP balances.
#[test]
fn offer_bridged_in_limited_book_leg_floors_the_mid_leg() {
    run_bundle(include_str!("vectors/offer_bridged_in_limited_floor_106708835.json"));
}

/// #106710940 3F9DDD254714 (finding 95): a tfPassive|tfSell offer of 16.71404
/// SOL for 1250 XRP under the SOL issuer's TickSize 6. rippled rounds the
/// rate UP to the tick (74787500 drops/SOL) and re-derives TakerPays with
/// `multiply` under Number semantics: 1250001266.5 rounds half-EVEN to
/// 1250001266 drops, and the filed rate is then `divide`'s `+5` folded
/// half-even to `…22CEF8678`. Our XRP-side multiply still carried the legacy
/// `+7` form and filed 1250001267 one book level higher. Byte-exact on the
/// created Offer and its directory page.
#[test]
fn offer_tick_size_sell_rounds_taker_pays_half_even() {
    run_bundle(include_str!("vectors/offer_ticksize_halfeven_106710940.json"));
}

/// #106712068 070969D26A53 (finding 97): a tfPassive|tfSell USDC→RLUSD
/// crossing whose first fill is the DIRECT head inside a bridge attempt,
/// bound by the maker's funds (807.6986554420118 of 807.6986563115758
/// RLUSD). rippled's funds clamp is `limitOut(…, roundUp = false)`
/// (BookStep.cpp:790) — a floor at 16 digits — so the taker pays
/// 807.6978469356594 USDC; the site's unconditional ceil charged …595, and
/// the one-ulp drift reached the next maker's fill and line. Byte-exact on
/// the second maker's USDC line and its partially consumed offer.
#[test]
fn offer_bridge_direct_head_funds_bound_fill_floors_the_input() {
    run_bundle(include_str!("vectors/offer_bridge_head_funds_floor_106712068.json"));
}

/// #106714102 A1B242D893E7 (finding 99): 2.376631 USD.Bitstamp for 1759848
/// drops against the XRP/USD pool, with the order book's tip sitting between
/// the net limit and the transfer-rate-inflated one. rippled anchors the AMM
/// offer on the LOB quality, it comes out strictly better than the tip, so
/// the tip IS the AMM offer: `qualityUpperBound` waives the fee, the strand
/// is admitted, and the pool fills the whole want (in 2.372382758599013).
/// Our tail gate rested the offer whenever the tip sat in that band. Byte-
/// exact on both USD lines and both AccountRoots (taker and pool).
#[test]
fn offer_amm_admitted_when_pool_spot_beats_a_tip_inside_the_fee_band() {
    run_bundle(include_str!("vectors/offer_amm_tip_in_fee_band_106714102.json"));
}

/// #106715477 01019A7D3E59 (finding 99, revised): the same account and book
/// as #106714102 five hundred ledgers later — pool spot better than the
/// order book tip, tip inside the fee band — and mainnet RESTS the offer.
/// The anchored pool offer rippled generates (11.153 USD / 8272983 XRP,
/// 1.34815e-6) misses the taker's net limit (1.34781e-6), so the strand is
/// dropped at admission ("All strands dry"); the first specimen's anchored
/// offer cleared its limit. Admission judges the anchored offer, not the
/// spot. Byte-exact on the created Offer, both directory pages and the
/// taker's AccountRoot; the pool untouched.
#[test]
fn offer_amm_rests_when_the_anchored_pool_offer_misses_the_net_limit() {
    run_bundle(include_str!("vectors/offer_amm_anchored_misses_net_limit_106715477.json"));
}

/// #106716594 DE0753F0C77B (finding 101): the crossed book's tip is the
/// taker's OWN resting offer, one level beyond the net limit, and the
/// XRP/USD pool anchored at that level covers the whole want. rippled runs
/// tryAMM before it visits the level's offers (BookStep.cpp:837-846); the
/// pool fills everything, execOffer returns false, and the self offer is
/// never reached — it stays, OwnerCount 4. Our sweep self-reaped it before
/// the tail took the identical pool fill (OwnerCount 3, offer and page
/// gone). Byte-exact on the pool root, both USD lines and the taker's root.
#[test]
fn offer_pool_covering_the_want_leaves_the_takers_own_tip_offer_alone() {
    run_bundle(include_str!("vectors/offer_amm_fills_before_self_offer_reap_106716594.json"));
}

/// Finding 111 (#106723136 DDE9287F69AF): an OfferCreate selling 172.013 RLUSD
/// for 172.013 USDC with no flags — single-path, the direct book beyond the
/// limit, so the XRP bridge alone. Leg A (RLUSD→XRP) has a book tip inside
/// the limit AND a pool whose spot beats it: rippled's `tryAMM(tip)` runs
/// first and `getOffer(tip)` anchors a pool offer at the tip's quality, so
/// BookStep consumes the pool (301.29 RLUSD / 222.98 XRP, all of it needed)
/// and the tip's offer 8FD686B8 never moves. We priced the leg by the
/// multi-path fib slice, lost to the tip by 1.4e-4, took the tip's offer and
/// wrote the maker's nodes instead of the pool's. Three targets byte-pinned;
/// the taker's in is rippled's 171.7982923412997 RLUSD.
#[test]
fn offer_bridge_leg_takes_the_pool_anchored_at_its_book_tip() {
    run_bundle(include_str!("vectors/offer_bridge_leg_pool_anchored_at_the_tip_106723136.json"));
}

/// Finding 103 (#106720945 527C1FA56527): a tfSell|tfFillOrKill OfferCreate
/// selling 30,920,313.975492 XDC for 2497.84 XRPH. No direct book or pool;
/// the XRP bridge has an XDC/XRP pool behind a book tip and an XRP/XRPH pool
/// with no book. rippled's `tip()` judges leg A by the anchored pool offer's
/// OWN quality — Quality{amounts} of the 550,390,143.78 XDC / 466,550,412
/// drop slice that walks the spot up to the tip, 1.1797 XDC per drop against
/// a tip of 1.408 — and leg B by maxOffer's Quality{balances}, the feeless
/// spot; composed, 11,714 XDC per XRPH sits inside the 12,378 limit and one
/// single-path pass fills the whole offer (→ 3084.200478487 XRPH). We judged
/// leg A at the tip, read 13,986, and killed it. Five targets byte-pinned.
#[test]
fn offer_bridge_anchored_leg_is_judged_by_its_offers_own_quality() {
    run_bundle(include_str!("vectors/offer_bridge_anchored_leg_judged_by_its_offer_106720945.json"));
}

/// Finding 112 (#106727096 DD7DCA711B8A): a tfSell|tfImmediateOrCancel sale
/// of ONE drop for 0.000001 USD. The book's tip 5B469CA9 is a one-drop
/// remainder (TakerPays 1 drop, TakerGets 0.0000013709342 USD) whose
/// quality, recomputed from what is left, rounds WORSE than the directory it
/// sits in: rippled's OfferStream::step removes it unexecuted
/// (shouldRmSmallIncreasedQOffer — fixRmSmallIncreasedQOffers) and crosses
/// the next level, BD4970D4, for the drop (2.380987 → 2.380985629809433 USD).
/// We crossed the remainder itself and left BD4970D4 alone. Seven targets
/// byte-pinned, BD4970D4 among them (created earlier in the same ledger).
#[test]
fn offer_dust_remainder_whose_quality_rounds_worse_is_removed_not_crossed() {
    run_bundle(include_str!("vectors/offer_dust_xrp_offer_increased_quality_removed_106727096.json"));
}


/// Finding 114 (#106730661 3241437EB11D): a tfImmediateOrCancel buy of
/// 20.575959 XRP for 28.215814 RLUSD by a taker holding 0.0000206 RLUSD.
/// rippled's REV pass sizes the book against the WANT before the DirectStep
/// limits the sender to their funds: it consumes the 2.456-XRP tip 2B30E6F4
/// whole, and the `do … while (offers.step())` loop then steps the stream —
/// removing the expired 729F17CE and 392EFC07 on the next level — until the
/// live offer behind them ends the pass (a worse level). The fwd pass, limited
/// to the funds, is then "rejected by limitQuality": nothing crosses,
/// tecKILLED, and the two expired offers are gone with their page — "rm bad
/// offers even if the strand fails", applied through sbCancel. Our walk sized
/// every fill by min(want, funds) from the first offer and never left the tip.
/// Three targets byte-pinned: the reaped makers' owner directory and root and
/// the taker's root; the page …4F04DF824B0BC6DD was created and removed
/// earlier in the same ledger, so its pre-image is seated by hand.
#[test]
fn offer_killed_crossing_still_reaps_what_the_rev_pass_stepped_over() {
    run_bundle(include_str!("vectors/offer_killed_crossing_reaps_the_rev_pass_step_106730661.json"));
}

/// Finding 115 (#106731793 E3E8726FA355): the F114 rule's second face. A
/// tfSell|tfImmediateOrCancel sale of 1.5 XRP for XRPCAT. rippled's rev pass
/// takes rfD9A7M8's 01EA78BD whole — its entire 1.9M XRPCAT — reaps the
/// unfunded 1F26E638 beside it, and steps to the next level, where the same
/// maker's 9EABC3F5 is now worth 0.0003525 XRPCAT: `ceilOutStrict` clips its
/// XRP in to ZERO drops, `shouldRmSmallIncreasedQOffer` says remove, and
/// because the pass itself shrank the funds it is "became tiny" — stepped
/// past, not removed — which is how the stream reaches and reaps the unfunded
/// 8BB6A1F0 behind it. The funded fwd pass then takes 653713 XRPCAT of
/// 01EA78BD and 9EABC3F5 rests untouched. Two ports: the rev scan tracks what
/// its whole consumptions took from each maker, and the tiny-offer test
/// floors a funds-clipped XRP in to whole drops. Six targets byte-pinned,
/// both book pages among them — the reaps live in their entry lists.
#[test]
fn offer_rev_pass_steps_past_a_maker_it_emptied_and_reaps_behind_it() {
    run_bundle(include_str!("vectors/offer_rev_pass_steps_past_a_became_tiny_offer_106731793.json"));
}

/// Finding 116 (#106732842 7BD990E595DF): a buy of 19.718743 "666" for
/// 5.783103 XRP against a maker whose page TIES the taker's limit at 16
/// digits (2932794955540523e-10) while its own 5254632/17.91680659458702
/// encodes one ULP worse. rippled judges the limit by the level —
/// `checkQualityThreshold(offer.quality())` with the directory's quality —
/// and the maker is underfunded (12.709 of 17.917), so the funds-limited
/// fill floors to 3727433 drops, realises a rate inside the limit, and
/// mainnet crosses it: the maker's line drains to zero, its offer and owner
/// page are deleted, and the taker rests 7.00924955970754 for 2.055670 XRP.
/// Our own-rate pre-screen skipped the maker and rested the taker's full
/// offer. The pre-screen's own specimen, #105787531 BB6660FA (same tie, fully
/// funded), is decided by the achieved-quality judge instead: a whole-offer
/// fill realises exactly the own rate, one ULP over, and the pass is
/// rejected. Eight targets byte-pinned, both owner pages among them.
#[test]
fn offer_tie_level_is_judged_by_the_directory_quality_not_the_offers_own_rate() {
    run_bundle(include_str!("vectors/offer_tie_level_judged_by_directory_quality_106732842.json"));
}

/// Finding 117 (#106733197 DFD1E132035C): a tfImmediateOrCancel buy of
/// 3111.618064 XRP for RLUSD takes three underfunded bot makers whole on
/// three levels (0.8, 0.6 and 0.4 XRP) and rests nothing (the next live
/// level and the pool both miss the limit). rippled's stream deletes what it
/// steps past: the dead offers on the levels behind each fill, and — in the
/// forward pass of a successful iteration — the same makers' remaining
/// offers, now "became unfunded" (D8118C65, 119270E3), page by page. Our
/// rev-extent scan (F114) had already reaped the dead offers ahead of the
/// funded walk and emptied their levels; when the walk then reached the
/// first such level its page was gone, `reap_to_live_head` reported an
/// unreadable page as "live", and the stepping ended two levels short of
/// the two remaining offers. A page this apply has deleted is an emptied
/// level, not an unknown one. Eleven targets byte-pinned: the makers' roots
/// and owner pages, the taker's, and the book pages.
#[test]
fn offer_walk_steps_through_a_level_it_emptied_and_reaps_the_makers_remaining_offers() {
    run_bundle(include_str!("vectors/offer_emptied_level_page_is_stepped_through_106733197.json"));
}

/// Finding 118 (#106733048 E4014705): a tfSell of 8267788.778091 CSC for LHT
/// takes 109292.8270744682 CSC at the first level and the remainder at a 1:1
/// maker. rippled's `remainingIn = sendMax − sum(savedIns)` is IOUAmount
/// arithmetic — sixteen significant digits, nearest — so the remainder is
/// 8158495.951016532 and the 1:1 fill delivers exactly that. Our exact
/// 17-digit remainder (…5318) fed the derivation and the floor to sixteen
/// digits gave …6531: the taker's LHT line one ULP short. Ten targets
/// byte-pinned.
#[test]
fn offer_remaining_in_budget_is_an_stamount() {
    run_bundle(include_str!("vectors/offer_remaining_in_is_an_stamount_106733048.json"));
}

/// Finding 118, the rated face (#106732165 185A4B50): a tfSell|tfIoC of 200
/// EVR (issuer rate 1.002, sendMax grossed to 200.4) takes 0.2538819407514244
/// gross at a tiny first level; the remainder rippled carries is the
/// STAmount 200.1461180592486, and the maker's net is that over 1.002 —
/// 199.7466248096293 — leaving its TakerPays at 332.7392751903707. Our exact
/// remainder …485756 netted to …292 and left …708. Eight targets byte-pinned,
/// the residual offer among them.
#[test]
fn offer_remaining_in_budget_is_an_stamount_under_a_spend_rate() {
    run_bundle(include_str!("vectors/offer_remaining_in_is_an_stamount_rated_106732165.json"));
}

/// Finding 120 (#106734370 895B4F980691): a buy of 0.00389351611 BTC for
/// 220.3 XRP against the BTC/XRP pool (fee 19) with the CLOB tip 4A06477A one
/// hair past the taker's net limit 4A064769 and inside the transfer-rate
/// inflated one 4A0649D3. rippled's `tryAMM` anchors on that tip — its
/// `BookOfferCrossingStep::qualityThreshold(lobQuality)` unanchors the pool
/// (max offer) only when the tip is WORSE than `qualityThreshold_`, the
/// inflated limit — takes the tip-anchored slice, 99.673705 XRP for
/// 0.001760274370140327 BTC, finds "higher clob quality" on the next pass,
/// has the tip's own fill "rejected by limitQuality", and rests 120.626295 XRP
/// for 0.0021319129499415 BTC. Our direct-tail turn anchored only on a self
/// offer, ran the unanchored max offer, took 175.917309 XRP and rested
/// 44.382691. Five targets byte-pinned: the resting offer, both BTC lines, the
/// taker's root and the pool's XRP root.
#[test]
fn offer_tail_pool_turn_anchors_on_the_residual_tip_inside_the_inflated_limit() {
    run_bundle(include_str!("vectors/offer_tail_pool_anchored_on_the_residual_tip_106734370.json"));
}

/// Finding 121 (#106733288 9954A265, recurring #106734683 208D914F): a
/// tfSell of 20000 RLUSD for 101896 BBRL by a bot whose PREVIOUS offer still
/// rests at the RLUSD/BBRL book's tip (0.19612 RLUSD per BBRL, better than
/// this order's 0.19628 limit). rippled anchors `tryAMM` on that raw tip —
/// the taker's own offer included — and the tiny RLUSD/BBRL pool
/// (2.5007/12.874, fee 1000) cannot be moved there
/// (getAMMOfferStartWithTakerPays: pool.out·rate − pool.in/f < 0,
/// "changeSpotPrice calc failed"); `execOffer` then steps onto the self tip
/// and removes it ("even if no crossing occurs"), the pass consumed nothing,
/// the strand is dry, the bridged strand's bound misses the limit: nothing
/// crosses, the old offer is gone, the new one rests in full. Ours skipped the
/// self tip when peeking, anchored the pool on the LIMIT instead, sold
/// 0.005170288597693149 BBRL for 0.001014816793143435 RLUSD, kept the old
/// offer and rested the remainder. Two targets byte-pinned.
#[test]
fn offer_bridged_pool_anchors_on_the_takers_own_tip_and_the_dry_pass_removes_it() {
    run_bundle(include_str!("vectors/offer_bridged_pool_anchored_on_the_takers_own_tip_106733288.json"));
}

/// Finding 122 (#106734972 E7CB46461F09): a tfSell|tfImmediateOrCancel of
/// the taker's whole 7080.694618635 RLUSD balance for XRP that rippled fills
/// across 39 levels. The last level is funds-limited, and the figure it takes
/// is the LINE's remaining balance — rippled's DirectStep limits the iteration
/// to `maxSrcToDst`, the balance as the ledger carries it after every earlier
/// iteration's 16-digit debit — 653.9861023930549 after thirty-eight
/// half-even debits, where the budget's folded remainder (`sendMax −
/// sum(savedIns)`) says …550. Ours sized the fill from the folded remainder
/// and left two lines one ULP off. Every earlier iteration matches to the
/// digit. Sixty-seven targets byte-pinned.
#[test]
fn offer_funds_exhausting_fill_takes_the_lines_own_remaining_balance() {
    run_bundle(include_str!("vectors/offer_funds_exhausting_fill_takes_the_line_balance_106734972.json"));
}

/// Finding 122, second specimen (#106734622 C1183E11AEC3): a
/// tfImmediateOrCancel sale of RLUSD for 25.446614 XRP that runs the taker's
/// line dry on its fifth fill. The folded remainder carries a sixteenth
/// digit — 11048.84189960121 — that the line, debited level by level, does
/// not: 11048.8418996012. Mainnet takes the line's figure; one line was one
/// ULP off in ours. Eleven targets byte-pinned.
#[test]
fn offer_funds_exhausting_fill_takes_the_lines_own_remaining_balance_second_specimen() {
    run_bundle(include_str!("vectors/offer_funds_exhausting_fill_takes_the_line_balance_106734622.json"));
}

/// Finding 123 (#106734683 208D914F651B): the F121 bot buying back —
/// 50000 RLUSD for 256055 BBRL, no flags. Four bridged fib rounds match
/// rippled's iterations 0–3 to the drop (4812, 4812, 9624, 14436 through the
/// BBRL/XRP pool and rpiFwLYi's XRP→RLUSD offer). At iteration 4 the bridge's
/// next slice misses the limit, `activateNext` keeps only the direct strand,
/// `multiPath` is false, and the direct RLUSD/BBRL pool's offer is the
/// single-path, limit-sized one — 0.003821352718250826 BBRL for
/// 0.000746416288641 RLUSD — which mainnet takes before resting
/// 49999.95236579247. Our round carried the previous iteration's multi-path
/// verdict, sized a fib slice instead, refused it as beyond the limit and
/// rested 49999.95311220876. Thirteen targets byte-pinned.
#[test]
fn offer_bridged_round_takes_rippleds_current_activation_for_its_multipath_flag() {
    run_bundle(include_str!("vectors/offer_bridged_multipath_follows_the_current_activation_106734683.json"));
}

/// Finding 122, third specimen (#106734208 FDD69EE819BC): a
/// tfSell|tfImmediateOrCancel of 10000 RLUSD by a taker holding more than
/// that, filled across 64 levels — the BUDGET binds, not the line. rippled's
/// last fill takes `sendMax − sum(savedIns)`, the sorted fold: 2066.665195811112.
/// Our running gross-spend chain said …110 and overrode the fold the walk
/// already carried; the residual maker offer 043B556F was two ULPs off.
/// Every earlier iteration matches to the digit. One hundred and thirty-one
/// targets byte-pinned.
#[test]
fn offer_budget_bound_exhausting_fill_takes_the_folded_remainder() {
    run_bundle(include_str!("vectors/offer_budget_bound_fill_takes_the_folded_remainder_106734208.json"));
}

/// Finding 124 (#106735594 982E2CDBD610): a partial payment of 605278 drops
/// for 2.065196669826816 "666" against rJ4EpEPT's offer, funded for
/// 2.06519666982682 of its 20.021509. The fill is out-limited to the want,
/// and rippled's `ceilOutImpl` clamps the priced input to the OFFER's input —
/// which, for an underfunded maker, is the funds-limited input rounded DOWN
/// (`limitOut(…, roundUp=false)` at the funds clamp): the want's ceiling is
/// 605278 drops, the funds' floor 605277, and mainnet takes 605277. Ours
/// clamped only to the full wants and took 605278 — the maker's root and
/// residual offer one drop off. Three targets byte-pinned.
#[test]
fn offer_out_limited_input_is_clamped_to_the_underfunded_makers_own_input() {
    run_bundle(include_str!("vectors/offer_out_limited_input_clamped_to_the_funds_limited_input_106735594.json"));
}

/// Finding 125 (#106735332 F24954BB): an ImmediateOrCancel buy of 1 RLUSD for
/// up to 1000 XAH over two strands. Iteration 0 takes 4D9233FD's whole
/// 35.93848027814264 XAH for 0.4957959216879002 on the direct book — both
/// engines agree. rippled's iteration 1 ranks the strands on
/// `qualityUpperBound`, the first directory page still holding an entry: the
/// direct book's live tip is DE6AD7AC at 150 XAH/RLUSD, the pool's fib slice
/// 73.16, the bridge 72.502 — the bridge goes first and fills the remaining
/// 0.5042040783120998 for 36.5559691 XAH. Our ladder cursor still named the
/// emptied 72.486 level, ranked the direct strand first, and crossed the 150
/// offer for 75.63061250312109 XAH. Seventeen targets byte-pinned.
#[test]
fn offer_direct_upper_bound_reads_the_live_tip_not_the_emptied_level() {
    run_bundle(include_str!("vectors/offer_direct_upper_bound_reads_the_live_tip_106735332.json"));
}

/// Finding 127 (#106736673 9C615823): a 3.526798 XRP buy against rB18cdac's
/// ask of 899635.0815887755 for 89963508158877567 drops — seventeen digits
/// of drops. rippled's native STAmount subtracts the consumed 3526798 drops
/// exactly and rests the ask at 89963508155350769; our residual rounded the
/// difference back to sixteen significant digits and read …770. One target
/// byte-pinned.
#[test]
fn offer_makers_xrp_residual_is_subtracted_in_exact_drops() {
    run_bundle(include_str!("vectors/offer_xrp_residual_is_exact_drops_106736673.json"));
}

/// Finding 127, second specimen (#106736674 939B1191, the next ledger): the
/// same ask filled for 1013201 drops rests at 89963508154337568 on mainnet;
/// the sixteen-digit rounding read …570.
#[test]
fn offer_makers_xrp_residual_is_subtracted_in_exact_drops_second_specimen() {
    run_bundle(include_str!("vectors/offer_xrp_residual_is_exact_drops_second_specimen_106736674.json"));
}

/// Finding 128 (#106735983 92E1A551): a tfPassive buy of 0.02379 BTC for
/// 1910.26219 RLUSD over the XRP bridge. rippled's `limitOut` composes the
/// strand's quality function from `tip()`, which takes the pool offer only
/// when the offer it would generate beats the book's tip; leg B's unbounded
/// maxOffer (9934678820119 drops for 1.781794014448948 BTC) does not, so the
/// function is constant, the pass runs for the whole 0.02379, the pool
/// executes first and the pass misses the limit: "All strands dry", the
/// offer rests whole. We composed the pool's curve, limited the output to
/// 0.0001645576049539761 BTC and filled at the limit. Twelve targets
/// byte-pinned.
#[test]
fn offer_bridged_limit_out_composes_the_tips_offer_not_the_forced_pool() {
    run_bundle(include_str!("vectors/offer_bridged_limit_out_composes_the_tip_106735983.json"));
}

/// Finding 128, second specimen (#106735988 ED391EC4, the same bot five
/// ledgers on): 1910.61699 RLUSD for 0.02379 BTC, mainnet rests it whole
/// after the unlimited pass delivers 0.023486064930029 and is rejected;
/// we filled 0.0004987469251640261 BTC at the limit.
#[test]
fn offer_bridged_limit_out_composes_the_tips_offer_not_the_forced_pool_second_specimen() {
    run_bundle(include_str!("vectors/offer_bridged_limit_out_composes_the_tip_second_specimen_106735988.json"));
}

/// Finding 133 (#106737559 6001F5CA): a tfSell|IoC of 3494.2683829543 RLUSD
/// for XRP crossing eleven iterations — seven book levels and four anchored
/// pool slices. rippled's DirectStep caps the source by
/// `PaymentSandbox::balanceHookIOU`: the lesser of the line as carried
/// (195.5222760261612 after ten sixteen-digit debits) and the original
/// balance less the deferred-credits table, a chronological sum of the ten
/// debits (3298.74610692814) — 195.52227602616. The sorted budget fold said
/// 195.522276026161 and the maker rURtT5MM's residual landed one unit high.
/// Eighteen targets byte-pinned.
#[test]
fn offer_taker_spend_is_capped_by_the_deferred_credits_balance() {
    run_bundle(include_str!("vectors/offer_taker_spend_capped_by_deferred_credits_106737559.json"));
}

/// Finding 135 — #106738995 C171D5D57C05 (rn3S6j5b, 44.73 XRP for 5M PHNIX,
/// prior balance 1.602638 XRP, OwnerCount 3). One offer crossed; rippled's
/// reserve gate (OfferCreate.cpp:835-851) compares `preFeeBalance_` against the
/// reserve for one more object and, below it, rests nothing: tesSUCCESS with the
/// fill kept. We placed the residual (extra Offer, two directory pages,
/// OwnerCount +1) because the gate ran only when nothing crossed.
#[test]
fn offer_residual_is_not_placed_below_the_pre_fee_reserve() {
    run_bundle(include_str!("vectors/offer_residual_not_placed_below_pre_fee_reserve_106738995.json"));
}

/// Findings 137 + 138 — #106739364 B2E8A289F84F (rnCEEqDn, 1 RLUSD for XAH,
/// IoC; the recurring key). `BookStep::forEachOffer` consumes the whole
/// same-quality directory group in one pass (iteration 1 takes three
/// 68118-drop offers), so mainnet finishes in five iterations without the
/// leg-A pool's Fibonacci slice ever outgrowing the CLOB maker; and a
/// multi-path pool slice prices its partial by the 16-digit `getRate` quality
/// (`ceilOutStrict`), not the exact in/out ratio.
#[test]
fn offer_bridged_pass_takes_the_same_quality_group() {
    run_bundle(include_str!("vectors/offer_bridged_pass_takes_same_quality_group_106739364.json"));
}

/// Same rule, the next firing 15 ledgers on — #106739379 99186CFE20EC.
#[test]
fn offer_bridged_pass_takes_the_same_quality_group_second_specimen() {
    run_bundle(include_str!("vectors/offer_bridged_pass_takes_same_quality_group_second_specimen_106739379.json"));
}

/// Finding 139 — #106738901 B7B46440CED2 (rMsXVzCug7, tfPassive ETH → RLUSD).
/// rippled's one limitQuality is fee-inclusive (`Quality{takerAmount.out,
/// sendMax}`) and the strands' RAW upper bounds are held against it: the
/// direct book's tip is the taker's own 7B23C16BD7DF at 3.997041e-4, inside
/// the 3.9975457e-4 limit, so two strands stay active (three Fibonacci pool
/// slices on the bridge). The direct strand never runs — the bridge wins every
/// iteration — so the taker's three self-offers survive.
#[test]
fn offer_passive_bridge_keeps_the_direct_strand_active_and_its_self_offers() {
    run_bundle(include_str!("vectors/offer_passive_bridge_keeps_direct_strand_and_self_offers_106738901.json"));
}

/// Finding 141 — #106740670 BEB587B55F6D (rMsXVzCug7, tfPassive, 1907.04056
/// RLUSD for 0.02338 BTC, bridged RLUSD → XRP → BTC). Leg B's book tip is
/// worse than the strand's limit, so the XRP/BTC pool is unanchored
/// (`maxOffer`), whose quality is the pool's SPOT (`Quality{balances}`): the
/// pool tips the leg, the strand's quality function is curve-shaped, and
/// `limitOut` sizes the single pass to exactly the limit quality — 115.85 RLUSD
/// for 0.00142027347946324 BTC, 1791.19 RLUSD resting. We compared the slice's
/// average, priced the leg as a fixed-rate book and rested the whole offer.
#[test]
fn offer_unanchored_pool_tips_by_spot_and_the_pass_is_limit_sized() {
    run_bundle(include_str!("vectors/offer_unanchored_pool_tips_by_spot_and_limits_the_pass_106740670.json"));
}

/// Finding 142 — #106742048 7AFAC4E00D8F (r3rhWeE31Jt5, 19.73 USDC for
/// 0.000243994516152997 BTC). `activateNext` runs before the first iteration
/// too: the bridge's upper bound misses the limit, one strand survives,
/// `multiPath` is false from the start, and the direct USDC/BTC pool answers
/// with the single-path, limit-sized slice (0.0201 USDC → 2.48e-7 BTC at
/// exactly the limit). We gave round 0 the built-strand entry, asked for a
/// Fibonacci slice, refused it and rested the whole offer.
#[test]
fn offer_round_zero_multipath_follows_strand_activation() {
    run_bundle(include_str!("vectors/offer_round_zero_multipath_follows_activation_106742048.json"));
}

/// Finding 145 — #106742494 3E0D8EB82426 (rTeLeproT3, tfSell 15,000 XAH for
/// 211.65 RLUSD). Iterations 0–5 match; at 6 the direct strand's bound — the
/// pool's 13x Fibonacci slice, composed with the previous round's multiPath —
/// misses the limit, `activateNext` drops it, and the bridge's anchored pass
/// sells the remaining 3815.39521136895 XAH through both pools. We admitted
/// the direct pool on its single-path spot bound and took two limit-sized
/// slices first.
#[test]
fn offer_direct_pool_is_consulted_only_if_its_strand_survives_activation() {
    run_bundle(include_str!("vectors/offer_direct_pool_consulted_only_if_strand_survives_activation_106742494.json"));
}

/// Finding 144 — #106742463 1E7ED524888A (rM2gGZdJ, tfSell 2,001,000 XRPS for
/// 3613.806 CNY through both pools, eight iterations). rippled's residual is
/// `takerAmount.in - actualAmountIn`, the latter the STAmount SUM of the
/// iterations' ins (each `+=` canonicalized): 1800041.456967373 → a residual
/// of 200958.543032627. Our running subtraction left 200958.5430326276.
#[test]
fn offer_residual_is_the_original_minus_the_summed_ins() {
    run_bundle(include_str!("vectors/offer_residual_is_original_minus_the_summed_ins_106742463.json"));
}

// Finding 149 — #106743984 12111F1FB0FA (rwKpr5aUr3bV, tfSell|FoK, 1170 XRP
// → RLUSD): a level whose every offer is consumed in full is a LIMITING pass,
// and rippled re-runs it in reverse with the level's own 16-digit total as
// the target; the re-run trims the last maker to `total16 − Σprev`
// (724.9999999999999 for 5FD7's whole 500,000,000 drops, its line
// 745.5000000000001) — the input capped at the offer's own (`ceilOutImpl`).
#[test]
fn offer_limiting_level_rerun_reprices_the_last_maker() {
    run_bundle(include_str!("vectors/offer_limiting_level_rerun_reprices_the_last_maker_106743984.json"));
}

// Finding 143 — #106746247 9C42CD0DCFF7 (rMsXVzCug7, an ETH ask at 2506.88
// RLUSD/ETH placed right after its own bid at 2499.37 in the same ledger):
// the bid sits at the bids-book tip, outside the ask's limit. rippled's
// `limitSelfCrossQuality` removes a self-offer only inside the limit and the
// pass ends at one beyond it; we deleted the bid and its page. The bundle
// seats every Offer/DirectoryNode the account's earlier transactions in the
// ledger created or touched, which is what puts the bid on the tip.
#[test]
fn offer_direct_pass_stops_at_a_self_offer_beyond_the_limit() {
    run_bundle(include_str!("vectors/offer_direct_pass_stops_at_a_self_offer_beyond_the_limit_106746247.json"));
}

// Finding 153 — #106746952 AC39E946C478 (rwHSyWL5Yd, tfSell|FoK XRPH → XRPHAI,
// tecKILLED): round one consumes rhVbJJS1's 51E6 — its whole XRPHAI balance
// — and the trailing stream step lands on the same owner's ACB8. rippled's
// OfferStream sees the owner funded at the iteration's start and unfunded
// now ("became unfunded"): stepped past, but never a permanent removal, so
// the killed transaction leaves it — with its page, owner directory and
// root — exactly as it was. We reaped it for good. Four untouched pins.
#[test]
fn offer_became_unfunded_offer_survives_a_killed_crossing() {
    run_bundle(include_str!("vectors/offer_became_unfunded_offer_survives_a_killed_crossing_106746952.json"));
}

/// Finding 163 (#106771950 B1283A3C1288): rBTwLga3i2 buys 1M XRP with
/// 1,380,000.5 USD.Bitstamp (limit 1.3800005e-6 USD/drop). The tip is a
/// 2208-drop dust offer FILED at 1.3800013e-6, beyond the strict limit but
/// inside the 1.0015-inflated one, whose own amounts price at 1.3795e-6.
/// rippled attempts it, consumes it whole at its amounts, and the strand
/// check passes; we ended the walk at the level and crossed nothing.
#[test]
fn offer_live_tip_beyond_strict_inside_inflated_is_attempted_106771950() {
    run_bundle(include_str!(
        "vectors/offer_live_tip_beyond_strict_inside_inflated_is_attempted_106771950.json"
    ));
}

#[test]
fn offer_ioc_sell_in_capped_fill_keeps_the_limit_forgiveness_106759265() {
    run_bundle(include_str!("vectors/offer_ioc_sell_in_capped_fill_keeps_the_limit_forgiveness_106759265.json"));
}

#[test]
fn offer_dead_book_tip_inside_the_limit_pins_a_constant_quality_function_106774713() {
    run_bundle(include_str!("vectors/offer_dead_book_tip_inside_the_limit_pins_a_constant_quality_function_106774713.json"));
}

#[test]
fn offer_pool_alone_survives_a_book_tip_beyond_the_limit_106766924() {
    run_bundle(include_str!("vectors/offer_pool_alone_survives_a_book_tip_beyond_the_limit_106766924.json"));
}

#[test]
fn offer_issuer_owned_maker_still_grosses_the_taker_spend_106758320() {
    run_bundle(include_str!("vectors/offer_issuer_owned_maker_still_grosses_the_taker_spend_106758320.json"));
}

#[test]
fn offer_sell_past_its_minimum_rests_the_unsold_remainder_106758110() {
    run_bundle(include_str!("vectors/offer_sell_past_its_minimum_rests_the_unsold_remainder_106758110.json"));
}

#[test]
fn offer_crossing_remaining_out_folds_to_sixteen_digits_106769838() {
    run_bundle(include_str!("vectors/offer_crossing_remaining_out_folds_to_sixteen_digits_106769838.json"));
}

#[test]
fn offer_bridged_leg_out_remainder_folds_to_sixteen_digits_106781871() {
    run_bundle(include_str!("vectors/offer_bridged_leg_out_remainder_folds_to_sixteen_digits_106781871.json"));
}
