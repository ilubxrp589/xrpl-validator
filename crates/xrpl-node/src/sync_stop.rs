//! Clean stop for ws-sync at a ledger boundary (warm restart,
//! docs/superpowers/specs/2026-09-24-warm-restart-design.md).
//!
//! ws-sync holds the gate while it processes one ledger (fetch, write, verify). The SIGTERM handler
//! requests the stop and then takes the gate: once it holds it, no ledger is in flight and none
//! will start, and `last_verified` names the ledger `state.rocks` holds.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

pub struct SyncStop {
    stop: AtomicBool,
    gate: tokio::sync::Mutex<()>,
    last_verified: parking_lot::Mutex<Option<(u32, String)>>,
}

/// The process-wide instance ws-sync and the SIGTERM handler share.
pub static SYNC_STOP: LazyLock<SyncStop> = LazyLock::new(SyncStop::new);

impl Default for SyncStop {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncStop {
    pub fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            gate: tokio::sync::Mutex::new(()),
            last_verified: parking_lot::Mutex::new(None),
        }
    }

    /// ws-sync, before each ledger: the guard to hold until the ledger is done, or `None` once a
    /// stop was requested (the caller parks and processes nothing more).
    pub async fn enter_ledger(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        let guard = self.gate.lock().await;
        if self.stop.load(Ordering::Acquire) {
            None
        } else {
            Some(guard)
        }
    }

    /// ws-sync, after a ledger landed and its root matched the network's account hash.
    pub fn record_verified(&self, seq: u32, account_hash: &str) {
        *self.last_verified.lock() = Some((seq, account_hash.to_string()));
    }

    pub fn last_verified(&self) -> Option<(u32, String)> {
        self.last_verified.lock().clone()
    }

    /// SIGTERM: request the stop and wait up to `timeout` for the ledger in flight to finish.
    /// `Some(last verified ledger)` once parked — the gate then stays held until the process
    /// exits; `None` on timeout or when no ledger has verified in this process.
    pub async fn request_stop_and_park(&self, timeout: Duration) -> Option<(u32, String)> {
        self.stop.store(true, Ordering::Release);
        match tokio::time::timeout(timeout, self.gate.lock()).await {
            Ok(guard) => {
                std::mem::forget(guard);
                self.last_verified()
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn parks_immediately_when_idle() {
        let s = SyncStop::new();
        s.record_verified(10, "AB");
        assert_eq!(s.request_stop_and_park(Duration::from_secs(1)).await, Some((10, "AB".to_string())));
    }

    #[tokio::test]
    async fn no_ticket_before_any_verified_ledger() {
        let s = SyncStop::new();
        assert_eq!(s.request_stop_and_park(Duration::from_secs(1)).await, None);
    }

    #[tokio::test]
    async fn waits_for_the_ledger_in_flight() {
        let s = Arc::new(SyncStop::new());
        s.record_verified(10, "A");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let worker = s.clone();
        let task = tokio::spawn(async move {
            let gate = worker.enter_ledger().await.expect("no stop requested yet");
            let _ = entered_tx.send(());
            tokio::time::sleep(Duration::from_millis(200)).await;
            worker.record_verified(11, "B");
            drop(gate);
        });
        entered_rx.await.unwrap();
        assert_eq!(s.request_stop_and_park(Duration::from_secs(5)).await, Some((11, "B".to_string())));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn times_out_when_a_ledger_hangs() {
        let s = Arc::new(SyncStop::new());
        s.record_verified(10, "A");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let worker = s.clone();
        tokio::spawn(async move {
            let _gate = worker.enter_ledger().await.expect("no stop requested yet");
            let _ = entered_tx.send(());
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        entered_rx.await.unwrap();
        assert_eq!(s.request_stop_and_park(Duration::from_millis(300)).await, None);
    }

    #[tokio::test]
    async fn no_ledger_starts_after_the_stop() {
        let s = SyncStop::new();
        s.record_verified(10, "A");
        assert!(s.request_stop_and_park(Duration::from_secs(1)).await.is_some());
        let next = tokio::time::timeout(Duration::from_millis(200), s.enter_ledger()).await;
        assert!(next.is_err(), "the parked gate stays held: ws-sync can never start another ledger");
    }
}
