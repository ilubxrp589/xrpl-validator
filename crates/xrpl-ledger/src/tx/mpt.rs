//! Multi-Purpose Token (MPT) primitives — the MPTokensV1 subset the
//! differential harness needs: direct MPT Payments and MPT Clawback.
//!
//! Ground truth (3.2.1 vendored sources):
//! - Payment.cpp:516-590 — the direct MPT arm (V1: MPT payments NEVER route
//!   through the flow engine; `ripple` requires `!isDstMPT || mpTokensV2`).
//! - TokenHelpers.cpp:1060-1204 — directSendNoFeeMPT / directSendNoLimitMPT.
//! - MPTokenHelpers.cpp — requireAuth (Legacy), canTransfer, isAnyFrozen,
//!   transferRate.
//! - ledger_entries.macro — sfOutstandingAmount is SoeRequired (a zero stays
//!   written as "0"); sfMPTAmount is SoeDefault (a zero is OMITTED).
//!
//! Out of scope, stated: MPTokensV2 (flow-engine MPT strands, overflow
//! waivers), vault ReferenceHolding recursion in canTransfer, DomainID
//! credential auth, LockedAmount escrow arithmetic. Each is amendment- or
//! feature-gated territory no specimen exercises; the entry points note them.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{TxFields, TxResult};
use xrpl_core::types::Hash256;

pub const LSF_MPT_LOCKED: u64 = 0x0000_0001;
pub const LSF_MPT_CAN_TRANSFER: u64 = 0x0000_0020;
pub const LSF_MPT_CAN_CLAWBACK: u64 = 0x0000_0040;
pub const LSF_MPT_REQUIRE_AUTH: u64 = 0x0000_0004;
pub const LSF_MPT_AUTHORIZED: u64 = 0x0000_0002;
/// kMaxMpTokenAmount — MPT values are signed-64 positive.
pub const MAX_MPT_AMOUNT: u64 = 0x7FFF_FFFF_FFFF_FFFF;

/// Parse an MPT amount JSON: `{"mpt_issuance_id": <48 hex>, "value": <dec>}`.
/// Returns the 24-byte MPTID and the value. Values arrive as decimal strings
/// (canonical) — a bare number is tolerated on read.
pub fn parse_mpt_amount(v: &serde_json::Value) -> Option<([u8; 24], u64)> {
    let id_hex = v.get("mpt_issuance_id").and_then(|x| x.as_str())?;
    let bytes = hex::decode(id_hex).ok()?;
    let id: [u8; 24] = bytes.as_slice().try_into().ok()?;
    let val = match v.get("value")? {
        serde_json::Value::String(s) => s.parse::<u64>().ok()?,
        serde_json::Value::Number(n) => n.as_u64()?,
        _ => return None,
    };
    Some((id, val))
}

fn json_at(sandbox: &Sandbox, key: &Hash256) -> Option<serde_json::Value> {
    sandbox.read(key).and_then(|d| serde_json::from_slice(&d).ok())
}

/// Read a u64 ledger field serialized as a DECIMAL string ("281380138") —
/// the MPT amount family — treating an absent field as zero (SoeDefault).
fn dec_field(obj: &serde_json::Value, f: &str) -> u64 {
    match obj.get(f) {
        Some(serde_json::Value::String(s)) => s.parse::<u64>().unwrap_or(0),
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

/// Write a decimal-string u64 field honoring its Soe class: `required` fields
/// always carry the value; default fields are REMOVED at zero (rippled omits
/// a defaulted STUInt64 from the serialization, so it never reaches JSON).
fn set_dec_field(obj: &mut serde_json::Value, f: &str, v: u64, required: bool) {
    if v == 0 && !required {
        if let Some(o) = obj.as_object_mut() {
            o.remove(f);
        }
        return;
    }
    obj[f] = serde_json::Value::String(v.to_string());
}

/// The issuance's Issuer account, decoded from either address form.
fn issuance_issuer(issuance: &serde_json::Value) -> Option<[u8; 20]> {
    issuance.get("Issuer").and_then(|v| v.as_str()).and_then(crate::tx::offer::decode20)
}

/// `requireAuth` (MPTokenHelpers.cpp:304, Legacy auth): the issuer is always
/// authorized; anyone else must HOLD an MPToken at all, and — when the
/// issuance carries lsfMPTRequireAuth — that token must be lsfMPTAuthorized.
/// DomainID credential auth and vault recursion are unported (no specimen).
pub fn require_auth(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: &serde_json::Value,
    account: &[u8; 20],
) -> Option<TxResult> {
    let issuer = issuance_issuer(issuance)?;
    if issuer == *account {
        return None;
    }
    let Some(token) = json_at(sandbox, &keylet::mptoken_key(issuance_key, account)) else {
        return Some(TxResult::NoAuth);
    };
    let iflags = issuance.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
    if iflags & LSF_MPT_REQUIRE_AUTH != 0
        && token.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_AUTHORIZED == 0
    {
        return Some(TxResult::NoAuth);
    }
    None
}

/// `isAnyFrozen` for MPT: issuance lsfMPTLocked, or either party's MPToken
/// lsfMPTLocked. (Vault pseudo-account freeze recursion unported.)
fn any_frozen(
    sandbox: &Sandbox,
    issuance_key: &Hash256,
    issuance: &serde_json::Value,
    accounts: &[&[u8; 20]],
) -> bool {
    if issuance.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_LOCKED != 0 {
        return true;
    }
    accounts.iter().any(|a| {
        json_at(sandbox, &keylet::mptoken_key(issuance_key, a))
            .and_then(|t| t.get("Flags").and_then(|f| f.as_u64()))
            .map(|f| f & LSF_MPT_LOCKED != 0)
            .unwrap_or(false)
    })
}

/// `directSendNoFeeMPT` (TokenHelpers.cpp:1060): the single-leg MPT move.
/// Issuer endpoints move OutstandingAmount on the issuance; holder endpoints
/// move MPTAmount on their MPToken. Auth is NOT checked here — callers did.
pub fn direct_send_no_fee(
    sandbox: &mut Sandbox,
    issuance_key: &Hash256,
    sender: &[u8; 20],
    receiver: &[u8; 20],
    amt: u64,
) -> TxResult {
    let Some(mut issuance) = json_at(sandbox, issuance_key) else {
        return TxResult::ObjectNotFound;
    };
    let Some(issuer) = issuance_issuer(&issuance) else { return TxResult::Malformed };
    let outstanding = dec_field(&issuance, "OutstandingAmount");
    let mut issuance_dirty = false;

    if *sender == issuer {
        // V1 has no overflow check HERE (V2-gated); the mint cap lives in
        // directSendNoLimitMPT's issuer branch, which our caller applies.
        set_dec_field(&mut issuance, "OutstandingAmount", outstanding + amt, true);
        issuance_dirty = true;
    } else {
        let tkey = keylet::mptoken_key(issuance_key, sender);
        let Some(mut token) = json_at(sandbox, &tkey) else { return TxResult::NoAuth };
        let bal = dec_field(&token, "MPTAmount");
        if bal < amt {
            return TxResult::InsufficientFunds;
        }
        set_dec_field(&mut token, "MPTAmount", bal - amt, false);
        sandbox.write(tkey, serde_json::to_vec(&token).unwrap_or_default());
    }

    if *receiver == issuer {
        let out_now = dec_field(&issuance, "OutstandingAmount");
        if out_now < amt {
            return TxResult::Malformed; // tecINTERNAL territory — cannot occur
        }
        set_dec_field(&mut issuance, "OutstandingAmount", out_now - amt, true);
        issuance_dirty = true;
    } else {
        let tkey = keylet::mptoken_key(issuance_key, receiver);
        let Some(mut token) = json_at(sandbox, &tkey) else { return TxResult::NoAuth };
        let bal = dec_field(&token, "MPTAmount");
        set_dec_field(&mut token, "MPTAmount", bal + amt, false);
        sandbox.write(tkey, serde_json::to_vec(&token).unwrap_or_default());
    }

    if issuance_dirty {
        sandbox.write(*issuance_key, serde_json::to_vec(&issuance).unwrap_or_default());
    }
    TxResult::Success
}

/// The direct MPT Payment arm (Payment.cpp:516-590, MPTokensV1). Assumes the
/// caller already ran the generic destination checks (pseudo-account, dest
/// exists, dest tag). Returns the final TxResult including rippled's
/// tail mapping of tecINSUFFICIENT_FUNDS / tecPATH_DRY → tecPATH_PARTIAL.
pub fn apply_mpt_payment(
    tx: &TxFields,
    sandbox: &mut Sandbox,
    dest: &[u8; 20],
    mptid: [u8; 24],
    value: u64,
    partial: bool,
) -> TxResult {
    let issuance_key = keylet::mpt_issuance_key(&mptid);
    let Some(issuance) = json_at(sandbox, &issuance_key) else {
        return TxResult::ObjectNotFound; // requireAuth's missing-issuance code
    };
    if let Some(r) = require_auth(sandbox, &issuance_key, &issuance, &tx.account) {
        return r;
    }
    if let Some(r) = require_auth(sandbox, &issuance_key, &issuance, dest) {
        return r;
    }
    let Some(issuer) = issuance_issuer(&issuance) else { return TxResult::Malformed };

    // canTransfer: issuer endpoints always may; third parties need
    // lsfMPTCanTransfer. (ReferenceHolding vault recursion unported.)
    if tx.account != issuer
        && *dest != issuer
        && issuance.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_CAN_TRANSFER == 0
    {
        return TxResult::NoAuth;
    }

    // verifyDepositPreauth — unconditional on this arm (Payment.cpp:533).
    if let Some(dst_root) = json_at(sandbox, &keylet::account_root_key(dest)) {
        if dst_root["Flags"].as_u64().unwrap_or(0) & 0x0100_0000 != 0
            && tx.account != *dest
            && sandbox.read(&keylet::deposit_preauth_key(dest, &tx.account)).is_none()
        {
            return TxResult::NoPermission;
        }
    }

    // Transfer rate applies only between holders; the same branch owns the
    // lock check (issuer moves stay allowed while locked).
    let mut rate: u64 = 1_000_000_000;
    if tx.account != issuer && *dest != issuer {
        if any_frozen(sandbox, &issuance_key, &issuance, &[&tx.account, dest]) {
            return TxResult::Locked;
        }
        let fee = dec_field(&issuance, "TransferFee");
        rate = 1_000_000_000 + 10_000 * fee;
    }

    // maxSourceAmount: SendMax's value when present (preflight pinned it to
    // the same MPT), else the delivery amount itself (Payment.cpp:73).
    let max_source = tx
        .fields
        .get("SendMax")
        .and_then(parse_mpt_amount)
        .map(|(_, v)| v)
        .unwrap_or(value);

    // "No rounding. It'll change once MPT integrated into DEX" — the cost is
    // value×rate/1e9 in exact integer arithmetic (u128 intermediate).
    let cost_of = |v: u64| -> u64 { ((v as u128) * (rate as u128) / 1_000_000_000u128) as u64 };
    let mut deliver = value;
    let mut required = cost_of(value);
    if partial && required > max_source {
        required = max_source;
        deliver = ((max_source as u128) * 1_000_000_000u128 / (rate as u128)) as u64;
    }
    let deliver_min = tx.fields.get("DeliverMin").and_then(parse_mpt_amount).map(|(_, v)| v);
    if required > max_source || deliver_min.is_some_and(|m| deliver < m) {
        return TxResult::PathPartial;
    }

    // accountSend → directSendNoLimitMPT: issuer-adjacent moves are one leg
    // (with the V1 mint cap); holder→holder transits via the issuer bridge —
    // issuer→receiver the delivery, sender→issuer the cost.
    let res = if tx.account == issuer || *dest == issuer {
        if tx.account == issuer {
            let max = issuance
                .get("MaximumAmount")
                .map(|_| dec_field(&issuance, "MaximumAmount"))
                .unwrap_or(MAX_MPT_AMOUNT);
            let outstanding = dec_field(&issuance, "OutstandingAmount");
            if outstanding.checked_add(deliver).map(|s| s > max).unwrap_or(true) {
                return TxResult::PathPartial; // tecPATH_DRY → mapped below anyway
            }
        }
        direct_send_no_fee(sandbox, &issuance_key, &tx.account, dest, deliver)
    } else {
        let leg = direct_send_no_fee(sandbox, &issuance_key, &issuer, dest, deliver);
        if leg != TxResult::Success {
            leg
        } else {
            direct_send_no_fee(sandbox, &issuance_key, &tx.account, &issuer, required)
        }
    };

    match res {
        TxResult::InsufficientFunds | TxResult::PathDry => TxResult::PathPartial,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// MPT issuance lifecycle — MPTokenIssuanceCreate / Destroy / Set, and
// MPTokenAuthorize. Previously fee-only stubs (probe STUB_TYPES).
// Sources: MPTokenIssuanceCreate.cpp::create, MPTokenIssuanceDestroy.cpp,
// MPTokenIssuanceSet.cpp, MPTokenHelpers.cpp::authorizeMPToken (all 3.2.1).
// ---------------------------------------------------------------------------

use crate::ledger::transactor::Transactor;

fn issuance_id_of(tx: &TxFields) -> Option<[u8; 24]> {
    let s = tx.fields.get("MPTokenIssuanceID")?.as_str()?;
    hex::decode(s).ok()?.as_slice().try_into().ok()
}

fn read_json(sandbox: &Sandbox, k: &Hash256) -> Option<serde_json::Value> {
    sandbox.read(k).and_then(|d| serde_json::from_slice(&d).ok())
}

/// Pre-fee balance (do_apply runs after apply_common): rippled's
/// `priorBalance_` — the same convention CheckCash calibrated.
fn prior_balance(sandbox: &Sandbox, tx: &TxFields) -> u64 {
    read_json(sandbox, &keylet::account_root_key(&tx.account))
        .and_then(|a| a["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0)
        .saturating_add(tx.fee)
}

fn owner_count(sandbox: &Sandbox, acct: &[u8; 20]) -> u64 {
    read_json(sandbox, &keylet::account_root_key(acct))
        .and_then(|a| a["OwnerCount"].as_u64())
        .unwrap_or(0)
}

/// MPTokenIssuanceCreate — mint a new issuance object under the creator.
pub struct MPTokenIssuanceCreateTransactor;

impl Transactor for MPTokenIssuanceCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenIssuanceCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // A NON-ZERO TransferFee demands tfMPTCanTransfer; the cap is 50000.
        // A8A08D3E (l105864866) carries an explicit "TransferFee": 0 with no
        // CanTransfer flag and mainnet accepts it — the flag requirement is
        // `fee > 0` only (MPTokenIssuanceCreate.cpp preflight). Further tem
        // checks (metadata length, DomainID rules) unported — no specimen.
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if let Some(f) = tx.fields.get("TransferFee").and_then(|v| v.as_u64()) {
            if f > 50_000 || (f > 0 && flags & LSF_MPT_CAN_TRANSFER == 0) {
                return TxResult::Malformed;
            }
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let oc = owner_count(sandbox, &tx.account);
        if prior_balance(sandbox, tx) < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
            return TxResult::InsufficientReserve;
        }
        // makeMptID(seq, account): seq_be32 || account — the tx's seq value
        // (ticket seq when ticketed), same rule as offer/check keylets.
        let seq = if tx.uses_ticket() { tx.ticket_seq.unwrap_or(0) } else { tx.sequence };
        let mut mptid = [0u8; 24];
        mptid[..4].copy_from_slice(&seq.to_be_bytes());
        mptid[4..].copy_from_slice(&tx.account);
        let ikey = keylet::mpt_issuance_key(&mptid);
        let node = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &ikey);
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & !0x8000_0000;
        let mut obj = serde_json::json!({
            "LedgerEntryType": "MPTokenIssuance",
            "Flags": flags,
            "Issuer": hex::encode(tx.account),
            "OutstandingAmount": "0",
            "OwnerNode": format!("{node:x}"),
            "Sequence": seq,
        });
        // Optional passthroughs, in the ledger's own spellings: MaximumAmount
        // is a UInt64 amount-family field (decimal STRING — the l106134471
        // issuance shows "1000000"); AssetScale/TransferFee are numbers;
        // metadata/domain are hex strings.
        if let Some(m) = tx.fields.get("MaximumAmount") {
            obj["MaximumAmount"] = m.clone();
        }
        // AssetScale/TransferFee/MutableFlags are soeDEFAULT on the ledger
        // object: an explicit zero in the tx (A8A08D3E carries both) is
        // serialized AWAY by rippled — the stored object omits the field.
        for f in ["AssetScale", "TransferFee", "MutableFlags"] {
            match tx.fields.get(f).and_then(|v| v.as_u64()) {
                Some(v) if v > 0 => obj[f] = serde_json::Value::Number(v.into()),
                _ => {}
            }
        }
        for f in ["MPTokenMetadata", "DomainID"] {
            match tx.fields.get(f).and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => obj[f] = serde_json::Value::String(s.to_string()),
                _ => {}
            }
        }
        sandbox.write(ikey, serde_json::to_vec(&obj).unwrap_or_default());
        crate::tx::offer::owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

/// MPTokenIssuanceDestroy — the issuer retires an empty issuance.
pub struct MPTokenIssuanceDestroyTransactor;

impl Transactor for MPTokenIssuanceDestroyTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenIssuanceDestroy" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("MPTokenIssuanceID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = issuance_id_of(tx) else { return TxResult::Malformed };
        let ikey = keylet::mpt_issuance_key(&id);
        let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
        let issuer_ok = iss
            .get("Issuer")
            .and_then(|v| v.as_str())
            .and_then(crate::tx::offer::decode20)
            .map(|i| i == tx.account)
            .unwrap_or(false);
        if !issuer_ok {
            return TxResult::NoPermission;
        }
        if dec_field(&iss, "OutstandingAmount") != 0 || dec_field(&iss, "LockedAmount") != 0 {
            return TxResult::HasObligations;
        }
        let hint = iss
            .get("OwnerNode")
            .and_then(|v| v.as_str())
            .and_then(|s| u64::from_str_radix(s, 16).ok());
        sandbox.delete(ikey);
        crate::ledger::directory::owner_dir_remove(sandbox, &tx.account, &ikey, hint, false);
        crate::tx::offer::owner_count_add(sandbox, &tx.account, -1);
        TxResult::Success
    }
}

/// MPTokenAuthorize — a holder opts in/out of an MPT (creating or deleting
/// their MPToken), or the issuer flips lsfMPTAuthorized on a holder's.
pub struct MPTokenAuthorizeTransactor;

const TF_MPT_UNAUTHORIZE: u64 = 0x0000_0001;

impl Transactor for MPTokenAuthorizeTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenAuthorize" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("MPTokenIssuanceID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = issuance_id_of(tx) else { return TxResult::Malformed };
        let ikey = keylet::mpt_issuance_key(&id);
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let holder = tx
            .fields
            .get("Holder")
            .and_then(|h| h.as_str())
            .and_then(crate::tx::offer::decode20);

        let Some(holder_id) = holder else {
            // HOLDER-side (tx account is the holder).
            let tkey = keylet::mptoken_key(&ikey, &tx.account);
            if flags & TF_MPT_UNAUTHORIZE != 0 {
                // Delete own empty MPToken.
                let Some(tok) = read_json(sandbox, &tkey) else {
                    return TxResult::ObjectNotFound;
                };
                if dec_field(&tok, "MPTAmount") != 0 || dec_field(&tok, "LockedAmount") != 0 {
                    return TxResult::HasObligations;
                }
                let hint = tok
                    .get("OwnerNode")
                    .and_then(|v| v.as_str())
                    .and_then(|s| u64::from_str_radix(s, 16).ok());
                sandbox.delete(tkey);
                crate::ledger::directory::owner_dir_remove(
                    sandbox, &tx.account, &tkey, hint, false,
                );
                crate::tx::offer::owner_count_add(sandbox, &tx.account, -1);
                return TxResult::Success;
            }
            // Create own MPToken (opt in).
            let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
            let is_issuer = iss
                .get("Issuer")
                .and_then(|v| v.as_str())
                .and_then(crate::tx::offer::decode20)
                .map(|i| i == tx.account)
                .unwrap_or(false);
            if is_issuer {
                return TxResult::NoPermission;
            }
            if sandbox.exists(&tkey) {
                return TxResult::Duplicate;
            }
            // Trust-line-style reserve: the first two owned items are free
            // (authorizeMPToken: uOwnerCount < 2 ⇒ zero reserve required).
            let oc = owner_count(sandbox, &tx.account);
            let need = if oc < 2 {
                0
            } else {
                crate::ledger::fees::account_reserve(sandbox, oc + 1)
            };
            if prior_balance(sandbox, tx) < need {
                return TxResult::InsufficientReserve;
            }
            let node = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &tkey);
            let obj = serde_json::json!({
                "LedgerEntryType": "MPToken",
                "Account": hex::encode(tx.account),
                "MPTokenIssuanceID": hex::encode_upper(id),
                "Flags": 0,
                "OwnerNode": format!("{node:x}"),
            });
            sandbox.write(tkey, serde_json::to_vec(&obj).unwrap_or_default());
            crate::tx::offer::owner_count_add(sandbox, &tx.account, 1);
            return TxResult::Success;
        };

        // ISSUER-side (allowlisting): flip lsfMPTAuthorized on the holder's
        // MPToken. Requires the issuance to enforce lsfMPTRequireAuth.
        if !sandbox.exists(&keylet::account_root_key(&holder_id)) {
            return TxResult::NoDst;
        }
        let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
        let is_issuer = iss
            .get("Issuer")
            .and_then(|v| v.as_str())
            .and_then(crate::tx::offer::decode20)
            .map(|i| i == tx.account)
            .unwrap_or(false);
        if !is_issuer {
            return TxResult::NoPermission;
        }
        if iss.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & LSF_MPT_REQUIRE_AUTH == 0 {
            return TxResult::NoAuth;
        }
        let tkey = keylet::mptoken_key(&ikey, &holder_id);
        let Some(mut tok) = read_json(sandbox, &tkey) else { return TxResult::ObjectNotFound };
        if let Some(root) = read_json(sandbox, &keylet::account_root_key(&holder_id)) {
            if ["AMMID", "VaultID", "LoanBrokerID"].iter().any(|f| root.get(f).is_some()) {
                return TxResult::NoPermission; // pseudo-accounts are implicitly authorized
            }
        }
        let fin = tok.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let fout = if flags & TF_MPT_UNAUTHORIZE != 0 {
            fin & !LSF_MPT_AUTHORIZED
        } else {
            fin | LSF_MPT_AUTHORIZED
        };
        tok["Flags"] = serde_json::Value::Number(fout.into());
        sandbox.write(tkey, serde_json::to_vec(&tok).unwrap_or_default());
        TxResult::Success
    }
}

/// MPTokenIssuanceSet — issuer locks/unlocks the issuance (or one holder's
/// MPToken) and adjusts mutable metadata. The DynamicMPT MutableFlags
/// mutation arms are unported (no specimen; the tmf constants would be
/// guesses) — lock/unlock, TransferFee and MPTokenMetadata updates are.
pub struct MPTokenIssuanceSetTransactor;

const TF_MPT_LOCK: u64 = 0x0000_0001;
const TF_MPT_UNLOCK: u64 = 0x0000_0002;
pub const LSF_MPT_CAN_LOCK: u64 = 0x0000_0002;

impl Transactor for MPTokenIssuanceSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "MPTokenIssuanceSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("MPTokenIssuanceID").is_none() {
            return TxResult::Malformed;
        }
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if flags & TF_MPT_LOCK != 0 && flags & TF_MPT_UNLOCK != 0 {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(id) = issuance_id_of(tx) else { return TxResult::Malformed };
        let ikey = keylet::mpt_issuance_key(&id);
        let Some(iss) = read_json(sandbox, &ikey) else { return TxResult::ObjectNotFound };
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let iflags = iss.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if iflags & LSF_MPT_CAN_LOCK == 0 && flags & (TF_MPT_LOCK | TF_MPT_UNLOCK) != 0 {
            return TxResult::NoPermission;
        }
        let is_issuer = iss
            .get("Issuer")
            .and_then(|v| v.as_str())
            .and_then(crate::tx::offer::decode20)
            .map(|i| i == tx.account)
            .unwrap_or(false);
        if !is_issuer {
            return TxResult::NoPermission;
        }
        let holder = tx
            .fields
            .get("Holder")
            .and_then(|h| h.as_str())
            .and_then(crate::tx::offer::decode20);
        let (target_key, mut target) = match holder {
            Some(hid) => {
                if !sandbox.exists(&keylet::account_root_key(&hid)) {
                    return TxResult::NoDst;
                }
                let tk = keylet::mptoken_key(&ikey, &hid);
                let Some(t) = read_json(sandbox, &tk) else {
                    return TxResult::ObjectNotFound;
                };
                (tk, t)
            }
            None => (ikey, iss),
        };
        let fin = target.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let mut fout = fin;
        if flags & TF_MPT_LOCK != 0 {
            fout |= LSF_MPT_LOCKED;
        } else if flags & TF_MPT_UNLOCK != 0 {
            fout &= !LSF_MPT_LOCKED;
        }
        if fout != fin {
            target["Flags"] = serde_json::Value::Number(fout.into());
        }
        // TransferFee / MPTokenMetadata / DomainID follow soeDEFAULT-style
        // updates on the ISSUANCE only: zero/empty removes, non-zero sets.
        if holder.is_none() {
            if let Some(f) = tx.fields.get("TransferFee").and_then(|v| v.as_u64()) {
                if f == 0 {
                    target.as_object_mut().map(|o| o.remove("TransferFee"));
                } else {
                    target["TransferFee"] = serde_json::Value::Number(f.into());
                }
            }
            if let Some(m) = tx.fields.get("MPTokenMetadata").and_then(|v| v.as_str()) {
                if m.is_empty() {
                    target.as_object_mut().map(|o| o.remove("MPTokenMetadata"));
                } else {
                    target["MPTokenMetadata"] = serde_json::Value::String(m.to_string());
                }
            }
        }
        sandbox.write(target_key, serde_json::to_vec(&target).unwrap_or_default());
        TxResult::Success
    }
}
