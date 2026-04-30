//! Regression test for `ffi/CallbackReadView.cpp` ltCHILD wildcard fix.
//!
//! Background: `keylet::child(K)` in rippled returns `Keylet{ltCHILD, K}`.
//! AccountDelete::preclaim and other directory walkers use this form to read
//! owned objects (offers, trustlines, etc.) without committing to a concrete
//! type up front — the caller checks `sle->getType()` after the read.
//!
//! Pre-fix, our shim's `CallbackReadView::read` rejected any SLE whose parsed
//! type didn't equal the keylet's type, with `ltANY` the only documented
//! wildcard. Result: every owner-dir entry returned `nullptr` →
//! `tefBAD_LEDGER` "directory node has index to object that is missing: K".
//!
//! 100% of recovery attempts hit "loop stuck on same missing key" because the
//! injection-and-retry path was rejected by the same type check. See
//! `divergences.jsonl` on m3060 for the production trail (Apr 22 → Apr 29
//! 2026: 64 entries, 90% AccountDelete, all unrecovered).
//!
//! This test exercises the fix in isolation via the `xrpl_test_callback_read`
//! shim entry: hand a real mainnet Offer SLE to the view and verify that
//! reading it via `Keylet{ltCHILD, K}` succeeds.

use xrpl_ffi::{ledger_entry_type as let_, test_callback_read};

/// Real mainnet Offer SLE — key `4C9478B3F8F7FCDA0C634B0E67C6B0DBF29AB3C483800F22794B8C47CA7CFBEC`,
/// fetched at ledger 103880944 (the pre-state for the AccountDelete tx in
/// 103880945 that hit this bug). 183 bytes; first byte `0x11` is sfLedgerEntryType,
/// next two `0x006F` decode to ltOFFER.
const OFFER_SLE_HEX: &str = "11006F22000200002405843BC62505AEBFA633000000000000000034000000000000000055BFD2DFF969B74FD9A0B9B57CEFDD7B0660D20F763E897D7B462B70A47B4853B55010FFA3A80EE625EC309BE19D677D21123DC2104E575EA7CA285A11C37937E08000644000000000E2D26865D4CA8FED80EAD00000000000000000000000000042494C0000000000B44D626D256AD24C3129FC07D5C3A17FFFA219FA8114177F0825BEE180DE30A03CB0C5FF504FB2505A8B";

const OFFER_KEY_HEX: &str =
    "4C9478B3F8F7FCDA0C634B0E67C6B0DBF29AB3C483800F22794B8C47CA7CFBEC";

fn key_array() -> [u8; 32] {
    let v = hex::decode(OFFER_KEY_HEX).expect("hex decode key");
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    a
}

fn sle_bytes() -> Vec<u8> {
    hex::decode(OFFER_SLE_HEX).expect("hex decode sle")
}

#[test]
fn ltchild_keylet_reads_offer_sle() {
    // The bug: keylet::child(K) returned nullptr for a real Offer SLE because
    // the type check treated ltCHILD as a concrete type that mismatched ltOFFER.
    // After the fix, ltCHILD is a wildcard alongside ltANY.
    let key = key_array();
    let sle = sle_bytes();
    assert!(
        test_callback_read(let_::CHILD, &key, &sle),
        "Keylet{{ltCHILD, K}} read of an Offer SLE must succeed; \
         pre-fix this returned nullptr and broke AccountDelete::preclaim"
    );
}

#[test]
fn ltany_keylet_reads_offer_sle() {
    // Sanity check: ltANY has always been the wildcard; this should still pass.
    let key = key_array();
    let sle = sle_bytes();
    assert!(test_callback_read(let_::ANY, &key, &sle));
}

#[test]
fn ltoffer_keylet_reads_offer_sle() {
    // Control: an exact-type read should obviously succeed.
    let key = key_array();
    let sle = sle_bytes();
    assert!(test_callback_read(let_::OFFER, &key, &sle));
}

#[test]
fn ltaccount_root_keylet_rejects_offer_sle() {
    // Negative control: a real type mismatch must still be rejected,
    // otherwise we've over-broadened the wildcard.
    let key = key_array();
    let sle = sle_bytes();
    assert!(
        !test_callback_read(let_::ACCOUNT_ROOT, &key, &sle),
        "Keylet{{ltACCOUNT_ROOT, K}} read of an Offer SLE must return null; \
         the wildcard fix must not disable type checks for non-wildcard types"
    );
}
