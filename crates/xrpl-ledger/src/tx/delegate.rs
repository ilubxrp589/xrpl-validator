//! DelegateSet — grant, change or withdraw another account's permission to
//! sign transactions on this one's behalf (rippled 3.4.0
//! `transactors/delegate/DelegateSet.cpp`, PermissionDelegationV1_1).
//!
//! Finding 360: the amendment reached majority on mainnet 2026-09-21 (then due
//! ~10-05; the majority restarted on 2026-09-24, so it is due ~2026-10-08) and the
//! engine had neither the transactor nor the delegated fee
//! path. Three parts, all here or wired from here:
//!   * DelegateSet: one `Delegate` object per (account, authorized) pair,
//!     keyed `keylet::delegate`, linked into BOTH owner directories
//!     (`OwnerNode` in the delegator's, `DestinationNode` in the delegate's,
//!     so AccountDelete finds inbound delegations), reserve and OwnerCount on
//!     the delegator only; an empty `Permissions` array deletes it.
//!   * the delegated transaction: fee from the delegate, sequence from the
//!     account (`apply_common`), `preFeeBalance_` = the account's untouched
//!     balance (`TxFields::account_fee`).
//!   * AccountDelete removes a delegation from either side through
//!     [`delete_delegate_object`] (rippled `DelegateSet::deleteDelegate`).
//!
//! Permission checks (`checkPermission`, granular sandboxes) end in
//! terNO_DELEGATE_PERMISSION or tem codes — never in a ledger — so a
//! delegated transaction that landed has passed them.

use crate::ledger::directory::{owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use serde_json::{json, Value};
use xrpl_core::types::Hash256;

/// `kPermissionMaxSize` (Protocol.h).
const PERMISSION_MAX: usize = 10;

fn authorize(tx: &TxFields) -> Option<[u8; 20]> {
    crate::tx::offer::decode20(tx.fields.get("Authorize")?.as_str()?)
}

fn permissions(tx: &TxFields) -> Vec<Value> {
    tx.fields.get("Permissions").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// A `PermissionValue` as its u32: a number, or a transaction-type /
/// granular-permission name (tx type code + 1; granular 65537+).
fn permission_value(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => xrpl_core::codec::definitions::permission_value_code(s).ok().map(u64::from),
        _ => None,
    }
}

fn node_of(obj: &Value, field: &str) -> Option<u64> {
    let v = obj.get(field)?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
}

pub struct DelegateSetTransactor;

impl Transactor for DelegateSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        let perms = permissions(tx);
        if perms.len() > PERMISSION_MAX {
            return TxResult::TemArrayTooLarge; // never lands
        }
        let Some(auth) = authorize(tx) else { return TxResult::Malformed };
        if auth == tx.account {
            return TxResult::Malformed; // cannot authorize self
        }
        let mut seen = std::collections::HashSet::new();
        for p in &perms {
            // rippled's JSON names the value ("Payment", "TrustlineAuthorize");
            // a decoded object carries the number. Both mean the same u32.
            let Some(v) = permission_value(&p["Permission"]["PermissionValue"]) else { return TxResult::Malformed };
            if !seen.insert(v) {
                return TxResult::Malformed; // duplicate permission
            }
        }
        // `isDelegable` refusals are temMALFORMED too, and a transaction that
        // landed has passed them, so they are not re-judged here.
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let Some(auth) = authorize(tx) else { return TxResult::Malformed };
        if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
            return TxResult::NoAccount;
        }
        if !sandbox.exists(&keylet::account_root_key(&auth)) {
            return TxResult::NoTarget;
        }
        if crate::tx::misc::is_pseudo_account(sandbox, &auth) {
            return TxResult::TecPseudoAccount;
        }
        if permissions(tx).is_empty() && !sandbox.exists(&keylet::delegate_key(&tx.account, &auth)) {
            return TxResult::NoEntry;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(auth) = authorize(tx) else { return TxResult::Malformed };
        let key = keylet::delegate_key(&tx.account, &auth);
        let perms = permissions(tx);
        if let Some(mut obj) = crate::tx::offer::json_at(sandbox, &key) {
            if perms.is_empty() {
                return delete_delegate_object(sandbox, &key);
            }
            obj["Permissions"] = Value::Array(perms);
            sandbox.write(key, serde_json::to_vec(&obj).unwrap_or_default());
            return TxResult::Success;
        }
        if perms.is_empty() {
            return TxResult::Malformed; // tecINTERNAL: preclaim refused this
        }
        // checkReserve(sleOwner, preFeeBalance_, {ownerCountDelta = 1}).
        let Some(owner) = crate::tx::offer::json_at(sandbox, &keylet::account_root_key(&tx.account)) else {
            return TxResult::NoAccount;
        };
        let bal = owner["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let oc = owner["OwnerCount"].as_u64().unwrap_or(0);
        if bal.saturating_add(tx.account_fee()) < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
            return TxResult::InsufficientReserve;
        }
        let owner_node = owner_dir_insert(sandbox, &tx.account, &key);
        let dest_node = owner_dir_insert(sandbox, &auth, &key);
        let obj = json!({
            "LedgerEntryType": "Delegate",
            "Flags": 0,
            "Account": hex::encode(tx.account),
            "Authorize": hex::encode(auth),
            "Permissions": perms,
            "OwnerNode": format!("{owner_node:x}"),
            "DestinationNode": format!("{dest_node:x}"),
        });
        sandbox.write(key, serde_json::to_vec(&obj).unwrap_or_default());
        crate::tx::offer::owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

/// A granular permission (permissions.macro): its value, the transaction type
/// it applies to, the flags it admits and the fields it adds to the common
/// ones.
struct Granular {
    value: u64,
    tx_type: &'static str,
    flags: u64,
    fields: &'static [&'static str],
}

const TF_UNIVERSAL: u64 = 0xC000_0000; // tfFullyCanonicalSig | tfInnerBatchTxn
const PAYMENT_FIELDS: &[&str] = &["Destination", "Amount", "SendMax", "InvoiceID", "DestinationTag", "CredentialIDs"];
const GRANULAR: &[Granular] = &[
    Granular { value: 65537, tx_type: "TrustSet", flags: TF_UNIVERSAL | 0x0001_0000, fields: &["LimitAmount"] },
    Granular { value: 65538, tx_type: "TrustSet", flags: TF_UNIVERSAL | 0x0010_0000, fields: &["LimitAmount"] },
    Granular { value: 65539, tx_type: "TrustSet", flags: TF_UNIVERSAL | 0x0020_0000, fields: &["LimitAmount"] },
    Granular { value: 65540, tx_type: "AccountSet", flags: TF_UNIVERSAL, fields: &["Domain"] },
    Granular { value: 65541, tx_type: "AccountSet", flags: TF_UNIVERSAL, fields: &["EmailHash"] },
    Granular { value: 65542, tx_type: "AccountSet", flags: TF_UNIVERSAL, fields: &["MessageKey"] },
    Granular { value: 65543, tx_type: "AccountSet", flags: TF_UNIVERSAL, fields: &["TransferRate"] },
    Granular { value: 65544, tx_type: "AccountSet", flags: TF_UNIVERSAL, fields: &["TickSize"] },
    Granular { value: 65545, tx_type: "Payment", flags: TF_UNIVERSAL, fields: PAYMENT_FIELDS },
    Granular { value: 65546, tx_type: "Payment", flags: TF_UNIVERSAL, fields: PAYMENT_FIELDS },
    Granular { value: 65547, tx_type: "MPTokenIssuanceSet", flags: TF_UNIVERSAL | 0x0001, fields: &["MPTokenIssuanceID", "Holder"] },
    Granular { value: 65548, tx_type: "MPTokenIssuanceSet", flags: TF_UNIVERSAL | 0x0002, fields: &["MPTokenIssuanceID", "Holder"] },
];
const PAYMENT_MINT: u64 = 65545;
const PAYMENT_BURN: u64 = 65546;

/// `TxFormats::getCommonFields` — every template admits these.
const COMMON_FIELDS: &[&str] = &[
    "TransactionType", "Flags", "SourceTag", "Account", "Sequence", "PreviousTxnID", "LastLedgerSequence",
    "AccountTxnID", "Fee", "OperationLimit", "Memos", "SigningPubKey", "TicketSequence", "TxnSignature",
    "Signers", "NetworkID", "Delegate", "Sponsor", "SponsorFlags", "SponsorSignature",
];

/// A decimal IOU value ("-1.5e-3", "100") as a Number, for exact ordering.
fn dec(s: &str) -> Option<crate::tx::number::Number> {
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |r| (true, r));
    let (m, e) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int_part, frac) = m.split_once('.').unwrap_or((m, ""));
    let digits = format!("{int_part}{frac}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some(crate::tx::number::Number::ZERO);
    }
    let mantissa: u128 = digits.parse().ok()?;
    crate::tx::number::Number::from_parts(neg, mantissa, e - frac.len() as i32, crate::tx::number::Rounding::ToNearest).ok()
}

fn is_positive(n: &crate::tx::number::Number) -> bool {
    !n.is_zero() && !n.negative
}

/// a <= b, exactly.
fn le(a: crate::tx::number::Number, b: crate::tx::number::Number) -> bool {
    a.sub(b, crate::tx::number::Rounding::ToNearest).is_ok_and(|d| d.is_zero() || d.negative)
}

/// Finding 393 (campaign 23 2-5, 2-6a..d, 3-4): rippled's permission check for
/// a delegated transaction — `invokeCheckPermission` (Transactor.h:336-365)
/// over `checkPermission` (Transactor.cpp:382-399). The Delegate(Account,
/// Delegate) object must exist; a whole-type permission (tx type + 1) admits
/// the transaction outright; otherwise the granular permissions it holds for
/// this type must admit every flag and every field (`checkGranularSandbox`,
/// Permissions.cpp:276-310, the common fields always admitted) and the type's
/// own `checkGranularSemantics` must pass (Payment: direct, non-XRP, a mint
/// or a burn; TrustSet: an existing line, limit unchanged). Anything else is
/// terNO_DELEGATE_PERMISSION — never in a ledger on its own, but a Batch inner
/// refused this way is not applied and not filed (apply.cpp:231, 249-256).
pub fn check_delegate_permission(tx: &TxFields, sandbox: &Sandbox) -> TxResult {
    let Some(delegate) = tx.delegate() else { return TxResult::Success };
    let Some(obj) = crate::tx::offer::json_at(sandbox, &keylet::delegate_key(&tx.account, &delegate)) else {
        return TxResult::NoDelegatePermission;
    };
    let held: Vec<u64> = obj["Permissions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| permission_value(&p["Permission"]["PermissionValue"]))
        .collect();
    let Ok(tt) = xrpl_core::codec::transaction_type_code(&tx.tx_type) else {
        return TxResult::NoDelegatePermission;
    };
    if held.contains(&(u64::from(tt) + 1)) {
        return TxResult::Success;
    }
    let granular: Vec<&Granular> =
        GRANULAR.iter().filter(|g| g.tx_type == tx.tx_type && held.contains(&g.value)).collect();
    if granular.is_empty() {
        return TxResult::NoDelegatePermission;
    }
    // checkGranularSandbox: flags, then fields.
    let allowed_flags = granular.iter().fold(0u64, |a, g| a | g.flags);
    let flags = tx.fields.get("Flags").and_then(Value::as_u64).unwrap_or(0);
    if flags & !allowed_flags != 0 {
        return TxResult::NoDelegatePermission;
    }
    for key in tx.fields.as_object().map(|o| o.keys()).into_iter().flatten() {
        // Only serialized fields count; the API's own keys (hash, metaData,
        // DeliverMax — rippled's alias of Amount) are not transaction fields.
        let is_field = xrpl_core::codec::lookup_field_def(key).is_some_and(|d| d.is_serialized);
        if !is_field || COMMON_FIELDS.contains(&key.as_str()) {
            continue;
        }
        if !granular.iter().any(|g| g.fields.contains(&key.as_str())) {
            return TxResult::NoDelegatePermission;
        }
    }
    let has = |v: u64| granular.iter().any(|g| g.value == v);
    match tx.tx_type.as_str() {
        "Payment" => payment_granular_semantics(tx, sandbox, has(PAYMENT_MINT), has(PAYMENT_BURN)),
        "TrustSet" => trust_set_granular_semantics(tx, sandbox),
        _ => TxResult::Success,
    }
}

/// An amount's asset for the granular same-asset tests: (MPT issuance id,
/// currency, issuer) — all None for XRP.
type AssetKey = (Option<String>, Option<[u8; 20]>, Option<[u8; 20]>);

/// `Payment::checkGranularSemantics` (Payment.cpp:290-374).
fn payment_granular_semantics(tx: &TxFields, sandbox: &Sandbox, mint: bool, burn: bool) -> TxResult {
    let no = TxResult::NoDelegatePermission;
    let amount = &tx.fields["Amount"];
    let asset = |v: &Value| -> Option<AssetKey> {
        if v.is_string() {
            return Some((None, None, None)); // XRP
        }
        if let Some(m) = v.get("mpt_issuance_id").and_then(Value::as_str) {
            return Some((Some(m.to_uppercase()), None, None));
        }
        Some((None, crate::tx::offer::amount_currency20(v), crate::tx::offer::amount_issuer20(v)))
    };
    let Some(amount_asset) = asset(amount) else { return no };
    if let Some(sm) = tx.fields.get("SendMax") {
        if asset(sm) != Some(amount_asset.clone()) {
            return no; // granular permissions are for direct payments only
        }
    }
    if amount.is_string() {
        return no;
    }
    let Some(dest) = tx.fields.get("Destination").and_then(Value::as_str).and_then(crate::tx::offer::decode20) else {
        return no;
    };
    if let Some(mpt) = &amount_asset.0 {
        // The issuance id encodes the issuer: sequence (4 bytes) ‖ issuer (20).
        let Some(issuer) = hex::decode(mpt).ok().filter(|b| b.len() == 24).and_then(|b| <[u8; 20]>::try_from(&b[4..]).ok())
        else {
            return no;
        };
        if mint && issuer == tx.account {
            return TxResult::Success;
        }
        if burn && issuer == dest {
            return TxResult::Success;
        }
        return no;
    }
    let (Some(cur), Some(issuer)) = (amount_asset.1, amount_asset.2) else { return no };
    if issuer != tx.account && issuer != dest {
        return no;
    }
    let Some(line) = crate::tx::offer::json_at(sandbox, &keylet::ripple_state_key(&tx.account, &dest, &cur)) else {
        return no;
    };
    let account_is_low = tx.account < dest;
    let dest_limit = dec(line[if account_is_low { "HighLimit" } else { "LowLimit" }]["value"].as_str().unwrap_or("0"));
    let raw = dec(line["Balance"]["value"].as_str().unwrap_or("0"));
    let (Some(dest_limit), Some(raw)) = (dest_limit, raw) else { return no };
    let account_is_holder = if account_is_low { is_positive(&raw) } else { !raw.is_zero() && raw.negative };
    let may_issue = mint && is_positive(&dest_limit);
    if may_issue && !account_is_holder {
        return TxResult::Success;
    }
    if burn && account_is_holder {
        if !crate::ledger::amendments::fix_cleanup_3_4_0(sandbox) {
            return TxResult::Success;
        }
        // fixCleanup3_4_0: a burn stops at the balance held.
        let held = if account_is_low { raw } else { raw.negated() };
        let Some(amt) = amount["value"].as_str().and_then(dec) else { return no };
        if le(amt, held) || may_issue {
            return TxResult::Success;
        }
    }
    no
}

/// `TrustSet::checkGranularSemantics` (TrustSet.cpp:125-150): granular
/// permissions never create a line and never change its limit.
fn trust_set_granular_semantics(tx: &TxFields, sandbox: &Sandbox) -> TxResult {
    let no = TxResult::NoDelegatePermission;
    let limit = &tx.fields["LimitAmount"];
    let (Some(cur), Some(issuer)) = (crate::tx::offer::amount_currency20(limit), crate::tx::offer::amount_issuer20(limit)) else {
        return no;
    };
    let Some(line) = crate::tx::offer::json_at(sandbox, &keylet::ripple_state_key(&tx.account, &issuer, &cur)) else {
        return no;
    };
    let side = if tx.account > issuer { "HighLimit" } else { "LowLimit" };
    let (Some(cur_limit), Some(want)) = (
        line[side]["value"].as_str().and_then(dec),
        limit["value"].as_str().and_then(dec),
    ) else {
        return no;
    };
    // Same currency and account by construction of the key; the value decides.
    match cur_limit.sub(want, crate::tx::number::Rounding::ToNearest) {
        Ok(d) if d.is_zero() => TxResult::Success,
        _ => no,
    }
}

/// rippled `DelegateSet::deleteDelegate`: unlink the object from the
/// delegator's directory (`OwnerNode`) and, when present, the delegate's
/// (`DestinationNode`) — both with keepRoot FALSE — release one owner count
/// from the DELEGATOR only, and erase it. Shared with AccountDelete, which
/// meets a delegation from either side.
pub fn delete_delegate_object(sandbox: &mut Sandbox, key: &Hash256) -> TxResult {
    let Some(obj) = crate::tx::offer::json_at(sandbox, key) else { return TxResult::NoEntry };
    let (Some(delegator), Some(delegatee)) = (
        obj["Account"].as_str().and_then(crate::tx::offer::decode20),
        obj["Authorize"].as_str().and_then(crate::tx::offer::decode20),
    ) else {
        return TxResult::Malformed;
    };
    owner_dir_remove(sandbox, &delegator, key, node_of(&obj, "OwnerNode"), false);
    if obj.get("DestinationNode").is_some() {
        owner_dir_remove(sandbox, &delegatee, key, node_of(&obj, "DestinationNode"), false);
    }
    crate::tx::offer::owner_count_add(sandbox, &delegator, -1);
    sandbox.delete(*key);
    TxResult::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet;
    use crate::ledger::state::LedgerState;
    use crate::tx::dispatch::apply_on_sandbox;
    use serde_json::{json, Value};

    const A: [u8; 20] = [0xA1; 20]; // delegating account
    const B: [u8; 20] = [0xB2; 20]; // delegate (authorized)
    const C: [u8; 20] = [0xC3; 20]; // bystander
    const FEE: u64 = 12;
    const BASE: u64 = 1_000_000;
    const INC: u64 = 200_000;

    fn root(id: [u8; 20], balance: u64, oc: u64) -> Value {
        json!({"LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": balance.to_string(),
               "Sequence": 10, "OwnerCount": oc, "Flags": 0})
    }

    fn state(accounts: &[([u8; 20], u64, u64)]) -> LedgerState {
        let mut st = LedgerState::new_unverified(LedgerHeader {
            sequence: 10_000, // AccountDelete needs Sequence + 256 behind it
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        });
        let fees = json!({"LedgerEntryType": "FeeSettings", "Flags": 0, "BaseFeeDrops": "10",
                          "ReserveBaseDrops": BASE.to_string(), "ReserveIncrementDrops": INC.to_string()});
        st.state_map.insert(keylet::fee_settings_key(), serde_json::to_vec(&fees).unwrap()).unwrap();
        for (id, bal, oc) in accounts {
            st.state_map.insert(keylet::account_root_key(id), serde_json::to_vec(&root(*id, *bal, *oc)).unwrap()).unwrap();
        }
        st
    }

    fn txf(account: [u8; 20], tx_type: &str, fields: Value) -> TxFields {
        let mut f = json!({"TransactionType": tx_type, "Account": hex::encode(account), "Fee": FEE.to_string()});
        for (k, v) in fields.as_object().unwrap() {
            f[k] = v.clone();
        }
        TxFields {
            account,
            tx_type: tx_type.into(),
            fee: FEE,
            sequence: 10,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: f,
            inner_batch: false,
        }
    }

    fn read(sb: &Sandbox, k: &Hash256) -> Option<Value> {
        sb.read(k).map(|b| serde_json::from_slice(&b).unwrap())
    }
    fn bal(sb: &Sandbox, id: [u8; 20]) -> u64 {
        read(sb, &keylet::account_root_key(&id)).unwrap()["Balance"].as_str().unwrap().parse().unwrap()
    }
    fn field(sb: &Sandbox, id: [u8; 20], f: &str) -> u64 {
        read(sb, &keylet::account_root_key(&id)).unwrap()[f].as_u64().unwrap_or(0)
    }
    fn dir_has(sb: &Sandbox, owner: [u8; 20], k: &Hash256) -> bool {
        read(sb, &keylet::owner_dir_key(&owner)).is_some_and(|d| {
            d["Indexes"].as_array().unwrap().iter().any(|x| x.as_str().unwrap().eq_ignore_ascii_case(&hex::encode(k.0)))
        })
    }
    fn perms(vals: &[u32]) -> Value {
        Value::Array(vals.iter().map(|v| json!({"Permission": {"PermissionValue": v}})).collect())
    }
    fn set(sb: &mut Sandbox, vals: &[u32]) -> String {
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(B), "Permissions": perms(vals)})), sb);
        r.code_str().to_string()
    }

    #[test]
    fn the_delegate_key_is_the_e_space_hash_of_both_accounts() {
        // indexHash(LedgerNameSpace::Delegate = 'E', account, authorized).
        let mut buf = vec![0x00, b'E'];
        buf.extend_from_slice(&A);
        buf.extend_from_slice(&B);
        assert_eq!(keylet::delegate_key(&A, &B), crate::shamap::hash::sha512_half(&buf));
    }

    #[test]
    fn delegate_set_creates_the_object_in_both_directories_and_charges_the_delegator() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1, 65537]), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        let o = read(&sb, &k).expect("the Delegate object");
        assert_eq!(o["LedgerEntryType"], "Delegate");
        assert_eq!(o["Account"].as_str().unwrap().to_lowercase(), hex::encode(A));
        assert_eq!(o["Authorize"].as_str().unwrap().to_lowercase(), hex::encode(B));
        assert_eq!(o["Permissions"], perms(&[1, 65537]));
        assert!(o.get("OwnerNode").is_some() && o.get("DestinationNode").is_some(), "{o}");
        assert!(dir_has(&sb, A, &k) && dir_has(&sb, B, &k), "linked into both owner directories");
        assert_eq!((field(&sb, A, "OwnerCount"), field(&sb, B, "OwnerCount")), (1, 0), "only the delegator pays reserve");
    }

    #[test]
    fn permission_values_arrive_as_names_in_live_json() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let named = json!([{"Permission": {"PermissionValue": "Payment"}}, {"Permission": {"PermissionValue": "TrustlineAuthorize"}}]);
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(B), "Permissions": named})), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert!(read(&sb, &keylet::delegate_key(&A, &B)).is_some());
        // "Payment" is tx type 0 → 1, and the name and the number are the same permission.
        let dup = json!([{"Permission": {"PermissionValue": "Payment"}}, {"Permission": {"PermissionValue": 1}}]);
        let mut sb = Sandbox::new(&st);
        let (r, applied) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(B), "Permissions": dup})), &mut sb);
        assert!(!applied && !r.is_claimed(), "a duplicate by name and number is temMALFORMED: {r:?}");
    }

    #[test]
    fn delegate_set_replaces_the_permissions_of_an_existing_object() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS");
        assert_eq!(set(&mut sb, &[8, 12]), "tesSUCCESS");
        let o = read(&sb, &keylet::delegate_key(&A, &B)).unwrap();
        assert_eq!(o["Permissions"], perms(&[8, 12]));
        assert_eq!(field(&sb, A, "OwnerCount"), 1);
    }

    #[test]
    fn delegate_set_with_no_permissions_deletes_it_from_both_directories() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS");
        assert_eq!(set(&mut sb, &[]), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        assert!(read(&sb, &k).is_none());
        // dirRemove(…, keepRoot = false): the emptied roots go too.
        assert!(read(&sb, &keylet::owner_dir_key(&A)).is_none() && read(&sb, &keylet::owner_dir_key(&B)).is_none());
        assert_eq!(field(&sb, A, "OwnerCount"), 0);
    }

    #[test]
    fn delegate_set_refusals_are_claimed_tecs() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        // Nothing to delete.
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[]), "tecNO_ENTRY");
        assert_eq!(bal(&sb, A), 50_000_000 - FEE);
        // Authorized account missing.
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(C), "Permissions": perms(&[1])})), &mut sb);
        assert_eq!(r.code_str(), "tecNO_TARGET");
        // Authorized account is a pseudo-account (an AMM's).
        let mut st2 = state(&[(A, 50_000_000, 0)]);
        let mut amm = root(C, 1, 1);
        amm["AMMID"] = json!("AB".repeat(32));
        st2.state_map.insert(keylet::account_root_key(&C), serde_json::to_vec(&amm).unwrap()).unwrap();
        let mut sb = Sandbox::new(&st2);
        let (r, _) = apply_on_sandbox(&txf(A, "DelegateSet", json!({"Authorize": hex::encode(C), "Permissions": perms(&[1])})), &mut sb);
        assert_eq!(r.code_str(), "tecPSEUDO_ACCOUNT");
        // Reserve for one more object, judged on the pre-fee balance.
        let st3 = state(&[(A, BASE + INC - 1, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st3);
        assert_eq!(set(&mut sb, &[1]), "tecINSUFFICIENT_RESERVE");
        let st4 = state(&[(A, BASE + INC, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st4);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS", "exactly the reserve pre-fee is enough");
    }

    #[test]
    fn delegate_set_malformations_never_land() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        for f in [
            json!({"Authorize": hex::encode(A), "Permissions": perms(&[1])}),         // self
            json!({"Authorize": hex::encode(B), "Permissions": perms(&[1, 1])}),      // duplicate
            json!({"Authorize": hex::encode(B), "Permissions": perms(&(1..=11).collect::<Vec<_>>())}), // > 10
        ] {
            let mut sb = Sandbox::new(&st);
            let (r, applied) = apply_on_sandbox(&txf(A, "DelegateSet", f.clone()), &mut sb);
            assert!(!applied && !r.is_claimed(), "{f} → {r:?}");
        }
    }

    // ---- a delegated transaction: the delegate pays, the account's sequence moves ----

    #[test]
    fn a_delegated_transaction_charges_the_fee_to_the_delegate() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        let mut t = txf(A, "Payment", json!({"Destination": hex::encode(C), "Amount": "1000000", "Delegate": hex::encode(B)}));
        t.fields["DeliverMax"] = json!("1000000");
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert_eq!(bal(&sb, A), 50_000_000 - 1_000_000, "the account pays the amount, not the fee");
        assert_eq!(bal(&sb, B), 50_000_000 - FEE, "the delegate pays the fee");
        assert_eq!(field(&sb, A, "Sequence"), 11, "the account's sequence is consumed");
        assert_eq!(field(&sb, B, "Sequence"), 10, "the delegate's is not");
        assert_eq!(bal(&sb, C), 51_000_000);
    }

    #[test]
    fn a_delegated_tec_still_charges_the_delegate() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        // DepositPreauth of an account that does not exist: tecNO_TARGET.
        let t = txf(A, "DepositPreauth", json!({"Authorize": hex::encode(C), "Delegate": hex::encode(B)}));
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tecNO_TARGET");
        assert_eq!((bal(&sb, A), bal(&sb, B)), (50_000_000, 50_000_000 - FEE));
        assert_eq!(field(&sb, A, "Sequence"), 11);
    }

    #[test]
    fn a_delegated_reserve_test_uses_the_accounts_untouched_balance() {
        // preFeeBalance_ is the ACCOUNT's balance before the fee; a delegate
        // paid it, so the account's balance IS that pre-fee balance. Adding
        // the fee back (the non-delegated reconstruction) would admit this.
        let st = state(&[(A, BASE + INC - 1, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        let t = txf(A, "DepositPreauth", json!({"Authorize": hex::encode(C), "Delegate": hex::encode(B)}));
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tecINSUFFICIENT_RESERVE");
        let st = state(&[(A, BASE + INC, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&t, &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        assert_eq!(bal(&sb, A), BASE + INC, "and the account still pays no fee");
    }

    // ---- Finding 393: the permission check a delegated Batch inner faces ----

    fn with_delegate_perms(vals: &[u32]) -> LedgerState {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, vals), "tesSUCCESS");
        let mods = sb.into_modifications();
        let mut out = st.clone();
        crate::ledger::sandbox::apply_modifications(&mut out, mods).unwrap();
        out
    }

    fn delegated(tx_type: &str, fields: Value, flags: u64) -> TxFields {
        let mut t = txf(A, tx_type, fields);
        t.fields["Delegate"] = json!(hex::encode(B));
        t.fields["Flags"] = json!(flags);
        t
    }

    #[test]
    fn check_delegate_permission_follows_rippleds_hierarchy() {
        const PAY: u32 = 1; // ttPAYMENT + 1
        const DOMAIN_SET: u32 = 65540; // AccountDomainSet
        let pay = || delegated("Payment", json!({"Destination": hex::encode(C), "Amount": "1000"}), 0);
        // No Delegate object at all.
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0)]);
        assert_eq!(check_delegate_permission(&pay(), &Sandbox::new(&st)), TxResult::NoDelegatePermission);
        // A whole-type permission admits the transaction.
        let st = with_delegate_perms(&[PAY]);
        assert_eq!(check_delegate_permission(&pay(), &Sandbox::new(&st)), TxResult::Success);
        // Not delegated at all: nothing to check.
        let mut own = pay();
        own.fields.as_object_mut().unwrap().remove("Delegate");
        assert_eq!(check_delegate_permission(&own, &Sandbox::new(&st)), TxResult::Success);
        // Granular AccountDomainSet: Domain passes; EmailHash is outside the template.
        let st = with_delegate_perms(&[DOMAIN_SET]);
        let dom = delegated("AccountSet", json!({"Domain": "6578616D706C65"}), 0x4000_0000);
        assert_eq!(check_delegate_permission(&dom, &Sandbox::new(&st)), TxResult::Success, "tfInnerBatchTxn is universal");
        let email = delegated("AccountSet", json!({"EmailHash": "0123456789ABCDEF0123456789ABCDEF"}), 0);
        assert_eq!(check_delegate_permission(&email, &Sandbox::new(&st)), TxResult::NoDelegatePermission);
        let flagged = delegated("AccountSet", json!({"Domain": "AB"}), 0x0001_0000);
        assert_eq!(check_delegate_permission(&flagged, &Sandbox::new(&st)), TxResult::NoDelegatePermission, "a non-universal flag");
        // A granular grant for another type does not reach a Payment.
        assert_eq!(check_delegate_permission(&pay(), &Sandbox::new(&st)), TxResult::NoDelegatePermission);
    }

    #[test]
    fn a_delegated_inner_without_permission_is_not_applied() {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut t = txf(A, "Payment", json!({"Destination": hex::encode(C), "Amount": "1000", "Delegate": hex::encode(B), "Fee": "0"}));
        t.fee = 0;
        t.inner_batch = true;
        let mut sb = Sandbox::new(&st);
        let (r, applied) = apply_on_sandbox(&t, &mut sb);
        assert_eq!((r.code_str(), applied), ("terNO_DELEGATE_PERMISSION", false));
        assert!(sb.modifications().is_empty(), "nothing moved");
    }

    // ---- AccountDelete removes inbound and outbound delegations ----

    /// AccountDelete's fee is one owner reserve (temBAD_FEE below it).
    fn account_delete(who: [u8; 20]) -> TxFields {
        let mut t = txf(who, "AccountDelete", json!({"Destination": hex::encode(C)}));
        t.fee = INC;
        t.fields["Fee"] = json!(INC.to_string());
        t
    }

    fn with_delegation() -> LedgerState {
        let st = state(&[(A, 50_000_000, 0), (B, 50_000_000, 0), (C, 50_000_000, 0)]);
        let mut sb = Sandbox::new(&st);
        assert_eq!(set(&mut sb, &[1]), "tesSUCCESS");
        let mods = sb.into_modifications();
        let mut out = st.clone();
        crate::ledger::sandbox::apply_modifications(&mut out, mods).unwrap();
        out
    }

    #[test]
    fn deleting_the_delegator_removes_its_delegation_from_the_delegates_directory() {
        let st = with_delegation();
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&account_delete(A), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        assert!(read(&sb, &k).is_none());
        assert!(read(&sb, &keylet::owner_dir_key(&B)).is_none(), "B's directory held only the delegation");
    }

    #[test]
    fn deleting_the_delegate_removes_the_delegation_and_frees_the_delegators_reserve() {
        let st = with_delegation();
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&account_delete(B), &mut sb);
        assert_eq!(r.code_str(), "tesSUCCESS");
        let k = keylet::delegate_key(&A, &B);
        assert!(read(&sb, &k).is_none());
        assert!(read(&sb, &keylet::owner_dir_key(&A)).is_none(), "A's directory held only the delegation");
        assert_eq!(field(&sb, A, "OwnerCount"), 0, "the delegator's reserve is released");
    }
}
