//! Incremental ledger state sync via RPC.
//!
//! After each ledger close, fetches transaction metadata to discover ALL
//! state objects that changed (accounts, offers, trust lines, directories, etc.),
//! then fetches each changed object in binary and writes it to RocksDB.
//! This keeps the local state tree in sync with the network without
//! re-downloading all ~30M objects.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    /// Set during backfill — inc-sync skips while this is true.
    pub backfilling: Arc<AtomicBool>,
    /// Last successfully synced ledger sequence.
    pub last_synced: Arc<AtomicU32>,
    /// Keys that failed all retries — re-attempted next round.
    repair_queue: Arc<Mutex<Vec<(u32, String)>>>, // (ledger_seq, index_hex)
}

impl IncrementalSyncer {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(Mutex::new(IncrementalSyncStats::default())),
            running: Arc::new(AtomicBool::new(false)),
            total_synced: Arc::new(AtomicU64::new(0)),
            consecutive_errors: Arc::new(AtomicU64::new(0)),
            backfilling: Arc::new(AtomicBool::new(false)),
            last_synced: Arc::new(AtomicU32::new(0)),
            repair_queue: Arc::new(Mutex::new(Vec::new())),
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

                let endpoints = [
                    "http://10.0.0.39:5005",
                    "http://10.0.0.39:5005",
                    "http://10.0.0.39:5005",
                ];

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
                            &client, &endpoints, &batch_keys, &db, pinned_seq,
                        ).await;
                        swept += batch_keys.len() as u64;
                        updated += fetched;
                        batch_keys.clear();

                        if swept % 10000 == 0 {
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

                if !batch_keys.is_empty() {
                    let fetched = fetch_batch(&client, &endpoints, &batch_keys, &db, pinned_seq).await;
                    swept += batch_keys.len() as u64;
                    updated += fetched;
                }

                let elapsed = start.elapsed().as_secs_f64();
                eprintln!("[sweep] DONE — {swept} keys swept, {updated} updated in {:.0}s", elapsed);

                let _ = std::fs::write(&done_path_clone, format!("{swept} keys, {updated} updated"));
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
        // Skip during backfill — they'd race on the same keys
        if self.backfilling.load(Ordering::Relaxed) {
            return;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return; // Already syncing — skip this round
        }

        let errs = self.consecutive_errors.load(Ordering::Relaxed);
        if errs > 0 && seq % (errs.min(60) as u32 + 1) != 0 {
            self.running.store(false, Ordering::SeqCst);
            return;
        }

        let stats = self.stats.clone();
        let running = self.running.clone();
        let total_synced = self.total_synced.clone();
        let cons_errors = self.consecutive_errors.clone();
        let last_synced = self.last_synced.clone();
        let repair_queue = self.repair_queue.clone();

        tokio::spawn(async move {
            // Fill any gap between last synced and requested seq
            let last = last_synced.load(Ordering::Relaxed);
            if last > 0 && seq > last + 1 {
                let gap = seq - last - 1;
                if gap <= 100 {
                    // Small gap: fetch at each specific ledger (recent, always available)
                    eprintln!("[inc-sync] Filling {gap}-ledger gap (#{} → #{})", last + 1, seq - 1);
                    let mut gap_fetched = 0u64;
                    let mut gap_deleted = 0u64;
                    let mut gap_failed = 0u64;
                    let mut first_mismatch_seq = 0u32;
                    let verify_client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .build()
                        .unwrap_or_default();
                    for fill_seq in (last + 1)..seq {
                        // RETRY until this ledger succeeds. Never skip — holes in
                        // state break the hash permanently. Brief pause between retries
                        // to avoid overwhelming rippled.
                        let mut ledger_ok = false;
                        for retry in 0..10u32 {
                            if retry > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(500 * (retry as u64).min(4))).await;
                            }
                            match sync_ledger_state(fill_seq, &db).await {
                                Ok((f, d, fa, ref _keys, _)) => {
                                    gap_fetched += f;
                                    gap_deleted += d;
                                    gap_failed += fa;
                                    // Don't update SHAMap during gap fill — just write to RocksDB.
                                    // The bottom-up rebuild after gap fill computes the correct hash.
                                    ledger_ok = true;
                                    break;
                                }
                                Err(e) => {
                                    if retry < 2 || retry % 3 == 0 {
                                        eprintln!("[inc-sync] Gap #{fill_seq} attempt {}: {e}", retry + 1);
                                    }
                                }
                            }
                        }
                        if !ledger_ok {
                            eprintln!("[inc-sync] Gap #{fill_seq} FAILED after 10 retries — stopping gap fill");
                            break;
                        }
                        // Brief pause between ledgers to avoid overwhelming rippled
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        // Log progress every 10 ledgers
                        if (fill_seq - last) % 10 == 0 {
                            eprintln!("[inc-sync] Gap progress: {} ledgers done", fill_seq - last);
                        }
                    }
                    eprintln!("[inc-sync] Gap fill: {gap_fetched} fetched, {gap_deleted} deleted, {gap_failed} failed across {gap} ledgers");

                    // Build leaf cache + compute hash after gap fill
                    let last_gap_seq = seq - 1;
                    { let hc = hash_comp.clone(); let d = db.clone();
                      tokio::task::spawn_blocking(move || { let ek: Vec<xrpl_core::types::Hash256> = Vec::new(); hc.update_and_hash(&d, &ek); }).await.ok(); }
                    let diag_hash = hash_comp.status.lock().computed_hash.clone();
                    let verify_client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5)).build().unwrap_or_default();
                    if let Ok(resp) = verify_client.post("http://10.0.0.39:5005")
                        .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":last_gap_seq}]}))
                        .send().await
                    {
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            if let Some(net) = body["result"]["ledger"]["account_hash"].as_str() {
                                let m = if diag_hash.to_uppercase() == net.to_uppercase() { "MATCH" } else { "MISMATCH" };
                                eprintln!("[DIAG] After gap fill — hash at #{last_gap_seq}: {m}");
                                if m == "MISMATCH" {
                                    eprintln!("[DIAG]   ours: {}", &diag_hash[..32]);
                                    eprintln!("[DIAG]   net:  {}", &net[..32]);
                                }
                            }
                        }
                    }
                } else if false && gap <= 200 { // DISABLED — batch gap fill poisons data
                    // Large gap (post-download catch-up): collect ALL modified keys from
                    // metadata, then batch-fetch at the CURRENT ledger. We don't need
                    // intermediate states — only the final state matters. Fetching at
                    // historical ledgers is unreliable (rippled prunes them).
                    eprintln!("[inc-sync] Large gap: {gap} ledgers (#{} → #{}) — batch fetch at #{seq}", last + 1, seq - 1);

                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(10))
                        .build()
                        .unwrap_or_default();

                    let mut all_modified: HashSet<String> = HashSet::new();
                    let mut all_deleted: HashSet<String> = HashSet::new();

                    // Phase 1: Collect the union of all modified/deleted keys from metadata
                    // Retry failed ledger RPCs — missing even one ledger's metadata
                    // means missed DeletedNode entries, leaving ghost objects.
                    let mut skipped_ledgers = 0u32;
                    for fill_seq in (last + 1)..seq {
                        let mut body_opt: Option<serde_json::Value> = None;
                        for attempt in 0..5u32 {
                            if attempt > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                            let resp = client
                                .post("http://10.0.0.39:5005")
                                .json(&serde_json::json!({
                                    "method": "ledger",
                                    "params": [{"ledger_index": fill_seq, "transactions": true, "expand": true, "binary": false}]
                                }))
                                .send()
                                .await;
                            if let Ok(r) = resp {
                                if let Ok(b) = r.json::<serde_json::Value>().await {
                                    if b["result"]["ledger"]["transactions"].is_array() {
                                        body_opt = Some(b);
                                        break;
                                    }
                                }
                            }
                        }
                        let body = match body_opt {
                            Some(b) => b,
                            None => { skipped_ledgers += 1; continue; }
                        };

                        if let Some(txs) = body["result"]["ledger"]["transactions"].as_array() {
                            for tx in txs {
                                let meta = if tx["metaData"].is_object() { &tx["metaData"] }
                                    else if tx["meta"].is_object() { &tx["meta"] }
                                    else { continue; };
                                if let Some(nodes) = meta["AffectedNodes"].as_array() {
                                    for node in nodes {
                                        if let Some(c) = node.get("CreatedNode") {
                                            if let Some(i) = c["LedgerIndex"].as_str() {
                                                all_modified.insert(i.to_string());
                                                all_deleted.remove(i);
                                            }
                                        }
                                        if let Some(m) = node.get("ModifiedNode") {
                                            if let Some(i) = m["LedgerIndex"].as_str() {
                                                if !all_deleted.contains(i) {
                                                    all_modified.insert(i.to_string());
                                                }
                                            }
                                        }
                                        if let Some(d) = node.get("DeletedNode") {
                                            if let Some(i) = d["LedgerIndex"].as_str() {
                                                all_deleted.insert(i.to_string());
                                                all_modified.remove(i);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if skipped_ledgers > 0 {
                        eprintln!("[inc-sync] WARNING: {skipped_ledgers} gap ledgers had no metadata — may miss deletions");
                    }

                    // Always include protocol-level singletons
                    all_modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string()); // LedgerHashes
                    all_modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string()); // NegativeUNL
                    all_modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string()); // Amendments
                    all_modified.insert("4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651".to_string()); // FeeSettings

                    eprintln!("[inc-sync] Gap metadata: {} modified, {} deleted — fetching at #{seq}", all_modified.len(), all_deleted.len());

                    // Phase 2: Fetch ALL modified keys at the CURRENT ledger (seq)
                    let mut fetched = 0u32;
                    let mut failed = 0u32;
                    let mut deleted_count = 0u32;
                    let mut modified_keys: Vec<xrpl_core::types::Hash256> = Vec::new();
                    let indices: Vec<String> = all_modified.into_iter().collect();

                    for chunk in indices.chunks(5) {
                        let mut handles = Vec::new();
                        for index_hex in chunk {
                            let client = client.clone();
                            let index = index_hex.clone();
                            let fetch_seq = seq; // current ledger — always available
                            handles.push(tokio::spawn(async move {
                                let resp = client
                                    .post("http://10.0.0.39:5005")
                                    .json(&serde_json::json!({
                                        "method": "ledger_entry",
                                        "params": [{"index": index, "binary": true, "ledger_index": fetch_seq}]
                                    }))
                                    .send()
                                    .await;
                                match resp {
                                    Ok(r) => {
                                        if let Ok(body) = r.json::<serde_json::Value>().await {
                                            if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                                                if let Ok(data) = hex::decode(data_hex) {
                                                    return Ok(Some((index, data)));
                                                }
                                            }
                                            // entryNotFound = failure, not deletion
                                            if body["result"]["error"].as_str() == Some("entryNotFound") {
                                                return Err(index);
                                            }
                                        }
                                        Err(index)
                                    }
                                    Err(_) => Err(index),
                                }
                            }));
                        }
                        for (i, handle) in handles.into_iter().enumerate() {
                            match handle.await {
                                Ok(Ok(Some((index_hex, data)))) => {
                                    if let Ok(key_bytes) = hex::decode(&index_hex) {
                                        if key_bytes.len() == 32 {
                                            let _ = db.put(&key_bytes, &data);
                                            let mut key = xrpl_core::types::Hash256([0u8; 32]);
                                            key.0.copy_from_slice(&key_bytes);
                                            modified_keys.push(key);
                                            fetched += 1;
                                        }
                                    }
                                }
                                Ok(Ok(None)) => {
                                    // entryNotFound — object deleted since gap metadata was collected
                                    // Delete from RocksDB to keep state consistent
                                    if let Ok(key_bytes) = hex::decode(&chunk[i]) {
                                        if key_bytes.len() == 32 {
                                            let _ = db.delete(&key_bytes);
                                            let mut key = xrpl_core::types::Hash256([0u8; 32]);
                                            key.0.copy_from_slice(&key_bytes);
                                            modified_keys.push(key);
                                            deleted_count += 1;
                                        }
                                    }
                                }
                                _ => { failed += 1; }
                            }
                        }
                    }

                    // Phase 3: Delete all deleted keys
                    for index_hex in &all_deleted {
                        if let Ok(key_bytes) = hex::decode(index_hex) {
                            if key_bytes.len() == 32 {
                                let _ = db.delete(&key_bytes);
                                let mut key = xrpl_core::types::Hash256([0u8; 32]);
                                key.0.copy_from_slice(&key_bytes);
                                modified_keys.push(key);
                                deleted_count += 1;
                            }
                        }
                    }

                    eprintln!("[inc-sync] Gap fill done: {fetched} fetched, {deleted_count} deleted, {failed} failed");
                }
            }

            // Drain repair queue: re-fetch keys that failed in previous rounds
            {
                let pending: Vec<(u32, String)> = {
                    let mut rq = repair_queue.lock();
                    std::mem::take(&mut *rq)
                };
                if !pending.is_empty() {
                    let repaired = repair_failed_keys(&pending, &db, seq).await;
                    let remaining = pending.len() - repaired;
                    if repaired > 0 {
                        eprintln!("[repair] Fixed {repaired}/{} stale keys", pending.len());
                    }
                    if remaining > 0 {
                        // Re-queue keys that still failed
                        let mut rq = repair_queue.lock();
                        for (lseq, idx) in &pending[repaired..] {
                            rq.push((*lseq, idx.clone()));
                        }
                    }
                    // Update SHAMap for all repaired keys
                    if repaired > 0 && hash_comp.is_ready() {
                        let repair_keys: Vec<xrpl_core::types::Hash256> = pending[..repaired]
                            .iter()
                            .filter_map(|(_, idx)| {
                                hex::decode(idx).ok().and_then(|b| {
                                    if b.len() == 32 {
                                        let mut k = xrpl_core::types::Hash256([0u8; 32]);
                                        k.0.copy_from_slice(&b);
                                        Some(k)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect();
                        if !repair_keys.is_empty() {
                            let hc = hash_comp.clone(); let d = db.clone();
                            tokio::task::spawn_blocking(move || { hc.update_round(&d, &repair_keys); }).await.ok();
                        }
                    }
                }
            }

            let result = sync_ledger_state(seq, &db).await;

            let mut should_verify = false;
            let mut hash_keys: Vec<xrpl_core::types::Hash256> = Vec::new();

            {
                let mut s = stats.lock();
                s.ledger_seq = seq;
                s.total_rounds += 1;

                match result {
                    Ok((fetched, deleted, failed, modified_keys, failed_keys)) => {
                        cons_errors.store(0, Ordering::Relaxed);

                        s.objects_fetched = fetched;
                        s.objects_deleted = deleted;
                        s.objects_failed = failed;
                        s.total_synced += fetched + deleted;
                        total_synced.fetch_add(fetched + deleted, Ordering::Relaxed);

                        if !modified_keys.is_empty() {
                            hash_keys = modified_keys;
                            should_verify = true;
                        }

                        // Queue failed keys for repair next round
                        if !failed_keys.is_empty() {
                            let mut rq = repair_queue.lock();
                            for fk in failed_keys {
                                rq.push((seq, fk));
                            }
                        }

                        last_synced.store(seq, Ordering::Relaxed);

                        if fetched + deleted > 0 || failed > 0 {
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
            } // lock dropped

            // Run heavy hash computation off the tokio runtime
            if !hash_keys.is_empty() {
                let hc = hash_comp.clone(); let d = db.clone();
                tokio::task::spawn_blocking(move || { hc.update_and_hash(&d, &hash_keys); }).await.ok();
            }

            // Verify hash inline for this specific ledger
            if should_verify {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap_or_default();
                if let Ok(resp) = client
                    .post("http://10.0.0.39:5005")
                    .json(&serde_json::json!({
                        "method": "ledger",
                        "params": [{"ledger_index": seq}]
                    }))
                    .send()
                    .await
                {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(net_hash) = body["result"]["ledger"]["account_hash"].as_str() {
                            hash_comp.set_network_hash(net_hash, seq);
                        }
                    }
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }

    /// Backfill a range of ledgers to close the gap between the pinned download
    /// and the current validated ledger. Fetches changes for each ledger sequentially,
    /// writes to RocksDB, and updates the SHAMap.
    pub fn backfill_range(
        &self,
        from_seq: u32,
        to_seq: u32,
        db: Arc<rocksdb::DB>,
        hash_comp: Arc<crate::state_hash::StateHashComputer>,
    ) {
        let stats = self.stats.clone();
        let total_synced = self.total_synced.clone();
        let backfilling = self.backfilling.clone();
        let last_synced = self.last_synced.clone();
        let repair_queue = self.repair_queue.clone();

        // Suppress inc-sync while backfilling
        backfilling.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let total = to_seq.saturating_sub(from_seq);
            eprintln!("[backfill] Catching up {total} ledgers (#{from_seq} → #{to_seq})...");
            let start = std::time::Instant::now();
            let mut synced = 0u32;
            let mut total_fetched = 0u64;
            let mut total_deleted = 0u64;
            let mut total_failed = 0u64;
            let mut errors = 0u32;

            let mut next_seq = from_seq + 1;
            let mut target_seq = to_seq;

            loop {
                while next_seq <= target_seq {
                    match sync_ledger_state(next_seq, &db).await {
                        Ok((fetched, deleted, failed, modified_keys, failed_keys)) => {
                            total_fetched += fetched;
                            total_deleted += deleted;
                            total_failed += failed;
                            if !modified_keys.is_empty() && hash_comp.is_ready() {
                                let hc = hash_comp.clone(); let d = db.clone();
                                tokio::task::spawn_blocking(move || { hc.update_round(&d, &modified_keys); }).await.ok();
                            }
                            // Queue failures for repair after backfill
                            if !failed_keys.is_empty() {
                                let mut rq = repair_queue.lock();
                                for fk in failed_keys {
                                    rq.push((next_seq, fk));
                                }
                            }
                            synced += 1;
                            errors = 0;
                        }
                        Err(e) => {
                            errors += 1;
                            if errors <= 3 {
                                eprintln!("[backfill] Error at #{next_seq}: {e}");
                            }
                            if errors > 10 {
                                eprintln!("[backfill] Too many errors, stopping");
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                    }
                    next_seq += 1;

                    if synced % 20 == 0 {
                        let elapsed = start.elapsed().as_secs_f64();
                        let rate = synced as f64 / elapsed.max(0.1);
                        eprintln!(
                            "[backfill] {synced} ledgers — {total_fetched} fetched, {total_deleted} deleted, {total_failed} failed — {rate:.1}/s"
                        );
                    }
                }

                if errors > 10 { break; }

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap_or_default();
                let latest: u32 = match client
                    .post("http://10.0.0.39:5005")
                    .json(&serde_json::json!({"method":"ledger","params":[{"ledger_index":"validated"}]}))
                    .send()
                    .await
                {
                    Ok(resp) => {
                        resp.json::<serde_json::Value>().await.ok()
                            .and_then(|v| v["result"]["ledger"]["ledger_index"].as_str()
                                .and_then(|s| s.parse().ok()))
                            .unwrap_or(target_seq)
                    }
                    Err(_) => target_seq,
                };

                if latest <= target_seq + 2 {
                    eprintln!("[backfill] Caught up — within {} of validated", latest.saturating_sub(target_seq));
                    break;
                }

                eprintln!("[backfill] Still {} behind — continuing...", latest - target_seq);
                target_seq = latest;
            }

            let elapsed = start.elapsed().as_secs_f64();
            eprintln!(
                "[backfill] DONE — {synced} ledgers in {elapsed:.1}s — {total_fetched} updated, {total_deleted} deleted, {total_failed} failed"
            );

            {
                let mut s = stats.lock();
                s.total_synced += total_fetched + total_deleted;
            }
            total_synced.fetch_add(total_fetched + total_deleted, Ordering::Relaxed);

            // Rebuild SHAMap from scratch to ensure consistency after backfill
            { let hc = hash_comp.clone(); let d = db.clone();
              tokio::task::spawn_blocking(move || { hc.rebuild(&d); }).await.ok(); }

            // Drain repair queue immediately after rebuild
            {
                let pending: Vec<(u32, String)> = {
                    let mut rq = repair_queue.lock();
                    std::mem::take(&mut *rq)
                };
                if !pending.is_empty() {
                    let repaired = repair_failed_keys(&pending, &db, target_seq).await;
                    eprintln!("[backfill] Repaired {repaired}/{} failed keys post-rebuild", pending.len());
                    if repaired > 0 && hash_comp.is_ready() {
                        let repair_keys: Vec<xrpl_core::types::Hash256> = pending[..repaired]
                            .iter()
                            .filter_map(|(_, idx)| {
                                hex::decode(idx).ok().and_then(|b| {
                                    if b.len() == 32 {
                                        let mut k = xrpl_core::types::Hash256([0u8; 32]);
                                        k.0.copy_from_slice(&b);
                                        Some(k)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .collect();
                        if !repair_keys.is_empty() {
                            let hc = hash_comp.clone(); let d = db.clone();
                            tokio::task::spawn_blocking(move || { hc.update_round(&d, &repair_keys); }).await.ok();
                        }
                    }
                }
            }

            last_synced.store(target_seq, Ordering::Relaxed);

            backfilling.store(false, Ordering::SeqCst);
            eprintln!("[backfill] Inc-sync resumed at ledger ~#{target_seq}");
        });
    }
}

/// Fetch transaction metadata for a closed ledger, extract all affected state
/// object indices, fetch each one in binary, and write to RocksDB.
/// Returns (fetched, deleted, failed, modified_keys, failed_keys).
/// `failed_keys` contains index hex strings that failed ALL retries.
async fn sync_ledger_state(
    seq: u32,
    db: &Arc<rocksdb::DB>,
) -> Result<(u64, u64, u64, Vec<xrpl_core::types::Hash256>, Vec<String>), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .pool_max_idle_per_host(1)
        .build()
        .map_err(|e| format!("client: {e}"))?;

    // Step 1: Fetch the ledger with transaction metadata
    let resp = client
        .post("http://10.0.0.39:5005")
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

    // Sort by TransactionIndex (execution order) — array order can differ!
    let mut sorted_txs: Vec<&serde_json::Value> = txs.iter().collect();
    sorted_txs.sort_by_key(|tx| {
        let meta = if tx["metaData"].is_object() { &tx["metaData"] }
            else if tx["meta"].is_object() { &tx["meta"] }
            else { return 0u64; };
        meta["TransactionIndex"].as_u64().unwrap_or(0)
    });

    for tx in sorted_txs {
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

    // Final cleanup: objects that appear in BOTH modified and deleted sets
    // (created by one tx, deleted by another, modified by a third in the same ledger)
    // should only be in deleted. They don't exist in the final state.
    for del_key in &deleted_indices {
        modified_indices.remove(del_key);
    }

    // Step 2b: Protocol-level singletons — force-fetch every round.
    // LedgerHashes changes every ledger. NegativeUNL and Amendments may change
    // via pseudo-transactions. FeeSettings changes on fee votes.
    modified_indices.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string()); // LedgerHashes
    modified_indices.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string()); // NegativeUNL
    modified_indices.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string()); // Amendments
    modified_indices.insert("4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651".to_string()); // FeeSettings
    // LedgerHashes sub-page: key = SHA512Half(0x0073 || uint32_be(seq/65536))
    {
        use sha2::{Sha512, Digest};
        let group = seq / 65536;
        let mut data = vec![0x00u8, 0x73];
        data.extend_from_slice(&group.to_be_bytes());
        let hash = Sha512::digest(&data);
        modified_indices.insert(hex::encode(&hash[..32]).to_uppercase());
    }

    // Step 3: Fetch ALL modified objects into memory FIRST.
    // Only commit to RocksDB if every single fetch succeeds.
    // One stale object permanently corrupts the Merkle hash.
    let mut fetched_data: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut failed = 0u64;
    let mut fetched = 0u64;
    let mut deleted = 0u64;
    let mut modified_keys: Vec<xrpl_core::types::Hash256> = Vec::new();
    let mut first_pass_failures: Vec<String> = Vec::new();

    let indices: Vec<String> = modified_indices.into_iter().collect();
    let mut all_ok = true;
    for index_hex in &indices {
        let mut ok = false;
        for _attempt in 0..3u32 {
            let resp = client
                .post("http://10.0.0.39:5005")
                .json(&serde_json::json!({
                    "method": "ledger_entry",
                    "params": [{"index": index_hex, "binary": true, "ledger_index": seq}]
                }))
                .send()
                .await;

            if let Ok(r) = resp {
                if let Ok(body) = r.json::<serde_json::Value>().await {
                    // Verify response is for the CORRECT ledger
                    let resp_seq = body["result"]["ledger_index"].as_u64().unwrap_or(0) as u32;
                    if resp_seq != 0 && resp_seq != seq {
                        eprintln!("[inc-sync] WRONG LEDGER: requested={seq} got={resp_seq} key={}...", &index_hex[..16]);
                        break; // treat as failure
                    }
                    if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                        if let Ok(data) = hex::decode(data_hex) {
                            if let Ok(key_bytes) = hex::decode(index_hex) {
                                if key_bytes.len() == 32 {
                                    let mut key = [0u8; 32];
                                    key.copy_from_slice(&key_bytes);
                                    fetched_data.push((key, data));
                                    ok = true;
                                    break;
                                }
                            }
                        }
                    }
                    // entryNotFound — try public fallback before giving up
                    if body["result"]["error"].as_str() == Some("entryNotFound") {
                        // Try xrplcluster.com as fallback (full history, reliable)
                        let fallback = client
                            .post("https://xrplcluster.com")
                            .json(&serde_json::json!({
                                "method": "ledger_entry",
                                "params": [{"index": index_hex, "binary": true, "ledger_index": seq}]
                            }))
                            .send()
                            .await;
                        if let Ok(r2) = fallback {
                            if let Ok(body2) = r2.json::<serde_json::Value>().await {
                                if let Some(data_hex) = body2["result"]["node_binary"].as_str() {
                                    if let Ok(data) = hex::decode(data_hex) {
                                        if let Ok(key_bytes) = hex::decode(index_hex) {
                                            if key_bytes.len() == 32 {
                                                let mut key = [0u8; 32];
                                                key.copy_from_slice(&key_bytes);
                                                fetched_data.push((key, data));
                                                ok = true;
                                                break;
                                            }
                                        }
                                    }
                                }
                                // Also entryNotFound on public = truly doesn't exist
                                if body2["result"]["error"].as_str() == Some("entryNotFound") {
                                    ok = true; // genuinely doesn't exist
                                    break;
                                }
                            }
                        }
                        break; // fallback failed too — ok stays false
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !ok {
            all_ok = false;
            failed += 1;
            first_pass_failures.push(index_hex.clone());
        }
    }

    // ABORT if any fetch failed — don't write partial data
    if !all_ok {
        return Err(format!("ledger #{seq}: {failed} objects failed to fetch — aborting to prevent stale data"));
    }

    // All fetches succeeded — commit to RocksDB atomically
    let mut batch = rocksdb::WriteBatch::default();
    for (key, data) in &fetched_data {
        batch.put(key, data);
        let mut k = xrpl_core::types::Hash256([0u8; 32]);
        k.0.copy_from_slice(key);
        modified_keys.push(k);
        fetched += 1;
    }
    // Delete removed objects
    for index_hex in &deleted_indices {
        if let Ok(key_bytes) = hex::decode(index_hex) {
            if key_bytes.len() == 32 {
                batch.delete(&key_bytes);
                let mut key = xrpl_core::types::Hash256([0u8; 32]);
                key.0.copy_from_slice(&key_bytes);
                modified_keys.push(key);
                deleted += 1;
            }
        }
    }
    if let Err(e) = db.write(batch) {
        return Err(format!("ledger #{seq}: RocksDB write failed: {e}"));
    }

    Ok((fetched, deleted, failed, modified_keys, first_pass_failures))
}

/// Re-fetch failed keys at the current validated ledger.
/// Returns the number of successfully repaired keys.
/// Successfully fetched keys are written to RocksDB.
async fn repair_failed_keys(
    pending: &[(u32, String)],
    db: &Arc<rocksdb::DB>,
    current_seq: u32,
) -> usize {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_default();

    let mut repaired = 0usize;
    for (_orig_seq, index_hex) in pending {
        let resp = client
            .post("http://10.0.0.39:5005")
            .json(&serde_json::json!({
                "method": "ledger_entry",
                "params": [{"index": index_hex, "binary": true, "ledger_index": current_seq}]
            }))
            .send()
            .await;

        if let Ok(r) = resp {
            if let Ok(body) = r.json::<serde_json::Value>().await {
                if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                    if let Ok(data) = hex::decode(data_hex) {
                        if let Ok(key_bytes) = hex::decode(index_hex) {
                            if key_bytes.len() == 32 {
                                let _ = db.put(&key_bytes, &data);
                                repaired += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    repaired
}

/// Fetch a batch of keys from RPC in parallel and write fresh binary to RocksDB.
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
