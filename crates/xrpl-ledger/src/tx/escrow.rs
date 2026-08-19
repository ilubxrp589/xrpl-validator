//! Escrow transactions — EscrowCreate, EscrowFinish, EscrowCancel.
//!
//! EscrowCreate: lock XRP until a condition or time is met.
//! EscrowFinish: release locked XRP to the destination.
//! EscrowCancel: return locked XRP to the creator.
//!
//! # DEAD CODE WARNING
//!
//! This module is **not called** by the live validator. Production transaction
//! application is delegated to rippled's C++ engine via FFI — see
//! `crates/xrpl-ffi/src/lib.rs` and `crates/xrpl-node/src/ffi_engine.rs`.
//!
//! This code is retained as a reference implementation / learning artifact.
//! Tests in this module prove the code works in isolation; they do NOT prove
//! the validator is correct.
//!
//! If you are adding a new amendment or tx type: add it to the FFI path,
//! not here. See `ffi/ARCHITECTURE.md` for the architectural decision record.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a hex-encoded 20-byte account ID from a JSON field.
fn parse_account_id(val: &serde_json::Value) -> Option<[u8; 20]> {
    let hex_str = val.as_str()?;
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

/// Read the `OwnerCount` of an account from the sandbox, returning a mutable
/// JSON value and the key so callers can write it back after mutation.
fn read_account(
    sandbox: &Sandbox,
    account_id: &[u8; 20],
) -> Option<(serde_json::Value, xrpl_core::types::Hash256)> {
    let key = keylet::account_root_key(account_id);
    let data = sandbox.read(&key)?;
    let val: serde_json::Value = serde_json::from_slice(&data).ok()?;
    Some((val, key))
}

/// Read balance in drops from an AccountRoot JSON value.
fn balance_of(acct: &serde_json::Value) -> u64 {
    acct["Balance"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Read OwnerCount from an AccountRoot JSON value.
fn owner_count_of(acct: &serde_json::Value) -> u64 {
    acct["OwnerCount"].as_u64().unwrap_or(0)
}

// ===========================================================================
// EscrowCreate
// ===========================================================================

/// EscrowCreate transactor — locks XRP in an Escrow ledger entry.
pub struct EscrowCreateTransactor;

impl EscrowCreateTransactor {
    fn amount_drops(tx: &TxFields) -> Option<u64> {
        match &tx.fields.get("Amount")? {
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    fn destination(tx: &TxFields) -> Option<[u8; 20]> {
        parse_account_id(tx.fields.get("Destination")?)
    }

    /// The escrowed Amount as an IOU `(leg, value)` when it is not XRP.
    ///
    /// Token escrow (`featureTokenEscrow`) lets an Escrow hold an issued
    /// currency; the value is locked OFF the sender's trust line rather than
    /// deducted from its XRP. #105823810 6AB38288 escrows 3750000 STSH and we
    /// rejected it temBAD_AMOUNT, where mainnet built the escrow in 7 nodes.
    fn iou_amount(tx: &TxFields) -> Option<(crate::tx::offer::Leg, (u128, i32))> {
        let amt = tx.fields.get("Amount")?;
        if !amt.is_object() {
            return None;
        }
        let leg = crate::tx::offer::leg_of(amt)?;
        if leg.xrp {
            return None;
        }
        let v = keylet::amount_mant_exp(amt)?;
        (v.0 != 0).then_some((leg, v))
    }
}

impl Transactor for EscrowCreateTransactor {
    /// val-070: Format validation — no state access.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "EscrowCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }

        // Amount is XRP drops, or — under token escrow — an issued currency.
        if Self::iou_amount(tx).is_none() {
            let amount = match Self::amount_drops(tx) {
                Some(a) => a,
                None => return TxResult::BadAmount,
            };
            if amount == 0 || amount > 100_000_000_000_000_000 {
                return TxResult::BadAmount;
            }
        }

        // Destination must be present and valid
        if Self::destination(tx).is_none() {
            return TxResult::Malformed;
        }

        // Must have at least FinishAfter or Condition (or both)
        let has_finish_after = tx.fields.get("FinishAfter").is_some();
        let has_condition = tx.fields.get("Condition").is_some();
        if !has_finish_after && !has_condition {
            return TxResult::Malformed;
        }

        TxResult::Success
    }

    /// val-071: State validation — read-only checks.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        let acct_data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let acct: serde_json::Value = match serde_json::from_slice(&acct_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // rippled checks the destination in TWO places and the order differs
        // by AMOUNT TYPE. `EscrowCreate::preclaim` requires it to exist
        // (tecNO_DST, EscrowCreate.cpp:344-346). `doApply` then does the
        // reserve and — ONLY for an XRP amount, `if (isXRP(amount))` — the
        // funding test (tecUNFUNDED), and only after that the tag test
        // (tecDST_TAG_NEEDED, :450-457). A TOKEN escrow therefore reaches the
        // tag test having had NO funding test at all.
        //
        // #106143718 `A3A1944D0A83` escrows 3160 XRPL (an IOU) to a
        // destination carrying lsfRequireDestTag with no DestinationTag.
        // Mainnet claims the fee with tecDST_TAG_NEEDED; we returned
        // tecUNFUNDED_PAYMENT from our own token-holding test, which rippled
        // does not perform at this point.
        let Some(dest_id) = Self::destination(tx) else {
            return TxResult::Malformed;
        };
        let Some(dst) = sandbox
            .read(&keylet::account_root_key(&dest_id))
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
        else {
            return TxResult::NoDst;
        };
        // A PSEUDO-ACCOUNT CANNOT RECEIVE AN ESCROW. rippled tests this
        // immediately after the tecNO_DST read and BEFORE the token helper and
        // the tag test (EscrowCreate.cpp:350-352), so it outranks every other
        // reason this transaction could fail — and it is deliberately NOT
        // amendment-gated, because every write to a discriminator field is.
        //
        // `isPseudoAccount` is "an AccountRoot carrying any field marked
        // `kSmdPseudoAccount`" (AccountRootHelpers.cpp:194-208). The SOTemplate
        // marks exactly three: `sfAMMID`, `sfVaultID`, `sfLoanBrokerID`
        // (sfields.macro:180, :203, :206). An AMM's own account is the one that
        // turns up on mainnet.
        //
        // #106331706 is FOUR EscrowCreates from one sender at consecutive
        // sequences, and this single rule accounts for all four. Two name AMM
        // accounts as Destination and mainnet refuses them fee-only (83398EAD
        // seq …659, 17D6DD3C seq …663); we created the escrows. The other two
        // then diverged on MUTATION COUNT in OPPOSITE directions — 3B106F32
        // 10 v 6 and 42CB3ACF 6 v 10, four objects shared between the extra and
        // missing lists — because the escrows we wrongly created consumed
        // owner-directory slots, so each later escrow landed on a different
        // PAGE than mainnet's. Two of the shared objects are DirectoryNodes
        // mid-chain (`idx=2/13`, `2/6`), which is what page placement depends
        // on. ⇒ a mut-count pair erring in both directions and sharing keys is
        // one misplacement, not two bugs.
        if ["AMMID", "VaultID", "LoanBrokerID"].iter().any(|f| dst.get(*f).is_some()) {
            return TxResult::NoPermission;
        }
        let needs_tag = dst["Flags"].as_u64().unwrap_or(0) & 0x0002_0000 != 0
            && tx.fields.get("DestinationTag").is_none();

        // A token escrow locks the issued currency off the sender's trust
        // line, so its XRP only has to cover the fee. The rules here are
        // `escrowCreatePreclaimHelper<Issue>` (EscrowCreate.cpp:189-257), in
        // rippled's order — and ALL of them run in preclaim, before the
        // doApply-stage time and tag tests below.
        if let Some((leg, want)) = Self::iou_amount(tx) {
            use crate::tx::offer as ox;
            // The issuer cannot escrow its own IOU (:199-201).
            if leg.issuer == tx.account {
                return TxResult::NoPermission;
            }
            // The issuer must have opted into trust-line locking (:203-208).
            // Its AccountRoot is hydrated for every tx (collect_issuers), so
            // a readable issuer gates the rest; absence skips the helper
            // rather than inventing tecNO_ISSUER out of a fixture gap.
            if let Some(iss) = ox::json_at(sandbox, &keylet::account_root_key(&leg.issuer)) {
                let iss_flags = iss["Flags"].as_u64().unwrap_or(0);
                if iss_flags & 0x4000_0000 == 0 {
                    // lsfAllowTrustLineLocking
                    return TxResult::NoPermission;
                }
                // The sender must have a line at all (:210-213).
                let lkey = keylet::ripple_state_key(&tx.account, &leg.issuer, &leg.cur);
                let Some(line) = ox::json_at(sandbox, &lkey) else {
                    return TxResult::NoLine;
                };
                // Frozen sender or destination is tecFROZEN (:233-238):
                // the issuer's global freeze, or the ISSUER's side of that
                // party's line. The destination needs no line — only a
                // present-and-frozen one condemns. (requireAuth :225-230 and
                // the canAdd precision test :252-254 are not modeled — no
                // specimen, and neither state occurs on the corpora's
                // tokens.)
                let issuer_side = |who: &[u8; 20]| -> u64 {
                    if &leg.issuer > who { 0x0080_0000 } else { 0x0040_0000 }
                };
                let frozen = |who: &[u8; 20]| -> bool {
                    if iss_flags & 0x0040_0000 != 0 {
                        return true; // lsfGlobalFreeze
                    }
                    ox::json_at(sandbox, &keylet::ripple_state_key(who, &leg.issuer, &leg.cur))
                        .map(|l| l["Flags"].as_u64().unwrap_or(0) & issuer_side(who) != 0)
                        .unwrap_or(false)
                };
                if frozen(&tx.account) || frozen(&dest_id) {
                    return TxResult::Frozen;
                }
                // Funds under IgnoreFreeze (:240-250): non-positive holdings
                // or holdings short of the amount are tecINSUFFICIENT_FUNDS —
                // NOT tecUNFUNDED_PAYMENT, which this path used to answer
                // (and answered wrongly even for FUNDED senders whenever the
                // line was missing from the sandbox; the probe hydrates it
                // now).
                let (neg, bal) = ox::signed_value(&line["Balance"]);
                let party_holds = if tx.account < leg.issuer { !neg } else { neg };
                if !(party_holds && bal.0 > 0)
                    || ox::me_cmp(bal, want) == std::cmp::Ordering::Less
                {
                    return TxResult::InsufficientFunds;
                }
            }
        }

        // doApply's FIRST act (EscrowCreate.cpp:422-428): a CancelAfter or
        // FinishAfter already at-or-before the parent close time is
        // tecNO_PERMISSION — `after(closeTime, mark)` is strictly `>`
        // (View.cpp:559-562), and the harness header's close_time IS the
        // parent's close. It outranks the XRP funding test and the tag test,
        // both later in doApply.
        //
        // The 14-specimen burst (#106261496/582/583, e.g. C9BB730F) is one
        // bot creating escrows with near-immediate FinishAfter and losing
        // the race — parent close 63-92s past BOTH marks, senders fully
        // funded (1e9 held against 3438 wanted). #617051F3 pins the order:
        // its destination also demands a tag, and mainnet still answers
        // NO_PERMISSION.
        let close = sandbox.base().close_time() as u64;
        for f in ["CancelAfter", "FinishAfter"] {
            if let Some(mark) = tx.fields.get(f).and_then(|v| v.as_u64()) {
                if close > mark {
                    return TxResult::NoPermission;
                }
            }
        }

        if Self::iou_amount(tx).is_some() {
            if needs_tag {
                return TxResult::DstTagNeeded;
            }
            if balance_of(&acct) < tx.fee {
                return TxResult::UnfundedPayment;
            }
            return TxResult::Success;
        }

        // Check balance >= amount + fee
        let balance = balance_of(&acct);
        let amount = Self::amount_drops(tx).unwrap_or(0);
        let total_needed = amount.saturating_add(tx.fee);
        if balance < total_needed {
            return TxResult::UnfundedPayment;
        }
        // XRP escrow: the funding test comes FIRST, then the tag test.
        if needs_tag {
            return TxResult::DstTagNeeded;
        }

        TxResult::Success
    }

    /// val-072: Apply — deduct amount, create Escrow object, increment OwnerCount.
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let iou = Self::iou_amount(tx);
        let amount = match (&iou, Self::amount_drops(tx)) {
            (Some(_), _) => 0, // token escrow moves no XRP
            (None, Some(a)) => a,
            (None, None) => return TxResult::BadAmount,
        };
        let dest_id = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };

        // --- Sender: deduct amount and increment OwnerCount ---
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let sender_balance = balance_of(&sender);
        if iou.is_none() {
            if sender_balance < amount {
                return TxResult::UnfundedPayment;
            }
            sender["Balance"] = serde_json::Value::String((sender_balance - amount).to_string());
        }

        let oc = owner_count_of(&sender);
        sender["OwnerCount"] = serde_json::Value::Number((oc + 1).into());

        sandbox.write(sender_key, serde_json::to_vec(&sender).expect("serializing valid JSON Value"));

        // --- Create the Escrow ledger entry ---
        let escrow_key = keylet::escrow_key(&tx.account, tx.sequence);

        let mut escrow = serde_json::json!({
            "LedgerEntryType": "Escrow",
            "Account": hex::encode(tx.account),
            "Destination": hex::encode(dest_id),
            "Amount": match &iou {
                Some(_) => tx.fields["Amount"].clone(),
                None => serde_json::Value::String(amount.to_string()),
            },
            "OwnerNode": "0",
        });

        // Optional fields
        if let Some(v) = tx.fields.get("FinishAfter") {
            escrow["FinishAfter"] = v.clone();
        }
        if let Some(v) = tx.fields.get("CancelAfter") {
            escrow["CancelAfter"] = v.clone();
        }
        if let Some(v) = tx.fields.get("Condition") {
            escrow["Condition"] = v.clone();
        }

        sandbox.write(escrow_key, serde_json::to_vec(&escrow).expect("serializing valid JSON Value"));

        // An escrow is listed in every directory that needs to find it
        // (EscrowCreate.cpp doApply): always the sender's; the destination's
        // unless it is a self-send; and, for an IOU, the ISSUER's — "added to
        // the issuer's owner directory to help track the total locked
        // balance". This module previously kept no directory entries at all,
        // which stayed invisible only because no sampled ledger carried escrow
        // traffic until #105823810.
        crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &escrow_key);
        if dest_id != tx.account {
            crate::ledger::directory::owner_dir_insert(sandbox, &dest_id, &escrow_key);
            // Mainnet's meta carries the destination's AccountRoot as a no-op
            // Modified, the same touch CheckCreate already reproduces.
            let dkey = keylet::account_root_key(&dest_id);
            if let Some(d) = sandbox.read(&dkey) {
                sandbox.write(dkey, d);
            }
        }
        if let Some((leg, want)) = iou {
            if leg.issuer != tx.account && leg.issuer != dest_id {
                crate::ledger::directory::owner_dir_insert(sandbox, &leg.issuer, &escrow_key);
            }
            // Lock the tokens: they leave the sender's line and are held by the
            // escrow object itself, so no counterparty is credited
            // (`escrowLockApplyHelper`). #105823810's sender goes from holding
            // 92500000 STSH to 88750000 — exactly the escrowed 3750000.
            crate::tx::offer::line_adjust(sandbox, &tx.account, &leg, want, false);
        }

        TxResult::Success
    }
}

// ===========================================================================
// EscrowFinish
// ===========================================================================

/// EscrowFinish transactor — releases locked XRP to the destination.
/// The escrowed Amount off the ESCROW OBJECT as an IOU `(leg, value)`.
///
/// `EscrowCreate::iou_amount` reads the TRANSACTION, which is no use to Finish
/// and Cancel — those carry only `Owner` + `OfferSequence`, so the amount has
/// to come from the ledger entry. Both of them parsed `Amount` as
/// `as_str().parse::<u64>()` and fell back to `unwrap_or(0)`, which is how a
/// token escrow silently released NOTHING.
fn escrow_iou(escrow: &serde_json::Value) -> Option<(crate::tx::offer::Leg, (u128, i32))> {
    let amt = escrow.get("Amount")?;
    if !amt.is_object() {
        return None;
    }
    let leg = crate::tx::offer::leg_of(amt)?;
    if leg.xrp {
        return None;
    }
    let v = keylet::amount_mant_exp(amt)?;
    (v.0 != 0).then_some((leg, v))
}

/// Unlink an escrow from every directory `EscrowCreate` filed it in, and touch
/// the destination the way creation does. The mirror of the three
/// `owner_dir_insert` calls there: always the owner's, the destination's unless
/// self-sent, and — for a token escrow — the ISSUER's.
///
/// A real token escrow carries all three hints, e.g. #106179351's
/// `E7CFE233788C`: `OwnerNode "7"`, `DestinationNode "0"`, `IssuerNode "92"`.
/// Neither Finish nor Cancel removed ANY directory entry before this.
fn escrow_dir_teardown(
    sandbox: &mut Sandbox,
    escrow: &serde_json::Value,
    escrow_key: &xrpl_core::types::Hash256,
    owner_id: &[u8; 20],
) {
    use crate::ledger::directory::owner_dir_remove;
    let dirnum = |k: &str| escrow.get(k).map(crate::tx::offer::dirnum);
    owner_dir_remove(sandbox, owner_id, escrow_key, dirnum("OwnerNode"), true);

    let dest_id = escrow.get("Destination").and_then(parse_account_id);
    if let Some(d) = dest_id {
        if d != *owner_id {
            owner_dir_remove(sandbox, &d, escrow_key, dirnum("DestinationNode"), true);
            // Creation records the destination's AccountRoot as a no-op
            // Modified; the cancel/finish meta carries it too — #106179351's
            // third AccountRoot is exactly this, with no FinalFields at all.
            let dkey = keylet::account_root_key(&d);
            if let Some(b) = sandbox.read(&dkey) {
                sandbox.write(dkey, b);
            }
        }
    }
    if let Some((leg, _)) = escrow_iou(escrow) {
        if leg.issuer != *owner_id && Some(leg.issuer) != dest_id {
            owner_dir_remove(sandbox, &leg.issuer, escrow_key, dirnum("IssuerNode"), true);
        }
    }
}

pub struct EscrowFinishTransactor;

impl EscrowFinishTransactor {
    fn owner(tx: &TxFields) -> Option<[u8; 20]> {
        parse_account_id(tx.fields.get("Owner")?)
    }

    fn offer_sequence(tx: &TxFields) -> Option<u32> {
        tx.fields
            .get("OfferSequence")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
    }
}

impl Transactor for EscrowFinishTransactor {
    /// val-073: Format validation.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "EscrowFinish" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if Self::owner(tx).is_none() {
            return TxResult::Malformed;
        }
        if Self::offer_sequence(tx).is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    /// val-074: State validation — escrow must exist.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let owner_id = match Self::owner(tx) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let offer_seq = match Self::offer_sequence(tx) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let esc_key = keylet::escrow_key(&owner_id, offer_seq);

        if !sandbox.exists(&esc_key) {
            return TxResult::NoEntry;
        }

        TxResult::Success
    }

    /// val-075: Apply — credit destination, delete escrow, decrement OwnerCount.
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let owner_id = match Self::owner(tx) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let offer_seq = match Self::offer_sequence(tx) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };

        // --- Read the Escrow object ---
        let esc_key = keylet::escrow_key(&owner_id, offer_seq);
        let esc_data = match sandbox.read(&esc_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let escrow: serde_json::Value = match serde_json::from_slice(&esc_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // --- Time checks ---
        let close_time = sandbox.base().header.close_time as u64;

        // Bug 3 fix: If escrow has CancelAfter and it has passed, the escrow is
        // expired and can only be cancelled, not finished.
        if let Some(cancel_after) = escrow.get("CancelAfter").and_then(|v| v.as_u64()) {
            if close_time > cancel_after {
                return TxResult::NoPermission;
            }
        }

        // Bug 2 fix: If escrow has FinishAfter, close_time must be past it.
        // TODO: Also verify Condition/Fulfillment crypto (cryptoconditions) when present.
        if let Some(finish_after) = escrow.get("FinishAfter").and_then(|v| v.as_u64()) {
            if close_time <= finish_after {
                return TxResult::NoPermission;
            }
        }

        // Parse Amount from escrow
        let amount = escrow["Amount"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // Parse Destination from escrow
        let dest_id = match escrow.get("Destination").and_then(|v| parse_account_id(v)) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };

        // --- Credit the destination ---
        let dest_key = keylet::account_root_key(&dest_id);
        let dest_data = match sandbox.read(&dest_key) {
            Some(d) => d,
            None => return TxResult::NoDst,
        };
        let mut dest: serde_json::Value = match serde_json::from_slice(&dest_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let dest_balance = balance_of(&dest);
        let new_dest_balance = match dest_balance.checked_add(amount) {
            Some(b) => b,
            None => return TxResult::Malformed,
        };
        dest["Balance"] = serde_json::Value::String(new_dest_balance.to_string());
        sandbox.write(dest_key, serde_json::to_vec(&dest).expect("serializing valid JSON Value"));

        // A TOKEN escrow RELEASES the issued currency to the destination. Same
        // hole Cancel had: `Amount` is an object, so the XRP credit above adds
        // 0 drops and the tokens simply never arrived. `EscrowCreate` locked
        // them off the sender's line and nothing ever gave them back.
        if let Some((leg, want)) = escrow_iou(&escrow) {
            crate::tx::offer::line_adjust(sandbox, &dest_id, &leg, want, true);
        }

        // --- Delete the Escrow object ---
        sandbox.delete(esc_key);

        // --- Decrement the owner's OwnerCount ---
        let owner_key = keylet::account_root_key(&owner_id);
        let owner_data = match sandbox.read(&owner_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut owner_acct: serde_json::Value = match serde_json::from_slice(&owner_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let oc = owner_count_of(&owner_acct);
        owner_acct["OwnerCount"] = serde_json::Value::Number(oc.saturating_sub(1).into());
        sandbox.write(owner_key, serde_json::to_vec(&owner_acct).expect("serializing valid JSON Value"));

        escrow_dir_teardown(sandbox, &escrow, &esc_key, &owner_id);

        TxResult::Success
    }
}

// ===========================================================================
// EscrowCancel
// ===========================================================================

/// EscrowCancel transactor — returns locked XRP to the creator.
pub struct EscrowCancelTransactor;

impl EscrowCancelTransactor {
    fn owner(tx: &TxFields) -> Option<[u8; 20]> {
        parse_account_id(tx.fields.get("Owner")?)
    }

    fn offer_sequence(tx: &TxFields) -> Option<u32> {
        tx.fields
            .get("OfferSequence")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
    }
}

impl Transactor for EscrowCancelTransactor {
    /// val-076: Format validation.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "EscrowCancel" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if Self::owner(tx).is_none() {
            return TxResult::Malformed;
        }
        if Self::offer_sequence(tx).is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    /// val-077: State validation — escrow must exist.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let owner_id = match Self::owner(tx) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let offer_seq = match Self::offer_sequence(tx) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let esc_key = keylet::escrow_key(&owner_id, offer_seq);

        if !sandbox.exists(&esc_key) {
            return TxResult::NoEntry;
        }

        TxResult::Success
    }

    /// val-078: Apply — credit owner with Amount, delete escrow, decrement OwnerCount.
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let owner_id = match Self::owner(tx) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let offer_seq = match Self::offer_sequence(tx) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };

        // --- Read the Escrow object ---
        let esc_key = keylet::escrow_key(&owner_id, offer_seq);
        let esc_data = match sandbox.read(&esc_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let escrow: serde_json::Value = match serde_json::from_slice(&esc_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // --- Time check: if escrow has CancelAfter, only allow anyone to cancel
        // after CancelAfter has passed. Before that, only the escrow creator can cancel. ---
        let close_time = sandbox.base().header.close_time as u64;
        if let Some(cancel_after) = escrow.get("CancelAfter").and_then(|v| v.as_u64()) {
            if close_time <= cancel_after {
                // CancelAfter hasn't passed yet — only the escrow creator may cancel
                if tx.account != owner_id {
                    return TxResult::NoPermission;
                }
            }
        }

        // Parse Amount from escrow
        let amount = escrow["Amount"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // --- Credit the owner (escrow creator) ---
        let owner_key = keylet::account_root_key(&owner_id);
        let owner_data = match sandbox.read(&owner_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut owner_acct: serde_json::Value = match serde_json::from_slice(&owner_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let owner_balance = balance_of(&owner_acct);
        let new_owner_balance = match owner_balance.checked_add(amount) {
            Some(b) => b,
            None => return TxResult::Malformed,
        };
        owner_acct["Balance"] = serde_json::Value::String(new_owner_balance.to_string());

        let oc = owner_count_of(&owner_acct);
        owner_acct["OwnerCount"] = serde_json::Value::Number(oc.saturating_sub(1).into());

        sandbox.write(owner_key, serde_json::to_vec(&owner_acct).expect("serializing valid JSON Value"));

        // A TOKEN escrow returns the issued currency to the sender's line —
        // the mirror of the `line_adjust(.., false)` that `EscrowCreate` used
        // to lock it. The XRP credit above is a no-op for one of these, since
        // `Amount` is an object and parses to 0 drops.
        //
        // #106179351: EscrowCancel attempted=192, MATCH=0, every one of them
        // this shape. We returned nothing and unlinked nothing.
        if let Some((leg, want)) = escrow_iou(&escrow) {
            crate::tx::offer::line_adjust(sandbox, &owner_id, &leg, want, true);
        }

        // --- Delete the Escrow object and unlink it everywhere ---
        sandbox.delete(esc_key);
        escrow_dir_teardown(sandbox, &escrow, &esc_key, &owner_id);

        TxResult::Success
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::{apply_modifications, Sandbox};
    use crate::ledger::state::LedgerState;
    use crate::ledger::transactor::apply_common;
    use xrpl_core::types::Hash256;

    fn make_state() -> LedgerState {
        let header = LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        LedgerState::new_unverified(header)
    }

    fn add_account(state: &mut LedgerState, id: &[u8; 20], balance: u64, seq: u32) {
        let acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": seq,
            "OwnerCount": 0,
            "Flags": 0,
        });
        let key = keylet::account_root_key(id);
        state
            .state_map
            .insert(key, serde_json::to_vec(&acct).unwrap())
            .unwrap();
    }

    fn read_balance(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        balance_of(&v)
    }

    fn read_owner_count(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        owner_count_of(&v)
    }

    fn read_balance_from_state(state: &LedgerState, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = state.state_map.lookup(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(data).unwrap();
        balance_of(&v)
    }

    // -----------------------------------------------------------------------
    // EscrowCreate tests
    // -----------------------------------------------------------------------

    /// Token escrow: a non-XRP Amount is legal, the value is locked OFF the
    /// sender's trust line rather than deducted from its XRP, and the escrow is
    /// listed in THREE owner directories — sender, destination and issuer
    /// ("added to the issuer's owner directory to help track the total locked
    /// balance", EscrowCreate.cpp doApply). #105823810 6AB38288 escrows
    /// 3750000 STSH: we returned temBAD_AMOUNT and applied nothing, where
    /// mainnet built it in 7 nodes.
    /// A pseudo-account cannot receive an escrow: rippled refuses fee-only with
    /// tecNO_PERMISSION before the token helper or the tag test ever run
    /// (EscrowCreate.cpp:350-352). The discriminators are `AMMID`, `VaultID`
    /// and `LoanBrokerID` (sfields.macro:180, :203, :206) — an AMM's own
    /// account is the mainnet case, #106331706 83398EAD and 17D6DD3C.
    #[test]
    fn an_escrow_to_a_pseudo_account_is_refused() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];

        let build = |disc: Option<&str>| {
            let mut state = make_state();
            add_account(&mut state, &sender, 50_000_000, 1);
            add_account(&mut state, &dest, 50_000_000, 1);
            if let Some(f) = disc {
                let dkey = keylet::account_root_key(&dest);
                let mut acct: serde_json::Value =
                    serde_json::from_slice(state.state_map.lookup(&dkey).unwrap()).unwrap();
                acct[f] = serde_json::json!(hex::encode_upper([0xABu8; 32]));
                state.state_map.insert(dkey, serde_json::to_vec(&acct).unwrap()).unwrap();
            }
            state
        };
        let tx = TxFields {
            account: sender,
            tx_type: "EscrowCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "1000000",
                "FinishAfter": 900,
            }),
        };

        // An ordinary destination is fine — this is the control, and it is what
        // proves the refusal below is the discriminator and not the fixture.
        let plain = build(None);
        assert_eq!(
            EscrowCreateTransactor.preclaim(&tx, &Sandbox::new(&plain)),
            TxResult::Success,
            "an ordinary destination must still accept an escrow"
        );

        // Each discriminator alone is enough, and it outranks everything after.
        for f in ["AMMID", "VaultID", "LoanBrokerID"] {
            let state = build(Some(f));
            assert_eq!(
                EscrowCreateTransactor.preclaim(&tx, &Sandbox::new(&state)),
                TxResult::NoPermission,
                "a destination carrying {f} is a pseudo-account and cannot receive an escrow"
            );
        }
    }

    #[test]
    fn a_token_escrow_locks_the_line_and_lists_three_directories() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let issuer = [0x03u8; 20];
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "STS", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();

        let mut state = make_state();
        for id in [&sender, &dest, &issuer] {
            add_account(&mut state, id, 50_000_000, 1);
        }
        // A lockable token's issuer carries lsfAllowTrustLineLocking — the
        // preclaim now enforces it (tecNO_PERMISSION otherwise), so the
        // fixture must model what mainnet's escrowable issuers actually set.
        {
            let ikey = keylet::account_root_key(&issuer);
            let mut ia: serde_json::Value =
                serde_json::from_slice(&Sandbox::new(&state).read(&ikey).unwrap()).unwrap();
            ia["Flags"] = serde_json::json!(0x4000_0000u64);
            state.state_map.insert(ikey, serde_json::to_vec(&ia).unwrap()).unwrap();
        }
        let (lo, hi) = if sender < issuer { (sender, issuer) } else { (issuer, sender) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur),
                        "issuer": "0000000000000000000000000000000000000000",
                        "value": if sender < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
        });
        let lkey = keylet::ripple_state_key(&sender, &issuer, &cur);
        state.state_map.insert(lkey, serde_json::to_vec(&line).unwrap()).unwrap();

        let tx = TxFields {
            account: sender,
            tx_type: "EscrowCreate".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": {"currency": "STS", "issuer": hex::encode(issuer), "value": "30"},
                "FinishAfter": 900,
            }),
        };
        let mut sandbox = Sandbox::new(&state);
        assert_eq!(EscrowCreateTransactor.preflight(&tx), TxResult::Success, "an IOU Amount is legal");
        assert_eq!(EscrowCreateTransactor.preclaim(&tx, &sandbox), TxResult::Success);
        assert_eq!(EscrowCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let ekey = keylet::escrow_key(&sender, 5);
        let esc: serde_json::Value =
            serde_json::from_slice(&sandbox.read(&ekey).expect("escrow created")).unwrap();
        assert_eq!(esc["Amount"]["value"].as_str(), Some("30"), "the IOU amount is kept verbatim");

        // The tokens left the sender's line and no counterparty was credited.
        let held = crate::tx::offer::available(
            &sandbox,
            &sender,
            &crate::tx::offer::leg_of(&tx.fields["Amount"]).unwrap(),
        );
        assert!(
            crate::tx::offer::me_cmp(held, (70_000_000_000_000_000u128, -15)).is_eq(),
            "100 held minus 30 escrowed leaves 70, got {held:?}",
        );

        // Sender, destination AND issuer all list the escrow.
        for (who, label) in [(&sender, "sender"), (&dest, "destination"), (&issuer, "issuer")] {
            let root = keylet::owner_dir_key(who);
            let dir: serde_json::Value =
                serde_json::from_slice(&sandbox.read(&root).unwrap_or_else(|| panic!("{label} dir"))).unwrap();
            let listed = dir["Indexes"].as_array().map(|a| {
                a.iter().any(|e| e.as_str() == Some(&hex::encode_upper(ekey.0)))
            });
            assert_eq!(listed, Some(true), "{label}'s owner directory must list the escrow");
        }
    }

    #[test]
    fn escrow_create_full_pipeline() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &alice, 100_000_000, 1); // 100 XRP
        add_account(&mut state, &bob, 50_000_000, 1);

        let tx = TxFields {
            account: alice,
            tx_type: "EscrowCreate".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(bob),
                "Amount": "25000000",
                "FinishAfter": 600000000,
            }),
        };

        let transactor = EscrowCreateTransactor;

        // Preflight
        assert_eq!(transactor.preflight(&tx), TxResult::Success);

        let mods = {
            let mut sandbox = Sandbox::new(&state);

            // Preclaim
            assert_eq!(transactor.preclaim(&tx, &sandbox), TxResult::Success);

            // Common (deducts fee=12, increments sequence 1→2)
            assert_eq!(apply_common(&tx, &mut sandbox), TxResult::Success);

            // do_apply
            assert_eq!(transactor.do_apply(&tx, &mut sandbox), TxResult::Success);

            // Alice: 100M - 12(fee) - 25M(amount) = 74,999,988
            assert_eq!(read_balance(&sandbox, &alice), 74_999_988);
            // Alice OwnerCount: 0 → 1
            assert_eq!(read_owner_count(&sandbox, &alice), 1);

            // Escrow object should exist
            let esc_key = keylet::escrow_key(&alice, 1);
            assert!(sandbox.exists(&esc_key));

            // Verify escrow contents
            let esc_data = sandbox.read(&esc_key).unwrap();
            let esc: serde_json::Value = serde_json::from_slice(&esc_data).unwrap();
            assert_eq!(esc["LedgerEntryType"], "Escrow");
            assert_eq!(esc["Amount"].as_str().unwrap(), "25000000");
            assert_eq!(esc["Destination"].as_str().unwrap(), hex::encode(bob));
            assert_eq!(esc["FinishAfter"], 600000000);

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();
        assert_eq!(read_balance_from_state(&state, &alice), 74_999_988);
    }

    #[test]
    fn escrow_create_preflight_no_condition_no_finish() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let tx = TxFields {
            account: alice,
            tx_type: "EscrowCreate".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(bob),
                "Amount": "25000000",
                // No FinishAfter, no Condition
            }),
        };
        assert_eq!(EscrowCreateTransactor.preflight(&tx), TxResult::Malformed);
    }

    #[test]
    fn escrow_create_insufficient_balance() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &alice, 1_000_000, 1); // only 1 XRP
        // The DESTINATION has to exist for this to be a funding test at all:
        // `EscrowCreate::preclaim` reads it and returns tecNO_DST before any
        // funding is considered (EscrowCreate.cpp:344-346). Without bob here
        // the case asserted UnfundedPayment while rippled would say tecNO_DST,
        // and it only passed because we had no destination check.
        add_account(&mut state, &bob, 50_000_000, 1);

        let tx = TxFields {
            account: alice,
            tx_type: "EscrowCreate".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(bob),
                "Amount": "50000000",
                "FinishAfter": 600000000,
            }),
        };

        let sandbox = Sandbox::new(&state);
        assert_eq!(
            EscrowCreateTransactor.preclaim(&tx, &sandbox),
            TxResult::UnfundedPayment
        );
    }

    // -----------------------------------------------------------------------
    // EscrowFinish tests
    // -----------------------------------------------------------------------

    #[test]
    fn escrow_finish_full_pipeline() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let charlie = [0x03u8; 20]; // finisher (anyone can finish)
        let mut state = make_state();
        add_account(&mut state, &alice, 74_999_988, 2); // after escrow create
        add_account(&mut state, &bob, 50_000_000, 1);
        add_account(&mut state, &charlie, 10_000_000, 1);

        // Set Alice's OwnerCount to 1 (she has an escrow)
        {
            let key = keylet::account_root_key(&alice);
            let data = state.state_map.lookup(&key).unwrap();
            let mut acct: serde_json::Value = serde_json::from_slice(data).unwrap();
            acct["OwnerCount"] = serde_json::Value::Number(1.into());
            state
                .state_map
                .insert(key, serde_json::to_vec(&acct).unwrap())
                .unwrap();
        }

        // Insert the Escrow object (as if created by EscrowCreate with seq=1)
        // FinishAfter=5 so that close_time=10 > 5 allows finishing
        let escrow_obj = serde_json::json!({
            "LedgerEntryType": "Escrow",
            "Account": hex::encode(alice),
            "Destination": hex::encode(bob),
            "Amount": "25000000",
            "FinishAfter": 5,
            "OwnerNode": "0",
        });
        let esc_key = keylet::escrow_key(&alice, 1);
        state
            .state_map
            .insert(esc_key, serde_json::to_vec(&escrow_obj).unwrap())
            .unwrap();

        // Charlie finishes Alice's escrow
        let tx = TxFields {
            account: charlie,
            tx_type: "EscrowFinish".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Owner": hex::encode(alice),
                "OfferSequence": 1,
            }),
        };

        let transactor = EscrowFinishTransactor;
        assert_eq!(transactor.preflight(&tx), TxResult::Success);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            assert_eq!(transactor.preclaim(&tx, &sandbox), TxResult::Success);
            assert_eq!(apply_common(&tx, &mut sandbox), TxResult::Success);
            assert_eq!(transactor.do_apply(&tx, &mut sandbox), TxResult::Success);

            // Bob receives 25 XRP: 50M + 25M = 75M
            assert_eq!(read_balance(&sandbox, &bob), 75_000_000);
            // Alice's OwnerCount goes from 1 → 0
            assert_eq!(read_owner_count(&sandbox, &alice), 0);
            // Escrow object deleted
            assert!(!sandbox.exists(&esc_key));

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();
        assert_eq!(read_balance_from_state(&state, &bob), 75_000_000);
    }

    #[test]
    fn escrow_finish_no_escrow() {
        let alice = [0x01u8; 20];
        let charlie = [0x03u8; 20];
        let mut state = make_state();
        add_account(&mut state, &charlie, 10_000_000, 1);

        let tx = TxFields {
            account: charlie,
            tx_type: "EscrowFinish".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Owner": hex::encode(alice),
                "OfferSequence": 99, // does not exist
            }),
        };

        let sandbox = Sandbox::new(&state);
        assert_eq!(
            EscrowFinishTransactor.preclaim(&tx, &sandbox),
            TxResult::NoEntry
        );
    }

    // -----------------------------------------------------------------------
    // EscrowCancel tests
    // -----------------------------------------------------------------------

    #[test]
    fn escrow_cancel_full_pipeline() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &alice, 74_999_988, 2); // after escrow create
        add_account(&mut state, &bob, 50_000_000, 1);

        // Set Alice's OwnerCount to 1
        {
            let key = keylet::account_root_key(&alice);
            let data = state.state_map.lookup(&key).unwrap();
            let mut acct: serde_json::Value = serde_json::from_slice(data).unwrap();
            acct["OwnerCount"] = serde_json::Value::Number(1.into());
            state
                .state_map
                .insert(key, serde_json::to_vec(&acct).unwrap())
                .unwrap();
        }

        // Insert the Escrow object
        let escrow_obj = serde_json::json!({
            "LedgerEntryType": "Escrow",
            "Account": hex::encode(alice),
            "Destination": hex::encode(bob),
            "Amount": "25000000",
            "CancelAfter": 500000000,
            "OwnerNode": "0",
        });
        let esc_key = keylet::escrow_key(&alice, 1);
        state
            .state_map
            .insert(esc_key, serde_json::to_vec(&escrow_obj).unwrap())
            .unwrap();

        // Alice cancels the escrow
        let tx = TxFields {
            account: alice,
            tx_type: "EscrowCancel".to_string(),
            fee: 12,
            sequence: 2,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Owner": hex::encode(alice),
                "OfferSequence": 1,
            }),
        };

        let transactor = EscrowCancelTransactor;
        assert_eq!(transactor.preflight(&tx), TxResult::Success);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            assert_eq!(transactor.preclaim(&tx, &sandbox), TxResult::Success);
            assert_eq!(apply_common(&tx, &mut sandbox), TxResult::Success);
            assert_eq!(transactor.do_apply(&tx, &mut sandbox), TxResult::Success);

            // Alice gets her 25 XRP back: 74,999,988 - 12(fee) + 25,000,000 = 99,999,976
            assert_eq!(read_balance(&sandbox, &alice), 99_999_976);
            // Alice's OwnerCount: 1 → 0
            assert_eq!(read_owner_count(&sandbox, &alice), 0);
            // Escrow deleted
            assert!(!sandbox.exists(&esc_key));

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();
        assert_eq!(read_balance_from_state(&state, &alice), 99_999_976);
    }

    #[test]
    fn escrow_cancel_preflight_missing_owner() {
        let alice = [0x01u8; 20];
        let tx = TxFields {
            account: alice,
            tx_type: "EscrowCancel".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                // no Owner
                "OfferSequence": 1,
            }),
        };
        assert_eq!(EscrowCancelTransactor.preflight(&tx), TxResult::Malformed);
    }

    #[test]
    fn escrow_cancel_preflight_missing_offer_sequence() {
        let alice = [0x01u8; 20];
        let tx = TxFields {
            account: alice,
            tx_type: "EscrowCancel".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Owner": hex::encode(alice),
                // no OfferSequence
            }),
        };
        assert_eq!(EscrowCancelTransactor.preflight(&tx), TxResult::Malformed);
    }
}
