//! Live engine — applies transactions to real ledger state each round.
//!
//! Opens the sled DB (synced state), and when a ledger closes, looks up
//! affected accounts, applies transactions via the sandbox, and commits
//! changes back to sled.

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

/// Live engine state.
pub struct LiveEngine {
    db: sled::Db,
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
}

impl LiveEngine {
    /// Open the sled DB at the given path.
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = sled::open(path).map_err(|e| format!("sled open: {e}"))?;
        eprintln!("[engine] Opened sled DB: {} entries", db.len());
        Ok(Self {
            db,
            ledger_seq: 0,
            total_coins: 99_985_687_626_634_189, // approximate current mainnet
            round_applied: 0,
            round_failed: 0,
            total_applied: 0,
        })
    }

    /// Look up a ledger object by its keylet hash.
    pub fn get(&self, key: &Hash256) -> Option<Vec<u8>> {
        self.db.get(key.0).ok()?.map(|v| v.to_vec())
    }

    /// Write a ledger object by its keylet hash.
    pub fn put(&self, key: &Hash256, data: &[u8]) -> Result<(), String> {
        self.db.insert(key.0, data).map_err(|e| format!("sled put: {e}"))?;
        Ok(())
    }

    /// Delete a ledger object.
    pub fn delete(&self, key: &Hash256) -> Result<(), String> {
        self.db.remove(key.0).map_err(|e| format!("sled delete: {e}"))?;
        Ok(())
    }

    /// Look up an account by its 20-byte ID, decode from binary to JSON.
    pub fn get_account_json(&self, account_id: &[u8; 20]) -> Option<serde_json::Value> {
        let key = xrpl_ledger::ledger::keylet::account_root_key(account_id);
        let data = self.get(&key)?;
        // Try to decode the binary XRPL format to JSON
        xrpl_core::codec::decode_transaction_binary(&data).ok()
    }

    /// Apply a decoded transaction to the state.
    /// Returns (success, fee_drops).
    pub fn apply_transaction(
        &mut self,
        tx_type: &str,
        account_id: &[u8; 20],
        fee: u64,
        fields: &serde_json::Value,
    ) -> (bool, u64) {
        // Look up sender account
        let acct_key = xrpl_ledger::ledger::keylet::account_root_key(account_id);
        let acct_data = match self.get(&acct_key) {
            Some(d) => d,
            None => {
                // Account not in our state — just track the fee
                self.round_failed += 1;
                return (false, fee);
            }
        };

        // Decode binary to JSON
        let mut acct_json = match xrpl_core::codec::decode_transaction_binary(&acct_data) {
            Ok(v) => v,
            Err(_) => {
                self.round_failed += 1;
                return (false, fee);
            }
        };

        // Deduct fee from balance
        let balance = acct_json.get("Balance")
            .and_then(|b| b.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if balance < fee {
            self.round_failed += 1;
            return (false, 0);
        }

        acct_json["Balance"] = serde_json::Value::String((balance - fee).to_string());

        // Re-encode to binary and write back
        // For now, store the JSON as bytes (not binary-encoded)
        // TODO: use encode_transaction_json for proper binary
        let updated = serde_json::to_vec(&acct_json).unwrap_or_default();
        let _ = self.put(&acct_key, &updated);

        // Track fee destruction
        self.total_coins = self.total_coins.saturating_sub(fee);
        self.round_applied += 1;
        self.total_applied += 1;

        (true, fee)
    }

    /// Called when a new ledger round starts.
    pub fn new_round(&mut self, ledger_seq: u32) {
        self.ledger_seq = ledger_seq;
        self.round_applied = 0;
        self.round_failed = 0;
    }

    /// Number of entries in the DB.
    pub fn entry_count(&self) -> usize {
        self.db.len()
    }
}
