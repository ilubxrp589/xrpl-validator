//! What this node calls itself on the network, and how it reads other nodes' names.
//!
//! The version is `0.<minor>.<patch>` plus the commit it was built from. 0.3.0 was the July 2026 hybrid release;
//! 0.4.0 is the independence push. 1.0.0 is kept for the day it signs ledgers it computed itself.

/// The peer handshake's User-Agent and our /crawl `build_version`, e.g. `xrpl-rust-validator/0.4.0+56f2653`.
/// The commit comes from build.rs (`XRPL_BUILD_COMMIT`).
pub const PEER_VERSION: &str = concat!("xrpl-rust-validator/0.4.0+", env!("XRPL_BUILD_COMMIT"));

/// The label for a peer, from its public /crawl answer. A node that hides its server section is "hidden": its
/// `overlay.active` list holds ITS peers' versions, not its own (that list once labelled three hubs with ours).
pub fn crawl_label(body: &serde_json::Value) -> String {
    match body.pointer("/server/build_version").and_then(|v| v.as_str()) {
        Some(v) if v.starts_with(|c: char| c.is_ascii_digit()) => format!("rippled-{v}"),
        Some(v) if !v.is_empty() => v.to_string(),
        _ => "hidden".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rippled_number_gets_the_prefix() {
        assert_eq!(crawl_label(&json!({"server": {"build_version": "3.4.1"}})), "rippled-3.4.1");
    }

    #[test]
    fn a_named_build_is_kept_as_is() {
        let body = json!({"server": {"build_version": "xrpl-rust-validator/0.4.0+56f2653"}});
        assert_eq!(crawl_label(&body), "xrpl-rust-validator/0.4.0+56f2653");
    }

    #[test]
    fn hidden_server_never_borrows_a_peers_version() {
        // The XRP Kuwait hubs' shape on 2026-10-14: no server section, our own node first in their overlay.
        let body = json!({"overlay": {"active": [{"version": "xrpl-rust-validator/0.1.0"}]}, "version": 2});
        assert_eq!(crawl_label(&body), "hidden");
        assert_eq!(crawl_label(&json!({"server": {"build_version": ""}})), "hidden");
    }

    #[test]
    fn peer_version_carries_the_commit() {
        let commit = PEER_VERSION.strip_prefix("xrpl-rust-validator/0.4.0+").expect("name/version prefix");
        assert!(!commit.is_empty() && !commit.contains('$'), "commit stamp: {commit:?}");
    }
}
