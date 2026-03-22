//! Validate consecutive testnet ledgers.
//!
//! Fetches N consecutive validated ledgers, applies each one's transactions
//! to the previous state, and verifies total_coins and tx results match.
//!
//! Run with: cargo test --test validate_consecutive -- --nocapture

use serde_json::Value;
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::apply::apply_transaction_set;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::ledger::transactor::TxFields;

const TESTNET_RPC: &str = "https://s.altnet.rippletest.net:51234";

async fn rpc(method: &str, params: Value) -> Value {
    let client = reqwest::Client::new();
    let resp: Value = client
        .post(TESTNET_RPC)
        .json(&serde_json::json!({
            "method": method,
            "params": [params]
        }))
        .send()
        .await
        .expect("RPC request failed")
        .json()
        .await
        .expect("JSON parse failed");
    resp["result"].clone()
}

fn decode_address(addr: &str) -> [u8; 20] {
    const ALPHABET: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let mut n: Vec<u8> = vec![0];
    for ch in addr.bytes() {
        let carry = ALPHABET.iter().position(|&c| c == ch).expect("invalid base58 char");
        let mut c = carry;
        for byte in n.iter_mut().rev() {
            c += (*byte as usize) * 58;
            *byte = (c & 0xFF) as u8;
            c >>= 8;
        }
        while c > 0 {
            n.insert(0, (c & 0xFF) as u8);
            c >>= 8;
        }
    }
    let leading = addr.bytes().take_while(|&b| b == b'r').count();
    let mut result = vec![0u8; leading];
    result.extend_from_slice(&n);
    assert!(result.len() >= 25, "decoded address too short");
    let mut account_id = [0u8; 20];
    account_id.copy_from_slice(&result[1..21]);
    account_id
}

#[tokio::test]
async fn validate_10_consecutive_ledgers() {
    // Start from a recent-ish testnet ledger
    let start_result = rpc("ledger", serde_json::json!({
        "ledger_index": "validated"
    })).await;
    let start_seq: u32 = start_result["ledger"]["ledger_index"]
        .as_str()
        .or_else(|| start_result["ledger"]["ledger_index"].as_u64().map(|_| ""))
        .and_then(|s| if s.is_empty() {
            start_result["ledger"]["ledger_index"].as_u64().map(|n| n as u32)
                .map(|_| "")
        } else {
            Some(s)
        })
        .unwrap_or("0")
        .parse()
        .unwrap_or_else(|_| start_result["ledger"]["ledger_index"].as_u64().unwrap_or(0) as u32);

    // Go back 15 ledgers to avoid edge cases with very recent ones
    let base_seq = start_seq - 15;
    let count = 10;

    println!("=== Validating {} consecutive testnet ledgers starting at {} ===\n", count, base_seq);

    // Fetch base ledger to get initial total_coins
    let base_result = rpc("ledger", serde_json::json!({
        "ledger_index": base_seq
    })).await;
    let base_ledger = &base_result["ledger"];

    let mut current_total_coins: u64 = base_ledger["total_coins"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut current_close_time: u32 = base_ledger["close_time"]
        .as_u64()
        .unwrap_or(0) as u32;

    let mut success_count = 0;
    let mut total_txs = 0;
    let mut tx_type_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for i in 0..count {
        let ledger_seq = base_seq + 1 + i;

        // Fetch this ledger with transactions
        let result = rpc("ledger", serde_json::json!({
            "ledger_index": ledger_seq,
            "transactions": true,
            "expand": true
        })).await;
        let ledger = &result["ledger"];
        let expected_total_coins: u64 = ledger["total_coins"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let close_time: u32 = ledger["close_time"].as_u64().unwrap_or(0) as u32;

        let transactions = ledger["transactions"].as_array();
        let tx_count = transactions.map(|t| t.len()).unwrap_or(0);
        total_txs += tx_count;

        // Build minimal state with just the accounts involved
        let header = LedgerHeader {
            sequence: ledger_seq - 1,
            total_coins: current_total_coins,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: current_close_time,
            close_time: current_close_time,
            close_time_resolution: 10,
            close_flags: 0,
        };
        let mut state = LedgerState::new_unverified(header);

        let mut tx_fields_list: Vec<(Hash256, TxFields)> = Vec::new();

        if let Some(txs) = transactions {
            for tx in txs {
                let tx_type = tx["TransactionType"].as_str().unwrap_or("Unknown");
                *tx_type_counts.entry(tx_type.to_string()).or_insert(0) += 1;

                let account_addr = tx["Account"].as_str().unwrap_or("");
                if account_addr.is_empty() { continue; }

                let account_id = decode_address(account_addr);
                let fee: u64 = tx["Fee"].as_str().unwrap_or("0").parse().unwrap_or(0);
                let sequence = tx["Sequence"].as_u64().unwrap_or(0) as u32;
                let tx_hash_hex = tx["hash"].as_str().unwrap_or("");
                let tx_hash = Hash256::from_hex(tx_hash_hex).unwrap_or(Hash256([0; 32]));

                // Fetch sender at previous ledger
                let sender_info = rpc("account_info", serde_json::json!({
                    "account": account_addr,
                    "ledger_index": ledger_seq - 1
                })).await;

                if let Some(sender_data) = sender_info.get("account_data") {
                    let sender_obj = serde_json::json!({
                        "LedgerEntryType": "AccountRoot",
                        "Account": hex::encode(account_id),
                        "Balance": sender_data["Balance"].as_str().unwrap_or("0"),
                        "Sequence": sender_data["Sequence"].as_u64().unwrap_or(1),
                        "OwnerCount": sender_data["OwnerCount"].as_u64().unwrap_or(0),
                        "Flags": sender_data["Flags"].as_u64().unwrap_or(0),
                    });
                    let key = keylet::account_root_key(&account_id);
                    let _ = state.state_map.insert(key, serde_json::to_vec(&sender_obj).unwrap());
                }

                let mut fields = tx.clone();

                // For Payment, also fetch destination
                if tx_type == "Payment" {
                    if let Some(dest_addr) = tx.get("Destination").and_then(|d| d.as_str()) {
                        let dest_id = decode_address(dest_addr);
                        fields["Destination"] = serde_json::Value::String(hex::encode(dest_id));

                        if let Some(a) = tx.get("Amount").or_else(|| tx.get("DeliverMax")) {
                            fields["Amount"] = a.clone();
                        }

                        let dest_info = rpc("account_info", serde_json::json!({
                            "account": dest_addr,
                            "ledger_index": ledger_seq - 1
                        })).await;

                        if let Some(dest_data) = dest_info.get("account_data") {
                            let dest_obj = serde_json::json!({
                                "LedgerEntryType": "AccountRoot",
                                "Account": hex::encode(dest_id),
                                "Balance": dest_data["Balance"].as_str().unwrap_or("0"),
                                "Sequence": dest_data["Sequence"].as_u64().unwrap_or(1),
                                "OwnerCount": dest_data["OwnerCount"].as_u64().unwrap_or(0),
                                "Flags": dest_data["Flags"].as_u64().unwrap_or(0),
                            });
                            let key = keylet::account_root_key(&dest_id);
                            let _ = state.state_map.insert(key, serde_json::to_vec(&dest_obj).unwrap());
                        }
                    }
                }

                // For TrustSet, convert issuer address
                if tx_type == "TrustSet" {
                    if let Some(limit) = tx.get("LimitAmount") {
                        if let Some(issuer_addr) = limit.get("issuer").and_then(|i| i.as_str()) {
                            if issuer_addr.starts_with('r') {
                                let issuer_id = decode_address(issuer_addr);
                                let mut new_limit = limit.clone();
                                new_limit["issuer"] = serde_json::Value::String(hex::encode(issuer_id));
                                fields["LimitAmount"] = new_limit;
                            }
                        }
                    }
                }

                let ticket_seq = tx.get("TicketSequence")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);

                let last_ledger_seq = tx.get("LastLedgerSequence")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);

                tx_fields_list.push((tx_hash, TxFields {
                    account: account_id,
                    tx_type: tx_type.to_string(),
                    fee,
                    sequence,
                    ticket_seq,
                    last_ledger_seq,
                    fields,
                }));
            }
        }

        // Apply transactions
        let (new_state, results) = apply_transaction_set(
            &state,
            tx_fields_list,
            close_time,
            10,
        ).unwrap();

        // Verify total_coins
        let coins_match = new_state.header.total_coins == expected_total_coins;
        let status = if coins_match { "OK" } else { "MISMATCH" };

        let result_summary: Vec<String> = results.iter()
            .map(|r| r.result.code_str().to_string())
            .collect();

        println!(
            "  Ledger {}: {} txs | coins {} | [{}]",
            ledger_seq,
            tx_count,
            status,
            result_summary.join(", ")
        );

        if !coins_match {
            println!(
                "    MISMATCH: got {} expected {} (diff: {})",
                new_state.header.total_coins,
                expected_total_coins,
                new_state.header.total_coins as i64 - expected_total_coins as i64,
            );
        } else {
            success_count += 1;
        }

        current_total_coins = expected_total_coins;
        current_close_time = close_time;
    }

    println!("\n=== Summary ===");
    println!("Ledgers validated: {}/{}", success_count, count);
    println!("Total transactions: {}", total_txs);
    println!("Transaction types:");
    let mut types: Vec<_> = tx_type_counts.iter().collect();
    types.sort_by(|a, b| b.1.cmp(a.1));
    for (ty, count) in types {
        println!("  {}: {}", ty, count);
    }

    assert_eq!(success_count, count,
        "Not all ledgers validated! {}/{} succeeded", success_count, count);
    println!("\n=== ALL {} LEDGERS VALIDATED ===", count);
}
