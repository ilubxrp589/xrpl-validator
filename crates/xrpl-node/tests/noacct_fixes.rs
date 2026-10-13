//! The 3.3 NoAccount audit's fixes (~/xrpl-ops/o33/noaccount-audit.md, 10-13), each pinned through the
//! live pipeline (`native_apply_one`) against rippled 3.4.1. Every test here failed at main 493bc07.
//!
//! They come in two groups:
//! - **Can appear in validated ledgers,** where rippled files a tec and we said tes:
//!   - A4: EscrowCancel's time gate;
//!   - A5: PermissionedDomainSet's credential issuer;
//!   - A6: NFTokenMint with an `Amount`.
//! - **Ter against claim, which only bites when we build a ledger ourselves:**
//!   - Clawback's check order;
//!   - A3: AMMBid's AuthAccounts;
//!   - A2: checkPermission for a standalone delegated transaction.
use std::collections::HashMap;

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_node::native_apply::{build_txfields, native_apply_one};

const ALICE: [u8; 20] = [0xA1; 20];
const BOB: [u8; 20] = [0xB0; 20];
/// Never in any ledger here: the missing account.
const CAROL: [u8; 20] = [0xC3; 20];
const DAVE: [u8; 20] = [0xD4; 20];
const GATE: [u8; 20] = [0x6A; 20];
/// The parent ledger's close time: the "now" that time gates read.
const PARENT_CLOSE: u32 = 813_000_000;

fn ledger(accounts: &[[u8; 20]]) -> LedgerState {
    let header = LedgerHeader {
        sequence: 107_600_000,
        total_coins: 99_986_000_000_000_000,
        parent_hash: Hash256([0x10; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: PARENT_CLOSE - 4,
        close_time: PARENT_CLOSE,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);
    for id in accounts {
        let root = json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": "100000000",
            "Sequence": 7,
            "OwnerCount": 0,
            "Flags": 0,
        });
        put(&mut state, keylet::account_root_key(id), &root);
    }
    state
}

fn put(state: &mut LedgerState, key: Hash256, object: &Value) {
    state.state_map.insert(key, serde_json::to_vec(object).unwrap()).unwrap();
}

fn apply(state: &LedgerState, tx: Value) -> (String, HashMap<Hash256, SandboxEntry>) {
    native_apply_one(state, &build_txfields(&tx).expect("txfields"))
}

/// A claimed tec leaves only the fee and the sequence: the sender's root, nothing else.
fn only_the_fee(mods: &HashMap<Hash256, SandboxEntry>, sender: &[u8; 20]) {
    assert_eq!(mods.len(), 1, "{mods:?}");
    assert!(mods.contains_key(&keylet::account_root_key(sender)));
}

// ---- A4: EscrowCancel's time gate (EscrowCancel.cpp:133-141) ----

fn escrow(state: &mut LedgerState, cancel_after: Option<u32>) {
    let mut e = json!({
        "LedgerEntryType": "Escrow",
        "Account": hex::encode(ALICE),
        "Destination": hex::encode(BOB),
        "Amount": "1000000",
        "FinishAfter": PARENT_CLOSE - 1_000,
        "OwnerNode": "0",
        "DestinationNode": "0",
        "Flags": 0,
    });
    if let Some(t) = cancel_after {
        e["CancelAfter"] = json!(t);
    }
    put(state, keylet::escrow_key(&ALICE, 5), &e);
}

fn cancel(by: [u8; 20]) -> Value {
    json!({
        "TransactionType": "EscrowCancel",
        "Account": hex::encode(by),
        "Owner": hex::encode(ALICE),
        "OfferSequence": 5,
        "Fee": "12",
        "Sequence": 7,
        "Flags": 0,
    })
}

#[test]
fn not_even_the_owner_cancels_before_cancel_after() {
    let mut state = ledger(&[ALICE, BOB]);
    escrow(&mut state, Some(PARENT_CLOSE + 100));
    let (ter, mods) = apply(&state, cancel(ALICE));
    assert_eq!(ter, "tecNO_PERMISSION");
    only_the_fee(&mods, &ALICE);
}

#[test]
fn an_escrow_without_cancel_after_cannot_be_cancelled() {
    let mut state = ledger(&[ALICE, BOB]);
    escrow(&mut state, None);
    let (ter, mods) = apply(&state, cancel(BOB));
    assert_eq!(ter, "tecNO_PERMISSION");
    only_the_fee(&mods, &BOB);
}

// ---- A5: PermissionedDomainSet's credential issuer (PermissionedDomainSet.cpp:57-62) ----

#[test]
fn a_domain_accepting_a_missing_issuers_credential_is_tec_no_issuer() {
    let state = ledger(&[ALICE]);
    let tx = json!({
        "TransactionType": "PermissionedDomainSet",
        "Account": hex::encode(ALICE),
        "AcceptedCredentials": [{ "Credential": { "Issuer": hex::encode(CAROL), "CredentialType": "4B5943" } }],
        "Fee": "12",
        "Sequence": 7,
        "Flags": 0,
    });
    let (ter, mods) = apply(&state, tx);
    assert_eq!(ter, "tecNO_ISSUER");
    only_the_fee(&mods, &ALICE);
}

// ---- A6: NFTokenMint with Amount runs tokenOfferCreatePreclaim (NFTokenMint.cpp:207-217) ----

#[test]
fn a_mint_offered_to_a_missing_destination_is_tec_no_dst() {
    let state = ledger(&[ALICE]);
    let tx = json!({
        "TransactionType": "NFTokenMint",
        "Account": hex::encode(ALICE),
        "NFTokenTaxon": 0,
        "Amount": "1000000",
        "Destination": hex::encode(CAROL),
        "Fee": "12",
        "Sequence": 7,
        "Flags": 8,
    });
    let (ter, mods) = apply(&state, tx);
    assert_eq!(ter, "tecNO_DST");
    only_the_fee(&mods, &ALICE);
}

/// A transferable token with a royalty, offered for an IOU: its issuer (the minter) needs a
/// line for that IOU (NFTokenHelpers.cpp:831-847). ALICE has none with GATE.
#[test]
fn a_mint_with_a_royalty_in_an_iou_needs_the_issuers_trust_line() {
    let state = ledger(&[ALICE, GATE]);
    let tx = json!({
        "TransactionType": "NFTokenMint",
        "Account": hex::encode(ALICE),
        "NFTokenTaxon": 0,
        "TransferFee": 1000,
        "Amount": { "currency": "USD", "issuer": hex::encode(GATE), "value": "10" },
        "Fee": "12",
        "Sequence": 7,
        "Flags": 8,
    });
    let (ter, mods) = apply(&state, tx);
    assert_eq!(ter, "tecNO_LINE");
    only_the_fee(&mods, &ALICE);
}

// ---- Clawback: the holder before the issuer's flags (Clawback.cpp:189-192, then :108-109) ----

/// ALICE never enabled clawback (no lsfAllowTrustLineClawback), and CAROL, the holder, doesn't
/// exist. rippled answers terNO_ACCOUNT (a retry) before looking at ALICE's flags; we said
/// tecNO_PERMISSION (claimed).
#[test]
fn a_clawback_from_a_missing_holder_is_ter_no_account_before_the_flags() {
    let state = ledger(&[ALICE]);
    let tx = json!({
        "TransactionType": "Clawback",
        "Account": hex::encode(ALICE),
        "Amount": { "currency": "USD", "issuer": hex::encode(CAROL), "value": "10" },
        "Fee": "12",
        "Sequence": 7,
        "Flags": 0,
    });
    let (ter, mods) = apply(&state, tx);
    assert_eq!(ter, "terNO_ACCOUNT");
    assert!(mods.is_empty(), "a ter changes nothing: {mods:?}");
}

// ---- A3: AMMBid's AuthAccounts must exist (AMMBid.cpp:118-127) ----

#[test]
fn a_bid_naming_a_missing_auth_account_is_ter_no_account() {
    let mut state = ledger(&[ALICE, GATE]);
    let mut usd = [0u8; 20];
    usd[12..15].copy_from_slice(b"USD");
    let pool = [0xAA; 20];
    let amm = json!({
        "LedgerEntryType": "AMM",
        "Account": hex::encode(pool),
        "Asset": { "currency": "XRP" },
        "Asset2": { "currency": "USD", "issuer": hex::encode(GATE) },
        "LPTokenBalance": {
            "currency": hex::encode(keylet::amm_lpt_currency(&[0u8; 20], &usd)),
            "issuer": hex::encode(pool),
            "value": "1000",
        },
        "TradingFee": 0,
        "Flags": 0,
    });
    put(&mut state, keylet::amm_key(&[0u8; 20], &[0u8; 20], &usd, &GATE), &amm);
    let tx = json!({
        "TransactionType": "AMMBid",
        "Account": hex::encode(ALICE),
        "Asset": { "currency": "XRP" },
        "Asset2": { "currency": "USD", "issuer": hex::encode(GATE) },
        "AuthAccounts": [{ "AuthAccount": { "Account": hex::encode(CAROL) } }],
        "Fee": "12",
        "Sequence": 7,
        "Flags": 0,
    });
    let (ter, mods) = apply(&state, tx);
    assert_eq!(ter, "terNO_ACCOUNT");
    assert!(mods.is_empty(), "a ter changes nothing: {mods:?}");
}

// ---- A2: checkPermission for a standalone delegated transaction (applySteps.cpp:188-190) ----

/// DAVE signs as ALICE's Delegate, but ALICE's Delegate object for him grants TrustSet only
/// (permission value 21: TrustSet's type 20 + 1), not Payment. The object is loaded, so even a
/// partial state can judge it. tests/missing_pool.rs covers a missing object, which only a
/// complete state can refuse.
#[test]
fn a_delegated_payment_its_delegate_object_does_not_grant_is_ter_no_delegate_permission() {
    let mut state = ledger(&[ALICE, BOB, DAVE]);
    let delegate = json!({
        "LedgerEntryType": "Delegate",
        "Account": hex::encode(ALICE),
        "Authorize": hex::encode(DAVE),
        "Permissions": [{ "Permission": { "PermissionValue": 21 } }],
        "OwnerNode": "0",
        "Flags": 0,
    });
    put(&mut state, keylet::delegate_key(&ALICE, &DAVE), &delegate);
    let tx = json!({
        "TransactionType": "Payment",
        "Account": hex::encode(ALICE),
        "Destination": hex::encode(BOB),
        "Amount": "1000000",
        "Delegate": hex::encode(DAVE),
        "Fee": "12",
        "Sequence": 7,
        "Flags": 0,
    });
    let (ter, mods) = apply(&state, tx);
    assert_eq!(ter, "terNO_DELEGATE_PERMISSION");
    assert!(mods.is_empty(), "a ter changes nothing: {mods:?}");
}
