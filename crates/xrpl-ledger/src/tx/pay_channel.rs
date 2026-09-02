//! Payment Channel transaction types: PaymentChannelCreate, PaymentChannelClaim,
//! PaymentChannelFund.
//!
//! Payment channels allow fast, off-ledger XRP transfers. The sender locks XRP
//! into a channel object, and the destination can claim from it using signed
//! authorizations. The sender can add more XRP to the channel.
//!
//! PaymentChannelCreate: Lock XRP into a new channel at pay_channel_key(account, sequence).
//! PaymentChannelClaim:  Claim XRP from a channel. If fully claimed, delete channel.
//! PaymentChannelFund:   Add more XRP to an existing channel.
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

/// Extract a 20-byte account ID from a hex string field.
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

/// Extract an XRP amount in drops from a string or number value.
fn parse_drops(val: &serde_json::Value) -> Option<u64> {
    match val {
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// A u64 directory hint stored as a JSON number or (rippled's format) an
/// unpadded hex string.
fn parse_hint(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(|v| {
        v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
    })
}

/// `isChannelExpired` (PaymentChannelHelpers.cpp:89): a PRESENT time field the
/// parent close time has passed. fixCleanup3_2_0's `after()` compare is STRICT
/// (`close > t`) — unlike offer `hasExpired`'s inclusive `>=`. A present zero
/// is expired (rippled tests field presence, not value).
fn is_channel_expired(sandbox: &Sandbox, t: Option<u64>) -> bool {
    t.is_some_and(|t| (sandbox.base().header.close_time as u64) > t)
}

/// `closeChannel` (PaymentChannelHelpers.cpp:26): unlink the channel from the
/// SOURCE's owner directory (keep_root=true, rippled passes keepRoot), and
/// from the DESTINATION's when sfDestinationNode is present (channels made
/// after fixPayChanRecipientOwnerDir live in both); refund Amount − Balance to
/// the source; OwnerCount −1 on the source only; erase the channel.
fn close_channel(
    sandbox: &mut Sandbox,
    channel_key: &xrpl_core::types::Hash256,
    channel: &serde_json::Value,
) -> TxResult {
    let Some(src) = channel.get("Account").and_then(parse_account_id) else {
        return TxResult::Malformed;
    };
    let owner_node = parse_hint(channel.get("OwnerNode"));
    crate::ledger::directory::owner_dir_remove(sandbox, &src, channel_key, owner_node, true);
    if channel.get("DestinationNode").is_some() {
        if let Some(dst) = channel.get("Destination").and_then(parse_account_id) {
            let hint = parse_hint(channel.get("DestinationNode"));
            crate::ledger::directory::owner_dir_remove(sandbox, &dst, channel_key, hint, true);
        }
    }
    let amount = channel.get("Amount").and_then(parse_drops).unwrap_or(0);
    let balance = channel.get("Balance").and_then(parse_drops).unwrap_or(0);
    let src_key = keylet::account_root_key(&src);
    if let Some(mut acct) = sandbox
        .read(&src_key)
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
    {
        let bal = acct["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        acct["Balance"] =
            serde_json::Value::String((bal + amount.saturating_sub(balance)).to_string());
        sandbox.write(src_key, serde_json::to_vec(&acct).unwrap_or_default());
    }
    crate::tx::offer::owner_count_add(sandbox, &src, -1);
    sandbox.delete(*channel_key);
    TxResult::Success
}

// ---------------------------------------------------------------------------
// PaymentChannelCreate
// ---------------------------------------------------------------------------

/// PaymentChannelCreate transactor — lock XRP into a new payment channel.
pub struct PaymentChannelCreateTransactor;

impl Transactor for PaymentChannelCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PaymentChannelCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Destination").is_none() {
            return TxResult::Malformed;
        }
        let amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        if amount == 0 {
            return TxResult::BadAmount;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }

        // Check sender has enough balance for Amount + fee
        if let Some(data) = sandbox.read(&acct_key) {
            if let Ok(acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                let balance = acct["Balance"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let amount = tx.fields.get("Amount").and_then(|a| parse_drops(a)).unwrap_or(0);
                // F85 — PayChanCreate::preclaim (PayChan.cpp:205-218): the
                // reserve is for OwnerCount + 1 (the channel to be created);
                // balance < reserve is tecINSUFFICIENT_RESERVE, balance <
                // reserve + amount is tecUNFUNDED. #106703535 F44C919F: 1000
                // drops from an account below its next reserve — mainnet
                // refuses, we created the channel.
                let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
                let reserve = crate::ledger::fees::account_reserve(sandbox, oc + 1);
                if balance < reserve {
                    return TxResult::InsufficientReserve;
                }
                if balance < reserve.checked_add(amount).unwrap_or(u64::MAX) {
                    return TxResult::Unfunded;
                }
            }
        }

        // Destination must exist, and may insist on a tag.
        // `PaymentChannelCreate::preclaim` (:96-104): tecNO_DST, then
        // lsfDisallowIncomingPayChan -> tecNO_PERMISSION, then
        // lsfRequireDestTag with no DestinationTag -> tecDST_TAG_NEEDED.
        // Found by grepping every transactor that emits tecDST_TAG_NEEDED
        // after #106143718 showed EscrowCreate missing it: rippled has the
        // check in SEVEN transactors and we had it in two.
        // ⚠ No failing ledger pins THIS one — it is here because rippled's
        // condition is unambiguous and identical to the form already proven
        // in `check.rs`.
        if let Some(dest) = tx.fields.get("Destination").and_then(|d| parse_account_id(d)) {
            let dest_key = keylet::account_root_key(&dest);
            let Some(dst) = sandbox
                .read(&dest_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else {
                return TxResult::NoDst;
            };
            let dflags = dst["Flags"].as_u64().unwrap_or(0);
            // The flag test the comment above has always CLAIMED and the code
            // never made: a destination may refuse paychans outright, and that
            // ruling comes BEFORE the tag test
            // (PaymentChannelCreate.cpp:99-101).
            if dflags & 0x1000_0000 != 0 {
                return TxResult::NoPermission; // lsfDisallowIncomingPayChan
            }
            if dflags & 0x0002_0000 != 0 && tx.fields.get("DestinationTag").is_none() {
                return TxResult::DstTagNeeded; // lsfRequireDestTag
            }
            // A PSEUDO-ACCOUNT cannot receive a payment channel — same three
            // discriminators as everywhere else (`sfAMMID`, `sfVaultID`,
            // `sfLoanBrokerID`; AccountRootHelpers.cpp:194-208), and likewise
            // NOT amendment-gated because every write to those fields is.
            //
            // ⚠ ORDER DIFFERS FROM CheckCreate. There the pseudo test sits
            // BEFORE the tag test (CheckCreate.cpp:93-98); here it sits AFTER
            // it (PaymentChannelCreate.cpp:103-113). A pseudo-account
            // destination that also carries lsfRequireDestTag therefore yields
            // tecNO_PERMISSION for a Check and tecDST_TAG_NEEDED for a
            // PayChannel. Copying one transactor's order into the other is the
            // easy way to be wrong here.
            //
            // ⚠ No failing ledger pins this — it is the sibling of the escrow
            // rule (`8e48764`), written because rippled applies `isPseudoAccount`
            // in five places and we had implemented two.
            if ["AMMID", "VaultID", "LoanBrokerID"].iter().any(|f| dst.get(f).is_some()) {
                return TxResult::NoPermission;
            }
        } else {
            return TxResult::Malformed;
        }

        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        let dest = match tx.fields.get("Destination").and_then(|d| parse_account_id(d)) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };

        // Deduct Amount from sender
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender_acct: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let sender_balance = sender_acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if sender_balance < amount {
            return TxResult::Unfunded;
        }

        sender_acct["Balance"] =
            serde_json::Value::String((sender_balance - amount).to_string());

        // Increment OwnerCount
        let owner_count = sender_acct["OwnerCount"].as_u64().unwrap_or(0);
        sender_acct["OwnerCount"] = serde_json::Value::Number((owner_count + 1).into());
        sandbox.write(sender_key, serde_json::to_vec(&sender_acct).unwrap());

        // fixPayChanCancelAfter: a CancelAfter already past at create time is
        // refused outright (tecEXPIRED) — checked before any state moves.
        if let Some(ca) = tx.fields.get("CancelAfter").and_then(|v| v.as_u64()) {
            if (sandbox.base().header.close_time as u64) > ca {
                return TxResult::Expired;
            }
        }

        // Create PayChannel object. keylet::payChan hashes src || DST || seq.
        let seq = if tx.uses_ticket() {
            tx.ticket_seq.unwrap_or(0)
        } else {
            tx.sequence
        };
        let channel_key = keylet::pay_channel_key(&tx.account, &dest, seq);

        // The channel lives in BOTH owner directories (the destination's via
        // fixPayChanRecipientOwnerDir), and the object stores both hints —
        // closeChannel unlinks through them. Same rule the offer hints fix
        // established: an object WE create must carry everything a later tx
        // reads off a hydrated one.
        let owner_node =
            crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &channel_key);
        let dest_node = crate::ledger::directory::owner_dir_insert(sandbox, &dest, &channel_key);

        let mut channel_obj = serde_json::json!({
            "LedgerEntryType": "PayChannel",
            "Flags": 0,
            "Account": hex::encode(tx.account),
            "Destination": hex::encode(dest),
            "Amount": amount.to_string(),
            "Balance": "0",
            "Sequence": seq,
            "OwnerNode": format!("{owner_node:x}"),
            "DestinationNode": format!("{dest_node:x}"),
        });
        if let Some(sd) = tx.fields.get("SettleDelay") {
            channel_obj["SettleDelay"] = sd.clone();
        }
        if let Some(pk) = tx.fields.get("PublicKey") {
            channel_obj["PublicKey"] = pk.clone();
        }
        for f in ["CancelAfter", "SourceTag", "DestinationTag"] {
            if let Some(v) = tx.fields.get(f) {
                channel_obj[f] = v.clone();
            }
        }
        sandbox.write(channel_key, serde_json::to_vec(&channel_obj).unwrap());

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// PaymentChannelClaim
// ---------------------------------------------------------------------------

/// PaymentChannelClaim transactor — claim XRP from a payment channel.
/// If the channel is fully claimed, it is deleted.
pub struct PaymentChannelClaimTransactor;

impl Transactor for PaymentChannelClaimTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PaymentChannelClaim" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // fixCleanup3_2_0: an all-zero Channel is malformed.
        match tx.fields.get("Channel").and_then(|v| v.as_str()) {
            None => return TxResult::Malformed,
            Some(s) if s.chars().all(|c| c == '0') => return TxResult::Malformed,
            _ => {}
        }
        // Balance/Amount must be XRP and positive; Balance may not exceed
        // Amount (PaymentChannelClaim.cpp preflight). A non-string amount
        // JSON is an IOU — not XRP.
        let bal = tx.fields.get("Balance");
        let amt = tx.fields.get("Amount");
        for v in [bal, amt].into_iter().flatten() {
            match parse_drops(v) {
                Some(d) if d > 0 && v.is_string() => {}
                _ => return TxResult::BadAmount,
            }
        }
        if let (Some(b), Some(a)) = (bal.and_then(parse_drops), amt.and_then(parse_drops)) {
            if b > a {
                return TxResult::BadAmount;
            }
        }
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if flags & 0x0001_0000 != 0 && flags & 0x0002_0000 != 0 {
            return TxResult::Malformed; // tfRenew + tfClose together
        }
        // A Signature requires both PublicKey and Balance. The signature
        // itself is NOT re-verified: every replayed tx already passed
        // mainnet validation, and preflight crypto cannot change the meta.
        if tx.fields.get("Signature").is_some()
            && (tx.fields.get("PublicKey").is_none() || bal.is_none())
        {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Decode Channel (hex of the 32-byte keylet)
        let channel_hex = match tx.fields.get("Channel").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let channel_bytes = match hex::decode(channel_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => return TxResult::Malformed,
        };
        let mut channel_key_arr = [0u8; 32];
        channel_key_arr.copy_from_slice(&channel_bytes);
        let channel_key = xrpl_core::types::Hash256(channel_key_arr);

        // Read the PayChannel object. Claim's missing-channel code is
        // tecNO_TARGET (Fund's is tecNO_ENTRY — rippled keeps them distinct).
        let channel_data = match sandbox.read(&channel_key) {
            Some(d) => d,
            None => return TxResult::NoTarget,
        };
        let mut channel: serde_json::Value = match serde_json::from_slice(&channel_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let dest = match channel.get("Destination").and_then(parse_account_id) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let creator = match channel.get("Account").and_then(parse_account_id) {
            Some(c) => c,
            None => return TxResult::Malformed,
        };

        // EXPIRY FIRST, before the permission gate: ANY claimant — even a
        // stranger to the channel — closes an expired one
        // (PaymentChannelClaim.cpp:123-127).
        let cancel_after = channel.get("CancelAfter").and_then(|v| v.as_u64());
        let cur_expiration = channel.get("Expiration").and_then(|v| v.as_u64());
        if is_channel_expired(sandbox, cancel_after) || is_channel_expired(sandbox, cur_expiration)
        {
            return close_channel(sandbox, &channel_key, &channel);
        }

        if tx.account != dest && tx.account != creator {
            return TxResult::NoPermission;
        }

        // Balance arm — only when the tx carries one. A pure tfClose/tfRenew
        // claim has no Balance at all (A4E67EFD: Flags=131072, nothing else).
        if tx.fields.get("Balance").is_some() {
            let req_balance = match tx.fields.get("Balance").and_then(parse_drops) {
                Some(b) => b,
                None => return TxResult::Malformed,
            };
            let chan_balance = channel.get("Balance").and_then(parse_drops).unwrap_or(0);
            let chan_funds = channel.get("Amount").and_then(parse_drops).unwrap_or(0);
            let has_sig = tx.fields.get("Signature").is_some();

            // The destination may only claim WITH a channel authorization;
            // a signature must be by the CHANNEL's key (both tecNO_PERMISSION
            // under fixCleanup3_2_0; pre-fix they were tem codes).
            if tx.account == dest && !has_sig {
                return TxResult::NoPermission;
            }
            if has_sig {
                let tx_pk = tx.fields.get("PublicKey").and_then(|v| v.as_str()).unwrap_or("");
                let ch_pk = channel.get("PublicKey").and_then(|v| v.as_str()).unwrap_or("");
                if !tx_pk.eq_ignore_ascii_case(ch_pk) {
                    return TxResult::NoPermission;
                }
            }

            if req_balance > chan_funds {
                return TxResult::UnfundedPayment;
            }
            if req_balance <= chan_balance {
                // nothing newly requested
                return TxResult::UnfundedPayment;
            }

            let dest_key = keylet::account_root_key(&dest);
            let Some(mut dest_acct) = sandbox
                .read(&dest_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else {
                return TxResult::NoDst;
            };

            // verifyDepositPreauth, basic arm (credential-granted preauth is
            // unported — no specimen exercises it).
            if dest_acct["Flags"].as_u64().unwrap_or(0) & 0x0100_0000 != 0
                && tx.account != dest
                && sandbox.read(&keylet::deposit_preauth_key(&dest, &tx.account)).is_none()
            {
                return TxResult::NoPermission;
            }

            channel["Balance"] = serde_json::Value::String(req_balance.to_string());
            let dbal =
                dest_acct["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            dest_acct["Balance"] =
                serde_json::Value::String((dbal + (req_balance - chan_balance)).to_string());
            sandbox.write(dest_key, serde_json::to_vec(&dest_acct).unwrap_or_default());
            sandbox.write(channel_key, serde_json::to_vec(&channel).unwrap_or_default());
        }

        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);

        if flags & 0x0001_0000 != 0 {
            // tfRenew: source only; clears any scheduled Expiration.
            if creator != tx.account {
                return TxResult::NoPermission;
            }
            if let Some(o) = channel.as_object_mut() {
                o.remove("Expiration");
            }
            sandbox.write(channel_key, serde_json::to_vec(&channel).unwrap_or_default());
        }

        if flags & 0x0002_0000 != 0 {
            // tfClose: immediate when the receiver asks or the channel is dry
            // (Balance == Amount, AFTER any Balance arm above); otherwise
            // schedule Expiration = parentCloseTime + SettleDelay, taking the
            // EARLIER of that and any existing Expiration.
            let chan_balance = channel.get("Balance").and_then(parse_drops).unwrap_or(0);
            let chan_funds = channel.get("Amount").and_then(parse_drops).unwrap_or(0);
            if dest == tx.account || chan_balance == chan_funds {
                return close_channel(sandbox, &channel_key, &channel);
            }
            let settle_delay = channel.get("SettleDelay").and_then(|v| v.as_u64()).unwrap_or(0);
            // saturatingAdd clamps at u32::MAX under fixCleanup3_2_0.
            let settle_exp = ((sandbox.base().header.close_time as u64) + settle_delay)
                .min(u32::MAX as u64);
            let cur = channel.get("Expiration").and_then(|v| v.as_u64());
            if cur.is_none() || cur.is_some_and(|c| c > settle_exp) {
                channel["Expiration"] = serde_json::Value::Number(settle_exp.into());
                sandbox.write(channel_key, serde_json::to_vec(&channel).unwrap_or_default());
            }
        }

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// PaymentChannelFund
// ---------------------------------------------------------------------------

/// PaymentChannelFund transactor — add more XRP to an existing payment channel.
pub struct PaymentChannelFundTransactor;

impl Transactor for PaymentChannelFundTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "PaymentChannelFund" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("Channel").is_none() {
            return TxResult::Malformed;
        }
        let amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        if amount == 0 {
            return TxResult::BadAmount;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let add_amount = match tx.fields.get("Amount").and_then(|a| parse_drops(a)) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // Decode Channel
        let channel_hex = match tx.fields.get("Channel").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return TxResult::Malformed,
        };
        let channel_bytes = match hex::decode(channel_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => return TxResult::Malformed,
        };
        let mut channel_key_arr = [0u8; 32];
        channel_key_arr.copy_from_slice(&channel_bytes);
        let channel_key = xrpl_core::types::Hash256(channel_key_arr);

        // Read channel
        let channel_data = match sandbox.read(&channel_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let mut channel: serde_json::Value = match serde_json::from_slice(&channel_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // EXPIRY FIRST (PaymentChannelFund.cpp): funding an expired channel
        // closes it instead — from ANY sender, before the owner-only gate.
        let cancel_after = channel.get("CancelAfter").and_then(|v| v.as_u64());
        let cur_expiration = channel.get("Expiration").and_then(|v| v.as_u64());
        if is_channel_expired(sandbox, cancel_after) || is_channel_expired(sandbox, cur_expiration)
        {
            return close_channel(sandbox, &channel_key, &channel);
        }

        // Only the channel creator can fund it or extend it
        let creator = match channel.get("Account").and_then(parse_account_id) {
            Some(c) => c,
            None => return TxResult::Malformed,
        };
        if creator != tx.account {
            return TxResult::NoPermission;
        }

        // Optional Expiration extension: may not come below
        // min(parentCloseTime + SettleDelay, current Expiration) — violation
        // is tecNO_PERMISSION under fixCleanup3_2_0 (was temBAD_EXPIRATION).
        if let Some(new_exp) = tx.fields.get("Expiration").and_then(|v| v.as_u64()) {
            let settle_delay = channel.get("SettleDelay").and_then(|v| v.as_u64()).unwrap_or(0);
            let mut min_exp = ((sandbox.base().header.close_time as u64) + settle_delay)
                .min(u32::MAX as u64);
            if let Some(c) = cur_expiration {
                if c < min_exp {
                    min_exp = c;
                }
            }
            if new_exp < min_exp {
                return TxResult::NoPermission;
            }
            channel["Expiration"] = serde_json::Value::Number(new_exp.into());
            sandbox.write(channel_key, serde_json::to_vec(&channel).unwrap_or_default());
        }

        // Reserve then funds (PaymentChannelFund.cpp): balance < reserve is
        // tecINSUFFICIENT_RESERVE; balance < reserve + amount is tecUNFUNDED.
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender_acct: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let sender_balance = sender_acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let oc = sender_acct["OwnerCount"].as_u64().unwrap_or(0);
        let reserve = crate::ledger::fees::account_reserve(sandbox, oc);
        if sender_balance < reserve {
            return TxResult::InsufficientReserve;
        }
        if sender_balance < reserve.saturating_add(add_amount) {
            return TxResult::Unfunded;
        }

        // Funding a channel whose destination is gone is refused.
        if let Some(dst) = channel.get("Destination").and_then(parse_account_id) {
            if !sandbox.exists(&keylet::account_root_key(&dst)) {
                return TxResult::NoDst;
            }
        }

        sender_acct["Balance"] =
            serde_json::Value::String((sender_balance - add_amount).to_string());
        sandbox.write(sender_key, serde_json::to_vec(&sender_acct).unwrap());

        // Increase channel Amount (checked to prevent overflow)
        let channel_amount = channel.get("Amount")
            .and_then(|a| parse_drops(a))
            .unwrap_or(0);
        let new_channel_amount = match channel_amount.checked_add(add_amount) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };
        channel["Amount"] =
            serde_json::Value::String(new_channel_amount.to_string());
        sandbox.write(channel_key, serde_json::to_vec(&channel).unwrap());

        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn make_state_with_accounts(
        accounts: &[(&[u8; 20], u64)],
    ) -> LedgerState {
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
        let mut state = LedgerState::new_unverified(header);
        for (id, balance) in accounts {
            let acct = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(id),
                "Balance": balance.to_string(),
                "Sequence": 1,
                "OwnerCount": 0,
            });
            let key = keylet::account_root_key(id);
            state
                .state_map
                .insert(key, serde_json::to_vec(&acct).unwrap())
                .unwrap();
        }
        state
    }

    fn read_field_u64(sandbox: &Sandbox, account: &[u8; 20], field: &str) -> u64 {
        let key = keylet::account_root_key(account);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        v[field]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| v[field].as_u64())
            .unwrap_or(0)
    }

    /// A destination may refuse payment channels outright, and a pseudo-account
    /// can never receive one (PaymentChannelCreate.cpp:99-113).
    ///
    /// ⚠ The ORDER differs from CheckCreate: there the pseudo test precedes the
    /// tag test, here it follows it. The third case pins that — a pseudo-account
    /// that ALSO requires a tag yields tecDST_TAG_NEEDED for a PayChannel, where
    /// a Check would yield tecNO_PERMISSION.
    #[test]
    fn pay_channel_create_refused_by_flag_and_by_pseudo_account() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "10000000",
                "PublicKey": "030FBA552C9626B1DECA8CFAD9F2121DCA55C1066928210397EDCF4F625F6E272C",
            }),
        };
        let build = |mutate: &dyn Fn(&mut serde_json::Value)| {
            let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);
            let dkey = keylet::account_root_key(&dest);
            let mut acct: serde_json::Value =
                serde_json::from_slice(state.state_map.lookup(&dkey).unwrap()).unwrap();
            mutate(&mut acct);
            let mut state = state;
            state.state_map.insert(dkey, serde_json::to_vec(&acct).unwrap()).unwrap();
            state
        };

        // Control: an ordinary destination still accepts a channel.
        let plain = build(&|_| {});
        assert_eq!(
            PaymentChannelCreateTransactor.preclaim(&tx, &Sandbox::new(&plain)),
            TxResult::Success,
            "an ordinary destination must still accept a payment channel"
        );

        // lsfDisallowIncomingPayChan (0x10000000).
        let refuses = build(&|a| a["Flags"] = serde_json::json!(0x1000_0000u64));
        assert_eq!(
            PaymentChannelCreateTransactor.preclaim(&tx, &Sandbox::new(&refuses)),
            TxResult::NoPermission,
            "lsfDisallowIncomingPayChan must refuse the channel"
        );

        // Each pseudo-account discriminator alone is enough.
        for f in ["AMMID", "VaultID", "LoanBrokerID"] {
            let pseudo = build(&|a| a[f] = serde_json::json!(hex::encode_upper([0xABu8; 32])));
            assert_eq!(
                PaymentChannelCreateTransactor.preclaim(&tx, &Sandbox::new(&pseudo)),
                TxResult::NoPermission,
                "a destination carrying {f} is a pseudo-account and cannot receive a channel"
            );
        }

        // ORDER: the tag test runs FIRST here, unlike CheckCreate.
        let both = build(&|a| {
            a["Flags"] = serde_json::json!(0x0002_0000u64);
            a["AMMID"] = serde_json::json!(hex::encode_upper([0xABu8; 32]));
        });
        assert_eq!(
            PaymentChannelCreateTransactor.preclaim(&tx, &Sandbox::new(&both)),
            TxResult::DstTagNeeded,
            "lsfRequireDestTag outranks the pseudo-account test for a PayChannel"
        );
    }

    #[test]
    fn pay_channel_create_and_partial_claim() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create a payment channel with 10 XRP
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "10000000",
                "PublicKey": "030FBA552C9626B1DECA8CFAD9F2121DCA55C1066928210397EDCF4F625F6E272C",
            }),
        };

        assert_eq!(PaymentChannelCreateTransactor.preflight(&create_tx), TxResult::Success);
        assert_eq!(PaymentChannelCreateTransactor.preclaim(&create_tx, &sandbox), TxResult::Success);
        assert_eq!(PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        // Sender balance: 50M - 10M = 40M
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 40_000_000);
        // OwnerCount should be 1
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 1);

        // Channel should exist
        let channel_key = keylet::pay_channel_key(&sender, &dest, 1);
        assert!(sandbox.exists(&channel_key));

        // Partial claim — claim 3 XRP from the 10 XRP channel
        let channel_hex = hex::encode(channel_key.0);
        let claim_tx = TxFields {
            account: dest,
            tx_type: "PaymentChannelClaim".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Balance": "3000000",
                "PublicKey": "030FBA552C9626B1DECA8CFAD9F2121DCA55C1066928210397EDCF4F625F6E272C",
                "Signature": "DEADBEEF",
            }),
        };

        assert_eq!(PaymentChannelClaimTransactor.preflight(&claim_tx), TxResult::Success);
        assert_eq!(PaymentChannelClaimTransactor.preclaim(&claim_tx, &sandbox), TxResult::Success);
        assert_eq!(PaymentChannelClaimTransactor.do_apply(&claim_tx, &mut sandbox), TxResult::Success);

        // Dest balance: 10M + 3M = 13M
        assert_eq!(read_field_u64(&sandbox, &dest, "Balance"), 13_000_000);
        // Channel should still exist (partially claimed)
        assert!(sandbox.exists(&channel_key));

        // Read channel to verify Balance updated
        let ch_data = sandbox.read(&channel_key).unwrap();
        let ch: serde_json::Value = serde_json::from_slice(&ch_data).unwrap();
        assert_eq!(ch["Balance"].as_str().unwrap(), "3000000");
        assert_eq!(ch["Amount"].as_str().unwrap(), "10000000");
    }

    #[test]
    fn pay_channel_full_claim_deletes_channel() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create channel with 5 XRP
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "5000000",
                "PublicKey": "030FBA552C9626B1DECA8CFAD9F2121DCA55C1066928210397EDCF4F625F6E272C",
            }),
        };
        PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox);

        let channel_key = keylet::pay_channel_key(&sender, &dest, 1);
        let channel_hex = hex::encode(channel_key.0);

        // Fully claim the channel
        let claim_tx = TxFields {
            account: dest,
            tx_type: "PaymentChannelClaim".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Balance": "5000000",
                "PublicKey": "030FBA552C9626B1DECA8CFAD9F2121DCA55C1066928210397EDCF4F625F6E272C",
                "Signature": "DEADBEEF",
                // Full claims do NOT delete the channel by themselves —
                // deletion needs tfClose (receiver ⇒ immediate) or expiry.
                "Flags": 131072u64,
            }),
        };

        assert_eq!(PaymentChannelClaimTransactor.do_apply(&claim_tx, &mut sandbox), TxResult::Success);

        // Channel should be deleted
        assert!(!sandbox.exists(&channel_key));
        // Dest balance: 10M + 5M = 15M
        assert_eq!(read_field_u64(&sandbox, &dest, "Balance"), 15_000_000);
        // Sender OwnerCount back to 0
        assert_eq!(read_field_u64(&sandbox, &sender, "OwnerCount"), 0);
    }

    #[test]
    fn pay_channel_fund_increases_amount() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create channel with 5 XRP
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "5000000",
            }),
        };
        PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Sender balance: 50M - 5M = 45M
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 45_000_000);

        let channel_key = keylet::pay_channel_key(&sender, &dest, 1);
        let channel_hex = hex::encode(channel_key.0);

        // Fund the channel with 3 more XRP
        let fund_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelFund".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Amount": "3000000",
            }),
        };

        assert_eq!(PaymentChannelFundTransactor.preflight(&fund_tx), TxResult::Success);
        assert_eq!(PaymentChannelFundTransactor.preclaim(&fund_tx, &sandbox), TxResult::Success);
        assert_eq!(PaymentChannelFundTransactor.do_apply(&fund_tx, &mut sandbox), TxResult::Success);

        // Sender balance: 45M - 3M = 42M
        assert_eq!(read_field_u64(&sandbox, &sender, "Balance"), 42_000_000);

        // Channel Amount should be 5M + 3M = 8M
        let ch_data = sandbox.read(&channel_key).unwrap();
        let ch: serde_json::Value = serde_json::from_slice(&ch_data).unwrap();
        assert_eq!(ch["Amount"].as_str().unwrap(), "8000000");
    }

    #[test]
    fn pay_channel_fund_wrong_party() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state_with_accounts(&[(&sender, 50_000_000), (&dest, 10_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create channel
        let create_tx = TxFields {
            account: sender,
            tx_type: "PaymentChannelCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": "5000000",
            }),
        };
        PaymentChannelCreateTransactor.do_apply(&create_tx, &mut sandbox);

        let channel_key = keylet::pay_channel_key(&sender, &dest, 1);
        let channel_hex = hex::encode(channel_key.0);

        // Destination tries to fund — should fail (only creator can fund)
        let fund_tx = TxFields {
            account: dest,
            tx_type: "PaymentChannelFund".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Channel": channel_hex,
                "Amount": "3000000",
            }),
        };

        assert_eq!(
            PaymentChannelFundTransactor.do_apply(&fund_tx, &mut sandbox),
            TxResult::NoPermission
        );
    }
}
