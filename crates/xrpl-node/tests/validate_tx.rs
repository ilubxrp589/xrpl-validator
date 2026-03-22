//! Live test: validate a real XRPL testnet transaction.
//!
//! Fetches a real signed transaction from testnet, runs it through
//! our validation pipeline, and verifies field extraction + signature.
//!
//! Run with: cargo test -p xrpl-node --features live-tests -- validate_tx --nocapture

#![cfg(feature = "live-tests")]

use xrpl_node::mempool::{validate_transaction, ValidationResult};

const TESTNET_RPC: &str = "https://s.altnet.rippletest.net:51234";

#[tokio::test]
async fn validate_real_testnet_transaction() {
    println!("Fetching a real transaction from testnet...");

    let client = reqwest::Client::new();

    // Get a validated ledger with expanded binary transactions
    let resp = client
        .post(TESTNET_RPC)
        .json(&serde_json::json!({
            "method": "ledger",
            "params": [{
                "ledger_index": "validated",
                "transactions": true,
                "expand": true,
                "binary": true
            }]
        }))
        .send()
        .await
        .expect("RPC failed")
        .json::<serde_json::Value>()
        .await
        .expect("JSON parse failed");

    let txs = resp["result"]["ledger"]["transactions"]
        .as_array()
        .expect("no transactions array");

    if txs.is_empty() {
        println!("No transactions in this ledger, skipping");
        return;
    }

    println!("Found {} transactions in ledger", txs.len());

    let mut validated_count = 0;

    for (i, tx) in txs.iter().enumerate().take(5) {
        let tx_blob_hex = tx["tx_blob"].as_str().expect("missing tx_blob");
        let tx_blob = hex::decode(tx_blob_hex).expect("bad hex");

        println!("\n--- Transaction {} ({} bytes) ---", i + 1, tx_blob.len());

        let (result, tx_hash, fields) = validate_transaction(&tx_blob);

        println!("  Hash: {}", hex::encode_upper(tx_hash.0));
        println!("  Result: {:?}", result);

        if let Some(ref f) = fields {
            println!("  Account: {}", f.account);
            println!("  Fee: {} drops", f.fee);
            println!("  Sequence: {}", f.sequence);
            println!("  LastLedgerSeq: {:?}", f.last_ledger_seq);
        }

        match result {
            ValidationResult::Valid => {
                validated_count += 1;
                println!("  SIGNATURE VERIFIED");

                // Verify fields were extracted
                let f = fields.as_ref().expect("valid tx should have fields");
                assert!(!f.account.is_empty(), "account should not be empty");
                assert!(f.fee > 0, "fee should be positive");
            }
            ValidationResult::Invalid(ref reason) => {
                println!("  INVALID: {reason}");
                // Some transactions may use multi-signing or other patterns
                // we don't fully support yet — that's OK
            }
        }
    }

    println!("\n{validated_count}/{} transactions validated with signature verification",
        txs.len().min(5));

    // At least one should pass
    assert!(
        validated_count > 0,
        "at least one testnet transaction should validate"
    );
}
