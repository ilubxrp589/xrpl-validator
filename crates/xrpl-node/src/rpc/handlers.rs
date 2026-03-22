//! JSON-RPC request handlers.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::mempool::queue::MempoolEntry;
use crate::mempool::{validate_transaction, TransactionQueue, ValidationResult};

/// Shared application state for RPC handlers.
pub struct AppState {
    pub mempool: Arc<TransactionQueue>,
    pub start_time: Instant,
    pub peer_count: Arc<std::sync::atomic::AtomicUsize>,
    pub latest_ledger: Arc<parking_lot::RwLock<Option<u32>>>,
}

/// Route a JSON-RPC method to the appropriate handler.
pub fn handle_rpc(method: &str, params: &Value, state: &AppState) -> Value {
    match method {
        "submit" => handle_submit(params, state),
        "server_info" => handle_server_info(state),
        "fee" => handle_fee(),
        "ledger" => handle_ledger(state),
        "peers" => handle_peers(state),
        _ => json!({
            "error": "unknownCmd",
            "error_message": format!("Unknown method: {method}"),
        }),
    }
}

fn handle_submit(params: &Value, state: &AppState) -> Value {
    let tx_blob_hex = params
        .get(0)
        .and_then(|p| p.get("tx_blob"))
        .and_then(|v| v.as_str())
        .or_else(|| params.get("tx_blob").and_then(|v| v.as_str()));

    let tx_blob_hex = match tx_blob_hex {
        Some(h) => h,
        None => {
            return json!({
                "error": "invalidParams",
                "error_message": "Missing tx_blob parameter",
            });
        }
    };

    let tx_blob = match hex::decode(tx_blob_hex) {
        Ok(b) => b,
        Err(e) => {
            return json!({
                "error": "invalidParams",
                "error_message": format!("Invalid hex in tx_blob: {e}"),
            });
        }
    };

    let (result, tx_hash, fields) = validate_transaction(&tx_blob);
    let hash_hex = hex::encode_upper(tx_hash.0);

    match result {
        ValidationResult::Valid => {
            let entry = MempoolEntry {
                tx_blob: tx_blob.clone(),
                tx_hash,
                fee: fields.as_ref().map(|f| f.fee).unwrap_or(0),
                added_at: Instant::now(),
                last_ledger_seq: fields.as_ref().and_then(|f| f.last_ledger_seq),
            };

            let added = state.mempool.add(entry);

            json!({
                "status": "success",
                "engine_result": if added { "tesSUCCESS" } else { "tefPAST_SEQ" },
                "engine_result_code": if added { 0 } else { -190 },
                "engine_result_message": if added { "The transaction was applied." } else { "Duplicate transaction." },
                "tx_blob": tx_blob_hex,
                "tx_hash": hash_hex,
            })
        }
        ValidationResult::Invalid(reason) => {
            json!({
                "status": "error",
                "engine_result": "temMALFORMED",
                "engine_result_code": -299,
                "engine_result_message": reason,
                "tx_hash": hash_hex,
            })
        }
    }
}

fn handle_server_info(state: &AppState) -> Value {
    let uptime = state.start_time.elapsed().as_secs();
    let peers = state.peer_count.load(std::sync::atomic::Ordering::Relaxed);
    let ledger = state.latest_ledger.read().unwrap_or(0);

    json!({
        "status": "success",
        "info": {
            "build_version": "xrpl-node-rs/0.1.0",
            "server_state": "connected",
            "peers": peers,
            "validated_ledger": {
                "seq": ledger,
            },
            "uptime": uptime,
            "mempool_size": state.mempool.len(),
        }
    })
}

fn handle_fee() -> Value {
    json!({
        "status": "success",
        "current_ledger_size": 0,
        "expected_ledger_size": 0,
        "drops": {
            "base_fee": "10",
            "median_fee": "5000",
            "minimum_fee": "10",
            "open_ledger_fee": "10",
        }
    })
}

fn handle_ledger(state: &AppState) -> Value {
    let ledger = state.latest_ledger.read().unwrap_or(0);
    json!({
        "status": "success",
        "ledger_index": ledger,
        "closed": true,
        "validated": true,
    })
}

fn handle_peers(state: &AppState) -> Value {
    let peers = state.peer_count.load(std::sync::atomic::Ordering::Relaxed);
    json!({
        "status": "success",
        "peers": peers,
    })
}
