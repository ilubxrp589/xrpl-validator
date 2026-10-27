//! Independence Step 4's source switch, `XRPL_LEDGER_SOURCE`: where each ledger the engine applies comes from.
//!
//! - `rpc` (the default): .39's xrpld, as always: ws-sync's `ledgerClosed` stream and its `ledger` calls.
//! - `peers`: mainnet peers. The hash comes from our own tally of trusted validations, and each ledger arrives
//!   proven back to that hash ([`crate::peer_ledgers`]). No request goes to .39 or to a public server: the RPC
//!   client refuses every call and counts it ([`SourceStats::rpc_refused`]), so a code path that still asks .39
//!   shows up on /metrics instead of quietly leaning on it.
//!
//! Any other value reads as `rpc`, with one warning.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::{json, Value};

/// Where the engine's ledgers come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Rpc,
    Peers,
}

/// `XRPL_LEDGER_SOURCE`'s value as a source.
pub fn parse(v: Option<&str>) -> Source {
    match v.map(|s| s.trim().to_ascii_lowercase()) {
        None => Source::Rpc,
        Some(s) if s.is_empty() || s == "rpc" => Source::Rpc,
        Some(s) if s == "peers" => Source::Peers,
        Some(s) => {
            eprintln!("[ledger-source] XRPL_LEDGER_SOURCE={s:?} is neither rpc nor peers: using rpc");
            Source::Rpc
        }
    }
}

/// The source for this process, read once.
pub fn mode() -> Source {
    static MODE: OnceLock<Source> = OnceLock::new();
    *MODE.get_or_init(|| parse(std::env::var("XRPL_LEDGER_SOURCE").ok().as_deref()))
}

/// Whether ledgers come from peers.
pub fn peers() -> bool {
    mode() == Source::Peers
}

/// Peer mode's counters.
#[derive(Debug, Default)]
pub struct SourceStats {
    /// Ledgers whose inputs came from peers.
    pub from_peers: AtomicU64,
    /// Ledgers written from state objects peers sent, because the engine could not vouch for its own writes.
    pub keyed_fallbacks: AtomicU64,
    /// Ledger hashes found by walking parent hashes back from a ledger the quorum named.
    pub chain_walks: AtomicU64,
    /// Times the loop held a ledger peers had not delivered.
    pub holds: AtomicU64,
    /// RPC calls refused in peer mode: each one is a code path still asking .39.
    pub rpc_refused: AtomicU64,
}

pub fn stats() -> &'static SourceStats {
    static STATS: OnceLock<SourceStats> = OnceLock::new();
    STATS.get_or_init(SourceStats::default)
}

/// The total XRP (drops) of the newest ledger landed from peers: from its proven header, so the engine's network
/// total needs no .39 poll. 0 before the first.
pub static LAST_TOTAL_DROPS: AtomicU64 = AtomicU64::new(0);

/// The `ledger_source` section of /api/engine.
pub fn json() -> Value {
    let s = stats();
    json!({
        "mode": if peers() { "peers" } else { "rpc" },
        "from_peers": s.from_peers.load(Ordering::Relaxed),
        "keyed_fallbacks": s.keyed_fallbacks.load(Ordering::Relaxed),
        "chain_walks": s.chain_walks.load(Ordering::Relaxed),
        "holds": s.holds.load(Ordering::Relaxed),
        "rpc_refused": s.rpc_refused.load(Ordering::Relaxed),
    })
}

/// Prometheus lines for /metrics.
pub fn metrics_text() -> String {
    let s = stats();
    let mut out = String::new();
    out.push_str(&format!("xrpl_ledger_source_peers {}\n", u8::from(peers())));
    for (name, v) in [
        ("from_peers", &s.from_peers),
        ("keyed_fallbacks", &s.keyed_fallbacks),
        ("chain_walks", &s.chain_walks),
        ("holds", &s.holds),
        ("rpc_refused", &s.rpc_refused),
    ] {
        out.push_str(&format!("xrpl_ledger_source_{name}_total {}\n", v.load(Ordering::Relaxed)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_the_three_spellings() {
        assert_eq!(parse(None), Source::Rpc);
        assert_eq!(parse(Some("rpc")), Source::Rpc);
        assert_eq!(parse(Some("")), Source::Rpc);
        assert_eq!(parse(Some("peers")), Source::Peers);
        assert_eq!(parse(Some("PEERS ")), Source::Peers);
        assert_eq!(parse(Some("both")), Source::Rpc, "anything else is the default");
    }

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        for line in metrics_text().lines() {
            assert!(line.starts_with("xrpl_ledger_source_"), "{line:?}");
        }
    }
}
