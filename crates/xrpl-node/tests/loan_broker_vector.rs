//! LendingProtocol loan-broker byte-exact vectors: real devnet transactions of the five loan-broker types
//! (LoanBrokerSet, LoanBrokerDelete, LoanBrokerCoverDeposit, LoanBrokerCoverWithdraw, LoanBrokerCoverClawback), each
//! with the pre-state of everything it reads, its recorded result and the post-state of everything its metadata names.
//!
//! - Devnet runs rippled 3.5.0-rc5 with SingleAssetVault, LendingProtocol, LendingProtocolV1_1 and fixCleanup3_5_0
//!   enabled. Every bundle carries the Amendments object of its parent ledger, so the engine applies each transaction
//!   under the rules devnet applied it under.
//! - A vector passes when the engine returns the recorded TransactionResult, every object the metadata names comes
//!   out byte-identical (threading included), and the engine wrote no object the metadata does not name — the pass
//!   criteria of `vault_vector.rs`, whose harness this is.
//! - Bundles: `scripts/loan_broker_bundle.py <hash> <ledger> <out>` with `XRPL_RPC` set to a devnet server (it drives
//!   `scripts/fetch_tx_bundle.py`, the format of the campaign vectors).
//! - All of them are from one run of a test suite on devnet, ledgers 7129413-7129651 (2026-11-21).
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
    state.state_map.insert(key32(key_hex), serde_json::to_vec(&jv).unwrap()).unwrap();
}

fn encoded(bytes: &[u8]) -> String {
    let mut jv: Value = serde_json::from_slice(bytes).unwrap();
    canon_for_encode(&mut jv);
    hex::encode_upper(xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap())
}

/// Hydrate the pre-state, apply the transaction, compare the result and every object.
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
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    for (k, v) in bundle["pre"].as_object().unwrap() {
        hydrate(&mut state, k, v.as_str().unwrap());
    }
    let txf = build_txfields(&bundle["tx"]).expect("txfields");
    let (ter, mut mods) = native_apply_one(&state, &txf);
    assert_eq!(ter, bundle["result"].as_str().unwrap(), "devnet recorded this transaction result");
    let pre = |k: &Hash256| state.state_map.lookup(k).map(|b| b.to_vec());
    xrpl_ledger::ledger::threading::stamp_threading(&mut mods, &pre, bundle["tx"]["hash"].as_str().unwrap(), seq);

    let expect = bundle["expect"].as_object().unwrap();
    let mut differences = Vec::new();
    for (k, want) in expect {
        let want = want.as_str().unwrap().trim().to_uppercase();
        match mods.get(&key32(k)) {
            None => {
                // Not written: right only for a pinned, untouched pre-image.
                let seated = bundle["pre"][k].as_str().unwrap_or_default().trim().to_uppercase();
                if want.is_empty() || want != seated {
                    differences.push(format!("{k}: devnet changed it, the engine did not write it"));
                }
            }
            Some(SandboxEntry::Deleted) => {
                if !want.is_empty() {
                    differences.push(format!("{k}: the engine deleted it, devnet kept it"));
                }
            }
            Some(SandboxEntry::Created(b)) | Some(SandboxEntry::Modified(b)) => {
                if want.is_empty() {
                    differences.push(format!("{k}: devnet deleted it, the engine kept it"));
                } else if encoded(b) != want {
                    let ours: Value = serde_json::from_slice(b).unwrap();
                    let mut theirs = xrpl_core::codec::decode::decode_transaction_binary(&hex::decode(&want).unwrap()).unwrap();
                    hexify_addresses(&mut theirs);
                    differences.push(format!("{k}: bytes differ\n  ours   {ours}\n  devnet {theirs}"));
                }
            }
        }
    }
    // Nothing beyond what the metadata names.
    for (k, entry) in &mods {
        let key = hex::encode_upper(k.0);
        if !expect.contains_key(&key) {
            let what = match entry {
                SandboxEntry::Deleted => "deleted".to_string(),
                SandboxEntry::Created(b) | SandboxEntry::Modified(b) => String::from_utf8_lossy(b).into_owned(),
            };
            differences.push(format!("{key}: the engine wrote an object devnet's metadata does not name: {what}"));
        }
    }
    assert!(differences.is_empty(), "{} difference(s):\n{}", differences.len(), differences.join("\n"));
}

// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerSet
// ---------------------------------------------------------------------------------------------------------------

/// 0D773E36BF…: a broker on a closed-ended XRP vault: the LoanBroker, its pseudo-account, both directory links, the owner's two reserve units.
#[test]
fn loan_broker_set_xrp_devnet_7129515() {
    run(include_str!("vectors/loan_broker_set_xrp_devnet_7129515.json"));
}

/// 931634CAC7…: a broker on a closed-ended USD vault: the pseudo-account's trust line and its directory as well; the issuer's root threaded.
#[test]
fn loan_broker_set_iou_devnet_7129447() {
    run(include_str!("vectors/loan_broker_set_iou_devnet_7129447.json"));
}

/// 90461A3EF6…: a broker on a closed-ended vault of an MPT: the pseudo-account's MPToken and its directory as well.
#[test]
fn loan_broker_set_mpt_devnet_7129450() {
    run(include_str!("vectors/loan_broker_set_mpt_devnet_7129450.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerCoverDeposit
// ---------------------------------------------------------------------------------------------------------------

/// 8444E68F3E…: 10000 drops; tesSUCCESS.
#[test]
fn loan_broker_cover_deposit_xrp_devnet_7129516() {
    run(include_str!("vectors/loan_broker_cover_deposit_xrp_devnet_7129516.json"));
}

/// 299F45DC76…: 10000 USD; tesSUCCESS.
#[test]
fn loan_broker_cover_deposit_iou_devnet_7129448() {
    run(include_str!("vectors/loan_broker_cover_deposit_iou_devnet_7129448.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerCoverWithdraw
// ---------------------------------------------------------------------------------------------------------------

/// EC9F46C8DC…: all 10000 USD of the cover back to the owner; tesSUCCESS.
#[test]
fn loan_broker_cover_withdraw_iou_to_self_devnet_7129451() {
    run(include_str!("vectors/loan_broker_cover_withdraw_iou_to_self_devnet_7129451.json"));
}

/// 818371AE77…: 10000 USD to a Destination holding a line; tesSUCCESS.
#[test]
fn loan_broker_cover_withdraw_iou_to_destination_devnet_7129442() {
    run(include_str!("vectors/loan_broker_cover_withdraw_iou_to_destination_devnet_7129442.json"));
}

/// 71CC04F2B9…: XRP to a Destination that does not exist; tecNO_DST.
#[test]
fn loan_broker_cover_withdraw_xrp_tec_no_dst_devnet_7129517() {
    run(include_str!("vectors/loan_broker_cover_withdraw_xrp_tec_no_dst_devnet_7129517.json"));
}

/// 71978B6E73…: USD to a Destination with no trust line; tecNO_LINE.
#[test]
fn loan_broker_cover_withdraw_iou_tec_no_line_devnet_7129491() {
    run(include_str!("vectors/loan_broker_cover_withdraw_iou_tec_no_line_devnet_7129491.json"));
}

/// DBA6011F21…: an MPT to a Destination while the owner's MPToken is locked; tecLOCKED.
#[test]
fn loan_broker_cover_withdraw_mpt_tec_locked_devnet_7129459() {
    run(include_str!("vectors/loan_broker_cover_withdraw_mpt_tec_locked_devnet_7129459.json"));
}

/// 2803655FDD…: a USD amount from an XRP broker; tecWRONG_ASSET.
#[test]
fn loan_broker_cover_withdraw_tec_wrong_asset_devnet_7129487() {
    run(include_str!("vectors/loan_broker_cover_withdraw_tec_wrong_asset_devnet_7129487.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerCoverClawback
// ---------------------------------------------------------------------------------------------------------------

/// D22C6998E4…: the MPT's issuer takes the whole cover of 10000 (LoanBrokerID and Amount); tesSUCCESS.
#[test]
fn loan_broker_cover_clawback_mpt_all_devnet_7129413() {
    run(include_str!("vectors/loan_broker_cover_clawback_mpt_all_devnet_7129413.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// LoanBrokerDelete
// ---------------------------------------------------------------------------------------------------------------

/// 94D8F7238B…: a USD broker with no cover, after its only loan was deleted: the broker, its pseudo-account, the pseudo-account's line and directory go; tesSUCCESS.
#[test]
fn loan_broker_delete_iou_devnet_7129651() {
    run(include_str!("vectors/loan_broker_delete_iou_devnet_7129651.json"));
}

// ---------------------------------------------------------------------------------------------------------------
// The harness itself
// ---------------------------------------------------------------------------------------------------------------

/// A vector with a different recorded result fails.
#[test]
#[should_panic(expected = "devnet recorded this transaction result")]
fn the_harness_refuses_a_different_result() {
    let mut bundle: Value = serde_json::from_str(include_str!("vectors/loan_broker_cover_deposit_xrp_devnet_7129516.json")).unwrap();
    bundle["result"] = Value::String("tecFROZEN".to_string());
    run(&bundle.to_string());
}

/// A vector whose expected LoanBroker is another object's image fails.
#[test]
#[should_panic(expected = "bytes differ")]
fn the_harness_refuses_different_bytes() {
    let mut bundle: Value = serde_json::from_str(include_str!("vectors/loan_broker_cover_deposit_xrp_devnet_7129516.json")).unwrap();
    let broker = bundle["tx"]["LoanBrokerID"].as_str().unwrap().to_string();
    let other = bundle["expect"].as_object().unwrap().iter().find(|(k, v)| **k != broker && !v.as_str().unwrap().is_empty()).unwrap().1.clone();
    bundle["expect"][&broker] = other;
    run(&bundle.to_string());
}

/// A vector that leaves out an object the transaction wrote fails.
#[test]
#[should_panic(expected = "does not name")]
fn the_harness_refuses_an_unnamed_write() {
    let mut bundle: Value = serde_json::from_str(include_str!("vectors/loan_broker_cover_deposit_xrp_devnet_7129516.json")).unwrap();
    let broker = bundle["tx"]["LoanBrokerID"].as_str().unwrap().to_string();
    bundle["expect"].as_object_mut().unwrap().remove(&broker);
    run(&bundle.to_string());
}
