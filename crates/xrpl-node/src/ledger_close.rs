//! Ledger close — bridge between live engine (RocksDB/binary) and the
//! xrpl-ledger apply pipeline (LedgerState/JSON).
//!
//! Each round:
//! 1. Collect raw binary transactions
//! 2. Decode them to TxFields
//! 3. Build a LedgerState from affected RocksDB entries (binary→JSON)
//! 4. Run apply_transaction_set()
//! 5. Write modified state back to RocksDB (JSON→binary re-fetch)

use std::sync::Arc;

use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::apply::{apply_transaction_set, AppliedTx};
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::ledger::transactor::TxFields;
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

/// Decode a raw binary transaction into TxFields for the apply pipeline.
/// Uses xrpl_core's binary codec to parse to JSON, then extracts the fields.
pub fn decode_raw_tx(raw_tx: &[u8]) -> Option<(Hash256, TxFields)> {
    let decoded = xrpl_core::codec::decode_transaction_binary(raw_tx).ok()?;

    // Extract Account (as 20-byte ID)
    let account_str = decoded.get("Account")?.as_str()?;
    let account_id = crate::engine::decode_address(account_str)?;

    // Extract tx type
    let tx_type = decoded.get("TransactionType")?.as_str()?.to_string();

    // Extract fee
    let fee: u64 = decoded.get("Fee")?.as_str()?.parse().ok()?;

    // Extract sequence
    let sequence: u32 = decoded.get("Sequence")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    // Extract optional fields
    let last_ledger_seq = decoded.get("LastLedgerSequence")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let ticket_seq = decoded.get("TicketSequence")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    // Compute tx hash
    let tx_hash = {
        use sha2::{Digest, Sha512};
        let prefix: [u8; 4] = [0x54, 0x58, 0x4E, 0x00]; // "TXN\0"
        let mut hasher = Sha512::new();
        hasher.update(&prefix);
        hasher.update(raw_tx);
        let full = hasher.finalize();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&full[..32]);
        Hash256(hash)
    };

    let tx_fields = TxFields {
        account: account_id,
        tx_type,
        fee,
        sequence,
        ticket_seq,
        last_ledger_seq,
        fields: decoded,
    };

    Some((tx_hash, tx_fields))
}

/// Decode a ledger object to JSON.
/// Handles both formats: raw binary (XRPL serialization) and already-JSON.
/// Tolerant of trailing bytes (network objects may have appended index/metadata).
pub fn decode_ledger_object(data: &[u8]) -> Option<serde_json::Value> {
    // Check if it's already JSON (starts with '{')
    if data.first() == Some(&0x7B) {
        // Try direct parse first
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
            return Some(v);
        }
        // JSON might be corrupted — try parsing just the valid prefix
        // (find the last '}' and parse up to there)
        if let Some(end) = data.iter().rposition(|&b| b == 0x7D) {
            return serde_json::from_slice(&data[..=end]).ok();
        }
        return None;
    }
    // Try binary decode — be tolerant of trailing bytes
    // The codec is strict about consuming all bytes, but RocksDB entries
    // from network RPC may have extra data (index, metadata bytes).
    // Strategy: try full, then progressively strip trailing bytes.
    if let Ok(v) = xrpl_core::codec::decode_transaction_binary(data) {
        return Some(v);
    }
    // Try stripping trailing bytes — the object likely ends before the data does
    for trim in [30, 32, 48, 53, 64, 76] {
        if data.len() > trim {
            if let Ok(v) = xrpl_core::codec::decode_transaction_binary(&data[..data.len() - trim]) {
                return Some(v);
            }
        }
    }
    None
}

/// Result of a ledger close operation.
pub struct LedgerCloseResult {
    /// The new ledger hash we computed.
    pub ledger_hash: Hash256,
    /// The account_hash we computed.
    pub account_hash: Hash256,
    /// The transaction_hash we computed.
    pub tx_hash: Hash256,
    /// Applied transactions with results.
    pub applied: Vec<AppliedTx>,
    /// Total fees burned.
    pub total_fees: u64,
    /// Modified keylets (for RocksDB update).
    pub modified_keys: Vec<Hash256>,
    /// New total coins after fee burn.
    pub new_total_coins: u64,
}

/// Close a ledger round: apply all transactions and produce the new state.
///
/// This builds a minimal LedgerState from affected RocksDB entries,
/// runs the full apply pipeline, and returns the resulting hashes.
pub fn close_ledger(
    db: &Arc<rocksdb::DB>,
    raw_transactions: &[(Hash256, Vec<u8>)],
    prev_ledger_seq: u32,
    prev_ledger_hash: [u8; 32],
    total_coins: u64,
    close_time: u32,
    parent_close_time: u32,
) -> Option<LedgerCloseResult> {
    if raw_transactions.is_empty() {
        return None;
    }

    // Step 1: Decode all transactions
    let mut tx_fields: Vec<(Hash256, TxFields)> = Vec::new();
    let mut affected_accounts: std::collections::HashSet<[u8; 20]> = std::collections::HashSet::new();

    for (_orig_hash, raw) in raw_transactions {
        if let Some((tx_hash, fields)) = decode_raw_tx(raw) {
            affected_accounts.insert(fields.account);

            // Also track destinations for Payments
            if fields.tx_type == "Payment" {
                if let Some(dest_str) = fields.fields.get("Destination").and_then(|v| v.as_str()) {
                    if let Some(dest_id) = crate::engine::decode_address(dest_str) {
                        affected_accounts.insert(dest_id);
                    }
                }
            }

            tx_fields.push((tx_hash, fields));
        }
    }

    if tx_fields.is_empty() {
        return None;
    }

    // Step 2: Build LedgerState from affected accounts
    let header = LedgerHeader {
        sequence: prev_ledger_seq,
        total_coins,
        parent_hash: Hash256(prev_ledger_hash),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time,
        close_time: parent_close_time,
        close_time_resolution: 10,
        close_flags: 0,
    };

    let mut state = LedgerState::new_unverified(header);

    // Load affected accounts from RocksDB into the state SHAMap
    let mut loaded = 0;
    let mut failed = 0;
    let mut missing = 0;
    for account_id in &affected_accounts {
        let acct_key = keylet::account_root_key(account_id);
        match db.get(&acct_key.0) {
            Ok(Some(binary_data)) => {
                match decode_ledger_object(&binary_data) {
                    Some(json_obj) => {
                        if let Ok(json_bytes) = serde_json::to_vec(&json_obj) {
                            let _ = state.state_map.insert(acct_key, json_bytes);
                            loaded += 1;
                        } else {
                            failed += 1;
                        }
                    }
                    None => {
                        // Decode failed — log first few with actual error
                        if failed < 3 {
                            let err = if binary_data.first() == Some(&0x7B) {
                                format!("JSON parse: {:?}", serde_json::from_slice::<serde_json::Value>(&binary_data).err())
                            } else {
                                format!("Binary decode: {:?}", xrpl_core::codec::decode_transaction_binary(&binary_data).err())
                            };
                            eprintln!(
                                "[ledger-close] Decode failed key={} len={} err={}",
                                hex::encode(&acct_key.0[..8]),
                                binary_data.len(),
                                err,
                            );
                        }
                        failed += 1;
                    }
                }
            }
            Ok(None) => {
                missing += 1;
            }
            Err(_) => {
                failed += 1;
            }
        }
    }

    if loaded == 0 {
        return None;
    }

    eprintln!(
        "[ledger-close] Loaded {loaded}/{} affected accounts ({failed} decode failures, {missing} missing), applying {} txs",
        affected_accounts.len(), tx_fields.len()
    );

    // Step 3: Run the apply pipeline
    let (new_state, applied) = match apply_transaction_set(&state, tx_fields, close_time, 10) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("[ledger-close] Apply failed: {e:?}");
            return None;
        }
    };

    // Step 4: Extract results
    let total_fees: u64 = applied.iter().map(|a| a.fee).sum();
    let account_hash = new_state.state_map.root_hash();
    let tx_hash = new_state.tx_map.root_hash();
    let ledger_hash = new_state.ledger_hash();

    // Collect modified keylets
    let modified_keys: Vec<Hash256> = affected_accounts
        .iter()
        .map(keylet::account_root_key)
        .collect();

    eprintln!(
        "[ledger-close] Ledger #{} closed: {} txs, {} fees, hash={}",
        prev_ledger_seq + 1,
        applied.len(),
        total_fees,
        hex::encode(&ledger_hash.0[..8]),
    );

    Some(LedgerCloseResult {
        ledger_hash,
        account_hash,
        tx_hash,
        applied,
        total_fees,
        modified_keys,
        new_total_coins: new_state.header.total_coins,
    })
}
