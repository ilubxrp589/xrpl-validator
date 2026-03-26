//! Incremental ledger state sync via RPC.
//!
//! After each ledger close, fetches transaction metadata to discover ALL
//! state objects that changed (accounts, offers, trust lines, directories, etc.),
//! then fetches each changed object in binary and writes it to RocksDB.
//! This keeps the local state tree in sync with the network without
//! re-downloading all ~30M objects.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use parking_lot::Mutex;

#[derive(Clone, Default, serde::Serialize)]
pub struct SweepStats {
    pub swept: u64,
    pub updated: u64,
    pub total_keys: u64,
    pub running: bool,
    pub rate: f64,
}

#[derive(Clone, Default, serde::Serialize)]
pub struct IncrementalSyncStats {
    pub ledger_seq: u32,
    pub objects_fetched: u64,
    pub objects_deleted: u64,
    pub objects_failed: u64,
    pub total_synced: u64,
    pub total_rounds: u64,
    pub running: bool,
    pub sweep: SweepStats,
}

pub struct IncrementalSyncer {
    pub stats: Arc<Mutex<IncrementalSyncStats>>,
    running: Arc<AtomicBool>,
    total_synced: Arc<AtomicU64>,
    consecutive_errors: Arc<AtomicU64>,
}

impl IncrementalSyncer {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(Mutex::new(IncrementalSyncStats::default())),
            running: Arc::new(AtomicBool::new(false)),
            total_synced: Arc::new(AtomicU64::new(0)),
            consecutive_errors: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Background sweep: iterate ALL RocksDB keys and re-fetch each one at the
    /// current validated ledger. This fixes the franken-state from bulk sync
    /// (where different keys were downloaded at different ledger versions).
    /// Progress is persisted to disk so restarts resume where they left off.
    pub fn start_sweep(&self, db: Arc<rocksdb::DB>, data_dir: &str) {
        let cursor_path = std::path::PathBuf::from(data_dir).join("sweep_cursor.bin");
        let done_path = std::path::PathBuf::from(data_dir).join("sweep_done");

        // If sweep already completed, skip entirely
        if done_path.exists() {
            eprintln!("[sweep] Already completed (marker exists). Skipping.");
            return;
        }

        // Load resume cursor if it exists
        let resume_key: Option<Vec<u8>> = std::fs::read(&cursor_path).ok().filter(|k| k.len() == 32);
        if let Some(ref k) = resume_key {
            eprintln!("[sweep] Resuming from cursor {}", hex::encode(k));
        }

        let stats = self.stats.clone();

        // Count total keys first
        let total: u64 = db.property_value("rocksdb.estimate-num-keys")
            .ok().flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        {
            let mut s = stats.lock();
            s.sweep.running = true;
            s.sweep.total_keys = total;
        }

        eprintln!("[sweep] Starting background sweep of ~{total} keys...");

        let cursor_path_clone = cursor_path.clone();
        let done_path_clone = done_path.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sweep runtime");

            rt.block_on(async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(8))
                    .build()
                    .expect("sweep client");

                // Use multiple RPC endpoints to spread load
                let endpoints = [
                    "https://s1.ripple.com:51234",
                    "http://127.0.0.1:5005",
                    "https://xrplcluster.com",
                ];

                // Pin ALL fetches to one specific ledger sequence — this is the key
                // to getting a consistent state hash. "validated" is a moving target.
                let pinned_seq: u32 = match client.post(endpoints[0])
                    .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
                    .send().await
                    .and_then(|r| Ok(r))
                {
                    Ok(resp) => {
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            body["result"]["ledger_index"].as_u64()
                                .or_else(|| body["result"]["ledger"]["ledger_index"].as_u64())
                                .unwrap_or(0) as u32
                        } else { 0 }
                    }
                    Err(_) => 0,
                };
                if pinned_seq == 0 {
                    eprintln!("[sweep] FATAL: could not fetch validated ledger seq — aborting sweep");
                    let mut s = stats.lock();
                    s.sweep.running = false;
                    return;
                }
                eprintln!("[sweep] Pinning all fetches to ledger #{pinned_seq}");

                let mut swept: u64 = 0;
                let mut updated: u64 = 0;
                let start = std::time::Instant::now();
                let mut batch_keys: Vec<Vec<u8>> = Vec::new();
                let mut last_key: Option<Vec<u8>> = None;

                // Iterate keys — resume from cursor if available
                let iter = if let Some(ref cursor) = resume_key {
                    db.iterator(rocksdb::IteratorMode::From(cursor, rocksdb::Direction::Forward))
                } else {
                    db.iterator(rocksdb::IteratorMode::Start)
                };

                for item in iter {
                    let (key, _val) = match item {
                        Ok(kv) => kv,
                        Err(_) => continue,
                    };
                    if key.len() != 32 { continue; }
                    last_key = Some(key.to_vec());
                    batch_keys.push(key.to_vec());

                    if batch_keys.len() >= 250 {
                        let fetched = fetch_batch(
                            &client,
                            &endpoints,
                            &batch_keys,
                            &db,
                            pinned_seq,
                        ).await;
                        swept += batch_keys.len() as u64;
                        updated += fetched;
                        batch_keys.clear();

                        if swept % 10000 == 0 {
                            // Save cursor to disk every 10k keys
                            if let Some(ref lk) = last_key {
                                let _ = std::fs::write(&cursor_path_clone, lk);
                            }

                            let elapsed = start.elapsed().as_secs_f64();
                            let rate = swept as f64 / elapsed;
                            let remaining = if rate > 0.0 {
                                (total.saturating_sub(swept)) as f64 / rate
                            } else { 0.0 };
                            eprintln!(
                                "[sweep] {swept}/{total} ({:.1}%) — {updated} updated — {rate:.0}/s — ~{:.0}min left",
                                swept as f64 / total as f64 * 100.0,
                                remaining / 60.0,
                            );
                            let mut s = stats.lock();
                            s.sweep.swept = swept;
                            s.sweep.updated = updated;
                            s.sweep.rate = rate;
                        }
                    }
                }

                // Final batch
                if !batch_keys.is_empty() {
                    let fetched = fetch_batch(&client, &endpoints, &batch_keys, &db, pinned_seq).await;
                    swept += batch_keys.len() as u64;
                    updated += fetched;
                }

                let elapsed = start.elapsed().as_secs_f64();
                eprintln!("[sweep] DONE — {swept} keys swept, {updated} updated in {:.0}s", elapsed);

                // Mark sweep as complete — won't run again
                let _ = std::fs::write(&done_path_clone, format!("{swept} keys, {updated} updated"));
                // Remove cursor file — no longer needed
                let _ = std::fs::remove_file(&cursor_path_clone);

                let mut s = stats.lock();
                s.sweep.swept = swept;
                s.sweep.updated = updated;
                s.sweep.running = false;
                s.sweep.rate = swept as f64 / elapsed;
            });
        });
    }

    /// Sync all state changes from ledger `seq` into `db`, then update the SHAMap.
    /// Call this after each ledger close.
    pub fn sync_ledger(
        &self,
        seq: u32,
        db: Arc<rocksdb::DB>,
        hash_comp: Arc<crate::state_hash::StateHashComputer>,
    ) {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // Already syncing — skip this round
        }

        // Exponential backoff: if errors are stacking up, skip rounds to let rate limits reset.
        // Skip 1 round per consecutive error (up to 60 rounds = ~3-4 minutes of silence).
        let errs = self.consecutive_errors.load(Ordering::Relaxed);
        if errs > 0 && seq % (errs.min(60) as u32 + 1) != 0 {
            self.running.store(false, Ordering::SeqCst);
            return;
        }

        let stats = self.stats.clone();
        let running = self.running.clone();
        let total_synced = self.total_synced.clone();
        let cons_errors = self.consecutive_errors.clone();

        tokio::spawn(async move {
            let result = sync_ledger_state(seq, &db).await;

            let mut s = stats.lock();
            s.ledger_seq = seq;
            s.total_rounds += 1;

            match result {
                Ok((fetched, deleted, failed, modified_keys)) => {
                    // Reset error backoff on ANY success
                    cons_errors.store(0, Ordering::Relaxed);

                    s.objects_fetched = fetched;
                    s.objects_deleted = deleted;
                    s.objects_failed = failed;
                    s.total_synced += fetched + deleted;
                    total_synced.fetch_add(fetched + deleted, Ordering::Relaxed);

                    if !modified_keys.is_empty() && hash_comp.is_ready() {
                        hash_comp.update_round(&db, &modified_keys);
                    }

                    if fetched + deleted > 0 {
                        eprintln!(
                            "[inc-sync] Ledger #{seq}: {fetched} updated, {deleted} deleted, {failed} failed"
                        );
                    }
                }
                Err(e) => {
                    let errs = cons_errors.fetch_add(1, Ordering::Relaxed) + 1;
                    s.objects_failed += 1;
                    if errs <= 3 || errs % 30 == 0 {
                        eprintln!("[inc-sync] Ledger #{seq}: error: {e} (backoff: skip {errs} rounds)");
                    }
                }
            }

            s.running = false;
            drop(s);
            running.store(false, Ordering::SeqCst);
        });
    }
}

/// Fetch transaction metadata for a closed ledger, extract all affected state
/// object indices, fetch each one in binary, and write to RocksDB.
/// Returns (fetched, deleted, failed, modified_keys).
async fn sync_ledger_state(
    seq: u32,
    db: &Arc<rocksdb::DB>,
) -> Result<(u64, u64, u64, Vec<xrpl_core::types::Hash256>), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("client: {e}"))?;

    // Step 1: Fetch the ledger with transaction metadata
    let resp = client
        .post("http://127.0.0.1:5005")
        .json(&serde_json::json!({
            "method": "ledger",
            "params": [{
                "ledger_index": seq,
                "transactions": true,
                "expand": true,
                "binary": false
            }]
        }))
        .send()
        .await
        .map_err(|e| format!("ledger rpc: {e}"))?;

    let body: serde_json::Value = resp.json().await.map_err(|e| format!("json: {e}"))?;

    let txs = body["result"]["ledger"]["transactions"]
        .as_array()
        .ok_or_else(|| "no transactions array".to_string())?;

    // Step 2: Extract all affected state object indices from metadata
    let mut modified_indices: HashSet<String> = HashSet::new();
    let mut deleted_indices: HashSet<String> = HashSet::new();

    for tx in txs {
        let meta = if tx["metaData"].is_object() {
            &tx["metaData"]
        } else if tx["meta"].is_object() {
            &tx["meta"]
        } else {
            continue;
        };

        if let Some(nodes) = meta["AffectedNodes"].as_array() {
            for node in nodes {
                if let Some(created) = node.get("CreatedNode") {
                    if let Some(idx) = created["LedgerIndex"].as_str() {
                        modified_indices.insert(idx.to_string());
                    }
                }
                if let Some(modified) = node.get("ModifiedNode") {
                    if let Some(idx) = modified["LedgerIndex"].as_str() {
                        modified_indices.insert(idx.to_string());
                    }
                }
                if let Some(deleted) = node.get("DeletedNode") {
                    if let Some(idx) = deleted["LedgerIndex"].as_str() {
                        deleted_indices.insert(idx.to_string());
                        modified_indices.remove(idx);
                    }
                }
            }
        }
    }

    // Step 3: Fetch each modified object in binary and write to RocksDB
    let mut fetched = 0u64;
    let mut failed = 0u64;
    let mut modified_keys: Vec<xrpl_core::types::Hash256> = Vec::new();

    // Fetch in parallel batches of 20
    let indices: Vec<String> = modified_indices.into_iter().collect();
    for chunk in indices.chunks(20) {
        let mut handles = Vec::new();
        for index_hex in chunk {
            let client = client.clone();
            let index = index_hex.clone();
            let ledger_seq = seq;
            handles.push(tokio::spawn(async move {
                let resp = client
                    .post("http://127.0.0.1:5005")
                    .json(&serde_json::json!({
                        "method": "ledger_entry",
                        "params": [{"index": index, "binary": true, "ledger_index": ledger_seq}]
                    }))
                    .send()
                    .await;
                match resp {
                    Ok(r) => {
                        if let Ok(body) = r.json::<serde_json::Value>().await {
                            if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                                if let Ok(data) = hex::decode(data_hex) {
                                    return Some((index, data));
                                }
                            }
                        }
                        None
                    }
                    Err(_) => None,
                }
            }));
        }

        for handle in handles {
            if let Ok(Some((index_hex, data))) = handle.await {
                if let Ok(key_bytes) = hex::decode(&index_hex) {
                    if key_bytes.len() == 32 {
                        let _ = db.put(&key_bytes, &data);
                        let mut key = xrpl_core::types::Hash256([0u8; 32]);
                        key.0.copy_from_slice(&key_bytes);
                        modified_keys.push(key);
                        fetched += 1;
                    } else {
                        failed += 1;
                    }
                } else {
                    failed += 1;
                }
            } else {
                failed += 1;
            }
        }
    }

    // Step 4: Delete removed objects from RocksDB
    let mut deleted = 0u64;
    for index_hex in &deleted_indices {
        if let Ok(key_bytes) = hex::decode(index_hex) {
            if key_bytes.len() == 32 {
                let _ = db.delete(&key_bytes);
                let mut key = xrpl_core::types::Hash256([0u8; 32]);
                key.0.copy_from_slice(&key_bytes);
                modified_keys.push(key);
                deleted += 1;
            }
        }
    }

    Ok((fetched, deleted, failed, modified_keys))
}

/// Fetch a batch of keys from RPC in parallel and write fresh binary to RocksDB.
/// Returns count of successfully updated entries.
/// `pinned_seq` pins all fetches to one ledger so every object is consistent.
async fn fetch_batch(
    client: &reqwest::Client,
    endpoints: &[&str],
    keys: &[Vec<u8>],
    db: &Arc<rocksdb::DB>,
    pinned_seq: u32,
) -> u64 {
    let mut handles = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        let client = client.clone();
        let index_hex = hex::encode(key);
        let endpoint = endpoints[i % endpoints.len()].to_string();
        handles.push(tokio::spawn(async move {
            let resp = client
                .post(&endpoint)
                .json(&serde_json::json!({
                    "method": "ledger_entry",
                    "params": [{"index": index_hex, "binary": true, "ledger_index": pinned_seq}]
                }))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    if let Ok(body) = r.json::<serde_json::Value>().await {
                        if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                            if let Ok(data) = hex::decode(data_hex) {
                                return Some((index_hex, data));
                            }
                        }
                    }
                    None
                }
                Err(_) => None,
            }
        }));
    }

    let mut updated = 0u64;
    for handle in handles {
        if let Ok(Some((index_hex, data))) = handle.await {
            if let Ok(key_bytes) = hex::decode(&index_hex) {
                if key_bytes.len() == 32 {
                    let _ = db.put(&key_bytes, &data);
                    updated += 1;
                }
            }
        }
    }
    updated
}
