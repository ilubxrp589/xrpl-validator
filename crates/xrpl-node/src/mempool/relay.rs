//! Transaction relay — flood valid transactions with a seen-filter.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use xrpl_core::types::Hash256;

/// Tracks which transaction hashes we've already relayed.
/// Prevents duplicate relays across the network.
pub struct RelayFilter {
    /// Hash → time first seen.
    seen: DashMap<Hash256, Instant>,
    /// How long to remember a hash before allowing re-relay.
    ttl: Duration,
}

impl RelayFilter {
    pub fn new(ttl: Duration) -> Self {
        Self {
            seen: DashMap::new(),
            ttl,
        }
    }

    /// Check if we should relay this transaction.
    /// Returns true if not seen recently (and marks it as seen).
    /// Uses entry API for atomic check-and-insert to prevent races.
    pub fn should_relay(&self, hash: &Hash256) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.seen.entry(*hash) {
            Entry::Occupied(mut e) => {
                if e.get().elapsed() < self.ttl {
                    false // seen recently
                } else {
                    e.insert(Instant::now()); // expired, refresh
                    true
                }
            }
            Entry::Vacant(e) => {
                e.insert(Instant::now());
                true
            }
        }
    }

    /// Mark a hash as seen without checking.
    pub fn mark_seen(&self, hash: Hash256) {
        self.seen.insert(hash, Instant::now());
    }

    /// Remove expired entries to free memory.
    pub fn cleanup(&self) {
        self.seen.retain(|_, time| time.elapsed() < self.ttl);
    }

    /// Number of tracked hashes.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_relay_allowed() {
        let filter = RelayFilter::new(Duration::from_secs(300));
        let hash = Hash256([0xAA; 32]);
        assert!(filter.should_relay(&hash));
    }

    #[test]
    fn duplicate_blocked() {
        let filter = RelayFilter::new(Duration::from_secs(300));
        let hash = Hash256([0xBB; 32]);
        assert!(filter.should_relay(&hash));
        assert!(!filter.should_relay(&hash)); // second time blocked
    }

    #[test]
    fn different_hashes_independent() {
        let filter = RelayFilter::new(Duration::from_secs(300));
        let h1 = Hash256([0x01; 32]);
        let h2 = Hash256([0x02; 32]);
        assert!(filter.should_relay(&h1));
        assert!(filter.should_relay(&h2));
    }

    #[test]
    fn mark_seen() {
        let filter = RelayFilter::new(Duration::from_secs(300));
        let hash = Hash256([0xCC; 32]);
        filter.mark_seen(hash);
        assert!(!filter.should_relay(&hash));
    }

    #[test]
    fn cleanup_removes_expired() {
        let filter = RelayFilter::new(Duration::from_millis(1));
        let hash = Hash256([0xDD; 32]);
        filter.mark_seen(hash);
        assert_eq!(filter.len(), 1);

        std::thread::sleep(Duration::from_millis(5));
        filter.cleanup();
        assert_eq!(filter.len(), 0);
    }
}
