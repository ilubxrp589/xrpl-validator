//! State hash computation — build SHAMap from RocksDB, incrementally update,
//! and verify against network's account_hash each round.
//!
//! The SHAMap is kept in memory after the initial build so it can be
//! incrementally updated each round with only the modified entries.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::tree::{SHAMap, TreeType};

/// State hash computation result.
#[derive(Clone, Default, serde::Serialize)]
pub struct StateHashStatus {
    pub computing: bool,
    pub entries_processed: u64,
    pub entries_total: u64,
    pub computed_hash: String,
    pub network_hash: String,
    pub matches: Option<bool>,
    pub compute_time_secs: f64,
    pub ledger_seq: u32,
    /// How many consecutive rounds the hash matched.
    pub consecutive_matches: u32,
    /// Whether we're ready to sign with our own hash.
    pub ready_to_sign: bool,
}

/// Shared state for the background hash computation.
pub struct StateHashComputer {
    pub status: Arc<Mutex<StateHashStatus>>,
    /// The live SHAMap — kept in memory for incremental updates.
    shamap: Arc<Mutex<Option<SHAMap>>>,
    computing: Arc<AtomicBool>,
    entries_processed: Arc<AtomicU64>,
    /// Consecutive rounds where our hash matched the network.
    consecutive_matches: Arc<AtomicU32>,
}

impl StateHashComputer {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(StateHashStatus::default())),
            shamap: Arc::new(Mutex::new(None)),
            computing: Arc::new(AtomicBool::new(false)),
            entries_processed: Arc::new(AtomicU64::new(0)),
            consecutive_matches: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Start the background SHAMap build from all RocksDB entries.
    /// The SHAMap is kept in memory after completion for incremental updates.
    pub fn start_computation(
        &self,
        db: Arc<rocksdb::DB>,
        estimated_total: u64,
    ) {
        if self.computing.swap(true, Ordering::SeqCst) {
            return;
        }

        let status = self.status.clone();
        let shamap_slot = self.shamap.clone();
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
            eprintln!("[state-hash] Building SHAMap from ~{estimated_total} RocksDB entries...");

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
                            if shamap.insert(Hash256(key_arr), value.to_vec()).is_err() {
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

            let root_hash = shamap.root_hash();
            let hash_hex = hex::encode(root_hash.0);
            eprintln!(
                "[state-hash] SHAMap built: {} entries in {elapsed:.1}s — account_hash={}",
                shamap.len(), &hash_hex[..16],
            );

            // Store the SHAMap for incremental updates
            *shamap_slot.lock() = Some(shamap);

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

    /// Incrementally update the SHAMap with modified entries after a round.
    /// Reads the current data for each modified key from RocksDB and updates the tree.
    /// Returns the new root hash, or None if the SHAMap isn't ready yet.
    pub fn update_round(
        &self,
        db: &Arc<rocksdb::DB>,
        modified_keys: &[Hash256],
    ) -> Option<Hash256> {
        if modified_keys.is_empty() {
            // No changes — return current hash
            let guard = self.shamap.lock();
            return guard.as_ref().map(|m| m.root_hash());
        }

        let mut guard = self.shamap.lock();
        let shamap = guard.as_mut()?;

        let mut updated = 0;
        for key in modified_keys {
            match db.get(&key.0) {
                Ok(Some(data)) => {
                    let _ = shamap.insert(*key, data.to_vec());
                    updated += 1;
                }
                Ok(None) => {
                    // Object deleted — remove from SHAMap
                    let _ = shamap.delete(key);
                }
                Err(_) => {}
            }
        }

        let root = shamap.root_hash();
        let hash_hex = hex::encode(root.0);

        // Update status
        {
            let mut s = self.status.lock();
            s.computed_hash = hash_hex;
        }

        if updated > 0 {
            eprintln!(
                "[state-hash] Incremental update: {updated}/{} keys — hash={}",
                modified_keys.len(),
                hex::encode(&root.0[..8]),
            );
        }

        Some(root)
    }

    /// Compare our hash against the network's. Returns true if they match.
    pub fn set_network_hash(&self, hash: &str, ledger_seq: u32) -> bool {
        let mut s = self.status.lock();
        s.network_hash = hash.to_string();
        s.ledger_seq = ledger_seq;

        if !s.computed_hash.is_empty() && !s.network_hash.is_empty() {
            let matches = s.computed_hash.to_uppercase() == s.network_hash.to_uppercase();
            s.matches = Some(matches);
            if matches {
                let n = self.consecutive_matches.fetch_add(1, Ordering::Relaxed) + 1;
                s.consecutive_matches = n;
                s.ready_to_sign = n >= 3; // 3 consecutive matches = ready
                if n <= 5 || n % 100 == 0 {
                    eprintln!("[state-hash] MATCH #{n}! Our hash matches network for ledger #{ledger_seq}");
                }
            } else {
                self.consecutive_matches.store(0, Ordering::Relaxed);
                s.consecutive_matches = 0;
                s.ready_to_sign = false;
            }
            matches
        } else {
            false
        }
    }

    /// Check if we're ready to sign with our own hash (3+ consecutive matches).
    pub fn is_ready_to_sign(&self) -> bool {
        self.consecutive_matches.load(Ordering::Relaxed) >= 3
    }

    /// Get the current computed hash.
    pub fn current_hash(&self) -> Option<Hash256> {
        let guard = self.shamap.lock();
        guard.as_ref().map(|m| m.root_hash())
    }

    pub fn progress(&self) -> u64 {
        self.entries_processed.load(Ordering::Relaxed)
    }

    pub fn is_computing(&self) -> bool {
        self.computing.load(Ordering::SeqCst)
    }

    /// Check if the SHAMap has been built.
    pub fn is_ready(&self) -> bool {
        self.shamap.lock().is_some()
    }
}

/// Compute a ledger header hash independently.
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
pub fn compute_tx_hash(tx_hashes: &[Hash256]) -> Hash256 {
    if tx_hashes.is_empty() {
        return Hash256([0u8; 32]);
    }
    let mut tree = SHAMap::new(TreeType::Transaction);
    for hash in tx_hashes {
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
        let hashes = vec![Hash256([0x01; 32]), Hash256([0x02; 32])];
        let h1 = compute_tx_hash(&hashes);
        let h2 = compute_tx_hash(&hashes);
        assert_eq!(h1, h2);
        assert_ne!(h1, Hash256([0u8; 32]));
    }
}
