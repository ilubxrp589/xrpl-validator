//! Transaction mempool — fee-priority queue with dedup and expiry.

use std::collections::BTreeMap;
use std::cmp::Reverse;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::RwLock;
use xrpl_core::types::Hash256;

/// A transaction entry in the mempool.
#[derive(Debug, Clone)]
pub struct MempoolEntry {
    pub tx_blob: Vec<u8>,
    pub tx_hash: Hash256,
    pub fee: u64,
    pub added_at: Instant,
    pub last_ledger_seq: Option<u32>,
}

/// Key for fee-priority ordering: (fee descending, tx hash bytes).
type QueueKey = (Reverse<u64>, [u8; 32]);

/// Fee-priority transaction queue with deduplication and capacity limits.
pub struct TransactionQueue {
    /// Ordered by fee descending for deterministic dequeue.
    queue: RwLock<BTreeMap<QueueKey, MempoolEntry>>,
    /// Fast dedup lookup.
    seen: DashMap<Hash256, ()>,
    /// Maximum capacity.
    max_capacity: usize,
}

impl TransactionQueue {
    pub fn new(max_capacity: usize) -> Self {
        Self {
            queue: RwLock::new(BTreeMap::new()),
            seen: DashMap::new(),
            max_capacity,
        }
    }

    /// Add a transaction. Returns false if already seen (dedup).
    pub fn add(&self, entry: MempoolEntry) -> bool {
        let key = (Reverse(entry.fee), entry.tx_hash.0);
        let mut q = self.queue.write();

        // Dedup check inside write lock to prevent race condition
        if self.seen.contains_key(&entry.tx_hash) {
            return false;
        }

        // Capacity check — evict lowest-fee tx if full
        if q.len() >= self.max_capacity {
            if let Some((&lowest_key, _)) = q.iter().next_back() {
                // Only evict if new tx has higher fee
                if entry.fee <= lowest_key.0 .0 {
                    return false;
                }
                let evicted_hash = Hash256(lowest_key.1);
                q.remove(&lowest_key);
                self.seen.remove(&evicted_hash);
            }
        }

        self.seen.insert(entry.tx_hash, ());
        q.insert(key, entry);
        true
    }

    /// Take up to `max` highest-fee transactions for a consensus proposal.
    pub fn take_batch(&self, max: usize) -> Vec<MempoolEntry> {
        let q = self.queue.read();
        q.values().take(max).cloned().collect()
    }

    /// Remove a transaction (e.g., included in a validated ledger).
    pub fn remove(&self, hash: &Hash256) {
        self.seen.remove(hash);
        let mut q = self.queue.write();
        q.retain(|k, _| k.1 != hash.0);
    }

    /// Expire transactions whose LastLedgerSequence has passed.
    pub fn expire(&self, current_ledger: u32) {
        let mut q = self.queue.write();
        let before = q.len();
        q.retain(|_, entry| {
            if let Some(lls) = entry.last_ledger_seq {
                if lls < current_ledger {
                    self.seen.remove(&entry.tx_hash);
                    return false;
                }
            }
            true
        });
        let expired = before - q.len();
        if expired > 0 {
            tracing::debug!(expired, remaining = q.len(), "expired stale transactions");
        }
    }

    /// Check if a transaction hash is already in the mempool.
    pub fn contains(&self, hash: &Hash256) -> bool {
        self.seen.contains_key(hash)
    }

    /// Number of transactions in the queue.
    pub fn len(&self) -> usize {
        self.queue.read().len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.queue.read().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fee: u64, byte: u8) -> MempoolEntry {
        MempoolEntry {
            tx_blob: vec![byte; 50],
            tx_hash: Hash256([byte; 32]),
            fee,
            added_at: Instant::now(),
            last_ledger_seq: None,
        }
    }

    #[test]
    fn add_and_len() {
        let q = TransactionQueue::new(100);
        assert!(q.add(entry(10, 0xAA)));
        assert!(q.add(entry(20, 0xBB)));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn dedup() {
        let q = TransactionQueue::new(100);
        assert!(q.add(entry(10, 0xAA)));
        assert!(!q.add(entry(10, 0xAA))); // same hash
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn take_batch_ordered_by_fee() {
        let q = TransactionQueue::new(100);
        q.add(entry(5, 0x01));
        q.add(entry(50, 0x02));
        q.add(entry(20, 0x03));

        let batch = q.take_batch(10);
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].fee, 50); // highest first
        assert_eq!(batch[1].fee, 20);
        assert_eq!(batch[2].fee, 5);
    }

    #[test]
    fn capacity_evicts_lowest() {
        let q = TransactionQueue::new(2);
        q.add(entry(10, 0x01));
        q.add(entry(20, 0x02));

        // Queue full. New tx with fee=30 should evict fee=10
        assert!(q.add(entry(30, 0x03)));
        assert_eq!(q.len(), 2);
        assert!(!q.contains(&Hash256([0x01; 32]))); // evicted
        assert!(q.contains(&Hash256([0x03; 32]))); // added
    }

    #[test]
    fn capacity_rejects_low_fee() {
        let q = TransactionQueue::new(2);
        q.add(entry(10, 0x01));
        q.add(entry(20, 0x02));

        // New tx with fee=5 — lower than everything, rejected
        assert!(!q.add(entry(5, 0x03)));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn remove() {
        let q = TransactionQueue::new(100);
        q.add(entry(10, 0xAA));
        q.remove(&Hash256([0xAA; 32]));
        assert_eq!(q.len(), 0);
        assert!(!q.contains(&Hash256([0xAA; 32])));
    }

    #[test]
    fn expire() {
        let q = TransactionQueue::new(100);
        let mut e = entry(10, 0xAA);
        e.last_ledger_seq = Some(100);
        q.add(e);

        let mut e2 = entry(20, 0xBB);
        e2.last_ledger_seq = Some(200);
        q.add(e2);

        q.expire(150); // expires AA (lls=100), keeps BB (lls=200)
        assert_eq!(q.len(), 1);
        assert!(!q.contains(&Hash256([0xAA; 32])));
        assert!(q.contains(&Hash256([0xBB; 32])));
    }
}
