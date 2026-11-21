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

/// Real vault-era ledger objects through the path the engine and the node use
/// for object bytes: `xrpl_core::codec::decode::decode_transaction_binary` to
/// engine JSON, [`canon_for_encode`], `encode_transaction_json` back to the
/// bytes the state tree hashes. The objects are XRPL Devnet's (ledger
/// 7131244; see `keylet::vault_devnet_fixture`). An object "survives" when
///
///  1. decoding its bytes keeps every field rippled's own JSON shows, with
///     rippled's value for every STNumber and STInt32 field;
///  2. the decoded JSON encodes back to the same bytes;
///  3. rippled's JSON encodes to the same bytes.
///
/// The codec is the SDK's (`xrpl-core`, a path dependency), so what fails
/// here fails for every Vault the node would store or hash.
#[cfg(test)]
mod vault_devnet_round_trip_tests {
    use super::canon_for_encode;
    use crate::ledger::keylet::vault_devnet_fixture::object;
    use serde_json::{json, Value};
    use xrpl_core::codec::decode::decode_transaction_binary;
    use xrpl_core::codec::encode::encode_transaction_json;

    /// Keys rippled's `ledger_entry` adds that are not fields of the object.
    const NOT_FIELDS: [&str; 2] = ["index", "mpt_issuance_id"];
    /// The STNumber (type 9) and STInt32 (type 10) fields of the three
    /// objects (ledger_entries.macro:492-626).
    const NUMBER_FIELDS: [&str; 15] = [
        "AssetsTotal", "AssetsAvailable", "AssetsMaximum", "LossUnrealized", "DebtTotal", "DebtMaximum",
        "CoverAvailable", "LoanOriginationFee", "LoanServiceFee", "LatePaymentFee", "ClosePaymentFee",
        "PeriodicPayment", "PrincipalOutstanding", "TotalValueOutstanding", "ManagementFeeOutstanding",
    ];

    fn survives(name: &str) {
        let (mut theirs, blob) = object(name);
        for k in NOT_FIELDS {
            theirs.as_object_mut().unwrap().remove(k);
        }
        let decoded = decode_transaction_binary(&blob).unwrap_or_else(|e| panic!("{name}: decode: {e:?}"));
        for (field, value) in theirs.as_object().unwrap() {
            assert!(decoded.get(field).is_some(), "{name}: decoding dropped {field}; decoded {decoded}");
            if NUMBER_FIELDS.contains(&field.as_str()) || field == "LoanScale" {
                assert_eq!(&decoded[field], value, "{name}: {field}");
            }
        }
        assert_eq!(decoded.as_object().unwrap().len(), theirs.as_object().unwrap().len(), "{name}: decoded {decoded}");

        let mut ours = decoded.clone();
        canon_for_encode(&mut ours);
        let again = encode_transaction_json(&ours, false).unwrap_or_else(|e| panic!("{name}: encode decoded: {e:?}"));
        assert_eq!(hex::encode_upper(&again), hex::encode_upper(&blob), "{name}: decoded JSON re-encodes differently");

        canon_for_encode(&mut theirs);
        let from_json = encode_transaction_json(&theirs, false).unwrap_or_else(|e| panic!("{name}: encode rippled JSON: {e:?}"));
        assert_eq!(hex::encode_upper(&from_json), hex::encode_upper(&blob), "{name}: rippled's JSON encodes differently");
    }

    #[test]
    fn an_xrp_vault_survives() {
        survives("vault_xrp");
    }

    #[test]
    fn a_closed_ended_iou_vault_survives() {
        survives("vault_usd_closed_ended");
    }

    #[test]
    fn a_loan_broker_survives() {
        survives("loan_broker_xrp");
        survives("loan_broker_usd");
    }

    #[test]
    fn a_loan_survives() {
        survives("loan_usd");
    }

    #[test]
    fn a_share_issuance_survives() {
        survives("share_issuance_xrp");
    }

    #[test]
    fn a_share_issuance_with_a_reference_holding_survives() {
        survives("share_issuance_usd");
    }

    #[test]
    fn a_pseudo_account_root_survives() {
        survives("vault_pseudo_xrp");
        survives("vault_pseudo_usd");
        survives("loan_broker_pseudo_usd");
    }

    /// Inventory Q5 — STNumber on the wire (STNumber.cpp:68-107,
    /// Number.h:691-726): a signed 64-bit mantissa and a signed 32-bit
    /// exponent, big-endian, the mantissa normalised to nineteen digits and,
    /// above 2^63 − 1, written as a tenth of itself with the exponent one
    /// higher; zero is mantissa 0 with exponent i32::MIN. In JSON it is the
    /// string `to_string(Number)` gives (Number.cpp:1413-1505). Each value
    /// must go JSON → bytes → JSON unchanged.
    #[test]
    fn the_codec_round_trips_stnumber_byte_exactly() {
        let wire = |m: i64, e: i32| {
            let mut b = vec![0x94u8]; // STI_NUMBER (9), sfAssetsTotal (4)
            b.extend_from_slice(&m.to_be_bytes());
            b.extend_from_slice(&e.to_be_bytes());
            b
        };
        let cases: [(&str, i64, i32); 9] = [
            ("0", 0, i32::MIN),
            ("1", 1_000_000_000_000_000_000, -18),
            ("10000", 1_000_000_000_000_000_000, -14),
            ("-25.5", -2_550_000_000_000_000_000, -17),
            ("10000.15696446053", 1_000_015_696_446_053_000, -15),
            ("25.01593752207985077", 2_501_593_752_207_985_077, -17),
            // 9.5e18 is above 2^63 − 1: a tenth of it, exponent + 1.
            ("9500000000000000000", 950_000_000_000_000_000, 1),
            ("9223372036854775807", 9_223_372_036_854_775_807, 0),
            // Outside [-28, -8] the string is scientific, trailing zeros off.
            ("1e-30", 1_000_000_000_000_000_000, -48),
        ];
        for (text, mantissa, exponent) in cases {
            let bytes = encode_transaction_json(&json!({"AssetsTotal": text}), false).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert_eq!(hex::encode_upper(&bytes), hex::encode_upper(wire(mantissa, exponent)), "{text} encodes");
            let back = decode_transaction_binary(&bytes).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert_eq!(back["AssetsTotal"], Value::String(text.to_string()), "{text} decodes");
        }
    }

    /// Inventory Q5 — STInt32 (STInteger.cpp; `sfLoanScale`, INT32 1): four
    /// bytes, two's complement, big-endian; a JSON number. The bytes are the
    /// devnet Loan's own (`A1 FFFFFFF5` = -11).
    #[test]
    fn the_codec_round_trips_stint32_byte_exactly() {
        for (value, bytes) in [(-11i64, "A1FFFFFFF5"), (0, "A100000000"), (7, "A100000007"), (i32::MIN as i64, "A180000000")] {
            let encoded = encode_transaction_json(&json!({"LoanScale": value}), false).unwrap_or_else(|e| panic!("{value}: {e:?}"));
            assert_eq!(hex::encode_upper(&encoded), bytes, "{value} encodes");
            let back = decode_transaction_binary(&encoded).unwrap_or_else(|e| panic!("{value}: {e:?}"));
            assert_eq!(back["LoanScale"], json!(value), "{value} decodes");
        }
    }
}
