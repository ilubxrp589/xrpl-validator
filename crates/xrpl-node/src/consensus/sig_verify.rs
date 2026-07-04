//! Peer-message signature verification (S1 proposals, S2 validations) — closes SECURITY 7.3/7.4
//! ("proposals/validations accepted without cryptographic validation").
//!
//! Proposal preimage (rippled `RCLCxPeerPos::hash_append` + `checkSign`):
//!   sha512Half('PRP\0' ‖ be32(proposeSeq) ‖ be32(closeTime) ‖ prevLedger[32] ‖ txSetHash[32])
//! `basic_sha512_half_hasher` declares `endian::big`, so `hash_append` byte-reverses every integral
//! — the 'PRP\0' HashPrefix (0x50525000) travels through the same u32 path as the sequence and
//! close time. The signature is ECDSA over the DIGEST; rippled's `verifyDigest` hard-rejects
//! ed25519 ("secp256k1 required for digest signing"), so 0xED signing keys are counted
//! `Unsupported` and never treated as forgeries.
//!
//! OBSERVE-MODE by default: callers count (and sample-log) failures but still forward the message,
//! so a preimage regression can never silence consensus telemetry. `XRPL_SIG_ENFORCE=1` drops
//! messages that fail verification (rippled's ingest behavior). Flip only after the counters have
//! shown ~0 UNL-peer failures for 24h+.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use xrpl_core::crypto::signing::sha512_half;

/// 'P','R','P',0 — HashPrefix::proposal, big-endian (matches hash_append of u32 0x50525000).
const HASH_PREFIX_PROPOSAL: [u8; 4] = [0x50, 0x52, 0x50, 0x00];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigOutcome {
    /// Signature verified against the embedded key.
    Valid,
    /// Malformed key/hash fields or a signature that fails ECDSA verification.
    Invalid,
    /// A key type the wire protocol cannot digest-sign with (ed25519) — not evidence of forgery.
    Unsupported,
}

pub struct SigCounters {
    pub ok: AtomicU64,
    pub fail: AtomicU64,
    pub unsupported: AtomicU64,
}

impl SigCounters {
    const fn new() -> Self {
        Self {
            ok: AtomicU64::new(0),
            fail: AtomicU64::new(0),
            unsupported: AtomicU64::new(0),
        }
    }

    fn record(&self, outcome: SigOutcome) {
        match outcome {
            SigOutcome::Valid => self.ok.fetch_add(1, Ordering::Relaxed),
            SigOutcome::Invalid => self.fail.fetch_add(1, Ordering::Relaxed),
            SigOutcome::Unsupported => self.unsupported.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.ok.load(Ordering::Relaxed),
            self.fail.load(Ordering::Relaxed),
            self.unsupported.load(Ordering::Relaxed),
        )
    }
}

/// Process-wide proposal-verification counters (exposed on /metrics).
pub static PROPOSAL_SIG: SigCounters = SigCounters::new();
/// Process-wide validation-verification counters (S2 wires these).
pub static VALIDATION_SIG: SigCounters = SigCounters::new();

/// `XRPL_SIG_ENFORCE=1` → callers drop messages that fail verification. Default observe-only.
pub fn enforce() -> bool {
    static ENFORCE: OnceLock<bool> = OnceLock::new();
    *ENFORCE.get_or_init(|| {
        std::env::var("XRPL_SIG_ENFORCE").map(|v| v == "1").unwrap_or(false)
    })
}

/// The exact digest rippled signs a peer proposal over.
pub fn proposal_signing_hash(
    propose_seq: u32,
    close_time: u32,
    prev_ledger: &[u8; 32],
    tx_set_hash: &[u8; 32],
) -> [u8; 32] {
    let mut buf = Vec::with_capacity(4 + 4 + 4 + 32 + 32);
    buf.extend_from_slice(&HASH_PREFIX_PROPOSAL);
    buf.extend_from_slice(&propose_seq.to_be_bytes());
    buf.extend_from_slice(&close_time.to_be_bytes());
    buf.extend_from_slice(prev_ledger);
    buf.extend_from_slice(tx_set_hash);
    sha512_half(&buf)
}

/// Verify a TMProposeSet's signature (fields exactly as they arrive on the wire).
/// Counts into [`PROPOSAL_SIG`]; returns the outcome so the caller can enforce or observe.
pub fn verify_proposal(
    node_pub_key: &[u8],
    signature: &[u8],
    propose_seq: u32,
    close_time: u32,
    prev_ledger: &[u8],
    tx_set_hash: &[u8],
) -> SigOutcome {
    let outcome = verify_proposal_inner(
        node_pub_key,
        signature,
        propose_seq,
        close_time,
        prev_ledger,
        tx_set_hash,
    );
    PROPOSAL_SIG.record(outcome);
    outcome
}

fn verify_proposal_inner(
    node_pub_key: &[u8],
    signature: &[u8],
    propose_seq: u32,
    close_time: u32,
    prev_ledger: &[u8],
    tx_set_hash: &[u8],
) -> SigOutcome {
    if node_pub_key.len() == 33 && node_pub_key[0] == 0xED {
        return SigOutcome::Unsupported; // rippled cannot digest-sign proposals with ed25519
    }
    if !(node_pub_key.len() == 33 && (node_pub_key[0] == 0x02 || node_pub_key[0] == 0x03)) {
        return SigOutcome::Invalid;
    }
    let (Ok(prev), Ok(set)) = (
        <&[u8; 32]>::try_from(prev_ledger),
        <&[u8; 32]>::try_from(tx_set_hash),
    ) else {
        return SigOutcome::Invalid;
    };
    let digest = proposal_signing_hash(propose_seq, close_time, prev, set);
    match xrpl_core::crypto::secp256k1::verify(node_pub_key, &digest, signature) {
        Ok(true) => SigOutcome::Valid,
        _ => SigOutcome::Invalid,
    }
}

// ---------------------------------------------------------------------------
// S2 — validation (STValidation) signature verification
// ---------------------------------------------------------------------------

/// 'V','A','L',0 — HashPrefix::validation.
const HASH_PREFIX_VALIDATION: [u8; 4] = [0x56, 0x41, 0x4C, 0x00];

/// A validation blob split for verification: the embedded key, the signature, and the exact
/// signing preimage ('VAL\0' ‖ blob-with-the-Signature-field-removed, original byte order kept —
/// rippled's `addWithoutSigningFields` omits only sfSignature for validations).
pub struct SplitValidation {
    pub signing_pub_key: Vec<u8>,
    pub signature: Vec<u8>,
    pub signing_preimage: Vec<u8>,
}

/// Walk one canonical field header. Returns (type_code, field_code, bytes_consumed).
fn read_field_header(blob: &[u8], i: usize) -> Option<(u8, u8, usize)> {
    let b0 = *blob.get(i)?;
    let t = b0 >> 4;
    let f = b0 & 0x0F;
    match (t, f) {
        (0, 0) => {
            // both extended: byte1 = type, byte2 = field
            Some((*blob.get(i + 1)?, *blob.get(i + 2)?, 3))
        }
        (0, f) => {
            // extended type: byte1 = type
            Some((*blob.get(i + 1)?, f, 2))
        }
        (t, 0) => {
            // extended field: byte1 = field
            Some((t, *blob.get(i + 1)?, 2))
        }
        (t, f) => Some((t, f, 1)),
    }
}

/// Decode a VL length prefix. Returns (length, prefix_bytes).
fn read_vl_length(blob: &[u8], i: usize) -> Option<(usize, usize)> {
    let b0 = *blob.get(i)? as usize;
    if b0 <= 192 {
        Some((b0, 1))
    } else if b0 <= 240 {
        let b1 = *blob.get(i + 1)? as usize;
        Some((193 + (b0 - 193) * 256 + b1, 2))
    } else if b0 <= 254 {
        let b1 = *blob.get(i + 1)? as usize;
        let b2 = *blob.get(i + 2)? as usize;
        Some((12481 + (b0 - 241) * 65536 + b1 * 256 + b2, 3))
    } else {
        None
    }
}

/// Value length for a field of `type_code` starting at `i` (after the header).
/// Returns (value_len, len_prefix_bytes). None = type we can't skip (nested/unknown).
fn field_value_len(blob: &[u8], i: usize, type_code: u8) -> Option<(usize, usize)> {
    match type_code {
        1 => Some((2, 0)),   // UInt16
        2 => Some((4, 0)),   // UInt32
        3 => Some((8, 0)),   // UInt64
        4 => Some((16, 0)),  // Hash128
        5 => Some((32, 0)),  // Hash256
        6 => {
            // Amount: XRP = 8 bytes; IOU (high bit of first byte set) = 48
            let first = *blob.get(i)?;
            Some((if first & 0x80 != 0 { 48 } else { 8 }, 0))
        }
        7 | 8 | 19 => {
            // Blob / AccountID / Vector256 — VL-prefixed
            let (len, pfx) = read_vl_length(blob, i)?;
            Some((len, pfx))
        }
        16 => Some((1, 0)),  // UInt8
        17 => Some((20, 0)), // Hash160
        _ => None,           // STObject/STArray/etc. — never in a validation; treat as unparseable
    }
}

/// Split an incoming STValidation blob into (key, signature, signing preimage).
/// None = malformed / unparseable blob.
pub fn split_validation_blob(blob: &[u8]) -> Option<SplitValidation> {
    let mut preimage = Vec::with_capacity(4 + blob.len());
    preimage.extend_from_slice(&HASH_PREFIX_VALIDATION);
    let mut signing_pub_key: Option<Vec<u8>> = None;
    let mut signature: Option<Vec<u8>> = None;

    let mut i = 0;
    while i < blob.len() {
        let (t, f, hdr) = read_field_header(blob, i)?;
        let (vlen, pfx) = field_value_len(blob, i + hdr, t)?;
        let end = i + hdr + pfx + vlen;
        if end > blob.len() {
            return None;
        }
        let value = &blob[i + hdr + pfx..end];
        if t == 7 && f == 6 {
            // sfSignature — excluded from the signing preimage
            signature = Some(value.to_vec());
        } else {
            preimage.extend_from_slice(&blob[i..end]);
            if t == 7 && f == 3 {
                signing_pub_key = Some(value.to_vec());
            }
        }
        i = end;
    }

    Some(SplitValidation {
        signing_pub_key: signing_pub_key?,
        signature: signature?,
        signing_preimage: preimage,
    })
}

/// Verify an incoming STValidation blob's signature (rippled: `verify(pubKey, getSignData, sig)`
/// — ed25519 signs the raw prefixed message; secp256k1 signs its sha512Half digest).
/// Counts into [`VALIDATION_SIG`]; returns the outcome so the caller can enforce or observe.
pub fn verify_validation_blob(blob: &[u8]) -> SigOutcome {
    let outcome = verify_validation_inner(blob);
    VALIDATION_SIG.record(outcome);
    outcome
}

fn verify_validation_inner(blob: &[u8]) -> SigOutcome {
    let Some(split) = split_validation_blob(blob) else {
        return SigOutcome::Invalid;
    };
    let key = &split.signing_pub_key;
    if key.len() == 33 && key[0] == 0xED {
        return match xrpl_core::crypto::ed25519::verify(key, &split.signing_preimage, &split.signature)
        {
            Ok(true) => SigOutcome::Valid,
            _ => SigOutcome::Invalid,
        };
    }
    if key.len() == 33 && (key[0] == 0x02 || key[0] == 0x03) {
        let digest = sha512_half(&split.signing_preimage);
        return match xrpl_core::crypto::secp256k1::verify(key, &digest, &split.signature) {
            Ok(true) => SigOutcome::Valid,
            _ => SigOutcome::Invalid,
        };
    }
    SigOutcome::Invalid
}

/// Extra /metrics lines (appended after `render_prometheus` output).
pub fn metrics_text() -> String {
    let (p_ok, p_fail, p_unsup) = PROPOSAL_SIG.snapshot();
    let (v_ok, v_fail, v_unsup) = VALIDATION_SIG.snapshot();
    format!(
        "# HELP xrpl_proposal_sig_ok_total Peer proposals whose signature verified.\n\
         # TYPE xrpl_proposal_sig_ok_total counter\n\
         xrpl_proposal_sig_ok_total {p_ok}\n\
         # HELP xrpl_proposal_sig_fail_total Peer proposals failing signature verification.\n\
         # TYPE xrpl_proposal_sig_fail_total counter\n\
         xrpl_proposal_sig_fail_total {p_fail}\n\
         # HELP xrpl_proposal_sig_unsupported_total Proposals with non-digest-signable key types (ed25519).\n\
         # TYPE xrpl_proposal_sig_unsupported_total counter\n\
         xrpl_proposal_sig_unsupported_total {p_unsup}\n\
         # HELP xrpl_validation_sig_ok_total Peer validations whose signature verified.\n\
         # TYPE xrpl_validation_sig_ok_total counter\n\
         xrpl_validation_sig_ok_total {v_ok}\n\
         # HELP xrpl_validation_sig_fail_total Peer validations failing signature verification.\n\
         # TYPE xrpl_validation_sig_fail_total counter\n\
         xrpl_validation_sig_fail_total {v_fail}\n\
         # HELP xrpl_validation_sig_unsupported_total Validations with unsupported key types.\n\
         # TYPE xrpl_validation_sig_unsupported_total counter\n\
         xrpl_validation_sig_unsupported_total {v_unsup}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use xrpl_core::crypto::secp256k1;

    /// Layout self-check: the preimage is exactly 'PRP\0' ‖ be32 ‖ be32 ‖ 32 ‖ 32.
    #[test]
    fn proposal_preimage_layout() {
        let prev = [0xAA_u8; 32];
        let set = [0xBB_u8; 32];
        let got = proposal_signing_hash(0x0102_0304, 0x0506_0708, &prev, &set);

        let mut expect = Vec::new();
        expect.extend_from_slice(&[0x50, 0x52, 0x50, 0x00]);
        expect.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        expect.extend_from_slice(&[0x05, 0x06, 0x07, 0x08]);
        expect.extend_from_slice(&prev);
        expect.extend_from_slice(&set);
        assert_eq!(expect.len(), 76);
        assert_eq!(got, sha512_half(&expect));
    }

    /// Round-trip: a locally-signed proposal verifies; any tampered field fails.
    #[test]
    fn proposal_sign_verify_roundtrip() {
        let seed = [7_u8; 16];
        let (private_key, public_key) =
            secp256k1::derive_keypair(&seed).expect("test keypair");
        assert_eq!(public_key.len(), 33);

        let prev = [0x11_u8; 32];
        let set = [0x22_u8; 32];
        let digest = proposal_signing_hash(42, 800_000_000, &prev, &set);
        let sig = secp256k1::sign(&private_key, &digest).expect("test sign");

        assert_eq!(
            verify_proposal(&public_key, &sig, 42, 800_000_000, &prev, &set),
            SigOutcome::Valid
        );
        // tampered propose_seq
        assert_eq!(
            verify_proposal(&public_key, &sig, 43, 800_000_000, &prev, &set),
            SigOutcome::Invalid
        );
        // tampered tx-set hash
        let set2 = [0x23_u8; 32];
        assert_eq!(
            verify_proposal(&public_key, &sig, 42, 800_000_000, &prev, &set2),
            SigOutcome::Invalid
        );
        // truncated signature
        assert_eq!(
            verify_proposal(&public_key, &sig[..sig.len() - 4], 42, 800_000_000, &prev, &set),
            SigOutcome::Invalid
        );
    }

    #[test]
    fn ed25519_keys_are_unsupported_not_forged() {
        let mut key = vec![0xED_u8];
        key.extend_from_slice(&[9_u8; 32]);
        assert_eq!(
            verify_proposal(&key, &[0_u8; 70], 1, 2, &[0_u8; 32], &[0_u8; 32]),
            SigOutcome::Unsupported
        );
    }

    #[test]
    fn malformed_fields_are_invalid() {
        let seed = [8_u8; 16];
        let (_, public_key) = secp256k1::derive_keypair(&seed).expect("test keypair");
        // wrong-length prev ledger
        assert_eq!(
            verify_proposal(&public_key, &[0_u8; 70], 1, 2, &[0_u8; 31], &[0_u8; 32]),
            SigOutcome::Invalid
        );
        // garbage key
        assert_eq!(
            verify_proposal(&[0x04_u8; 12], &[0_u8; 70], 1, 2, &[0_u8; 32], &[0_u8; 32]),
            SigOutcome::Invalid
        );
    }

    /// Build a realistic validation blob in canonical order (mirrors validation.rs's writer,
    /// plus a ConsensusHash and a UInt64 Cookie like real mainnet validations carry).
    fn build_validation_blob(pubkey: &[u8], sign: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let mut unsigned = Vec::new();
        unsigned.extend_from_slice(&[0x22]); // Flags (UInt32 f2)
        unsigned.extend_from_slice(&0x8000_0001_u32.to_be_bytes());
        unsigned.extend_from_slice(&[0x26]); // LedgerSequence (UInt32 f6)
        unsigned.extend_from_slice(&105_368_524_u32.to_be_bytes());
        unsigned.extend_from_slice(&[0x29]); // SigningTime (UInt32 f9)
        unsigned.extend_from_slice(&800_000_123_u32.to_be_bytes());
        unsigned.extend_from_slice(&[0x3A]); // Cookie (UInt64 f10)
        unsigned.extend_from_slice(&0xDEAD_BEEF_u64.to_be_bytes());
        unsigned.extend_from_slice(&[0x51]); // LedgerHash (Hash256 f1)
        unsigned.extend_from_slice(&[0x42_u8; 32]);
        unsigned.extend_from_slice(&[0x52]); // ConsensusHash (Hash256 f2)
        unsigned.extend_from_slice(&[0x43_u8; 32]);
        unsigned.extend_from_slice(&[0x73, 33]); // SigningPubKey (VL f3)
        unsigned.extend_from_slice(pubkey);

        let mut preimage = Vec::new();
        preimage.extend_from_slice(&HASH_PREFIX_VALIDATION);
        preimage.extend_from_slice(&unsigned);
        let sig = sign(&preimage);

        let mut blob = unsigned;
        blob.extend_from_slice(&[0x76, sig.len() as u8]); // Signature (VL f6)
        blob.extend_from_slice(&sig);
        blob
    }

    #[test]
    fn validation_secp_roundtrip_and_tamper() {
        let seed = [3_u8; 16];
        let (private_key, public_key) =
            secp256k1::derive_keypair(&seed).expect("test keypair");
        let blob = build_validation_blob(&public_key, |msg| {
            secp256k1::sign(&private_key, &sha512_half(msg)).expect("sign")
        });
        assert_eq!(verify_validation_blob(&blob), SigOutcome::Valid);

        // tamper the LedgerSequence value (bytes 5..9 hold the Flags value; sequence sits after
        // its 0x26 header) — flip one byte inside the blob body before the signature
        let mut bad = blob.clone();
        bad[7] ^= 0x01;
        assert_eq!(verify_validation_blob(&bad), SigOutcome::Invalid);

        // signature field present but truncated → parse still works, verify fails
        let mut short = blob.clone();
        let cut = short.len() - 2;
        short[cut - 1] = short[cut - 1].wrapping_add(1);
        assert_eq!(verify_validation_blob(&short), SigOutcome::Invalid);
    }

    #[test]
    fn validation_ed25519_roundtrip() {
        let seed = [5_u8; 16];
        let (private_key, public_key) =
            xrpl_core::crypto::ed25519::derive_keypair(&seed).expect("test keypair");
        assert_eq!(public_key.len(), 33);
        assert_eq!(public_key[0], 0xED);
        let blob = build_validation_blob(&public_key, |msg| {
            xrpl_core::crypto::ed25519::sign(&private_key, msg).expect("sign")
        });
        assert_eq!(verify_validation_blob(&blob), SigOutcome::Valid);
    }

    #[test]
    fn validation_split_strips_only_signature() {
        let seed = [6_u8; 16];
        let (private_key, public_key) =
            secp256k1::derive_keypair(&seed).expect("test keypair");
        let blob = build_validation_blob(&public_key, |msg| {
            secp256k1::sign(&private_key, &sha512_half(msg)).expect("sign")
        });
        let split = split_validation_blob(&blob).expect("parses");
        assert_eq!(split.signing_pub_key, public_key);
        // preimage = 'VAL\0' + everything except the trailing signature field
        assert_eq!(&split.signing_preimage[..4], &HASH_PREFIX_VALIDATION);
        let sig_field_len = 2 + split.signature.len(); // 0x76 + len byte + sig
        assert_eq!(split.signing_preimage.len(), 4 + blob.len() - sig_field_len);
    }

    #[test]
    fn validation_malformed_blobs_are_invalid() {
        assert_eq!(verify_validation_blob(&[]), SigOutcome::Invalid);
        assert_eq!(verify_validation_blob(&[0x26, 0x00]), SigOutcome::Invalid); // truncated u32
        // no signature field
        assert_eq!(
            verify_validation_blob(&{
                let mut b = vec![0x73, 33];
                b.extend_from_slice(&[0x02; 33]);
                b
            }),
            SigOutcome::Invalid
        );
    }

    #[test]
    fn counters_accumulate() {
        let before = PROPOSAL_SIG.snapshot();
        let mut key = vec![0xED_u8];
        key.extend_from_slice(&[1_u8; 32]);
        verify_proposal(&key, &[], 1, 2, &[0_u8; 32], &[0_u8; 32]);
        let after = PROPOSAL_SIG.snapshot();
        assert_eq!(after.2, before.2 + 1);
        assert!(metrics_text().contains("xrpl_proposal_sig_unsupported_total"));
    }
}
