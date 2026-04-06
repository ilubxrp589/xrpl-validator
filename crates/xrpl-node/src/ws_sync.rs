//! Real-time state sync via WebSocket subscription to rippled.
//!
//! Architecture:
//! 1. Subscribe to "ledger" + "transactions" streams
//! 2. Accumulate transaction metadata as txs stream in
//! 3. On ledgerClosed: process the accumulated data, fetch objects, update state
//! 4. For the FIRST ledger after subscribe: use RPC to get full tx metadata
//!    (some txs may have streamed before we subscribed)
//!
//! No gap fill. No historical fetches. Objects hot in rippled's cache.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
use crate::rippled_client::RippledClient;

#[cfg(feature = "ffi")]
type FfiVerifierHandle = Arc<crate::ffi_verifier::FfiVerifier>;
#[cfg(not(feature = "ffi"))]
type FfiVerifierHandle = Arc<()>;

pub async fn start_ws_sync(
    db: Arc<rocksdb::DB>,
    hash_comp: Arc<crate::state_hash::StateHashComputer>,
    last_synced: Arc<AtomicU32>,
    history: Option<Arc<parking_lot::Mutex<crate::history::HistoryStore>>>,
    ffi_verifier: Option<FfiVerifierHandle>,
) {
    let rpc = RippledClient::new();

    loop {
        let ws_url = rpc.ws_url();
        eprintln!("[ws-sync] Connecting to {ws_url}...");
        let ws = match connect_async(ws_url).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("[ws-sync] Connect failed: {e}");
                let next = rpc.next_ws_endpoint();
                eprintln!("[ws-sync] Switching to {next}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        let (mut sink, mut stream) = ws.split();

        // Subscribe to ledger stream only — we fetch tx metadata via RPC
        // (streaming txs arrive BEFORE ledgerClosed, causing timing issues)
        let sub = serde_json::json!({"command":"subscribe","streams":["ledger"]});
        if let Err(e) = sink.send(tokio_tungstenite::tungstenite::Message::Text(sub.to_string())).await {
            eprintln!("[ws-sync] Subscribe failed: {e}");
            continue;
        }
        eprintln!("[ws-sync] Connected + subscribed");

        // Flat cache builds from RocksDB on first call — no invalidation needed.

        // Accumulate tx metadata per ledger
        let mut acc_seq: u32 = 0;
        let mut acc_modified: HashSet<String> = HashSet::new();
        let mut acc_deleted: HashSet<String> = HashSet::new();
        let mut acc_tx_count: u32 = 0;
        let mut first_ledger = true;
        let mut processing = false;
        let mut last_processed: u32 = 0;

        while let Some(msg) = stream.next().await {
            let text = match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(t)) => t,
                Ok(tokio_tungstenite::tungstenite::Message::Ping(data)) => {
                    let _ = sink.send(tokio_tungstenite::tungstenite::Message::Pong(data)).await;
                    continue;
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                    eprintln!("[ws-sync] Server closed connection");
                    break;
                }
                Err(e) => {
                    eprintln!("[ws-sync] Read error: {e}");
                    break;
                }
                _ => continue,
            };

            let body: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let msg_type = body.get("type").and_then(|t| t.as_str()).unwrap_or("");

            // Ledger closed — process ALL ledgers from last_synced+1 to this one
            if msg_type == "ledgerClosed" {
                let closed_seq = body["ledger_index"].as_u64().unwrap_or(0) as u32;
                if closed_seq == 0 { continue; }

                // Fill gap from last_processed+1 to closed_seq
                // SECURITY(5.3): Use Acquire ordering to ensure visibility of prior stores
                let mut start_seq = if last_processed > 0 { last_processed + 1 } else { last_synced.load(Ordering::Acquire) + 1 };

                // If behind, skip to current. DON'T invalidate the tree — it's correct.
                // Gap ledgers are processed via fast path (tree update, no hash check).
                // Only the final ledger gets a hash check.
                if closed_seq > start_seq + 1 {
                    let gap = closed_seq - start_seq;
                    eprintln!("[ws-sync] {gap} ledgers behind — processing gap via fast path");
                }

                for process_seq in start_seq..=closed_seq {
                    let account_hash = if process_seq == closed_seq {
                        fetch_account_hash(&rpc, process_seq).await.unwrap_or_default()
                    } else {
                        String::new()
                    };

                    acc_modified.clear();
                    acc_deleted.clear();
                    acc_tx_count = 0;

                    let mut meta_ok = false;
                    let mut ledger_header: LedgerHeader = LedgerHeader::default();
                    for attempt in 0..3u32 {
                        match fetch_ledger_metadata(&rpc, process_seq, &mut acc_modified, &mut acc_deleted, &mut acc_tx_count).await {
                            Ok((_sorted_txs, hdr)) => {
                                ledger_header = hdr;
                                meta_ok = true;
                                break;
                            }
                            Err(e) => {
                                if attempt < 2 {
                                    eprintln!("[ws-sync] Metadata #{process_seq} attempt {}: {e} — retry", attempt + 1);
                                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                    acc_modified.clear();
                                    acc_deleted.clear();
                                    acc_tx_count = 0;
                                } else {
                                    // Ledger too old — skip rest of batch, let next ledgerClosed catch up
                                    eprintln!("[ws-sync] Metadata #{process_seq} FAILED — breaking batch");
                                    break;
                                }
                            }
                        }
                    }
                    // FFI independent verification — blocking, spawn off-thread.
                    // Use OwnedSnapshot when DB is caught up (steady state),
                    // fall back to pure RPC during catchup (stale DB = crash).
                    #[cfg(feature = "ffi")]
                    if meta_ok {
                        if let Some(ref verifier) = ffi_verifier {
                            let tx_blobs = fetch_ledger_tx_blobs(&rpc, process_seq).await;
                            if !tx_blobs.is_empty() {
                                let v = verifier.clone();
                                let hdr = ledger_header.clone();
                                let seq = process_seq;
                                // Steady state = last successful DB write is
                                // within 2 ledgers of what we're verifying.
                                let synced = last_synced.load(Ordering::Acquire);
                                let steady = synced > 0 && process_seq.saturating_sub(synced) <= 2;
                                if steady {
                                    let snap = std::sync::Arc::new(
                                        crate::ffi_engine::OwnedSnapshot::new(db.clone())
                                    );
                                    tokio::task::spawn_blocking(move || {
                                        v.verify_ledger_with_snapshot(
                                            seq, &tx_blobs,
                                            hdr.parent_hash, hdr.parent_close_time, hdr.total_drops,
                                            Some(snap.as_ref()),
                                        );
                                    });
                                } else {
                                    tokio::task::spawn_blocking(move || {
                                        v.verify_ledger(
                                            seq, &tx_blobs,
                                            hdr.parent_hash, hdr.parent_close_time, hdr.total_drops,
                                        );
                                    });
                                }
                            }
                        }
                    }
                    #[cfg(not(feature = "ffi"))]
                    let _ = (&ffi_verifier, &ledger_header);
                    if !meta_ok {
                        // Break the entire batch — let the next ledgerClosed event
                        // trigger skip-ahead if we're too far behind
                        last_processed = process_seq;
                        break;
                    }

                    // Always include protocol-level singletons
                    acc_modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string()); // LedgerHashes (skip list)
                    acc_modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string()); // NegativeUNL
                    acc_modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string()); // Amendments
                    acc_modified.insert("4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651".to_string()); // FeeSettings
                    // LedgerHashes sub-page: key = SHA512Half(0x0073 || uint32_be(seq/65536))
                    // Changes every ledger (new hash appended), rolls to new key every 65536 ledgers
                    {
                        use sha2::{Sha512, Digest};
                        let group = process_seq / 65536;
                        let mut data = vec![0x00u8, 0x73];
                        data.extend_from_slice(&group.to_be_bytes());
                        let hash = Sha512::digest(&data);
                        acc_modified.insert(hex::encode(&hash[..32]).to_uppercase());
                    }

                    // Final cleanup
                    for d in acc_deleted.iter().cloned().collect::<Vec<_>>() {
                        acc_modified.remove(&d);
                    }

                    // Only hash-check the last ledger in the batch — gap ledgers use fast path
                    let compute_hash = process_seq == closed_seq;
                    let result = process_ledger(
                        &rpc, &db, &hash_comp,
                        process_seq, &acc_modified, &acc_deleted,
                        &account_hash, acc_tx_count, compute_hash,
                        &history,
                    ).await;

                    last_processed = process_seq;
                    if result {
                        // SECURITY(5.3): Use Release ordering so Acquire loads see this
                        last_synced.store(process_seq, Ordering::Release);
                    }
                } // end for process_seq

                first_ledger = false;
                acc_seq = closed_seq + 1;
            }
        }

        eprintln!("[ws-sync] Disconnected. Reconnecting in 3s...");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

fn extract_affected_nodes(body: &serde_json::Value, modified: &mut HashSet<String>, deleted: &mut HashSet<String>) {
    let meta = if body["meta"].is_object() { &body["meta"] }
        else if body["metaData"].is_object() { &body["metaData"] }
        else { return; };

    if let Some(nodes) = meta["AffectedNodes"].as_array() {
        for node in nodes {
            if let Some(c) = node.get("CreatedNode") {
                if let Some(i) = c["LedgerIndex"].as_str() {
                    modified.insert(i.to_string());
                    deleted.remove(i);
                }
            }
            if let Some(m) = node.get("ModifiedNode") {
                if let Some(i) = m["LedgerIndex"].as_str() {
                    // Guard: don't re-add if already marked deleted
                    if !deleted.contains(i) {
                        modified.insert(i.to_string());
                    }
                }
            }
            if let Some(d) = node.get("DeletedNode") {
                if let Some(i) = d["LedgerIndex"].as_str() {
                    deleted.insert(i.to_string());
                    modified.remove(i);
                }
            }
        }
    }
}

/// Ledger header fields needed by FFI apply.
#[derive(Debug, Clone, Default)]
pub struct LedgerHeader {
    pub parent_hash: [u8; 32],
    pub parent_close_time: u32,
    pub total_drops: u64,
}

async fn fetch_ledger_metadata(
    rpc: &RippledClient, seq: u32,
    modified: &mut HashSet<String>, deleted: &mut HashSet<String>,
    tx_count: &mut u32,
) -> Result<(Vec<serde_json::Value>, LedgerHeader), String> {
    let body = rpc.call("ledger", serde_json::json!({"ledger_index": seq, "transactions": true, "expand": true, "binary": false})).await?;
    let ledger = &body["result"]["ledger"];
    let txs = ledger["transactions"].as_array()
        .ok_or("no transactions")?;

    // Extract header fields needed for FFI apply
    let parent_hash = ledger["parent_hash"].as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| {
            if b.len() == 32 {
                let mut a = [0u8; 32];
                a.copy_from_slice(&b);
                Some(a)
            } else {
                None
            }
        })
        .unwrap_or([0u8; 32]);
    let parent_close_time = ledger["parent_close_time"].as_u64().unwrap_or(0) as u32;
    let total_drops = ledger["total_coins"].as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0u64);
    let header = LedgerHeader { parent_hash, parent_close_time, total_drops };

    // Sort by TransactionIndex (execution order) — array order can differ!
    let mut sorted_txs: Vec<serde_json::Value> = txs.iter().cloned().collect();
    sorted_txs.sort_by_key(|tx| {
        let meta = if tx["meta"].is_object() { &tx["meta"] }
            else if tx["metaData"].is_object() { &tx["metaData"] }
            else { return 0u64; };
        meta["TransactionIndex"].as_u64().unwrap_or(0)
    });

    for tx in &sorted_txs {
        extract_affected_nodes(tx, modified, deleted);
        *tx_count += 1;
    }
    Ok((sorted_txs, header))
}

/// Fetch binary tx_blobs for every tx in the ledger, sorted by TransactionIndex.
/// Used only when FFI verification is enabled — adds one extra RPC per ledger.
/// Returns an empty Vec on error (FFI verification skipped for this ledger).
#[cfg(feature = "ffi")]
async fn fetch_ledger_tx_blobs(rpc: &RippledClient, seq: u32) -> Vec<Vec<u8>> {
    let body = match rpc.call(
        "ledger",
        serde_json::json!({"ledger_index": seq, "transactions": true, "expand": true, "binary": true}),
    ).await {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let empty = Vec::new();
    let txs = body["result"]["ledger"]["transactions"].as_array().unwrap_or(&empty);
    // Pair each blob with its TransactionIndex extracted from the meta blob.
    // Meta prefix: sfTransactionIndex = 0x20 0x1C + u32 big-endian.
    // Pseudo-tx type IDs (network-generated, no signature, skip FFI verify)
    const PSEUDO_TX_TYPES: [u16; 3] = [100, 101, 102]; // EnableAmendment, SetFee, UNLModify
    let mut ordered: Vec<(u32, Vec<u8>)> = Vec::with_capacity(txs.len());
    for tx in txs.iter() {
        let tx_blob_hex = tx["tx_blob"].as_str().or_else(|| tx.as_str());
        let Some(hex_str) = tx_blob_hex else { continue; };
        let Ok(bytes) = hex::decode(hex_str) else { continue; };
        // Skip pseudo-txs: first byte 0x12 (sfTransactionType), next 2 bytes = type u16
        if bytes.len() >= 3 && bytes[0] == 0x12 {
            let tt = u16::from_be_bytes([bytes[1], bytes[2]]);
            if PSEUDO_TX_TYPES.contains(&tt) { continue; }
        }
        let meta_hex = tx["meta"].as_str().unwrap_or("");
        let idx = if meta_hex.len() >= 12 {
            let meta_bytes = hex::decode(&meta_hex[..12]).unwrap_or_default();
            if meta_bytes.len() >= 6 && meta_bytes[0] == 0x20 && meta_bytes[1] == 0x1C {
                u32::from_be_bytes([meta_bytes[2], meta_bytes[3], meta_bytes[4], meta_bytes[5]])
            } else {
                u32::MAX
            }
        } else {
            u32::MAX
        };
        ordered.push((idx, bytes));
    }
    ordered.sort_by_key(|(i, _)| *i);
    ordered.into_iter().map(|(_, b)| b).collect()
}

async fn process_ledger(
    rpc: &RippledClient,
    db: &Arc<rocksdb::DB>,
    hash_comp: &Arc<crate::state_hash::StateHashComputer>,
    seq: u32,
    modified: &HashSet<String>,
    deleted: &HashSet<String>,
    account_hash: &str,
    tx_count: u32,
    compute_hash: bool,
    history: &Option<Arc<parking_lot::Mutex<crate::history::HistoryStore>>>,
) -> bool {
    let ledger_start = std::time::Instant::now();

    // Fetch all modified objects in parallel — 32 concurrent requests max.
    // Previous sequential approach: 500 objs × 3ms = 1.5s
    // Parallel with 32 concurrent: 16 batches × 3ms = ~50ms
    let semaphore = Arc::new(tokio::sync::Semaphore::new(64));
    let rpc_url = rpc.rpc_url().to_string();
    let client = rpc.client.clone();

    let fetch_futures: Vec<_> = modified.iter().map(|index_hex| {
        let sem = semaphore.clone();
        let client = client.clone();
        let url = rpc_url.clone();
        let index_hex = index_hex.clone();
        async move {
            let _permit = sem.acquire().await.ok()?;
            for attempt in 0..3u32 {
                let resp = client.post(&url)
                    .json(&serde_json::json!({
                        "method": "ledger_entry",
                        "params": [{"index": &index_hex, "binary": true, "ledger_index": seq}]
                    }))
                    .send().await;

                if let Ok(r) = resp {
                    if let Ok(body) = r.json::<serde_json::Value>().await {
                        let resp_seq = body["result"]["ledger_index"].as_u64().unwrap_or(0) as u32;
                        if resp_seq != 0 && resp_seq != seq {
                            if attempt < 2 { continue; }
                            return None;
                        }
                        if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                            if let Ok(data) = hex::decode(data_hex) {
                                if let Ok(kb) = hex::decode(&index_hex) {
                                    if kb.len() == 32 {
                                        let mut key = [0u8; 32];
                                        key.copy_from_slice(&kb);
                                        return Some((key, data));
                                    }
                                }
                            }
                        }
                        if body["result"]["error"].as_str() == Some("entryNotFound") {
                            if let Ok(kb) = hex::decode(&index_hex) {
                                if kb.len() == 32 {
                                    let mut key = [0u8; 32];
                                    key.copy_from_slice(&kb);
                                    return Some((key, Vec::new()));
                                }
                            }
                        }
                    }
                }
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
            None
        }
    }).collect();

    let results = futures_util::future::join_all(fetch_futures).await;
    let mut fetched_data: Vec<([u8; 32], Vec<u8>)> = Vec::with_capacity(results.len());
    let mut failed = 0u32;
    for r in results {
        match r {
            Some(pair) => fetched_data.push(pair),
            None => failed += 1,
        }
    }

    if failed > 0 {
        eprintln!("[ws-sync] Ledger #{seq}: {failed} failures — invalidating tree for rebuild");
        hash_comp.invalidate_tree();
    }

    // Atomic write to RocksDB
    let mut batch = rocksdb::WriteBatch::default();
    let mut keys: Vec<Hash256> = Vec::new();

    for (key, data) in &fetched_data {
        if data.is_empty() {
            // entryNotFound on both local + public — delete stale entry
            batch.delete(key);
        } else {
            batch.put(key, data);
        }
        keys.push(Hash256(*key));
    }
    for index_hex in deleted {
        if let Ok(kb) = hex::decode(index_hex) {
            if kb.len() == 32 {
                batch.delete(&kb);
                let mut k = Hash256([0u8; 32]);
                k.0.copy_from_slice(&kb);
                keys.push(k);
            }
        }
    }

    if let Err(e) = db.write(batch) {
        eprintln!("[ws-sync] Ledger #{seq}: DB write failed: {e}");
        return false;
    }

    if compute_hash {
        // Run on blocking thread pool — keeps SSE feed + peers responsive during 2.5s hash
        let hc = hash_comp.clone();
        let d = db.clone();
        let hash_result = tokio::task::spawn_blocking(move || {
            hc.update_and_hash(&d, &keys)
        }).await.ok().flatten();
        if let Some(root) = hash_result {
            let ours = hex::encode(root.0);
            if !account_hash.is_empty() {
                let matched = ours.to_uppercase() == account_hash.to_uppercase();
                hash_comp.set_network_hash(account_hash, seq);
                let round_time = ledger_start.elapsed().as_secs_f64();
                hash_comp.push_sync_log(crate::state_hash::SyncLogEntry {
                    seq, matched, txs: tx_count, objs: fetched_data.len() as u32,
                    time_secs: round_time, healed: false,
                });
                if let Some(ref hist) = history {
                    hist.lock().record(&crate::history::LedgerRound {
                        seq, matched, healed: false, txs: tx_count,
                        objs: fetched_data.len() as u32,
                        round_time_ms: (round_time * 1000.0) as u32,
                        compute_time_ms: 0, peers: 0,
                        timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
                    });
                }
                if matched {
                    eprintln!("[ws-sync] #{seq}: MATCH ({tx_count} txs, {} objs)", fetched_data.len());
                } else {
                    eprintln!("[ws-sync] #{seq}: MISMATCH ({tx_count} txs) — self-healing retry...",);

                    // SELF-HEALING: re-fetch ALL modified+singleton objects and recompute
                    // Likely cause: transient stale data from rippled under load
                    let mut healed = false;
                    for retry in 0..2u32 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let mut retry_batch = rocksdb::WriteBatch::default();
                        let mut retry_keys: Vec<Hash256> = Vec::new();
                        let mut retry_ok = true;

                        for index_hex in modified.iter().chain(deleted.iter()) {
                            let resp = rpc.client.post(rpc.rpc_url())
                                .json(&serde_json::json!({
                                    "method": "ledger_entry",
                                    "params": [{"index": index_hex, "binary": true, "ledger_index": seq}]
                                }))
                                .send().await;
                            if let Ok(r) = resp {
                                if let Ok(body) = r.json::<serde_json::Value>().await {
                                    if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                                        if let (Ok(data), Ok(kb)) = (hex::decode(data_hex), hex::decode(index_hex)) {
                                            if kb.len() == 32 {
                                                let mut key = [0u8; 32];
                                                key.copy_from_slice(&kb);
                                                retry_batch.put(&key, &data);
                                                retry_keys.push(Hash256(key));
                                            }
                                        }
                                    } else if body["result"]["error"].as_str() == Some("entryNotFound") {
                                        if let Ok(kb) = hex::decode(index_hex) {
                                            if kb.len() == 32 {
                                                retry_batch.delete(&kb);
                                                let mut k = Hash256([0u8; 32]);
                                                k.0.copy_from_slice(&kb);
                                                retry_keys.push(k);
                                            }
                                        }
                                    } else {
                                        retry_ok = false;
                                        break;
                                    }
                                }
                            } else {
                                retry_ok = false;
                                break;
                            }
                        }

                        if !retry_ok || retry_keys.is_empty() { continue; }
                        if let Err(e) = db.write(retry_batch) {
                            eprintln!("[ws-sync] Retry DB write failed: {e}");
                        }

                        // Recompute hash with fresh data
                        let hc2 = hash_comp.clone(); let d2 = db.clone();
                        let root2 = tokio::task::spawn_blocking(move || {
                            hc2.update_and_hash(&d2, &retry_keys)
                        }).await.ok().flatten();
                        if let Some(root2) = root2 {
                            let ours2 = hex::encode(root2.0);
                            let matched2 = ours2.to_uppercase() == account_hash.to_uppercase();
                            hash_comp.set_network_hash(account_hash, seq);
                            if matched2 {
                                eprintln!("[ws-sync] #{seq}: HEALED on retry {} — re-fetch fixed it", retry + 1);
                                healed = true;
                                break;
                            } else {
                                eprintln!("[ws-sync] #{seq}: retry {} still mismatches ours={}", retry + 1, &ours2[..16]);
                            }
                        }
                    }

                    if !healed {
                        eprintln!("[ws-sync] #{seq}: MISMATCH persists — FULL SCAN starting...");
                        // Full scan: compare every key in ledger_data against our DB
                        let mut diag_marker: Option<String> = None;
                        let mut n_diff = 0u32;
                        let mut n_missing = 0u32;
                        let mut n_total = 0u64;
                        loop {
                            let mut p = serde_json::json!({"ledger_index":seq,"binary":true,"limit":2048});
                            if let Some(ref m) = diag_marker { p["marker"] = serde_json::Value::String(m.clone()); }
                            let Ok(r) = rpc.client.post(rpc.rpc_url())
                                .json(&serde_json::json!({"method":"ledger_data","params":[p]}))
                                .send().await else { break };
                            let Ok(body) = r.json::<serde_json::Value>().await else { break };
                            if let Some(objs) = body["result"]["state"].as_array() {
                                for obj in objs {
                                    if let (Some(idx), Some(net_data)) = (obj["index"].as_str(), obj["data"].as_str()) {
                                        if let Ok(kb) = hex::decode(idx) {
                                            if kb.len() == 32 {
                                                n_total += 1;
                                                match db.get(&kb) {
                                                    Ok(Some(ours)) => {
                                                        if let Ok(nd) = hex::decode(net_data) {
                                                            if ours.as_ref() as &[u8] != nd.as_slice() {
                                                                n_diff += 1;
                                                                if n_diff <= 5 {
                                                                    eprintln!("[ws-sync] DIFF {} ours={}b net={}b", idx, ours.len(), nd.len());
                                                                }
                                                            }
                                                        }
                                                    }
                                                    Ok(None) => {
                                                        n_missing += 1;
                                                        if n_missing <= 5 { eprintln!("[ws-sync] MISSING {}", idx); }
                                                    }
                                                    Err(_) => {}
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            diag_marker = body["result"]["marker"].as_str().map(String::from);
                            if diag_marker.is_none() { break; }
                            if n_total % 2_000_000 < 2048 {
                                eprintln!("[ws-sync] SCAN {n_total}... ({n_diff} diff, {n_missing} missing)");
                            }
                        }
                        // Also count extra keys in our DB
                        let snap = db.snapshot();
                        let our_count = snap.iterator(rocksdb::IteratorMode::Start)
                            .filter(|i| i.as_ref().map(|(k,_)| k.len() == 32).unwrap_or(false))
                            .count() as u64;
                        let extra = our_count.saturating_sub(n_total);
                        eprintln!("[ws-sync] SCAN DONE: net={n_total} ours={our_count} extra={extra} | {n_diff} diff, {n_missing} missing");
                        // Only run full scan once — subsequent mismatches just log
                        // (the scan takes ~5min and blocks sync)
                    }
                    if healed {
                        let heal_time = ledger_start.elapsed().as_secs_f64();
                        hash_comp.push_sync_log(crate::state_hash::SyncLogEntry {
                            seq, matched: true, txs: tx_count, objs: fetched_data.len() as u32,
                            time_secs: heal_time, healed: true,
                        });
                        if let Some(ref hist) = history {
                            hist.lock().record(&crate::history::LedgerRound {
                                seq, matched: true, healed: true, txs: tx_count,
                                objs: fetched_data.len() as u32,
                                round_time_ms: (heal_time * 1000.0) as u32,
                                compute_time_ms: 0, peers: 0,
                                timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
                            });
                        }
                    }
                    return healed;
                }
                return matched;
            }
        }
    } else {
        // Fast path: update flat cache via merge (no hash computation)
        use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};
        use xrpl_ledger::shamap::node::nibble_at;
        let mut cache = hash_comp.leaf_cache.lock();
        if !cache.is_empty() {
            let mut updates: Vec<(Hash256, Option<Hash256>)> = Vec::with_capacity(keys.len());
            for key in &keys {
                match db.get(&key.0) {
                    Ok(Some(data)) => {
                        let mut buf = Vec::with_capacity(data.len() + 32);
                        buf.extend_from_slice(&data);
                        buf.extend_from_slice(&key.0);
                        updates.push((*key, Some(sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf))));
                    }
                    Ok(None) => updates.push((*key, None)),
                    Err(_) => {}
                }
            }
            updates.sort_unstable_by(|a, b| a.0 .0.cmp(&b.0 .0));
            let mut nc = Vec::with_capacity(cache.len() + updates.len());
            let (mut ci, mut ui) = (0, 0);
            while ci < cache.len() || ui < updates.len() {
                if ui >= updates.len() { nc.extend_from_slice(&cache[ci..]); break; }
                else if ci >= cache.len() { for u in &updates[ui..] { if let Some(h) = u.1 { nc.push((u.0, h)); } } break; }
                else if cache[ci].0 .0 < updates[ui].0 .0 { nc.push(cache[ci]); ci += 1; }
                else if cache[ci].0 .0 > updates[ui].0 .0 { if let Some(h) = updates[ui].1 { nc.push((updates[ui].0, h)); } ui += 1; }
                else { if let Some(h) = updates[ui].1 { nc.push((updates[ui].0, h)); } ci += 1; ui += 1; }
            }
            *cache = nc;
        }
        eprintln!("[ws-sync] #{seq}: synced ({tx_count} txs, {} objs)", fetched_data.len());
    }
    false
}

async fn fetch_account_hash(rpc: &RippledClient, seq: u32) -> Option<String> {
    let body = rpc.call("ledger", serde_json::json!({"ledger_index": seq})).await.ok()?;
    body["result"]["ledger"]["account_hash"].as_str().map(String::from)
}
