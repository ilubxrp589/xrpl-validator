//! Integration test: apply a real mainnet Payment tx via Rust → FFI → libxrpl.
//!
//! Mirrors ffi/test_shim.c but in Rust. Proves the Rust bindings produce the
//! same result as the C integration test.

use xrpl_ffi::{apply_with_mutations, libxrpl_version, parse_tx, preflight, LedgerInfo, MemoryProvider, MutationKind};

/// Real mainnet Payment tx (referral reward, ledger 103354511).
/// Hash: 39077702C3FCE0DDC5693065FC0DA35576E4D0112FDEA08D6CAD099074033ABA
const MAINNET_PAYMENT: &str = concat!(
    "12000022800000002404E59B6E61400000000000000A68400000000000000F",
    "732103ECD7DE6564273713EE4EA28A1F4522B3B480F66DC903EB4E5309D32F",
    "633A6DAC74463044022074B318BE47C213C5B9A57341A454033D3CDF97FBB6",
    "998FDA654B4F879A9C1C6502204F520B978C98C8F857B8111FD5E00E94EE16",
    "A2C503EF522FDFA7B0131201051A8114E5A5902FEBDA49C3BDE5B7E4522C62",
    "F3D49E4666831492BD9E89265D9F5853BF1AAB400766CDDBDAEC3CF9EA7D1D",
    "526566657272616C207265776172642066726F6D204D61676E65746963E1F1"
);

/// Sender's AccountRoot at ledger 103354510 (sequence manually bumped from 6D to 6E
/// because an earlier tx in ledger 103354511 advanced it).
const SENDER_KEYLET: &str = "CED60F22A245F8DE393F2351C5097A81836153584DC3C24B803FA1B9906A506A";
const SENDER_SLE: &str = concat!(
    "11006122000000002404E59B6E25062910882D0000000055B8E0C782ADB99C",
    "79445A6C72A3553C6FFD6E7BC529A906DF6FEEE09F439EADFA62400000000776FE378",
    "114E5A5902FEBDA49C3BDE5B7E4522C62F3D49E4666"
);

const DEST_KEYLET: &str = "5180078F1F6E062E4F01B17D6D05E734DF5976F0EDB187ED9EBF652A6F47D28D";
const DEST_SLE: &str = concat!(
    "1100612200000000240432660C2506290E532D00000012553059070AA6AB0E",
    "4DEAC9CEE66914270E0FAD957168D465B657C359132498A644",
    "62400000000C234F9F8",
    "11492BD9E89265D9F5853BF1AAB400766CDDBDAEC3C"
);

fn hex_to_bytes(s: &str) -> Vec<u8> {
    hex::decode(s).expect("hex decode")
}

fn hex_to_array32(s: &str) -> [u8; 32] {
    let v = hex_to_bytes(s);
    assert_eq!(v.len(), 32);
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    a
}

#[test]
fn libxrpl_version_readable() {
    let v = libxrpl_version();
    println!("libxrpl version: {v}");
    assert!(v.contains('.'));
}

#[test]
fn parse_mainnet_payment() {
    let tx = hex_to_bytes(MAINNET_PAYMENT);
    let parsed = parse_tx(&tx).expect("parse");
    assert_eq!(parsed.tx_type, "Payment");
    let hash_hex = hex::encode_upper(parsed.hash);
    assert_eq!(
        hash_hex,
        "39077702C3FCE0DDC5693065FC0DA35576E4D0112FDEA08D6CAD099074033ABA"
    );
}

#[test]
fn preflight_mainnet_payment() {
    let tx = hex_to_bytes(MAINNET_PAYMENT);
    let outcome = preflight(&tx, &[], 0, 0);
    println!("preflight: TER={} ({})", outcome.ter, outcome.ter_name);
    assert!(outcome.is_success(), "preflight should return tesSUCCESS");
    assert_eq!(outcome.ter_name, "tesSUCCESS");
}

#[test]
fn apply_mainnet_payment_returns_mutations() {
    let tx = hex_to_bytes(MAINNET_PAYMENT);
    let sender_key = hex_to_array32(SENDER_KEYLET);
    let dest_key = hex_to_array32(DEST_KEYLET);
    let sender_sle = hex_to_bytes(SENDER_SLE);
    let dest_sle = hex_to_bytes(DEST_SLE);

    let mut provider = MemoryProvider::new();
    provider.insert(sender_key, sender_sle);
    provider.insert(dest_key, dest_sle);

    let ledger = LedgerInfo {
        seq: 103354511,
        parent_close_time: 797193960,
        total_drops: 99985687626634189,
        parent_hash: [0u8; 32],
        base_fee_drops: 10,
        reserve_drops: 10_000_000,
        increment_drops: 2_000_000,
    };

    let outcome = apply_with_mutations(&tx, &[], &ledger, &provider, 0, 0)
        .expect("apply returned null");

    println!(
        "apply: TER={} ({}) applied={} drops_destroyed={}",
        outcome.ter, outcome.ter_name, outcome.applied, outcome.drops_destroyed
    );
    println!("mutations: {}", outcome.mutations.len());
    for m in &outcome.mutations {
        println!(
            "  {:?}  key={}  {} bytes",
            m.kind,
            &hex::encode_upper(m.key)[..16],
            m.data.len()
        );
    }

    assert!(outcome.is_success(), "apply should return tesSUCCESS");
    assert!(outcome.applied, "tx should be applied");
    assert_eq!(outcome.mutations.len(), 2, "should have 2 SLE mutations (sender + dest)");

    // Both should be Modified (both accounts pre-existed)
    for m in &outcome.mutations {
        assert_eq!(m.kind, MutationKind::Modified);
        assert_eq!(m.data.len(), 87); // matching input SLE size
    }

    // Verify balances changed correctly. Sender loses amount+fee=25 drops, dest gains amount=10.
    // Balances are in the last 9 bytes preceding the Account field. Scan for 0x62 marker.
    fn scan_balance(data: &[u8]) -> u64 {
        for i in 7..data.len().saturating_sub(9) {
            if data[i] == 0x62 {
                let raw = u64::from_be_bytes(data[i + 1..i + 9].try_into().unwrap());
                if raw & 0x8000000000000000 != 0 { continue; } // IOU
                if raw & 0x4000000000000000 == 0 { continue; } // not positive
                let drops = raw & 0x3FFFFFFFFFFFFFFF;
                if drops <= 100_000_000_000_000_000 {
                    return drops;
                }
            }
        }
        0
    }

    for m in &outcome.mutations {
        let bal = scan_balance(&m.data);
        if m.key == sender_key {
            println!("sender new balance: {bal}");
            assert_eq!(bal, 125_238_814, "sender loses amount(10) + fee(15) = 25");
        } else if m.key == dest_key {
            println!("dest new balance: {bal}");
            assert_eq!(bal, 203_640_745, "dest gains amount (10)");
        }
    }
}
