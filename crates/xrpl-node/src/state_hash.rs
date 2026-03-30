//! State hash computation — build SHAMap from RocksDB, incrementally update,
//! and verify against network's account_hash each round.
//!
//! The SHAMap is kept in memory after the initial build so it can be
//! incrementally updated each round with only the modified entries.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;
use rayon::prelude::*;
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
    /// Persistent sorted leaf hashes — kept in memory, updated incrementally.
    /// Eliminates the 22s RocksDB scan on every round.
    pub leaf_cache: Arc<Mutex<Vec<(Hash256, Hash256)>>>,
    /// Cached hashes for each of the 16 root-level branches.
    /// Only dirty branches are recomputed each round.
    branch_hashes: Arc<Mutex<[Hash256; 16]>>,
    /// Bitmask of which branches need recomputing (bit N = branch N is dirty).
    pub dirty_branches: Arc<Mutex<u16>>,
}

impl StateHashComputer {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(StateHashStatus::default())),
            shamap: Arc::new(Mutex::new(None)),
            computing: Arc::new(AtomicBool::new(false)),
            entries_processed: Arc::new(AtomicU64::new(0)),
            consecutive_matches: Arc::new(AtomicU32::new(0)),
            leaf_cache: Arc::new(Mutex::new(Vec::new())),
            branch_hashes: Arc::new(Mutex::new([Hash256([0u8; 32]); 16])),
            dirty_branches: Arc::new(Mutex::new(0xFFFF)), // all dirty initially
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
            eprintln!("[state-hash] FAST BUILD: computing leaf hashes from RocksDB ({estimated_total} est.)...");

            use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};
            use xrpl_ledger::shamap::node::{ZERO_HASH, InnerNode, LeafNode, SHAMapNode};

            // Single pass: iterate RocksDB, compute leaf hashes immediately, discard data.
            // Only (key, leaf_hash) pairs are kept — no raw object data in memory.
            let mut leaf_hashes: Vec<(Hash256, Hash256)> = Vec::with_capacity(estimated_total as usize);
            let mut count: u64 = 0;
            let mut skipped: u64 = 0;

            let iter = db.iterator(rocksdb::IteratorMode::Start);
            for item in iter {
                match item {
                    Ok((key, value)) => {
                        if key.len() == 32 {
                            let mut key_arr = [0u8; 32];
                            key_arr.copy_from_slice(&key);
                            let hash_key = Hash256(key_arr);

                            // Compute leaf hash inline — data is never stored
                            // Rippled order: prefix || data || key
                            let mut buf = Vec::with_capacity(value.len() + 32);
                            buf.extend_from_slice(&value);
                            buf.extend_from_slice(&key_arr);
                            let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);

                            leaf_hashes.push((hash_key, leaf_hash));
                        } else {
                            skipped += 1;
                        }
                        count += 1;
                        if count % 1_000_000 == 0 {
                            entries_processed.store(count, Ordering::Relaxed);
                            let elapsed = start.elapsed().as_secs_f64();
                            let rate = count as f64 / elapsed;
                            eprintln!(
                                "[state-hash] Hashing: {count}/{estimated_total} ({:.1}%) — {rate:.0}/s",
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

            let total_entries = leaf_hashes.len();
            {
                let real_count = total_entries as u64;
                status.lock().entries_total = real_count;
                entries_processed.store(real_count, Ordering::Relaxed);
            }

            let pass1_time = start.elapsed().as_secs_f64();
            eprintln!(
                "[state-hash] {total_entries} leaf hashes computed in {pass1_time:.1}s ({skipped} skipped)",
            );

            // Build SHAMap using insert_hash_only (same tree structure as incremental updates).
            // This avoids the structural mismatch between bulk_build and insert_hash_only
            // that causes incremental updates to produce incorrect hashes.
            eprintln!("[state-hash] Building SHAMap tree via insert_hash_only...");
            let build_start = std::time::Instant::now();
            let mut shamap = SHAMap::new(TreeType::State);
            for (key, leaf_hash) in &leaf_hashes {
                let _ = shamap.insert_hash_only(*key, *leaf_hash);
            }
            let root_hash = shamap.root_hash();
            let hash_hex = hex::encode(root_hash.0);
            let build_time = build_start.elapsed().as_secs_f64();
            eprintln!(
                "[state-hash] SHAMap built in {build_time:.1}s — hash={hash_hex}",
            );

            drop(leaf_hashes);

            *shamap_slot.lock() = Some(shamap);

            let elapsed = start.elapsed().as_secs_f64();
            entries_processed.store(count, Ordering::Relaxed);
            eprintln!(
                "[state-hash] DONE: {total_entries} entries in {elapsed:.1}s — account_hash={hash_hex}",
            );

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

        use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE};

        let mut updated = 0;
        for key in modified_keys {
            match db.get(&key.0) {
                Ok(Some(data)) => {
                    // Compute leaf hash inline — don't store data in tree
                    // Rippled order: prefix || data || key
                    let mut buf = Vec::with_capacity(data.len() + 32);
                    buf.extend_from_slice(&data);
                    buf.extend_from_slice(&key.0);
                    let leaf_hash = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                    let _ = shamap.insert_hash_only(*key, leaf_hash);
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
                eprintln!(
                    "[state-hash] MISMATCH ledger #{ledger_seq}: ours={} network={}",
                    &s.computed_hash[..16],
                    &s.network_hash[..16.min(s.network_hash.len())],
                );
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

    /// Fast incremental hash: update changed entries in the cached sorted array,
    /// then recompute root hash. ~2.5s per round instead of 25s full scan.
    /// First call does a full scan to populate the cache.
    pub fn update_and_hash(&self, db: &Arc<rocksdb::DB>, modified_keys: &[Hash256]) -> Option<Hash256> {
        use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};
        use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};

        let start = std::time::Instant::now();
        let mut cache = self.leaf_cache.lock();

        // First call: populate cache from RocksDB
        if cache.is_empty() {
            eprintln!("[state-hash] First round — building leaf cache from RocksDB...");
            let snapshot = db.snapshot();
            let iter = snapshot.iterator(rocksdb::IteratorMode::Start);
            for item in iter {
                if let Ok((key, value)) = item {
                    if key.len() == 32 {
                        let mut k = [0u8; 32];
                        k.copy_from_slice(&key);
                        let mut buf = Vec::with_capacity(value.len() + 32);
                        buf.extend_from_slice(&value);
                        buf.extend_from_slice(&k);
                        let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                        cache.push((Hash256(k), lh));
                    }
                }
            }
            drop(snapshot);
            // Already sorted (RocksDB key order)
            let build_time = start.elapsed().as_secs_f64();
            eprintln!("[state-hash] Leaf cache built: {} entries in {build_time:.1}s", cache.len());
        } else {
            // Single-pass merge: collect changes, sort, merge into new Vec in one sweep.
            // O(n + k·log k) instead of O(k·n) from individual inserts/removes.
            let mut dirty: u16 = 0;
            let mut updates: Vec<(Hash256, Option<Hash256>)> = Vec::with_capacity(modified_keys.len());
            for key in modified_keys {
                dirty |= 1 << (nibble_at(key, 0) as u16);
                match db.get(&key.0) {
                    Ok(Some(data)) => {
                        let mut buf = Vec::with_capacity(data.len() + 32);
                        buf.extend_from_slice(&data);
                        buf.extend_from_slice(&key.0);
                        let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                        updates.push((*key, Some(lh)));
                    }
                    Ok(None) => {
                        updates.push((*key, None)); // delete
                    }
                    Err(_) => {}
                }
            }
            updates.sort_unstable_by(|a, b| a.0 .0.cmp(&b.0 .0));

            // Merge: single pass through old cache + sorted updates → new cache
            let mut new_cache = Vec::with_capacity(cache.len() + updates.len());
            let mut ci = 0usize;
            let mut ui = 0usize;
            while ci < cache.len() || ui < updates.len() {
                if ui >= updates.len() {
                    // No more updates — copy rest of cache
                    new_cache.extend_from_slice(&cache[ci..]);
                    break;
                } else if ci >= cache.len() {
                    // No more cache — insert remaining updates
                    for u in &updates[ui..] {
                        if let Some(h) = u.1 { new_cache.push((u.0, h)); }
                    }
                    break;
                } else if cache[ci].0 .0 < updates[ui].0 .0 {
                    new_cache.push(cache[ci]);
                    ci += 1;
                } else if cache[ci].0 .0 > updates[ui].0 .0 {
                    if let Some(h) = updates[ui].1 { new_cache.push((updates[ui].0, h)); }
                    ui += 1;
                } else {
                    // Same key — update or delete
                    if let Some(h) = updates[ui].1 { new_cache.push((updates[ui].0, h)); }
                    ci += 1;
                    ui += 1;
                }
            }
            *cache = new_cache;

            let mut db_dirty = self.dirty_branches.lock();
            *db_dirty |= dirty;
        }

        let update_time = start.elapsed().as_secs_f64();
        let count = cache.len();

        // Compute root hash from sorted cache (bottom-up, parallel at root level)
        fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
            if entries.is_empty() { return ZERO_HASH; }
            if entries.len() == 1 { return entries[0].1; }
            let mut child_hashes = [ZERO_HASH; 16];
            let mut pos = 0;
            for nibble in 0..16u8 {
                let end = entries[pos..].partition_point(|&(key, _)| nibble_at(&key, depth) <= nibble) + pos;
                let bucket_start = entries[pos..end].partition_point(|&(key, _)| nibble_at(&key, depth) < nibble) + pos;
                if bucket_start < end {
                    child_hashes[nibble as usize] = compute_subtree(&entries[bucket_start..end], depth + 1);
                }
                pos = end;
            }
            let mut data = [0u8; 16 * 32];
            for (i, h) in child_hashes.iter().enumerate() {
                data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
            }
            sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
        }

        // Only recompute dirty branches — massive speedup for incremental updates
        let mut dirty_bits = self.dirty_branches.lock();
        let dirty = *dirty_bits;
        *dirty_bits = 0; // reset
        drop(dirty_bits);

        let dirty_count = dirty.count_ones();

        // Partition cache into 16 root-level buckets
        let mut bucket_ranges: Vec<(usize, usize)> = Vec::with_capacity(16);
        let mut pos = 0;
        for nibble in 0..16u8 {
            let end = cache[pos..].partition_point(|&(key, _)| nibble_at(&key, 0) <= nibble) + pos;
            let bucket_start = cache[pos..end].partition_point(|&(key, _)| nibble_at(&key, 0) < nibble) + pos;
            bucket_ranges.push((bucket_start, end));
            pos = end;
        }

        let mut bh = self.branch_hashes.lock();

        if dirty == 0xFFFF {
            // All branches dirty (first call or cache rebuild) — compute all in parallel
            let buckets: Vec<&[(Hash256, Hash256)]> = bucket_ranges.iter()
                .map(|&(s, e)| &cache[s..e])
                .collect();
            let results: Vec<Hash256> = buckets.par_iter()
                .map(|bucket| compute_subtree(bucket, 1))
                .collect();
            for (i, h) in results.into_iter().enumerate() {
                bh[i] = h;
            }
        } else {
            // Only recompute dirty branches — typically 3-5 out of 16
            for nibble in 0..16u8 {
                if dirty & (1 << nibble) != 0 {
                    let (s, e) = bucket_ranges[nibble as usize];
                    bh[nibble as usize] = compute_subtree(&cache[s..e], 1);
                }
            }
        }

        let mut root_data = [0u8; 16 * 32];
        for (i, h) in bh.iter().enumerate() {
            root_data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }
        drop(bh);
        let root_hash = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &root_data);

        let hash_hex = hex::encode(root_hash.0);
        let total = start.elapsed().as_secs_f64();
        eprintln!("[state-hash] FAST: {count} entries in {total:.1}s (update={update_time:.1}s compute={:.1}s dirty={dirty_count}/16) — {}",
            total - update_time, &hash_hex[..16]);

        let mut s = self.status.lock();
        s.computed_hash = hash_hex;
        s.computing = false;

        Some(root_hash)
    }

    /// Compute root hash from RocksDB using sorted bottom-up approach.
    /// No tree structure — just sort + recursive hash computation.
    /// Proven correct: produces identical hash to insert_hash_only and rippled.
    /// Phase 1: scan RocksDB snapshot (~9s)
    /// Phase 2: parallel leaf hashing with rayon (~3s)
    /// Phase 3: sort + recursive bottom-up hash (~9s)
    /// Total: ~20s
    pub fn rebuild(&self, db: &Arc<rocksdb::DB>) {
        use rayon::prelude::*;
        use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};
        use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};

        let start = std::time::Instant::now();
        eprintln!("[state-hash] Computing hash (single-pass bottom-up)...");

        // Single pass: scan RocksDB snapshot + compute leaf hashes inline.
        // RocksDB iterates in key-sorted order — no sort needed.
        // No separate entries Vec — hash each value immediately, discard data.
        // Memory: only the leaf_hashes Vec (~1.2GB for 18.7M entries).
        let snapshot = db.snapshot();
        let mut leaf_hashes: Vec<(Hash256, Hash256)> = Vec::with_capacity(19_000_000);
        let iter = snapshot.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            if let Ok((key, value)) = item {
                if key.len() == 32 {
                    let mut k = [0u8; 32];
                    k.copy_from_slice(&key);
                    let mut buf = Vec::with_capacity(value.len() + 32);
                    buf.extend_from_slice(&value);
                    buf.extend_from_slice(&k);
                    let lh = sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf);
                    leaf_hashes.push((Hash256(k), lh));
                }
            }
        }
        drop(snapshot);
        let scan_time = start.elapsed().as_secs_f64();
        let count = leaf_hashes.len();
        // Already sorted (RocksDB key order) — no sort needed

        fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
            if entries.is_empty() { return ZERO_HASH; }
            if entries.len() == 1 { return entries[0].1; }
            let mut child_hashes = [ZERO_HASH; 16];
            let mut pos = 0;
            for nibble in 0..16u8 {
                let end = entries[pos..].partition_point(|&(key, _)| {
                    nibble_at(&key, depth) <= nibble
                }) + pos;
                let bucket_start = entries[pos..end].partition_point(|&(key, _)| {
                    nibble_at(&key, depth) < nibble
                }) + pos;
                if bucket_start < end {
                    child_hashes[nibble as usize] = compute_subtree(&entries[bucket_start..end], depth + 1);
                }
                pos = end;
            }
            let mut data = [0u8; 16 * 32];
            for (i, h) in child_hashes.iter().enumerate() {
                data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
            }
            sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data)
        }

        // Parallelize: split into 16 subtrees by first nibble, compute each in parallel
        let mut buckets: Vec<&[(Hash256, Hash256)]> = Vec::with_capacity(16);
        let mut pos = 0;
        for nibble in 0..16u8 {
            let end = leaf_hashes[pos..].partition_point(|&(key, _)| {
                nibble_at(&key, 0) <= nibble
            }) + pos;
            let bucket_start = leaf_hashes[pos..end].partition_point(|&(key, _)| {
                nibble_at(&key, 0) < nibble
            }) + pos;
            buckets.push(&leaf_hashes[bucket_start..end]);
            pos = end;
        }

        let child_hashes: Vec<Hash256> = buckets.par_iter()
            .map(|bucket| compute_subtree(bucket, 1))
            .collect();

        let mut root_data = [0u8; 16 * 32];
        for (i, h) in child_hashes.iter().enumerate() {
            root_data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }
        let root_hash = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &root_data);
        drop(leaf_hashes); // Free 1.2GB before logging
        let hash_hex = hex::encode(root_hash.0);
        let total = start.elapsed().as_secs_f64();
        eprintln!("[state-hash] DONE: {count} entries in {total:.1}s (scan+hash={scan_time:.1}s compute={:.1}s) — {hash_hex}",
            total - scan_time);

        // Force jemalloc to return freed memory to OS
        #[cfg(target_os = "linux")]
        unsafe { libc::malloc_trim(0); }

        let mut s = self.status.lock();
        s.computed_hash = hash_hex.clone();
        s.computing = false;

        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("[state-hash] REBUILD DONE: {count} entries in {elapsed:.1}s — hash={hash_hex}");
    }

    pub fn progress(&self) -> u64 {
        self.entries_processed.load(Ordering::Relaxed)
    }

    pub fn is_computing(&self) -> bool {
        self.computing.load(Ordering::SeqCst)
    }

    /// Accept a pre-built SHAMap (from inline download build).
    pub fn set_shamap(&self, map: SHAMap) {
        let root_hash = map.root_hash();
        let hash_hex = hex::encode(root_hash.0);
        *self.shamap.lock() = Some(map);
        let mut s = self.status.lock();
        s.computed_hash = hash_hex;
        s.computing = false;
        self.computing.store(false, Ordering::SeqCst);
    }

    /// Check if the SHAMap has been built.
    pub fn is_ready(&self) -> bool {
        self.shamap.lock().is_some()
    }
}

/// Insert a leaf into a SHAMap subtree starting at a given depth.
/// Used for parallel tree construction — each subtree starts at depth 1.
fn insert_at_depth(node: &mut xrpl_ledger::shamap::node::SHAMapNode, key: Hash256, leaf_hash: Hash256, depth: usize) {
    use xrpl_ledger::shamap::node::{InnerNode, LeafNode, SHAMapNode, nibble_at};

    if depth >= 64 { return; }

    match node {
        SHAMapNode::Inner(ref mut inner) => {
            let nibble = nibble_at(&key, depth);
            if inner.has_child(nibble) {
                let child = inner.get_child_node_mut(nibble).unwrap();
                insert_at_depth(child, key, leaf_hash, depth + 1);
            } else {
                inner.set_child_node(nibble, SHAMapNode::Leaf(LeafNode::new_hash_only(key, leaf_hash)));
            }
        }
        SHAMapNode::Leaf(ref existing) => {
            if existing.key() == &key {
                *node = SHAMapNode::Leaf(LeafNode::new_hash_only(key, leaf_hash));
            } else {
                let existing_key = *existing.key();
                let existing_nibble = nibble_at(&existing_key, depth);
                let new_nibble = nibble_at(&key, depth);

                let old_node = std::mem::replace(node, SHAMapNode::Inner(Box::<InnerNode>::default()));

                if let SHAMapNode::Inner(ref mut inner) = node {
                    if existing_nibble == new_nibble {
                        inner.set_child_node(existing_nibble, old_node);
                        let child = inner.get_child_node_mut(existing_nibble).unwrap();
                        insert_at_depth(child, key, leaf_hash, depth + 1);
                    } else {
                        inner.set_child_node(existing_nibble, old_node);
                        inner.set_child_node(new_nibble, SHAMapNode::Leaf(LeafNode::new_hash_only(key, leaf_hash)));
                    }
                }
            }
        }
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
