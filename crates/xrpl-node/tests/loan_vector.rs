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

