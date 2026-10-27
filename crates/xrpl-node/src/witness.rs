//! Independence Step 4: an optional witness for the peer source, `XRPL_WITNESS_RPC=<url>`.
//!
//! With peers as the ledger source nothing the validator applies or signs comes from .39. While .39 still runs, it
//! can still check: for every ledger landed from peers, the witness asks that server for the ledger's hash (no
//! transactions) and holds it against ours, off the loop, on its own task. A differ is logged and counted. The
//! answer also goes to our own tally as the network's word ([`crate::own_validated::network_validated`]), so its
//! agree and differ counts stay live. It never feeds the data path, and an unreachable witness only counts as
//! unreachable: switching .39 off costs the validator nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

/// `XRPL_WITNESS_RPC`, read once.
pub fn url() -> Option<&'static str> {
    static URL: OnceLock<Option<String>> = OnceLock::new();
    URL.get_or_init(|| std::env::var("XRPL_WITNESS_RPC").ok().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()))
        .as_deref()
}

pub fn enabled() -> bool {
    url().is_some()
}

#[derive(Debug, Default)]
pub struct WitnessStats {
    pub agree: AtomicU64,
    pub differ: AtomicU64,
    pub unreachable: AtomicU64,
    /// Ledgers dropped because the witness had fallen 64 behind.
    pub dropped: AtomicU64,
}

pub fn stats() -> &'static WitnessStats {
    static S: OnceLock<WitnessStats> = OnceLock::new();
    S.get_or_init(WitnessStats::default)
}

/// The ledger hash in a `ledger` reply, only when the server calls that ledger validated.
pub fn parse_hash(body: &Value) -> Option<[u8; 32]> {
    let r = &body["result"];
    if r["validated"].as_bool() != Some(true) {
        return None;
    }
    let h = r["ledger"]["ledger_hash"].as_str().or_else(|| r["ledger_hash"].as_str())?;
    hex::decode(h).ok()?.try_into().ok()
}

/// Ledger `seq` landed from peers with hash `ours`: the witness checks it, if there is one. Never blocks.
pub fn landed(seq: u32, ours: [u8; 32]) {
    let Some(url) = url() else { return };
    static TX: OnceLock<tokio::sync::mpsc::Sender<(u32, [u8; 32])>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(run(url, rx));
        eprintln!("[witness] ON — each ledger landed from peers is checked against {url} (never fed from it)");
        tx
    });
    if tx.try_send((seq, ours)).is_err() {
        stats().dropped.fetch_add(1, Ordering::Relaxed);
    }
}

async fn run(url: &'static str, mut rx: tokio::sync::mpsc::Receiver<(u32, [u8; 32])>) {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_default();
    let st = stats();
    while let Some((seq, ours)) = rx.recv().await {
        let body = json!({"method": "ledger", "params": [{"ledger_index": seq}]});
        let theirs = match client.post(url).json(&body).send().await {
            Ok(r) => r.json::<Value>().await.ok().as_ref().and_then(parse_hash),
            Err(_) => None,
        };
        match theirs {
            None => {
                st.unreachable.fetch_add(1, Ordering::Relaxed);
            }
            Some(t) if t == ours => {
                st.agree.fetch_add(1, Ordering::Relaxed);
                crate::own_validated::network_validated(seq, t);
            }
            Some(t) => {
                st.differ.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[witness] #{seq}: the witness says {}, the ledger we took from peers is {}",
                    &hex::encode_upper(t)[..16],
                    &hex::encode_upper(ours)[..16]
                );
                crate::own_validated::network_validated(seq, t);
            }
        }
    }
}

/// The `witness` section of /api/engine.
pub fn json() -> Value {
    let st = stats();
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    json!({
        "enabled": enabled(),
        "agree": l(&st.agree),
        "differ": l(&st.differ),
        "unreachable": l(&st.unreachable),
        "dropped": l(&st.dropped),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hash_takes_only_validated_replies() {
        let h = "AB".repeat(32);
        assert_eq!(parse_hash(&json!({"result": {"validated": true, "ledger": {"ledger_hash": h}}})), Some([0xAB; 32]));
        assert_eq!(parse_hash(&json!({"result": {"validated": true, "ledger_hash": h}})), Some([0xAB; 32]));
        assert_eq!(parse_hash(&json!({"result": {"validated": false, "ledger": {"ledger_hash": h}}})), None);
        assert_eq!(parse_hash(&json!({"result": {"ledger": {"ledger_hash": h}}})), None);
        assert_eq!(parse_hash(&json!({"result": {"validated": true, "ledger": {"ledger_hash": "XYZ"}}})), None);
    }

    #[test]
    fn landed_is_a_noop_when_disabled() {
        if !enabled() {
            let before = stats().dropped.load(Ordering::Relaxed);
            landed(1, [0; 32]);
            assert_eq!(stats().dropped.load(Ordering::Relaxed), before);
        }
    }
}
