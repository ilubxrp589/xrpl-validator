//! VALAUDIT lockstep M1 — shadow harness unit tests.
use xrpl_node::lockstep::{evaluate_ledger, LockstepOutcome};
use xrpl_node::state_hash::compute_ledger_hash;

const SEQ: u32 = 104_600_000;
const DROPS: u64 = 99_987_000_000;
const PARENT: [u8; 32] = [0x11; 32];
const TXH: [u8; 32] = [0x22; 32];
const ACCT: [u8; 32] = [0x33; 32];
const PCT: u32 = 770_000_000;
const CT: u32 = 770_000_003;
const RES: u8 = 10;
const FLAGS: u8 = 0;

#[test]
fn matches_when_our_hash_equals_network() {
    let net = compute_ledger_hash(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS).0;
    let out = evaluate_ledger(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS, &net);
    assert!(out.matched, "correct inputs must match");
    assert_eq!(out.our_ledger_hash, net, "our hash must equal the network hash on a match");
}

#[test]
fn mismatch_when_a_header_field_is_wrong() {
    let net = compute_ledger_hash(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT, RES, FLAGS).0;
    let out = evaluate_ledger(SEQ, DROPS, &PARENT, &TXH, &ACCT, PCT, CT + 1, RES, FLAGS, &net);
    assert!(!out.matched, "a wrong close_time must not match");
}
