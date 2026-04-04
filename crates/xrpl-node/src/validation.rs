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

    // STI_VECTOR256 (type 19 = 0x13, field 3 = Amendments)
    // Since type >= 16: first byte = (0 << 4) | field, second byte = type
    pub const AMENDMENTS: [u8; 2] = [0x03, 0x13];
}

/// Compute amendment hash from its name string: SHA-512Half(amendment_name).
fn amendment_hash(name: &str) -> Hash256 {
    let mut hasher = Sha512::new();
    hasher.update(name.as_bytes());
    let full = hasher.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&full[..32]);
    Hash256(result)
}

/// Known XRPL amendments we support — vote YES on flag ledgers.
/// This list includes all enabled mainnet amendments as of 2026-03.
pub fn supported_amendments() -> Vec<Hash256> {
    // All currently enabled + voting amendments on XRPL mainnet
    let names = [
        "MultiSign", "TrustSetAuth", "FeeEscalation",
        "PayChan", "CryptoConditions", "TickSize",
        "fix1368", "Escrow", "CryptoConditionsSuite",
        "fix1373", "EnforceInvariants", "FlowCross",
        "SortedDirectories", "fix1201", "fix1512",
        "fix1513", "fix1523", "fix1528",
        "DepositAuth", "Checks", "fix1571",
        "fix1543", "fix1623", "DepositPreauth",
        "fix1515", "fix1578", "MultiSignReserve",
        "fixTakerDryOfferRemoval", "fixMasterKeyAsRegularKey",
        "fixCheckThreading", "fixPayChanRecipientOwnerDir",
        "DeletableAccounts", "fixQualityUpperBound",
        "RequireFullyCanonicalSig", "fix1781",
        "HardenedValidations", "fixAmendmentMajorityCalc",
        "NegativeUNL", "TicketBatch", "FlowSortStrands",
        "fixSTAmountCanonicalize", "fixRmSmallIncreasedQOffers",
        "CheckCashMakesTrustLine", "ExpandedSignerList",
        "NonFungibleTokensV1_1", "fixTrustLinesToSelf",
        "fixRemoveNFTokenAutoTrustLine", "ImmediateOfferKilled",
        "DisallowIncoming", "XRPFees", "fixUniversalNumber",
        "fixNonFungibleTokensV1_2", "fixNFTokenRemint",
        "fixReducedOffersV1", "Clawback",
        "AMM", "XChainBridge",
        "fixDisallowIncomingV1", "DID",
        "fixFillOrKill", "fixNFTokenReserve",
        "fixInnerObjTemplate", "fixAMMOverflowOffer",
        "PriceOracle", "fixEmptyDID",
        "fixXChainRewardRounding", "fixPreviousTxnID",
        "fixAMMv1_1", "NFTokenMintOffer",
        "DeepFreeze", "PermissionedDomains",
        "Credentials", "AMMClawback",
        "fixReducedOffersV2", "fixEnforceNFTokenTrustline",
        "fixInnerObjTemplate2",
    ];
    names.iter().map(|n| amendment_hash(n)).collect()
}

/// Build the serialized validation object (without signature) for signing.
/// If `amendments` is Some, includes the Amendments field (for flag ledger votes).
fn build_validation_for_signing(
    ledger_seq: u32,
    ledger_hash: &[u8; 32],
    signing_time: u32,
    signing_pub_key: &[u8],
    flags: u32,
    amendments: Option<&[Hash256]>,
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
    let spk_len = signing_pub_key.len();
    if spk_len > 192 {
        eprintln!("[validation] signing pub key too long ({spk_len} bytes, max 192)");
        return Vec::new();
    }
    buf.push(spk_len as u8);
    buf.extend_from_slice(signing_pub_key);

    // Type 19 (VECTOR256): Amendments(3) — only on flag ledgers
    if let Some(amends) = amendments {
        if !amends.is_empty() {
            buf.extend_from_slice(&field::AMENDMENTS);
            // VL length: total bytes = num_amendments * 32
            let total_bytes = amends.len() * 32;
            encode_vl_length(&mut buf, total_bytes);
            for h in amends {
                buf.extend_from_slice(&h.0);
            }
        }
    }

    buf
}

/// Encode a VL (variable length) prefix into the buffer.
fn encode_vl_length(buf: &mut Vec<u8>, len: usize) {
    if len <= 192 {
        buf.push(len as u8);
    } else if len <= 12480 {
        let adjusted = len - 193;
        buf.push((adjusted / 256 + 193) as u8);
        buf.push((adjusted % 256) as u8);
    } else {
        let adjusted = len - 12481;
        buf.push(241);
        buf.push((adjusted / 65536) as u8);
        buf.push(((adjusted / 256) % 256) as u8);
    }
}

/// Sign a validation for the given ledger.
/// Returns the full serialized validation (including signature) ready for TMValidation.
/// If `amendments` is Some, includes amendment votes (for flag ledgers).
pub fn sign_validation(
    identity: &NodeIdentity,
    ledger_seq: u32,
    ledger_hash: &[u8; 32],
    amendments: Option<&[Hash256]>,
) -> Vec<u8> {
    // Signing time: seconds since Ripple epoch (2000-01-01)
    // Ripple epoch = 946684800 Unix seconds
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let signing_time: u32 = now_unix
        .checked_sub(946684800)
        .unwrap_or(0)
        .try_into()
        .unwrap_or(u32::MAX);

    // Flags: 0x80000000 = vfFullValidation (we fully validated this ledger)
    let flags: u32 = 0x80000000;

    let pub_key = identity.public_key();

    // Build the validation object without signature
    let unsigned = build_validation_for_signing(
        ledger_seq, ledger_hash, signing_time, pub_key, flags, amendments,
    );

    // Hash for signing: SHA512Half(VAL\0 || unsigned_validation)
    let prefix: [u8; 4] = [0x56, 0x41, 0x4C, 0x00]; // "VAL\0"
    let mut hasher = Sha512::new();
    hasher.update(&prefix);
    hasher.update(&unsigned);
    let full_hash = hasher.finalize();
    let sign_hash: [u8; 32] = full_hash[..32]
        .try_into()
        .expect("SHA-512 output is always 64 bytes, first 32 always converts");

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

/// Build a validator manifest — maps master key to signing key + declares domain.
/// Uses the same key for both master and signing (sequence=1).
///
/// Manifest format (XRPL serialized object, canonical field order):
/// - Sequence (UINT32, type=2 field=4 → 0x24)
/// - PublicKey / master key (VL, type=7 field=1 → 0x71)
/// - SigningPubKey / ephemeral key (VL, type=7 field=3 → 0x73)
/// - Domain (VL, type=7 field=7 → 0x77) — ASCII domain for verification
/// - Signature from signing key (VL, type=7 field=6 → 0x76)
/// - MasterSignature from master key (VL, type=7 field=18 → 0x70 0x12)
pub fn build_manifest(identity: &NodeIdentity) -> Vec<u8> {
    let master_key = identity.master_public_key(); // Ed25519 (33 bytes, 0xED prefix)
    let signing_key = identity.public_key();        // Secp256k1 (33 bytes, 0x02/0x03 prefix)
    let domain = std::env::var("XRPL_DOMAIN").unwrap_or_else(|_| "halcyon-names.io".to_string());

    // Build unsigned manifest (fields in canonical order by type then field)
    let mut unsigned = Vec::with_capacity(256);

    // Sequence = 1 (UINT32, type=2 field=4 → 0x24)
    unsigned.push(0x24);
    unsigned.extend_from_slice(&1u32.to_be_bytes());

    // PublicKey = Ed25519 master key (VL, type=7 field=1 → 0x71)
    unsigned.push(0x71);
    unsigned.push(master_key.len() as u8);
    unsigned.extend_from_slice(master_key);

    // SigningPubKey = Secp256k1 ephemeral key (VL, type=7 field=3 → 0x73)
    unsigned.push(0x73);
    unsigned.push(signing_key.len() as u8);
    unsigned.extend_from_slice(signing_key);

    // Domain (VL, type=7 field=7 → 0x77) — raw ASCII bytes
    if !domain.is_empty() {
        let domain_bytes = domain.as_bytes();
        unsigned.push(0x77);
        encode_vl_length(&mut unsigned, domain_bytes.len());
        unsigned.extend_from_slice(domain_bytes);
    }

    // Hash for signing: SHA512Half(MAN\0 || unsigned_manifest)
    let prefix: [u8; 4] = [0x4D, 0x41, 0x4E, 0x00]; // "MAN\0"
    let mut hasher = Sha512::new();
    hasher.update(&prefix);
    hasher.update(&unsigned);
    let full_hash = hasher.finalize();
    let sign_hash: [u8; 32] = full_hash[..32]
        .try_into()
        .expect("SHA-512 output is always 64 bytes, first 32 always converts");

    // Signature from SIGNING key (Secp256k1) — proves the ephemeral key authorized this manifest
    let signing_sig = match identity.sign(&sign_hash) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("[manifest] Signing key signature failed: {e}");
            return Vec::new();
        }
    };

    // MasterSignature from MASTER key (Ed25519) — proves the master key authorized this manifest
    let master_sig = match identity.master_sign(&sign_hash) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("[manifest] Master key signature failed: {e}");
            return Vec::new();
        }
    };

    // Build full manifest with both signatures
    let mut manifest = unsigned;

    // Signature (VL, type=7 field=6 → 0x76) — from signing/ephemeral key
    manifest.push(0x76);
    encode_vl_length(&mut manifest, signing_sig.len());
    manifest.extend_from_slice(&signing_sig);

    // MasterSignature (VL, type=7 field=18 → 0x70 0x12) — from master key
    manifest.push(0x70);
    manifest.push(0x12);
    encode_vl_length(&mut manifest, master_sig.len());
    manifest.extend_from_slice(&master_sig);

    eprintln!("[manifest] Master key: {}", hex::encode(master_key));
    eprintln!("[manifest] Signing key: {}", hex::encode(signing_key));
    eprintln!("[manifest] Domain: {domain}");
    eprintln!("[manifest] Built manifest ({} bytes)", manifest.len());
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

        let validation = sign_validation(&identity, 100, &ledger_hash, None);

        // Should be non-empty
        assert!(validation.len() > 50);

        // Should contain the ledger hash
        assert!(validation.windows(32).any(|w| w == &ledger_hash));

        // Should contain the public key
        let pk = identity.public_key();
        assert!(validation.windows(pk.len()).any(|w| w == pk));
    }

    #[test]
    fn sign_validation_with_amendments() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let identity = NodeIdentity::generate().unwrap();
        let ledger_hash = [0xAB; 32];

        let amendments = supported_amendments();
        let validation = sign_validation(&identity, 256, &ledger_hash, Some(&amendments));

        // Should be larger than without amendments
        let plain = sign_validation(&identity, 256, &ledger_hash, None);
        assert!(validation.len() > plain.len());

        // Should contain the Amendments field code [0x03, 0x13]
        assert!(validation.windows(2).any(|w| w == [0x03, 0x13]));
    }

    #[test]
    fn amendment_hash_deterministic() {
        let h1 = amendment_hash("MultiSign");
        let h2 = amendment_hash("MultiSign");
        assert_eq!(h1, h2);

        let h3 = amendment_hash("PayChan");
        assert_ne!(h1, h3);
    }
}
