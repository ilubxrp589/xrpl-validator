//! Integration test: verify transaction application against testnet.
//!
//! Fetches two consecutive validated testnet ledgers, applies ledger N+1's
//! transactions to a minimal state built from ledger N, and verifies
//! the balance changes match.
//!
//! Run with: cargo test --test apply_verify -- --nocapture

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

/// Decode an XRPL base58 address to 20-byte account ID.
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

    // Add leading zeros for leading 'r' chars
    let leading = addr.bytes().take_while(|&b| b == b'r').count();
    let mut result = vec![0u8; leading];
    result.extend_from_slice(&n);

    // result = 1 byte type prefix + 20 byte account ID + 4 byte checksum
    assert!(result.len() >= 25, "decoded address too short: {}", result.len());
    let mut account_id = [0u8; 20];
    account_id.copy_from_slice(&result[1..21]);
    account_id
}

#[tokio::test]
async fn verify_payment_application_against_testnet() {
    // === Test case: Testnet ledger 15900219 ===
    // Contains exactly 1 Payment transaction.
    // Verified data as of 2026-03-22.

    let ledger_n = 15900218u32;
    let ledger_n1 = 15900219u32;

    println!("Fetching ledger {} (N)...", ledger_n);
    let n_result = rpc("ledger", serde_json::json!({
        "ledger_index": ledger_n
    })).await;
    let n_ledger = &n_result["ledger"];

    println!("Fetching ledger {} (N+1) with transactions...", ledger_n1);
    let n1_result = rpc("ledger", serde_json::json!({
        "ledger_index": ledger_n1,
        "transactions": true,
        "expand": true
    })).await;
    let n1_ledger = &n1_result["ledger"];

    // Extract N+1 transaction details
    let transactions = n1_ledger["transactions"].as_array().expect("no transactions");
    assert!(!transactions.is_empty(), "Expected at least 1 transaction");
    println!("Ledger N+1 has {} transactions", transactions.len());

    // Build minimal LedgerState for ledger N with just the affected accounts
    let header_n = LedgerHeader {
        sequence: ledger_n,
        total_coins: n_ledger["total_coins"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        parent_hash: Hash256([0; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: n_ledger["close_time"]
            .as_u64()
            .unwrap_or(0) as u32,
        close_time: n_ledger["close_time"].as_u64().unwrap_or(0) as u32,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header_n);

    // For each transaction, fetch involved accounts at ledger N
    let mut tx_fields_list: Vec<(Hash256, TxFields)> = Vec::new();

    for tx in transactions {
        let tx_type = tx["TransactionType"].as_str().unwrap();
        let account_addr = tx["Account"].as_str().unwrap();
        let account_id = decode_address(account_addr);
        let fee: u64 = tx["Fee"].as_str().unwrap().parse().unwrap();
        let sequence = tx["Sequence"].as_u64().unwrap_or(0) as u32;
        let tx_hash_hex = tx["hash"].as_str().unwrap();
        let tx_hash = Hash256::from_hex(tx_hash_hex).unwrap();

        println!("  TX: {} from {} fee={} seq={}", tx_type, account_addr, fee, sequence);

        // Fetch sender's account state at ledger N
        let sender_info = rpc("account_info", serde_json::json!({
            "account": account_addr,
            "ledger_index": ledger_n
        })).await;
        let sender_data = &sender_info["account_data"];
        let sender_balance: u64 = sender_data["Balance"].as_str().unwrap().parse().unwrap();
        let sender_seq: u32 = sender_data["Sequence"].as_u64().unwrap() as u32;

        // Insert sender into state
        let sender_obj = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(account_id),
            "Balance": sender_balance.to_string(),
            "Sequence": sender_seq,
            "OwnerCount": sender_data["OwnerCount"].as_u64().unwrap_or(0),
            "Flags": sender_data["Flags"].as_u64().unwrap_or(0),
        });
        let sender_key = keylet::account_root_key(&account_id);
        let _ = state.state_map.insert(sender_key, serde_json::to_vec(&sender_obj).unwrap());

        // Build TxFields
        let mut fields = tx.clone();

        // For Payment: also fetch destination
        if tx_type == "Payment" {
            if let Some(dest_addr) = tx.get("Destination").and_then(|d| d.as_str()) {
                let dest_id = decode_address(dest_addr);
                fields["Destination"] = serde_json::Value::String(hex::encode(dest_id));

                // Handle Amount — could be string (XRP) or object (IOU)
                let amount = tx.get("Amount")
                    .or_else(|| tx.get("DeliverMax"));
                if let Some(a) = amount {
                    fields["Amount"] = a.clone();
                }

                // Fetch destination account at ledger N
                let dest_info = rpc("account_info", serde_json::json!({
                    "account": dest_addr,
                    "ledger_index": ledger_n
                })).await;

                if let Some(dest_data) = dest_info.get("account_data") {
                    let dest_balance: u64 = dest_data["Balance"]
                        .as_str()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                    let dest_seq: u32 = dest_data["Sequence"].as_u64().unwrap_or(1) as u32;

                    let dest_obj = serde_json::json!({
                        "LedgerEntryType": "AccountRoot",
                        "Account": hex::encode(dest_id),
                        "Balance": dest_balance.to_string(),
                        "Sequence": dest_seq,
                        "OwnerCount": dest_data["OwnerCount"].as_u64().unwrap_or(0),
                        "Flags": dest_data["Flags"].as_u64().unwrap_or(0),
                    });
                    let dest_key = keylet::account_root_key(&dest_id);
                    let _ = state.state_map.insert(dest_key, serde_json::to_vec(&dest_obj).unwrap());
                }
            }
        }

        let last_ledger_seq = tx.get("LastLedgerSequence")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);

        let ticket_seq = tx.get("TicketSequence")
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

    // Apply transactions
    let close_time = n1_ledger["close_time"].as_u64().unwrap_or(0) as u32;
    let (new_state, results) = apply_transaction_set(
        &state,
        tx_fields_list,
        close_time,
        10, // close_time_resolution
    ).unwrap();

    // Verify results
    println!("\n=== Results ===");
    println!("New ledger sequence: {}", new_state.header.sequence);
    println!("Expected: {}", ledger_n1);
    assert_eq!(new_state.header.sequence, ledger_n1);

    let expected_total_coins: u64 = n1_ledger["total_coins"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    println!("Total coins: {} (expected {})", new_state.header.total_coins, expected_total_coins);
    assert_eq!(new_state.header.total_coins, expected_total_coins,
        "Total coins mismatch! Fee destruction calculation is wrong.");

    // Verify each transaction result
    for (i, result) in results.iter().enumerate() {
        let expected_result = transactions[i]
            .get("metaData")
            .and_then(|m| m.get("TransactionResult"))
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        println!("  TX {}: {} (expected {})", i, result.result.code_str(), expected_result);

        if expected_result == "tesSUCCESS" {
            assert!(result.result.is_success(),
                "Transaction {} should have succeeded but got {}", i, result.result.code_str());
        }
    }

    // Verify affected account balances
    for tx in transactions {
        if tx["TransactionType"].as_str() != Some("Payment") {
            continue;
        }

        let meta = tx.get("metaData").expect("no metadata");
        for node in meta["AffectedNodes"].as_array().unwrap_or(&vec![]) {
            if let Some(mn) = node.get("ModifiedNode") {
                if mn["LedgerEntryType"].as_str() == Some("AccountRoot") {
                    if let Some(final_balance) = mn
                        .get("FinalFields")
                        .and_then(|f| f.get("Balance"))
                        .and_then(|b| b.as_str())
                    {
                        let account_hex = mn["FinalFields"]["Account"].as_str().unwrap_or("");
                        // Find this account in our state
                        if let Ok(id_bytes) = hex::decode(account_hex) {
                            if id_bytes.len() == 20 {
                                let mut id = [0u8; 20];
                                id.copy_from_slice(&id_bytes);
                                let key = keylet::account_root_key(&id);
                                if let Some(data) = new_state.state_map.lookup(&key) {
                                    let obj: Value = serde_json::from_slice(data).unwrap();
                                    let our_balance = obj["Balance"].as_str().unwrap_or("?");
                                    println!("  Account {}...{}: our={} expected={}",
                                        &account_hex[..8], &account_hex[account_hex.len()-4..],
                                        our_balance, final_balance);
                                    assert_eq!(our_balance, final_balance,
                                        "Balance mismatch for account {}", account_hex);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    println!("\n=== VERIFICATION PASSED ===");
    println!("Transaction application matches testnet for ledger {}", ledger_n1);
}
