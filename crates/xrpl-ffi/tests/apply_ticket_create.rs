//! Integration test: apply a real mainnet TicketCreate via Rust → FFI → libxrpl
//! and assert the post-apply AccountRoot has `Sequence` advanced by
//! `TicketCount + 1`.
//!
//! ## Why this test exists
//!
//! The FFI shadow verifier on m3060 logs ~54 tx divergences clustered on
//! `terPRE_SEQ` / `tefPAST_SEQ` post-TicketCreate. Every cascade follows the
//! same shape: the first divergent tx is the one immediately after a
//! successful TicketCreate, with `tx.Sequence = prev.Sequence + 2` (1 ticket
//! reserved + 1 for the tx itself). The hypothesis is that libxrpl's apply of
//! TicketCreate — or the `MutationCollector` hook that receives its mutations
//! — leaves the AccountRoot's Sequence off by one, so the next tx reads a
//! stale seq from the overlay and hits `terPRE_SEQ`.
//!
//! **If this test fails:** the bug is in the libxrpl binding / C++ collector
//! path. Fix there.
//!
//! **If this test passes:** libxrpl is emitting the correct post-apply
//! AccountRoot. The bug is upstream in the overlay threading order or
//! somewhere in `apply_ledger_in_order`. Investigation continues there.
//!
//! ## Test data
//!
//! Tx: `C873F269F2355C34F9335548FC5368536997CEFD8AFF47B6BAA401E1665409C1`
//! - Real mainnet TicketCreate from ledger 103515367
//! - Account: rBtVeRQ8NWUjco5scoivCQbUe7Lbdbb7Me
//! - Sequence: 102165188 (0x0616EAC4)
//! - TicketCount: 1
//! - Fee: 12 drops
//! - Result in rippled: tesSUCCESS, post-Sequence: 102165190 (0x0616EAC6)
//!
//! The SLE bytes below are the pre-ledger AccountRoot for the sender, with
//! ONE byte-level patch: Balance bumped from 2,399,556 drops (the real
//! pre-ledger value, which is ~500 drops short of the ticket reserve) to
//! 10,000,000 drops (10 XRP) so the reserve check passes. The Sequence
//! field is unmodified — it's already 102165188 pre-tx, matching tx.Sequence.
//! The DirectoryNode is provided verbatim from pre-ledger 103515366.

use xrpl_ffi::{apply_with_mutations, LedgerInfo, MemoryProvider, MutationKind};

/// Real mainnet TicketCreate tx bytes (binary form).
/// Fields: Type=TicketCreate, Sequence=102165188, TicketCount=1, Fee=12,
/// Account=rBtVeRQ8..., signed ed25519.
const TICKET_CREATE_TX: &str = concat!(
    "12000A240616EAC420280000000168400000000000000C73",
    "21ED56BE608F7FA62F99D7920C91AC0412E116807370F7A816067D2B3E4B7A90C5FA",
    "744099BD6028D99EF20D14E4BAD16D1AED0ADEB672896933D3989AAF1C6E6EB746F2",
    "7838E88F6A06680ABA5B6C0D089DC08E99A33C06045123705E1E7BE1DDA77706",
    "8114776E623952A78D90F0CCDB342771CDB8F7EA654C",
);

/// Sender AccountRoot keylet (the 32-byte SLE key for rBtVeRQ8...'s AccountRoot).
const SENDER_KEYLET: &str = "2B7EE4494372DAAEE9226010B426EFDF3B563DFA7257046F73AE3F31D142D1A9";

/// Pre-ledger sender AccountRoot bytes, with Balance patched from
/// 4000000000249D44 (2,399,556 drops) → 4000000000989680 (10,000,000 drops).
/// All other fields unchanged. Sequence is 0x0616EAC4 = 102165188, which
/// matches the tx's Sequence field.
const SENDER_SLE_PATCHED_BALANCE: &str = concat!(
    "1100612200000000",           // LedgerEntryType=AccountRoot, Flags=0
    "240616EAC4",                 // Sequence=102165188
    "25062B84E6",                 // PreviousTxnLgrSeq
    "2D00000006",                 // OwnerCount=6
    "55DC2C2B92E5BAC9A10208927D079DAB26680DA4713D21F1E2FCFF4EF627E7A9C7", // PreviousTxnID
    "624000000000989680",         // Balance=10,000,000 drops (patched)
    "8114776E623952A78D90F0CCDB342771CDB8F7EA654C", // Account
);

/// Owner DirectoryNode keylet for the sender.
const OWNER_DIR_KEYLET: &str = "046B38DBB039CC558305235BBE2BACE9E4F914D919012B1D859DA93FD8A1E6E4";

/// Pre-ledger sender OwnerDir bytes, verbatim from ledger 103515366. Holds
/// 6 indexes (6 entries × 32 bytes = 192 bytes, matching OwnerCount=6).
const OWNER_DIR_SLE: &str = concat!(
    "1100642200000000",           // LedgerEntryType=DirectoryNode, Flags=0
    "25062B84DF",                 // PreviousTxnLgrSeq
    "310000000000000000",         // IndexNext=0
    "320000000000000000",         // IndexPrevious=0
    "55400D1E5493AFFC8D133184A53C9CCB43260F806F0130D936D6B73F11D106DC7A", // PreviousTxnID
    "58046B38DBB039CC558305235BBE2BACE9E4F914D919012B1D859DA93FD8A1E6E4", // RootIndex
    "8214776E623952A78D90F0CCDB342771CDB8F7EA654C", // Owner
    // sfIndexes (Vector256, 6 × 32 bytes = 192 bytes)
    "0113C0",                     // field code + length prefix 0xC0=192
    "56A640C180C8441EE6B1AAB28270639ED7626A2662F7288E4489A36E2F20EF0A9",
    "4709F2423B065146470BCA3B23E6B7C1DEC00FF590B992B18DF0E88D6A4EF1EC5",
    "531254FF74BE9A6AD3A40D3DCF15357DAC26B26B957C2153ECCD07DC3C770FD38",
    "8EDB28BC8C99223D964E0A46D39362812C53D3907001A12013366CC15BAE2E40A",
    "1558DC7638FF795B2D8D10C59F8624C52988A254D51EA4A92700F14E4F68F5D5D",
    "29A6A3CFA7A5E9EDFDEF22620961F680DBF2BF30D1F515253754C9A0DD9",
);

fn hex_to_bytes(s: &str) -> Vec<u8> {
    hex::decode(s).expect("hex decode")
}

fn hex_to_array32(s: &str) -> [u8; 32] {
    let v = hex_to_bytes(s);
    assert_eq!(v.len(), 32, "keylet must be 32 bytes");
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    a
}

/// Scan a serialized AccountRoot SLE for the `Sequence` field (UInt32, field 4).
/// Encoding: type_code=2, field_code=4 → single byte 0x24 followed by 4
/// big-endian bytes.
fn scan_account_sequence(data: &[u8]) -> Option<u32> {
    for i in 0..data.len().saturating_sub(5) {
        if data[i] == 0x24 {
            return Some(u32::from_be_bytes(data[i + 1..i + 5].try_into().ok()?));
        }
    }
    None
}

#[test]
fn ticket_create_advances_sequence_by_count_plus_one() {
    let tx = hex_to_bytes(TICKET_CREATE_TX);
    let sender_key = hex_to_array32(SENDER_KEYLET);
    let dir_key = hex_to_array32(OWNER_DIR_KEYLET);
    let sender_sle = hex_to_bytes(SENDER_SLE_PATCHED_BALANCE);
    let dir_sle = hex_to_bytes(OWNER_DIR_SLE);

    // Sanity: verify the pre-Sequence in our patched SLE decodes correctly.
    assert_eq!(
        scan_account_sequence(&sender_sle),
        Some(102_165_188),
        "patched SLE should have pre-Sequence 102165188"
    );

    let mut provider = MemoryProvider::new();
    provider.insert(sender_key, sender_sle);
    provider.insert(dir_key, dir_sle);

    let ledger = LedgerInfo {
        seq: 103_515_367,
        parent_close_time: 797_804_900, // approximate; not hash-sensitive for apply
        total_drops: 99_985_687_626_634_189,
        parent_hash: [0u8; 32],
        base_fee_drops: 10,
        reserve_drops: 1_000_000,  // 1 XRP mainnet base
        increment_drops: 200_000,  // 0.2 XRP per owned object
    };

    let outcome = apply_with_mutations(&tx, &[], &ledger, &provider, 0, 0)
        .expect("apply returned null");

    println!(
        "ticket_create apply: TER={} ({}) applied={} mutations={}",
        outcome.ter,
        outcome.ter_name,
        outcome.applied,
        outcome.mutations.len(),
    );
    for m in &outcome.mutations {
        let kind_str = match m.kind {
            MutationKind::Created => "Created",
            MutationKind::Modified => "Modified",
            MutationKind::Deleted => "Deleted",
        };
        println!(
            "  {kind_str:<8}  key={}  {} bytes",
            &hex::encode_upper(m.key)[..16],
            m.data.len(),
        );
    }
    if !outcome.last_fatal.is_empty() {
        println!("last_fatal: {}", outcome.last_fatal);
    }

    assert!(
        outcome.is_success(),
        "apply should return tesSUCCESS, got {}",
        outcome.ter_name
    );

    // Find the AccountRoot mutation and pull its Sequence.
    let account_mut = outcome
        .mutations
        .iter()
        .find(|m| m.key == sender_key)
        .expect("outcome.mutations must include an AccountRoot mutation for sender");

    assert_eq!(
        account_mut.kind,
        MutationKind::Modified,
        "sender AccountRoot should be Modified (not Created/Deleted)"
    );

    let post_seq = scan_account_sequence(&account_mut.data)
        .expect("post-apply AccountRoot must decode Sequence");

    println!("pre Sequence:  102165188");
    println!("post Sequence: {post_seq}");
    println!("delta:         {}", post_seq as i64 - 102_165_188_i64);

    // The assertion that nails the bug: TicketCount=1, so seq must advance by 2.
    assert_eq!(
        post_seq, 102_165_190,
        "post-TicketCreate Sequence must equal pre + TicketCount + 1 (188 + 2 = 190). \
         If this assertion fails, libxrpl/FFI is emitting a stale AccountRoot for \
         TicketCreate — that is the root cause of the 54 sequence divergences."
    );
}

/// The second TicketCreate from the same account, immediately after C873F269
/// in ledger 103515367. Its Sequence is 102165190 (= first TC's 188 + 2).
/// In production this got `terPRE_SEQ` from FFI; rippled applied it successfully.
const TICKET_CREATE_TX_SECOND: &str = "12000A240616EAC620280000000168400000000000000C7321ED56BE608F7FA62F99D7920C91AC0412E116807370F7A816067D2B3E4B7A90C5FA74402763A72E9AB829CF757D668127B37D8BECE153DDD9D9014025F0CE181B0E6E00AB938607D1C188CA7ECF6FA9DF1F7F9C2F85817CA0ECA2F9A33E75FF8ED197068114776E623952A78D90F0CCDB342771CDB8F7EA654C";

/// Multi-tx simulation: apply TC@188, thread its mutations back into the
/// provider, then apply TC@190 on top. This replicates how `apply_ledger_in_order`
/// handles consecutive txs from the same account within a single ledger.
///
/// **If TC@190 returns `tesSUCCESS`:** the libxrpl / FFI / provider chain all
/// work correctly across consecutive TicketCreate calls. The production
/// divergence must come from somewhere ELSE — possibly an earlier cross-account
/// tx modifying the AccountRoot in a way that corrupts the threaded state, or
/// a subtle overlay race.
///
/// **If TC@190 returns `terPRE_SEQ`:** we've reproduced the production bug in
/// a reproducible unit test. The bug is in libxrpl's read-back or provider
/// integration for TicketCreate's post-apply state.
#[test]
fn ticket_create_followed_by_next_sequence_tx_succeeds() {
    let tx1 = hex_to_bytes(TICKET_CREATE_TX);
    let tx2 = hex_to_bytes(TICKET_CREATE_TX_SECOND);
    let sender_key = hex_to_array32(SENDER_KEYLET);
    let dir_key = hex_to_array32(OWNER_DIR_KEYLET);
    let sender_sle = hex_to_bytes(SENDER_SLE_PATCHED_BALANCE);
    let dir_sle = hex_to_bytes(OWNER_DIR_SLE);

    let mut provider = MemoryProvider::new();
    provider.insert(sender_key, sender_sle);
    provider.insert(dir_key, dir_sle);

    let ledger = LedgerInfo {
        seq: 103_515_367,
        parent_close_time: 797_804_900,
        total_drops: 99_985_687_626_634_189,
        parent_hash: [0u8; 32],
        base_fee_drops: 10,
        reserve_drops: 1_000_000,
        increment_drops: 200_000,
    };

    // === Step 1: apply TC@188 ===
    let outcome1 = apply_with_mutations(&tx1, &[], &ledger, &provider, 0, 0)
        .expect("tx1 apply returned null");
    println!(
        "tx1 (TC@188): TER={} ({}) mutations={}",
        outcome1.ter, outcome1.ter_name, outcome1.mutations.len()
    );
    assert!(outcome1.is_success(), "tx1 must succeed, got {}", outcome1.ter_name);

    // === Step 2: thread tx1's mutations back into the provider ===
    // Same logic as `apply_ledger_in_order`'s threading loop.
    for m in &outcome1.mutations {
        match m.kind {
            MutationKind::Deleted => {
                // TicketCreate shouldn't delete anything; panic if this ever fires
                panic!("unexpected delete in TC mutation for key {}", hex::encode_upper(m.key));
            }
            _ => {
                provider.insert(m.key, m.data.clone());
            }
        }
    }

    // Sanity-check: the threaded AccountRoot has the new post-TC Sequence.
    let threaded_sender = provider.read(&sender_key).expect("sender must be in provider");
    let threaded_seq = scan_account_sequence(threaded_sender)
        .expect("threaded AccountRoot must decode Sequence");
    println!("threaded sender Sequence after TC@188: {threaded_seq}");
    assert_eq!(
        threaded_seq, 102_165_190,
        "after threading TC@188's mutation, the provider's AccountRoot must have Sequence=190"
    );

    // === Step 3: apply TC@190 on top ===
    let outcome2 = apply_with_mutations(&tx2, &[], &ledger, &provider, 0, 0)
        .expect("tx2 apply returned null");
    println!(
        "tx2 (TC@190): TER={} ({}) mutations={}",
        outcome2.ter, outcome2.ter_name, outcome2.mutations.len(),
    );
    if !outcome2.last_fatal.is_empty() {
        println!("  last_fatal: {}", outcome2.last_fatal);
    }

    // The production FFI on m3060 reports terPRE_SEQ here. Rippled applied
    // both TC@188 and TC@190 successfully. If this test ALSO returns terPRE_SEQ
    // we've reproduced the production bug. If it returns tesSUCCESS, the bug
    // is not in libxrpl's isolated per-tx apply.
    assert!(
        outcome2.is_success(),
        "TC@190 should succeed after TC@188 threaded. Got {} — this is the \
         exact failure we see in production at ledger 103515367. REPRODUCED.",
        outcome2.ter_name
    );
}

// We need a `read` method on MemoryProvider for the assertion above.
// It's already implemented via the SleProvider trait.
use xrpl_ffi::SleProvider;
