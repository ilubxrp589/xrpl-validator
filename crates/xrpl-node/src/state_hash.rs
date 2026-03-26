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
    /// Uses a fast two-pass approach:
    /// Pass 1: iterate all entries, compute leaf hashes (I/O bound)
    /// Pass 2: build SHAMap from pre-sorted entries (CPU bound)
    /// Much faster than naive insert-one-at-a-time.
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
            eprintln!("[state-hash] FAST BUILD: collecting {estimated_total} entries from RocksDB...");

            // Pass 1: Collect all (key, value) pairs from RocksDB
            // RocksDB iterates in sorted key order which is ideal for SHAMap
            let mut entries: Vec<(Hash256, Vec<u8>)> = Vec::with_capacity(estimated_total as usize);
            let mut count: u64 = 0;
            let mut skipped: u64 = 0;

            let iter = db.iterator(rocksdb::IteratorMode::Start);
            for item in iter {
                match item {
                    Ok((key, value)) => {
                        if key.len() == 32 {
                            let mut key_arr = [0u8; 32];
                            key_arr.copy_from_slice(&key);
                            entries.push((Hash256(key_arr), value.to_vec()));
                        } else {
                            skipped += 1;
                        }
                        count += 1;
                        if count % 1_000_000 == 0 {
                            entries_processed.store(count, Ordering::Relaxed);
                            let elapsed = start.elapsed().as_secs_f64();
                            let rate = count as f64 / elapsed;
                            eprintln!(
                                "[state-hash] Pass 1: {count}/{estimated_total} ({:.1}%) — {rate:.0}/s",
                                count as f64 / estimated_total as f64 * 100.0,
                            );
                            status.lock().entries_processed = count;
                        }
                    }
                    Err(e) => {
                        skipped += 1;
                        if skipped <= 3 {
                            eprintln!("[state-hash] RocksDB error: {e}");
                        }
                    }
                }
            }

            // Update entries_total with REAL count (not the broken estimate)
            {
                let real_count = entries.len() as u64;
                status.lock().entries_total = real_count;
                entries_processed.store(real_count, Ordering::Relaxed);
            }

            let pass1_time = start.elapsed().as_secs_f64();
            eprintln!(
                "[state-hash] Pass 1 done: {} entries in {pass1_time:.1}s ({skipped} skipped). Computing hash...",
                entries.len(),
            );

            // Pass 2: Compute root hash directly (no tree allocation)
            // Compute leaf hashes, then build Merkle tree bottom-up by nibble grouping.
            use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};

            let total_entries = entries.len();

            // Compute all leaf hashes: SHA512Half("MLN\0" || key || data)
            let mut leaf_hashes: Vec<(Hash256, Hash256)> = Vec::with_capacity(total_entries);
            for (i, (key, data)) in entries.iter().enumerate() {
                let mut buf = Vec::with_capacity(32 + data.len());
                buf.extend_from_slice(&key.0);
                buf.extend_from_slice(data);
                let hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                leaf_hashes.push((*key, hash));
                if (i + 1) % 2_000_000 == 0 {
                    eprintln!(
                        "[state-hash] Pass 2a (leaf hashes): {}/{total_entries} ({:.1}%)",
                        i + 1, (i + 1) as f64 / total_entries as f64 * 100.0,
                    );
                }
            }
            // Free the raw data — we only need the hashes now
            drop(entries);

            let pass2a_time = start.elapsed().as_secs_f64();
            eprintln!(
                "[state-hash] Pass 2a done: {} leaf hashes in {:.1}s",
                leaf_hashes.len(), pass2a_time - pass1_time,
            );

            // Build Merkle tree bottom-up using recursive nibble grouping
            fn compute_inner_hash(
                leaves: &[(Hash256, Hash256)],  // (key, leaf_hash) sorted by key
                depth: usize,
            ) -> Hash256 {
                use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_INNER_NODE};
                use xrpl_ledger::shamap::node::ZERO_HASH;

                if leaves.is_empty() {
                    return ZERO_HASH;
                }
                if leaves.len() == 1 {
                    // In rippled's SHAMap, a single leaf in a subtree lives at
                    // whatever depth it becomes unique — its leaf hash goes directly
                    // into the parent inner node's child slot.
                    return leaves[0].1;
                }

                // Group by nibble at this depth
                let mut children: [Hash256; 16] = [ZERO_HASH; 16];
                let mut child_start = 0;

                for nibble in 0..16u8 {
                    // Find the range of entries with this nibble at `depth`
                    let end = leaves[child_start..].partition_point(|&(key, _)| {
                        let byte = key.0[depth / 2];
                        let n = if depth % 2 == 0 { (byte >> 4) & 0x0F } else { byte & 0x0F };
                        n <= nibble
                    }) + child_start;

                    let start_for_nibble = leaves[child_start..end].partition_point(|&(key, _)| {
                        let byte = key.0[depth / 2];
                        let n = if depth % 2 == 0 { (byte >> 4) & 0x0F } else { byte & 0x0F };
                        n < nibble
                    }) + child_start;

                    if start_for_nibble < end {
                        children[nibble as usize] = compute_inner_hash(
                            &leaves[start_for_nibble..end],
                            depth + 1,
                        );
                    }
                    child_start = end;
                }

                // Hash the inner node
                let mut data = Vec::with_capacity(16 * 32);
                for h in &children {
                    data.extend_from_slice(&h.0);
                }
                sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
            }

            // Sort by key (RocksDB already gives sorted order, but ensure it)
            // leaf_hashes is already sorted since RocksDB iterates in key order
            let root_hash = compute_inner_hash(&leaf_hashes, 0);
            let hash_hex = hex::encode(root_hash.0);


            let elapsed = start.elapsed().as_secs_f64();
            entries_processed.store(count, Ordering::Relaxed);
            eprintln!(
                "[state-hash] DONE: {total_entries} entries in {elapsed:.1}s — account_hash={hash_hex}",
            );

            // Build the full SHAMap for incremental updates.
            // This takes a while but only happens once at startup.
            eprintln!("[state-hash] Building full SHAMap for incremental updates...");
            let shamap_start = std::time::Instant::now();
            let mut shamap = SHAMap::new(TreeType::State);
            {
                // Re-iterate RocksDB to get (key, data) pairs for the SHAMap
                let iter = db.iterator(rocksdb::IteratorMode::Start);
                let mut inserted: u64 = 0;
                for item in iter {
                    if let Ok((key, value)) = item {
                        if key.len() == 32 {
                            let mut key_arr = [0u8; 32];
                            key_arr.copy_from_slice(&key);
                            let _ = shamap.insert(Hash256(key_arr), value.to_vec());
                            inserted += 1;
                            if inserted % 1_000_000 == 0 {
                                eprintln!(
                                    "[state-hash] SHAMap insert: {inserted}/{total_entries} ({:.1}%)",
                                    inserted as f64 / total_entries as f64 * 100.0,
                                );
                            }
                        }
                    }
                }
                eprintln!(
                    "[state-hash] SHAMap built: {} entries in {:.1}s",
                    inserted, shamap_start.elapsed().as_secs_f64(),
                );

                // Verify SHAMap root matches our computed hash
                let shamap_hash = hex::encode(shamap.root_hash().0);
                if shamap_hash == hash_hex {
                    eprintln!("[state-hash] SHAMap root hash MATCHES computed hash!");
                } else {
                    eprintln!(
                        "[state-hash] WARNING: SHAMap root hash DIFFERS!\n  computed: {hash_hex}\n  shamap:   {shamap_hash}",
                    );
                }
            }
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
