//! Transaction validation — decode, verify signature, extract fields.

use xrpl_core::codec::{decode_transaction_binary, encode_transaction_json};
use xrpl_core::crypto::signing::sha512_half;
use xrpl_core::types::Hash256;

/// Result of validating a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Transaction is valid.
    Valid,
    /// Transaction is invalid — reason provided.
    Invalid(String),
}

/// Fields extracted from a decoded transaction.
#[derive(Debug, Clone)]
pub struct TxFields {
    pub tx_hash: Hash256,
    pub fee: u64,
    pub account: String,
    pub sequence: u32,
    pub last_ledger_seq: Option<u32>,
}

/// Validate a raw transaction blob: decode, verify signature, extract fields.
///
/// Returns the validation result and extracted fields (if decodable).
pub fn validate_transaction(tx_blob: &[u8]) -> (ValidationResult, Hash256, Option<TxFields>) {
    // Compute transaction hash: SHA512Half(TXN\0 || blob)
    let tx_hash = transaction_hash(tx_blob);

    // Size checks
    if tx_blob.len() < 20 {
        return (ValidationResult::Invalid("transaction too small".into()), tx_hash, None);
    }
    if tx_blob.len() > 1_000_000 {
        return (ValidationResult::Invalid("transaction too large".into()), tx_hash, None);
    }

    // Decode via xrpl-core binary codec
    let decoded = match decode_transaction_binary(tx_blob) {
        Ok(v) => v,
        Err(e) => {
            return (
                ValidationResult::Invalid(format!("decode failed: {e}")),
                tx_hash,
                None,
            );
        }
    };

    // Extract required fields
    let account = match decoded.get("Account").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return (ValidationResult::Invalid("missing Account".into()), tx_hash, None),
    };

    let fee = match decoded.get("Fee").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()) {
        Some(f) => f,
        None => return (ValidationResult::Invalid("missing or invalid Fee".into()), tx_hash, None),
    };

    let sequence = match decoded.get("Sequence").and_then(|v| v.as_u64()) {
        Some(s) => s as u32,
        None => return (ValidationResult::Invalid("missing Sequence".into()), tx_hash, None),
    };

    let last_ledger_seq = decoded
        .get("LastLedgerSequence")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    // Extract signature fields
    let signing_pub_key = decoded.get("SigningPubKey").and_then(|v| v.as_str());
    let txn_signature = decoded.get("TxnSignature").and_then(|v| v.as_str());

    // Fee sanity: reject absurdly high fees (> 10 XRP = 10,000,000 drops)
    if fee > 10_000_000 {
        return (
            ValidationResult::Invalid(format!("fee too high: {fee} drops")),
            tx_hash,
            None,
        );
    }

    // Verify signature if both fields present
    if let (Some(pubkey_hex), Some(sig_hex)) = (signing_pub_key, txn_signature) {
        let pubkey = match hex::decode(pubkey_hex) {
            Ok(k) if !k.is_empty() => k,
            _ => {
                return (
                    ValidationResult::Invalid("invalid SigningPubKey".into()),
                    tx_hash,
                    None,
                );
            }
        };
        let signature = match hex::decode(sig_hex) {
            Ok(s) => s,
            Err(_) => {
                return (
                    ValidationResult::Invalid("invalid TxnSignature hex".into()),
                    tx_hash,
                    None,
                );
            }
        };

        // Re-encode for signing: remove TxnSignature from JSON first
        // (belt-and-suspenders: encoder also skips non-signing fields)
        let mut signing_json = decoded.clone();
        if let Some(obj) = signing_json.as_object_mut() {
            obj.remove("TxnSignature");
        }
        match encode_transaction_json(&signing_json, true) {
            Ok(signing_bytes) => {
                // Signing hash = SHA512Half(STX\0 || signing_bytes)
                let mut prefixed = Vec::with_capacity(4 + signing_bytes.len());
                prefixed.extend_from_slice(&[0x53, 0x54, 0x58, 0x00]); // STX\0
                prefixed.extend_from_slice(&signing_bytes);
                let signing_hash = sha512_half(&prefixed);

                // Determine key type and verify.
                // Ed25519: verify against raw prefixed message (Ed25519 does internal hashing)
                // Secp256k1: verify against SHA512-half hash (ECDSA signs a hash)
                let valid = if pubkey.len() == 33 && pubkey[0] == 0xED {
                    // Ed25519 — sign/verify the raw message, not the hash
                    xrpl_core::crypto::ed25519::verify(&pubkey, &prefixed, &signature)
                        .unwrap_or(false)
                } else if pubkey.len() == 33 && (pubkey[0] == 0x02 || pubkey[0] == 0x03) {
                    // Secp256k1 — sign/verify the SHA512-half hash
                    xrpl_core::crypto::secp256k1::verify(&pubkey, &signing_hash, &signature)
                        .unwrap_or(false)
                } else {
                    false
                };

                if !valid {
                    tracing::debug!(
                        key_type = if pubkey[0] == 0xED { "ed25519" } else { "secp256k1" },
                        "signature verification failed"
                    );
                    return (
                        ValidationResult::Invalid("signature verification failed".into()),
                        tx_hash,
                        Some(TxFields { tx_hash, fee, account, sequence, last_ledger_seq }),
                    );
                }
            }
            Err(e) => {
                tracing::debug!("encode_for_signing failed: {e}");
                return (
                    ValidationResult::Invalid(format!("encode_for_signing failed: {e}")),
                    tx_hash,
                    Some(TxFields { tx_hash, fee, account, sequence, last_ledger_seq }),
                );
            }
        }
    }

    let fields = TxFields {
        tx_hash,
        fee,
        account,
        sequence,
        last_ledger_seq,
    };

    (ValidationResult::Valid, tx_hash, Some(fields))
}

/// Compute the transaction hash from a raw blob.
pub fn transaction_hash(tx_blob: &[u8]) -> Hash256 {
    let mut hash_input = Vec::with_capacity(4 + tx_blob.len());
    hash_input.extend_from_slice(&[0x54, 0x58, 0x4E, 0x00]); // TXN\0
    hash_input.extend_from_slice(tx_blob);
    let h = sha512_half(&hash_input);
    Hash256(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_small() {
        let (result, _, _) = validate_transaction(&[0u8; 5]);
        assert!(matches!(result, ValidationResult::Invalid(_)));
    }

    #[test]
    fn too_large() {
        let (result, _, _) = validate_transaction(&vec![0u8; 2_000_000]);
        assert!(matches!(result, ValidationResult::Invalid(_)));
    }

    #[test]
    fn hash_deterministic() {
        let blob = vec![1, 2, 3, 4, 5];
        let h1 = transaction_hash(&blob);
        let h2 = transaction_hash(&blob);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_uses_txn_prefix() {
        let blob = vec![1, 2, 3];
        let tx_hash = transaction_hash(&blob);
        let raw_hash = Hash256(sha512_half(&blob));
        assert_ne!(tx_hash, raw_hash);
    }

    #[test]
    fn invalid_blob_fails_decode() {
        // Random bytes won't decode as a valid XRPL transaction
        let blob = vec![0xFF; 100];
        let (result, _, fields) = validate_transaction(&blob);
        // Should fail at decode or missing fields
        assert!(
            matches!(result, ValidationResult::Invalid(_)) || fields.is_none(),
            "random blob should not validate"
        );
    }
}
