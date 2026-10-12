//! UNL (Unique Node List) fetcher.
//!
//! Downloads and parses published validator lists (e.g., vl.ripple.com).
//! Format:
//! ```json
//! {
//!   "public_key": "...",
//!   "manifest": "...",
//!   "blob": "<base64 JSON with validators array>",
//!   "signature": "...",
//!   "version": 1
//! }
//! ```
//!
//! The base64-decoded blob contains:
//! ```json
//! {
//!   "sequence": N,
//!   "expiration": N,
//!   "validators": [
//!     { "validation_public_key": "<hex>", "manifest": "..." }
//!   ]
//! }
//! ```

use std::time::Duration;

/// Default UNL sources (tried in order) — rippled's own `validator_list_sites`.
/// The XRPL Foundation's list is served at `unl.xrplf.org`: its certificate
/// (renewed 2026-09-09) names `unl.xrplf.org` and `unl.xrpl.foundation` only,
/// so the older `vl.xrplf.org` alias fails TLS on the hostname. Every list is
/// verified on its own publisher signature (`unl_verify`), not on TLS.
pub const DEFAULT_UNL_SOURCES: &[&str] = &[
    "https://vl.ripple.com/",
    "https://unl.xrplf.org/",
];

/// How long the consensus monitor waits before retrying a failed UNL fetch:
/// one minute, doubling, capped at fifteen. A publisher outage — or a
/// certificate fault on its side, as on 2026-09-23 when vl.ripple.com served
/// its leaf without the intermediate — must delay consensus tracking, not
/// disable it for the life of the process.
pub fn unl_retry_delay(attempt: u32) -> Duration {
    Duration::from_secs((60u64 << attempt.min(4)).min(900))
}

/// A trusted validator's public key + current ephemeral signing key.
#[derive(Debug, Clone)]
pub struct UnlEntry {
    /// Hex-encoded master public key (33 bytes: 0xED + 32 for Ed25519).
    pub public_key: String,
    /// Hex-encoded ephemeral signing key (33 bytes, Secp256k1 0x02/0x03 prefix),
    /// extracted from the validator's manifest. Used to verify proposal signatures.
    pub signing_key: Option<String>,
}

/// Parse a base64-encoded manifest blob to extract the signing public key.
/// Manifest format (STObject): Sequence(0x24), PublicKey(0x71), SigningPubKey(0x73),
/// Domain(0x77), Signature(0x76), MasterSignature(0x7012).
pub fn parse_manifest_signing_key(manifest_b64: &str) -> Option<String> {
    use base64::Engine;
    // Manifests sometimes use URL-safe base64 without padding; try both.
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(manifest_b64))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(manifest_b64))
        .ok()?;
    // Scan for field 0x73 (SigningPubKey: type=7 Blob, field=3)
    let mut i = 0;
    while i < bytes.len() {
        let tag = bytes[i];
        // Field 0x73 = (7<<4)|3 = SigningPubKey
        if tag == 0x73 {
            i += 1;
            if i >= bytes.len() { return None; }
            // Read VL length
            let (len, vl_hdr) = read_vl(&bytes, i)?;
            i += vl_hdr;
            if i + len > bytes.len() { return None; }
            if len != 33 { return None; } // Must be 33 bytes
            return Some(hex::encode_upper(&bytes[i..i + len]));
        }
        // Skip the field's value
        let type_code = tag >> 4;
        let value_len = match type_code {
            1 => 2,   // UInt16
            2 => 4,   // UInt32
            3 => 8,   // UInt64
            5 => 32,  // UInt256
            7 | 8 => {
                // VL blob
                i += 1;
                let (len, vl_hdr) = read_vl(&bytes, i)?;
                i += vl_hdr;
                len
            }
            _ => return None, // Unknown/unsupported — bail
        };
        if type_code != 7 && type_code != 8 {
            i += 1;
        }
        i += value_len;
    }
    None
}

fn read_vl(data: &[u8], pos: usize) -> Option<(usize, usize)> {
    if pos >= data.len() { return None; }
    let b1 = data[pos] as usize;
    if b1 <= 192 {
        Some((b1, 1))
    } else if b1 <= 240 {
        if pos + 1 >= data.len() { return None; }
        Some((193 + (b1 - 193) * 256 + data[pos + 1] as usize, 2))
    } else if b1 <= 254 {
        if pos + 2 >= data.len() { return None; }
        Some((
            12481 + (b1 - 241) * 65536 + (data[pos + 1] as usize) * 256 + data[pos + 2] as usize,
            3,
        ))
    } else {
        None
    }
}

/// Fetch a validator-list document (the outer JSON) from a URL.
async fn fetch_vl_body(url: &str) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("client build: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request {url}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }

    resp.json().await.map_err(|e| format!("parse JSON: {e}"))
}

/// Fetch and parse a UNL from a URL — UNVERIFIED (the original path; the
/// signatures are not checked). Prefer [`fetch_default_unl_verified`].
pub async fn fetch_unl(url: &str) -> Result<Vec<UnlEntry>, String> {
    let body = fetch_vl_body(url).await?;

    // Extract blob field (base64)
    let blob_b64 = body
        .get("blob")
        .and_then(|v| v.as_str())
        .ok_or("no 'blob' field")?;

    // Decode base64
    use base64::Engine;
    let blob_bytes = base64::engine::general_purpose::STANDARD
        .decode(blob_b64)
        .map_err(|e| format!("base64 decode: {e}"))?;

    // Parse blob JSON
    let blob: serde_json::Value = serde_json::from_slice(&blob_bytes)
        .map_err(|e| format!("blob JSON: {e}"))?;

    let validators = blob
        .get("validators")
        .and_then(|v| v.as_array())
        .ok_or("no 'validators' array in blob")?;

    let mut entries = Vec::with_capacity(validators.len());
    for v in validators {
        if let Some(key) = v.get("validation_public_key").and_then(|k| k.as_str()) {
            let signing_key = v
                .get("manifest")
                .and_then(|m| m.as_str())
                .and_then(parse_manifest_signing_key);
            entries.push(UnlEntry {
                public_key: key.to_uppercase(),
                signing_key,
            });
        }
    }

    Ok(entries)
}

/// Last-good state for the sequence gate: `{publisher, sequence}`.
fn read_last_good(path: &str) -> Option<(String, u64)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some((v.get("publisher")?.as_str()?.to_string(), v.get("sequence")?.as_u64()?))
}

fn write_last_good(path: &str, publisher: &str, sequence: u64) {
    let v = serde_json::json!({
        "publisher": publisher,
        "sequence": sequence,
        "written_unix": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    if let Err(e) = std::fs::write(path, v.to_string()) {
        eprintln!("[unl] last-good persist failed ({path}): {e}");
    }
}

/// va-06: fetch, VERIFY, and gate a UNL from the default sources.
///
/// Env:
///   XRPL_UNL_PINNED_KEY — publisher master key pin (hex; recommended)
///   XRPL_UNL_ENFORCE=1  — fail-closed: no unverified fallback
///   XRPL_UNL_STATE      — last-good file (default /mnt/xrpl-data/unl-last-good.json)
///
/// Every source is tried verified first (publisher pin, manifest chain, blob
/// signature, expiry, per-validator manifests, sequence monotonicity vs the
/// persisted last-good). Observe mode (default) falls back to the unverified
/// parse only after EVERY source fails verification, loudly; enforce mode
/// fails closed instead — the caller keeps whatever UNL it already has.
pub async fn fetch_default_unl_verified() -> Result<Vec<UnlEntry>, String> {
    let pinned = std::env::var("XRPL_UNL_PINNED_KEY").ok();
    let enforce = std::env::var("XRPL_UNL_ENFORCE").map(|v| v == "1").unwrap_or(false);
    let state_path = std::env::var("XRPL_UNL_STATE")
        .unwrap_or_else(|_| "/mnt/xrpl-data/unl-last-good.json".to_string());
    let mut last_err = String::new();
    for url in DEFAULT_UNL_SOURCES {
        let body = match fetch_vl_body(url).await {
            Ok(b) => b,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        match crate::unl_verify::verify_vl(&body, pinned.as_deref()) {
            Ok(v) => {
                let last = read_last_good(&state_path)
                    .filter(|(p, _)| *p == v.publisher)
                    .map(|(_, s)| s);
                if !crate::unl_verify::sequence_acceptable(last, v.sequence) {
                    last_err = format!(
                        "{url}: sequence regression {} < last-good {}",
                        v.sequence,
                        last.unwrap_or(0)
                    );
                    eprintln!("[unl] VERIFY-REJECT {last_err}");
                    continue;
                }
                write_last_good(&state_path, &v.publisher, v.sequence);
                write_last_good_body(&state_path, &body);
                eprintln!(
                    "[unl] VERIFIED {url}: publisher {}… seq {} validators {} (manifests rejected: {})",
                    &v.publisher[..12.min(v.publisher.len())],
                    v.sequence,
                    v.entries.len(),
                    v.manifests_rejected
                );
                return Ok(v.entries);
            }
            Err(e) => {
                last_err = format!("{url}: {e}");
                eprintln!("[unl] VERIFY-FAIL {last_err}");
            }
        }
    }
    if enforce {
        // Fail closed: never an unverified list. The last list that verified, verified again (pin, signatures,
        // expiry, sequence), is the one fallback.
        return match last_good_list(&state_path, pinned.as_deref()) {
            Ok(entries) => {
                eprintln!("[unl] ⚠ every source failed verification ({last_err}); enforce mode keeps the LAST GOOD list ({} validators, verified again)", entries.len());
                Ok(entries)
            }
            Err(e) => Err(format!("all UNL sources failed verification (fail-closed): {last_err}; last good list: {e}")),
        };
    }
    eprintln!(
        "[unl] ⚠ every source failed verification — observe mode falls back to the UNVERIFIED parse ({last_err})"
    );
    fetch_default_unl().await
}

/// Where the last verified list document is kept: beside the last-good state file.
fn last_good_body_path(state_path: &str) -> String {
    format!("{}.body.json", state_path.trim_end_matches(".json"))
}

fn write_last_good_body(state_path: &str, body: &serde_json::Value) {
    let path = last_good_body_path(state_path);
    if let Err(e) = std::fs::write(&path, body.to_string()) {
        eprintln!("[unl] last-good list persist failed ({path}): {e}");
    }
}

/// Step 1.3: the last list that verified, read back and verified again, as enforce mode's only fallback. It
/// must still match the pin, verify, be unexpired, and be no older than the last-good sequence.
pub fn last_good_list(state_path: &str, pinned: Option<&str>) -> Result<Vec<UnlEntry>, String> {
    let path = last_good_body_path(state_path);
    let body: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?)
        .map_err(|e| format!("{path}: {e}"))?;
    let v = crate::unl_verify::verify_vl(&body, pinned)?;
    let last = read_last_good(state_path).filter(|(p, _)| *p == v.publisher).map(|(_, s)| s);
    if !crate::unl_verify::sequence_acceptable(last, v.sequence) {
        return Err(format!("stored list sequence {} is older than last-good {}", v.sequence, last.unwrap_or(0)));
    }
    Ok(v.entries)
}

/// Try multiple UNL sources, return the first successful one.
pub async fn fetch_default_unl() -> Result<Vec<UnlEntry>, String> {
    let mut last_err = String::new();
    for url in DEFAULT_UNL_SOURCES {
        match fetch_unl(url).await {
            Ok(entries) if !entries.is_empty() => {
                eprintln!("[unl] Fetched {} validators from {url}", entries.len());
                return Ok(entries);
            }
            Ok(_) => {
                last_err = format!("{url}: empty list");
            }
            Err(e) => {
                eprintln!("[unl] Failed {url}: {e}");
                last_err = e;
            }
        }
    }
    Err(format!("all UNL sources failed: {last_err}"))
}

#[cfg(test)]
mod last_good_tests {
    use super::*;

    const VL: &str = include_str!("../tests/vectors/vl_ripple_2026-08-31.json");

    fn seeded(sequence: u64) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("unl-last-good.json").to_string_lossy().into_owned();
        let body: serde_json::Value = serde_json::from_str(VL).unwrap();
        write_last_good(&state, body["public_key"].as_str().unwrap(), sequence);
        write_last_good_body(&state, &body);
        (dir, state)
    }

    #[test]
    fn enforce_mode_falls_back_to_the_last_good_list_verified_again() {
        let (_d, state) = seeded(85);
        assert_eq!(last_good_list(&state, None).expect("stored list verifies").len(), 35);
        let ripple = "ED2677ABFFD1B33AC6FBC3062B71F1E8397C1505E1C42C64D11AD1B28FF73F4734";
        assert_eq!(last_good_list(&state, Some(ripple)).unwrap().len(), 35);
        assert!(last_good_list(&state, Some("ED42AEC58B701EEBB77356FFFEC26F83C1F0407263530F068C7C73D392C7E06FD1")).is_err());
    }

    #[test]
    fn a_tampered_or_older_stored_list_is_refused() {
        let (_d, state) = seeded(86);
        assert!(last_good_list(&state, None).unwrap_err().contains("older"), "a newer list was seen since");
        let (_d2, state) = seeded(85);
        let path = last_good_body_path(&state);
        let tampered = std::fs::read_to_string(&path).unwrap().replacen("\"blob\":\"", "\"blob\":\"AA", 1);
        std::fs::write(&path, tampered).unwrap();
        assert!(last_good_list(&state, None).is_err());
        let (_d3, state) = seeded(85);
        std::fs::remove_file(last_good_body_path(&state)).unwrap();
        assert!(last_good_list(&state, None).is_err(), "no stored list: nothing to fall back to");
    }
}

#[cfg(test)]
mod retry_tests {
    #[test]
    fn the_foundation_list_is_fetched_from_its_canonical_host() {
        assert!(super::DEFAULT_UNL_SOURCES.contains(&"https://unl.xrplf.org/"));
        assert!(!super::DEFAULT_UNL_SOURCES.contains(&"https://vl.xrplf.org/"));
    }

    #[test]
    fn a_failed_unl_fetch_is_retried_with_capped_backoff() {
        let secs: Vec<u64> = (0..7).map(|a| super::unl_retry_delay(a).as_secs()).collect();
        assert_eq!(secs, vec![60, 120, 240, 480, 900, 900, 900]);
    }
}
