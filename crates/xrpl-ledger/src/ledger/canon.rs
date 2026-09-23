//! The codec's canonical spelling of engine JSON (moved from
//! `xrpl_node::native_apply`, which re-exports it): one table for every path
//! that serializes an object or a transaction — the byte compare, the Batch
//! inner ids the harness pairs with the ledger (`batch_inner_ids`) and the
//! inner ids the engine itself stamps (`tx::batch`, finding 396).

use serde_json::Value;

/// Re-spell engine-internal JSON into the canonical forms the binary codec
/// demands (same table as differential_probe's byte census).
pub fn canon_for_encode(v: &mut Value) {
    const U64_HEX: &[&str] = &[
        "OwnerNode", "BookNode", "LowNode", "HighNode", "DestinationNode",
        "IndexNext", "IndexPrevious", "XChainClaimID", "XChainAccountCreateCount",
        "XChainAccountClaimCount", "ReferenceCount", "NFTokenOfferNode", "IssuerNode",
        "AssetPrice",
        // 2026-08-31 census vs definitions.json: the remaining u64 SFields a
        // ledger entry can carry (hex-string forms pass through unchanged, so
        // these are identity for decoded objects and normalization for any
        // future number-form engine write). Hook/Emit families (Xahau, never
        // in mainnet state) deliberately absent.
        "ExchangeRate", "SubjectNode", "LoanBrokerNode", "VaultNode",
        "BaseFee", "Cookie", "ServerVersion",
    ];
    const U64_DEC: &[&str] = &["MaximumAmount", "OutstandingAmount", "MPTAmount", "LockedAmount"];
    const ACCTS: &[&str] = &[
        "Account", "Owner", "Destination", "Issuer", "RegularKey", "Authorize",
        "Unauthorize", "NFTokenMinter", "Holder", "OtherChainSource",
        "AttestationSignerAccount", "AttestationRewardAccount", "LockingChainDoor",
        "IssuingChainDoor", "issuer",
        // 2026-08-31: every remaining AccountID-typed SField (definitions.json
        // census). "Subject" alone was 386 of 19.8M hydrate-audit failures —
        // every Credential in the mirror was unencodable, its 40-hex Subject
        // fed to the base58 address parser (InvalidBase58/InvalidAddress).
        "Subject", "Delegate", "Counterparty", "Borrower",
        "OtherChainDestination", "EmitCallback", "HookAccount",
    ];
    match v {
        Value::Array(a) => {
            for e in a {
                canon_for_encode(e);
            }
        }
        Value::Object(o) => {
            for (name, val) in o.iter_mut() {
                if U64_HEX.contains(&name.as_str()) {
                    let n = val.as_u64().or_else(|| {
                        val.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())
                    });
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if U64_DEC.contains(&name.as_str()) {
                    let n = val
                        .as_u64()
                        .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()));
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if ACCTS.contains(&name.as_str()) {
                    if let Some(s) = val.as_str() {
                        if s.len() == 40 {
                            if let Ok(b) = hex::decode(s) {
                                if let Ok(arr) = <[u8; 20]>::try_from(b.as_slice()) {
                                    *val = Value::String(
                                        xrpl_core::AccountId::from_bytes(arr).to_address(),
                                    );
                                }
                            }
                        }
                    }
                } else {
                    canon_for_encode(val);
                }
            }
        }
        _ => {}
    }
}
