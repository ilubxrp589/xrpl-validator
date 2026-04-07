//! State hash computation — flat 256-bucket hasher with dirty tracking.
//!
//! Maintains 256 sorted Vec<(key, leaf_hash)> buckets indexed by key[0].
//! Only dirty buckets are recomputed each round via rayon parallelism.
//! Proven correct: same compute_subtree algorithm as the 28k-match run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;
use rayon::prelude::*;
use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_LEAF_NODE, HASH_PREFIX_INNER_NODE};
use xrpl_ledger::shamap::node::{ZERO_HASH, nibble_at};
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
    pub consecutive_matches: u32,
    pub ready_to_sign: bool,
    pub total_matches: u64,
    pub total_mismatches: u64,
    pub sync_log: Vec<SyncLogEntry>,
}

#[derive(Clone, serde::Serialize)]
pub struct SyncLogEntry {
    pub seq: u32,
    pub matched: bool,
    pub txs: u32,
    pub objs: u32,
    pub time_secs: f64,
    pub healed: bool,
}

// ---- Flat 256-bucket hasher ----

/// Recursive Merkle hash over a sorted slice of (key, leaf_hash) pairs.
/// Partitions by nibble at `depth`, recurses into sub-slices.
/// Base cases: empty → ZERO_HASH, single entry → leaf_hash (collapse).
fn compute_subtree(entries: &[(Hash256, Hash256)], depth: usize) -> Hash256 {
    if entries.is_empty() { return ZERO_HASH; }
    if entries.len() == 1 { return entries[0].1; }
    let mut ch = [ZERO_HASH; 16];
    let mut pos = 0;
    for n in 0..16u8 {
        let end = entries[pos..].partition_point(|&(k, _)| nibble_at(&k, depth) <= n) + pos;
        let bs = entries[pos..end].partition_point(|&(k, _)| nibble_at(&k, depth) < n) + pos;
        if bs < end { ch[n as usize] = compute_subtree(&entries[bs..end], depth + 1); }
        pos = end;
    }
    let mut d = [0u8; 16 * 32];
    for (i, h) in ch.iter().enumerate() { d[i * 32..(i + 1) * 32].copy_from_slice(&h.0); }
    sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &d)
}

/// Compute leaf hash from raw object data + key (rippled order: prefix || data || key).
#[inline]
fn leaf_hash(data: &[u8], key: &[u8; 32]) -> Hash256 {
    let mut buf = Vec::with_capacity(data.len() + 32);
    buf.extend_from_slice(data);
    buf.extend_from_slice(key);
    sha512_half_prefixed(&HASH_PREFIX_LEAF_NODE, &buf)
}

/// 65536-bucket hasher (depth-4) with dirty tracking and cached subtree hashes.
/// Index = first 2 bytes of key = 65536 buckets, ~286 entries each.
/// With ~500 modified keys/ledger, only ~500 of 65536 buckets are dirty (0.8%).
const NUM_BUCKETS: usize = 65536;

#[derive(Clone)]
struct FlatHasher {
    /// 65536 sorted vecs indexed by key[0..2] (first 2 bytes = depth-4 in trie).
    buckets: Vec<Vec<(Hash256, Hash256)>>,
    /// Cached subtree hash per bucket — only recomputed when dirty.
    bucket_hashes: Vec<Hash256>,
    /// Dirty flags per bucket.
    dirty: Vec<bool>,
    /// Total entry count across all buckets.
    total: usize,
}

impl FlatHasher {
    /// Bucket index from key: first 2 bytes = u16.
    #[inline]
    fn bucket_idx(key: &Hash256) -> usize {
        ((key.0[0] as usize) << 8) | (key.0[1] as usize)
    }

    /// Build from a RocksDB snapshot. RocksDB iterates in sorted key order,
    /// so entries within each bucket are already sorted.
    fn build_from_db(db: &rocksdb::DB) -> Self {
        let mut buckets: Vec<Vec<(Hash256, Hash256)>> = (0..NUM_BUCKETS).map(|_| Vec::with_capacity(300)).collect();
        let snapshot = db.snapshot();
        let mut total = 0usize;
        for item in snapshot.iterator(rocksdb::IteratorMode::Start) {
            if let Ok((key, value)) = item {
                if key.len() == 32 {
                    let mut k = [0u8; 32];
                    k.copy_from_slice(&key);
                    let h = Hash256(k);
                    buckets[Self::bucket_idx(&h)].push((h, leaf_hash(&value, &k)));
                    total += 1;
                    if total % 2_000_000 == 0 {
                        eprintln!("[state-hash] Scanning: {total}...");
                    }
                }
            }
        }
        drop(snapshot);

        // Compute all 65536 bucket hashes in parallel
        let bucket_hashes: Vec<Hash256> = buckets.par_iter()
            .map(|b| compute_subtree(b, 4))
            .collect();

        Self { buckets, bucket_hashes, dirty: vec![false; NUM_BUCKETS], total }
    }

    /// Update a single key. `new_hash` = Some for upsert, None for delete.
    #[inline]
    fn update(&mut self, key: Hash256, new_hash: Option<Hash256>) {
        let idx = Self::bucket_idx(&key);
        let bucket = &mut self.buckets[idx];
        match bucket.binary_search_by(|&(k, _)| k.0.cmp(&key.0)) {
            Ok(pos) => match new_hash {
                Some(h) => bucket[pos].1 = h,
                None => { bucket.remove(pos); self.total -= 1; }
            },
            Err(pos) => if let Some(h) = new_hash {
                bucket.insert(pos, (key, h));
                self.total += 1;
            },
        }
        self.dirty[idx] = true;
    }

    /// Compute root hash — only recomputes dirty buckets via rayon.
    /// With depth-4: 65536 buckets → 256 depth-2 → 16 depth-1 → 1 root.
    fn root_hash(&mut self) -> Hash256 {
        if self.total == 0 { return ZERO_HASH; }

        // Recompute only dirty buckets in parallel
        let dirty_work: Vec<(usize, &[(Hash256, Hash256)])> = (0..NUM_BUCKETS)
            .filter(|&i| self.dirty[i])
            .map(|i| (i, self.buckets[i].as_slice()))
            .collect();

        if !dirty_work.is_empty() {
            let new_hashes: Vec<(usize, Hash256)> = dirty_work.par_iter()
                .map(|&(i, slice)| (i, compute_subtree(slice, 4)))
                .collect();
            for (i, h) in new_hashes {
                self.bucket_hashes[i] = h;
                self.dirty[i] = false;
            }
        }

        // Combine: 65536 → 4096 → 256 → 16 → 1
        // Step 1: 65536 buckets → 4096 depth-3 hashes (group by first 3 nibbles)
        let mut depth3 = vec![ZERO_HASH; 4096];
        for d3 in 0..4096usize {
            let mut data = [0u8; 16 * 32];
            let mut any = false;
            for n in 0..16usize {
                let h = &self.bucket_hashes[d3 * 16 + n];
                data[n * 32..(n + 1) * 32].copy_from_slice(&h.0);
                if *h != ZERO_HASH { any = true; }
            }
            if any {
                depth3[d3] = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data);
            }
        }

        // Step 2: 4096 → 256 depth-2 hashes
        let mut depth2 = [ZERO_HASH; 256];
        for d2 in 0..256usize {
            let mut data = [0u8; 16 * 32];
            let mut any = false;
            for n in 0..16usize {
                let h = &depth3[d2 * 16 + n];
                data[n * 32..(n + 1) * 32].copy_from_slice(&h.0);
                if *h != ZERO_HASH { any = true; }
            }
            if any {
                depth2[d2] = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data);
            }
        }

        // Step 3: 256 → 16 depth-1 hashes
        let mut depth1 = [ZERO_HASH; 16];
        for n0 in 0..16usize {
            let mut data = [0u8; 16 * 32];
            let mut any = false;
            for n1 in 0..16usize {
                let h = &depth2[n0 * 16 + n1];
                data[n1 * 32..(n1 + 1) * 32].copy_from_slice(&h.0);
                if *h != ZERO_HASH { any = true; }
            }
            if any {
                depth1[n0] = sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &data);
            }
        }

        // Step 4: 16 → 1 root hash
        let mut root_data = [0u8; 16 * 32];
        for (i, h) in depth1.iter().enumerate() {
            root_data[i * 32..(i + 1) * 32].copy_from_slice(&h.0);
        }
        sha512_half_prefixed(&HASH_PREFIX_INNER_NODE, &root_data)
    }

    /// Save cache to disk as raw binary. Atomic: writes .tmp then renames.
    /// Format: "XRPLCACH" + version(u32) + count(u64) + entries(key[32] + hash[32])
    fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::io::{BufWriter, Write};
        let temp = path.with_extension("tmp");
        let f = std::fs::File::create(&temp)?;
        let mut w = BufWriter::with_capacity(8 * 1024 * 1024, f);
        w.write_all(b"XRPLCACH")?;
        w.write_all(&2u32.to_le_bytes())?; // v2: 65536 buckets
        w.write_all(&(self.total as u64).to_le_bytes())?;
        for bucket in &self.buckets {
            for (key, hash) in bucket {
                w.write_all(&key.0)?;
                w.write_all(&hash.0)?;
            }
        }
        w.flush()?;
        drop(w);
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// Load cache from disk. Returns None if file missing, corrupt, or wrong version.
    fn load(path: &Path) -> Option<Self> {
        let start = std::time::Instant::now();
        let data = std::fs::read(path).ok()?;
        if data.len() < 20 { return None; }
        if &data[..8] != b"XRPLCACH" { return None; }
        let ver = u32::from_le_bytes(data[8..12].try_into().ok()?);
        if ver != 2 {
            eprintln!("[state-hash] Cache version {ver} != 2 — rebuilding");
            return None;
        }
        let count = u64::from_le_bytes(data[12..20].try_into().ok()?) as usize;
        if data.len() != 20 + count * 64 {
            eprintln!("[state-hash] Cache file size mismatch: expected {} bytes, got {}", 20 + count * 64, data.len());
            return None;
        }

        let mut buckets: Vec<Vec<(Hash256, Hash256)>> = (0..NUM_BUCKETS).map(|_| Vec::with_capacity(count / NUM_BUCKETS + 1)).collect();
        let entries = &data[20..];
        for i in 0..count {
            let off = i * 64;
            let mut key = [0u8; 32];
            let mut hash = [0u8; 32];
            key.copy_from_slice(&entries[off..off + 32]);
            hash.copy_from_slice(&entries[off + 32..off + 64]);
            let h = Hash256(key);
            buckets[Self::bucket_idx(&h)].push((h, Hash256(hash)));
        }

        let read_time = start.elapsed().as_secs_f64();

        // Compute bucket hashes in parallel
        let bucket_hashes: Vec<Hash256> = buckets.par_iter()
            .map(|b| compute_subtree(b, 4))
            .collect();

        let total_time = start.elapsed().as_secs_f64();
        eprintln!("[state-hash] Cache loaded: {count} entries in {total_time:.1}s (read={read_time:.1}s hash={:.1}s)",
            total_time - read_time);
        Some(Self { buckets, bucket_hashes, dirty: vec![false; NUM_BUCKETS], total: count })
    }
}

// ---- StateHashComputer ----

pub struct StateHashComputer {
    pub status: Arc<Mutex<StateHashStatus>>,
    /// SHAMap — kept for set_shamap() compatibility (bulk download).
    pub shamap: Arc<Mutex<Option<SHAMap>>>,
    computing: Arc<AtomicBool>,
    entries_processed: Arc<AtomicU64>,
    pub consecutive_matches: Arc<AtomicU32>,
    /// Flat 256-bucket hasher — the hot path for hash computation.
    hasher: Arc<Mutex<Option<FlatHasher>>>,
    /// Path for persisting the leaf cache to disk.
    cache_path: Mutex<Option<PathBuf>>,
    // Legacy fields kept for API compat (unused in hot path)
    pub leaf_cache: Arc<Mutex<Vec<(Hash256, Hash256)>>>,
    pub dirty_branches: Arc<Mutex<u16>>,
    /// Live wallet (AccountRoot) count — scanned at startup, adjusted per-ledger.
    pub wallet_count: Arc<std::sync::atomic::AtomicU64>,
    /// Recent wallet count history (ledger_seq, count) for graphing. Ring buffer, last 300.
    pub wallet_history: Arc<Mutex<Vec<(u32, u64)>>>,
}

impl StateHashComputer {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(StateHashStatus::default())),
            shamap: Arc::new(Mutex::new(None)),
            computing: Arc::new(AtomicBool::new(false)),
            entries_processed: Arc::new(AtomicU64::new(0)),
            consecutive_matches: Arc::new(AtomicU32::new(0)),
            hasher: Arc::new(Mutex::new(None)),
            cache_path: Mutex::new(None),
            leaf_cache: Arc::new(Mutex::new(Vec::new())),
            dirty_branches: Arc::new(Mutex::new(0xFFFF)),
            wallet_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wallet_history: Arc::new(Mutex::new(Vec::with_capacity(300))),
        }
    }

    /// Clone the current FlatHasher for shadow hash computation.
    /// MUST be called BEFORE process_ledger modifies the real hasher.
    /// Returns None if hasher isn't built yet.
    pub fn snapshot_hasher(&self) -> Option<Box<dyn std::any::Any + Send>> {
        let guard = self.hasher.lock();
        guard.as_ref().map(|h| Box::new(h.clone()) as Box<dyn std::any::Any + Send>)
    }

    /// Compute shadow hash using a pre-captured hasher snapshot + overlay.
    pub fn shadow_hash_from_snapshot(
        hasher_snapshot: Box<dyn std::any::Any + Send>,
        overlay: &std::collections::HashMap<[u8; 32], Option<Vec<u8>>>,
    ) -> Option<Hash256> {
        let mut shadow = *hasher_snapshot.downcast::<FlatHasher>().ok()?;
        for (key, value) in overlay {
            let hash256_key = Hash256(*key);
            match value {
                Some(data) => shadow.update(hash256_key, Some(leaf_hash(data, key))),
                None => shadow.update(hash256_key, None),
            }
        }
        Some(shadow.root_hash())
    }

    /// Compute a shadow state hash by cloning the current hasher and applying
    /// the FFI overlay mutations on top. Returns `None` if the hasher isn't
    /// built yet (still syncing).
    ///
    /// This does NOT modify the real state hash — the clone is discarded after
    /// computing the root.
    pub fn shadow_hash(
        &self,
        overlay: &std::collections::HashMap<[u8; 32], Option<Vec<u8>>>,
    ) -> Option<Hash256> {
        let hasher_guard = self.hasher.lock();
        let hasher = hasher_guard.as_ref()?;
        let mut shadow = hasher.clone();
        drop(hasher_guard);

        for (key, value) in overlay {
            let hash256_key = Hash256(*key);
            match value {
                Some(data) => {
                    let lh = leaf_hash(data, key);
                    shadow.update(hash256_key, Some(lh));
                }
                None => {
                    shadow.update(hash256_key, None);
                }
            }
        }

        Some(shadow.root_hash())
    }

    /// Scan RocksDB for AccountRoot entries (sfLedgerEntryType == 0x0061).
    /// Called once at startup after bulk sync completes. Typically takes 5-10s
    /// over 18.8M entries.
    pub fn scan_wallet_count(&self, db: &rocksdb::DB) {
        let t0 = std::time::Instant::now();
        let mut count: u64 = 0;
        let iter = db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let Ok((_key, value)) = item else { continue; };
            // AccountRoot SLE: first 2 bytes after the outer type marker
            // Binary SLE format: 0x11 0x00 <LedgerEntryType u16>
            // AccountRoot = 0x0061 → bytes [0]=0x11, [1]=0x00, [2]=0x61
            if value.len() >= 3 && value[0] == 0x11 && value[1] == 0x00 && value[2] == 0x61 {
                count += 1;
            }
        }
        self.wallet_count.store(count, std::sync::atomic::Ordering::Relaxed);
        eprintln!("[wallets] Scanned {count} AccountRoot entries in {:.1}s", t0.elapsed().as_secs_f64());
    }

    /// Adjust wallet count when ledger processing finds created/deleted AccountRoots.
    pub fn adjust_wallet_count(&self, created: i64, ledger_seq: u32) {
        use std::sync::atomic::Ordering;
        if created > 0 {
            self.wallet_count.fetch_add(created as u64, Ordering::Relaxed);
        } else if created < 0 {
            self.wallet_count.fetch_sub((-created) as u64, Ordering::Relaxed);
        }
        let current = self.wallet_count.load(Ordering::Relaxed);
        let mut hist = self.wallet_history.lock();
        hist.push((ledger_seq, current));
        // Keep last 300 entries (~15 min at 3s/ledger)
        if hist.len() > 300 {
            let excess = hist.len() - 300;
            hist.drain(..excess);
        }
    }

    /// Set the path for persisting the leaf cache.
    pub fn set_cache_path(&self, path: PathBuf) {
        *self.cache_path.lock() = Some(path);
    }

    /// Save the current cache to disk (call periodically or on shutdown).
    pub fn save_cache(&self) {
        let path = self.cache_path.lock().clone();
        if let Some(ref path) = path {
            if let Some(ref h) = *self.hasher.lock() {
                match h.save(path) {
                    Ok(()) => eprintln!("[state-hash] Cache saved: {} entries to {}", h.total, path.display()),
                    Err(e) => eprintln!("[state-hash] Cache save failed: {e}"),
                }
            }
        }
    }

    /// Background SHAMap build (used by start_computation path).
    pub fn start_computation(&self, db: Arc<rocksdb::DB>, estimated_total: u64) {
        if self.computing.swap(true, Ordering::SeqCst) { return; }

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

            use xrpl_ledger::shamap::node::{InnerNode, LeafNode, SHAMapNode};

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
                            leaf_hashes.push((Hash256(key_arr), leaf_hash(&value, &key_arr)));
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
                        if skipped <= 3 { eprintln!("[state-hash] RocksDB error: {e}"); }
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
            eprintln!("[state-hash] {total_entries} leaf hashes computed in {pass1_time:.1}s ({skipped} skipped)");

            eprintln!("[state-hash] Building SHAMap tree via insert_hash_only...");
            let build_start = std::time::Instant::now();
            let mut shamap = SHAMap::new(TreeType::State);
            for (key, lh) in &leaf_hashes {
                let _ = shamap.insert_hash_only(*key, *lh);
            }
            let root_hash = shamap.root_hash();
            let hash_hex = hex::encode(root_hash.0);
            let build_time = build_start.elapsed().as_secs_f64();
            eprintln!("[state-hash] SHAMap built in {build_time:.1}s — hash={hash_hex}");

            drop(leaf_hashes);
            *shamap_slot.lock() = Some(shamap);

            let elapsed = start.elapsed().as_secs_f64();
            entries_processed.store(count, Ordering::Relaxed);
            eprintln!("[state-hash] DONE: {total_entries} entries in {elapsed:.1}s — account_hash={hash_hex}");

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

    /// Incremental update via flat hasher. Falls back to SHAMap if hasher not built.
    pub fn update_round(&self, db: &Arc<rocksdb::DB>, modified_keys: &[Hash256]) -> Option<Hash256> {
        // Try flat hasher first
        let mut guard = self.hasher.lock();
        if let Some(ref mut h) = *guard {
            for key in modified_keys {
                match db.get(&key.0) {
                    Ok(Some(data)) => h.update(*key, Some(leaf_hash(&data, &key.0))),
                    Ok(None) => h.update(*key, None),
                    Err(_) => {}
                }
            }
            let root = h.root_hash();
            self.status.lock().computed_hash = hex::encode(root.0);
            return Some(root);
        }
        drop(guard);

        // Fallback: SHAMap path
        if modified_keys.is_empty() {
            return self.shamap.lock().as_ref().map(|m| m.root_hash());
        }
        let mut map_guard = self.shamap.lock();
        let shamap = map_guard.as_mut()?;
        for key in modified_keys {
            match db.get(&key.0) {
                Ok(Some(data)) => { let _ = shamap.insert_hash_only(*key, leaf_hash(&data, &key.0)); }
                Ok(None) => { let _ = shamap.delete(key); }
                Err(_) => {}
            }
        }
        let root = shamap.root_hash();
        self.status.lock().computed_hash = hex::encode(root.0);
        Some(root)
    }

    pub fn set_network_hash(&self, hash: &str, ledger_seq: u32) -> bool {
        let mut s = self.status.lock();
        s.network_hash = hash.to_string();
        s.ledger_seq = ledger_seq;
        if !s.computed_hash.is_empty() && !s.network_hash.is_empty() {
            let matches = s.computed_hash.to_uppercase() == s.network_hash.to_uppercase();
            s.matches = Some(matches);
            if matches {
                // SECURITY(5.3): Use AcqRel ordering — this value gates ready_to_sign
                let n = self.consecutive_matches.fetch_add(1, Ordering::AcqRel) + 1;
                s.consecutive_matches = n;
                s.ready_to_sign = n >= 3;
                if n <= 5 || n % 100 == 0 {
                    eprintln!("[state-hash] MATCH #{n}! Our hash matches network for ledger #{ledger_seq}");
                }
                // Auto-save cache every 500 matches
                if n % 500 == 0 && n > 0 {
                    let hasher = self.hasher.clone();
                    let path = self.cache_path.lock().clone();
                    if let Some(path) = path {
                        std::thread::spawn(move || {
                            if let Some(ref h) = *hasher.lock() {
                                match h.save(&path) {
                                    Ok(()) => eprintln!("[state-hash] Auto-saved cache: {} entries", h.total),
                                    Err(e) => eprintln!("[state-hash] Auto-save failed: {e}"),
                                }
                            }
                        });
                    }
                }
            } else {
                self.consecutive_matches.store(0, Ordering::Release);
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

    pub fn push_sync_log(&self, entry: SyncLogEntry) {
        let mut s = self.status.lock();
        if entry.matched { s.total_matches += 1; } else { s.total_mismatches += 1; }
        s.sync_log.insert(0, entry);
        s.sync_log.truncate(100);
    }

    pub fn is_ready_to_sign(&self) -> bool {
        self.consecutive_matches.load(Ordering::Acquire) >= 3
    }

    pub fn current_hash(&self) -> Option<Hash256> {
        let guard = self.hasher.lock();
        if guard.is_some() {
            let hash_hex = &self.status.lock().computed_hash;
            if hash_hex.len() == 64 {
                let mut bytes = [0u8; 32];
                if hex::decode_to_slice(hash_hex, &mut bytes).is_ok() {
                    return Some(Hash256(bytes));
                }
            }
            return None;
        }
        drop(guard);
        self.shamap.lock().as_ref().map(|m| m.root_hash())
    }

    /// Flat bucketed hash — O(log n) updates, only dirty buckets recomputed.
    /// Build: ~10s from RocksDB. Incremental: ~50-100ms per ledger.
    pub fn update_and_hash(&self, db: &Arc<rocksdb::DB>, modified_keys: &[Hash256]) -> Option<Hash256> {
        let start = std::time::Instant::now();
        let mut guard = self.hasher.lock();

        // Lazy build — try disk cache first, then RocksDB scan
        if guard.is_none() {
            // Check RocksDB entry count to validate cache freshness
            let db_count = db.property_value("rocksdb.estimate-num-keys")
                .ok().flatten()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            // Always build from RocksDB — fast enough (~20s) and avoids stale cache issues.
            // Cache is still SAVED on SIGTERM and every 500 matches for future optimization.
            eprintln!("[state-hash] Building 65536-bucket hasher from RocksDB...");
            let h = FlatHasher::build_from_db(db);
            eprintln!("[state-hash] Hasher built: {} entries in {:.1}s", h.total, start.elapsed().as_secs_f64());
            *guard = Some(h);
        }

        let hasher = guard.as_mut().unwrap();
        let build_time = start.elapsed().as_secs_f64();

        // Update modified keys — O(log n) binary search per key
        let mut updated = 0u32;
        for key in modified_keys {
            match db.get(&key.0) {
                Ok(Some(data)) => { hasher.update(*key, Some(leaf_hash(&data, &key.0))); updated += 1; }
                Ok(None) => { hasher.update(*key, None); updated += 1; }
                Err(_) => {}
            }
        }

        let update_time = start.elapsed().as_secs_f64();

        // Compute root hash — only dirty buckets recomputed
        let root = hasher.root_hash();
        let n_dirty = hasher.dirty.iter().filter(|&&d| d).count(); // after root_hash, should be 0

        let hash_hex = hex::encode(root.0);
        let total = start.elapsed().as_secs_f64();
        let count = hasher.total;
        eprintln!("[state-hash] FAST: {count} entries in {total:.3}s (update={:.3}s compute={:.3}s keys={updated}) — {}",
            update_time - build_time, total - update_time, &hash_hex[..16]);

        let mut s = self.status.lock();
        s.computed_hash = hash_hex;
        s.compute_time_secs = total - update_time;
        s.computing = false;
        Some(root)
    }

    /// Invalidate — forces rebuild from RocksDB on next call.
    pub fn invalidate_tree(&self) {
        *self.hasher.lock() = None;
        *self.shamap.lock() = None;
        // Delete disk cache to prevent stale load on next restart
        if let Some(ref path) = *self.cache_path.lock() {
            let _ = std::fs::remove_file(path);
        }
        eprintln!("[state-hash] Hasher + cache cleared — will rebuild from RocksDB");
    }

    /// Full rebuild from RocksDB (used by inc-sync backfill).
    pub fn rebuild(&self, db: &Arc<rocksdb::DB>) {
        let start = std::time::Instant::now();
        eprintln!("[state-hash] Computing hash (single-pass bottom-up)...");

        let snapshot = db.snapshot();
        let mut leaf_hashes: Vec<(Hash256, Hash256)> = Vec::with_capacity(19_000_000);
        for item in snapshot.iterator(rocksdb::IteratorMode::Start) {
            if let Ok((key, value)) = item {
                if key.len() == 32 {
                    let mut k = [0u8; 32];
                    k.copy_from_slice(&key);
                    leaf_hashes.push((Hash256(k), leaf_hash(&value, &k)));
                }
            }
        }
        drop(snapshot);
        let scan_time = start.elapsed().as_secs_f64();
        let count = leaf_hashes.len();

        // 16-way parallel hash
        let mut buckets: Vec<&[(Hash256, Hash256)]> = Vec::with_capacity(16);
        let mut pos = 0;
        for nibble in 0..16u8 {
            let end = leaf_hashes[pos..].partition_point(|&(key, _)| nibble_at(&key, 0) <= nibble) + pos;
            let bs = leaf_hashes[pos..end].partition_point(|&(key, _)| nibble_at(&key, 0) < nibble) + pos;
            buckets.push(&leaf_hashes[bs..end]);
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
        drop(leaf_hashes);
        let hash_hex = hex::encode(root_hash.0);
        let total = start.elapsed().as_secs_f64();
        eprintln!("[state-hash] DONE: {count} entries in {total:.1}s (scan+hash={scan_time:.1}s compute={:.1}s) — {hash_hex}",
            total - scan_time);

        #[cfg(target_os = "linux")]
        unsafe { libc::malloc_trim(0); }

        let mut s = self.status.lock();
        s.computed_hash = hash_hex.clone();
        s.computing = false;
        eprintln!("[state-hash] REBUILD DONE: {count} entries in {total:.1}s — hash={hash_hex}");
    }

    pub fn progress(&self) -> u64 { self.entries_processed.load(Ordering::Relaxed) }
    pub fn is_computing(&self) -> bool { self.computing.load(Ordering::SeqCst) }

    pub fn set_shamap(&self, map: SHAMap) {
        let root_hash = map.root_hash();
        let hash_hex = hex::encode(root_hash.0);
        *self.shamap.lock() = Some(map);
        let mut s = self.status.lock();
        s.computed_hash = hash_hex;
        s.computing = false;
        self.computing.store(false, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.hasher.lock().is_some() || self.shamap.lock().is_some()
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
    let prefix: [u8; 4] = [0x4C, 0x57, 0x52, 0x00];
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

    #[test]
    fn compute_subtree_empty() {
        assert_eq!(compute_subtree(&[], 0), ZERO_HASH);
    }

    #[test]
    fn compute_subtree_single() {
        let h = Hash256([0x42; 32]);
        let entries = [(Hash256([0xAB; 32]), h)];
        assert_eq!(compute_subtree(&entries, 0), h);
    }

    #[test]
    fn compute_subtree_deterministic() {
        let entries = vec![
            (Hash256([0x10; 32]), Hash256([0xAA; 32])),
            (Hash256([0x20; 32]), Hash256([0xBB; 32])),
        ];
        let h1 = compute_subtree(&entries, 0);
        let h2 = compute_subtree(&entries, 0);
        assert_eq!(h1, h2);
        assert_ne!(h1, ZERO_HASH);
    }
}
