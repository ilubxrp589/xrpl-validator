//! Choosing which rippled to hydrate a historical fixture from.
//!
//! **The corpora AGE OUT.** Our own .39 keeps a rolling window (~290k ledgers
//! as of 2026-08-10, `105919185-106209950`), so a fixture that hydrated locally
//! last month must come from s2 today. A server pinned per SWEEP SCRIPT cannot
//! express that, and on 2026-08-10 both failure directions were live:
//!
//!   * `batch2sweep.sh` pinned `--rpc <lan>` while 7 of its 26 ledgers had
//!     rolled out of the window. .39 answers `lgrNotFound` for all seven; they
//!     passed only because `~/loop/dxcache` still held answers from when they
//!     were in range. With `DX_CACHE=` #105909285 goes 51/51 ->
//!     all-DIVERGE-TER — a cache wipe would have turned a hydration gap into
//!     51 plausible "engine findings", the exact direction the oracle rule
//!     warns about.
//!   * an ad-hoc census sweep omitted `--rpc` entirely and spent 5h24m on s2
//!     for ledgers the LAN node held — the same 144 ledgers the gate did in
//!     40 min, because the disk cache keys on the SERVER and so shares nothing
//!     between an s2-bound run and a .39-bound one.
//!
//! Deciding per LEDGER means no caller can get it wrong, including ones that
//! pass nothing at all.

use serde_json::{json, Value};

/// Public history server. Reaches every ledger, at WAN latency.
pub const DEFAULT_RPC: &str = "https://s2.ripple.com:51234";

/// The LAN validator, preferred whenever it still holds the target ledger.
/// Overridable via `DX_LAN_RPC`.
pub fn lan_rpc() -> String {
    std::env::var("DX_LAN_RPC").unwrap_or_else(|_| "http://10.0.0.39:5005".to_string())
}

/// A node's `complete_ledgers`, UNCACHED — the window MOVES, so this is the one
/// lookup that must never be served from the probes' immutable-result cache.
/// `None` when the node is unreachable or reports no range.
pub fn ledger_window(url: &str) -> Option<Vec<(u32, u32)>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let body: Value = client
        .post(url)
        .json(&json!({"method": "server_info", "params": [{}]}))
        .send()
        .ok()?
        .json()
        .ok()?;
    parse_complete_ledgers(body["result"]["info"]["complete_ledgers"].as_str()?)
}

/// `"a-b,c-d"` (or `"empty"`) as inclusive ranges.
fn parse_complete_ledgers(s: &str) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() || part == "empty" {
            continue;
        }
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
            out.push((a, b));
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Choose the node to hydrate `at_seq` from: `preferred` if it still holds that
/// ledger, else [`DEFAULT_RPC`]. `None` means "no preference" — try the LAN node.
///
/// `--rpc` is therefore a PREFERENCE, not a pin: a script that hardcodes the
/// LAN node keeps working as its fixtures age out from under it.
pub fn select_rpc(preferred: Option<String>, at_seq: u32) -> String {
    let pref = preferred.unwrap_or_else(lan_rpc);
    match ledger_window(&pref) {
        Some(w) if w.iter().any(|(a, b)| at_seq >= *a && at_seq <= *b) => pref,
        Some(_) => {
            eprintln!("RPC: {pref} no longer holds ledger {at_seq}; hydrating from {DEFAULT_RPC}");
            DEFAULT_RPC.to_string()
        }
        None => {
            if pref != DEFAULT_RPC {
                eprintln!("RPC: {pref} unreachable; hydrating from {DEFAULT_RPC}");
            }
            DEFAULT_RPC.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_rippled_reports() {
        // .39 on 2026-08-10, and the multi-range form a resynced node shows.
        assert_eq!(
            parse_complete_ledgers("105919185-106209950"),
            Some(vec![(105_919_185, 106_209_950)])
        );
        assert_eq!(
            parse_complete_ledgers("32570-32571,105919185-106209950"),
            Some(vec![(32570, 32571), (105_919_185, 106_209_950)])
        );
        // A single sequence has no dash; "empty" and junk yield no window at
        // all, which `select_rpc` must read as "fall back", never as "covered".
        assert_eq!(parse_complete_ledgers("42"), Some(vec![(42, 42)]));
        assert_eq!(parse_complete_ledgers("empty"), None);
        assert_eq!(parse_complete_ledgers(""), None);
    }

    #[test]
    fn window_membership_is_inclusive_at_both_ends() {
        let w = parse_complete_ledgers("105919185-106209950").unwrap();
        let holds = |s: u32| w.iter().any(|(a, b)| s >= *a && s <= *b);
        assert!(holds(105_919_185) && holds(106_209_950));
        // The seven batch2 fixtures that had aged out by 2026-08-10.
        assert!(!holds(105_919_184));
        assert!(!holds(105_909_285));
        assert!(!holds(105_916_476));
    }
}
