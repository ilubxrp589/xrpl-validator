//! Live engine — applies transactions to real ledger state.
//!
//! Uses RocksDB for fast key-value lookups on 18.7M+ state objects.
//! RocksDB opens instantly regardless of dataset size.

use std::path::Path;
use std::sync::Arc;
use parking_lot::Mutex;
use xrpl_core::types::Hash256;

/// Decode an XRPL base58 address to 20-byte account ID.
pub fn decode_address(addr: &str) -> Option<[u8; 20]> {
    const ALPHABET: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let mut n: Vec<u8> = vec![0];
    for ch in addr.bytes() {
        let carry = ALPHABET.iter().position(|&c| c == ch)?;
        let mut c = carry;
        for byte in n.iter_mut().rev() {
            c += (*byte as usize) * 58;
            *byte = (c & 0xFF) as u8;
            c >>= 8;
        }
        while c > 0 {
            n.insert(0, (c & 0xFF) as u8);
            c >>= 8;
        }
    }
    let leading = addr.bytes().take_while(|&b| b == b'r').count();
    let mut result = vec![0u8; leading];
    result.extend_from_slice(&n);
    if result.len() < 25 { return None; }
    let mut account_id = [0u8; 20];
    account_id.copy_from_slice(&result[1..21]);
    Some(account_id)
}

/// Simple base58 encode for address display (not critical — just for logging).
fn bs58_encode(_bytes: &[u8]) -> String {
    hex::encode(_bytes)
}

/// Extract XRP Balance from raw binary AccountRoot.
/// Scans for field code 0x61 (Amount type=6, field=1 = Balance)
/// followed by 8 bytes of XRP amount encoding.
/// Public version for use in verification
pub fn extract_xrp_balance_pub(data: &[u8]) -> Option<u64> {
    extract_xrp_balance(data)
}

fn extract_xrp_balance(data: &[u8]) -> Option<u64> {
    // Scan for 0x61 byte (sfBalance = STI_AMOUNT | 1)
    // Start at offset 7 to skip LedgerEntryType (0x11 0x00 0x61) + Flags (0x22 + 4 bytes)
    let start = 7.min(data.len());
    for i in start..data.len().saturating_sub(9) {
        if data[i] == 0x61 {
            let amount_bytes: [u8; 8] = data[i+1..i+9].try_into().ok()?;
            let raw = u64::from_be_bytes(amount_bytes);
            if raw & 0x8000000000000000 != 0 { continue; }
            let drops = raw & 0x3FFFFFFFFFFFFFFF;
            return Some(drops);
        }
    }
    None
}

/// Set the XRP balance in a raw binary AccountRoot object.
/// Finds the 0x61 (Balance) field and overwrites the 8-byte amount in-place.
/// Returns the modified data, or None if the balance field wasn't found.
fn set_xrp_balance(data: &mut [u8], new_drops: u64) -> bool {
    let start = 7.min(data.len());
    for i in start..data.len().saturating_sub(9) {
        if data[i] == 0x61 {
            let amount_bytes: [u8; 8] = match data[i+1..i+9].try_into() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let raw = u64::from_be_bytes(amount_bytes);
            if raw & 0x8000000000000000 != 0 { continue; } // skip IOUs
            // Encode new XRP balance: bit 62 set = positive, bits 0-61 = drops
            let new_raw = if new_drops > 0 {
                0x4000000000000000 | new_drops
            } else {
                0x4000000000000000 // zero balance
            };
            data[i+1..i+9].copy_from_slice(&new_raw.to_be_bytes());
            return true;
        }
    }
    false
}

/// Extract the Destination account ID from a raw binary transaction.
/// Scans for field code 0x83 (STI_ACCOUNT type=8, field=3 = Destination)
/// followed by a VL-length byte (0x14 = 20 bytes) and 20-byte account ID.
pub fn extract_destination(tx_data: &[u8]) -> Option<[u8; 20]> {
    for i in 0..tx_data.len().saturating_sub(22) {
        if tx_data[i] == 0x83 && tx_data[i + 1] == 0x14 {
            let mut id = [0u8; 20];
            id.copy_from_slice(&tx_data[i + 2..i + 22]);
            return Some(id);
        }
    }
    None
}

/// Extract the Amount (XRP drops) from a raw binary transaction.
/// Scans for field code 0x61 (STI_AMOUNT type=6, field=1 = Amount).
pub fn extract_tx_amount(tx_data: &[u8]) -> Option<u64> {
    for i in 0..tx_data.len().saturating_sub(9) {
        if tx_data[i] == 0x61 {
            let amount_bytes: [u8; 8] = tx_data[i+1..i+9].try_into().ok()?;
            let raw = u64::from_be_bytes(amount_bytes);
            // Only XRP amounts (bit 63 = 0), positive (bit 62 = 1)
            if raw & 0x8000000000000000 != 0 { continue; } // IOU
            if raw & 0x4000000000000000 == 0 { continue; } // negative/zero
            let drops = raw & 0x3FFFFFFFFFFFFFFF;
            return Some(drops);
        }
    }
    None
}

/// Live engine state — RocksDB backed.
pub struct LiveEngine {
    db: Arc<rocksdb::DB>,
    fetch_tx: std::sync::mpsc::Sender<([u8; 20], [u8; 32])>,
    /// Current ledger sequence we're tracking.
    pub ledger_seq: u32,
    /// Total coins (for fee destruction).
    pub total_coins: u64,
    /// Transactions applied this round.
    pub round_applied: u32,
    /// Transactions failed this round.
    pub round_failed: u32,
    /// Total transactions applied since startup.
    pub total_applied: u64,
    /// Total failures since startup.
    pub total_failed: u64,
    /// Accounts modified this round (for verification).
    round_modified: Vec<([u8; 20], u64, u64)>, // (account_id, balance_before, fee)
    /// Keylets modified this round (for SHAMap incremental update).
    round_modified_keys: Vec<Hash256>,
    /// Verification stats.
    pub verified_match: u64,
    pub verified_mismatch: u64,
    pub verified_total: u64,
}

impl LiveEngine {
    /// Open the RocksDB at the given path.
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_max_open_files(256);
        opts.set_keep_log_file_num(2);
        // Optimize for reads
        opts.set_max_background_jobs(2);
        opts.optimize_for_point_lookup(64); // 64MB block cache

        let db = Arc::new(rocksdb::DB::open(&opts, path)
            .map_err(|e| format!("rocksdb open: {e}"))?);

        let count = db.property_value("rocksdb.estimate-num-keys")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        eprintln!("[engine] Opened RocksDB: ~{count} entries");

        // Background fetchers — multiple threads for concurrent RPC lookups
        let (fetch_tx, fetch_rx) = std::sync::mpsc::channel::<([u8; 20], [u8; 32])>();
        let fetch_rx = Arc::new(std::sync::Mutex::new(fetch_rx));
        for worker in 0..4 {
            let fetch_db = db.clone();
            let rx = fetch_rx.clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .expect("reqwest client builder failed");

                loop {
                    let (acct_id, key) = match rx.lock().unwrap().recv() {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let resp = client.post("https://s2.ripple.com:51234")
                        .json(&serde_json::json!({
                            "method": "ledger_entry",
                            "params": [{"index": hex::encode(key), "binary": true, "ledger_index": "validated"}]
                        }))
                        .send();

                    if let Ok(r) = resp {
                        if let Ok(body) = r.json::<serde_json::Value>() {
                            if let Some(data_hex) = body["result"]["node_binary"].as_str() {
                                if let Ok(data) = hex::decode(data_hex) {
                                    let _ = fetch_db.put(key, &data);
                                }
                            }
                        }
                    }
                }
            });
        }

        Ok(Self {
            db,
            fetch_tx,
            ledger_seq: 0,
            // Approximate total XRP in drops as of early 2026.
            // This is a bootstrap value — the network poll (server_info)
            // corrects it every ~30s with the actual on-ledger total.
            total_coins: 99_985_687_626_634_189,
            round_applied: 0,
            round_failed: 0,
            total_applied: 0,
            total_failed: 0,
            round_modified: Vec::new(),
            round_modified_keys: Vec::new(),
            verified_match: 0,
            verified_mismatch: 0,
            verified_total: 0,
        })
    }

    /// Queue a missing account for background fetch from RPC (deduplicated).
    fn queue_fetch(&self, account_id: &[u8; 20], key: &Hash256) {
        // Quick check: don't re-queue if already in DB (race with fetcher)
        if self.get(&key.0).is_some() { return; }
        let _ = self.fetch_tx.send((*account_id, key.0));
    }

    /// Look up a ledger object by its keylet hash.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db.get(key).ok()?
    }

    /// Write a ledger object.
    pub fn put(&self, key: &[u8], data: &[u8]) -> Result<(), String> {
        self.db.put(key, data).map_err(|e| format!("rocksdb put: {e}"))
    }

    /// Delete a ledger object.
    pub fn delete(&self, key: &[u8]) -> Result<(), String> {
        self.db.delete(key).map_err(|e| format!("rocksdb delete: {e}"))
    }

    /// Apply a decoded transaction to the state with full write-back.
    ///
    /// Deducts the fee from the sender's balance and writes it back to RocksDB.
    /// For XRP Payment transactions, also credits the destination account.
    /// Returns (success, fee_burned).
    pub fn apply_transaction(
        &mut self,
        tx_type: &str,
        account_id: &[u8; 20],
        fee: u64,
        raw_tx: &[u8],
    ) -> (bool, u64) {
        let acct_key = xrpl_ledger::ledger::keylet::account_root_key(account_id);

        let mut acct_data = match self.get(&acct_key.0) {
            Some(d) => d,
            None => {
                self.queue_fetch(account_id, &acct_key);
                self.round_failed += 1; self.total_failed += 1;
                if self.round_failed <= 3 {
                    eprintln!("[engine] MISS key={}", hex::encode(&acct_key.0[..8]));
                }
                return (false, fee);
            }
        };

        let balance = match extract_xrp_balance(&acct_data) {
            Some(b) => b,
            None => {
                let _ = self.db.delete(&acct_key.0);
                self.queue_fetch(account_id, &acct_key);
                self.round_failed += 1; self.total_failed += 1;
                return (false, fee);
            }
        };

        if balance < fee {
            self.round_failed += 1; self.total_failed += 1;
            return (false, 0);
        }

        // --- Fee deduction: debit sender and write back ---
        let new_sender_balance = balance - fee;

        // For Payment txs, also debit the sent amount
        let mut xrp_sent: u64 = 0;
        if tx_type == "Payment" {
            if let Some(amount) = extract_tx_amount(raw_tx) {
                if new_sender_balance >= amount {
                    xrp_sent = amount;
                }
                // If insufficient balance for amount, tx still succeeds for fee
                // (the payment itself would fail but fee is still burned)
            }
        }

        let final_sender_balance = new_sender_balance.saturating_sub(xrp_sent);
        if set_xrp_balance(&mut acct_data, final_sender_balance) {
            let _ = self.db.put(&acct_key.0, &acct_data);
            self.round_modified_keys.push(acct_key);
        }

        // --- Credit destination for XRP Payments ---
        if xrp_sent > 0 {
            if let Some(dest_id) = extract_destination(raw_tx) {
                let dest_key = xrpl_ledger::ledger::keylet::account_root_key(&dest_id);
                if let Some(mut dest_data) = self.get(&dest_key.0) {
                    if let Some(dest_balance) = extract_xrp_balance(&dest_data) {
                        let new_dest_balance = dest_balance.saturating_add(xrp_sent);
                        if set_xrp_balance(&mut dest_data, new_dest_balance) {
                            let _ = self.db.put(&dest_key.0, &dest_data);
                            self.round_modified_keys.push(dest_key);
                        }
                    }
                } else {
                    // Destination not in our DB — queue fetch
                    self.queue_fetch(&dest_id, &dest_key);
                }
            }
        }

        self.total_coins = self.total_coins.saturating_sub(fee);
        self.round_applied += 1;
        self.total_applied += 1;

        // Track for verification
        self.round_modified.push((*account_id, balance, fee));

        (true, fee)
    }

    /// Called when a new ledger round starts.
    /// Returns (accounts to verify, keylets modified) from the previous round.
    pub fn new_round(&mut self, ledger_seq: u32) -> (Vec<([u8; 20], u64, u64)>, Vec<Hash256>) {
        let prev_modified = std::mem::take(&mut self.round_modified);
        let prev_keys = std::mem::take(&mut self.round_modified_keys);
        self.ledger_seq = ledger_seq;
        self.round_applied = 0;
        self.round_failed = 0;
        (prev_modified, prev_keys)
    }

    /// Record verification result.
    pub fn record_verification(&mut self, matches: u32, mismatches: u32) {
        self.verified_match += matches as u64;
        self.verified_mismatch += mismatches as u64;
        self.verified_total += (matches + mismatches) as u64;
    }

    /// Get DB handle for background verification.
    pub fn db_ref(&self) -> &Arc<rocksdb::DB> {
        &self.db
    }

    /// Estimated entry count.
    pub fn entry_count(&self) -> usize {
        self.db.property_value("rocksdb.estimate-num-keys")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Get a snapshot of the DB for consistent iteration.
    /// Returns an Arc to the DB for use in background tasks.
    pub fn db_arc(&self) -> Arc<rocksdb::DB> {
        self.db.clone()
    }
}

/// Migrate data from sled to RocksDB (one-time).
pub fn migrate_sled_to_rocksdb(sled_path: &Path, rocks_path: &Path) -> Result<usize, String> {
    eprintln!("[migrate] Opening sled at {}...", sled_path.display());
    let sled_db = sled::open(sled_path).map_err(|e| format!("sled: {e}"))?;
    let count = sled_db.len();
    eprintln!("[migrate] Sled has {count} entries. Migrating to RocksDB...");

    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);
    opts.set_max_background_jobs(4);
    let rocks_db = rocksdb::DB::open(&opts, rocks_path)
        .map_err(|e| format!("rocksdb: {e}"))?;

    let mut migrated = 0;
    let mut batch = rocksdb::WriteBatch::default();

    for item in sled_db.iter() {
        let (key, val) = item.map_err(|e| format!("sled iter: {e}"))?;
        batch.put(&key, &val);
        migrated += 1;

        if migrated % 100_000 == 0 {
            rocks_db.write(batch).map_err(|e| format!("rocks write: {e}"))?;
            batch = rocksdb::WriteBatch::default();
            eprintln!("[migrate] {migrated}/{count} ({:.1}%)", migrated as f64 / count as f64 * 100.0);
        }
    }

    if !batch.is_empty() {
        rocks_db.write(batch).map_err(|e| format!("rocks write: {e}"))?;
    }

    eprintln!("[migrate] Done! {migrated} entries migrated.");
    Ok(migrated)
}
