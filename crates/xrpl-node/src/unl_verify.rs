//! unl_verify — va-06 (roadmap S3): publisher-verified UNL.
//!
//! `unl_fetch` downloads a validator list and, until now, believed it. This
//! module makes the list prove itself, exactly as rippled's ValidatorList
//! does before trusting a fetched blob:
//!
//!   1. the publisher's MASTER key matches the operator's pin
//!      (`XRPL_UNL_PINNED_KEY`, optional — unpinned runs still verify 2-5),
//!   2. the publisher's manifest binds master → ephemeral signing key, with
//!      BOTH signatures checked over `'MAN\0' ‖ fields-minus-both-signatures`
//!      (rippled `Manifest::verify`: addWithoutSigningFields omits
//!      sfSignature and sfMasterSignature; ed25519 signs the raw prefixed
//!      message, secp256k1 its sha512Half digest — the S1/S2 dispatch),
//!   3. the blob signature verifies under that ephemeral key over the raw
//!      base64-decoded blob bytes,
//!   4. the blob has not expired,
//!   5. each validator entry's own manifest verifies and binds the entry's
//!      master key before its signing key is believed (closes unl.rs
//!      SECURITY(7.5) for fetched lists — an unverifiable manifest keeps the
//!      master key trusted but yields no signing key).
//!
//! Sequence monotonicity and last-good persistence live with the fetcher
//! (`unl_fetch::fetch_default_unl_verified`), which owns the IO.

use serde_json::Value;

use crate::consensus::sig_verify::{field_value_len, read_field_header, read_vl_length};
use crate::unl_fetch::UnlEntry;
use xrpl_core::crypto::signing::sha512_half;

const HASH_PREFIX_MANIFEST: [u8; 4] = [0x4D, 0x41, 0x4E, 0x00]; // 'MAN\0'

/// Seconds between the Unix and Ripple epochs (2000-01-01).
const RIPPLE_EPOCH: u64 = 946_684_800;

/// A fetched list that survived every check.
#[derive(Debug)]
pub struct VerifiedUnl {
    /// Hex master key of the publisher (upper case).
    pub publisher: String,
    pub sequence: u64,
    /// Ripple-epoch seconds.
    pub expiration: u64,
    pub entries: Vec<UnlEntry>,
    /// Validator entries whose own manifest failed verification (their
    /// signing key was withheld; the master key is still listed).
    pub manifests_rejected: u32,
}

/// A manifest STObject split for verification.
pub struct SplitManifest {
    pub master_key: Vec<u8>,
    pub signing_key: Option<Vec<u8>>,
    pub sequence: u32,
    master_sig: Vec<u8>,
    sig: Option<Vec<u8>>,
    /// `'MAN\0' ‖ fields-minus-both-signatures`, original byte order kept.
    preimage: Vec<u8>,
}

/// ed25519 verifies the raw message; secp256k1 verifies its sha512Half
/// digest — rippled's key-type dispatch, shared with S1/S2.
fn verify_key_sig(key: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    if key.len() == 33 && key[0] == 0xED {
        return matches!(xrpl_core::crypto::ed25519::verify(key, msg, sig), Ok(true));
    }
    if key.len() == 33 && (key[0] == 0x02 || key[0] == 0x03) {
        let digest = sha512_half(msg);
        return matches!(xrpl_core::crypto::secp256k1::verify(key, &digest, sig), Ok(true));
    }
    false
}

/// Manifests travel as base64 in several paddings; try the same three
/// engines `unl_fetch::parse_manifest_signing_key` learned to accept.
pub fn decode_b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s))
        .ok()
}

/// Split a manifest STObject: keys, sequence, both signatures, and the
/// signing preimage. None = malformed.
pub fn split_manifest(bytes: &[u8]) -> Option<SplitManifest> {
    let mut preimage = Vec::with_capacity(4 + bytes.len());
    preimage.extend_from_slice(&HASH_PREFIX_MANIFEST);
    let mut master_key: Option<Vec<u8>> = None;
    let mut signing_key: Option<Vec<u8>> = None;
    let mut master_sig: Option<Vec<u8>> = None;
    let mut sig: Option<Vec<u8>> = None;
    let mut sequence: Option<u32> = None;

    let mut i = 0;
    while i < bytes.len() {
        let (t, f, hdr) = read_field_header(bytes, i)?;
        let (vlen, pfx) = field_value_len(bytes, i + hdr, t)?;
        let end = i + hdr + pfx + vlen;
        if end > bytes.len() {
            return None;
        }
        let value = &bytes[i + hdr + pfx..end];
        match (t, f) {
            (7, 6) => sig = Some(value.to_vec()),            // sfSignature
            (7, 18) => master_sig = Some(value.to_vec()),    // sfMasterSignature
            _ => {
                preimage.extend_from_slice(&bytes[i..end]);
                match (t, f) {
                    (7, 1) => master_key = Some(value.to_vec()), // sfPublicKey
                    (7, 3) => signing_key = Some(value.to_vec()), // sfSigningPubKey
                    (2, 4) => sequence = <[u8; 4]>::try_from(value).ok().map(u32::from_be_bytes),
                    _ => {}
                }
            }
        }
        i = end;
    }

    Some(SplitManifest {
        master_key: master_key?,
        signing_key,
        sequence: sequence?,
        master_sig: master_sig?,
        sig,
        preimage,
    })
}

/// Verify a manifest's signature chain. A sequence of `u32::MAX` is a master
/// key REVOCATION: the manifest may verify, but it must never yield a
/// signing key.
pub fn verify_manifest(bytes: &[u8]) -> Result<SplitManifest, String> {
    let m = split_manifest(bytes).ok_or("manifest: malformed STObject")?;
    if !verify_key_sig(&m.master_key, &m.preimage, &m.master_sig) {
        return Err("manifest: master signature invalid".into());
    }
    if m.sequence == u32::MAX {
        return Err("manifest: master key revoked (sequence = 2^32-1)".into());
    }
    let sk = m.signing_key.as_deref().ok_or("manifest: no SigningPubKey")?;
    let esig = m.sig.as_deref().ok_or("manifest: no ephemeral Signature")?;
    if !verify_key_sig(sk, &m.preimage, esig) {
        return Err("manifest: ephemeral signature invalid".into());
    }
    Ok(m)
}

/// Verify one fetched validator-list document end to end (checks 1-5 above).
/// `pinned_master_hex` = the operator's pin; None still verifies everything
/// except the pin itself.
pub fn verify_vl(body: &Value, pinned_master_hex: Option<&str>) -> Result<VerifiedUnl, String> {
    let publisher = body
        .get("public_key")
        .and_then(|v| v.as_str())
        .ok_or("vl: no public_key")?
        .to_uppercase();
    if let Some(pin) = pinned_master_hex {
        if !pin.eq_ignore_ascii_case(&publisher) {
            return Err(format!(
                "vl: publisher {} does not match pinned key {}",
                &publisher[..12.min(publisher.len())],
                &pin[..12.min(pin.len())]
            ));
        }
    }

    let manifest_b64 = body.get("manifest").and_then(|v| v.as_str()).ok_or("vl: no manifest")?;
    let manifest_bytes = decode_b64(manifest_b64).ok_or("vl: manifest base64")?;
    let m = verify_manifest(&manifest_bytes).map_err(|e| format!("vl publisher {e}"))?;
    if hex::encode_upper(&m.master_key) != publisher {
        return Err("vl: manifest master key is not the publisher key".into());
    }

    let blob_b64 = body.get("blob").and_then(|v| v.as_str()).ok_or("vl: no blob")?;
    let blob_bytes = decode_b64(blob_b64).ok_or("vl: blob base64")?;
    let sig_hex = body.get("signature").and_then(|v| v.as_str()).ok_or("vl: no signature")?;
    let sig = hex::decode(sig_hex).map_err(|_| "vl: signature hex")?;
    let signing_key = m.signing_key.as_deref().ok_or("vl: publisher manifest has no signing key")?;
    if !verify_key_sig(signing_key, &blob_bytes, &sig) {
        return Err("vl: blob signature invalid".into());
    }

    let blob: Value = serde_json::from_slice(&blob_bytes).map_err(|e| format!("vl: blob JSON: {e}"))?;
    let sequence = blob.get("sequence").and_then(|v| v.as_u64()).ok_or("vl: blob has no sequence")?;
    let expiration = blob.get("expiration").and_then(|v| v.as_u64()).unwrap_or(0);
    let now_ripple = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(RIPPLE_EPOCH))
        .unwrap_or(0);
    if expiration != 0 && expiration <= now_ripple {
        return Err(format!("vl: list expired (expiration {expiration} <= now {now_ripple})"));
    }

    let validators = blob
        .get("validators")
        .and_then(|v| v.as_array())
        .ok_or("vl: no validators array")?;
    let mut entries = Vec::with_capacity(validators.len());
    let mut manifests_rejected = 0u32;
    for v in validators {
        let Some(master_hex) = v.get("validation_public_key").and_then(|k| k.as_str()) else {
            continue;
        };
        let master_hex = master_hex.to_uppercase();
        let signing = v
            .get("manifest")
            .and_then(|s| s.as_str())
            .and_then(decode_b64)
            .and_then(|b| match verify_manifest(&b) {
                Ok(vm) if hex::encode_upper(&vm.master_key) == master_hex => {
                    vm.signing_key.map(|k| hex::encode_upper(&k))
                }
                _ => {
                    manifests_rejected += 1;
                    None
                }
            });
        entries.push(UnlEntry { public_key: master_hex, signing_key: signing });
    }

    Ok(VerifiedUnl { publisher, sequence, expiration, entries, manifests_rejected })
}

/// Sequence gate for a publisher's successive lists: accept an equal or
/// newer sequence, refuse a regression (replayed old list).
pub fn sequence_acceptable(last_good: Option<u64>, new: u64) -> bool {
    match last_good {
        Some(last) => new >= last,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VL: &str = include_str!("../tests/vectors/vl_ripple_2026-08-31.json");

    fn body() -> Value {
        serde_json::from_str(VL).expect("vector parses")
    }

    /// The captured live vl.ripple.com document is the oracle: rippled-side
    /// signatures over real keys. (Its expiration is 2027; when it passes,
    /// recapture a fresh vector rather than loosening the check.)
    #[test]
    fn live_vector_verifies_end_to_end() {
        let v = verify_vl(&body(), None).expect("live vl verifies");
        assert_eq!(v.sequence, 85);
        assert_eq!(v.entries.len(), 35);
        assert_eq!(v.manifests_rejected, 0, "every published validator manifest verifies");
        assert!(v.entries.iter().all(|e| e.signing_key.is_some()));
        assert!(v.publisher.starts_with("ED2677ABFFD1B33AC6FB"));
    }

    #[test]
    fn pin_mismatch_fails() {
        let err = verify_vl(&body(), Some("ED00000000000000000000000000000000000000000000000000000000000000FF"))
            .unwrap_err();
        assert!(err.contains("pinned"), "{err}");
    }

    #[test]
    fn pin_match_passes() {
        let publisher = body()["public_key"].as_str().unwrap().to_lowercase();
        verify_vl(&body(), Some(&publisher)).expect("case-insensitive pin accepts");
    }

    #[test]
    fn tampered_blob_fails() {
        use base64::Engine;
        let mut b = body();
        let blob_b64 = b["blob"].as_str().unwrap();
        let mut blob = decode_b64(blob_b64).unwrap();
        // Flip one byte inside the signed region.
        let mid = blob.len() / 2;
        blob[mid] ^= 0x01;
        b["blob"] = Value::String(base64::engine::general_purpose::STANDARD.encode(&blob));
        let err = verify_vl(&b, None).unwrap_err();
        assert!(err.contains("blob signature"), "{err}");
    }

    #[test]
    fn tampered_manifest_fails() {
        use base64::Engine;
        let mut b = body();
        let mut m = decode_b64(b["manifest"].as_str().unwrap()).unwrap();
        let mid = m.len() / 2;
        m[mid] ^= 0x01;
        b["manifest"] = Value::String(base64::engine::general_purpose::STANDARD.encode(&m));
        assert!(verify_vl(&b, None).is_err());
    }

    #[test]
    fn publisher_manifest_splits() {
        let m = decode_b64(body()["manifest"].as_str().unwrap()).unwrap();
        let s = split_manifest(&m).expect("splits");
        assert_eq!(s.master_key.len(), 33);
        assert_eq!(s.master_key[0], 0xED);
        assert!(s.signing_key.is_some());
        assert!(s.sequence > 0 && s.sequence != u32::MAX);
    }

    #[test]
    fn sequence_gate() {
        assert!(sequence_acceptable(None, 1));
        assert!(sequence_acceptable(Some(85), 85));
        assert!(sequence_acceptable(Some(85), 86));
        assert!(!sequence_acceptable(Some(85), 84));
    }
}
