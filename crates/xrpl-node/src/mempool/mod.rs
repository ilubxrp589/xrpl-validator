//! Transaction mempool — validation, queuing, and relay.
//!
//! Validates incoming transactions, maintains a fee-priority queue,
//! deduplicates, and relays to connected peers.
