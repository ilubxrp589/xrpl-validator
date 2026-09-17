# Batch Leg B — Native Batch Transactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the native Rust engine apply a `Batch` transaction the way rippled 3.3.0 does — outer fee/sequence, then the inner transactions on nested sandboxes under the four modes — so the shadow engine stays at parity when BatchV1_1 activates on mainnet.

**Architecture:** A new `BatchTransactor` (crates/xrpl-ledger/src/tx/batch.rs) validates the outer transaction (flags, inner-field rules, uniqueness, BatchSigners structure, fee formula), and in `do_apply` runs each inner through a new sandbox-level applier `apply_on_sandbox` with snapshot/rollback, folding tes/tec results per rippled's `applyBatchTransactions`. Inner transactions are ordinary `TxFields` with a new `inner_batch: bool` flag that relaxes the fee-zero gate every transactor currently enforces. The native shadow skips inner ledger entries and attributes the outer's mutation set against the union of the outer's and inners' metadata (the same fold leg A already does for the FFI leg). Devnet Batch ledgers become byte-exact vectors through a merged bundle (outer ∪ inners).

**Tech Stack:** Rust (crates `xrpl-ledger`, `xrpl-node`), `serde_json`, the existing `Sandbox` snapshot API, Python 3 for the bundle merge script, the existing `fetch_ledger_fixture.py` / `fetch_tx_bundle.py` tooling.

**Spec:** `docs/superpowers/specs/2026-09-14-batch-support-design.md` — section "Leg B: native engine (shadow parity)" and "rippled semantics (3.3.0, Batch.cpp / apply.cpp / Transactor.cpp)". The plan argues from that spec; read both.

## Global Constraints

- Work in a fresh worktree off `main` (currently `7bd0c74`): `git worktree add /tmp/claude-1000/-home-localai/d6509b21-22a5-467b-a51d-16eb8c7262a5/scratchpad/wt244 -b t0-batch-b main`. Build with `CARGO_TARGET_DIR=/tmp/claude-1000/-home-localai/d6509b21-22a5-467b-a51d-16eb8c7262a5/scratchpad/wt244_target` (its own target dir — shared target dirs have produced stale test binaries before).
- Commit format: `feat(xrpl-ledger): …` / `fix(xrpl-node): …` / `test(xrpl-node): …`. End every commit message with the two attribution lines given in the session (Co-Authored-By + Claude-Session).
- No `unwrap()` / `expect()` in library code (crates/xrpl-ledger/src, crates/xrpl-node/src); tests may use them.
- `cargo clippy -p xrpl-ledger -p xrpl-node -- -D warnings` must stay clean.
- Cap of 4 unpushed commits on the branch: at 4, push `t0-batch-b` to origin from .39 before the next commit.
- rippled's exact rules are the spec's "rippled semantics" section; when this plan and rippled disagree, rippled wins — quote the C++ in the comment.
- The validator on m3060 is never touched by this work; nothing here deploys.
- Devnet RPC for fixtures and bundles: `https://s.devnet.rippletest.net:51234`. `fetch_tx_bundle.py` reads its RPC from the env var `XRPL_RPC` (default `http://127.0.0.1:5005/`); `fetch_ledger_fixture.py` takes `--rpc`.
- Flag bit values (from rippled `TxFlags.h`, quoted in the spec): `tfAllOrNothing 0x0001_0000`, `tfOnlyOne 0x0002_0000`, `tfUntilFailure 0x0004_0000`, `tfIndependent 0x0008_0000`, `tfInnerBatchTxn 0x4000_0000`. `maxBatchTxCount = 8`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/xrpl-ledger/src/ledger/transactor.rs` (modify) | `TxFields` gains `inner_batch: bool` and `TxFields::from_json`; `TxResult` gains the Batch result codes; `fee_missing()` helper. |
| `crates/xrpl-ledger/src/tx/*.rs` (modify, mechanical) | every `tx.fee == 0` fee gate becomes `tx.fee_missing()`. |
| `crates/xrpl-ledger/src/tx/dispatch.rs` (modify) | registers `"Batch"`; new `apply_on_sandbox(tx, sb) -> (TxResult, bool)` — the full pipeline on an existing sandbox with rollback. |
| `crates/xrpl-ledger/src/tx/batch.rs` (create) | `BatchTransactor`: preflight, preclaim, `do_apply` (the inner loop), `batch_base_fee`, `take_inner_results()`. |
| `crates/xrpl-ledger/src/tx/mod.rs` (modify) | `pub mod batch;` |
| `crates/xrpl-node/src/native_apply.rs` (modify) | `build_txfields` delegates to `TxFields::from_json`; new pure `batch_attribution(ordered)`. |
| `crates/xrpl-node/src/native_shadow.rs` (modify) | skips inner entries, folds inner metas into the outer's compare, reports inner TER mismatches. |
| `scripts/merge_batch_bundle.py` (create) | merges the outer's bundle with each inner's bundle into one Batch bundle. |
| `crates/xrpl-node/tests/batch_vector.rs` (create) | the byte-exact vector suite for Batch. |
| `crates/xrpl-node/tests/vectors/batch_*.json` (create) | the devnet vectors. |
| scratchpad `gate54.sh` (modify) | adds `batch_vector` to the gate's suite list `V=`. |

---

### Task 1: `TxFields::inner_batch`, `TxFields::from_json`, and the Batch result codes

**Files:**
- Modify: `crates/xrpl-ledger/src/ledger/transactor.rs:354-376` (struct + impl), the `TxResult` enum (line ~31 onward) and its `code_str` match (line ~284-340)
- Modify: `crates/xrpl-node/src/native_apply.rs:68-92` (`build_txfields`)
- Test: unit tests inside `transactor.rs` (`mod tests`)

**Interfaces:**
- Produces: `TxFields { …existing…, pub inner_batch: bool }`; `TxFields::from_json(txjson: &serde_json::Value) -> Option<TxFields>` (sets `inner_batch: false`); `TxFields::fee_missing(&self) -> bool`; `TxResult::{InvalidFlag, Redundant, BadSigner, InvalidInnerBatch, ArrayEmpty, TemArrayTooLarge, SeqAndTicket, BadSignature, InvalidTx}` with `code_str` = `temINVALID_FLAG`, `temREDUNDANT`, `temBAD_SIGNER`, `temINVALID_INNER_BATCH`, `temARRAY_EMPTY`, `temARRAY_TOO_LARGE`, `temSEQ_AND_TICKET`, `temBAD_SIGNATURE`, `temINVALID`. All nine are `tem` codes: `is_claimed()` must return false for them (check the existing `is_claimed` implementation only names `tec` variants — do not add them there).

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod tests` block at the bottom of `transactor.rs`:

```rust
    #[test]
    fn txfields_from_json_reads_the_common_fields_and_defaults_inner_batch_off() {
        let tx = serde_json::json!({
            "TransactionType": "Payment",
            "Account": "0000000000000000000000000000000000000001",
            "Fee": "12",
            "Sequence": 7,
            "LastLedgerSequence": 99,
            "Amount": "1000000",
        });
        let f = TxFields::from_json(&tx).expect("fields");
        assert_eq!(f.tx_type, "Payment");
        assert_eq!(f.fee, 12);
        assert_eq!(f.sequence, 7);
        assert_eq!(f.ticket_seq, None);
        assert_eq!(f.last_ledger_seq, Some(99));
        assert!(!f.inner_batch);
        assert!(!f.fee_missing());
    }

    #[test]
    fn fee_missing_is_waived_for_a_batch_inner() {
        let tx = serde_json::json!({
            "TransactionType": "Payment",
            "Account": "0000000000000000000000000000000000000001",
            "Fee": "0",
            "Sequence": 7,
        });
        let mut f = TxFields::from_json(&tx).expect("fields");
        assert!(f.fee_missing(), "a standalone zero-fee tx is missing its fee");
        f.inner_batch = true;
        assert!(!f.fee_missing(), "a batch inner carries Fee 0 by rule");
    }

    #[test]
    fn batch_result_codes_have_their_rippled_names() {
        assert_eq!(TxResult::InvalidFlag.code_str(), "temINVALID_FLAG");
        assert_eq!(TxResult::Redundant.code_str(), "temREDUNDANT");
        assert_eq!(TxResult::BadSigner.code_str(), "temBAD_SIGNER");
        assert_eq!(TxResult::InvalidInnerBatch.code_str(), "temINVALID_INNER_BATCH");
        assert_eq!(TxResult::ArrayEmpty.code_str(), "temARRAY_EMPTY");
        assert_eq!(TxResult::TemArrayTooLarge.code_str(), "temARRAY_TOO_LARGE");
        assert_eq!(TxResult::SeqAndTicket.code_str(), "temSEQ_AND_TICKET");
        assert_eq!(TxResult::BadSignature.code_str(), "temBAD_SIGNATURE");
        assert_eq!(TxResult::InvalidTx.code_str(), "temINVALID");
        for r in [TxResult::InvalidFlag, TxResult::Redundant, TxResult::BadSigner,
                  TxResult::InvalidInnerBatch, TxResult::ArrayEmpty, TxResult::TemArrayTooLarge,
                  TxResult::SeqAndTicket, TxResult::BadSignature, TxResult::InvalidTx] {
            assert!(!r.is_claimed(), "{:?} is a tem code, never claimed", r);
            assert!(!r.is_success());
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib txfields_from_json fee_missing batch_result_codes 2>&1 | tail -20` (with `T` set to the worktree's target dir)
Expected: compile errors — `from_json`, `inner_batch`, `fee_missing`, and the new variants do not exist.

- [ ] **Step 3: Add the field, the constructor, the helper and the variants**

In `transactor.rs`, extend the struct:

```rust
pub struct TxFields {
    pub account: [u8; 20],
    pub tx_type: String,
    pub fee: u64,
    pub sequence: u32,
    pub ticket_seq: Option<u32>,
    pub last_ledger_seq: Option<u32>,
    pub fields: serde_json::Value,
    /// Set by `BatchTransactor` for the inner transactions it applies
    /// (rippled's `tapBATCH`): the inner carries `Fee: "0"` by rule, so
    /// the fee-zero gate every transactor enforces is waived, and no
    /// signature is expected.
    pub inner_batch: bool,
}
```

Then, in `impl TxFields`, add:

```rust
    /// The preflight fee gate: a standalone transaction with `Fee: "0"`
    /// is malformed (`temBAD_FEE`); a batch inner carries `Fee: "0"` by
    /// rule (rippled preflight1 under `tapBATCH`).
    pub fn fee_missing(&self) -> bool {
        self.fee == 0 && !self.inner_batch
    }

    /// The common-field reader that used to live in
    /// `xrpl_node::native_apply::build_txfields`. Accepts the engine's hex
    /// account dialect and the pseudo-transaction zero account.
    pub fn from_json(txjson: &serde_json::Value) -> Option<TxFields> {
        // ⬇ move the body of `build_txfields` (native_apply.rs:68-92) here
        //    unchanged, and set `inner_batch: false` in the struct literal.
        todo_move_body_here
    }
```

Move the body of `build_txfields` from `crates/xrpl-node/src/native_apply.rs` lines 68-92 into `from_json` (the account decoding, fee/sequence/ticket/last-ledger reads, and the `fields: txjson.clone()` — whatever the current body does, byte for byte), adding `inner_batch: false` to the struct literal it builds. Replace the placeholder line above with that body. Then make `build_txfields` a one-line delegate:

```rust
pub fn build_txfields(txjson: &Value) -> Option<TxFields> {
    TxFields::from_json(txjson)
}
```

Every other place in the repo that constructs `TxFields { … }` by literal (search: `grep -rn "TxFields {" crates/ --include=*.rs`) must gain `inner_batch: false,` — the compiler will list them.

Add the variants to `TxResult` (next to the other `tem` codes, e.g. after `BadFee`):

```rust
    /// temINVALID_FLAG — Batch: not exactly one mode flag, or tfInnerBatchTxn on the outer.
    InvalidFlag,
    /// temREDUNDANT — Batch: duplicate inner, or duplicate (account, sequence) under AllOrNothing/UntilFailure.
    Redundant,
    /// temBAD_SIGNER — Batch: BatchSigners not sorted/unique, missing or spurious signer.
    BadSigner,
    /// temINVALID_INNER_BATCH — an inner without tfInnerBatchTxn, or tfInnerBatchTxn without a parent batch.
    InvalidInnerBatch,
    /// temARRAY_EMPTY — Batch: RawTransactions absent or empty.
    ArrayEmpty,
    /// temARRAY_TOO_LARGE — Batch: more than 8 inners or signers (the tem code; `ArrayTooLarge` is the tec).
    TemArrayTooLarge,
    /// temSEQ_AND_TICKET — an inner with both or neither of Sequence / TicketSequence.
    SeqAndTicket,
    /// temBAD_SIGNATURE — an inner carrying SigningPubKey / TxnSignature / Signers.
    BadSignature,
    /// temINVALID — an inner of a disallowed type (Batch inside Batch, pseudo types).
    InvalidTx,
```

and their names in `code_str`:

```rust
            TxResult::InvalidFlag => "temINVALID_FLAG",
            TxResult::Redundant => "temREDUNDANT",
            TxResult::BadSigner => "temBAD_SIGNER",
            TxResult::InvalidInnerBatch => "temINVALID_INNER_BATCH",
            TxResult::ArrayEmpty => "temARRAY_EMPTY",
            TxResult::TemArrayTooLarge => "temARRAY_TOO_LARGE",
            TxResult::SeqAndTicket => "temSEQ_AND_TICKET",
            TxResult::BadSignature => "temBAD_SIGNATURE",
            TxResult::InvalidTx => "temINVALID",
```

If `TxResult` derives `Debug` already (the test uses `{:?}`), nothing more; otherwise add `#[derive(Debug)]` — check the existing derive line.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib txfields_from_json fee_missing batch_result_codes 2>&1 | tail -5`
Expected: `test result: ok. 3 passed`

Then the whole workspace must still build and its suites pass:
Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib 2>&1 | grep "test result"` and `CARGO_TARGET_DIR=$T cargo build -p xrpl-node --tests 2>&1 | grep -E "^error|Finished"`
Expected: lib `ok`, node tests build `Finished`.

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-ledger/src/ledger/transactor.rs crates/xrpl-node/src/native_apply.rs
git commit -m "feat(xrpl-ledger): TxFields::from_json, inner_batch flag and the Batch tem codes"
```

---

### Task 2: Waive the fee-zero gate for batch inners (mechanical)

**Files:**
- Modify: every file in `crates/xrpl-ledger/src/tx/` containing `tx.fee == 0` (account.rs, check.rs, credential.rs, mpt.rs, pay_channel.rs, misc.rs, oracle.rs, ticket.rs, nftoken.rs, trust_set.rs, amm.rs, escrow.rs, payment.rs, xchain.rs, offer.rs — 45 sites)
- Test: unit test in `crates/xrpl-ledger/src/tx/payment.rs` (or its existing `mod tests`)

**Interfaces:**
- Consumes: `TxFields::fee_missing()` (Task 1).
- Produces: every transactor's preflight passes a zero-fee tx when `inner_batch` is set.

- [ ] **Step 1: Write the failing test**

In `crates/xrpl-ledger/src/tx/payment.rs`, inside its `#[cfg(test)] mod tests` (create the module at the bottom of the file if the file has none):

```rust
    #[test]
    fn payment_preflight_waives_fee_zero_for_a_batch_inner() {
        use crate::ledger::transactor::{Transactor, TxFields, TxResult};
        let tx = serde_json::json!({
            "TransactionType": "Payment",
            "Account": "0000000000000000000000000000000000000001",
            "Destination": "0000000000000000000000000000000000000002",
            "Amount": "1000000",
            "Fee": "0",
            "Sequence": 3,
            "Flags": 0x4000_0000u64,
        });
        let mut f = TxFields::from_json(&tx).expect("fields");
        assert_eq!(PaymentTransactor.preflight(&f), TxResult::BadFee, "standalone: Fee 0 is temBAD_FEE");
        f.inner_batch = true;
        assert_ne!(PaymentTransactor.preflight(&f), TxResult::BadFee, "inner: Fee 0 is the rule");
    }
```

If `TxResult` does not implement `PartialEq`, compare `code_str()` strings instead (`assert_eq!(….code_str(), "temBAD_FEE")`).

- [ ] **Step 2: Run the test to verify it fails**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib payment_preflight_waives_fee_zero 2>&1 | tail -6`
Expected: FAIL on the second assertion (`inner: Fee 0 is the rule`) — the gate still reads `tx.fee == 0`.

- [ ] **Step 3: Replace the gate everywhere**

```bash
cd crates/xrpl-ledger/src/tx
grep -c "tx.fee == 0" *.rs | grep -v ":0"          # 45 sites across 15 files, before
sed -i 's/tx\.fee == 0/tx.fee_missing()/g' account.rs check.rs credential.rs mpt.rs pay_channel.rs misc.rs oracle.rs ticket.rs nftoken.rs trust_set.rs amm.rs escrow.rs payment.rs xchain.rs offer.rs
grep -c "tx.fee == 0" *.rs | grep -v ":0"          # must print nothing, after
grep -c "tx.fee_missing()" *.rs | grep -v ":0"     # must total 45
```

Measure the outcome (the two greps) — do not trust `sed`'s silence.

- [ ] **Step 4: Run the test to verify it passes, then the whole lib**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib payment_preflight_waives_fee_zero 2>&1 | tail -3`
Expected: `ok. 1 passed`
Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib 2>&1 | grep "test result"`
Expected: `ok` (285+ tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-ledger/src/tx/
git commit -m "feat(xrpl-ledger): the fee-zero preflight gate is fee_missing(), waived for batch inners"
```

---

### Task 3: `apply_on_sandbox` — the full pipeline on an existing sandbox

**Files:**
- Modify: `crates/xrpl-ledger/src/tx/dispatch.rs` (add the function next to `get_transactor`)
- Test: unit tests in `dispatch.rs` (`mod tests`)

**Interfaces:**
- Consumes: `get_transactor(&str) -> Option<Box<dyn Transactor>>`, `is_pseudo(&str)`, `apply_common(&TxFields, &mut Sandbox) -> TxResult`, `Sandbox::{snapshot, restore_snapshot}`, `crate::ledger::transactor::{account_txn_id_armed, stamp_account_txn_id}`, `crate::ledger::apply::settle_expired(&mut Sandbox, snapshot)`.
- Produces: `pub fn apply_on_sandbox(tx: &TxFields, sb: &mut Sandbox) -> (TxResult, bool)` — returns the result and whether the transaction was *applied* (rippled's `applied`: true for `tes` and claimed `tec`). On `false` the sandbox is exactly as it was on entry.

This mirrors `xrpl_node::native_apply::native_apply_one` (native_apply.rs:94-165) but on a caller-supplied sandbox. Keep the two in step: the node function may later delegate to this one; that refactor is out of scope here.

- [ ] **Step 1: Write the failing tests**

Append to `dispatch.rs`:

```rust
#[cfg(test)]
mod apply_on_sandbox_tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet;
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use crate::ledger::transactor::{TxFields, TxResult};
    use xrpl_core::types::Hash256;

    fn state_with_accounts(accts: &[([u8; 20], u64, u32)]) -> LedgerState {
        let header = LedgerHeader {
            sequence: 1,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        let mut state = LedgerState::new_unverified(header);
        for (id, balance, seq) in accts {
            let acct_json = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(id),
                "Balance": balance.to_string(),
                "Sequence": seq,
                "OwnerCount": 0,
                "Flags": 0,
            });
            state.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&acct_json).unwrap()).unwrap();
        }
        state
    }

    fn acct(n: u8) -> [u8; 20] { let mut a = [0u8; 20]; a[19] = n; a }

    fn payment(from: [u8; 20], to: [u8; 20], drops: u64, fee: u64, seq: u32, inner: bool) -> TxFields {
        let tx = serde_json::json!({
            "TransactionType": "Payment",
            "Account": hex::encode(from),
            "Destination": hex::encode(to),
            "Amount": drops.to_string(),
            "Fee": fee.to_string(),
            "Sequence": seq,
            "Flags": if inner { 0x4000_0000u64 } else { 0 },
        });
        let mut f = TxFields::from_json(&tx).expect("fields");
        f.inner_batch = inner;
        f
    }

    fn balance_of(sb: &Sandbox, id: &[u8; 20]) -> (u64, u32) {
        let v: serde_json::Value = serde_json::from_slice(&sb.read(&keylet::account_root_key(id)).unwrap()).unwrap();
        (v["Balance"].as_str().unwrap().parse().unwrap(), v["Sequence"].as_u64().unwrap() as u32)
    }

    #[test]
    fn a_standalone_payment_moves_xrp_charges_the_fee_and_bumps_the_sequence() {
        let state = state_with_accounts(&[(acct(1), 50_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 1_000_000, 12, 5, false), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert!(applied);
        assert_eq!(balance_of(&sb, &acct(1)), (50_000_000 - 1_000_000 - 12, 6));
        assert_eq!(balance_of(&sb, &acct(2)), (21_000_000, 1));
    }

    #[test]
    fn a_batch_inner_pays_no_fee_but_consumes_its_sequence() {
        let state = state_with_accounts(&[(acct(1), 50_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 1_000_000, 0, 5, true), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert!(applied);
        assert_eq!(balance_of(&sb, &acct(1)), (49_000_000, 6));
    }

    #[test]
    fn a_failed_inner_leaves_the_sandbox_untouched() {
        let state = state_with_accounts(&[(acct(1), 50_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        // Wrong sequence: preclaim rejects (tefPAST_SEQ / temBAD_SEQUENCE), not applied.
        let before = sb.snapshot();
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 1_000_000, 0, 99, true), &mut sb);
        assert!(!applied, "{}", r.code_str());
        assert_eq!(sb.snapshot().len(), before.len(), "no entries written");
    }

    #[test]
    fn a_tec_inner_is_applied_with_only_its_sequence_consumed() {
        // 1 XRP balance, 0.2 XRP reserve rule aside: sending more than held is tecUNFUNDED_PAYMENT.
        let state = state_with_accounts(&[(acct(1), 1_000_000, 5), (acct(2), 20_000_000, 1)]);
        let mut sb = Sandbox::new(&state);
        let (r, applied) = apply_on_sandbox(&payment(acct(1), acct(2), 900_000_000, 0, 5, true), &mut sb);
        assert!(r.code_str().starts_with("tec"), "{}", r.code_str());
        assert!(applied, "a tec is claimed: applied with the common changes only");
        assert_eq!(balance_of(&sb, &acct(1)), (1_000_000, 6), "fee 0, sequence consumed, nothing moved");
        assert_eq!(balance_of(&sb, &acct(2)), (20_000_000, 1));
    }
}
```

If the sequence-mismatch case in the third test is caught by a different stage than expected, the assertion only requires `applied == false` and an unchanged sandbox — any of the engine's existing "bad sequence" outcomes satisfies it.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib apply_on_sandbox_tests 2>&1 | tail -8`
Expected: compile error — `apply_on_sandbox` not found.

- [ ] **Step 3: Implement `apply_on_sandbox`**

In `dispatch.rs`:

```rust
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{account_txn_id_armed, apply_common, stamp_account_txn_id, TxFields, TxResult};

/// The transaction pipeline on a caller-supplied sandbox — rippled's
/// `apply(app, view, tx, flags, j)` shape: preflight → preclaim →
/// common (fee, sequence/ticket) → doApply, with a claimed (`tec`)
/// result keeping only the common changes and any other failure leaving
/// the sandbox exactly as it was on entry. Returns `(result, applied)`
/// where `applied` is true for `tes` and claimed `tec` — the pair
/// `applyBatchTransactions` folds by.
///
/// Mirrors `xrpl_node::native_apply::native_apply_one`, which runs the
/// same pipeline on a fresh sandbox over a `LedgerState`.
pub fn apply_on_sandbox(tx: &TxFields, sb: &mut Sandbox) -> (TxResult, bool) {
    let entry = sb.snapshot();
    let Some(transactor) = get_transactor(&tx.tx_type) else {
        // An unsupported type is applied as its common changes only.
        let common = apply_common(tx, sb);
        if common.is_success() {
            return (TxResult::Unsupported, true);
        }
        sb.restore_snapshot(entry);
        return (common, false);
    };
    if is_pseudo(&tx.tx_type) {
        let pf = transactor.preflight(tx);
        if !pf.is_success() {
            return (pf, false);
        }
        let applied = transactor.do_apply(tx, sb);
        if applied.is_success() {
            return (TxResult::Success, true);
        }
        sb.restore_snapshot(entry);
        return (applied, false);
    }
    let preflight = transactor.preflight(tx);
    if !preflight.is_success() {
        if preflight.is_claimed() {
            let common = apply_common(tx, sb);
            if common.is_success() {
                return (preflight, true);
            }
            sb.restore_snapshot(entry);
            return (common, false);
        }
        return (preflight, false);
    }
    let preclaim = transactor.preclaim(tx, sb);
    if !preclaim.is_success() && !preclaim.is_claimed() {
        return (preclaim, false);
    }
    if !preclaim.is_success() {
        let common = apply_common(tx, sb);
        if common.is_success() {
            return (preclaim, true);
        }
        sb.restore_snapshot(entry);
        return (common, false);
    }
    let common = apply_common(tx, sb);
    if !common.is_success() {
        sb.restore_snapshot(entry);
        return (common, false);
    }
    let post_common = sb.snapshot();
    let txn_id_armed = account_txn_id_armed(tx, sb);
    let applied = transactor.do_apply(tx, sb);
    if applied.is_success() {
        stamp_account_txn_id(tx, sb, txn_id_armed);
        (TxResult::Success, true)
    } else if applied.is_claimed() {
        if applied == TxResult::Expired {
            crate::ledger::apply::settle_expired(sb, post_common);
        } else if applied != TxResult::Killed {
            sb.restore_snapshot(post_common);
        }
        (applied, true)
    } else {
        sb.restore_snapshot(entry);
        (applied, false)
    }
}
```

Check the exact paths of `account_txn_id_armed`, `stamp_account_txn_id` and `settle_expired` in the repo (`grep -rn "pub fn account_txn_id_armed\|pub fn stamp_account_txn_id\|pub fn settle_expired" crates/xrpl-ledger/src`) and adjust the `use` lines; `native_apply_one` shows the same three calls, so they are public. If `TxResult` lacks `PartialEq`, compare with `matches!(applied, TxResult::Expired)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib apply_on_sandbox_tests 2>&1 | tail -8`
Expected: `ok. 4 passed`. If the fourth test's tec code surprises you (e.g. the reserve rule fires first as `tecUNFUNDED_PAYMENT` vs `tecINSUFFICIENT_RESERVE`), the assertion only requires a `tec` prefix — do not change the engine.

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-ledger/src/tx/dispatch.rs
git commit -m "feat(xrpl-ledger): apply_on_sandbox — the tx pipeline on an existing sandbox with rollback"
```

---

### Task 4: `BatchTransactor` — preflight, preclaim, the inner loop

**Files:**
- Create: `crates/xrpl-ledger/src/tx/batch.rs`
- Modify: `crates/xrpl-ledger/src/tx/mod.rs` (add `pub mod batch;`), `crates/xrpl-ledger/src/tx/dispatch.rs:70` (register `"Batch" => Some(Box::new(crate::tx::batch::BatchTransactor)),` next to `"Payment"`)
- Test: unit tests inside `batch.rs`

**Interfaces:**
- Consumes: `apply_on_sandbox` (Task 3), `TxFields::from_json` + `inner_batch` (Task 1), the new `TxResult` variants (Task 1), `Sandbox::{snapshot, restore_snapshot}`.
- Produces: `pub struct BatchTransactor;` implementing `Transactor`; `pub fn batch_base_fee(outer: &serde_json::Value, base_fee_drops: u64) -> u64`; `pub fn take_inner_results() -> Vec<String>` (thread-local: the per-inner TER strings of the last `do_apply` on this thread, in RawTransactions order, drained on read); `pub const TF_ALL_OR_NOTHING: u64 = 0x0001_0000; TF_ONLY_ONE = 0x0002_0000; TF_UNTIL_FAILURE = 0x0004_0000; TF_INDEPENDENT = 0x0008_0000; TF_INNER_BATCH_TXN = 0x4000_0000; MAX_BATCH_TX_COUNT: usize = 8`.

The rules, from the spec's "rippled semantics" (Batch.cpp preflight/preclaim, apply.cpp `applyBatchTransactions`):

1. Outer flags: exactly one of the four mode bits, else `temINVALID_FLAG`; `tfInnerBatchTxn` set on the outer → `temINVALID_FLAG`.
2. `RawTransactions`: absent or empty → `temARRAY_EMPTY`; more than 8 → `temARRAY_TOO_LARGE`. Each element is `{"RawTransaction": {…inner tx json…}}`.
3. Each inner: `TransactionType` must not be `Batch` and must not be a pseudo type (`is_pseudo`) → `temINVALID`; `Flags` must include `tfInnerBatchTxn` → else `temINVALID_INNER_BATCH`; `Fee` must be `"0"` → else `temBAD_FEE`; `SigningPubKey` must be absent or `""`, and `TxnSignature` / `Signers` absent → else `temBAD_SIGNATURE`; exactly one of `Sequence` (non-zero) and `TicketSequence` → else `temSEQ_AND_TICKET`; the inner must parse with `TxFields::from_json` → else `temMALFORMED`.
4. Duplicates: two inners with identical JSON → `temREDUNDANT`. Under `tfAllOrNothing` or `tfUntilFailure`, two inners with the same `(Account, Sequence-or-TicketSequence)` → `temREDUNDANT`.
5. `BatchSigners` (optional array of `{"BatchSigner": {"Account": …, …}}`): more than 8 → `temARRAY_TOO_LARGE`; must be strictly ascending by the 20-byte account and unique → else `temBAD_SIGNER`; no signer may be the outer `Account` → `temBAD_SIGNER`; the set of signer accounts must equal the set of inner `Account`s that differ from the outer account → else `temBAD_SIGNER`. (Signature verification is not modelled — the engine does not verify signatures anywhere today; say so in the module doc.)
6. Fee: `outer.Fee >= batch_base_fee(outer, 10)` else `temBAD_FEE`, where `batch_base_fee = base × (1 + signer_count) + base` for the outer's own two units (rippled: `base + calculateBaseFee(outer) + Σ calculateBaseFee(inner) + base × signerCount`; every ordinary inner's `calculateBaseFee` is one base unit, so with `n` inners: `base × (2 + n + signers)`). Reference base fee is 10 drops.
7. `do_apply` (the outer's own `doApply` is empty; fee and sequence are already charged by `apply_common`): quote and follow rippled —

```text
for each inner in RawTransactions order:
    per-tx view over the batch view; (ret) = apply(inner, tapBATCH)
    if applied (tes or tec-claim): fold into the batch view
    if !tes:
        if AllOrNothing: return false      // caller discards the whole batch view
        if UntilFailure: break
    else if OnlyOne: break
return applied != 0                          // caller folds the batch view only if true
```

Our mapping: the "batch view" is `sandbox` after `apply_common`; `batch_snap = sandbox.snapshot()` at `do_apply` entry; each inner runs through `apply_on_sandbox` (which already rolls back a non-applied inner); on `AllOrNothing` failure `sandbox.restore_snapshot(batch_snap)`. `do_apply` returns `TxResult::Success` in every case (the outer itself is `tesSUCCESS`; only its inners' folding differs).

- [ ] **Step 1: Write the failing tests**

Create `crates/xrpl-ledger/src/tx/batch.rs` with the tests first (the implementation goes above them in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet;
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use crate::ledger::transactor::{apply_common, Transactor, TxFields};
    use serde_json::{json, Value};
    use xrpl_core::types::Hash256;

    fn acct(n: u8) -> [u8; 20] { let mut a = [0u8; 20]; a[19] = n; a }
    fn hexa(n: u8) -> String { hex::encode(acct(n)) }

    fn inner_payment(from: u8, to: u8, drops: u64, seq: u32) -> Value {
        json!({ "RawTransaction": {
            "TransactionType": "Payment", "Account": hexa(from), "Destination": hexa(to),
            "Amount": drops.to_string(), "Fee": "0", "Sequence": seq, "Flags": 0x4000_0000u64,
            "SigningPubKey": "" } })
    }

    fn outer(mode: u64, inners: Vec<Value>, signers: Option<Vec<u8>>) -> Value {
        let mut o = json!({
            "TransactionType": "Batch", "Account": hexa(1), "Fee": "1000", "Sequence": 5,
            "Flags": mode, "RawTransactions": inners,
        });
        if let Some(s) = signers {
            o["BatchSigners"] = Value::Array(s.into_iter().map(|n| json!({"BatchSigner": {"Account": hexa(n)}})).collect());
        }
        o
    }

    fn pf(o: &Value) -> String {
        BatchTransactor.preflight(&TxFields::from_json(o).expect("fields")).code_str().to_string()
    }

    #[test]
    fn preflight_accepts_a_well_formed_until_failure_batch() {
        let o = outer(TF_UNTIL_FAILURE, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 7)], None);
        assert_eq!(pf(&o), "tesSUCCESS");
    }

    #[test]
    fn preflight_rejects_zero_or_two_mode_flags_and_the_inner_flag_on_the_outer() {
        assert_eq!(pf(&outer(0, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 7)], None)), "temINVALID_FLAG");
        assert_eq!(pf(&outer(TF_ONLY_ONE | TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 7)], None)), "temINVALID_FLAG");
        assert_eq!(pf(&outer(TF_INDEPENDENT | TF_INNER_BATCH_TXN, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 7)], None)), "temINVALID_FLAG");
    }

    #[test]
    fn preflight_rejects_empty_and_oversized_inner_arrays() {
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![], None)), "temARRAY_EMPTY");
        let nine: Vec<Value> = (0..9).map(|i| inner_payment(1, 2, 1, 10 + i)).collect();
        assert_eq!(pf(&outer(TF_INDEPENDENT, nine, None)), "temARRAY_TOO_LARGE");
    }

    #[test]
    fn preflight_checks_each_inner_s_field_rules() {
        let mut bad_flag = inner_payment(1, 2, 1, 6); bad_flag["RawTransaction"]["Flags"] = json!(0);
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![bad_flag, inner_payment(1, 2, 1, 7)], None)), "temINVALID_INNER_BATCH");
        let mut bad_fee = inner_payment(1, 2, 1, 6); bad_fee["RawTransaction"]["Fee"] = json!("10");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![bad_fee, inner_payment(1, 2, 1, 7)], None)), "temBAD_FEE");
        let mut signed = inner_payment(1, 2, 1, 6); signed["RawTransaction"]["TxnSignature"] = json!("3045");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![signed, inner_payment(1, 2, 1, 7)], None)), "temBAD_SIGNATURE");
        let mut both = inner_payment(1, 2, 1, 6); both["RawTransaction"]["TicketSequence"] = json!(9);
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![both, inner_payment(1, 2, 1, 7)], None)), "temSEQ_AND_TICKET");
        let mut nested = inner_payment(1, 2, 1, 6); nested["RawTransaction"]["TransactionType"] = json!("Batch");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![nested, inner_payment(1, 2, 1, 7)], None)), "temINVALID");
    }

    #[test]
    fn preflight_rejects_duplicates_by_mode() {
        let dup = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 6)], None);
        assert_eq!(pf(&dup), "temREDUNDANT", "identical inners are redundant in every mode");
        let same_seq = outer(TF_UNTIL_FAILURE, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 2, 6)], None);
        assert_eq!(pf(&same_seq), "temREDUNDANT", "same (account, sequence) under UntilFailure");
        let same_seq_ok = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 2, 6)], None);
        assert_eq!(pf(&same_seq_ok), "tesSUCCESS", "Independent tolerates a shared sequence (the second fails at apply)");
    }

    #[test]
    fn preflight_checks_the_signer_set_against_the_inner_accounts() {
        // inner from account 3 ≠ outer account 1 → a signer for 3 is required
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(3, 2, 1, 1)], None)), "temBAD_SIGNER");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(3, 2, 1, 1)], Some(vec![3]))), "tesSUCCESS");
        // a signer that is the outer account, a spurious signer, an unsorted pair
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(3, 2, 1, 1)], Some(vec![1, 3]))), "temBAD_SIGNER");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(3, 2, 1, 1)], Some(vec![3, 4]))), "temBAD_SIGNER");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![inner_payment(3, 2, 1, 1), inner_payment(4, 2, 1, 1)], Some(vec![4, 3]))), "temBAD_SIGNER");
    }

    #[test]
    fn batch_base_fee_counts_two_units_plus_one_per_inner_and_per_signer() {
        let o = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(3, 2, 1, 1)], Some(vec![3]));
        assert_eq!(batch_base_fee(&o, 10), 10 * (2 + 2 + 1));
        let mut cheap = o.clone(); cheap["Fee"] = json!("40");
        assert_eq!(pf(&cheap), "temBAD_FEE");
    }

    // ---- do_apply ----

    fn state_with_accounts(accts: &[(u8, u64, u32)]) -> LedgerState {
        let header = LedgerHeader {
            sequence: 1, total_coins: 100_000_000_000_000_000, parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
            parent_close_time: 0, close_time: 10, close_time_resolution: 10, close_flags: 0,
        };
        let mut state = LedgerState::new_unverified(header);
        for (n, balance, seq) in accts {
            let id = acct(*n);
            let j = json!({"LedgerEntryType": "AccountRoot", "Account": hex::encode(id),
                            "Balance": balance.to_string(), "Sequence": seq, "OwnerCount": 0, "Flags": 0});
            state.state_map.insert(keylet::account_root_key(&id), serde_json::to_vec(&j).unwrap()).unwrap();
        }
        state
    }

    fn balance_seq(sb: &Sandbox, n: u8) -> (u64, u32) {
        let v: Value = serde_json::from_slice(&sb.read(&keylet::account_root_key(&acct(n))).unwrap()).unwrap();
        (v["Balance"].as_str().unwrap().parse().unwrap(), v["Sequence"].as_u64().unwrap() as u32)
    }

    fn run(o: &Value, state: &LedgerState) -> (Sandbox<'_>, String, Vec<String>) {
        let f = TxFields::from_json(o).expect("fields");
        let mut sb = Sandbox::new(state);
        assert!(BatchTransactor.preflight(&f).is_success());
        assert!(apply_common(&f, &mut sb).is_success());
        let r = BatchTransactor.do_apply(&f, &mut sb).code_str().to_string();
        (sb, r, take_inner_results())
    }

    #[test]
    fn until_failure_applies_both_inners_and_the_outer_fee_and_sequence() {
        let state = state_with_accounts(&[(1, 50_000_000, 5), (2, 20_000_000, 1)]);
        let o = outer(TF_UNTIL_FAILURE, vec![inner_payment(1, 2, 1_000_000, 6), inner_payment(1, 2, 2_000_000, 7)], None);
        let (sb, r, inners) = run(&o, &state);
        assert_eq!(r, "tesSUCCESS");
        assert_eq!(inners, vec!["tesSUCCESS", "tesSUCCESS"]);
        assert_eq!(balance_seq(&sb, 1), (50_000_000 - 1000 - 3_000_000, 8), "outer fee + seq, both inners' seqs");
        assert_eq!(balance_seq(&sb, 2), (23_000_000, 1));
    }

    #[test]
    fn until_failure_stops_at_the_first_failure_and_keeps_the_earlier_inner() {
        let state = state_with_accounts(&[(1, 50_000_000, 5), (2, 20_000_000, 1)]);
        // second inner has a stale sequence (6 again) → not applied; third never runs
        let o = outer(TF_UNTIL_FAILURE, vec![inner_payment(1, 2, 1_000_000, 6), inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 8)], None);
        // (account,seq) duplicates are temREDUNDANT under UntilFailure, so use Independent-legal shape: bump to seq 99
        let mut o = o; o["RawTransactions"][1]["RawTransaction"]["Sequence"] = json!(99);
        let (sb, r, inners) = run(&o, &state);
        assert_eq!(r, "tesSUCCESS");
        assert_eq!(inners.len(), 2, "the loop broke after the failing second inner");
        assert_eq!(inners[0], "tesSUCCESS");
        assert!(!inners[1].starts_with("tes"), "{}", inners[1]);
        assert_eq!(balance_seq(&sb, 1), (50_000_000 - 1000 - 1_000_000, 7));
        assert_eq!(balance_seq(&sb, 2), (21_000_000, 1));
    }

    #[test]
    fn all_or_nothing_discards_every_inner_on_a_failure_but_keeps_the_outer_charge() {
        let state = state_with_accounts(&[(1, 50_000_000, 5), (2, 20_000_000, 1)]);
        let mut o = outer(TF_ALL_OR_NOTHING, vec![inner_payment(1, 2, 1_000_000, 6), inner_payment(1, 2, 1, 7)], None);
        o["RawTransactions"][1]["RawTransaction"]["Sequence"] = json!(99);
        let (sb, r, inners) = run(&o, &state);
        assert_eq!(r, "tesSUCCESS", "the outer itself succeeds");
        assert_eq!(inners.len(), 2);
        assert_eq!(balance_seq(&sb, 1), (50_000_000 - 1000, 6), "only the outer's fee and sequence remain");
        assert_eq!(balance_seq(&sb, 2), (20_000_000, 1));
    }

    #[test]
    fn only_one_stops_after_the_first_success() {
        let state = state_with_accounts(&[(1, 50_000_000, 5), (2, 20_000_000, 1)]);
        let mut o = outer(TF_ONLY_ONE, vec![inner_payment(1, 2, 1, 7), inner_payment(1, 2, 1_000_000, 6), inner_payment(1, 2, 5, 7)], None);
        o["RawTransactions"][0]["RawTransaction"]["Sequence"] = json!(99); // first fails (bad seq), second succeeds, third never runs
        let (sb, r, inners) = run(&o, &state);
        assert_eq!(r, "tesSUCCESS");
        assert_eq!(inners.len(), 2);
        assert_eq!(balance_seq(&sb, 1), (50_000_000 - 1000 - 1_000_000, 7));
    }

    #[test]
    fn independent_runs_every_inner_and_folds_each_applied_one() {
        let state = state_with_accounts(&[(1, 50_000_000, 5), (2, 20_000_000, 1)]);
        let mut o = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1_000_000, 6), inner_payment(1, 2, 1, 7), inner_payment(1, 2, 2_000_000, 7)], None);
        o["RawTransactions"][1]["RawTransaction"]["Sequence"] = json!(99); // fails, the others succeed
        let (sb, r, inners) = run(&o, &state);
        assert_eq!(r, "tesSUCCESS");
        assert_eq!(inners.len(), 3);
        assert_eq!(balance_seq(&sb, 1), (50_000_000 - 1000 - 3_000_000, 8));
        assert_eq!(balance_seq(&sb, 2), (23_000_000, 1));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add `pub mod batch;` to `crates/xrpl-ledger/src/tx/mod.rs` (alphabetically among the others), then:
Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib tx::batch 2>&1 | tail -6`
Expected: compile errors — `BatchTransactor`, the `TF_*` constants, `batch_base_fee`, `take_inner_results` do not exist.

- [ ] **Step 3: Implement the transactor**

Above the tests in `batch.rs`:

```rust
//! rippled `Batch` (BatchV1_1 — `Batch.cpp`, `apply.cpp
//! applyBatchTransactions`): one outer transaction carrying 2..8 inner
//! transactions applied inside it under one of four modes. The outer's own
//! `doApply` is empty (fee and sequence are the common changes); the inners
//! run on a view over the batch view and fold in by result.
//!
//! Not modelled: BatchSigners signature verification. The engine verifies no
//! signatures today (validated ledgers carry only verified transactions);
//! the structural checks on the signer set are enforced.
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::tx::dispatch::{apply_on_sandbox, is_pseudo};
use serde_json::Value;
use std::cell::RefCell;

pub const TF_ALL_OR_NOTHING: u64 = 0x0001_0000;
pub const TF_ONLY_ONE: u64 = 0x0002_0000;
pub const TF_UNTIL_FAILURE: u64 = 0x0004_0000;
pub const TF_INDEPENDENT: u64 = 0x0008_0000;
pub const TF_INNER_BATCH_TXN: u64 = 0x4000_0000;
pub const MAX_BATCH_TX_COUNT: usize = 8;
const MODE_MASK: u64 = TF_ALL_OR_NOTHING | TF_ONLY_ONE | TF_UNTIL_FAILURE | TF_INDEPENDENT;

thread_local! {
    static INNER_RESULTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The per-inner results of the last `do_apply` on this thread, in
/// RawTransactions order, drained on read. The shadow reports them beside
/// the inner entries' recorded results.
pub fn take_inner_results() -> Vec<String> {
    INNER_RESULTS.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

fn flags_of(v: &Value) -> u64 {
    v.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0)
}

fn inner_jsons(outer: &Value) -> Vec<&Value> {
    outer
        .get("RawTransactions")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|e| e.get("RawTransaction")).collect())
        .unwrap_or_default()
}

fn signer_accounts(outer: &Value) -> Option<Vec<[u8; 20]>> {
    let arr = outer.get("BatchSigners")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        let acct = e.get("BatchSigner").and_then(|s| s.get("Account")).and_then(|a| a.as_str())?;
        out.push(decode_account(acct)?);
    }
    Some(out)
}

/// The engine's account dialect is 40 hex chars; `TxFields::from_json`
/// decodes the same way — keep the two in step.
fn decode_account(s: &str) -> Option<[u8; 20]> {
    let b = hex::decode(s).ok()?;
    <[u8; 20]>::try_from(b.as_slice()).ok()
}

/// `base + calculateBaseFee(outer) + Σ calculateBaseFee(inner) + base × signers`
/// with every ordinary transaction's base fee one unit: `base × (2 + n + s)`.
pub fn batch_base_fee(outer: &Value, base_fee_drops: u64) -> u64 {
    let n = inner_jsons(outer).len() as u64;
    let s = signer_accounts(outer).map(|v| v.len() as u64).unwrap_or(0);
    base_fee_drops.saturating_mul(2 + n + s)
}

pub struct BatchTransactor;

impl BatchTransactor {
    fn preflight_inner(inner: &Value) -> TxResult {
        let ty = inner.get("TransactionType").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "Batch" || is_pseudo(ty) {
            return TxResult::InvalidTx;
        }
        if flags_of(inner) & TF_INNER_BATCH_TXN == 0 {
            return TxResult::InvalidInnerBatch;
        }
        if inner.get("Fee").and_then(|f| f.as_str()) != Some("0") {
            return TxResult::BadFee;
        }
        let spk_empty = inner.get("SigningPubKey").map(|k| k.as_str() == Some("")).unwrap_or(true);
        if !spk_empty || inner.get("TxnSignature").is_some() || inner.get("Signers").is_some() {
            return TxResult::BadSignature;
        }
        let has_seq = inner.get("Sequence").and_then(|s| s.as_u64()).map(|s| s != 0).unwrap_or(false);
        let has_ticket = inner.get("TicketSequence").is_some();
        if has_seq == has_ticket {
            return TxResult::SeqAndTicket;
        }
        if TxFields::from_json(inner).is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
}

impl Transactor for BatchTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "Batch" {
            return TxResult::Malformed;
        }
        let flags = flags_of(&tx.fields);
        if (flags & MODE_MASK).count_ones() != 1 || flags & TF_INNER_BATCH_TXN != 0 {
            return TxResult::InvalidFlag;
        }
        let inners = inner_jsons(&tx.fields);
        if inners.is_empty() {
            return TxResult::ArrayEmpty;
        }
        if inners.len() > MAX_BATCH_TX_COUNT {
            return TxResult::TemArrayTooLarge;
        }
        let mut seen_json: Vec<&Value> = Vec::with_capacity(inners.len());
        let mut seen_seq: Vec<([u8; 20], u64)> = Vec::with_capacity(inners.len());
        let seq_unique = flags & (TF_ALL_OR_NOTHING | TF_UNTIL_FAILURE) != 0;
        let mut inner_accounts: Vec<[u8; 20]> = Vec::new();
        for inner in &inners {
            let r = Self::preflight_inner(inner);
            if !r.is_success() {
                return r;
            }
            if seen_json.iter().any(|s| *s == *inner) {
                return TxResult::Redundant;
            }
            seen_json.push(inner);
            let Some(acct) = inner.get("Account").and_then(|a| a.as_str()).and_then(decode_account) else {
                return TxResult::Malformed;
            };
            let seq_or_ticket = inner
                .get("Sequence").and_then(|s| s.as_u64()).filter(|s| *s != 0)
                .or_else(|| inner.get("TicketSequence").and_then(|s| s.as_u64()))
                .unwrap_or(0);
            if seq_unique && seen_seq.contains(&(acct, seq_or_ticket)) {
                return TxResult::Redundant;
            }
            seen_seq.push((acct, seq_or_ticket));
            if acct != tx.account && !inner_accounts.contains(&acct) {
                inner_accounts.push(acct);
            }
        }
        // BatchSigners: sorted, unique, none the outer account, exactly the
        // inner accounts that differ from the outer account.
        let signers = match tx.fields.get("BatchSigners") {
            None => Vec::new(),
            Some(_) => match signer_accounts(&tx.fields) {
                Some(s) => s,
                None => return TxResult::BadSigner,
            },
        };
        if signers.len() > MAX_BATCH_TX_COUNT {
            return TxResult::TemArrayTooLarge;
        }
        if signers.windows(2).any(|w| w[0] >= w[1]) {
            return TxResult::BadSigner;
        }
        if signers.iter().any(|s| *s == tx.account) {
            return TxResult::BadSigner;
        }
        let mut required = inner_accounts.clone();
        required.sort();
        if signers != required {
            return TxResult::BadSigner;
        }
        if tx.fee < batch_base_fee(&tx.fields, 10) {
            return TxResult::BadFee;
        }
        TxResult::Success
    }

    fn preclaim(&self, _tx: &TxFields, _sandbox: &Sandbox) -> TxResult {
        TxResult::Success
    }

    /// `applyBatchTransactions` (apply.cpp), quoted:
    ///
    /// ```text
    /// for (STObject rb : batchTxn.getFieldArray(sfRawTransactions)) {
    ///     auto const result = applyOneTransaction(STTx{std::move(rb)});
    ///     //   perTxBatchView over batchView; apply(…, tapBATCH);
    ///     //   if (ret.applied && (tes || tecClaim)) perTxBatchView.apply(batchView);
    ///     if (result.applied) ++applied;
    ///     if (!isTesSuccess(result.ter)) {
    ///         if (mode & tfAllOrNothing) return false;   // caller discards batchView
    ///         if (mode & tfUntilFailure) break;
    ///     } else if (mode & tfOnlyOne) break;
    /// }
    /// return applied != 0;                               // caller folds batchView
    /// ```
    ///
    /// `sandbox` here is the batch view (the common changes are already in
    /// it); `apply_on_sandbox` is the per-transaction view with its own
    /// rollback; AllOrNothing's `return false` is a restore to the entry
    /// snapshot. The outer's own result is always tesSUCCESS.
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let flags = flags_of(&tx.fields);
        let batch_snap = sandbox.snapshot();
        let mut results: Vec<String> = Vec::new();
        for inner in inner_jsons(&tx.fields) {
            let Some(mut f) = TxFields::from_json(inner) else {
                results.push(TxResult::Malformed.code_str().to_string());
                if flags & TF_ALL_OR_NOTHING != 0 {
                    sandbox.restore_snapshot(batch_snap);
                    break;
                }
                if flags & TF_UNTIL_FAILURE != 0 {
                    break;
                }
                continue;
            };
            f.inner_batch = true;
            let (r, _applied) = apply_on_sandbox(&f, sandbox);
            results.push(r.code_str().to_string());
            if !r.is_success() {
                if flags & TF_ALL_OR_NOTHING != 0 {
                    sandbox.restore_snapshot(batch_snap);
                    break;
                }
                if flags & TF_UNTIL_FAILURE != 0 {
                    break;
                }
            } else if flags & TF_ONLY_ONE != 0 {
                break;
            }
        }
        INNER_RESULTS.with(|r| *r.borrow_mut() = results);
        TxResult::Success
    }
}
```

Notes for the implementer:
- `is_pseudo` must be `pub` in `dispatch.rs` (it is called from `native_apply.rs` already, so it is).
- `Sandbox::snapshot()` returns a `HashMap<Hash256, SandboxEntry>`; `restore_snapshot` takes it by value — clone `batch_snap` before the loop if the borrow checker asks (`sandbox.restore_snapshot(batch_snap.clone())`).
- If `Value` comparison `*s == *inner` complains about references, compare `s == inner`.
- If the `Sandbox` lifetime in the test helper `run` needs naming (`Sandbox<'_>`), follow the compiler.

Register the type in `dispatch.rs`'s `get_transactor` match, next to `"Payment"`:

```rust
        "Batch" => Some(Box::new(crate::tx::batch::BatchTransactor)),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-ledger --lib tx::batch 2>&1 | tail -16`
Expected: `ok. 12 passed`. If `until_failure_stops_at_the_first_failure…` fails because the engine's stale-sequence outcome is a claimed `tec` (applied, sequence consumed) rather than a `tef`/`tem` — read the actual code printed by the assertion; a `tec` still breaks the loop under UntilFailure (`!isTesSuccess`), and the expected balance/sequence in that test must then be recomputed (sequence 8, balance unchanged by the failed inner). Adjust the test to the engine's real code path, never the loop.

- [ ] **Step 5: Clippy, then commit**

Run: `CARGO_TARGET_DIR=$T cargo clippy -p xrpl-ledger -- -D warnings 2>&1 | tail -3`
Expected: no warnings.

```bash
git add crates/xrpl-ledger/src/tx/batch.rs crates/xrpl-ledger/src/tx/mod.rs crates/xrpl-ledger/src/tx/dispatch.rs
git commit -m "feat(xrpl-ledger): BatchTransactor — preflight, signer set, fee formula, and the four-mode inner loop"
```

(This is the 4th commit on the branch — push it before Task 5's commit: `git push -u origin t0-batch-b`.)

---

### Task 5: The native shadow skips inner entries and attributes them to their outer

**Files:**
- Modify: `crates/xrpl-node/src/native_apply.rs` (add `batch_attribution`)
- Modify: `crates/xrpl-node/src/native_shadow.rs:535-575` (the `for tx in &ordered` loop in `on_ledger`: skip inners; iterate the outer's and its inners' `AffectedNodes`; report inner TERs)
- Test: unit tests in `native_apply.rs` for `batch_attribution`; the shadow wiring is exercised by Task 7's vector and the devnet fixture through `parity_probe`'s native leg.

**Interfaces:**
- Consumes: `xrpl_ledger::tx::batch::{take_inner_results, TF_INNER_BATCH_TXN}`.
- Produces: `pub struct BatchAttribution { pub skip: HashSet<String>, pub inners_of: HashMap<String, Vec<String>> }` and `pub fn batch_attribution(ordered: &[&Value]) -> BatchAttribution` — `skip` holds the hashes (upper-case hex) of every entry carrying `tfInnerBatchTxn` with a `metaData.ParentBatchID`; `inners_of[outer_hash]` lists those inner hashes in `TransactionIndex` order.

Leg A's fold for the FFI leg (`ffi_engine.rs:2279-2312`) keys inners by parsing the outer's blob; the native shadow has the decoded JSON, so `ParentBatchID` on the inner's metadata is the link.

- [ ] **Step 1: Write the failing tests**

Append to `native_apply.rs`:

```rust
#[cfg(test)]
mod batch_attribution_tests {
    use super::*;
    use serde_json::json;

    fn entry(hash: &str, idx: u64, flags: u64, parent: Option<&str>) -> Value {
        let mut meta = json!({"TransactionIndex": idx, "TransactionResult": "tesSUCCESS", "AffectedNodes": []});
        if let Some(p) = parent { meta["ParentBatchID"] = json!(p); }
        json!({"hash": hash, "TransactionType": "Payment", "Flags": flags, "metaData": meta})
    }

    #[test]
    fn inner_entries_are_skipped_and_grouped_under_their_outer_in_index_order() {
        let o = json!({"hash": "AA", "TransactionType": "Batch", "Flags": 0x0004_0000u64,
                       "metaData": {"TransactionIndex": 1, "TransactionResult": "tesSUCCESS", "AffectedNodes": []}});
        let i2 = entry("CC", 3, 0x4000_0000, Some("AA"));
        let i1 = entry("BB", 2, 0x4000_0000, Some("AA"));
        let other = entry("DD", 4, 0, None);
        let ordered: Vec<&Value> = vec![&o, &i1, &i2, &other];
        let a = batch_attribution(&ordered);
        assert_eq!(a.skip.len(), 2);
        assert!(a.skip.contains("BB") && a.skip.contains("CC"));
        assert_eq!(a.inners_of.get("AA").cloned().unwrap_or_default(), vec!["BB".to_string(), "CC".to_string()]);
        assert!(!a.skip.contains("DD"));
    }

    #[test]
    fn an_inner_flag_without_a_parent_is_not_an_inner() {
        let lone = entry("EE", 1, 0x4000_0000, None);
        let ordered: Vec<&Value> = vec![&lone];
        let a = batch_attribution(&ordered);
        assert!(a.skip.is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-node --lib batch_attribution 2>&1 | tail -6`
Expected: compile error — `batch_attribution` not found.

- [ ] **Step 3: Implement `batch_attribution` and wire the shadow**

In `native_apply.rs`:

```rust
use std::collections::{HashMap, HashSet};

/// Batch (BatchV1_1): rippled records every inner transaction as its own
/// ledger entry — own hash, own metadata carrying `ParentBatchID`, the
/// indices right after its outer — and applies it INSIDE the outer's
/// application (`applyBatchTransactions`). The native engine's
/// `BatchTransactor::do_apply` does the same, so inner entries are skipped
/// in the replay and their metadata is attributed to the outer.
pub struct BatchAttribution {
    pub skip: HashSet<String>,
    pub inners_of: HashMap<String, Vec<String>>,
}

pub fn batch_attribution(ordered: &[&Value]) -> BatchAttribution {
    let mut skip = HashSet::new();
    let mut inners_of: HashMap<String, Vec<String>> = HashMap::new();
    for tx in ordered {
        let flags = tx.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let parent = tx["metaData"].get("ParentBatchID").and_then(|p| p.as_str());
        if flags & xrpl_ledger::tx::batch::TF_INNER_BATCH_TXN == 0 {
            continue;
        }
        let Some(parent) = parent else { continue };
        let hash = tx["hash"].as_str().unwrap_or("").to_uppercase();
        skip.insert(hash.clone());
        inners_of.entry(parent.to_uppercase()).or_default().push(hash);
    }
    BatchAttribution { skip, inners_of }
}
```

(`ordered` is already sorted by `TransactionIndex` by the caller, so the `Vec` order is the inners' order.)

In `native_shadow.rs::on_ledger`, right after `ordered.sort_by_key(...)` (line ~509) add:

```rust
        let attribution = crate::native_apply::batch_attribution(&ordered);
        let by_hash: HashMap<String, &Value> = ordered
            .iter()
            .map(|t| (t["hash"].as_str().unwrap_or("").to_uppercase(), *t))
            .collect();
```

At the top of the `for tx in &ordered {` loop body (line ~535), before `build_txfields`:

```rust
            let this_hash = tx["hash"].as_str().unwrap_or("").to_uppercase();
            if attribution.skip.contains(&this_hash) {
                continue; // applied inside its outer Batch
            }
```

Where the loop iterates the expected metadata — `for node in tx["metaData"]["AffectedNodes"].as_array().into_iter().flatten()` (line ~561) — change it to iterate the outer's nodes followed by each inner's nodes, in order:

```rust
                let mut node_sources: Vec<&Value> = vec![*tx];
                if let Some(inners) = attribution.inners_of.get(&this_hash) {
                    for ih in inners {
                        if let Some(it) = by_hash.get(ih) {
                            node_sources.push(*it);
                        }
                    }
                }
                for src in &node_sources {
                for node in src["metaData"]["AffectedNodes"].as_array().into_iter().flatten() {
                    // …existing per-node compare body unchanged…
                }
                }
```

Keep every line of the existing per-node body as it is; only the enclosing iteration changes. Where a key appears in more than one source's nodes (an inner touching what the outer or an earlier inner touched), the LAST occurrence's `FinalFields` is the post-batch image and must win: if the existing body compares each node immediately, first collect `(key → node)` into a `Vec` keyed map that later entries overwrite, then run the body over that map's values. Leg A does exactly this ("Created wins over Modified, Created-then-Deleted is no entry at all" — `ffi_engine.rs:2300-2312`); mirror those two rules.

After the TER compare of an outer whose `inners_of` entry exists, add the inner-TER compare:

```rust
            if let Some(inners) = attribution.inners_of.get(&this_hash) {
                let ours = xrpl_ledger::tx::batch::take_inner_results();
                for (i, ih) in inners.iter().enumerate() {
                    let want = by_hash
                        .get(ih)
                        .and_then(|it| it["metaData"]["TransactionResult"].as_str())
                        .unwrap_or("?");
                    let got = ours.get(i).map(String::as_str).unwrap_or("(not run)");
                    if got != want {
                        ter_mm.push(format!("{this_hash} BATCH-INNER[{i}] {ih}: our_ter={got} net_ter={want}"));
                    }
                }
            }
```

`ter_mm` is the existing `Vec<String>` the loop already appends TER mismatches to (declared just above the loop). Use the same receipt path the existing TER-mismatch branch uses; do not invent a new one.

- [ ] **Step 4: Run the tests and build the node**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-node --lib batch_attribution 2>&1 | tail -4`
Expected: `ok. 2 passed`
Run: `CARGO_TARGET_DIR=$T cargo build -p xrpl-node --bins --tests 2>&1 | grep -E "^error|^warning: unused|Finished"`
Expected: `Finished`, no errors.
Run: `CARGO_TARGET_DIR=$T cargo clippy -p xrpl-node -- -D warnings 2>&1 | tail -3`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-node/src/native_apply.rs crates/xrpl-node/src/native_shadow.rs
git commit -m "feat(xrpl-node): the native shadow skips Batch inner entries and attributes them to their outer"
```

---

### Task 6: Devnet fixtures and merged Batch bundles

**Files:**
- Create: `scripts/merge_batch_bundle.py`
- Create (data): `crates/xrpl-node/tests/vectors/batch_until_failure_two_payments_devnet_5309670.json` and one bundle per additional mode found in devnet ledger 5309584 (name them `batch_<mode>_<shape>_devnet_<seq>.json`)
- Test: the merge script's self-check (it exits non-zero when a key's post-image is missing).

**Interfaces:**
- Consumes: `scripts/fetch_ledger_fixture.py <seq> --rpc URL --outdir DIR`, `XRPL_RPC=<url> scripts/fetch_tx_bundle.py <hashprefix> <seq> <out.json> [targets…]`.
- Produces: bundle JSON with the same shape the vector harness reads (`seq`, `parent_close_time`, `total_coins`, `parent_hash`, `pre`, `tx`, `result`, `expect`) where `tx` is the OUTER Batch transaction and `expect` is the union of the outer's and inners' post-images.

- [ ] **Step 1: Fetch the devnet fixtures**

```bash
S=/tmp/claude-1000/-home-localai/d6509b21-22a5-467b-a51d-16eb8c7262a5/scratchpad
mkdir -p $S/fixtriage/devnet
python3 scripts/fetch_ledger_fixture.py 5309670 5309584 --rpc https://s.devnet.rippletest.net:51234 --outdir $S/fixtriage/devnet
ls -la $S/fixtriage/devnet/
```

Expected: `l5309670_blobs.txt`, `l5309670_expected.json`, `l5309584_blobs.txt`, `l5309584_expected.json`. Then list the Batch outers and their inners:

```bash
python3 - <<'PY'
import json
for seq in (5309670, 5309584):
    e=json.load(open(f'/tmp/claude-1000/-home-localai/d6509b21-22a5-467b-a51d-16eb8c7262a5/scratchpad/fixtriage/devnet/l{seq}_expected.json'))
    for h,t in e['tx_json'].items():
        if t.get('TransactionType')=='Batch':
            inners=[(ih,it['TransactionType'],it['metaData']['TransactionResult']) for ih,it in e['tx_json'].items() if it.get('metaData',{}).get('ParentBatchID','').upper()==h.upper()]
            print(seq, h[:12], 'flags=%#x'%t.get('Flags',0), t['metaData']['TransactionResult'], 'inners:', inners)
PY
```

Record the outer hash prefixes and modes printed; they name the bundles.

- [ ] **Step 2: Write the merge script**

`scripts/merge_batch_bundle.py`:

```python
#!/usr/bin/env python3
"""merge_batch_bundle.py <outer.json> <inner1.json> [<inner2.json> …] <out.json>

Merges the bundle of a Batch OUTER transaction with the bundles of its
INNER entries (each produced by fetch_tx_bundle.py on that entry's own
hash) into one bundle the vector harness can replay through the native
BatchTransactor:

  tx      = the outer's tx (it carries RawTransactions)
  result  = the outer's recorded TransactionResult
  pre     = union of every bundle's pre-images (identical keys must agree)
  expect  = union of every bundle's expectations; a key expected by several
            takes the image from the LAST inner (RawTransactions order =
            TransactionIndex order), which is the post-batch image.
Exits 2 if two pre-images disagree — that means the bundles were fetched
from different ledgers.
"""
import json, sys

def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    paths, out = sys.argv[1:-1], sys.argv[-1]
    outer = json.load(open(paths[0]))
    inners = [json.load(open(p)) for p in paths[1:]]
    merged = {k: outer[k] for k in ("seq", "parent_close_time", "total_coins", "parent_hash", "tx", "result")}
    pre, expect = dict(outer.get("pre", {})), dict(outer.get("expect", {}))
    for b in inners:
        if b["seq"] != outer["seq"]:
            sys.exit(f"inner from ledger {b['seq']} but outer from {outer['seq']}")
        for k, v in b.get("pre", {}).items():
            if k in pre and pre[k].strip().upper() != v.strip().upper():
                sys.exit(2)
            pre[k] = v
        for k, v in b.get("expect", {}).items():
            expect[k] = v
    merged["pre"], merged["expect"] = pre, expect
    merged["inner_hashes"] = [b["tx"]["hash"] for b in inners]
    json.dump(merged, open(out, "w"))
    print(f"{out}: pre={len(pre)} expect={len(expect)} inners={len(inners)}")

if __name__ == "__main__":
    main()
```

`chmod +x scripts/merge_batch_bundle.py`.

- [ ] **Step 3: Build the bundles**

For each Batch outer printed in Step 1 (example: outer `A1B2C3…` in 5309670 with inners `F835E19C…` and `C7A3E441…` — use the real hashes):

```bash
export XRPL_RPC=https://s.devnet.rippletest.net:51234/
python3 scripts/fetch_tx_bundle.py <OUTER12> 5309670 /tmp/bundles/b5309670_outer.json
python3 scripts/fetch_tx_bundle.py <INNER1_12> 5309670 /tmp/bundles/b5309670_i1.json
python3 scripts/fetch_tx_bundle.py <INNER2_12> 5309670 /tmp/bundles/b5309670_i2.json
python3 scripts/merge_batch_bundle.py /tmp/bundles/b5309670_outer.json /tmp/bundles/b5309670_i1.json /tmp/bundles/b5309670_i2.json \
   crates/xrpl-node/tests/vectors/batch_until_failure_two_payments_devnet_5309670.json
```

Expected: each `fetch_tx_bundle.py` line ends `self-check: own meta reproduces …` and `pre=… targets=…`; the merge prints its counts. If `fetch_tx_bundle.py` refuses an inner entry (it should not — an inner is an ordinary entry with its own meta), report the exact error rather than working around it.

Repeat for 5309584's Batch(es), naming each bundle by its mode (`0x10000` AllOrNothing, `0x20000` OnlyOne, `0x40000` UntilFailure, `0x80000` Independent) and shape.

- [ ] **Step 4: Commit the script and the vectors**

```bash
git add scripts/merge_batch_bundle.py crates/xrpl-node/tests/vectors/batch_*.json
git commit -m "test(xrpl-node): devnet Batch vectors — merged outer+inner bundles, and the merge script"
```

---

### Task 7: The `batch_vector` suite

**Files:**
- Create: `crates/xrpl-node/tests/batch_vector.rs`
- Modify: scratchpad `gate54.sh` — add `batch_vector` to the `V="…"` suite list (mkcycle derives every gate from it)

**Interfaces:**
- Consumes: the bundles from Task 6; `native_apply_one` (the outer runs its inners inside `BatchTransactor::do_apply`); `take_inner_results`.

- [ ] **Step 1: Write the failing test file**

Copy the harness verbatim from `crates/xrpl-node/tests/account_delete_vector.rs` lines 1-100 (`key32`, `hydrate`, `run_bundle` including its `expect` loop with the deletion pin and the untouched-pin rules) into `batch_vector.rs`, changing only the module doc to:

```rust
//! Byte-exact vector drills for Batch (BatchV1_1, 2026-09-17).
//!
//! Each vector replays one devnet Batch OUTER against the union of its and
//! its inners' pre-images and compares every touched object byte-for-byte
//! with the ledger; the inners are applied inside the outer by
//! `BatchTransactor::do_apply`, exactly as rippled's
//! `applyBatchTransactions` does. `inner_results` pins each inner's recorded
//! TransactionResult.
```

then append, after `run_bundle`:

```rust
fn run_batch_bundle(bundle_json: &str) {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
    let want_inner: Vec<String> = bundle["inner_results"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default();
    run_bundle(bundle_json);
    let ours = xrpl_ledger::tx::batch::take_inner_results();
    assert_eq!(ours, want_inner, "each inner's TransactionResult, in RawTransactions order");
}

#[test]
fn batch_until_failure_two_payments_devnet_5309670() {
    run_batch_bundle(include_str!("vectors/batch_until_failure_two_payments_devnet_5309670.json"));
}
```

plus one `#[test]` per additional bundle produced in Task 6, named after the file.

`inner_results` is not written by `merge_batch_bundle.py` yet: add it there — in `main()` after `merged["inner_hashes"] = …`, add `merged["inner_results"] = [b["result"] for b in inners]` (each inner bundle's `result` is that entry's recorded `TransactionResult`), re-run the merge for every bundle, and re-commit the vectors with the script change (`fix(scripts): merge_batch_bundle records inner_results`).

- [ ] **Step 2: Run the suite to verify it fails or passes for the right reason**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-node --test batch_vector 2>&1 | grep -E "^test |test result|must byte|assertion" | head`
Expected: with Tasks 1-5 in place the vectors should PASS. If a target fails to byte-match, drill it as any finding: replay with `PROBE_BUNDLE=<bundle> cargo test -p xrpl-node --test probe_bundle -- --nocapture` (the throwaway prober honours `DX_*` narration), compare with rippled's narration of the same devnet ledger through `parity_probe` on the drill tree (`XRPL_FFI_TRACE=1 XRPL_FFI_TRACE_TX=<8hex>`), and fix the engine — never the vector.

- [ ] **Step 3: Add the suite to the gate list**

In `/tmp/claude-1000/-home-localai/d6509b21-22a5-467b-a51d-16eb8c7262a5/scratchpad/gate54.sh`, append ` batch_vector` inside the `V="…"` string (alphabetical position after `amm_withdraw_vector`). Verify with `grep -c batch_vector $S/gate54.sh` → `1`.

- [ ] **Step 4: Run the full node test build and the neighbouring suites**

Run: `CARGO_TARGET_DIR=$T cargo test -p xrpl-node --test batch_vector --test payment_flow_vector --test offer_fill_vector 2>&1 | grep "test result"`
Expected: three `ok` lines (the Batch work must not move the 107 offer / 66 payment vectors).

- [ ] **Step 5: Commit**

```bash
git add crates/xrpl-node/tests/batch_vector.rs
git commit -m "test(xrpl-node): batch_vector — byte-exact devnet Batch vectors through the native transactor"
```

---

### Task 8: Full-ledger confirmation on the drill tree and the hand-off

**Files:**
- No source changes. Runs on m3060's drill tree (`~/xrpl-drill`), synced file-by-file (never `rsync --delete`).
- Modify: `docs/superpowers/specs/2026-09-14-batch-support-design.md` status line.

- [ ] **Step 1: Sync the branch's changed files to the drill tree and build `differential_probe`**

```bash
S=/tmp/claude-1000/-home-localai/d6509b21-22a5-467b-a51d-16eb8c7262a5/scratchpad; W=$S/wt244
SSH="sshpass -p 1234qwer ssh -o PreferredAuthentications=password -o StrictHostKeyChecking=no m3060@10.0.0.97"
for f in $(git -C $W diff --name-only main -- crates/); do
  sshpass -p 1234qwer scp -q -o PreferredAuthentications=password $W/$f m3060@10.0.0.97:~/xrpl-drill/$f
done
sshpass -p 1234qwer scp -q -o PreferredAuthentications=password $S/fixtriage/devnet/l5309670_* $S/fixtriage/devnet/l5309584_* m3060@10.0.0.97:/tmp/fixt/
$SSH 'cd ~/xrpl-drill; PATH=$HOME/.cargo/bin:$PATH CARGO_BUILD_JOBS=3 nice -n 15 cargo build --release --features ffi --bin differential_probe > /tmp/dp_build.log 2>&1; echo BUILD_EXIT=$?; ls -la target/release/differential_probe | awk "{print \$6, \$7, \$8}"'
```

Expected: `BUILD_EXIT=0` and a fresh mtime (verify the mtime — a stale binary has fooled this loop before).

- [ ] **Step 2: Run the arbiter on both devnet ledgers**

```bash
$SSH 'cd ~/xrpl-drill; for q in 5309670 5309584; do nice -n 15 timeout 2400 ./target/release/differential_probe /tmp/fixt/l${q}_blobs.txt /tmp/fixt/l${q}_expected.json --rpc https://s.devnet.rippletest.net:51234 > /tmp/dp_$q.log 2>&1; echo "$q EXIT=$?"; grep -a "SUMMARY:\|PROBE:\|DIVERGE-" /tmp/dp_$q.log | head -5; done'
```

Expected: `SUMMARY: N/N attempted txs MATCH` for each, with the Batch outers among the matches and no `DIVERGE-TER` on inner entries (they are skipped). Leg A's known devnet residue — divergences only on `VaultDeposit` / `LoanPay` hydration (devnet-only amendments) — is acceptable and must be listed as such. An `EXIT=3` with `PROBE: HYDRATION-FAILED` is a transient fetch, not a verdict: re-run.

- [ ] **Step 3: Update the spec status and record the hand-off**

Change the spec's `**Status:**` line to: `Leg A deployed (cycles 129/130). Leg B implemented on branch t0-batch-b (plan docs/superpowers/plans/2026-09-17-batch-leg-b-native-transactor.md): BatchTransactor + shadow attribution; devnet vectors byte-exact; dp <results from Step 2>. Awaiting the next deploy cycle.`

```bash
git add docs/superpowers/specs/2026-09-14-batch-support-design.md
git commit -m "docs: Batch spec status — leg B implemented and verified on devnet"
git push origin t0-batch-b
```

Then report to James: the branch, the vector count, the dp results, and that merging to main and a deploy cycle are the next (separate) decision — a deploy restarts the validator, which stays his call while a soak is running.

---

## Self-Review

**Spec coverage.** Leg B bullets: `tx/batch.rs` preflight (flags, counts, inner field rules, uniqueness) → Task 4; fee formula → Task 4 (`batch_base_fee`); BatchSigners structural checks → Task 4; `apply` running the inners on nested sandboxes with the four modes → Task 4 (`do_apply`) on Task 3's `apply_on_sandbox`; sequence/ticket consumption per inner → `apply_common` through Task 3, with Task 2 waiving the fee gate; native replay and shadow skipping inner entries and attributing the outer's mutation set against outer ∪ inner metas, receipts naming the outer with inner results → Task 5; vectors from the devnet fixtures, one per mode as they appear → Tasks 6-7; verification on the drill tree → Task 8. The spec's rippled-semantics list: outer flag exclusivity ✓, 2..8 inners unique by hash — we lack inner hashes natively and use whole-JSON equality (stated in Task 4 rule 4) ✓, disabled inner types ✓, inner field rules ✓, seq/ticket uniqueness by mode ✓, fee formula ✓, signer set ✓ (signature verification explicitly not modelled, as the spec allows), apply modes ✓. Gap accepted and documented: `tfInnerBatchTxn` on a STANDALONE entry (no parent) is never presented to a transactor natively — the shadow skips inner entries and the feed never hands a standalone inner to the engine — so the `temINVALID_INNER_BATCH` path exists only for inners missing the flag.

**Placeholder scan.** Task 1 Step 3 contains one deliberate move-instruction (`todo_move_body_here` — replaced by moving the existing 25-line function body, whose location is given to the line). Task 6 Step 3 uses `<OUTER12>`-style placeholders for hashes that Step 1 prints — they are inputs discovered at execution time, not unwritten design. No "add error handling"/"similar to Task N" remains.

**Type consistency.** `apply_on_sandbox(tx: &TxFields, sb: &mut Sandbox) -> (TxResult, bool)` is used with that signature in Task 4. `take_inner_results() -> Vec<String>` is used in Tasks 4, 5 and 7. `batch_attribution(&[&Value]) -> BatchAttribution { skip, inners_of }` is used in Task 5 as defined. `TxFields::from_json` / `inner_batch` / `fee_missing` are used as defined in Task 1. The `TxResult` variant names (`TemArrayTooLarge` distinct from the existing tec `ArrayTooLarge`; `InvalidTx` for `temINVALID`) are consistent across Tasks 1 and 4.
