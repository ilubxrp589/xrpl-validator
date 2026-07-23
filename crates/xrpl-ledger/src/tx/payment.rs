//! Payment transaction — XRP direct payment.
//!
//! The most fundamental transaction type. Moves XRP from one account to another.
//! Can also create new accounts if the amount meets the reserve requirement.
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

/// Payment transactor.
pub struct PaymentTransactor;

impl PaymentTransactor {
    /// Extract the destination account ID from the transaction fields.
    fn destination(tx: &TxFields) -> Option<[u8; 20]> {
        let dest_hex = tx.fields.get("Destination")?.as_str()?;
        let bytes = hex::decode(dest_hex).ok()?;
        if bytes.len() != 20 {
            return None;
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Some(arr)
    }

    /// Intermediate book-hop legs from the FIRST path. `Some(vec![])` means
    /// no usable hops; `None` means the path uses account elements (rippling)
    /// we can't model — the caller falls back to the single-book cross.
    fn path_hops(tx: &TxFields) -> Option<Vec<crate::tx::offer::Leg>> {
        let els = tx
            .fields
            .get("Paths")
            .and_then(|p| p.as_array())
            .and_then(|paths| paths.first())
            .and_then(|p| p.as_array());
        let Some(els) = els else { return Some(Vec::new()) };
        let mut legs: Vec<crate::tx::offer::Leg> = Vec::new();
        for el in els {
            let t = el.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
            if t & 0x01 != 0 {
                // An account hop through the ISSUER of the preceding hop's
                // currency is a no-op re-anchor (the value already lives on
                // that issuer's books) — skip it. True rippling through a
                // third party is not modeled.
                let acct = el
                    .get("account")
                    .and_then(|v| v.as_str())
                    .and_then(crate::tx::offer::decode20);
                match (acct, legs.last()) {
                    (Some(a), Some(prev)) if !prev.xrp && a == prev.issuer => continue,
                    _ => return None,
                }
            }
            let cur = el.get("currency").and_then(|v| v.as_str())?;
            if cur == "XRP" {
                legs.push(crate::tx::offer::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] });
                continue;
            }
            let mut c20 = [0u8; 20];
            if cur.len() == 40 {
                let b = hex::decode(cur).ok()?;
                c20.copy_from_slice(&b);
            } else if cur.len() == 3 {
                c20[12..15].copy_from_slice(cur.as_bytes());
            } else {
                return None;
            }
            let iss = el
                .get("issuer")
                .and_then(|v| v.as_str())
                .and_then(crate::tx::offer::decode20)?;
            legs.push(crate::tx::offer::Leg { xrp: false, cur: c20, issuer: iss });
        }
        Some(legs)
    }

    /// Extract the Amount in drops from the transaction fields.
    /// Amount can be a string (XRP drops) or an object (IOU — not handled here).
    fn amount_drops(tx: &TxFields) -> Option<u64> {
        match &tx.fields.get("Amount")? {
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None, // IOU object — not supported yet
        }
    }

    /// Read the reserve base from state, or use default.
    fn reserve_base(sandbox: &Sandbox) -> u64 {
        let fee_key = keylet::fee_settings_key();
        if let Some(data) = sandbox.read(&fee_key) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                if let Some(r) = v.get("ReserveBase").and_then(|r| r.as_u64()) {
                    return r;
                }
                // Newer format uses ReserveBaseDrops
                if let Some(r) = v
                    .get("ReserveBaseDrops")
                    .and_then(|r| r.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    return r;
                }
            }
        }
        // Default: 10 XRP (changed from 1 XRP in 2024)
        // Mainnet base reserve since the 2024 vote — the old 10 XRP default
        // turned real account-creates into phantom tecNO_DST_INSUF_XRP
        // (#105284279 7423D8FA).
        1_000_000
    }

    /// Per-owned-object reserve increment from FeeSettings.
    fn reserve_inc(sandbox: &Sandbox) -> u64 {
        let fee_key = keylet::fee_settings_key();
        if let Some(data) = sandbox.read(&fee_key) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                if let Some(r) = v.get("ReserveIncrement").and_then(|r| r.as_u64()) {
                    return r;
                }
                if let Some(r) = v
                    .get("ReserveIncrementDrops")
                    .and_then(|r| r.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    return r;
                }
            }
        }
        200_000
    }
}

impl Transactor for PaymentTransactor {
    /// val-060: Format validation — no state access.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        // Must be a Payment
        if tx.tx_type != "Payment" {
            return TxResult::Malformed;
        }

        // Fee must be positive
        if tx.fee == 0 {
            return TxResult::BadFee;
        }

        // Destination must be present and valid
        if Self::destination(tx).is_none() {
            return TxResult::Malformed;
        }

        // Amount: XRP drops or an IOU object.
        match Self::amount_drops(tx) {
            Some(amount) => {
                if amount == 0 || amount > 100_000_000_000_000_000 {
                    return TxResult::BadAmount;
                }
            }
            None => {
                // IOU delivery — validated by the engine in do_apply.
                let ok = tx.fields.get("Amount")
                    .and_then(crate::ledger::keylet::amount_mant_exp)
                    .is_some_and(|(m, _)| m > 0);
                if !ok {
                    return TxResult::BadAmount;
                }
            }
        }

        // Can't send to yourself (rippled allows it but it's a no-op)
        // Safe: we already checked destination is Some above
        let dest = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        if dest == tx.account {
            // rippled actually allows this — it's just a fee burn
            // We'll allow it too
        }

        TxResult::Success
    }

    /// val-061: State validation — read-only checks.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);

        // Sender must exist
        let acct_data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };

        let acct: serde_json::Value = match serde_json::from_slice(&acct_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Check sequence (skip for ticket-based txs)
        if !tx.uses_ticket() {
            let acct_seq = acct["Sequence"].as_u64().unwrap_or(0) as u32;
            if tx.sequence < acct_seq {
                return TxResult::PastSeq;
            }
            if tx.sequence > acct_seq {
                return TxResult::BadSequence;
            }
        }

        // Check LastLedgerSequence
        if let Some(max_ledger) = tx.last_ledger_seq {
            let current_seq = sandbox.base().header.sequence;
            if current_seq > max_ledger {
                return TxResult::MaxLedger;
            }
        }

        // Funding check applies only to a DIRECT full-delivery XRP payment.
        // Cross-currency (SendMax) buys the XRP via paths, and tfPartial
        // delivers what liquidity affords — both resolved in do_apply.
        let partial = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0002_0000 != 0;
        let pure_xrp = tx.fields.get("SendMax").is_none()
            && tx.fields.get("Paths").is_none()
            && tx.fields.get("Amount").map(|a| a.is_string()).unwrap_or(false);
        if pure_xrp && !partial {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let amount = Self::amount_drops(tx).unwrap_or(0);
            // rippled: mPriorBalance < amount + accountReserve(OwnerCount) —
            // the sender's reserve is untouchable (#105035381 D21350B6).
            let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
            let reserve = Self::reserve_base(sandbox)
                .saturating_add(Self::reserve_inc(sandbox).saturating_mul(oc));
            if balance < amount.saturating_add(reserve) {
                return TxResult::UnfundedPayment;
            }
        }

        // If destination doesn't exist, amount must meet reserve
        let dest = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let dest_key = keylet::account_root_key(&dest);
        if !sandbox.exists(&dest_key) {
            let reserve = Self::reserve_base(sandbox);
            let amount = Self::amount_drops(tx).unwrap_or(0);
            if amount < reserve {
                return TxResult::NoDstInsufXrp;
            }
        } else if tx.fields.get("DestinationTag").is_none() {
            // lsfRequireDestTag on the destination rejects untagged payments.
            let requires_tag = sandbox
                .read(&dest_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                .and_then(|a| a["Flags"].as_u64())
                .map(|f| f & 0x0002_0000 != 0)
                .unwrap_or(false);
            if requires_tag {
                return TxResult::DstTagNeeded;
            }
        }

        TxResult::Success
    }

    /// val-062: Apply payment state changes — direct XRP, direct IOU, or
    /// cross-currency via the order books (the crossing engine).
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let dest_id = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let amt_json = tx.fields.get("Amount").cloned().unwrap_or_default();
        let sendmax = tx.fields.get("SendMax").cloned();
        let partial = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0002_0000 != 0;
        let cross_currency = match (&sendmax, amt_json.is_string()) {
            (Some(sm), true) => !sm.is_string(),
            (Some(sm), false) => {
                sm.is_string()
                    || sm.get("currency").and_then(|v| v.as_str())
                        != amt_json.get("currency").and_then(|v| v.as_str())
                    || sm.get("issuer").and_then(|v| v.as_str())
                        != amt_json.get("issuer").and_then(|v| v.as_str())
            }
            (None, _) => false,
        } || tx.fields.get("Paths").is_some();

        if cross_currency {
            return self.apply_path_payment(tx, sandbox, &amt_json, sendmax.as_ref(), &dest_id, partial);
        }
        if !amt_json.is_string() {
            return self.apply_iou_direct(tx, sandbox, &amt_json, &dest_id, partial);
        }

        let amount = match Self::amount_drops(tx) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // --- Sender side ---
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Deduct amount (fee is deducted by apply_common)
        let sender_balance = sender["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if sender_balance < amount {
            return TxResult::UnfundedPayment;
        }

        sender["Balance"] = serde_json::Value::String((sender_balance - amount).to_string());
        sandbox.write(sender_key, serde_json::to_vec(&sender).expect("serializing valid JSON Value"));

        // --- Destination side ---
        let dest_key = keylet::account_root_key(&dest_id);

        if let Some(dest_data) = sandbox.read(&dest_key) {
            // Destination exists — add amount
            let mut dest: serde_json::Value = match serde_json::from_slice(&dest_data) {
                Ok(v) => v,
                Err(_) => return TxResult::Malformed,
            };

            let dest_balance = dest["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            let new_dest_balance = match dest_balance.checked_add(amount) {
                Some(b) => b,
                None => return TxResult::Malformed,
            };
            dest["Balance"] = serde_json::Value::String(new_dest_balance.to_string());
            sandbox.write(dest_key, serde_json::to_vec(&dest).expect("serializing valid JSON Value"));
        } else {
            // Destination doesn't exist — create new AccountRoot
            let reserve = Self::reserve_base(sandbox);
            if amount < reserve {
                return TxResult::NoDstInsufXrp;
            }

            let new_account = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(dest_id),
                "Balance": amount.to_string(),
                "Sequence": 1,
                "OwnerCount": 0,
                "Flags": 0,
            });
            sandbox.write(dest_key, serde_json::to_vec(&new_account).expect("serializing valid JSON Value"));
        }

        TxResult::Success
    }
}

impl PaymentTransactor {
    /// Direct same-currency IOU transfer over trust lines. Key-set faithful:
    /// sender/receiver lines adjust (receiver line created when absent),
    /// issuer-side legs settle implicitly. Insufficient holdings fail the
    /// rippled way: nothing delivered -> tecPATH_DRY, partial short-fall
    /// without tfPartialPayment -> tecPATH_PARTIAL (both fee-only).
    fn apply_iou_direct(
        &self,
        tx: &TxFields,
        sandbox: &mut Sandbox,
        amt_json: &serde_json::Value,
        dest: &[u8; 20],
        partial: bool,
    ) -> TxResult {
        use crate::tx::offer as ox;
        let (Some(leg), Some(want)) = (ox::leg_of(amt_json), crate::ledger::keylet::amount_mant_exp(amt_json)) else {
            return TxResult::Malformed;
        };
        // rippled delivers an IOU to the destination by rippling through the
        // issuer, which credits the destination's EXISTING trust line — a
        // payment never opens a trust line for the receiver. If the
        // destination neither issues the currency nor already trusts the
        // issuer, there is no line to credit and the strand is dry (#105797892
        // 41D13D: dest holds no BXE line → mainnet tecPATH_DRY; we phantom-
        // created the line and over-delivered). The sender's own line is
        // handled by available() above; only the receiving side needs a line.
        if dest != &leg.issuer {
            let dest_line = keylet::ripple_state_key(dest, &leg.issuer, &leg.cur);
            if !sandbox.exists(&dest_line) {
                return TxResult::PathDry;
            }
        }
        let avail = if tx.account == leg.issuer {
            want // issuers mint their own IOU
        } else {
            ox::available(sandbox, &tx.account, &leg)
        };
        if ox::me_is_zero(avail) {
            return TxResult::PathDry;
        }
        let deliver = if ox::me_cmp(avail, want).is_lt() {
            if !partial {
                return TxResult::PathPartial;
            }
            avail
        } else {
            want
        };
        ox::move_leg(sandbox, &tx.account, dest, &leg, deliver);
        TxResult::Success
    }

    /// Cross-currency delivery: spend the SendMax side across the order book
    /// to acquire the Amount side (the offer-crossing engine with payment
    /// semantics), then hand the acquisition to the destination. Arb-style
    /// self-payments (Account == Destination) skip the final hop.
    fn apply_path_payment(
        &self,
        tx: &TxFields,
        sandbox: &mut Sandbox,
        amt_json: &serde_json::Value,
        sendmax: Option<&serde_json::Value>,
        dest: &[u8; 20],
        partial: bool,
    ) -> TxResult {
        use crate::tx::offer as ox;
        let Some(sm_json) = sendmax else {
            // Paths without SendMax: spend the Amount currency itself.
            return self.apply_iou_direct(tx, sandbox, amt_json, dest, partial);
        };
        let (Some(want_leg), Some(want0)) = (ox::leg_of(amt_json), crate::ledger::keylet::amount_mant_exp(amt_json)) else {
            return TxResult::Malformed;
        };
        let (Some(spend_leg), Some(spend0)) = (ox::leg_of(sm_json), crate::ledger::keylet::amount_mant_exp(sm_json)) else {
            return TxResult::Malformed;
        };
        // The strand's input is bounded by what the sender actually holds of
        // the SendMax asset (issuer mints freely; XRP is balance-minus-
        // reserve; IOU is the trust-line holding). A sender with nothing to
        // spend is a dry path regardless of book or AMM depth.
        let spend_avail = ox::available(sandbox, &tx.account, &spend_leg);
        if ox::me_is_zero(spend_avail) {
            return TxResult::PathDry;
        }
        let spend0 = if ox::me_cmp(spend_avail, spend0).is_lt() { spend_avail } else { spend0 };
        let snap = sandbox.snapshot();
        // rippled only imposes the SendMax/Amount ratio as a per-offer quality
        // bound when tfLimitQuality is set. Otherwise the book is walked
        // best-first under the aggregate bounds alone (spend ≤ SendMax,
        // deliver ≤ Amount) — arb-style payments use a sentinel-max Amount
        // whose implied ratio would read every book as dry.
        let limit_quality =
            tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0004_0000 != 0;
        let threshold = if limit_quality {
            crate::ledger::keylet::offer_quality(sm_json, amt_json).unwrap_or(u64::MAX)
        } else {
            u64::MAX
        };
        // Multi-hop strands: the FIRST path's elements name the intermediate
        // currencies — one book per adjacent pair. Account elements
        // (rippling) are not modeled; those fall back to the single-book
        // cross. Intermediate acquisitions ride "in flight" through the
        // SENDER: each hop credits the sender and the next debits the exact
        // same amount, so the net-zero line drops out of the mutation set —
        // matching rippled, which never materializes it.
        let hops = Self::path_hops(tx);
        // The strand's output belongs to the DESTINATION: crediting the
        // sender first and forwarding would materialize an intermediate
        // trust line rippled never creates (and when the destination is the
        // issuer, the IOU is redeemed, not held).
        let delivered = if let Some(hops) = hops.as_ref().filter(|h| !h.is_empty()) {
            let mut chain: Vec<&ox::Leg> = std::iter::once(&spend_leg)
                .chain(hops.iter())
                .chain(std::iter::once(&want_leg))
                .collect();
            // A path may name an endpoint currency as a "hop" (e.g. deliver
            // RLUSD via [RLUSD-book, issuer]): same-leg neighbours are a
            // zero-length book — collapse them.
            chain.dedup_by(|a, b| a.xrp == b.xrp && a.cur == b.cur && a.issuer == b.issuer);
            // Intermediate value is IN FLIGHT: rippled never rests it on the
            // sender's trust lines. Snapshot those lines and restore them
            // byte-exact after the chain — net-zero routing can leave 1-ulp
            // dust (or a phantom created line) that the no-op filter keeps.
            let same = |a: &ox::Leg, b: &ox::Leg| a.xrp == b.xrp && a.cur == b.cur && a.issuer == b.issuer;
            // Capture, for each in-flight line, the line itself AND the
            // directory pages a mid-chain line-creation would touch (both
            // owners' dir roots + their last pages) — creating and then
            // forgetting the line must leave no dir droppings either.
            let mut inflight: Vec<_> = Vec::new();
            for l in hops.iter().filter(|l| !l.xrp && !same(l, &want_leg) && !same(l, &spend_leg)) {
                let lk = keylet::ripple_state_key(&tx.account, &l.issuer, &l.cur);
                let line_pre = sandbox.read(&lk);
                let absent = line_pre.is_none();
                inflight.push((lk, line_pre));
                if !absent {
                    continue; // pre-existing line: no creation, no dir droppings
                }
                for owner in [&tx.account, &l.issuer] {
                    let root = keylet::owner_dir_key(owner);
                    let pre = sandbox.read(&root);
                    if let Some(bytes) = &pre {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
                            let last = v.get("IndexPrevious").and_then(|p| {
                                p.as_u64().or_else(|| p.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                            }).unwrap_or(0);
                            if last != 0 {
                                let pk = keylet::dir_page_key(&root, last);
                                inflight.push((pk, sandbox.read(&pk)));
                            }
                        }
                    }
                    inflight.push((root, pre));
                }
            }
            let mut carry = spend0;
            let n = chain.len() - 1;
            for i in 0..n {
                let last = i + 1 == n;
                let benef = if last { dest } else { &tx.account };
                // Intermediate hops SELL the whole carry; the last hop buys
                // up to the Amount cap.
                let want_cap = if last { want0 } else { (9_990_000_000_000_000, 60) };
                let (rw, _rs, _c) = ox::cross_engine_to(
                    &tx.account, benef, want_cap, carry, chain[i + 1], chain[i],
                    threshold, !last, false, None, sandbox,
                );
                carry = ox::me_sub(want_cap, rw);
                if ox::me_is_zero(carry) {
                    break; // hop dried: nothing delivered
                }
            }
            for (k, pre) in inflight {
                match pre {
                    Some(bytes) => sandbox.write(k, bytes),
                    None => sandbox.forget(&k),
                }
            }
            carry
        } else {
            let (rem_want, _rem_spend, _crossed) = ox::cross_engine_to(
                &tx.account, dest, want0, spend0, &want_leg, &spend_leg, threshold, false,
                false, None, sandbox,
            );
            ox::me_sub(want0, rem_want)
        };
        if ox::me_is_zero(delivered) {
            sandbox.restore_snapshot(snap);
            return TxResult::PathDry;
        }
        if !partial && ox::me_cmp(delivered, want0).is_lt() {
            sandbox.restore_snapshot(snap);
            return TxResult::PathPartial;
        }
        // With tfPartialPayment, DeliverMin is the delivery floor: falling
        // short fails fee-only with tecPATH_PARTIAL.
        if partial {
            if let Some(dm) = tx
                .fields
                .get("DeliverMin")
                .and_then(crate::ledger::keylet::amount_mant_exp)
            {
                if ox::me_cmp(delivered, dm).is_lt() {
                    sandbox.restore_snapshot(snap);
                    return TxResult::PathPartial;
                }
            }
        }
        TxResult::Success
    }
}

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
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
    }

    fn read_balance(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        v["Balance"].as_str().unwrap().parse().unwrap()
    }

    fn read_balance_from_state(state: &LedgerState, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = state.state_map.lookup(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(data).unwrap();
        v["Balance"].as_str().unwrap().parse().unwrap()
    }

    fn payment_tx(sender: [u8; 20], dest: [u8; 20], amount: u64, fee: u64, seq: u32) -> TxFields {
        TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee,
            sequence: seq,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": amount.to_string(),
            }),
        }
    }

    #[test]
    fn preflight_valid_payment() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::Success);
    }

    #[test]
    fn preflight_zero_amount() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 0, 12, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::BadAmount);
    }

    #[test]
    fn preflight_zero_fee() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 1_000_000, 0, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::BadFee);
    }

    #[test]
    fn preflight_no_destination() {
        let sender = [0x01u8; 20];
        let tx = TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({"Amount": "1000000"}),
        };
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::Malformed);
    }

    #[test]
    fn preclaim_sender_not_found() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state(); // no accounts
        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1);
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::NoAccount);
    }

    #[test]
    fn preclaim_insufficient_balance() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 500_000, 1); // only 0.5 XRP
        add_account(&mut state, &dest, 50_000_000, 1);

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1); // needs 1M + 12
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::UnfundedPayment);
    }

    #[test]
    fn preclaim_new_dest_below_reserve() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        // dest doesn't exist; 0.5 XRP is below the 1 XRP base reserve

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 500_000, 12, 1);
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::NoDstInsufXrp);
    }

    #[test]
    fn preclaim_past_sequence() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 10); // seq=10
        add_account(&mut state, &dest, 50_000_000, 1);

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 5); // tx seq=5 < account seq=10
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::PastSeq);
    }

    #[test]
    fn do_apply_xrp_to_existing_account() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        add_account(&mut state, &dest, 10_000_000, 1);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            let tx = payment_tx(sender, dest, 5_000_000, 12, 1);

            // Run common (deducts fee, increments seq)
            let common = apply_common(&tx, &mut sandbox);
            assert_eq!(common, TxResult::Success);

            // Run payment apply
            let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
            assert_eq!(result, TxResult::Success);

            // Verify in sandbox
            // Sender: 50M - 12(fee) - 5M(amount) = 44,999,988
            assert_eq!(read_balance(&sandbox, &sender), 44_999_988);
            // Dest: 10M + 5M = 15M
            assert_eq!(read_balance(&sandbox, &dest), 15_000_000);

            sandbox.into_modifications()
        };

        // Commit and verify state changed
        apply_modifications(&mut state, mods).unwrap();
        assert_eq!(read_balance_from_state(&state, &sender), 44_999_988);
        assert_eq!(read_balance_from_state(&state, &dest), 15_000_000);
    }

    #[test]
    fn do_apply_creates_new_account() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 100_000_000, 1);
        // dest doesn't exist

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            let tx = payment_tx(sender, dest, 20_000_000, 12, 1); // 20 XRP > 10 XRP reserve

            let common = apply_common(&tx, &mut sandbox);
            assert_eq!(common, TxResult::Success);

            let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
            assert_eq!(result, TxResult::Success);

            // New account should exist with balance = amount
            assert_eq!(read_balance(&sandbox, &dest), 20_000_000);
            // Sender: 100M - 12 - 20M = 79,999,988
            assert_eq!(read_balance(&sandbox, &sender), 79_999_988);

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();

        // Verify new account was created in state
        let dest_key = keylet::account_root_key(&dest);
        let dest_data = state.state_map.lookup(&dest_key).expect("dest account should exist");
        let dest_obj: serde_json::Value = serde_json::from_slice(dest_data).unwrap();
        assert_eq!(dest_obj["LedgerEntryType"], "AccountRoot");
        assert_eq!(dest_obj["Sequence"], 1);
        assert_eq!(dest_obj["Balance"].as_str().unwrap(), "20000000");
    }

    #[test]
    fn do_apply_new_account_below_reserve_fails() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 100_000_000, 1);

        let mut sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 500_000, 12, 1); // 0.5 XRP < 1 XRP reserve

        let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
        assert_eq!(result, TxResult::NoDstInsufXrp);
    }

    #[test]
    fn full_pipeline_preflight_preclaim_apply() {
        let sender = [0xAAu8; 20];
        let dest = [0xBBu8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 200_000_000, 5);
        add_account(&mut state, &dest, 50_000_000, 1);

        let tx = payment_tx(sender, dest, 25_000_000, 15, 5);
        let transactor = PaymentTransactor;

        // Full pipeline
        assert_eq!(transactor.preflight(&tx), TxResult::Success);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            assert_eq!(transactor.preclaim(&tx, &sandbox), TxResult::Success);
            assert_eq!(apply_common(&tx, &mut sandbox), TxResult::Success);
            assert_eq!(transactor.do_apply(&tx, &mut sandbox), TxResult::Success);

            // Sender: 200M - 15(fee) - 25M = 174,999,985
            assert_eq!(read_balance(&sandbox, &sender), 174_999_985);
            // Dest: 50M + 25M = 75M
            assert_eq!(read_balance(&sandbox, &dest), 75_000_000);
            // Sender sequence: 5 → 6
            let sk = keylet::account_root_key(&sender);
            let sd = sandbox.read(&sk).unwrap();
            let sv: serde_json::Value = serde_json::from_slice(&sd).unwrap();
            assert_eq!(sv["Sequence"], 6);

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();
    }

    /// Seed a book where `issuer` (as maker) sells 5 USD for 5 XRP, then
    /// return (state, taker, issuer).
    fn state_with_usd_book() -> (LedgerState, [u8; 20], [u8; 20]) {
        let taker = [0x01u8; 20];
        let issuer = [0x03u8; 20];
        let mut state = make_state();
        add_account(&mut state, &taker, 50_000_000, 1);
        add_account(&mut state, &issuer, 50_000_000, 1);

        let mut sandbox = Sandbox::new(&state);
        let offer_tx = TxFields {
            account: issuer,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "5000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
            }),
        };
        assert_eq!(
            crate::tx::offer::OfferCreateTransactor.do_apply(&offer_tx, &mut sandbox),
            TxResult::Success
        );
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();
        (state, taker, issuer)
    }

    /// Arb-style conversion: sentinel-max Amount + tfPartialPayment must
    /// deliver whatever SendMax buys off the book (rippled imposes no
    /// SendMax/Amount quality bound without tfLimitQuality).
    #[test]
    fn path_payment_partial_sentinel_amount_delivers() {
        let (state, taker, issuer) = state_with_usd_book();
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(taker),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1000000000"},
                "SendMax": "5000000",
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
        // Taker spent the 5 XRP on the book.
        assert_eq!(read_balance(&sandbox, &taker), 45_000_000);
        // Taker now holds the acquired USD on a trust line.
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        assert!(sandbox.exists(&keylet::ripple_state_key(&taker, &issuer, &cur)));
    }

    /// A direct IOU payment can only be delivered to a destination that
    /// already trusts the issuer — rippled never opens the receiver's trust
    /// line. No line ⇒ tecPATH_DRY, not a phantom-created line + delivery
    /// (mainnet #105797892 41D13D: dest held no BXE line).
    #[test]
    fn direct_iou_payment_requires_destination_line() {
        let sender = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let dest_noline = [0x03u8; 20];
        let dest_trusts = [0x05u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        add_account(&mut state, &issuer, 50_000_000, 1);
        add_account(&mut state, &dest_noline, 50_000_000, 1);
        add_account(&mut state, &dest_trusts, 50_000_000, 1);

        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        // Insert a trust line `a`↔issuer with `a` holding +value USD.
        let add_line = |state: &mut LedgerState, a: &[u8; 20], value: &str| {
            let key = keylet::ripple_state_key(a, &issuer, &cur);
            let (lo, hi) = if a < &issuer { (*a, issuer) } else { (issuer, *a) };
            let low_bal = if a < &issuer { value.to_string() } else { format!("-{value}") };
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState",
                "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur),
                            "issuer": "0000000000000000000000000000000000000000",
                            "value": low_bal},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(key, serde_json::to_vec(&line).unwrap()).unwrap();
        };
        // Sender holds 100 USD to spend; only dest_trusts has a USD line.
        add_line(&mut state, &sender, "100");
        add_line(&mut state, &dest_trusts, "0");

        let pay = |dest: [u8; 20]| TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "25"},
            }),
        };

        // Destination without a USD line: dry, and no line phantom-created.
        let mut sb = Sandbox::new(&state);
        assert_eq!(PaymentTransactor.do_apply(&pay(dest_noline), &mut sb), TxResult::PathDry);
        assert!(!sb.exists(&keylet::ripple_state_key(&dest_noline, &issuer, &cur)));

        // Destination that already trusts the issuer: delivered.
        let mut sb2 = Sandbox::new(&state);
        assert_eq!(PaymentTransactor.do_apply(&pay(dest_trusts), &mut sb2), TxResult::Success);
    }

    /// Mainnet tx AAA6EB389D3A… (ledger 105035381): when the Destination IS
    /// the issuer of the delivered currency, the strand's output goes
    /// straight there and the IOU is redeemed — rippled never materializes a
    /// trust line for the sender in between. We used to route through the
    /// sender, leaving a zero-balance line (plus its directory pages and
    /// OwnerCount bumps) that mainnet's meta has no trace of.
    #[test]
    fn path_payment_to_issuer_creates_no_sender_line() {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let maker = [0x04u8; 20];
        let mut state = make_state();
        add_account(&mut state, &taker, 50_000_000, 1);
        add_account(&mut state, &maker, 50_000_000, 1);
        add_account(&mut state, &issuer, 50_000_000, 1);

        // Maker holds USD and sells 5 USD for 5 XRP.
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur),
                        "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "0"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "0"},
        });
        state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let offer_tx = TxFields {
            account: maker,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "5000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
            }),
        };
        assert_eq!(
            crate::tx::offer::OfferCreateTransactor.do_apply(&offer_tx, &mut sandbox),
            TxResult::Success
        );
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Pay USD to the ISSUER, sourcing it from the book with XRP.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(issuer),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1000000000"},
                "SendMax": "5000000",
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // No trust line for the sender: the IOU never rests with them.
        assert!(!sandbox.exists(&keylet::ripple_state_key(&taker, &issuer, &cur)));
        // The maker's holding fell — that IS the redemption.
        let ml = crate::tx::offer::json_at(&sandbox, &mkey).expect("maker line");
        assert_ne!(ml["Balance"]["value"].as_str(), Some("100"));
    }

    /// A sender holding NONE of the SendMax currency is a dry strand no
    /// matter how deep the book is: fee-only tecPATH_DRY (mainnet arb bots
    /// hit this constantly — SendMax in a currency they hold zero of).
    #[test]
    fn path_payment_unfunded_sendmax_is_dry() {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let maker = [0x04u8; 20];
        let mut state = make_state();
        add_account(&mut state, &taker, 50_000_000, 1);
        add_account(&mut state, &maker, 50_000_000, 1);

        // Maker sells 5 XRP for 5 USD — plenty of liquidity for the taker.
        let mut sandbox = Sandbox::new(&state);
        let offer_tx = TxFields {
            account: maker,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "TakerGets": "5000000",
            }),
        };
        assert_eq!(
            crate::tx::offer::OfferCreateTransactor.do_apply(&offer_tx, &mut sandbox),
            TxResult::Success
        );
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Taker spends USD they do not hold (no trust line at all).
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(taker),
                "Amount": "100000000000000000",
                "SendMax": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::PathDry);
        // Fee-only: taker's XRP untouched, maker's offer untouched.
        assert_eq!(read_balance(&sandbox, &taker), 50_000_000);
        assert!(sandbox.exists(&keylet::offer_key(&maker, 2)));
    }

    /// DeliverMin above what the book can produce: fee-only tecPATH_PARTIAL,
    /// not tecPATH_DRY, and no state mutation survives.
    #[test]
    fn path_payment_deliver_min_short_fails_partial() {
        let (state, taker, issuer) = state_with_usd_book();
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(taker),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1000000000"},
                "SendMax": "5000000",
                "DeliverMin": {"currency": "USD", "issuer": hex::encode(issuer), "value": "10"},
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::PathPartial);
        // Rolled back: XRP untouched, no trust line created.
        assert_eq!(read_balance(&sandbox, &taker), 50_000_000);
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        assert!(!sandbox.exists(&keylet::ripple_state_key(&taker, &issuer, &cur)));
    }
}
