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

    #[cfg(feature = "ffi")]
    let stage3_cfg = crate::stage3::Stage3Config::from_env();
    #[cfg(feature = "ffi")]
    eprintln!(
        "[stage3] start_ws_sync reached: Stage3Config.enabled={} XRPL_FFI_STAGE3={:?}",
        stage3_cfg.enabled,
        std::env::var("XRPL_FFI_STAGE3").ok()
    );
    #[cfg(feature = "ffi")]
    if stage3_cfg.enabled {
        eprintln!("[stage3] ENABLED — FFI overlay will source state.rocks bytes (singletons still RPC)");
    }
    // Publish stage3 state to FfiStats so /api/engine can expose it for
    // dashboards + watch_engine.py. Only when the ffi verifier is present.
    #[cfg(feature = "ffi")]
    if let Some(ref verifier) = ffi_verifier {
        verifier.shared_stats().lock().stage3_enabled = stage3_cfg.enabled;
    }

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
                    // Fetch account_hash for EVERY ledger so every one can be
                    // hash-verified (not just the latest in a gap-fill batch).
                    let account_hash = fetch_account_hash(&rpc, process_seq).await.unwrap_or_default();

                    acc_modified.clear();
                    acc_deleted.clear();
                    acc_tx_count = 0;

                    #[cfg(feature = "ffi")]
                    let mut ffi_overlay_opt: Option<crate::ffi_engine::LedgerOverlay> = None;

                    let mut meta_ok = false;
                    let mut ledger_header: LedgerHeader = LedgerHeader::default();
                    for attempt in 0..3u32 {
                        match fetch_ledger_metadata(&rpc, process_seq, &mut acc_modified, &mut acc_deleted, &mut acc_tx_count).await {
                            Ok((_sorted_txs, hdr, wallet_delta)) => {
                                ledger_header = hdr;
                                if wallet_delta != 0 {
                                    hash_comp.adjust_wallet_count(wallet_delta, process_seq);
                                }
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
                    // Include protocol-level singletons BEFORE FFI shadow hash
                    // so the shadow overlay covers ALL state changes.
                    if meta_ok {
                        acc_modified.insert("B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B".to_string()); // LedgerHashes
                        acc_modified.insert("2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244".to_string()); // NegativeUNL
                        acc_modified.insert("7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4".to_string()); // Amendments
                        acc_modified.insert("4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651".to_string()); // FeeSettings
                        {
                            use sha2::{Sha512, Digest};
                            let group = process_seq / 65536;
                            let mut data = vec![0x00u8, 0x73];
                            data.extend_from_slice(&group.to_be_bytes());
                            let hash = Sha512::digest(&data);
                            acc_modified.insert(hex::encode(&hash[..32]).to_uppercase());
                        }
                        for d in acc_deleted.iter().cloned().collect::<Vec<_>>() {
                            acc_modified.remove(&d);
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
                                // Network's recorded TransactionResult AND
                                // AffectedNodes per tx, for silent- and
                                // mutation-divergence detection.
                                let (expected_outcomes, expected_mutations) = fetch_ledger_expected_outcomes(&rpc, process_seq).await;
                                let synced = last_synced.load(Ordering::Acquire);
                                let steady = synced > 0 && process_seq.saturating_sub(synced) <= 2;
                                let shadow_ah = account_hash.clone();
                                // Always snapshot the hasher — we want the diff dump on EVERY
                                // mismatch, not just after the first 5 matches.
                                let hasher_snap = hash_comp.snapshot_hasher();
                                // Collect protocol-level modified objects for shadow hash.
                                // These are the keys ws_sync knows about but FFI doesn't
                                // apply (singletons, pseudo-tx effects). Fetch post-state
                                // from RPC so the shadow overlay is complete.
                                //
                                // Properly classify each RPC result as: present (use bytes),
                                // entryNotFound (treat as deleted), or fetch_error. If ANY
                                // modified key fails to fetch, abort the shadow check rather
                                // than reporting a false mismatch with an incomplete overlay.
                                let mut shadow_fetch_ok = true;
                                let proto_modified: Vec<(String, Option<String>)> = {
                                    let mut keys: Vec<String> = acc_modified.iter().cloned().collect();
                                    keys.extend(acc_deleted.iter().map(|k| k.clone()));
                                    let mut results = Vec::with_capacity(keys.len());
                                    for key_hex in &keys {
                                        if key_hex.len() != 64 { continue; }
                                        let mut classified: Option<Option<String>> = None;
                                        for attempt in 0..3u32 {
                                            match rpc.call("ledger_entry", serde_json::json!({
                                                "index": key_hex,
                                                "ledger_index": process_seq,
                                                "binary": true
                                            })).await {
                                                Ok(b) => {
                                                    if let Some(s) = b["result"]["node_binary"].as_str() {
                                                        classified = Some(Some(s.to_string()));
                                                        break;
                                                    } else if b["result"]["error"].as_str() == Some("entryNotFound") {
                                                        classified = Some(None);
                                                        break;
                                                    } else if attempt == 2 {
                                                        break;
                                                    }
                                                }
                                                Err(_) if attempt == 2 => break,
                                                Err(_) => {}
                                            }
                                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                        }
                                        match classified {
                                            Some(c) => results.push((key_hex.clone(), c)),
                                            None => {
                                                shadow_fetch_ok = false;
                                                eprintln!("[ffi-shadow] #{process_seq}: fetch failed for {} — skipping shadow check", &key_hex[..16]);
                                                break;
                                            }
                                        }
                                    }
                                    results
                                };
                                let deleted_keys: HashSet<String> = acc_deleted.clone();
                                // Build shadow overlay from RPC canonical bytes
                                // (same source as process_ledger uses for DB writes)
                                let shadow_overlay = build_shadow_overlay(&proto_modified, &deleted_keys);
                                let shadow_ok = shadow_fetch_ok;
                                let task = if steady {
                                    let snap = std::sync::Arc::new(
                                        crate::ffi_engine::OwnedSnapshot::new(db.clone())
                                    );
                                    let expected = if expected_outcomes.is_empty() { None } else { Some(expected_outcomes) };
                                    let expected_mut = if expected_mutations.is_empty() { None } else { Some(expected_mutations) };
                                    tokio::task::spawn_blocking(move || {
                                        let overlay = v.verify_ledger_with_snapshot(
                                            seq, &tx_blobs,
                                            hdr.parent_hash, hdr.parent_close_time, hdr.total_drops,
                                            Some(snap.as_ref()),
                                            expected.as_ref(),
                                            expected_mut.as_ref(),
                                        );
                                        if shadow_ok && !shadow_ah.is_empty() && !shadow_overlay.is_empty() {
                                            if let Some(hs) = hasher_snap {
                                                let matched = v.check_shadow_hash(hs, &shadow_overlay, &shadow_ah);
                                                if !matched {
                                                    dump_overlay_diff(seq, &overlay, &shadow_overlay);
                                                }
                                            }
                                        }
                                        overlay
                                    })
                                } else {
                                    let expected = if expected_outcomes.is_empty() { None } else { Some(expected_outcomes) };
                                    let expected_mut = if expected_mutations.is_empty() { None } else { Some(expected_mutations) };
                                    tokio::task::spawn_blocking(move || {
                                        let overlay = v.verify_ledger(
                                            seq, &tx_blobs,
                                            hdr.parent_hash, hdr.parent_close_time, hdr.total_drops,
                                            expected.as_ref(),
                                            expected_mut.as_ref(),
                                        );
                                        if shadow_ok && !shadow_ah.is_empty() && !shadow_overlay.is_empty() {
                                            if let Some(hs) = hasher_snap {
                                                let matched = v.check_shadow_hash(hs, &shadow_overlay, &shadow_ah);
                                                if !matched {
                                                    dump_overlay_diff(seq, &overlay, &shadow_overlay);
                                                }
                                            }
                                        }
                                        overlay
                                    })
                                };
                                if stage3_cfg.enabled {
                                    match task.await {
                                        Ok(o) => ffi_overlay_opt = Some(o),
                                        Err(e) => eprintln!("[stage3] FFI verify task join failed for #{seq}: {e}"),
                                    }
                                }
                                // else: drop(task) is implicit; task continues running fire-and-forget
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

                    // Hash-check EVERY ledger (gap or tip). Validation count requires it,
                    // and root recompute is only ~50ms per ledger (dirty-bucket parallel).
                    let compute_hash = true;
                    let _ = closed_seq;
                    let result = process_ledger(
                        &rpc, &db, &hash_comp,
                        process_seq, &acc_modified, &acc_deleted,
                        &account_hash, acc_tx_count, compute_hash,
                        &history,
                        #[cfg(feature = "ffi")] ffi_overlay_opt.take(),
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

/// Count AccountRoot creates minus deletes in one tx's metadata.
fn count_wallet_delta(body: &serde_json::Value) -> i64 {
    let meta = if body["meta"].is_object() { &body["meta"] }
        else if body["metaData"].is_object() { &body["metaData"] }
        else { return 0; };
    let mut delta: i64 = 0;
    if let Some(nodes) = meta["AffectedNodes"].as_array() {
        for node in nodes {
            if let Some(c) = node.get("CreatedNode") {
                if c["LedgerEntryType"].as_str() == Some("AccountRoot") {
                    delta += 1;
                }
            }
            if let Some(d) = node.get("DeletedNode") {
                if d["LedgerEntryType"].as_str() == Some("AccountRoot") {
                    delta -= 1;
                }
            }
        }
    }
    delta
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

/// Build the shadow overlay from RPC-fetched post-state objects.
/// Uses rippled's canonical `node_binary` for ALL keys (same source as
/// process_ledger), not FFI mutation data (which may serialize differently).
/// FFI overlay is only used to confirm WHICH keys changed — the actual bytes
/// come from rippled for hash consistency.
///
/// `rpc_objects` is `(key_hex, classified)` where `Some(data_hex)` means the
/// RPC returned bytes and `None` means rippled said `entryNotFound` (treat as
/// deletion). Keys that hit a fetch error must NOT appear here — the caller is
/// responsible for skipping the shadow check entirely in that case.
#[cfg(feature = "ffi")]
fn build_shadow_overlay(
    rpc_objects: &[(String, Option<String>)],
    deleted: &HashSet<String>,
) -> crate::ffi_engine::LedgerOverlay {
    let mut overlay = crate::ffi_engine::LedgerOverlay::new();
    for (key_hex, classified) in rpc_objects {
        let Ok(key_bytes) = hex::decode(key_hex) else { continue; };
        if key_bytes.len() != 32 { continue; }
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        if deleted.contains(key_hex) {
            overlay.insert(key, None);
        } else {
            match classified {
                Some(data_hex) if !data_hex.is_empty() => {
                    if let Ok(data) = hex::decode(data_hex) {
                        overlay.insert(key, Some(data));
                    }
                }
                Some(_) | None => {
                    // entryNotFound at this ledger → treat as deletion
                    overlay.insert(key, None);
                }
            }
        }
    }
    for key_hex in deleted {
        if let Ok(key_bytes) = hex::decode(key_hex) {
            if key_bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&key_bytes);
                if !overlay.contains_key(&key) {
                    overlay.insert(key, None);
                }
            }
        }
    }
    overlay
}

/// Dump set-difference between FFI overlay and RPC-derived shadow overlay on
/// mismatch. Writes to logs/overlay_diff_{seq}.txt. Keys in FFI but not shadow
/// = AffectedNodes missed them. Keys in shadow but not FFI = FFI didn't
/// compute them (e.g. tx-engine bug or state_rocks staleness).
#[cfg(feature = "ffi")]
fn dump_overlay_diff(
    seq: u32,
    ffi_overlay: &crate::ffi_engine::LedgerOverlay,
    shadow_overlay: &crate::ffi_engine::LedgerOverlay,
) {
    use std::io::Write;
    let path = std::path::PathBuf::from(format!("logs/overlay_diff_{seq}.txt"));
    if let Some(p) = path.parent() { let _ = std::fs::create_dir_all(p); }
    let Ok(mut f) = std::fs::File::create(&path) else { return; };
    let ffi_keys: HashSet<[u8; 32]> = ffi_overlay.keys().copied().collect();
    let shadow_keys: HashSet<[u8; 32]> = shadow_overlay.keys().copied().collect();
    let only_ffi: Vec<&[u8; 32]> = ffi_keys.difference(&shadow_keys).collect();
    let only_shadow: Vec<&[u8; 32]> = shadow_keys.difference(&ffi_keys).collect();
    let _ = writeln!(f, "# Ledger #{seq} overlay diff");
    let _ = writeln!(f, "# FFI overlay: {} keys", ffi_keys.len());
    let _ = writeln!(f, "# Shadow overlay (AffectedNodes-derived): {} keys", shadow_keys.len());
    let _ = writeln!(f, "\n## Keys ONLY in FFI overlay ({}) — AffectedNodes missed these", only_ffi.len());
    for k in only_ffi {
        let ffi_val = ffi_overlay.get(k);
        let tag = match ffi_val { Some(Some(d)) => format!("PUT({}B)", d.len()), Some(None) => "DEL".into(), _ => "??".into() };
        let _ = writeln!(f, "{} {tag}", hex::encode_upper(k));
    }
    let _ = writeln!(f, "\n## Keys ONLY in shadow overlay ({}) — FFI didn't emit these", only_shadow.len());
    for k in only_shadow {
        let sh_val = shadow_overlay.get(k);
        let tag = match sh_val { Some(Some(d)) => format!("PUT({}B)", d.len()), Some(None) => "DEL".into(), _ => "??".into() };
        let _ = writeln!(f, "{} {tag}", hex::encode_upper(k));
    }
    let _ = f.flush();
    eprintln!("[ffi-shadow] wrote key diff to {}", path.display());
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
) -> Result<(Vec<serde_json::Value>, LedgerHeader, i64), String> {
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

    let mut wallet_delta: i64 = 0;
    for tx in &sorted_txs {
        extract_affected_nodes(tx, modified, deleted);
        wallet_delta += count_wallet_delta(tx);
        *tx_count += 1;
    }
    Ok((sorted_txs, header, wallet_delta))
}

/// Fetch the network's recorded TransactionResult AND AffectedNodes for
/// every tx in `seq`, keyed by uppercase tx_hash. Used by silent- and
/// mutation-divergence detection. One extra RPC per ledger (non-binary
/// expand mode for JSON `hash` and `metaData`). Returns empty maps on
/// error — detection just gets skipped for that ledger.
///
/// Mutation set encoding: each entry is (key_hex_uppercase, kind_byte)
/// where kind ∈ {0=Created, 1=Modified, 2=Deleted}, matching libxrpl's
/// `xrpl_ffi::MutationKind`.
#[cfg(feature = "ffi")]
async fn fetch_ledger_expected_outcomes(
    rpc: &RippledClient, seq: u32,
) -> (
    std::collections::HashMap<String, String>,
    std::collections::HashMap<String, Vec<(String, u8)>>,
) {
    let mut outcomes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut mutations: std::collections::HashMap<String, Vec<(String, u8)>> = std::collections::HashMap::new();
    let body = match rpc.call(
        "ledger",
        serde_json::json!({"ledger_index": seq, "transactions": true, "expand": true, "binary": false}),
    ).await {
        Ok(b) => b,
        Err(_) => return (outcomes, mutations),
    };
    let empty = Vec::new();
    let txs = body["result"]["ledger"]["transactions"].as_array().unwrap_or(&empty);
    for tx in txs.iter() {
        let hash = match tx["hash"].as_str() {
            Some(h) => h.to_uppercase(),
            None => continue,
        };
        if let Some(result) = tx["metaData"]["TransactionResult"].as_str() {
            outcomes.insert(hash.clone(), result.to_string());
        }
        // AffectedNodes → Vec<(LedgerIndex, kind_byte)>
        if let Some(nodes) = tx["metaData"]["AffectedNodes"].as_array() {
            let mut entries: Vec<(String, u8)> = Vec::with_capacity(nodes.len());
            for node in nodes.iter() {
                let (kind_byte, inner) = if node.get("CreatedNode").is_some() {
                    (0u8, &node["CreatedNode"])
                } else if node.get("ModifiedNode").is_some() {
                    (1u8, &node["ModifiedNode"])
                } else if node.get("DeletedNode").is_some() {
                    (2u8, &node["DeletedNode"])
                } else {
                    continue;
                };
                if let Some(idx) = inner["LedgerIndex"].as_str() {
                    entries.push((idx.to_uppercase(), kind_byte));
                }
            }
            if !entries.is_empty() {
                mutations.insert(hash, entries);
            }
        }
    }
    (outcomes, mutations)
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
    #[cfg(feature = "ffi")] ffi_overlay: Option<crate::ffi_engine::LedgerOverlay>,
) -> bool {
    let ledger_start = std::time::Instant::now();

    // Stage 3: when ffi_overlay is supplied (caller has XRPL_FFI_STAGE3=1), the
    // FFI mutation overlay is the authoritative source for mutated SLEs. Only
    // keys NOT covered by libxrpl (the 5 protocol singletons mutated by
    // pseudo-txs, which apply_ledger_in_order filters out) fall back to RPC.
    let mut fetched_data: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut failed = 0u32;
    let mut stage3_used = false;
    #[cfg(feature = "ffi")]
    let mut stage3_overlay_keys: HashSet<[u8; 32]> = HashSet::new();

    #[cfg(feature = "ffi")]
    if let Some(overlay) = ffi_overlay {
        stage3_used = true;
        fetched_data.reserve(overlay.len() + 8);
        for (key, val) in &overlay {
            stage3_overlay_keys.insert(*key);
            match val {
                Some(data) => fetched_data.push((*key, data.clone())),
                None => fetched_data.push((*key, Vec::new())),
            }
        }
        // Fetch keys in modified ∪ deleted that aren't in the overlay
        // (the 5 protocol singletons mutated by pseudo-txs).
        for index_hex in modified.iter().chain(deleted.iter()) {
            let kb = match hex::decode(index_hex) {
                Ok(b) if b.len() == 32 => b,
                _ => continue,
            };
            let mut k = [0u8; 32];
            k.copy_from_slice(&kb);
            if stage3_overlay_keys.contains(&k) { continue; }
            let mut got = false;
            for attempt in 0..3u32 {
                let resp = rpc.client.post(rpc.rpc_url())
                    .json(&serde_json::json!({
                        "method": "ledger_entry",
                        "params": [{"index": index_hex, "binary": true, "ledger_index": seq}]
                    }))
                    .send().await;
                if let Ok(r) = resp {
                    if let Ok(body) = r.json::<serde_json::Value>().await {
                        if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                            if let Ok(data) = hex::decode(data_hex) {
                                fetched_data.push((k, data));
                                got = true;
                                break;
                            }
                        }
                        if body["result"]["error"].as_str() == Some("entryNotFound") {
                            fetched_data.push((k, Vec::new()));
                            got = true;
                            break;
                        }
                    }
                }
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
            if !got { failed += 1; }
        }
    }

    if !stage3_used {
        // Fetch all modified objects in parallel — 64 concurrent requests max.
        // Previous sequential approach: 500 objs × 3ms = 1.5s
        // Parallel with 64 concurrent: ~50ms
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
        fetched_data.reserve(results.len());
        for r in results {
            match r {
                Some(pair) => fetched_data.push(pair),
                None => failed += 1,
            }
        }
    }

    if failed > 0 {
        eprintln!("[ws-sync] Ledger #{seq}: {failed} fetch failures — ABORTING write to prevent state corruption");
        hash_comp.invalidate_tree();
        return false;
    }
    let fetch_ms = ledger_start.elapsed().as_millis() as u64;

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
    if !stage3_used {
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
                    let total_ms = ledger_start.elapsed().as_millis() as u64;
                    let hash_ms = total_ms.saturating_sub(fetch_ms);
                    eprintln!("[ws-sync] #{seq}: MATCH ({tx_count} txs, {} objs) fetch={}ms hash+write={}ms total={}ms",
                        fetched_data.len(), fetch_ms, hash_ms, total_ms);
                } else {
                    eprintln!("[ws-sync] #{seq}: MISMATCH ({tx_count} txs) — logging and moving on (shadow diff will capture root cause)");
                    return false;
                    // --- unreachable retry+scan code below, kept for reference ---
                    #[allow(unreachable_code)]
                    let mut healed = false;
                    #[allow(unreachable_code)]
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
        // Fast path: update the live hasher (so a later hash check in
        // steady-state finds it up-to-date) without recomputing the root.
        // Then merge into the persistent leaf_cache for restart caching.
        hash_comp.update_only(db, &keys);
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

#[cfg(all(test, feature = "ffi"))]
mod tests {
    use super::*;

    fn key_hex_for(seed: u8) -> String {
        // 32 bytes = 64 hex chars; fill with the seed for deterministic test keys.
        let mut k = String::with_capacity(64);
        for _ in 0..32 {
            k.push_str(&format!("{seed:02X}"));
        }
        k
    }

    /// Modified key with present RPC bytes lands as `Some(data)` in the overlay.
    #[test]
    fn shadow_overlay_present_modified_key() {
        let key = key_hex_for(0xAA);
        let data_hex = "DEADBEEF".to_string();
        let rpc_objects = vec![(key.clone(), Some(data_hex.clone()))];
        let deleted: HashSet<String> = HashSet::new();

        let overlay = build_shadow_overlay(&rpc_objects, &deleted);

        assert_eq!(overlay.len(), 1);
        let key_bytes: [u8; 32] = hex::decode(&key).unwrap().try_into().unwrap();
        assert_eq!(overlay.get(&key_bytes).unwrap().as_ref().unwrap(), &hex::decode(&data_hex).unwrap());
    }

    /// Modified key that rippled reports as `entryNotFound` (`classified = None`)
    /// MUST appear as a tombstone (`Some(None)`) in the overlay, not be silently
    /// dropped. This is the bug we just fixed.
    #[test]
    fn shadow_overlay_modified_key_entry_not_found_becomes_tombstone() {
        let key = key_hex_for(0xBB);
        let rpc_objects = vec![(key.clone(), None)];
        let deleted: HashSet<String> = HashSet::new();

        let overlay = build_shadow_overlay(&rpc_objects, &deleted);

        let key_bytes: [u8; 32] = hex::decode(&key).unwrap().try_into().unwrap();
        assert_eq!(overlay.len(), 1, "modified+entryNotFound must NOT be dropped");
        assert!(overlay.get(&key_bytes).unwrap().is_none(), "must be tombstone");
    }

    /// A key that's in the deleted set always becomes a tombstone, even if its
    /// rpc_objects entry was somehow `Some(...)`.
    #[test]
    fn shadow_overlay_deleted_set_always_wins() {
        let key = key_hex_for(0xCC);
        let rpc_objects = vec![(key.clone(), Some("01020304".to_string()))];
        let mut deleted: HashSet<String> = HashSet::new();
        deleted.insert(key.clone());

        let overlay = build_shadow_overlay(&rpc_objects, &deleted);

        let key_bytes: [u8; 32] = hex::decode(&key).unwrap().try_into().unwrap();
        assert_eq!(overlay.len(), 1);
        assert!(overlay.get(&key_bytes).unwrap().is_none());
    }

    /// A key that's only in the deleted set (never appears in rpc_objects)
    /// must still land in the overlay as a tombstone.
    #[test]
    fn shadow_overlay_deleted_only_key_lands_as_tombstone() {
        let key = key_hex_for(0xDD);
        let rpc_objects: Vec<(String, Option<String>)> = vec![];
        let mut deleted: HashSet<String> = HashSet::new();
        deleted.insert(key.clone());

        let overlay = build_shadow_overlay(&rpc_objects, &deleted);

        let key_bytes: [u8; 32] = hex::decode(&key).unwrap().try_into().unwrap();
        assert_eq!(overlay.len(), 1);
        assert!(overlay.get(&key_bytes).unwrap().is_none());
    }

    /// Mixed case: 1 modified+present, 1 modified+notfound, 1 deleted-only.
    /// The pre-fix bug would have produced overlay.len() == 2 (silently
    /// dropping the modified+notfound key). Fixed code returns 3.
    #[test]
    fn shadow_overlay_mixed_modes_yields_complete_overlay() {
        let k_present = key_hex_for(0x11);
        let k_notfound = key_hex_for(0x22);
        let k_deleted = key_hex_for(0x33);

        let rpc_objects = vec![
            (k_present.clone(), Some("AABBCCDD".to_string())),
            (k_notfound.clone(), None),
            (k_deleted.clone(), None),
        ];
        let mut deleted: HashSet<String> = HashSet::new();
        deleted.insert(k_deleted.clone());

        let overlay = build_shadow_overlay(&rpc_objects, &deleted);

        assert_eq!(overlay.len(), 3, "no key may be silently dropped");

        let kp: [u8; 32] = hex::decode(&k_present).unwrap().try_into().unwrap();
        let kn: [u8; 32] = hex::decode(&k_notfound).unwrap().try_into().unwrap();
        let kd: [u8; 32] = hex::decode(&k_deleted).unwrap().try_into().unwrap();

        assert!(overlay.get(&kp).unwrap().is_some(), "present must carry bytes");
        assert!(overlay.get(&kn).unwrap().is_none(), "notfound must be tombstone");
        assert!(overlay.get(&kd).unwrap().is_none(), "deleted must be tombstone");
    }

    /// Malformed key entries (wrong hex length, bad hex) must be skipped
    /// without panicking and without polluting the overlay.
    #[test]
    fn shadow_overlay_skips_malformed_keys() {
        let rpc_objects = vec![
            ("not-hex".to_string(), Some("DEADBEEF".to_string())),
            ("AA".to_string(), Some("DEADBEEF".to_string())),
            (key_hex_for(0xEE), Some("CAFEBABE".to_string())),
        ];
        let deleted: HashSet<String> = HashSet::new();

        let overlay = build_shadow_overlay(&rpc_objects, &deleted);
        assert_eq!(overlay.len(), 1, "only the well-formed key survives");
    }
}
