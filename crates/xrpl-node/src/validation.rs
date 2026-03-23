//! Validation signing — sign and broadcast TMValidation messages.
//!
//! After each ledger close, we sign the agreed-upon ledger hash
//! and broadcast it to peers, announcing "I validate this ledger."

use sha2::{Digest, Sha512};
use xrpl_core::types::Hash256;

use crate::peer::identity::NodeIdentity;

/// XRPL serialization field codes for validation objects.
/// These are type_code << 4 | field_code for common fields.
mod field {
    // STI_UINT32 (type 2)
    pub const LEDGER_SEQUENCE: [u8; 1] = [0x26]; // type=2, field=6
    pub const SIGNING_TIME: [u8; 1] = [0x29];    // type=2, field=9
    pub const FLAGS: [u8; 1] = [0x22];            // type=2, field=2

    // STI_HASH256 (type 5)
    pub const LEDGER_HASH: [u8; 1] = [0x51];           // type=5, field=1
    pub const CONSENSUS_HASH: [u8; 1] = [0x52];         // type=5, field=2

    // STI_VL (variable length, type 7)
    pub const SIGNING_PUB_KEY: [u8; 1] = [0x73]; // type=7, field=3
    pub const SIGNATURE: [u8; 1] = [0x76];        // type=7, field=6
}

/// Build the serialized validation object (without signature) for signing.
fn build_validation_for_signing(
    ledger_seq: u32,
    ledger_hash: &[u8; 32],
    signing_time: u32,
    signing_pub_key: &[u8],
    flags: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    // Fields must be in canonical order (sorted by type_code, then field_code)
    // Type 2 (UINT32): Flags(2), LedgerSequence(6), SigningTime(9)
    buf.extend_from_slice(&field::FLAGS);
    buf.extend_from_slice(&flags.to_be_bytes());

    buf.extend_from_slice(&field::LEDGER_SEQUENCE);
    buf.extend_from_slice(&ledger_seq.to_be_bytes());

    buf.extend_from_slice(&field::SIGNING_TIME);
    buf.extend_from_slice(&signing_time.to_be_bytes());

    // Type 5 (HASH256): LedgerHash(1), ConsensusHash(2)
    buf.extend_from_slice(&field::LEDGER_HASH);
    buf.extend_from_slice(ledger_hash);

    // Type 7 (VL): SigningPubKey(3) — length-prefixed
    buf.extend_from_slice(&field::SIGNING_PUB_KEY);
    buf.push(signing_pub_key.len() as u8);
    buf.extend_from_slice(signing_pub_key);

    buf
}

/// Sign a validation for the given ledger.
/// Returns the full serialized validation (including signature) ready for TMValidation.
pub fn sign_validation(
    identity: &NodeIdentity,
    ledger_seq: u32,
    ledger_hash: &[u8; 32],
) -> Vec<u8> {
    // Signing time: seconds since Ripple epoch (2000-01-01)
    // Ripple epoch = 946684800 Unix seconds
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let signing_time = (now_unix - 946684800) as u32;

    // Flags: 0x80000000 = vfFullValidation (we fully validated this ledger)
    let flags: u32 = 0x80000000;

    let pub_key = identity.public_key();

    // Build the validation object without signature
    let unsigned = build_validation_for_signing(
        ledger_seq, ledger_hash, signing_time, pub_key, flags,
    );

    // Hash for signing: SHA512Half(VAL\0 || unsigned_validation)
    let prefix: [u8; 4] = [0x56, 0x41, 0x4C, 0x00]; // "VAL\0"
    let mut hasher = Sha512::new();
    hasher.update(&prefix);
    hasher.update(&unsigned);
    let full_hash = hasher.finalize();
    let sign_hash: [u8; 32] = full_hash[..32].try_into().unwrap();

    // Sign with our Secp256k1 key
    let signature = match identity.sign(&sign_hash) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("[validation] Signing failed: {e}");
            return Vec::new();
        }
    };

    // Build the full validation with signature appended
    let mut full = unsigned;
    full.extend_from_slice(&field::SIGNATURE);
    // VL length encoding for signature (typically 72 bytes)
    let sig_len = signature.len();
    if sig_len < 192 {
        full.push(sig_len as u8);
    } else {
        // Multi-byte length (shouldn't happen for signatures)
        full.push(((sig_len - 193) / 256 + 193) as u8);
        full.push(((sig_len - 193) % 256) as u8);
    }
    full.extend_from_slice(&signature);

    full
}

/// Build a validator manifest — maps master key to signing key.
/// For simplicity, we use the same key for both (sequence=1).
///
/// Manifest format (XRPL serialized object):
/// - Sequence (UINT32, field 0x24)
/// - PublicKey / master key (VL, field 0x71)
/// - SigningPubKey / ephemeral key (VL, field 0x73)
/// - Signature from master key (VL, field 0x76)
///
/// If master == signing, only one signature is needed.
pub fn build_manifest(identity: &NodeIdentity) -> Vec<u8> {
    let pub_key = identity.public_key();

    // Build unsigned manifest
    let mut unsigned = Vec::with_capacity(128);

    // Sequence = 1 (UINT32, type=2 field=4 → 0x24)
    unsigned.push(0x24);
    unsigned.extend_from_slice(&1u32.to_be_bytes());

    // PublicKey = master key (VL, type=7 field=1 → 0x71)
    unsigned.push(0x71);
    unsigned.push(pub_key.len() as u8);
    unsigned.extend_from_slice(pub_key);

    // SigningPubKey = same key (VL, type=7 field=3 → 0x73)
    unsigned.push(0x73);
    unsigned.push(pub_key.len() as u8);
    unsigned.extend_from_slice(pub_key);

    // Sign with MAN\0 prefix
    let prefix: [u8; 4] = [0x4D, 0x41, 0x4E, 0x00]; // "MAN\0"
    let mut hasher = Sha512::new();
    hasher.update(&prefix);
    hasher.update(&unsigned);
    let full_hash = hasher.finalize();
    let sign_hash: [u8; 32] = full_hash[..32].try_into().unwrap();

    let signature = match identity.sign(&sign_hash) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("[manifest] Signing failed: {e}");
            return Vec::new();
        }
    };

    // Build full manifest with MasterSignature
    let mut manifest = unsigned;

    // MasterSignature (VL, type=7 field=0x12 → needs 2-byte field code)
    // Field 0x12 = 18, type 7: high nibble = 7, low nibble = 0 (field > 15)
    // Encoding: 0x70 0x12
    manifest.push(0x70);
    manifest.push(0x12);
    manifest.push(signature.len() as u8);
    manifest.extend_from_slice(&signature);

    manifest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_validation_produces_bytes() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let identity = NodeIdentity::generate().unwrap();
        let ledger_hash = [0xAB; 32];

        let validation = sign_validation(&identity, 100, &ledger_hash);

        // Should be non-empty
        assert!(validation.len() > 50);

        // Should contain the ledger hash
        assert!(validation.windows(32).any(|w| w == &ledger_hash));

        // Should contain the public key
        let pk = identity.public_key();
        assert!(validation.windows(pk.len()).any(|w| w == pk));
    }
}
