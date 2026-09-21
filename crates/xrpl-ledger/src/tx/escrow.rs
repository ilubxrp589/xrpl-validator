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

    /// The escrowed Amount as an MPT `(issuance id, value)`.
    ///
    /// Finding 332 (devnet 5418986 161CF6C1 / 5419010 C892D928): TokenEscrow
    /// covers MPTs too — `escrowCreatePreflightHelper<MPTIssue>` — and we
    /// answered temBAD_AMOUNT because the amount is neither drops nor an
    /// issued currency. The value is locked ON the holder's MPToken
    /// (MPTAmount −, LockedAmount +) and mirrored on the issuance.
    fn mpt_amount(tx: &TxFields) -> Option<([u8; 24], u64)> {
        let amt = tx.fields.get("Amount")?;
        if !amt.is_object() || amt.get("mpt_issuance_id").is_none() {
            return None;
        }
        crate::tx::mpt::parse_mpt_amount(amt)
    }
}

impl Transactor for EscrowCreateTransactor {
    /// val-070: Format validation — no state access.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "EscrowCreate" {
            return TxResult::Malformed;
        }
        if tx.fee_missing() {
            return TxResult::BadFee;
        }

        // Amount is XRP drops, or — under token escrow — an issued currency
        // or an MPT (finding 332: zero or past kMaxMpTokenAmount is
        // temBAD_AMOUNT, Escrow.cpp:114-116).
        if let Some(amt) = tx.fields.get("Amount").filter(|a| a.get("mpt_issuance_id").is_some()) {
            match crate::tx::mpt::parse_mpt_amount(amt) {
                Some((_, v)) if v > 0 && v <= crate::tx::mpt::MAX_MPT_AMOUNT => {}
                _ => return TxResult::BadAmount,
            }
        } else if Self::iou_amount(tx).is_none() {
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

        // Finding 322 (testnet 20864035 fuzz cancelafter:past): at least one
        // timeout, and a CancelAfter that is strictly after FinishAfter —
        // temBAD_EXPIRATION either way (Escrow.cpp:151-159), judged before
        // the fix1571 "FinishAfter or Condition" rule below.
        let finish_after = tx.fields.get("FinishAfter").and_then(|v| v.as_u64());
        let cancel_after = tx.fields.get("CancelAfter").and_then(|v| v.as_u64());
        if finish_after.is_none() && cancel_after.is_none() {
            return TxResult::BadExpiration;
        }
        if let (Some(c), Some(f)) = (cancel_after, finish_after) {
            if c <= f {
                return TxResult::BadExpiration;
            }
        }
        // Must have at least FinishAfter or Condition (or both)
        let has_finish_after = finish_after.is_some();
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

        // Finding 332: `escrowCreatePreclaimHelper<MPTIssue>` (Escrow.cpp:
        // 284-364), in rippled's order. The issuer of an MPT is embedded in
        // its id (bytes 4..24), which is what `amount.getIssuer()` reads.
        if let Some((mptid, want)) = Self::mpt_amount(tx) {
            use crate::tx::mpt as mp;
            let id_issuer: [u8; 20] = mptid[4..24].try_into().expect("24-byte id");
            if id_issuer == tx.account {
                return TxResult::NoPermission;
            }
            let ikey = keylet::mpt_issuance_key(&mptid);
            let Some(issuance) = mp::json_at(sandbox, &ikey) else {
                return TxResult::ObjectNotFound;
            };
            let iflags = issuance["Flags"].as_u64().unwrap_or(0);
            if iflags & mp::LSF_MPT_CAN_ESCROW == 0 {
                return TxResult::NoPermission;
            }
            if mp::issuance_issuer(&issuance) != Some(id_issuer) {
                return TxResult::NoPermission;
            }
            let tkey = keylet::mptoken_key(&ikey, &tx.account);
            let Some(token) = mp::json_at(sandbox, &tkey) else {
                return TxResult::ObjectNotFound;
            };
            for who in [&tx.account, &dest_id] {
                if let Some(t) = mp::require_auth_weak(sandbox, &ikey, &issuance, who) {
                    return t;
                }
            }
            for who in [&tx.account, &dest_id] {
                if mp::any_frozen(sandbox, &ikey, &issuance, &[who]) {
                    return TxResult::Locked;
                }
            }
            if !mp::can_transfer(&issuance, &tx.account, &dest_id) {
                return TxResult::NoAuth;
            }
            // accountHolds(IGNORE_FREEZE, IGNORE_AUTH) = the token's MPTAmount.
            let spendable = mp::dec_field(&token, "MPTAmount");
            if spendable == 0 || spendable < want {
                return TxResult::InsufficientFunds;
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

        // F85 — EscrowCreate::doApply (Escrow.cpp:496-506) judges the
        // post-fee balance (mSourceBalance) against the reserve for
        // OwnerCount + 1 — the escrow about to exist — BEFORE anything else:
        // tecINSUFFICIENT_RESERVE for XRP and token escrows alike; then, for an
        // XRP amount only, mSourceBalance < reserve + amount is tecUNFUNDED.
        // The destination tests (tag) come after both. #106703533 F84D3EB3:
        // 1000 drops from an account with 96 objects and 20274654 drops —
        // reserve(97) = 20400000, mainnet refuses; we escrowed it.
        let post_fee = balance_of(&acct).saturating_sub(tx.fee);
        let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
        let reserve = crate::ledger::fees::account_reserve(sandbox, oc + 1);
        if post_fee < reserve {
            return TxResult::InsufficientReserve;
        }
        if Self::iou_amount(tx).is_some() || Self::mpt_amount(tx).is_some() {
            if needs_tag {
                return TxResult::DstTagNeeded;
            }
            return TxResult::Success;
        }
        let amount = Self::amount_drops(tx).unwrap_or(0);
        if post_fee < reserve.saturating_add(amount) {
            return TxResult::Unfunded;
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
        let mpt = Self::mpt_amount(tx);
        let amount = match (&iou, &mpt, Self::amount_drops(tx)) {
            (Some(_), _, _) | (_, Some(_), _) => 0, // token escrow moves no XRP
            (None, None, Some(a)) => a,
            (None, None, None) => return TxResult::BadAmount,
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
        if iou.is_none() && mpt.is_none() {
            if sender_balance < amount {
                return TxResult::UnfundedPayment;
            }
            sender["Balance"] = serde_json::Value::String((sender_balance - amount).to_string());
        }

        let oc = owner_count_of(&sender);
        sender["OwnerCount"] = serde_json::Value::Number((oc + 1).into());

        sandbox.write(sender_key, serde_json::to_vec(&sender).expect("serializing valid JSON Value"));

        // --- Create the Escrow ledger entry ---
        // rippled keys the escrow AND stamps the object with getSeqValue()
        // (Escrow.cpp:531+544) — the tx sequence, or the ticket sequence for
        // a ticketed create. The object carried no Sequence at all until
        // 2026-08-31 (live shadow #106674441: ours 114B vs canonical 119B,
        // diff @8 = exactly the missing 0x24 UInt32).
        let seq_value = if tx.uses_ticket() { tx.ticket_seq.unwrap_or(0) } else { tx.sequence };
        let escrow_key = keylet::escrow_key(&tx.account, seq_value);

        let mut escrow = serde_json::json!({
            "LedgerEntryType": "Escrow",
            "Flags": 0,
            "Account": hex::encode(tx.account),
            "Sequence": seq_value,
            "Destination": hex::encode(dest_id),
            "Amount": if iou.is_some() || mpt.is_some() {
                tx.fields["Amount"].clone()
            } else {
                serde_json::Value::String(amount.to_string())
            },
        });

        // Optional fields
        for f in ["FinishAfter", "CancelAfter", "Condition", "SourceTag", "DestinationTag"] {
            if let Some(v) = tx.fields.get(f) {
                escrow[f] = v.clone();
            }
        }

        // An escrow is listed in every directory that needs to find it
        // (EscrowCreate.cpp doApply): always the sender's; the destination's
        // unless it is a self-send; and, for an IOU, the ISSUER's — "added to
        // the issuer's owner directory to help track the total locked
        // balance". The object stores each hint (byte census: our Escrow
        // lacked Flags AND DestinationNode — 14 bytes short of mainnet's
        // blob). The destination-root touch in the meta is threadOwners.
        let owner_node =
            crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &escrow_key);
        escrow["OwnerNode"] = serde_json::Value::String(format!("{owner_node:x}"));
        if dest_id != tx.account {
            let dn = crate::ledger::directory::owner_dir_insert(sandbox, &dest_id, &escrow_key);
            escrow["DestinationNode"] = serde_json::Value::String(format!("{dn:x}"));
        }
        if let Some((leg, want)) = iou {
            // Finding 164 (#106758324 330B52F56E04, r4uNAZYC2k escrowing 10
            // BST whose issuer charges 1.08): a token escrow snapshots the
            // issuer's transfer rate at creation — `(*slep)[sfTransferRate]
            // = transferRate(view, amount).value` when it is not parity
            // (EscrowCreate.cpp:497-499) — and EscrowFinish delivers against
            // the LESSER of that snapshot and the rate then in force. Our
            // object carried no rate (five bytes short) and the finish
            // credited the full 10 where mainnet credits 9.2592592592593.
            if let Some(rate) = crate::tx::offer::transfer_rate(sandbox, &leg) {
                if rate != 1_000_000_000 {
                    escrow["TransferRate"] = serde_json::Value::from(rate);
                }
            }
            if leg.issuer != tx.account && leg.issuer != dest_id {
                let inode =
                    crate::ledger::directory::owner_dir_insert(sandbox, &leg.issuer, &escrow_key);
                escrow["IssuerNode"] = serde_json::Value::String(format!("{inode:x}"));
            }
            // Lock the tokens: they leave the sender's line and are held by the
            // escrow object itself, so no counterparty is credited
            // (`escrowLockApplyHelper`). #105823810's sender goes from holding
            // 92500000 STSH to 88750000 — exactly the escrowed 3750000.
            crate::tx::offer::line_adjust(sandbox, &tx.account, &leg, want, false);
        }
        if let Some((mptid, want)) = mpt {
            // Finding 332: the MPT rate snapshot is the issuance's TransferFee
            // (View.cpp:851), stored when not parity; no IssuerNode — "the
            // locked balance is already stored directly in the
            // MPTokenIssuance object" (Escrow.cpp:580-583); then
            // `rippleLockEscrowMPT`.
            use crate::tx::mpt as mp;
            let ikey = keylet::mpt_issuance_key(&mptid);
            if let Some(issuance) = mp::json_at(sandbox, &ikey) {
                let rate = mp::issuance_transfer_rate(&issuance);
                if rate != 1_000_000_000 {
                    escrow["TransferRate"] = serde_json::Value::from(rate);
                }
            }
            let r = mp::lock_escrow(sandbox, &ikey, &tx.account, want);
            if r != TxResult::Success {
                return r;
            }
        }
        sandbox.write(escrow_key, serde_json::to_vec(&escrow).expect("serializing valid JSON Value"));

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
/// The escrowed Amount off the ESCROW OBJECT as an MPT `(issuance id, value)`.
fn escrow_mpt(escrow: &serde_json::Value) -> Option<([u8; 24], u64)> {
    let amt = escrow.get("Amount")?;
    if !amt.is_object() || amt.get("mpt_issuance_id").is_none() {
        return None;
    }
    crate::tx::mpt::parse_mpt_amount(amt)
}

/// `divideRound(amount, rate, asset, roundUp=true)` for an integral (MPT)
/// asset: amount / (rate / 1e9), rounded away from zero (STAmount.cpp
/// divRoundImpl, canonicalizeRound on an integral result).
fn mpt_divide_round_up(amount: u64, rate: u64) -> u64 {
    if rate == 1_000_000_000 {
        return amount;
    }
    ((amount as u128 * 1_000_000_000u128).div_ceil(rate as u128)) as u64
}

/// Finding 332 — `escrowUnlockApplyHelper<MPTIssue>` (Escrow.cpp:955-1017),
/// the part that DECIDES before anything is written: the receiver's MPToken
/// is created when the destination finishes its own escrow (reserve at
/// OwnerCount + 1 on the prior balance, `createMPToken`, OwnerCount + 1 on
/// `dest`), a still-missing token is tecNO_PERMISSION, and the net is the
/// gross less the transfer fee at the LESSER of the snapshot and the
/// issuance's rate — issuer endpoints pay none. Returns
/// `(issuance key, net, gross)`; the caller runs `unlock_escrow`.
fn mpt_unlock_plan(
    sandbox: &mut Sandbox,
    escrow: &serde_json::Value,
    mptid: [u8; 24],
    want: u64,
    owner_id: &[u8; 20],
    dest_id: &[u8; 20],
    dest: &mut serde_json::Value,
    create_asset: bool,
    prior_balance: u64,
    locked_rate: u64,
) -> Result<(xrpl_core::types::Hash256, u64, u64), TxResult> {
    use crate::tx::mpt as mp;
    let ikey = keylet::mpt_issuance_key(&mptid);
    let Some(issuance) = mp::json_at(sandbox, &ikey) else {
        return Err(TxResult::ObjectNotFound);
    };
    let Some(issuer) = mp::issuance_issuer(&issuance) else {
        return Err(TxResult::Malformed);
    };
    let dest_is_issuer = *dest_id == issuer;
    let owner_is_issuer = *owner_id == issuer;
    let tkey = keylet::mptoken_key(&ikey, dest_id);
    if !sandbox.exists(&tkey) && create_asset && !dest_is_issuer {
        let oc = dest["OwnerCount"].as_u64().unwrap_or(0);
        if prior_balance < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
            return Err(TxResult::InsufficientReserve);
        }
        mp::create_mptoken(sandbox, &ikey, &mptid, dest_id);
        dest["OwnerCount"] = serde_json::Value::Number((oc + 1).into());
    }
    if !sandbox.exists(&tkey) && !dest_is_issuer {
        return Err(TxResult::NoPermission);
    }
    let _ = escrow;
    let mut locked = locked_rate;
    let now = mp::issuance_transfer_rate(&issuance);
    if now < locked {
        locked = now;
    }
    let net = if !owner_is_issuer && !dest_is_issuer && locked != 1_000_000_000 {
        // Finding 338 (devnet 5423379 082003AB, 100 units at a 10% fee →
        // 90): the 3.4.0 RELEASE gates the MPT fee on fixCleanup3_4_0 —
        // "MPTs are integral, so round the delivered amount down":
        // `mulRatio(amount, parity, lockedRate, false)`; before it, the
        // divideRound-up delivery (91). Mainnet has not enabled the
        // amendment (2026-09-18); the rule follows the ledger's own
        // Amendments singleton, as fixCleanup3_3_0 does.
        if crate::ledger::amendments::fix_cleanup_3_4_0(sandbox) {
            ((want as u128 * 1_000_000_000u128) / locked as u128) as u64
        } else {
            mpt_divide_round_up(want, locked)
        }
    } else {
        want
    };
    Ok((ikey, net, want))
}

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
        if tx.fee_missing() {
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
        // Finding 349 (devnet #5488110 32C544D4, an EscrowFinish naming a
        // credential that does not exist): `credentials::valid` is the FIRST
        // thing EscrowFinish::preclaim does (EscrowFinish.cpp:196-201) —
        // tecBAD_CREDENTIALS, where we fell through to tecNO_PERMISSION.
        let cv = crate::tx::credential::credentials_valid(sandbox, tx, &tx.account);
        if cv != TxResult::Success {
            return cv;
        }
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
            // rippled Escrow: a missing escrow is tecNO_TARGET, not tecNO_ENTRY
            return TxResult::NoTarget;
        }

        // Finding 332: `escrowFinishPreclaimHelper<MPTIssue>` (Escrow.cpp:
        // 741-772) — the destination, unless it is the issuer, must pass the
        // issuance's weak auth and not be frozen.
        if let Some(escrow) = crate::tx::offer::json_at(sandbox, &esc_key) {
            if let Some((mptid, _)) = escrow_mpt(&escrow) {
                use crate::tx::mpt as mp;
                let id_issuer: [u8; 20] = mptid[4..24].try_into().expect("24-byte id");
                let dest_id = escrow.get("Destination").and_then(parse_account_id).unwrap_or(owner_id);
                if dest_id != id_issuer {
                    let ikey = keylet::mpt_issuance_key(&mptid);
                    let Some(issuance) = mp::json_at(sandbox, &ikey) else {
                        return TxResult::ObjectNotFound;
                    };
                    if let Some(t) = mp::require_auth_weak(sandbox, &ikey, &issuance, &dest_id) {
                        return t;
                    }
                    if mp::any_frozen(sandbox, &ikey, &issuance, &[&dest_id]) {
                        return TxResult::Locked;
                    }
                }
            }
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
            None => return TxResult::NoTarget,
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

        // Finding 164: a token escrow's delivery, `escrowUnlockApplyHelper`
        // (EscrowHelpers.h). The receiver must hold a line unless it is the
        // sender (tecNO_LINE), the locked rate is the lesser of the escrow's
        // snapshot and the issuer's current rate, the receiver gets
        // `amount − (amount − divideRound(amount, rate, up))` when neither
        // party is the issuer, and the credit may not breach the line's
        // limit (tecLIMIT_EXCEEDED). All decided before anything is written.
        let iou_credit: Option<(crate::tx::offer::Leg, (u128, i32))> = match escrow_iou(&escrow) {
            None => None,
            Some((leg, want)) => {
                use crate::tx::offer::{div_round16_up, me_cmp, norm16, stamount_signed_add};
                let dest_is_issuer = dest_id == leg.issuer;
                let owner_is_issuer = owner_id == leg.issuer;
                let line_key = keylet::ripple_state_key(&dest_id, &leg.issuer, &leg.cur);
                let line = if dest_is_issuer { None } else { crate::tx::offer::json_at(sandbox, &line_key) };
                // Finding 340 (#107088326 7B29A3CAC3C1, rnMYFsTv finishing its
                // OWN self-escrow of 1400000 XRPL14 with no line and 1.399968
                // XRP against a reserve of 1.6): `createAsset = destID ==
                // account_` — the owner being the destination changes nothing;
                // the line is created (reserve permitting) or the finish is
                // tecNO_LINE. The `dest != owner` guard here skipped both and
                // credited a line that did not exist (5 muts v 1).
                if !dest_is_issuer && line.is_none() {
                    // Finding 266 (#106937018 D157B8D102BD): a token escrow
                    // finished BY ITS DESTINATION creates the missing line.
                    // rippled's `escrowUnlockApplyHelper<Issue>`
                    // (EscrowHelpers.h) runs `trustCreate` when
                    // `createAsset = destID == accountID_` (EscrowFinish.cpp:368)
                    // — limit zero, NoRipple per the destination's
                    // DefaultRipple, the reserve on its side, both owner
                    // directories, OwnerCount + 1 — after `checkReserve` at
                    // OwnerCount + 1 on the PRE-FEE balance
                    // (tecNO_LINE_INSUF_RESERVE) and after preclaim's Legacy
                    // `requireAuth` on the destination (EscrowFinish.cpp:152).
                    // A finisher who is not the destination still needs the
                    // line (finding 164's tecNO_LINE). 992230e1 finished its own
                    // 46245596244 BBB escrow with no BBB line; mainnet built the
                    // line (0x220000, HighLimit 0) and moved it — we said
                    // tecNO_LINE, eight objects short.
                    if dest_id != tx.account {
                        return TxResult::NoLine;
                    }
                    if let Some(t) = crate::tx::offer::require_auth_ter(sandbox, &leg, &dest_id, false) {
                        if t != TxResult::Success {
                            return t;
                        }
                    }
                    let (bal, oc) = match crate::tx::offer::json_at(sandbox, &keylet::account_root_key(&dest_id)) {
                        Some(a) => (
                            a["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
                            a["OwnerCount"].as_u64().unwrap_or(0),
                        ),
                        None => return TxResult::NoDst,
                    };
                    // fixCleanup3_4_0 (EscrowFinish.cpp:345-348): the escrow
                    // being removed no longer counts against its owner's
                    // reserve — `decreaseOwnerCountForObject(account)` runs
                    // BEFORE the delivery, so when the destination IS the owner
                    // the line's reserve is judged at OwnerCount (not + 1).
                    let recycled = crate::ledger::amendments::fix_cleanup_3_4_0(sandbox) && dest_id == owner_id;
                    let need = oc + 1 - u64::from(recycled);
                    if bal.saturating_add(tx.fee) < crate::ledger::fees::account_reserve(sandbox, need) {
                        return TxResult::NoLineInsufReserve;
                    }
                    // `line_adjust` below creates the line as `trustCreate` does.
                }
                let mut locked = escrow
                    .get("TransferRate")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1_000_000_000);
                if let Some(now) = crate::tx::offer::transfer_rate(sandbox, &leg) {
                    if now < locked {
                        locked = now;
                    }
                }
                let final_amt = if !owner_is_issuer && !dest_is_issuer && locked != 1_000_000_000 {
                    let net = div_round16_up(want, (locked as u128, -9));
                    let fee = stamount_signed_add(false, want, true, net).1;
                    norm16(stamount_signed_add(false, want, true, fee).1)
                } else {
                    want
                };
                // The limit test runs only when the finisher is NOT the receiver
                // (`if (!createAsset)`, Escrow.cpp:921).
                if let Some(line) = line.as_ref().filter(|_| dest_id != tx.account) {
                    let dest_low = dest_id < leg.issuer;
                    let limit = line[if dest_low { "LowLimit" } else { "HighLimit" }]
                        .get("value")
                        .and_then(|v| v.as_str())
                        .and_then(|s| keylet::amount_mant_exp(&serde_json::json!({"value": s})))
                        .unwrap_or((0, 0));
                    let bal = line["Balance"]["value"]
                        .as_str()
                        .and_then(|s| {
                            let neg = s.starts_with('-');
                            keylet::amount_mant_exp(&serde_json::json!({"value": s.trim_start_matches('-')}))
                                .map(|m| (neg, m))
                        })
                        .unwrap_or((false, (0, 0)));
                    // The balance is filed from the low side; the receiver's
                    // holding is that value for the low account and its negation
                    // for the high one.
                    let held_neg = if dest_low { bal.0 } else { !bal.0 };
                    let (nneg, nbal) = stamount_signed_add(held_neg, bal.1, false, final_amt);
                    if !nneg && me_cmp(nbal, limit).is_gt() {
                        return TxResult::LimitExceeded;
                    }
                }
                Some((leg, final_amt))
            }
        };

        // Finding 332: the MPT unlock is decided here (token creation, reserve,
        // rate) and applied after the destination root is written.
        let mpt_unlock = match escrow_mpt(&escrow) {
            None => None,
            Some((mptid, want)) => {
                let locked_rate = escrow.get("TransferRate").and_then(|v| v.as_u64()).unwrap_or(1_000_000_000);
                // mPriorBalance is the FINISHER's pre-fee balance; the token is
                // created only when the finisher IS the destination.
                let prior = balance_of(&dest).saturating_add(tx.fee);
                match mpt_unlock_plan(
                    sandbox, &escrow, mptid, want, &owner_id, &dest_id, &mut dest,
                    dest_id == tx.account, prior, locked_rate,
                ) {
                    Ok(plan) => Some(plan),
                    Err(t) => return t,
                }
            }
        };

        let dest_balance = balance_of(&dest);
        let new_dest_balance = match dest_balance.checked_add(amount) {
            Some(b) => b,
            None => return TxResult::Malformed,
        };
        dest["Balance"] = serde_json::Value::String(new_dest_balance.to_string());
        sandbox.write(dest_key, serde_json::to_vec(&dest).expect("serializing valid JSON Value"));
        if let Some((ikey, net, gross)) = mpt_unlock {
            let r = crate::tx::mpt::unlock_escrow(sandbox, &ikey, &owner_id, &dest_id, net, gross);
            if r != TxResult::Success {
                return r;
            }
        }

        // A TOKEN escrow RELEASES the issued currency to the destination. Same
        // hole Cancel had: `Amount` is an object, so the XRP credit above adds
        // 0 drops and the tokens simply never arrived. `EscrowCreate` locked
        // them off the sender's line and nothing ever gave them back.
        // Finding 164: net of the locked transfer rate (decided above).
        if let Some((leg, final_amt)) = iou_credit {
            crate::tx::offer::line_adjust(sandbox, &dest_id, &leg, final_amt, true);
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
        if tx.fee_missing() {
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
            // rippled Escrow: a missing escrow is tecNO_TARGET, not tecNO_ENTRY
            return TxResult::NoTarget;
        }

        // Finding 332: `escrowCancelPreclaimHelper<MPTIssue>` (Escrow.cpp:
        // 1270-1296) — the owner must pass the issuance's weak auth.
        if let Some(escrow) = crate::tx::offer::json_at(sandbox, &esc_key) {
            if let Some((mptid, _)) = escrow_mpt(&escrow) {
                use crate::tx::mpt as mp;
                let ikey = keylet::mpt_issuance_key(&mptid);
                let Some(issuance) = mp::json_at(sandbox, &ikey) else {
                    return TxResult::ObjectNotFound;
                };
                if let Some(t) = mp::require_auth_weak(sandbox, &ikey, &issuance, &owner_id) {
                    return t;
                }
            }
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
            None => return TxResult::NoTarget,
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
            // EscrowCancel.cpp:174-178 + escrowUnlockApplyHelper<Issue>: the
            // owner cancelling its OWN token escrow (`createAsset = account ==
            // accountID_`) re-creates a line it deleted meanwhile, against
            // the reserve at OwnerCount + 1 on the pre-fee balance
            // (tecNO_LINE_INSUF_RESERVE) — under fixCleanup3_4_0 the escrow's
            // own count is recycled first, so the bar is OwnerCount.
            if owner_id == tx.account
                && owner_id != leg.issuer
                && !sandbox.exists(&keylet::ripple_state_key(&owner_id, &leg.issuer, &leg.cur))
            {
                let need = if crate::ledger::amendments::fix_cleanup_3_4_0(sandbox) { oc } else { oc + 1 };
                if owner_balance.saturating_add(tx.fee) < crate::ledger::fees::account_reserve(sandbox, need) {
                    return TxResult::NoLineInsufReserve;
                }
            }
            crate::tx::offer::line_adjust(sandbox, &owner_id, &leg, want, true);
        }
        // Finding 332: an MPT escrow unlocks back onto the owner's own token
        // at parity — sender and receiver are the same (Escrow.cpp:1407-1424).
        if let Some((mptid, want)) = escrow_mpt(&escrow) {
            let mut owner_json = crate::tx::offer::json_at(sandbox, &owner_key).unwrap_or_default();
            let prior = balance_of(&owner_json).saturating_add(tx.fee);
            match mpt_unlock_plan(
                sandbox, &escrow, mptid, want, &owner_id, &owner_id, &mut owner_json,
                owner_id == tx.account, prior, 1_000_000_000,
            ) {
                Ok((ikey, net, gross)) => {
                    sandbox.write(owner_key, serde_json::to_vec(&owner_json).expect("serializing valid JSON Value"));
                    let r = crate::tx::mpt::unlock_escrow(sandbox, &ikey, &owner_id, &owner_id, net, gross);
                    if r != TxResult::Success {
                        return r;
                    }
                }
                Err(t) => return t,
            }
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
            inner_batch: false,
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
            inner_batch: false,
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
            inner_batch: false,
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
                // No FinishAfter, no Condition — and no CancelAfter either, so
                // rippled's first rule answers: temBAD_EXPIRATION (finding 322).
            }),
            inner_batch: false,
        };
        assert_eq!(EscrowCreateTransactor.preflight(&tx), TxResult::BadExpiration);
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
            inner_batch: false,
        };

        let sandbox = Sandbox::new(&state);
        assert_eq!(
            EscrowCreateTransactor.preclaim(&tx, &sandbox),
            TxResult::InsufficientReserve
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
            inner_batch: false,
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

    /// Finding 332 fixtures: an issuance (CanEscrow|CanTransfer, TransferFee
    /// as given) held 500 by `holder`; `dest` holds an empty token.
    fn mpt_fixture(state: &mut LedgerState, issuer: &[u8; 20], holder: &[u8; 20], dest: &[u8; 20], fee: Option<u64>) -> [u8; 24] {
        let mut id = [0u8; 24];
        id[..4].copy_from_slice(&7u32.to_be_bytes());
        id[4..].copy_from_slice(issuer);
        let ikey = keylet::mpt_issuance_key(&id);
        let mut iss = serde_json::json!({
            "LedgerEntryType": "MPTokenIssuance",
            "Issuer": hex::encode(issuer),
            "Flags": crate::tx::mpt::LSF_MPT_CAN_ESCROW | crate::tx::mpt::LSF_MPT_CAN_TRANSFER,
            "Sequence": 7,
            "OutstandingAmount": "500",
            "OwnerNode": "0",
        });
        if let Some(f) = fee {
            iss["TransferFee"] = serde_json::Value::from(f);
        }
        state.state_map.insert(ikey, serde_json::to_vec(&iss).unwrap()).unwrap();
        for (who, bal) in [(holder, Some("500")), (dest, None)] {
            let mut t = serde_json::json!({
                "LedgerEntryType": "MPToken",
                "Account": hex::encode(who),
                "MPTokenIssuanceID": hex::encode_upper(id),
                "Flags": 0,
                "OwnerNode": "0",
            });
            if let Some(b) = bal {
                t["MPTAmount"] = serde_json::Value::String(b.into());
            }
            state.state_map.insert(keylet::mptoken_key(&ikey, who), serde_json::to_vec(&t).unwrap()).unwrap();
        }
        id
    }

    fn mpt_json(sandbox: &Sandbox, key: &Hash256) -> serde_json::Value {
        serde_json::from_slice(&sandbox.read(key).expect("object")).unwrap()
    }

    fn run_tx<'a>(state: &'a LedgerState, t: &dyn Transactor, tx: &TxFields) -> (TxResult, Sandbox<'a>) {
        let mut sb = Sandbox::new(state);
        let r = t.preflight(tx);
        if r != TxResult::Success {
            return (r, sb);
        }
        let r = t.preclaim(tx, &sb);
        if r != TxResult::Success {
            return (r, sb);
        }
        assert_eq!(apply_common(tx, &mut sb), TxResult::Success);
        (t.do_apply(tx, &mut sb), sb)
    }

    /// Finding 332: create locks the holder's token and the issuance; cancel
    /// by the owner puts it all back — LockedAmount absent again at zero.
    #[test]
    fn an_mpt_escrow_locks_on_create_and_unlocks_on_cancel() {
        let issuer = [0x0Au8; 20];
        let holder = [0x0Bu8; 20];
        let dest = [0x0Cu8; 20];
        let mut state = make_state();
        add_account(&mut state, &issuer, 100_000_000, 5);
        add_account(&mut state, &holder, 100_000_000, 3);
        add_account(&mut state, &dest, 100_000_000, 9);
        let id = mpt_fixture(&mut state, &issuer, &holder, &dest, None);
        let ikey = keylet::mpt_issuance_key(&id);
        let hkey = keylet::mptoken_key(&ikey, &holder);
        let create = TxFields {
            account: holder, tx_type: "EscrowCreate".into(), fee: 12, sequence: 3, last_ledger_seq: None, ticket_seq: None,
            fields: serde_json::json!({
                "Amount": {"mpt_issuance_id": hex::encode_upper(id), "value": "100"},
                "Destination": hex::encode(dest), "FinishAfter": 15, "CancelAfter": 20,
            }),
            inner_batch: false,
        };
        let (r, sb) = run_tx(&state, &EscrowCreateTransactor, &create);
        assert_eq!(r, TxResult::Success);
        let tok = mpt_json(&sb, &hkey);
        assert_eq!(tok["MPTAmount"], "400");
        assert_eq!(tok["LockedAmount"], "100");
        let iss = mpt_json(&sb, &ikey);
        assert_eq!(iss["LockedAmount"], "100");
        assert_eq!(iss["OutstandingAmount"], "500");
        let esc = mpt_json(&sb, &keylet::escrow_key(&holder, 3));
        assert!(esc.get("IssuerNode").is_none(), "MPT escrows carry no IssuerNode");
        assert!(esc.get("TransferRate").is_none(), "parity rate is not snapshotted");
        let mods = sb.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        let cancel = TxFields {
            account: holder, tx_type: "EscrowCancel".into(), fee: 12, sequence: 4, last_ledger_seq: None, ticket_seq: None,
            fields: serde_json::json!({"Owner": hex::encode(holder), "OfferSequence": 3}),
            inner_batch: false,
        };
        let (r, sb) = run_tx(&state, &EscrowCancelTransactor, &cancel);
        assert_eq!(r, TxResult::Success);
        let tok = mpt_json(&sb, &hkey);
        assert_eq!(tok["MPTAmount"], "500");
        assert!(tok.get("LockedAmount").is_none());
        let iss = mpt_json(&sb, &ikey);
        assert!(iss.get("LockedAmount").is_none());
        assert_eq!(iss["OutstandingAmount"], "500");
        assert!(!sb.exists(&keylet::escrow_key(&holder, 3)));
    }

    /// Finding 332: with a 10% TransferFee the create snapshots the rate
    /// (1.1e9) and the finish delivers divideRound(100, 1.1, up) = 91, the
    /// 9-unit fee leaving OutstandingAmount; the holder's lock drops by the
    /// gross 100.
    #[test]
    fn an_mpt_escrow_finish_takes_the_transfer_fee_off_the_outstanding_amount() {
        let issuer = [0x0Au8; 20];
        let holder = [0x0Bu8; 20];
        let dest = [0x0Cu8; 20];
        let mut state = make_state();
        add_account(&mut state, &issuer, 100_000_000, 5);
        add_account(&mut state, &holder, 100_000_000, 3);
        add_account(&mut state, &dest, 100_000_000, 9);
        let id = mpt_fixture(&mut state, &issuer, &holder, &dest, Some(10_000));
        let ikey = keylet::mpt_issuance_key(&id);
        let create = TxFields {
            account: holder, tx_type: "EscrowCreate".into(), fee: 12, sequence: 3, last_ledger_seq: None, ticket_seq: None,
            fields: serde_json::json!({
                "Amount": {"mpt_issuance_id": hex::encode_upper(id), "value": "100"},
                "Destination": hex::encode(dest), "FinishAfter": 15,
            }),
            inner_batch: false,
        };
        let (r, sb) = run_tx(&state, &EscrowCreateTransactor, &create);
        assert_eq!(r, TxResult::Success);
        assert_eq!(mpt_json(&sb, &keylet::escrow_key(&holder, 3))["TransferRate"], 1_100_000_000u64);
        let mods = sb.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        state.header.close_time = 20; // past FinishAfter
        let finish = TxFields {
            account: dest, tx_type: "EscrowFinish".into(), fee: 12, sequence: 9, last_ledger_seq: None, ticket_seq: None,
            fields: serde_json::json!({"Owner": hex::encode(holder), "OfferSequence": 3}),
            inner_batch: false,
        };
        let (r, sb) = run_tx(&state, &EscrowFinishTransactor, &finish);
        assert_eq!(r, TxResult::Success);
        assert_eq!(mpt_json(&sb, &keylet::mptoken_key(&ikey, &dest))["MPTAmount"], "91");
        let htok = mpt_json(&sb, &keylet::mptoken_key(&ikey, &holder));
        assert_eq!(htok["MPTAmount"], "400");
        assert!(htok.get("LockedAmount").is_none());
        let iss = mpt_json(&sb, &ikey);
        assert!(iss.get("LockedAmount").is_none());
        assert_eq!(iss["OutstandingAmount"], "491");
        assert!(!sb.exists(&keylet::escrow_key(&holder, 3)));

        // Finding 338: with fixCleanup3_4_0 enabled in the ledger's
        // Amendments singleton the delivery floors — 90, fee 10.
        let am = serde_json::json!({
            "LedgerEntryType": "Amendments", "Flags": 0,
            "Amendments": [crate::ledger::amendments::FIX_CLEANUP_3_4_0],
        });
        state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&am).unwrap()).unwrap();
        let (r, sb) = run_tx(&state, &EscrowFinishTransactor, &finish);
        assert_eq!(r, TxResult::Success);
        assert_eq!(mpt_json(&sb, &keylet::mptoken_key(&ikey, &dest))["MPTAmount"], "90");
        assert_eq!(mpt_json(&sb, &ikey)["OutstandingAmount"], "490");
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
            inner_batch: false,
        };

        let sandbox = Sandbox::new(&state);
        assert_eq!(
            EscrowFinishTransactor.preclaim(&tx, &sandbox),
            TxResult::NoTarget
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
            inner_batch: false,
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
            inner_batch: false,
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
            inner_batch: false,
        };
        assert_eq!(EscrowCancelTransactor.preflight(&tx), TxResult::Malformed);
    }

    fn enable_fix340(state: &mut LedgerState) {
        let am = serde_json::json!({
            "LedgerEntryType": "Amendments", "Flags": 0,
            "Amendments": [crate::ledger::amendments::FIX_CLEANUP_3_4_0],
        });
        state.state_map.insert(crate::ledger::keylet::amendments_key(), serde_json::to_vec(&am).unwrap()).unwrap();
    }

    /// fixCleanup3_4_0 (EscrowFinish.cpp:345-348): finishing one's OWN
    /// self-escrow onto a line deleted meanwhile — the escrow's reserve is
    /// recycled first, so the new line is judged at OwnerCount, not + 1.
    #[test]
    fn escrow_finish_self_escrow_reserve_is_recycled_under_fixcleanup340() {
        let sender = [0x01u8; 20];
        let issuer = [0x03u8; 20];
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "STS", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        add_account(&mut state, &issuer, 50_000_000, 1);
        {
            let ikey = keylet::account_root_key(&issuer);
            let mut ia: serde_json::Value = serde_json::from_slice(&Sandbox::new(&state).read(&ikey).unwrap()).unwrap();
            ia["Flags"] = serde_json::json!(0x4000_0000u64);
            state.state_map.insert(ikey, serde_json::to_vec(&ia).unwrap()).unwrap();
        }
        let (lo, hi) = if sender < issuer { (sender, issuer) } else { (issuer, sender) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                        "value": if sender < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
        });
        let lkey = keylet::ripple_state_key(&sender, &issuer, &cur);
        state.state_map.insert(lkey, serde_json::to_vec(&line).unwrap()).unwrap();
        let create = TxFields {
            account: sender, tx_type: "EscrowCreate".to_string(), fee: 12, sequence: 1,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(sender),
                "Amount": {"currency": "STS", "issuer": hex::encode(issuer), "value": "30"},
                "FinishAfter": 15,
            }),
            inner_batch: false,
        };
        let (r, sb) = run_tx(&state, &EscrowCreateTransactor, &create);
        assert_eq!(r, TxResult::Success);
        let mods = sb.modifications().clone();
        drop(sb);
        // The finish happens in a later ledger: past FinishAfter.
        state.header.close_time = 20;
        // Fold the create into the base, then delete the line (the owner
        // dropped it while the escrow was pending) and pin the XRP balance at
        // exactly the reserve for ONE object plus the fee.
        for (k, e) in mods {
            match e {
                crate::ledger::sandbox::SandboxEntry::Created(b) | crate::ledger::sandbox::SandboxEntry::Modified(b) => {
                    state.state_map.insert(k, b).unwrap();
                }
                crate::ledger::sandbox::SandboxEntry::Deleted => {
                    state.state_map.delete(&k).unwrap();
                }
            }
        }
        state.state_map.delete(&lkey).unwrap();
        let reserve_one = crate::ledger::fees::account_reserve(&Sandbox::new(&state), 1);
        {
            let akey = keylet::account_root_key(&sender);
            let mut a: serde_json::Value = serde_json::from_slice(&Sandbox::new(&state).read(&akey).unwrap()).unwrap();
            assert_eq!(a["OwnerCount"].as_u64(), Some(1), "the escrow is the owner's one object");
            a["Balance"] = serde_json::Value::String((reserve_one + 12).to_string());
            state.state_map.insert(akey, serde_json::to_vec(&a).unwrap()).unwrap();
        }
        let finish = TxFields {
            account: sender, tx_type: "EscrowFinish".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({"Owner": hex::encode(sender), "OfferSequence": 1}),
            inner_batch: false,
        };
        let (r, _) = run_tx(&state, &EscrowFinishTransactor, &finish);
        assert_eq!(r, TxResult::NoLineInsufReserve, "pre-amendment: the escrow still counts, the line needs reserve for two");
        enable_fix340(&mut state);
        let (r, sb) = run_tx(&state, &EscrowFinishTransactor, &finish);
        assert_eq!(r, TxResult::Success, "recycled: the line takes the escrow's place");
        assert!(sb.exists(&lkey), "the line was re-created");
    }

}
