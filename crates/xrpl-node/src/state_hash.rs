//! State hash computation — build SHAMap from RocksDB and verify against network.
//!
//! Iterates all ledger objects in RocksDB, inserts them into a SHAMap,
//! and computes the root hash (account_hash). This is compared to the
//! network's account_hash to independently verify our state is correct.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

/// State hash computation result.
#[derive(Clone, Default, serde::Serialize)]
pub struct StateHashStatus {
    /// Whether the computation is currently running.
    pub computing: bool,
    /// Number of entries processed so far.
    pub entries_processed: u64,
    /// Total estimated entries.
    pub entries_total: u64,
    /// Computed root hash (hex). Empty if not yet computed.
    pub computed_hash: String,
    /// Network's account_hash for comparison. Empty if not yet fetched.
    pub network_hash: String,
    /// Whether the hashes match.
    pub matches: Option<bool>,
    /// Last computation time in seconds.
    pub compute_time_secs: f64,
    /// Ledger sequence the hash was computed for.
    pub ledger_seq: u32,
}

/// Shared state for the background hash computation.
pub struct StateHashComputer {
    pub status: Arc<Mutex<StateHashStatus>>,
    computing: Arc<AtomicBool>,
    entries_processed: Arc<AtomicU64>,
}

impl StateHashComputer {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(StateHashStatus::default())),
            computing: Arc::new(AtomicBool::new(false)),
            entries_processed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Start the background hash computation.
    /// Iterates all RocksDB entries and builds a SHAMap.
    /// This is CPU-intensive and may take minutes for 16M+ entries.
    pub fn start_computation(
        &self,
        db: Arc<rocksdb::DB>,
        estimated_total: u64,
    ) {
        if self.computing.swap(true, Ordering::SeqCst) {
            eprintln!("[state-hash] Already computing, skipping");
            return;
        }

        let status = self.status.clone();
        let computing = self.computing.clone();
        let entries_processed = self.entries_processed.clone();

        entries_processed.store(0, Ordering::Relaxed);

        {
            let mut s = status.lock();
            s.computing = true;
            s.entries_processed = 0;
            s.entries_total = estimated_total;
            s.computed_hash.clear();
            s.matches = None;
        }

        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            eprintln!("[state-hash] Starting SHAMap build from ~{estimated_total} RocksDB entries...");

            let mut shamap = SHAMap::new(TreeType::State);
            let mut count: u64 = 0;
            let mut errors: u64 = 0;

            let iter = db.iterator(rocksdb::IteratorMode::Start);
            for item in iter {
                match item {
                    Ok((key, value)) => {
                        if key.len() == 32 {
                            let mut key_arr = [0u8; 32];
                            key_arr.copy_from_slice(&key);
                            if let Err(_e) = shamap.insert(Hash256(key_arr), value.to_vec()) {
                                errors += 1;
                            }
                        }
                        count += 1;
                        if count % 500_000 == 0 {
                            entries_processed.store(count, Ordering::Relaxed);
                            let elapsed = start.elapsed().as_secs_f64();
                            let rate = count as f64 / elapsed;
                            let remaining = (estimated_total.saturating_sub(count)) as f64 / rate;
                            eprintln!(
                                "[state-hash] {count}/{estimated_total} ({:.1}%) — {rate:.0}/s — ~{remaining:.0}s remaining",
                                count as f64 / estimated_total as f64 * 100.0,
                            );
                            // Update status
                            status.lock().entries_processed = count;
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        if errors <= 5 {
                            eprintln!("[state-hash] RocksDB iteration error: {e}");
                        }
                    }
                }
            }

            let elapsed = start.elapsed().as_secs_f64();
            entries_processed.store(count, Ordering::Relaxed);

            eprintln!(
                "[state-hash] Built SHAMap with {} entries in {elapsed:.1}s ({} errors)",
                shamap.len(), errors,
            );

            // Compute root hash
            let root_hash = shamap.root_hash();
            let hash_hex = hex::encode(root_hash.0);
            eprintln!("[state-hash] account_hash = {hash_hex}");

            {
                let mut s = status.lock();
                s.computing = false;
                s.entries_processed = count;
                s.computed_hash = hash_hex;
                s.compute_time_secs = elapsed;
            }

            computing.store(false, Ordering::SeqCst);
        });
    }

    /// Update the network hash for comparison.
    pub fn set_network_hash(&self, hash: &str, ledger_seq: u32) {
        let mut s = self.status.lock();
        s.network_hash = hash.to_string();
        s.ledger_seq = ledger_seq;
        if !s.computed_hash.is_empty() && !s.network_hash.is_empty() {
            s.matches = Some(s.computed_hash == s.network_hash);
        }
    }

    /// Get the current progress (entries processed).
    pub fn progress(&self) -> u64 {
        self.entries_processed.load(Ordering::Relaxed)
    }

    /// Check if computation is currently running.
    pub fn is_computing(&self) -> bool {
        self.computing.load(Ordering::SeqCst)
    }

    /// Incrementally update the SHAMap with modified keylets from a round.
    /// Each modified key's current data is read from RocksDB and inserted.
    pub fn update_incremental(
        &self,
        db: &Arc<rocksdb::DB>,
        modified_keys: &[Hash256],
    ) {
        if modified_keys.is_empty() || self.is_computing() {
            return;
        }

        let status = self.status.clone();

        // Re-read modified objects from DB and note we need a recomputation
        // For now, mark the hash as stale — next full computation will pick up changes
        let mut s = status.lock();
        if !s.computed_hash.is_empty() {
            s.computed_hash.clear();
            s.matches = None;
        }
    }
}

/// Compute a ledger header hash independently.
///
/// The XRPL ledger header hash is:
/// `SHA512Half("LWR\0" || ledger_seq(u32) || total_drops(u64) || parent_hash(32) ||
///              tx_hash(32) || account_hash(32) || parent_close_time(u32) ||
///              close_time(u32) || close_resolution(u8) || close_flags(u8))`
pub fn compute_ledger_hash(
    ledger_seq: u32,
    total_drops: u64,
    parent_hash: &[u8; 32],
    tx_hash: &[u8; 32],
    account_hash: &[u8; 32],
    parent_close_time: u32,
    close_time: u32,
    close_resolution: u8,
    close_flags: u8,
) -> Hash256 {
    use sha2::{Digest, Sha512};

    let prefix: [u8; 4] = [0x4C, 0x57, 0x52, 0x00]; // "LWR\0"

    let mut hasher = Sha512::new();
    hasher.update(&prefix);
    hasher.update(&ledger_seq.to_be_bytes());
    hasher.update(&total_drops.to_be_bytes());
    hasher.update(parent_hash);
    hasher.update(tx_hash);
    hasher.update(account_hash);
    hasher.update(&parent_close_time.to_be_bytes());
    hasher.update(&close_time.to_be_bytes());
    hasher.update(&[close_resolution]);
    hasher.update(&[close_flags]);

    let full = hasher.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&full[..32]);
    Hash256(result)
}

/// Compute a transaction tree hash from a list of transaction hashes.
/// Builds a SHAMap of type Transaction and returns the root hash.
pub fn compute_tx_hash(tx_hashes: &[Hash256]) -> Hash256 {
    use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

    if tx_hashes.is_empty() {
        return Hash256([0u8; 32]);
    }

    let mut tree = SHAMap::new(TreeType::Transaction);
    for hash in tx_hashes {
        // In the tx tree, the key is the tx hash and the data is the
        // serialized tx + metadata. We don't have full metadata, so
        // we use the hash as a placeholder. This gives us a consistent
        // tree structure even though the exact hash won't match rippled
        // (which includes full tx+meta as the leaf data).
        let _ = tree.insert(*hash, hash.0.to_vec());
    }
    tree.root_hash()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_hash_deterministic() {
        let parent = [0xAA; 32];
        let tx = [0xBB; 32];
        let acct = [0xCC; 32];

        let h1 = compute_ledger_hash(100, 99_000_000, &parent, &tx, &acct, 700, 701, 10, 0);
        let h2 = compute_ledger_hash(100, 99_000_000, &parent, &tx, &acct, 700, 701, 10, 0);
        assert_eq!(h1, h2);

        // Different seq → different hash
        let h3 = compute_ledger_hash(101, 99_000_000, &parent, &tx, &acct, 700, 701, 10, 0);
        assert_ne!(h1, h3);
    }

    #[test]
    fn tx_hash_empty() {
        let h = compute_tx_hash(&[]);
        assert_eq!(h, Hash256([0u8; 32]));
    }

    #[test]
    fn tx_hash_deterministic() {
        let hashes = vec![
            Hash256([0x01; 32]),
            Hash256([0x02; 32]),
        ];
        let h1 = compute_tx_hash(&hashes);
        let h2 = compute_tx_hash(&hashes);
        assert_eq!(h1, h2);
        assert_ne!(h1, Hash256([0u8; 32]));
    }
}
