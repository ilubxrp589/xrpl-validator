//! Live integration test: verify our ledger header hash matches rippled.
//!
//! Run with: cargo test -p xrpl-ledger --features live-tests -- hash_verify --nocapture

#![cfg(feature = "live-tests")]

use serde_json::Value;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;

const TESTNET_RPC: &str = "https://s.altnet.rippletest.net:51234";

async fn rpc_call(method: &str, params: Value) -> Value {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "method": method,
        "params": [params]
    });

    let resp = client
        .post(TESTNET_RPC)
        .json(&body)
        .send()
        .await
        .expect("RPC request failed");

    resp.json::<Value>().await.expect("RPC parse failed")
}

fn hex_to_hash256(hex_str: &str) -> Hash256 {
    let bytes = hex::decode(hex_str).expect("bad hex");
    assert_eq!(bytes.len(), 32, "hash must be 32 bytes");
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Hash256(arr)
}

#[tokio::test]
async fn verify_ledger_header_hash() {
    println!("Fetching validated ledger from testnet...");

    let resp = rpc_call("ledger", serde_json::json!({
        "ledger_index": "validated",
        "full": false,
        "accounts": false,
        "transactions": false,
    }))
    .await;

    let ledger = &resp["result"]["ledger"];
    assert!(
        resp["result"]["status"] == "success",
        "RPC failed: {resp}"
    );

    // Extract fields
    let expected_hash_hex = ledger["ledger_hash"]
        .as_str()
        .expect("missing ledger_hash");
    let sequence = ledger["ledger_index"]
        .as_str()
        .expect("missing ledger_index")
        .parse::<u32>()
        .expect("bad ledger_index");
    let total_coins = ledger["total_coins"]
        .as_str()
        .expect("missing total_coins")
        .parse::<u64>()
        .expect("bad total_coins");
    let parent_hash = ledger["parent_hash"]
        .as_str()
        .expect("missing parent_hash");
    let transaction_hash = ledger["transaction_hash"]
        .as_str()
        .expect("missing transaction_hash");
    let account_hash = ledger["account_hash"]
        .as_str()
        .expect("missing account_hash");
    let parent_close_time = ledger["parent_close_time"]
        .as_u64()
        .expect("missing parent_close_time") as u32;
    let close_time = ledger["close_time"]
        .as_u64()
        .expect("missing close_time") as u32;
    let close_time_resolution = ledger["close_time_resolution"]
        .as_u64()
        .expect("missing close_time_resolution") as u8;
    let close_flags = ledger["close_flags"]
        .as_u64()
        .expect("missing close_flags") as u8;

    println!("  Ledger #{sequence}");
    println!("  Expected hash: {expected_hash_hex}");
    println!("  Total coins:   {total_coins}");
    println!("  Close time:    {close_time}");

    // Build our LedgerHeader
    let header = LedgerHeader {
        sequence,
        total_coins,
        parent_hash: hex_to_hash256(parent_hash),
        transaction_hash: hex_to_hash256(transaction_hash),
        account_hash: hex_to_hash256(account_hash),
        parent_close_time,
        close_time,
        close_time_resolution,
        close_flags,
    };

    // Compute our hash
    let our_hash = header.hash();
    let our_hash_hex = hex::encode_upper(our_hash.0);
    let expected_hash = hex_to_hash256(expected_hash_hex);

    println!("  Our hash:      {our_hash_hex}");

    assert_eq!(
        our_hash, expected_hash,
        "\nHASH MISMATCH!\n  Expected: {expected_hash_hex}\n  Got:      {our_hash_hex}\n\
        \nSerialized header ({} bytes): {:02x?}",
        header.serialize().len(),
        &header.serialize()[..32], // first 32 bytes for debugging
    );

    println!("\n  MATCH! Our LedgerHeader hash matches rippled.");

    // Verify against multiple ledgers for confidence
    println!("\nVerifying 5 more ledgers...");
    for offset in 1..=5 {
        let seq = sequence - offset;
        let resp = rpc_call("ledger", serde_json::json!({
            "ledger_index": seq,
        }))
        .await;

        let l = &resp["result"]["ledger"];
        if l.is_null() {
            println!("  Ledger #{seq}: not available, skipping");
            continue;
        }

        let exp_hash = l["ledger_hash"].as_str().unwrap_or("?");
        let h = LedgerHeader {
            sequence: l["ledger_index"].as_str().unwrap().parse().unwrap(),
            total_coins: l["total_coins"].as_str().unwrap().parse().unwrap(),
            parent_hash: hex_to_hash256(l["parent_hash"].as_str().unwrap()),
            transaction_hash: hex_to_hash256(l["transaction_hash"].as_str().unwrap()),
            account_hash: hex_to_hash256(l["account_hash"].as_str().unwrap()),
            parent_close_time: l["parent_close_time"].as_u64().unwrap() as u32,
            close_time: l["close_time"].as_u64().unwrap() as u32,
            close_time_resolution: l["close_time_resolution"].as_u64().unwrap() as u8,
            close_flags: l["close_flags"].as_u64().unwrap() as u8,
        };

        let hash_hex = hex::encode_upper(h.hash().0);
        let matches = hash_hex == exp_hash;
        println!("  Ledger #{seq}: {}", if matches { "MATCH" } else { "MISMATCH" });
        assert!(matches, "Hash mismatch for ledger #{seq}: expected {exp_hash}, got {hash_hex}");
    }

    println!("\nAll 6 ledger hashes verified against testnet!");
}
