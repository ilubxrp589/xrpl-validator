//! VALAUDIT Phase 5 (va-05) — independent ledger_hash signing gate.
//!
//! After the Phase-3 gate opens (3 consecutive account_hash matches), Phase 5
//! goes further: the validator computes the ENTIRE ledger_hash itself from the
//! header fields + its independently-computed account_hash, and refuses to sign
//! unless that hash matches the network-reported value. On a match it signs its
//! OWN computed hash — so the signature is cryptographic proof that this node
//! independently verified THAT specific ledger.
//!
//! These tests exercise the pure decision function `verify_ledger_for_signing`
//! (library-level, no FFI) — the source of truth the `live_viewer.rs` signing
//! block calls. The WS-field parsing + Prometheus wiring are integration
//! concerns covered by the mandatory 7-day mainnet match-ratio watch.

use xrpl_node::state_hash::{compute_ledger_hash, verify_ledger_for_signing, StateHashComputer};

// A representative closed-ledger header.
const SEQ: u32 = 104_595_102;
const DROPS: u64 = 99_987_654_321;
const PARENT: [u8; 32] = [0x11; 32];
const TXH: [u8; 32] = [0x22; 32];
const ACCT: [u8; 32] = [0x33; 32];
const PCT: u32 = 770_000_000; // parent_close_time
const CT: u32 = 770_000_003; // close_time
const RES: u8 = 10; // close_resolution
const FLAGS: u8 = 0; // close_flags

#[test]
fn signs_locally_computed_hash_when_it_matches_network() {
    // The network reports exactly the hash we would independently compute.
    let our = compute_ledger_hash(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS);
    let decision =
        verify_ledger_for_signing(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS, &our.0);
    assert_eq!(
        decision,
        Some(our.0),
        "on match, must return our OWN computed hash to sign (not the network bytes verbatim)"
    );
}

#[test]
fn refuses_to_sign_when_our_hash_diverges_from_network() {
    // Network reports a hash we did not independently reach.
    let bogus_network = [0xFF; 32];
    let decision = verify_ledger_for_signing(
        SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS, &bogus_network,
    );
    assert_eq!(decision, None, "on divergence, must refuse to sign");
}

#[test]
fn a_single_wrong_header_field_flips_match_to_refuse() {
    // Guards the #1 Phase-5 risk: a mis-parsed WS header field. The network hash
    // is computed from the correct close_time; we feed close_time+1 and must
    // refuse — proving the gate is sensitive to every field feeding the hash.
    let network = compute_ledger_hash(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS).0;
    let decision = verify_ledger_for_signing(
        SEQ,
        DROPS,
        &PARENT,
        &TXH,
        &ACCT,
        PCT,
        CT + 1, // wrong close_time
        RES,
        FLAGS,
        &network,
    );
    assert_eq!(
        decision, None,
        "a wrong close_time must break the match and refuse to sign"
    );
}

// --- match-ratio gauge (xrpl_validator_ledger_hash_match_ratio) ---

#[test]
fn match_ratio_is_one_with_no_mismatches() {
    let h = StateHashComputer::new();
    for _ in 0..10 {
        h.record_ledger_hash_result(true);
    }
    assert_eq!(h.ledger_hash_match_ratio(), 1.0);
}

#[test]
fn match_ratio_drops_with_a_mismatch() {
    let h = StateHashComputer::new();
    for _ in 0..3 {
        h.record_ledger_hash_result(true);
    }
    h.record_ledger_hash_result(false); // 3 of 4 matched
    assert!((h.ledger_hash_match_ratio() - 0.75).abs() < 1e-9);
}

#[test]
fn match_ratio_window_rolls_at_1000() {
    let h = StateHashComputer::new();
    // One early mismatch, then 1000 matches — the mismatch rolls out of the window.
    h.record_ledger_hash_result(false);
    for _ in 0..1000 {
        h.record_ledger_hash_result(true);
    }
    assert_eq!(
        h.ledger_hash_match_ratio(),
        1.0,
        "the old mismatch must roll out of the 1000-entry window"
    );
}
