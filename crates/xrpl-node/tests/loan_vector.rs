//! LendingProtocol loan byte-exact vectors: real devnet transactions of the four Loan types (LoanSet, LoanPay,
//! LoanManage, LoanDelete), each with the pre-state of everything it reads, its recorded result and the post-state of
//! everything its metadata names.
//!
//! - Devnet runs rippled 3.5.0-rc5 with SingleAssetVault, LendingProtocol, LendingProtocolV1_1 and fixCleanup3_5_0
//!   enabled. Every bundle carries the Amendments object of its parent ledger, so the engine applies each transaction
//!   under the rules devnet applied it under.
//! - A vector passes when the engine returns the recorded TransactionResult, every object the metadata names comes
//!   out byte-identical (threading included), and the engine wrote no object the metadata does not name — the pass
//!   criteria and the harness of `loan_broker_vector.rs`, plus the ledger's close-time resolution (a Loan's
//!   sfStartDate is the open ledger's close time).
//! - Bundles: `scripts/loan_bundle.py <hash> <ledger> <out>` with `XRPL_RPC` set to a devnet server (it drives
//!   `scripts/fetch_tx_bundle.py`, the format of the campaign vectors).
//! - All of them are from one run of a test suite on devnet, ledgers 7129495-7129652 (2026-11-21). Several follow one
//!   loan: made, paid, paid off — or defaulted — and deleted.
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
        // The resolution of the ledger being built: a LoanSet's sfStartDate is the parent's close time plus it.
        close_time_resolution: bundle["close_time_resolution"].as_u64().unwrap_or(10) as u8,
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
// The vectors
// ---------------------------------------------------------------------------------------------------------------

/// D3EC180861…: an interest-free XRP loan of 10000 drops over three payments that takes overpayments, the broker's owner submitting: the Loan, both directory links, the borrower's reserve, the vault's and the broker's books. Devnet: tesSUCCESS.
#[test]
fn loan_set_xrp_overpayment_devnet_7129585() {
    run(include_str!("vectors/loan_set_xrp_overpayment_devnet_7129585.json"));
}

/// 0BE9AAC97F…: the same loan: 6668 drops with tfLoanOverpayment — one scheduled payment (3334) and an overpayment of the rest, the loan re-amortized. Devnet: tesSUCCESS.
#[test]
fn loan_pay_xrp_overpayment_devnet_7129587() {
    run(include_str!("vectors/loan_pay_xrp_overpayment_devnet_7129587.json"));
}

/// 29230D2666…: the same loan: the remaining 3332 drops in one payment — paid off, its balances and due date cleared. Devnet: tesSUCCESS.
#[test]
fn loan_pay_xrp_final_devnet_7129588() {
    run(include_str!("vectors/loan_pay_xrp_final_devnet_7129588.json"));
}

/// 1E1F0E2C73…: 1000 USD at 100% a year over three payments 400 s apart: the periodic payment, the total value rounded up, the loan scale, accrual on the vault. Devnet: tesSUCCESS.
#[test]
fn loan_set_iou_interest_devnet_7129539() {
    run(include_str!("vectors/loan_set_iou_interest_devnet_7129539.json"));
}

/// 8022CFE5C5…: the same loan: 334 USD, its first scheduled payment — principal up, interest down at the loan scale. Devnet: tesSUCCESS.
#[test]
fn loan_pay_iou_interest_devnet_7129541() {
    run(include_str!("vectors/loan_pay_iou_interest_devnet_7129541.json"));
}

/// 9A2D81DB10…: 1 USD at 50% a year over five payments. Devnet: tesSUCCESS.
#[test]
fn loan_set_iou_interest_five_payments_devnet_7129555() {
    run(include_str!("vectors/loan_set_iou_interest_five_payments_devnet_7129555.json"));
}

/// ECC468D0C9…: the same loan: 1 USD offered, one scheduled payment taken. Devnet: tesSUCCESS.
#[test]
fn loan_pay_iou_interest_first_of_five_devnet_7129558() {
    run(include_str!("vectors/loan_pay_iou_interest_first_of_five_devnet_7129558.json"));
}

/// 1B28858AA2…: 200000 drops at 50% a year over three payments, overpayments allowed: interest in whole drops. Devnet: tesSUCCESS.
#[test]
fn loan_set_xrp_interest_overpayment_devnet_7129504() {
    run(include_str!("vectors/loan_set_xrp_interest_overpayment_devnet_7129504.json"));
}

/// CF9E4E3428…: the same loan: 100002 drops with tfLoanOverpayment. Devnet: tesSUCCESS.
#[test]
fn loan_pay_xrp_interest_overpayment_devnet_7129506() {
    run(include_str!("vectors/loan_pay_xrp_interest_overpayment_devnet_7129506.json"));
}

/// CEFBED9BF7…: the same loan: 33334 drops is less than the re-amortized payment; tecINSUFFICIENT_PAYMENT. Devnet: tecINSUFFICIENT_PAYMENT.
#[test]
fn loan_pay_tec_insufficient_payment_devnet_7129508() {
    run(include_str!("vectors/loan_pay_tec_insufficient_payment_devnet_7129508.json"));
}
