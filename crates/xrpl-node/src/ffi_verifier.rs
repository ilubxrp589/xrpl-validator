//! FFI-based independent tx verifier for the live validator.
//!
//! Replaces the deprecated pure-Rust `tx_engine` module. Every ledger that the
//! validator syncs gets its tx set replayed through libxrpl (via `ffi_engine`)
//! and tracked against mainnet for any divergence. 100% agreement is the
//! correctness SLO — see `docs/SLO.md`.
//!
//! Feature-gated behind `--features ffi`.
//!
//! ## Architecture
//!
//! - One `FfiVerifier` instance per validator process, cloned via `Arc` into
//!   each sync task.
//! - Reads each ledger's amendment set from its parent's Amendments object
//!   (`ledger_amendments`; the set loaded at startup is only the fallback) and
//!   holds a shared `FfiStats` snapshot exposed on `/api/status`.
//! - `verify_ledger(...)` takes a pre-ordered list of tx blobs plus the
//!   ledger header fields and delegates to `ffi_engine::apply_ledger_in_order`.
//! - Every diverged result is recorded to `logs/divergences.jsonl` via
//!   `DivergenceLog` (append-only, survives restarts).
//!
//! ## Current state provider
//!
//! Uses `RpcProvider` with multi-endpoint failover and retry-on-overload.
//! A future revision will swap to `RocksDbProvider` (validator's own DB
//! snapshot) so we stop hammering rippled — but the RPC path is the
//! reference implementation we verified to 100% agreement on live mainnet.
//!
//! ## API (same shape as old TxEngine)
//!
//! - `FfiVerifier::new(rpc_urls)` — constructor
//! - `verify_ledger(seq, tx_blobs, parent_hash, parent_close_time, close_resolution, total_drops)`
//! - `stats()` → `FfiStats`

use std::any::Any;
use std::sync::Arc;

use crate::ffi_engine::{
    amendments_in_object, amendments_object_key, apply_ledger_in_order_with_net, fetch_amendments_within, fetch_mainnet_amendments, new_stats,
    DivergenceLog, FfiStats, LedgerOverlay, NetParams, OwnedSnapshot, SharedFfiStats,
};

/// The longest the RPC catch-up path waits for the parent's Amendments object (one `ledger_entry`, one try).
///
/// The read is the first thing the libxrpl leg of a ledger does; the apply, which on this path reads every
/// object over RPC, comes after it, and under the native writer ws-sync gives the two together 3 s
/// (`tokio::time::timeout(Duration::from_secs(3), task)` around the verify task in ws_sync.rs) before the ledger
/// goes uncompared. Half a second leaves the apply five sixths of that when the endpoint is silent. It is not
/// tight for a live endpoint: ws-sync counts a whole-ledger request at ~50 ms ("Saves ~50ms per ledger", the
/// tx_blobs fetch) and retries its own per-key `ledger_entry` reads 50 ms apart, so this allows ten times that
/// for one small object. A read that fails costs nothing but the wait: the last known set stays in force and the
/// next ledger asks again.
pub const AMENDMENT_READ_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// The amendment set the verifier holds, and what it knows about where that set is the chain's.
struct KnownAmendments {
    /// The set last read from a parent ledger's Amendments object (at start: the one loaded then). What a ledger
    /// falls back to when its own read yields nothing.
    set: Vec<[u8; 32]>,
    /// RPC catch-up path only: ledgers `lo..=hi` whose Amendments object is known to hold `set` as its enabled
    /// list. It starts at a successful read (`lo = hi =` the ledger read at) and `note_ledger` moves `hi` over
    /// each next ledger that carries no EnableAmendment. A ledger whose parent lies inside is handed `set` with
    /// no read. None: nothing vouched for (at start, and after another path replaced the set).
    vouched: Option<(u32, u32)>,
}

/// FFI-based per-ledger tx verifier. Send + Sync, cheap to `Arc`-clone.
pub struct FfiVerifier {
    stats: SharedFfiStats,
    rpc_urls: Vec<String>,
    /// Independence Step 4: with peers as the ledger source, the amendments come from our own state, not RPC.
    state_db: Option<Arc<rocksdb::DB>>,
    /// The amendment set last read from a parent ledger's Amendments object. Each ledger gets its own from
    /// `ledger_amendments`.
    amendments: parking_lot::RwLock<KnownAmendments>,
    /// The RPC catch-up path's last read of the Amendments object failed: its log line is written once, not per
    /// ledger, until a read succeeds.
    rpc_read_failing: std::sync::atomic::AtomicBool,
    divergence_log: Arc<DivergenceLog>,
    /// Separate log for `tec*`/`tesSUCCESS` divergences — cases the main
    /// `divergence_log` skips because libxrpl returned a "claimed" or
    /// "success" outcome that our threading still acts on, but which
    /// disagrees with the network's recorded TransactionResult.
    silent_divergence_log: Arc<DivergenceLog>,
    /// Same-TER, different-mutation-set divergences (BF6C928F class).
    mutation_divergence_log: Arc<DivergenceLog>,
}

impl FfiVerifier {
    /// Construct a verifier and load the active mainnet amendments from the
    /// first RPC endpoint. Blocks while amendments are fetched (up to ~40s
    /// on rippled overload — see `fetch_mainnet_amendments`).
    pub fn new(rpc_urls: Vec<String>) -> Self {
        assert!(!rpc_urls.is_empty(), "at least one RPC endpoint required");
        let amendments = fetch_mainnet_amendments(&rpc_urls[0]);
        tracing::info!(
            count = amendments.len(),
            "ffi_verifier: loaded active mainnet amendments"
        );
        Self::with_amendments(rpc_urls, amendments, None)
    }

    /// Independence Step 4 (`XRPL_LEDGER_SOURCE=peers`): the active amendments from our own state.rocks, now and for
    /// every ledger. No RPC.
    pub fn from_state(rpc_urls: Vec<String>, db: Arc<rocksdb::DB>) -> Self {
        let amendments = crate::ffi_engine::amendments_from_state(&db);
        tracing::info!(count = amendments.len(), "ffi_verifier: loaded active amendments from our own state");
        Self::with_amendments(rpc_urls, amendments, Some(db))
    }

    fn with_amendments(rpc_urls: Vec<String>, amendments: Vec<[u8; 32]>, state_db: Option<Arc<rocksdb::DB>>) -> Self {
        let silent_path = std::env::var("XRPL_FFI_SILENT_DIVERGENCE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/silent_divergences.jsonl"));
        let mutation_path = std::env::var("XRPL_FFI_MUTATION_DIVERGENCE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/mutation_divergences.jsonl"));
        Self {
            stats: new_stats(),
            rpc_urls,
            state_db,
            amendments: parking_lot::RwLock::new(KnownAmendments { set: amendments, vouched: None }),
            rpc_read_failing: std::sync::atomic::AtomicBool::new(false),
            divergence_log: Arc::new(DivergenceLog::new()),
            silent_divergence_log: Arc::new(DivergenceLog::with_path(silent_path)),
            mutation_divergence_log: Arc::new(DivergenceLog::with_path(mutation_path)),
        }
    }

    /// The amendment set ledger `ledger_seq` is applied under: the enabled list of its PARENT ledger's Amendments
    /// object, read for every ledger. rippled builds a ledger under its parent's rules (`rules_(prevLedger.rules_)`,
    /// Ledger.cpp:259), so the ledger that carries an EnableAmendment runs under the old set throughout and the
    /// next ledger under the new one; the shim turns this list into libxrpl's `Rules` for the whole ledger.
    ///
    /// Where the parent's object is read from:
    /// - `snapshot`, when the caller verifies on one: it is the pre-ledger state, so its object is the parent's;
    /// - otherwise our own state as it stands, when peers are the ledger source (no RPC in that mode; the state
    ///   is not pinned to the parent there, as the rest of that path's reads are not);
    /// - otherwise (the RPC catch-up path) `ledger_entry` at `ledger_seq - 1` on the first RPC endpoint (the same
    ///   ledger the RPC path reads every other object at) — but only when the object can have changed since the
    ///   set held was read. The enabled list is rewritten by an EnableAmendment pseudo-transaction and by nothing
    ///   else (Change::applyAmendment), and ws-sync reports every ledger it reads (`note_ledger`), so a ledger
    ///   whose parent is covered by that record is handed the set held with no request at all. When a read is
    ///   needed (the first ledger, the ledger after a change, a gap in what ws-sync reported) it is one try that
    ///   returns within [`AMENDMENT_READ_BUDGET`].
    ///
    /// A source that yields no enabled amendment (object absent or unreadable, RPC failed or too slow) leaves the
    /// last set known in force: an empty Rules would turn every amendment-gated transaction into a divergence.
    /// On the RPC path that is logged once until a read succeeds, and nothing is vouched for, so the next ledger
    /// asks again.
    ///
    /// SCOPING: this covers amendments the linked libxrpl already implements. One introduced by a newer rippled
    /// still needs the rebuild: an id cannot supply engine semantics.
    pub fn ledger_amendments(&self, ledger_seq: u32, snapshot: Option<&OwnedSnapshot>) -> Vec<[u8; 32]> {
        use std::sync::atomic::Ordering;
        let parent = ledger_seq.saturating_sub(1);
        let (read, over_rpc) = match (snapshot, &self.state_db) {
            (Some(snap), _) => (
                snap.get(&amendments_object_key())
                    .ok()
                    .flatten()
                    .map(|bytes| amendments_in_object(&bytes))
                    .unwrap_or_default(),
                false,
            ),
            (None, Some(db)) => (crate::ffi_engine::amendments_from_state(db), false),
            (None, None) => {
                {
                    let known = self.amendments.read();
                    if known.vouched.is_some_and(|(lo, hi)| (lo..=hi).contains(&parent)) {
                        return known.set.clone();
                    }
                }
                (fetch_amendments_within(&self.rpc_urls[0], &parent.to_string(), AMENDMENT_READ_BUDGET), true)
            }
        };
        if read.is_empty() {
            if !over_rpc {
                eprintln!("[ffi-amendments] #{ledger_seq}: no enabled amendments readable from the parent state — keeping the last known set");
            } else if !self.rpc_read_failing.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[ffi-amendments] #{ledger_seq}: the Amendments object at #{parent} was not read from {} within {} ms — keeping the last known set, and asking again each ledger (this line is not repeated until a read succeeds)",
                    self.rpc_urls[0],
                    AMENDMENT_READ_BUDGET.as_millis(),
                );
            }
            return self.amendments.read().set.clone();
        }
        if over_rpc && self.rpc_read_failing.swap(false, Ordering::Relaxed) {
            eprintln!("[ffi-amendments] #{ledger_seq}: the Amendments object is readable again (at #{parent})");
        }
        let mut known = self.amendments.write();
        let delta = crate::amendments::diff_amendment_sets(&known.set, &read);
        if !delta.is_empty() {
            eprintln!(
                "[ffi-amendments] #{ledger_seq}: the parent's enabled set differs from the last one used — {} added, {} removed",
                delta.added.len(),
                delta.removed.len(),
            );
            for a in &delta.added {
                eprintln!("[ffi-amendments]   + {}", hex::encode_upper(a));
            }
            for a in &delta.removed {
                eprintln!("[ffi-amendments]   - {}", hex::encode_upper(a));
            }
            known.set = read.clone();
            // A snapshot (or our own state) may trail the parent by a ledger or two, so it vouches for no
            // particular ledger; and what the RPC path vouched for was the set just replaced.
            known.vouched = None;
        }
        if over_rpc {
            known.vouched = Some((parent, parent));
        }
        read
    }

    /// ws-sync's word on a ledger it has read: whether it carries an EnableAmendment pseudo-transaction, the only
    /// thing that rewrites the Amendments object's enabled list. (The verifier cannot see this for itself: its tx
    /// blobs come without the pseudo-transactions.) A ledger that carries none leaves the list as its parent had
    /// it, so where the parent is the newest ledger vouched for, this one is vouched for too. One that carries a
    /// change is not, and neither is anything past a ledger never reported: the RPC catch-up path then reads.
    pub fn note_ledger(&self, ledger_seq: u32, changes_amendments: bool) {
        if changes_amendments {
            return;
        }
        let mut known = self.amendments.write();
        if let Some((lo, hi)) = known.vouched {
            if hi.checked_add(1) == Some(ledger_seq) {
                known.vouched = Some((lo, ledger_seq));
            }
        }
    }

    /// Apply and verify every tx in a ledger, in `TransactionIndex` order,
    /// against mainnet's recorded TER via libxrpl. Blocking — call from a
    /// spawn_blocking task or a dedicated thread. Each diverged result is
    /// logged to `logs/divergences.jsonl` and counted in `stats()`.
    ///
    /// Pure RPC path — used by the sidecar `ffi_test` binary that has no
    /// local state DB.
    pub fn verify_ledger(
        &self,
        ledger_seq: u32,
        sorted_tx_blobs: &[Vec<u8>],
        parent_hash: [u8; 32],
        parent_close_time: u32,
        close_resolution: u8,
        total_drops: u64,
        expected_outcomes: Option<&std::collections::HashMap<String, String>>,
        expected_mutations: Option<&std::collections::HashMap<String, Vec<(String, u8)>>>,
        batch_parents: Option<&std::collections::HashMap<String, String>>,
    ) -> LedgerOverlay {
        let amendments = self.ledger_amendments(ledger_seq, None);
        apply_ledger_in_order_with_net(
            &self.stats,
            sorted_tx_blobs,
            ledger_seq,
            &self.rpc_urls,
            &amendments,
            parent_hash,
            parent_close_time,
            close_resolution,
            total_drops,
            Some(self.divergence_log.as_ref()),
            None,
            Some(self.silent_divergence_log.as_ref()),
            expected_outcomes,
            Some(self.mutation_divergence_log.as_ref()),
            expected_mutations,
            batch_parents,
            None,
            &NetParams::default(),
        )
    }

    /// Apply + verify using a pre-captured `OwnedSnapshot` as the SLE
    /// source (with RPC fallback on miss). The snapshot MUST be taken at
    /// the PRE-ledger boundary — i.e., before ws_sync's process_ledger()
    /// writes post-state to the DB. Owner of this invariant is the caller
    /// (see ws_sync.rs call site).
    pub fn verify_ledger_with_snapshot(
        &self,
        ledger_seq: u32,
        sorted_tx_blobs: &[Vec<u8>],
        parent_hash: [u8; 32],
        parent_close_time: u32,
        close_resolution: u8,
        total_drops: u64,
        snapshot: Option<&OwnedSnapshot>,
        expected_outcomes: Option<&std::collections::HashMap<String, String>>,
        expected_mutations: Option<&std::collections::HashMap<String, Vec<(String, u8)>>>,
        batch_parents: Option<&std::collections::HashMap<String, String>>,
    ) -> LedgerOverlay {
        let amendments = self.ledger_amendments(ledger_seq, snapshot);
        apply_ledger_in_order_with_net(
            &self.stats,
            sorted_tx_blobs,
            ledger_seq,
            &self.rpc_urls,
            &amendments,
            parent_hash,
            parent_close_time,
            close_resolution,
            total_drops,
            Some(self.divergence_log.as_ref()),
            snapshot,
            Some(self.silent_divergence_log.as_ref()),
            expected_outcomes,
            Some(self.mutation_divergence_log.as_ref()),
            expected_mutations,
            batch_parents,
            None,
            &NetParams::default(),
        )
    }

    /// Compute shadow state hash using a PRE-CAPTURED hasher snapshot.
    /// The snapshot must be taken BEFORE process_ledger writes to the real hasher.
    /// Returns our computed state root (`Some([u8; 32])`), or `None` if the hasher
    /// isn't built yet. Match/mismatch vs `network_hash` is still tracked in stats;
    /// the returned root lets the lockstep shadow recompute the full ledger_hash.
    pub fn check_shadow_hash(
        &self,
        hasher_snapshot: Box<dyn std::any::Any + Send>,
        overlay: &LedgerOverlay,
        network_hash: &str,
    ) -> Option<[u8; 32]> {
        let shadow = match crate::state_hash::StateHashComputer::shadow_hash_from_snapshot(
            hasher_snapshot, overlay,
        ) {
            Some(h) => h,
            None => return None,
        };
        let ours = hex::encode(shadow.0).to_uppercase();
        let net = network_hash.to_uppercase();
        let matched = ours == net;

        if !matched {
            let seq = self.stats.lock().round_ledger_seq;
            eprintln!("[ffi-shadow] MISMATCH ledger #{seq}: overlay keys: {}  ours: {}  network: {}", overlay.len(), &ours[..16], &net[..16]);
            // Diagnostic: dump first 3 overlay keys + data length for investigation
            let mut count = 0;
            for (key, val) in overlay.iter() {
                if count >= 3 { break; }
                let key_hex = hex::encode_upper(key);
                match val {
                    Some(data) => eprintln!("[ffi-shadow]   key={} len={} first8={}", &key_hex[..16], data.len(), hex::encode_upper(&data[..data.len().min(8)])),
                    None => eprintln!("[ffi-shadow]   key={} DELETED", &key_hex[..16]),
                }
                count += 1;
            }
            // Persist to shadow mismatch log
            self.log_shadow_mismatch(seq, &ours, &net, overlay.len());
        }
        let mut s = self.stats.lock();
        s.shadow_hash_attempted += 1;
        if matched {
            s.shadow_hash_matched += 1;
        } else {
            s.shadow_hash_mismatched += 1;
        }
        s.shadow_hash_last = ours;
        s.shadow_hash_last_network = net;
        s.shadow_hash_last_matched = matched;
        Some(shadow.0)
    }

    fn log_shadow_mismatch(&self, seq: u32, ours: &str, network: &str, overlay_keys: usize) {
        use std::io::Write;
        let path = std::env::var("XRPL_FFI_SHADOW_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("logs/shadow_mismatches.jsonl"));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = serde_json::json!({
            "ts": ts,
            "ledger_seq": seq,
            "our_hash": ours,
            "network_hash": network,
            "overlay_keys": overlay_keys,
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }

    /// Snapshot the shared `FfiStats` (cheap clone, holds the mutex briefly).
    pub fn stats(&self) -> FfiStats {
        self.stats.lock().clone()
    }

    /// Direct access to the shared stats handle (for consumers that want to
    /// avoid cloning each time, e.g. Prometheus render).
    pub fn shared_stats(&self) -> SharedFfiStats {
        Arc::clone(&self.stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMENDMENTS_KEY: &str = "7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4";

    fn id(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    /// Write an Amendments object with this enabled list into the state, as ws-sync does with the network's bytes.
    fn put_amendments(db: &rocksdb::DB, enabled: &[[u8; 32]]) {
        let ids: Vec<String> = enabled.iter().map(hex::encode_upper).collect();
        let sle = serde_json::json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": ids});
        db.put(hex::decode(AMENDMENTS_KEY).unwrap(), crate::meta_check::encode(&sle).unwrap()).unwrap();
    }

    /// A verifier that started with `startup` as its amendment set. Its RPC endpoint answers nothing.
    fn verifier(db: &Arc<rocksdb::DB>, startup: &[[u8; 32]]) -> FfiVerifier {
        FfiVerifier::with_amendments(vec!["http://127.0.0.1:1".into()], startup.to_vec(), Some(db.clone()))
    }

    /// rippled builds a ledger under its PARENT's rules (`rules_(prevLedger.rules_)`, Ledger.cpp:259): the ledger
    /// that carries the EnableAmendment pseudo-transaction runs under the old set to its last transaction, and
    /// the very next ledger under the new one. The native leg freezes its rules that way (ba74cb9); the libxrpl
    /// leg gets its Rules from the list the verifier hands the shim, one list per ledger, so the verifier must
    /// read that list from the pre-ledger state it verifies on, for every ledger and not only for flag ledgers.
    #[test]
    fn a_ledger_is_verified_under_its_parents_amendments() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(rocksdb::DB::open_default(dir.path()).unwrap());
        let (old, new) = (vec![id(0xAA), id(0xBB)], vec![id(0xAA), id(0xBB), id(0xCC)]);
        // An amendment is enabled in the ledger after a flag ledger.
        let activation = 256 * 400_000 + 1;

        put_amendments(&db, &old);
        let v = verifier(&db, &old);
        let before_activation = OwnedSnapshot::new(db.clone());
        // The activation ledger's writes land (its EnableAmendment rewrote the Amendments object).
        put_amendments(&db, &new);
        let after_activation = OwnedSnapshot::new(db.clone());

        assert_eq!(
            v.ledger_amendments(activation, Some(&before_activation)),
            old,
            "the activation ledger itself runs under its parent's set, whatever the state holds by now"
        );
        assert_eq!(
            v.ledger_amendments(activation + 1, Some(&after_activation)),
            new,
            "the ledger after the activation runs under the new set (it is not a flag ledger: the next one is 255 away)"
        );
        assert_eq!(
            v.ledger_amendments(activation, Some(&before_activation)),
            old,
            "a ledger re-driven on its own pre-state still gets its parent's set, not the last one seen"
        );
    }

    /// A pre-state with no readable Amendments object must not empty the rules (every amendment-gated
    /// transaction would diverge): the last set known stays in force.
    #[test]
    fn a_pre_state_without_an_amendments_object_keeps_the_last_known_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(rocksdb::DB::open_default(dir.path()).unwrap());
        let known = vec![id(0xAA), id(0xBB)];
        let v = verifier(&db, &known);
        let empty_state = OwnedSnapshot::new(db.clone());
        assert_eq!(v.ledger_amendments(256 * 400_000 + 2, Some(&empty_state)), known);
        db.put(hex::decode(AMENDMENTS_KEY).unwrap(), b"not an object").unwrap();
        let garbled = OwnedSnapshot::new(db.clone());
        assert_eq!(v.ledger_amendments(256 * 400_000 + 3, Some(&garbled)), known);
    }

    /// A stand-in for the RPC endpoint the catch-up path reads the parent's Amendments object from. `answer` maps
    /// the `ledger_index` asked for to the enabled list (None: accept the connection and never reply, as a hung
    /// rippled does). Returns the URL and the count of requests received.
    fn endpoint(answer: impl Fn(u32) -> Option<Vec<[u8; 32]>> + Send + 'static) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = requests.clone();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // One request per connection: headers, then a body of Content-Length bytes.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
                    let Ok(n) = stream.read(&mut chunk) else { break None };
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break Some(buf[end + 4..end + 4 + len].to_vec());
                    }
                };
                let Some(body) = body else { continue };
                counted.fetch_add(1, Ordering::SeqCst);
                let asked: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(asked["method"], "ledger_entry", "{asked}");
                assert_eq!(asked["params"][0]["index"], AMENDMENTS_KEY, "{asked}");
                let at: u32 = asked["params"][0]["ledger_index"].as_str().unwrap().parse().unwrap();
                match answer(at) {
                    Some(enabled) => {
                        let ids: Vec<String> = enabled.iter().map(hex::encode_upper).collect();
                        let reply = serde_json::json!({"result": {"ledger_index": at, "validated": true, "status": "success",
                            "node": {"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": ids}}})
                        .to_string();
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                            reply.len()
                        );
                    }
                    None => held.push(stream),
                }
            }
        });
        (url, requests)
    }

    /// A verifier on the RPC catch-up path (no state of its own), started with `startup`.
    fn rpc_verifier(url: String, startup: &[[u8; 32]]) -> FfiVerifier {
        FfiVerifier::with_amendments(vec![url], startup.to_vec(), None)
    }

    /// The Amendments object's enabled list is rewritten only by an EnableAmendment pseudo-transaction
    /// (Change::applyAmendment), so once the parent's list is known, every following ledger that ws-sync reports
    /// as carrying none runs under the same list: the catch-up path must not ask the endpoint again for it.
    #[test]
    fn ledgers_that_carry_no_amendment_change_cost_the_catch_up_path_no_read() {
        use std::sync::atomic::Ordering;
        let on_chain = vec![id(0xAA), id(0xBB), id(0xCC)];
        let served = on_chain.clone();
        let (url, requests) = endpoint(move |_| Some(served.clone()));
        let v = rpc_verifier(url, &[id(0xAA)]);
        let first = 256 * 400_000 + 7;
        for seq in first..first + 6 {
            assert_eq!(v.ledger_amendments(seq, None), on_chain, "#{seq}");
            v.note_ledger(seq, false);
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1, "one read for the first ledger's parent, none after it");
        // A ledger re-driven (held, then processed again) is still covered.
        assert_eq!(v.ledger_amendments(first + 3, None), on_chain);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    /// The ledger that carries the EnableAmendment runs under its parent's list; the one after it must be read
    /// again (its parent's object was rewritten), and from there the new list is the known one.
    #[test]
    fn the_ledger_after_an_amendment_change_is_read_again_on_the_catch_up_path() {
        use std::sync::atomic::Ordering;
        let (old, new) = (vec![id(0xAA), id(0xBB)], vec![id(0xAA), id(0xBB), id(0xCC)]);
        let activation = 256 * 400_000 + 1;
        let (was, is) = (old.clone(), new.clone());
        let (url, requests) = endpoint(move |at| Some(if at < activation { was.clone() } else { is.clone() }));
        let v = rpc_verifier(url, &old);

        assert_eq!(v.ledger_amendments(activation - 1, None), old);
        v.note_ledger(activation - 1, false);
        assert_eq!(v.ledger_amendments(activation, None), old, "the activation ledger runs under its parent's set");
        v.note_ledger(activation, true);
        assert_eq!(requests.load(Ordering::SeqCst), 1, "nothing changed up to the activation ledger's parent");

        assert_eq!(v.ledger_amendments(activation + 1, None), new, "the ledger after it runs under the new set");
        v.note_ledger(activation + 1, false);
        assert_eq!(requests.load(Ordering::SeqCst), 2, "its parent carried the change: read");
        assert_eq!(v.ledger_amendments(activation + 2, None), new);
        v.note_ledger(activation + 2, false);
        assert_eq!(requests.load(Ordering::SeqCst), 2);

        // Re-driven on the far side of the change: not covered by what is known now, so read, and right.
        assert_eq!(v.ledger_amendments(activation, None), old);
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    /// Ledgers ws-sync did not report (a jump: reconnect, resync) may hide a change: the first ledger past the
    /// gap reads its parent's object. So does a ledger whose predecessor was never noted at all.
    #[test]
    fn a_gap_in_the_ledgers_seen_is_read_across_on_the_catch_up_path() {
        use std::sync::atomic::Ordering;
        let (old, new) = (vec![id(0xAA)], vec![id(0xAA), id(0xDD)]);
        let seq = 256 * 400_000 + 40;
        let (was, is) = (old.clone(), new.clone());
        let (url, requests) = endpoint(move |at| Some(if at < seq + 3 { was.clone() } else { is.clone() }));
        let v = rpc_verifier(url, &old);
        assert_eq!(v.ledger_amendments(seq, None), old);
        v.note_ledger(seq, false);
        assert_eq!(v.ledger_amendments(seq + 5, None), new, "ledgers {}..{} were never seen", seq + 1, seq + 4);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        // Verified but never noted: the next ledger cannot lean on it.
        assert_eq!(v.ledger_amendments(seq + 6, None), new);
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    /// An endpoint that accepts and never answers must cost the libxrpl leg a small, bounded wait: the whole leg
    /// (this read, then the apply) runs inside ws-sync's 3 s compare wait under the native writer, and a ledger
    /// past it goes uncompared. The last set known stays in force, and once the endpoint answers again the read
    /// is made and used.
    #[test]
    fn a_dead_endpoint_costs_the_catch_up_path_a_bounded_wait_and_keeps_the_last_known_set() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (known, on_chain) = (vec![id(0xAA), id(0xBB)], vec![id(0xAA), id(0xBB), id(0xCC)]);
        let alive = Arc::new(AtomicBool::new(false));
        let (up, served) = (alive.clone(), on_chain.clone());
        let (url, requests) = endpoint(move |_| up.load(Ordering::SeqCst).then(|| served.clone()));
        let v = rpc_verifier(url, &known);
        let seq = 256 * 400_000 + 9;
        for seq in seq..seq + 2 {
            let t0 = std::time::Instant::now();
            assert_eq!(v.ledger_amendments(seq, None), known, "#{seq}: the last known set stays in force");
            let waited = t0.elapsed();
            assert!(waited < std::time::Duration::from_millis(1500), "#{seq}: waited {waited:?} on a dead endpoint");
            v.note_ledger(seq, false);
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2, "a failed read vouches for nothing: the next ledger asks again");
        alive.store(true, Ordering::SeqCst);
        assert_eq!(v.ledger_amendments(seq + 2, None), on_chain, "the endpoint answers again");
        // Nothing listening at all: refused at once, same outcome.
        let refused = rpc_verifier("http://127.0.0.1:1".into(), &known);
        assert_eq!(refused.ledger_amendments(seq, None), known);
    }
}
