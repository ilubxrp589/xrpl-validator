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

// Result codes below track rippled 3.3.0
// (libxrpl/tx/transactors/system/Batch.cpp), not the older FFI-vendored copy.
pub const TF_ALL_OR_NOTHING: u64 = 0x0001_0000;
pub const TF_ONLY_ONE: u64 = 0x0002_0000;
pub const TF_UNTIL_FAILURE: u64 = 0x0004_0000;
pub const TF_INDEPENDENT: u64 = 0x0008_0000;
pub const TF_INNER_BATCH_TXN: u64 = 0x4000_0000;
pub const MAX_BATCH_TX_COUNT: usize = 8;
/// `kMaxBatchSigners = kMaxBatchTxCount * 3` (Protocol.h) — the BatchSigners
/// cap is independent of, and larger than, the inner-transaction cap.
pub const MAX_BATCH_SIGNERS: usize = MAX_BATCH_TX_COUNT * 3;
const MODE_MASK: u64 = TF_ALL_OR_NOTHING | TF_ONLY_ONE | TF_UNTIL_FAILURE | TF_INDEPENDENT;

thread_local! {
    static INNER_RESULTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The per-inner results of the last `do_apply` on this thread, in
/// RawTransactions order, drained on read. Meaningful only immediately after
/// a `Batch` `do_apply` on this thread — `do_apply` clears this at entry, so
/// a batch that fails preflight/preclaim (and so never reaches `do_apply`)
/// never exposes a previous batch's results, and a second call here (with no
/// intervening `do_apply`) drains an empty `Vec`.
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

/// Transaction JSON from the feed and from the vector bundles carries
/// base58 r-addresses inside `RawTransactions` and `BatchSigners`:
/// `native_apply::hexify_addresses` is applied to ledger-state images
/// (pre-images hydrated from the store), not to the transaction itself, and
/// `TxFields::from_json` only rewrites the outer's own top-level account
/// fields — nested `RawTransaction`/`BatchSigner` objects are untouched in
/// `tx.fields`. So a `Batch`'s inner and `BatchSigner` accounts arrive in
/// either dialect and must decode both, exactly as `TxFields::from_json`
/// does for the outer's own fields: hex first, checksummed base58 fallback,
/// via the same `tx::offer::decode20` helper.
fn decode_account(s: &str) -> Option<[u8; 20]> {
    crate::tx::offer::decode20(s)
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
    /// Per-inner checks, in rippled 3.3.0's order (Batch.cpp preflight):
    /// nested `Batch` → `temINVALID`; a disabled/pseudo inner type → the
    /// same `temINVALID_INNER_BATCH` a failing inner preflight produces
    /// (rippled's `kDisabledTxTypes` check and its `xrpl::preflight(...,
    /// TapBatch, ...)` call both fold into that code — a pseudo type fails
    /// the inner's own `preflight0`, since `isPseudoTx(tx) &&
    /// tx.isFlag(tfInnerBatchTxn)` is `temINVALID_FLAG` there, which the
    /// outer then reports as `temINVALID_INNER_BATCH`); missing
    /// `tfInnerBatchTxn` → `temINVALID_FLAG`; `checkSignatureFields` split
    /// three ways (`TxnSignature` → `temBAD_SIGNATURE`, `Signers` →
    /// `temBAD_SIGNER`, non-empty `SigningPubKey` → `temBAD_REGKEY`); a
    /// non-zero `Fee` → `temBAD_FEE`; an inner that fails to parse (our
    /// stand-in for "fails its own preflight") → `temINVALID_INNER_BATCH`;
    /// both or neither of `Sequence`/`TicketSequence` → `temSEQ_AND_TICKET`.
    fn preflight_inner(inner: &Value) -> TxResult {
        let ty = inner.get("TransactionType").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "Batch" {
            return TxResult::InvalidTx;
        }
        if is_pseudo(ty) {
            return TxResult::InvalidInnerBatch;
        }
        if flags_of(inner) & TF_INNER_BATCH_TXN == 0 {
            return TxResult::InvalidFlag;
        }
        if inner.get("TxnSignature").is_some() {
            return TxResult::BadSignature;
        }
        if inner.get("Signers").is_some() {
            return TxResult::BadSigner;
        }
        let spk_empty = inner.get("SigningPubKey").map(|k| k.as_str() == Some("")).unwrap_or(true);
        if !spk_empty {
            return TxResult::BadRegKey;
        }
        if inner.get("Fee").and_then(|f| f.as_str()) != Some("0") {
            return TxResult::BadFee;
        }
        if TxFields::from_json(inner).is_none() {
            return TxResult::InvalidInnerBatch;
        }
        let has_seq = inner.get("Sequence").and_then(|s| s.as_u64()).map(|s| s != 0).unwrap_or(false);
        let has_ticket = inner.get("TicketSequence").is_some();
        if has_seq == has_ticket {
            return TxResult::SeqAndTicket;
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
        // `RawTransactions` must be present and, if so, every element must
        // be `{"RawTransaction": {…object…}}` — rippled fails
        // deserialization on anything else, before `Batch::preflight` ever
        // runs. Checked structurally *before* counting inners: silently
        // dropping a malformed element (as a naive filter_map would) could
        // let a too-large array slip under the inner-count cap.
        let Some(raw_arr) = tx.fields.get("RawTransactions").and_then(|v| v.as_array()) else {
            return TxResult::ArrayEmpty;
        };
        if !raw_arr.iter().all(|e| matches!(e.get("RawTransaction"), Some(Value::Object(_)))) {
            return TxResult::Malformed;
        }
        let inners = inner_jsons(&tx.fields);
        // rippled: `if (rawTxns.size() <= 1) return temARRAY_EMPTY;` — a
        // single-inner batch is rejected, not just an empty one.
        if inners.len() <= 1 {
            return TxResult::ArrayEmpty;
        }
        if inners.len() > MAX_BATCH_TX_COUNT {
            return TxResult::TemArrayTooLarge;
        }
        // BatchSigners' size cap (kMaxBatchSigners = kMaxBatchTxCount * 3),
        // checked before the inner loop, same as rippled.
        if let Some(v) = tx.fields.get("BatchSigners") {
            match v.as_array() {
                Some(a) if a.len() > MAX_BATCH_SIGNERS => return TxResult::TemArrayTooLarge,
                Some(_) => {}
                None => return TxResult::BadSigner,
            }
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
            if seen_json.contains(inner) {
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
        if signers.windows(2).any(|w| w[0] >= w[1]) {
            return TxResult::BadSigner;
        }
        if signers.contains(&tx.account) {
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
        // Clear any previous batch's results up front: `take_inner_results`
        // must never expose a stale read from an earlier `Batch` on this
        // thread if this one turns out not to reach the loop below (it
        // always does today — `do_apply` only runs after preflight and
        // preclaim both succeed — but this keeps the invariant local rather
        // than relying on that).
        INNER_RESULTS.with(|r| r.borrow_mut().clear());
        let flags = flags_of(&tx.fields);
        let batch_snap = sandbox.snapshot();
        let mut results: Vec<String> = Vec::new();
        for inner in inner_jsons(&tx.fields) {
            let Some(mut f) = TxFields::from_json(inner) else {
                results.push(TxResult::Malformed.code_str().to_string());
                if flags & TF_ALL_OR_NOTHING != 0 {
                    sandbox.restore_snapshot(batch_snap.clone());
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
                    sandbox.restore_snapshot(batch_snap.clone());
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
        assert_eq!(
            pf(&outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6)], None)),
            "temARRAY_EMPTY",
            "rippled 3.3.0: rawTxns.size() <= 1 is temARRAY_EMPTY, not just 0"
        );
        let nine: Vec<Value> = (0..9).map(|i| inner_payment(1, 2, 1, 10 + i)).collect();
        assert_eq!(pf(&outer(TF_INDEPENDENT, nine, None)), "temARRAY_TOO_LARGE");
        let mut no_raw_txns = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 7)], None);
        no_raw_txns.as_object_mut().expect("object").remove("RawTransactions");
        assert_eq!(pf(&no_raw_txns), "temARRAY_EMPTY", "RawTransactions absent entirely");
    }

    #[test]
    fn preflight_rejects_a_malformed_raw_transactions_element_even_under_the_inner_count_cap() {
        // A 9-element array where one entry lacks "RawTransaction" must not
        // silently filter down to 8 well-formed inners and pass the ≤8 gate.
        let mut nine: Vec<Value> = (0..9).map(|i| inner_payment(1, 2, 1, 10 + i)).collect();
        nine[3] = json!({"NotARawTransaction": {}});
        assert_eq!(pf(&outer(TF_INDEPENDENT, nine, None)), "temMALFORMED");
    }

    #[test]
    fn preflight_checks_each_inner_s_field_rules() {
        // rippled 3.3.0: a missing tfInnerBatchTxn is temINVALID_FLAG (the
        // inner's own preflight0 check), not temINVALID_INNER_BATCH.
        let mut bad_flag = inner_payment(1, 2, 1, 6); bad_flag["RawTransaction"]["Flags"] = json!(0);
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![bad_flag, inner_payment(1, 2, 1, 7)], None)), "temINVALID_FLAG");
        let mut bad_fee = inner_payment(1, 2, 1, 6); bad_fee["RawTransaction"]["Fee"] = json!("10");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![bad_fee, inner_payment(1, 2, 1, 7)], None)), "temBAD_FEE");
        let mut both = inner_payment(1, 2, 1, 6); both["RawTransaction"]["TicketSequence"] = json!(9);
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![both, inner_payment(1, 2, 1, 7)], None)), "temSEQ_AND_TICKET");
        let mut nested = inner_payment(1, 2, 1, 6); nested["RawTransaction"]["TransactionType"] = json!("Batch");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![nested, inner_payment(1, 2, 1, 7)], None)), "temINVALID");
        // A pseudo-transaction type as an inner fails the inner's own
        // preflight0 (isPseudoTx && tfInnerBatchTxn -> temINVALID_FLAG
        // there), which Batch::preflight reports as temINVALID_INNER_BATCH.
        let mut pseudo = inner_payment(1, 2, 1, 6); pseudo["RawTransaction"]["TransactionType"] = json!("EnableAmendment");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![pseudo, inner_payment(1, 2, 1, 7)], None)), "temINVALID_INNER_BATCH");
    }

    #[test]
    fn preflight_splits_the_three_signature_field_rejections() {
        let mut txn_sig = inner_payment(1, 2, 1, 6); txn_sig["RawTransaction"]["TxnSignature"] = json!("3045");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![txn_sig, inner_payment(1, 2, 1, 7)], None)), "temBAD_SIGNATURE");
        let mut signers = inner_payment(1, 2, 1, 6); signers["RawTransaction"]["Signers"] = json!([]);
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![signers, inner_payment(1, 2, 1, 7)], None)), "temBAD_SIGNER");
        let mut reg_key = inner_payment(1, 2, 1, 6); reg_key["RawTransaction"]["SigningPubKey"] = json!("EDABCD");
        assert_eq!(pf(&outer(TF_INDEPENDENT, vec![reg_key, inner_payment(1, 2, 1, 7)], None)), "temBAD_REGKEY");
    }

    #[test]
    fn preflight_rejects_duplicates_by_mode() {
        let dup = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 6)], None);
        assert_eq!(pf(&dup), "temREDUNDANT", "identical inners are redundant in every mode");
        let same_seq = outer(TF_UNTIL_FAILURE, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 2, 6)], None);
        assert_eq!(pf(&same_seq), "temREDUNDANT", "same (account, sequence) under UntilFailure");
        let same_seq_ok = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 2, 6)], None);
        assert_eq!(pf(&same_seq_ok), "tesSUCCESS", "Independent tolerates a shared sequence (the second fails at apply)");
        let same_seq_only_one = outer(TF_ONLY_ONE, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 2, 6)], None);
        assert_eq!(pf(&same_seq_only_one), "tesSUCCESS", "OnlyOne is not in the AllOrNothing/UntilFailure dedup set either");
    }

    #[test]
    fn preflight_accepts_a_ticket_sequence_only_inner() {
        let mut ticket_only = inner_payment(1, 2, 1, 0);
        ticket_only["RawTransaction"].as_object_mut().expect("object").remove("Sequence");
        ticket_only["RawTransaction"]["TicketSequence"] = json!(9);
        let o = outer(TF_INDEPENDENT, vec![ticket_only, inner_payment(1, 2, 1, 7)], None);
        assert_eq!(pf(&o), "tesSUCCESS");
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
    fn preflight_rejects_more_than_max_batch_signers() {
        assert_eq!(MAX_BATCH_SIGNERS, 24, "kMaxBatchSigners = kMaxBatchTxCount * 3");
        // The cap is checked before the signer set is matched against the
        // inners, so a batch that would fail for other reasons still trips
        // temARRAY_TOO_LARGE first once the count exceeds 24.
        let signers: Vec<u8> = (10..(10 + MAX_BATCH_SIGNERS as u8 + 1)).collect();
        let o = outer(TF_INDEPENDENT, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 7)], Some(signers));
        assert_eq!(pf(&o), "temARRAY_TOO_LARGE");
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

    fn run<'a>(o: &Value, state: &'a LedgerState) -> (Sandbox<'a>, String, Vec<String>) {
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
        assert_eq!(inners, vec!["tesSUCCESS", "temBAD_SEQUENCE"]);
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

    #[test]
    fn take_inner_results_drains_and_a_second_call_is_empty() {
        let state = state_with_accounts(&[(1, 50_000_000, 5), (2, 20_000_000, 1)]);
        let o = outer(TF_UNTIL_FAILURE, vec![inner_payment(1, 2, 1, 6), inner_payment(1, 2, 1, 7)], None);
        let (_sb, _r, inners) = run(&o, &state);
        assert_eq!(inners, vec!["tesSUCCESS", "tesSUCCESS"]);
        assert_eq!(take_inner_results(), Vec::<String>::new(), "already drained by run()'s call");
    }
}
