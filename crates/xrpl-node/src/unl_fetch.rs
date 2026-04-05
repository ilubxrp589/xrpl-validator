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

/// Default UNL sources (tried in order).
pub const DEFAULT_UNL_SOURCES: &[&str] = &[
    "https://vl.ripple.com/",
    "https://vl.xrplf.org/",
];

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

/// Fetch and parse a UNL from a URL.
pub async fn fetch_unl(url: &str) -> Result<Vec<UnlEntry>, String> {
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

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("parse JSON: {e}"))?;

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
