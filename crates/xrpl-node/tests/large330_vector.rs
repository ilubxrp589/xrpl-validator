//! Number's 19-digit Large330 scale on the AMM, offer and payment paths: bundles whose expected result and objects
//! are what libxrpl (rippled 3.4.1, through the FFI shim) computed with SingleAssetVault, LendingProtocol and
//! LendingProtocolV1_1 added to mainnet's enabled amendments, i.e. the rules of the day either vault amendment
//! activates, when `setCurrentTransactionRules` (Rules.cpp:35-74) switches every transaction's IOU arithmetic from
//! sixteen digits to nineteen.
//!
//! There is no network outcome for these states: the expectation is libxrpl's, pinned by
//! `libxrpl_large330_differential.rs` (`L330_PIN_DIR`) on 2026-11-22 from bundles on which libxrpl and this engine
//! agreed byte for byte. Each file is the bundle as it ran: the pre-state with the rewritten Amendments object, the
//! transaction, libxrpl's result and every object it created, changed or deleted. Sources:
//!
//! - pinned mainnet / testnet vectors (the transaction as the network carried it, its signature intact);
//! - `__W<i>` / `__WT<i>` mutants: the same bundle with every trust-line balance, offer and pool multiplied by a
//!   factor (and, for WT, the transaction's own amounts: those ran unsigned under tapDRY_RUN);
//! - `__T<i>` / `__TICK<n>` targeted mutants: an AMMVote weight half a unit from a tie, NFT royalties whose product
//!   rounds differently through nineteen digits, offers placed under an issuer TickSize;
//! - `dev_*`: organic devnet transactions (rippled 3.5.0-rc5), their rules replaced as above.
//!
//! `scale_sensitive` in a file says libxrpl's answer at Large330 differs from its answer at the Small scale on the
//! same state: those are the vectors that hold the nineteen-digit rounding itself. The swapAssetOut specimens
//! (`pool.out - assetOut` is an amount: sixteen digits before the division) differed from libxrpl before 61cf884.
//!
//! Pass criteria: libxrpl's result code; every object libxrpl wrote, byte for byte (deleted where it deleted); and
//! no other write, except an object left equal to its pre-image apart from its threading fields.
use serde_json::Value;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

fn key32(hex_key: &str) -> Hash256 {
    Hash256(<[u8; 32]>::try_from(hex::decode(hex_key).unwrap().as_slice()).unwrap())
}

fn encoded(bytes: &[u8]) -> String {
    let mut jv: Value = serde_json::from_slice(bytes).unwrap();
    canon_for_encode(&mut jv);
    hex::encode_upper(xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap())
}

/// The object as JSON with its threading stamps set aside.
fn unthreaded(hex_bytes: &str) -> Value {
    let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&hex::decode(hex_bytes.trim()).unwrap()).unwrap();
    if let Some(o) = jv.as_object_mut() {
        o.remove("PreviousTxnID");
        o.remove("PreviousTxnLgrSeq");
    }
    jv
}

fn run(bundle_json: &str) {
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
        close_time_resolution: bundle["close_time_resolution"].as_u64().unwrap_or(10) as u8,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    let pre = bundle["pre"].as_object().unwrap();
    for (k, v) in pre {
        let bytes = hex::decode(v.as_str().unwrap().trim()).unwrap();
        let mut jv = xrpl_core::codec::decode::decode_transaction_binary(&bytes).unwrap();
        hexify_addresses(&mut jv);
        state.state_map.insert(key32(k), serde_json::to_vec(&jv).unwrap()).unwrap();
    }
    let txf = build_txfields(&bundle["tx"]).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    assert_eq!(ter, bundle["result"].as_str().unwrap(), "libxrpl's result at Large330");
    let seated = |k: &Hash256| state.state_map.lookup(k).map(|b| b.to_vec());
    xrpl_ledger::ledger::threading::stamp_threading(&mut mods, &seated, bundle["tx"]["hash"].as_str().unwrap(), seq);

    let expect = bundle["expect"].as_object().unwrap();
    let mut differences = Vec::new();
    for (k, want) in expect {
        let want = want.as_str().unwrap().trim().to_uppercase();
        match mods.get(&key32(k)) {
            None => differences.push(format!("{k}: libxrpl wrote it, the engine did not")),
            Some(SandboxEntry::Deleted) => {
                if !want.is_empty() {
                    differences.push(format!("{k}: the engine deleted it, libxrpl kept it"));
                }
            }
            Some(SandboxEntry::Created(b)) | Some(SandboxEntry::Modified(b)) => {
                if want.is_empty() {
                    differences.push(format!("{k}: libxrpl deleted it, the engine kept it"));
                } else if encoded(b) != want {
                    let ours: Value = serde_json::from_slice(b).unwrap();
                    let mut theirs = xrpl_core::codec::decode::decode_transaction_binary(&hex::decode(&want).unwrap()).unwrap();
                    hexify_addresses(&mut theirs);
                    differences.push(format!("{k}: bytes differ\n  ours    {ours}\n  libxrpl {theirs}"));
                }
            }
        }
    }
    // Nothing beyond what libxrpl wrote, but for an object the engine rewrote unchanged.
    for (k, entry) in &mods {
        let key = hex::encode_upper(k.0);
        if expect.contains_key(&key) {
            continue;
        }
        let unchanged = match entry {
            SandboxEntry::Modified(b) => pre.get(&key).and_then(Value::as_str).is_some_and(|p| unthreaded(p) == unthreaded(&encoded(b))),
            _ => false,
        };
        if !unchanged {
            differences.push(format!("{key}: the engine wrote it, libxrpl did not"));
        }
    }
    assert!(differences.is_empty(), "{} difference(s) from libxrpl at Large330:\n{}", differences.len(), differences.join("\n"));
}

/// AMMCreate: devnet organic; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn dev_ammcreate_23da605a4d2f_devnet_6898462() {
    run(include_str!("vectors/large330/dev_ammcreate_23da605a4d2f_devnet_6898462.json"));
}

/// AMMCreate: devnet organic; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn dev_ammcreate_43441b16d15a_devnet_7155647() {
    run(include_str!("vectors/large330/dev_ammcreate_43441b16d15a_devnet_7155647.json"));
}

/// AMMDeposit: devnet organic; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn dev_ammdeposit_a9ae08dc41f8_devnet_6895857() {
    run(include_str!("vectors/large330/dev_ammdeposit_a9ae08dc41f8_devnet_6895857.json"));
}

/// AMMDeposit: devnet organic; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn dev_ammdeposit_e7514b8263df_devnet_6895862() {
    run(include_str!("vectors/large330/dev_ammdeposit_e7514b8263df_devnet_6895862.json"));
}

/// Payment: payment enlarged (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_first_hop_trin_in_the_quality_function_106732759_fib3() {
    run(include_str!("vectors/large330/payment_first_hop_trin_in_the_quality_function_106732759__FIB3.json"));
}

/// Payment: payment enlarged (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_first_hop_trin_in_the_quality_function_106732759_fib30() {
    run(include_str!("vectors/large330/payment_first_hop_trin_in_the_quality_function_106732759__FIB30.json"));
}

/// Payment: payment enlarged (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_106734485_fib3() {
    run(include_str!("vectors/large330/payment_forward_pass_reswaps_the_downstream_pool_106734485__FIB3.json"));
}

/// Payment: payment enlarged (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_106734485_fib30() {
    run(include_str!("vectors/large330/payment_forward_pass_reswaps_the_downstream_pool_106734485__FIB30.json"));
}

/// Payment: payment enlarged (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_inflight_line_cleanup_keeps_dir_entries_106730304_fib3() {
    run(include_str!("vectors/large330/payment_inflight_line_cleanup_keeps_dir_entries_106730304__FIB3.json"));
}

/// Payment: payment enlarged (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_inflight_line_cleanup_keeps_dir_entries_106730304_fib30() {
    run(include_str!("vectors/large330/payment_inflight_line_cleanup_keeps_dir_entries_106730304__FIB30.json"));
}

/// AMMBid: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570_w0() {
    run(include_str!("vectors/large330/c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570__W0.json"));
}

/// AMMBid: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570_w1() {
    run(include_str!("vectors/large330/c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570__W1.json"));
}

/// AMMClawback: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn amm_clawback_partial_by_amount_testnet_20936598_w3() {
    run(include_str!("vectors/large330/amm_clawback_partial_by_amount_testnet_20936598__W3.json"));
}

/// AMMDeposit: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225_w0() {
    run(include_str!("vectors/large330/c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225__W0.json"));
}

/// AMMDeposit: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976_w2() {
    run(include_str!("vectors/large330/c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976__W2.json"));
}

/// AMMDeposit: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976_w3() {
    run(include_str!("vectors/large330/c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976__W3.json"));
}

/// AMMDeposit: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976_w4() {
    run(include_str!("vectors/large330/c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976__W4.json"));
}

/// AMMDeposit: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_8_h2_tfsingleasset_0_2_xrp_into_pc_testnet_21249001_w3() {
    run(include_str!("vectors/large330/c26a_5_8_h2_tfsingleasset_0_2_xrp_into_pc_testnet_21249001__W3.json"));
}

/// AMMWithdraw: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380_w0() {
    run(include_str!("vectors/large330/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380__W0.json"));
}

/// AMMWithdraw: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380_w1() {
    run(include_str!("vectors/large330/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380__W1.json"));
}

/// AMMWithdraw: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384_w0() {
    run(include_str!("vectors/large330/c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384__W0.json"));
}

/// AMMWithdraw: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384_w1() {
    run(include_str!("vectors/large330/c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384__W1.json"));
}

/// AMMWithdraw: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384_w3() {
    run(include_str!("vectors/large330/c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384__W3.json"));
}

/// OfferCreate: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_sell_crossing_takes_the_pools_whole_swap_106644289_w0() {
    run(include_str!("vectors/large330/offer_sell_crossing_takes_the_pools_whole_swap_106644289__W0.json"));
}

/// OfferCreate: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_w0() {
    run(include_str!("vectors/large330/offer_ulp_106679738__W0.json"));
}

/// OfferCreate: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_w1() {
    run(include_str!("vectors/large330/offer_ulp_106679738__W1.json"));
}

/// OfferCreate: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_w3() {
    run(include_str!("vectors/large330/offer_ulp_106679738__W3.json"));
}

/// OfferCreate: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_w4() {
    run(include_str!("vectors/large330/offer_ulp_106679738__W4.json"));
}

/// Payment: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_inflight_line_cleanup_keeps_dir_entries_106730304_w0() {
    run(include_str!("vectors/large330/payment_inflight_line_cleanup_keeps_dir_entries_106730304__W0.json"));
}

/// Payment: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_inflight_line_cleanup_keeps_dir_entries_106730304_w1() {
    run(include_str!("vectors/large330/payment_inflight_line_cleanup_keeps_dir_entries_106730304__W1.json"));
}

/// Payment: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_inflight_line_cleanup_keeps_dir_entries_106730304_w2() {
    run(include_str!("vectors/large330/payment_inflight_line_cleanup_keeps_dir_entries_106730304__W2.json"));
}

/// Payment: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409_w0() {
    run(include_str!("vectors/large330/payment_self_owned_offer_cross_moves_no_line_106913409__W0.json"));
}

/// Payment: pre-state scaled; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409_w1() {
    run(include_str!("vectors/large330/payment_self_owned_offer_cross_moves_no_line_106913409__W1.json"));
}

/// AMMCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244_wt0() {
    run(include_str!("vectors/large330/c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244__WT0.json"));
}

/// AMMCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244_wt3() {
    run(include_str!("vectors/large330/c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244__WT3.json"));
}

/// AMMCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244_wt4() {
    run(include_str!("vectors/large330/c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244__WT4.json"));
}

/// AMMDeposit: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225_wt0() {
    run(include_str!("vectors/large330/c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225__WT0.json"));
}

/// AMMDeposit: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225_wt1() {
    run(include_str!("vectors/large330/c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225__WT1.json"));
}

/// AMMDeposit: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976_wt1() {
    run(include_str!("vectors/large330/c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976__WT1.json"));
}

/// AMMDeposit: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976_wt3() {
    run(include_str!("vectors/large330/c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976__WT3.json"));
}

/// AMMDeposit: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976_wt4() {
    run(include_str!("vectors/large330/c26a_5_0_h1_tfsingleasset_0_3_xrp_into_pc_no_clw_line_testnet_21248976__WT4.json"));
}

/// AMMWithdraw: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380_wt0() {
    run(include_str!("vectors/large330/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380__WT0.json"));
}

/// AMMWithdraw: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380_wt1() {
    run(include_str!("vectors/large330/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380__WT1.json"));
}

/// AMMWithdraw: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380_wt2() {
    run(include_str!("vectors/large330/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380__WT2.json"));
}

/// AMMWithdraw: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380_wt4() {
    run(include_str!("vectors/large330/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380__WT4.json"));
}

/// AMMWithdraw: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384_wt1() {
    run(include_str!("vectors/large330/c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384__WT1.json"));
}

/// OfferCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_4_3_l8_offercreate_gets_0_2_usd_for_up_to_0_25_xrp_crosses_p1_testnet_20976264_wt2() {
    run(include_str!("vectors/large330/c16_4_3_l8_offercreate_gets_0_2_usd_for_up_to_0_25_xrp_crosses_p1_testnet_20976264__WT2.json"));
}

/// OfferCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_wt0() {
    run(include_str!("vectors/large330/offer_ulp_106679738__WT0.json"));
}

/// OfferCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_wt1() {
    run(include_str!("vectors/large330/offer_ulp_106679738__WT1.json"));
}

/// OfferCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_wt3() {
    run(include_str!("vectors/large330/offer_ulp_106679738__WT3.json"));
}

/// OfferCreate: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_wt4() {
    run(include_str!("vectors/large330/offer_ulp_106679738__WT4.json"));
}

/// Payment: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_inflight_line_cleanup_keeps_dir_entries_106730304_wt0() {
    run(include_str!("vectors/large330/payment_inflight_line_cleanup_keeps_dir_entries_106730304__WT0.json"));
}

/// Payment: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409_wt0() {
    run(include_str!("vectors/large330/payment_self_owned_offer_cross_moves_no_line_106913409__WT0.json"));
}

/// Payment: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409_wt1() {
    run(include_str!("vectors/large330/payment_self_owned_offer_cross_moves_no_line_106913409__WT1.json"));
}

/// Payment: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409_wt3() {
    run(include_str!("vectors/large330/payment_self_owned_offer_cross_moves_no_line_106913409__WT3.json"));
}

/// Payment: pre-state and transaction scaled (unsigned); libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409_wt4() {
    run(include_str!("vectors/large330/payment_self_owned_offer_cross_moves_no_line_106913409__WT4.json"));
}

/// AMMBid: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_3_5_l7_re_bids_p1_authaccounts_l8_l9_l10_l2_holder_refunds_itsel_testnet_20976247() {
    run(include_str!("vectors/large330/c16_3_5_l7_re_bids_p1_authaccounts_l8_l9_l10_l2_holder_refunds_itsel_testnet_20976247.json"));
}

/// AMMBid: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570() {
    run(include_str!("vectors/large330/c16_8_1_l2_ammbid_p2_in_time_slot_1_decayed_price_refund_to_l6_testnet_20977570.json"));
}

/// AMMCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_1_1_l1_ammcreate_p1_xrp_usd_25_123456_xrp_100_123456_usd_fee_500_testnet_20976137() {
    run(include_str!("vectors/large330/c16_1_1_l1_ammcreate_p1_xrp_usd_25_123456_xrp_100_123456_usd_fee_500_testnet_20976137.json"));
}

/// AMMCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_1_2_l2_ammcreate_p2_usd_eur_50_5_usd_40_25_eur_fee_1000_testnet_20976139() {
    run(include_str!("vectors/large330/c16_1_2_l2_ammcreate_p2_usd_eur_50_5_usd_40_25_eur_fee_1000_testnet_20976139.json"));
}

/// AMMCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_1_3_l3_ammcreate_p3_xrp_tfx_20_5_xrp_60_60606_tfx_fee_300_transf_testnet_20976141() {
    run(include_str!("vectors/large330/c16_1_3_l3_ammcreate_p3_xrp_tfx_20_5_xrp_60_60606_tfx_fee_300_transf_testnet_20976141.json"));
}

/// AMMCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c22_i_creates_an_xrp_usd_amm_devnet_5531476() {
    run(include_str!("vectors/large330/c22_i_creates_an_xrp_usd_amm_devnet_5531476.json"));
}

/// AMMCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244() {
    run(include_str!("vectors/large330/c25_0_13_a_creates_the_clw_xrp_pool_devnet_5595244.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn amm_deposit_eprice_106702459() {
    run(include_str!("vectors/large330/amm_deposit_eprice_106702459.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn amm_deposit_slotfee_106698295() {
    run(include_str!("vectors/large330/amm_deposit_slotfee_106698295.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_2_1_l4_tfsingleasset_1_234567_xrp_into_p1_no_lp_line_yet_testnet_20976165() {
    run(include_str!("vectors/large330/c16_2_1_l4_tfsingleasset_1_234567_xrp_into_p1_no_lp_line_yet_testnet_20976165.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_2_24_l6_tfsingleasset_p2_2_345678_eur_testnet_20976214() {
    run(include_str!("vectors/large330/c16_2_24_l6_tfsingleasset_p2_2_345678_eur_testnet_20976214.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225() {
    run(include_str!("vectors/large330/c16_2_29_l3_tfsingleasset_p3_5_555555_tfx_transfer_fee_waived_testnet_20976225.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_2_32_l6_tfsingleasset_p4_1_5_xrp_fee_0_pool_testnet_20976232() {
    run(include_str!("vectors/large330/c16_2_32_l6_tfsingleasset_p4_1_5_xrp_fee_0_pool_testnet_20976232.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_2_3_da_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_1_testnet_21248765() {
    run(include_str!("vectors/large330/c26a_2_3_da_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_1_testnet_21248765.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_2_4_db_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_fee_testnet_21248774() {
    run(include_str!("vectors/large330/c26a_2_4_db_tfsingleasset_1_5_usd_into_px_pre_fee_r_2_fee_testnet_21248774.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_2_6_dd_tfsingleasset_1_5_eur_into_pi_iou_iou_pre_fee_testnet_21248792() {
    run(include_str!("vectors/large330/c26a_2_6_dd_tfsingleasset_1_5_eur_into_pi_iou_iou_pre_fee_testnet_21248792.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_3_0_w2_tfsingleasset_200000_drops_into_tfx_pool_testnet_21248857() {
    run(include_str!("vectors/large330/c26a_3_0_w2_tfsingleasset_200000_drops_into_tfx_pool_testnet_21248857.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_4_10_g2_issuer_tfsingleasset_2_tfx_into_pm_testnet_21248957() {
    run(include_str!("vectors/large330/c26a_4_10_g2_issuer_tfsingleasset_2_tfx_into_pm_testnet_21248957.json"));
}

/// AMMDeposit: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_5_8_h2_tfsingleasset_0_2_xrp_into_pc_testnet_21249001() {
    run(include_str!("vectors/large330/c26a_5_8_h2_tfsingleasset_0_2_xrp_into_pc_testnet_21249001.json"));
}

/// AMMWithdraw: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_6_7_l10_withdraws_0_05_xrp_deep_frozen_usd_line_xrp_only_testnet_20976322() {
    run(include_str!("vectors/large330/c16_6_7_l10_withdraws_0_05_xrp_deep_frozen_usd_line_xrp_only_testnet_20976322.json"));
}

/// AMMWithdraw: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380() {
    run(include_str!("vectors/large330/c16_7_17_l6_tfsingleasset_p2_0_333333_eur_testnet_20976380.json"));
}

/// AMMWithdraw: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384() {
    run(include_str!("vectors/large330/c16_7_19_l6_tflimitlptoken_p2_min_0_05_eur_eprice_e_0_15_eur_testnet_20976384.json"));
}

/// AMMWithdraw: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_3_17_w3_tftwoasset_amount_xrp_80000_amount2_usd_max_5_li_testnet_21248887() {
    run(include_str!("vectors/large330/c26a_3_17_w3_tftwoasset_amount_xrp_80000_amount2_usd_max_5_li_testnet_21248887.json"));
}

/// AMMWithdraw: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_3_28_w5_tfsingleasset_0_1_usd_pre_fee_r_4_exactly_testnet_21248928() {
    run(include_str!("vectors/large330/c26a_3_28_w5_tfsingleasset_0_1_usd_pre_fee_r_4_exactly_testnet_21248928.json"));
}

/// AMMWithdraw: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26a_3_6_w6_tfsingleasset_0_2_usd_from_pi_at_oc_1_balance_1_25_testnet_21248849() {
    run(include_str!("vectors/large330/c26a_3_6_w6_tfsingleasset_0_2_usd_from_pi_at_oc_1_balance_1_25_testnet_21248849.json"));
}

/// Clawback: pinned vector; libxrpl tecPSEUDO_ACCOUNT; differs from the Small scale.
#[test]
fn c18_5_10_i1_claws_5_a_from_an_amm_account_testnet_20976517() {
    run(include_str!("vectors/large330/c18_5_10_i1_claws_5_a_from_an_amm_account_testnet_20976517.json"));
}

/// Clawback: pinned vector; libxrpl tecPSEUDO_ACCOUNT; differs from the Small scale.
#[test]
fn c18_9_1_i1_claws_1_usd_iou_from_the_amm_account_i1_lacks_allowtrustl_testnet_20976751() {
    run(include_str!("vectors/large330/c18_9_1_i1_claws_1_usd_iou_from_the_amm_account_i1_lacks_allowtrustl_testnet_20976751.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26c_p7_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249146() {
    run(include_str!("vectors/large330/c26c_p7_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249146.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c26c_p7b_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249238() {
    run(include_str!("vectors/large330/c26c_p7b_x1_ioc_buy30_limit_at_book_quality_pool_beside_book_testnet_21249238.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807() {
    run(include_str!("vectors/large330/offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807_fs0() {
    run(include_str!("vectors/large330/offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807__FS0.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807_fs1() {
    run(include_str!("vectors/large330/offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807__FS1.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807_fs2() {
    run(include_str!("vectors/large330/offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807__FS2.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807_fs3() {
    run(include_str!("vectors/large330/offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807__FS3.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn offer_bridged_pool_turn_is_bounded_by_the_deferred_line_106823771() {
    run(include_str!("vectors/large330/offer_bridged_pool_turn_is_bounded_by_the_deferred_line_106823771.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn offer_bridged_round_bound_is_per_iteration_106823899() {
    run(include_str!("vectors/large330/offer_bridged_round_bound_is_per_iteration_106823899.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn offer_bridged_strands_die_for_the_rest_of_the_crossing_106823884() {
    run(include_str!("vectors/large330/offer_bridged_strands_die_for_the_rest_of_the_crossing_106823884.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn offer_fib_slice_quality_is_getrate_rounded_106815773() {
    run(include_str!("vectors/large330/offer_fib_slice_quality_is_getrate_rounded_106815773.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ioc_sell_in_capped_fill_keeps_the_limit_forgiveness_106759265() {
    run(include_str!("vectors/large330/offer_ioc_sell_in_capped_fill_keeps_the_limit_forgiveness_106759265.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_residual_rests_on_a_one_ulp_line_fuzz_107060755() {
    run(include_str!("vectors/large330/offer_residual_rests_on_a_one_ulp_line_fuzz_107060755.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_residual_rests_only_while_the_account_is_still_funded_fuzz_107060755() {
    run(include_str!("vectors/large330/offer_residual_rests_only_while_the_account_is_still_funded_fuzz_107060755.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_sell_crossing_takes_the_pools_whole_swap_106644289() {
    run(include_str!("vectors/large330/offer_sell_crossing_takes_the_pools_whole_swap_106644289.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn offer_strand_refused_by_its_own_tip_stays_dead_fuzz_107009438() {
    run(include_str!("vectors/large330/offer_strand_refused_by_its_own_tip_stays_dead_fuzz_107009438.json"));
}

/// OfferCreate: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738() {
    run(include_str!("vectors/large330/offer_ulp_106679738.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn c18_3_15_h1_h2_9876543210987655_b_sendmax_14814814816481483_testnet_20976411() {
    run(include_str!("vectors/large330/c18_3_15_h1_h2_9876543210987655_b_sendmax_14814814816481483_testnet_20976411.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_xrp_residual_is_exact_drops_106736673() {
    run(include_str!("vectors/large330/offer_xrp_residual_is_exact_drops_106736673.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_anchored_pool_slice_and_its_level_share_one_pass_106770629() {
    run(include_str!("vectors/large330/payment_anchored_pool_slice_and_its_level_share_one_pass_106770629.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn payment_first_hop_trin_in_the_quality_function_106732759() {
    run(include_str!("vectors/large330/payment_first_hop_trin_in_the_quality_function_106732759.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn payment_five_pool_path_rev_pass_escalates_past_a_sub_drop_rung_106921383() {
    run(include_str!("vectors/large330/payment_five_pool_path_rev_pass_escalates_past_a_sub_drop_rung_106921383.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_106734485() {
    run(include_str!("vectors/large330/payment_forward_pass_reswaps_the_downstream_pool_106734485.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_106734485_fs0() {
    run(include_str!("vectors/large330/payment_forward_pass_reswaps_the_downstream_pool_106734485__FS0.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_106734485_fs1() {
    run(include_str!("vectors/large330/payment_forward_pass_reswaps_the_downstream_pool_106734485__FS1.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_106734485_fs2() {
    run(include_str!("vectors/large330/payment_forward_pass_reswaps_the_downstream_pool_106734485__FS2.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_forward_pass_reswaps_the_downstream_pool_106734485_fs3() {
    run(include_str!("vectors/large330/payment_forward_pass_reswaps_the_downstream_pool_106734485__FS3.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn payment_forward_pool_hop_reanchors_to_the_cached_out_106823210() {
    run(include_str!("vectors/large330/payment_forward_pool_hop_reanchors_to_the_cached_out_106823210.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn payment_forward_pool_hop_reanchors_to_the_cached_out_second_specimen_106823225() {
    run(include_str!("vectors/large330/payment_forward_pool_hop_reanchors_to_the_cached_out_second_specimen_106823225.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn payment_inflight_line_cleanup_keeps_dir_entries_106730304() {
    run(include_str!("vectors/large330/payment_inflight_line_cleanup_keeps_dir_entries_106730304.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_iou_subtraction_at_the_cusp_rounds_the_coarse_mantissa_106823197() {
    run(include_str!("vectors/large330/payment_iou_subtraction_at_the_cusp_rounds_the_coarse_mantissa_106823197.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_mixed_book_segment_nets_at_mulratio_nearest_second_specimen_106733664() {
    run(include_str!("vectors/large330/payment_mixed_book_segment_nets_at_mulratio_nearest_second_specimen_106733664.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_ninety_six_digit_amount_parses_fuzz_107009438() {
    run(include_str!("vectors/large330/payment_ninety_six_digit_amount_parses_fuzz_107009438.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_parked_carry_take_back_is_capped_at_the_park_106733664() {
    run(include_str!("vectors/large330/payment_parked_carry_take_back_is_capped_at_the_park_106733664.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale; a swapAssetOut specimen: differed from libxrpl before 61cf884.
#[test]
fn payment_pool_drain_max_offer_106704888() {
    run(include_str!("vectors/large330/payment_pool_drain_max_offer_106704888.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_pool_stands_aside_on_raw_quality_not_fee_spot_107056200_fs0() {
    run(include_str!("vectors/large330/payment_pool_stands_aside_on_raw_quality_not_fee_spot_107056200__FS0.json"));
}

/// Payment: pinned vector; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn payment_self_owned_offer_cross_moves_no_line_106913409() {
    run(include_str!("vectors/large330/payment_self_owned_offer_cross_moves_no_line_106913409.json"));
}

/// AMMVote: targeted mutant; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn vote_weight_half_t0() {
    run(include_str!("vectors/large330/vote_weight_half__T0.json"));
}

/// NFTokenAcceptOffer: targeted mutant; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn nft_royalty_double_rounding_t0() {
    run(include_str!("vectors/large330/nft_royalty_double_rounding__T0.json"));
}

/// NFTokenAcceptOffer: targeted mutant; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn nft_royalty_double_rounding_t1() {
    run(include_str!("vectors/large330/nft_royalty_double_rounding__T1.json"));
}

/// NFTokenAcceptOffer: targeted mutant; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn nft_royalty_double_rounding_t2() {
    run(include_str!("vectors/large330/nft_royalty_double_rounding__T2.json"));
}

/// NFTokenAcceptOffer: targeted mutant; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn nft_royalty_double_rounding_t3() {
    run(include_str!("vectors/large330/nft_royalty_double_rounding__T3.json"));
}

/// NFTokenAcceptOffer: targeted mutant; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn nft_royalty_double_rounding_t4() {
    run(include_str!("vectors/large330/nft_royalty_double_rounding__T4.json"));
}

/// NFTokenAcceptOffer: targeted mutant; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn nft_royalty_double_rounding_t5() {
    run(include_str!("vectors/large330/nft_royalty_double_rounding__T5.json"));
}

/// OfferCreate: issuer TickSize set; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807_tick3() {
    run(include_str!("vectors/large330/offer_bridged_pool_slice_remainder_is_sixteen_digits_106823807__TICK3.json"));
}

/// OfferCreate: issuer TickSize set; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_sell_crossing_takes_the_pools_whole_swap_106644289_tick8() {
    run(include_str!("vectors/large330/offer_sell_crossing_takes_the_pools_whole_swap_106644289__TICK8.json"));
}

/// OfferCreate: issuer TickSize set; libxrpl tesSUCCESS; differs from the Small scale.
#[test]
fn offer_ulp_106679738_tick8() {
    run(include_str!("vectors/large330/offer_ulp_106679738__TICK8.json"));
}
