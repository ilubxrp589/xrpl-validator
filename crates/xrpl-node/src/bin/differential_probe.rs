//! differential_probe — measure the NATIVE Rust transactor engine against
//! mainnet truth (the FFI/libxrpl path already matches, verified separately by
//! `parity_probe` + the backlog gates).
//!
//! For a fixture ledger it replays every tx through the native engine
//! (`xrpl_ledger::tx::dispatch` transactors), builds each tx's (TER, mutation
//! key-set) exactly the way the FFI compare does — `(hex_upper(key), C/M/D
//! byte)` with no-op-Modified filtering — and compares to the mainnet-recorded
//! TER + AffectedNodes in the fixture. Emits a per-tx verdict and a per-type
//! conformance aggregate.
//!
//! This is a MEASUREMENT instrument. It changes no native logic, merges
//! nothing, and never touches the live validator. The native path is a
//! documented reference implementation; this tells us, first-hand, how far it
//! stands from parity — the input to deciding the fully-Rust roadmap.
//!
//! Usage: differential_probe <blobs.txt> <expected.json> [--rpc URL] [--json]
//! Exit: 0 all attempted txs MATCH, 1 some DIVERGE, 2 fixture/setup error.

#![cfg(feature = "ffi")]

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::header::LedgerHeader;
use xrpl_ledger::ledger::keylet;
use xrpl_ledger::ledger::sandbox::{apply_modifications, Sandbox, SandboxEntry};
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::ledger::transactor::{apply_common, TxFields, TxResult};
use xrpl_ledger::tx::dispatch::get_transactor;

const DEFAULT_RPC: &str = "https://s2.ripple.com:51234";

/// Fee-only stub transactors (misc.rs `stub_transactor!`): wired but do_apply
/// makes zero type-specific state changes. Reported as SKIP-STUB so the map is
/// honest rather than showing them as ordinary logic divergences.
const STUB_TYPES: &[&str] = &[
    "TicketCreate", "OracleSet", "OracleDelete", "DIDSet", "DIDDelete",
    "XChainCreateBridge", "XChainCreateClaimID", "XChainCommit", "XChainClaim",
    "XChainModifyBridge", "XChainAccountCreateCommit", "XChainAddClaimAttestation",
    "XChainAddAccountCreateAttestation", "PermissionedDomainSet",
    "PermissionedDomainDelete", "AMMClawback", "MPTokenIssuanceCreate",
    "MPTokenIssuanceDestroy", "MPTokenIssuanceSet", "MPTokenAuthorize",
];

/// Account-valued tx fields the native transactors expect as 20-byte hex.
const ACCOUNT_FIELDS: &[&str] = &["Destination", "Owner", "Authorize", "Unauthorize", "RegularKey"];

fn decode_address(addr: &str) -> Option<[u8; 20]> {
    const ALPHABET: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let mut n: Vec<u8> = vec![0];
    for ch in addr.bytes() {
        let carry = ALPHABET.iter().position(|&c| c == ch)?;
        let mut c = carry;
        for byte in n.iter_mut().rev() {
            c += (*byte as usize) * 58;
            *byte = (c & 0xFF) as u8;
            c >>= 8;
        }
        while c > 0 {
            n.insert(0, (c & 0xFF) as u8);
            c >>= 8;
        }
    }
    let leading = addr.bytes().take_while(|&b| b == b'r').count();
    let mut result = vec![0u8; leading];
    result.extend_from_slice(&n);
    if result.len() < 25 {
        return None;
    }
    let mut id = [0u8; 20];
    id.copy_from_slice(&result[1..21]);
    Some(id)
}

fn rpc(url: &str, method: &str, params: Value) -> Option<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .ok()?;
    let body = client
        .post(url)
        .json(&json!({"method": method, "params": [params]}))
        .send()
        .ok()?
        .json::<Value>()
        .ok()?;
    Some(body["result"].clone())
}

/// Fetch an AccountRoot at `ledger_index` and store it as native JSON (Account
/// field converted to hex; other fields preserved for transactor fidelity).
fn load_account(state: &mut LedgerState, url: &str, addr: &str, ledger_index: u32) {
    let Some(id) = decode_address(addr) else { return };
    let Some(res) = rpc(url, "account_info",
        json!({"account": addr, "ledger_index": ledger_index})) else { return };
    let Some(data) = res.get("account_data") else { return };
    let mut obj = data.clone();
    obj["Account"] = json!(hex::encode(id));
    let key = keylet::account_root_key(&id);
    let _ = state.state_map.insert(key, serde_json::to_vec(&obj).unwrap_or_default());
}

/// Recursively rewrite any base58 classic-address string (`r…`) to 20-byte hex,
/// so a rippled-JSON ledger object matches the native engine's account-field
/// convention (hex). Conservative: only touches strings that decode to a valid
/// 25-byte account payload.
fn hexify_addresses(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.starts_with('r') && s.len() >= 25 && s.len() <= 40 {
                if let Some(id) = decode_address(s) {
                    *v = json!(hex::encode(id));
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(hexify_addresses),
        Value::Object(m) => m.values_mut().for_each(hexify_addresses),
        _ => {}
    }
}

/// Load a ledger object (RippleState, Offer, Check, NFTokenOffer, DirectoryNode,
/// …) that existed at `ledger_index` by its 64-hex ledger index, transcoded to
/// the native account-hex convention. Objects created mid-ledger return
/// entryNotFound and are skipped (native creates them via forward threading).
/// This is what lets native SEE the pre-state a tx references instead of
/// phantom-creating it (2026-07-17: the map was pessimistic without it).
fn load_object(state: &mut LedgerState, url: &str, index_hex: &str, ledger_index: u32) {
    let Ok(kb) = hex::decode(index_hex) else { return };
    if kb.len() != 32 {
        return;
    }
    let Some(res) = rpc(url, "ledger_entry",
        json!({"index": index_hex, "ledger_index": ledger_index})) else { return };
    let Some(mut node) = res.get("node").cloned() else { return }; // entryNotFound → no node
    hexify_addresses(&mut node);
    let mut k = [0u8; 32];
    k.copy_from_slice(&kb);
    let _ = state.state_map.insert(Hash256(k), serde_json::to_vec(&node).unwrap_or_default());
}

/// 20-byte currency code from a JSON currency string (3-char ISO or 40-hex).
fn currency_code(iso: &str) -> [u8; 20] {
    let mut code = [0u8; 20];
    if iso.len() == 3 {
        code[12..15].copy_from_slice(iso.as_bytes());
    } else if iso.len() == 40 {
        if let Ok(b) = hex::decode(iso) {
            if b.len() == 20 {
                code.copy_from_slice(&b);
            }
        }
    }
    code
}

fn decode_issuer(s: &str) -> Option<[u8; 20]> {
    if s.starts_with('r') {
        decode_address(s)
    } else {
        let b = hex::decode(s).ok()?;
        (b.len() == 20).then(|| {
            let mut a = [0u8; 20];
            a.copy_from_slice(&b);
            a
        })
    }
}

/// Keys the native transactor will READ that may be absent from affected-nodes
/// (mainnet didn't modify them). Loading these prevents phantom-creates and
/// gives the directory walk its root pages. Returned as upper-hex strings.
/// Transactor-specific; extend as more types are hardened.
fn native_read_keys(txj: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    if txj["TransactionType"].as_str() == Some("TrustSet") {
        if let (Some(acct), Some(limit)) = (
            txj["Account"].as_str().and_then(decode_address),
            txj.get("LimitAmount"),
        ) {
            if let (Some(issuer), Some(cur)) = (
                limit.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                limit.get("currency").and_then(|v| v.as_str()),
            ) {
                let currency = currency_code(cur);
                keys.push(hex::encode_upper(keylet::ripple_state_key(&acct, &issuer, &currency).0));
                keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
                keys.push(hex::encode_upper(keylet::owner_dir_key(&issuer).0));
            }
        }
    }
    if txj["TransactionType"].as_str() == Some("OfferCancel") {
        if let (Some(acct), Some(seq)) = (
            txj["Account"].as_str().and_then(decode_address),
            txj.get("OfferSequence").and_then(|v| v.as_u64()),
        ) {
            keys.push(hex::encode_upper(keylet::offer_key(&acct, seq as u32).0));
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
    }
    if txj["TransactionType"].as_str() == Some("Payment") {
        let acct = txj["Account"].as_str().and_then(decode_address);
        let dest = txj.get("Destination").and_then(|v| v.as_str()).and_then(decode_address);
        let mut line_for = |who: Option<[u8; 20]>, amt: Option<&Value>, keys: &mut Vec<String>| {
            let (Some(w), Some(a)) = (who, amt) else { return };
            let (Some(iss), Some(cur)) = (
                a.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                a.get("currency").and_then(|v| v.as_str()),
            ) else { return };
            let c = currency_code(cur);
            keys.push(hex::encode_upper(keylet::ripple_state_key(&w, &iss, &c).0));
            keys.push(hex::encode_upper(keylet::owner_dir_key(&iss).0));
        };
        let amt = txj.get("Amount");
        let sm = txj.get("SendMax");
        line_for(acct, amt, &mut keys);
        line_for(dest, amt, &mut keys);
        line_for(acct, sm, &mut keys);
        if let Some(a) = acct {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&a).0));
        }
        if let Some(d) = dest {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&d).0));
        }
    }
    if txj["TransactionType"].as_str() == Some("CheckCash") {
        if let Some(cid) = txj.get("CheckID").and_then(|v| v.as_str()) {
            keys.push(cid.to_uppercase());
        }
        if let Some(acct) = txj["Account"].as_str().and_then(decode_address) {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
    }
    if txj["TransactionType"].as_str() == Some("CheckCreate") {
        if let (Some(acct), Some(dest)) = (
            txj["Account"].as_str().and_then(decode_address),
            txj.get("Destination").and_then(|v| v.as_str()).and_then(decode_address),
        ) {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
            keys.push(hex::encode_upper(keylet::owner_dir_key(&dest).0));
        }
    }
    if txj["TransactionType"].as_str() == Some("OfferCreate") {
        if let Some(acct) = txj["Account"].as_str().and_then(decode_address) {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
        let cur20 = |v: &Value| -> Option<[u8; 20]> {
            match v {
                Value::String(_) => Some([0u8; 20]),
                Value::Object(o) => {
                    let c = o.get("currency")?.as_str()?;
                    if c == "XRP" { return Some([0u8; 20]); }
                    if c.len() == 40 {
                        return <[u8; 20]>::try_from(hex::decode(c).ok()?.as_slice()).ok();
                    }
                    let cb = c.as_bytes();
                    if cb.is_empty() || cb.len() > 8 { return None; }
                    let mut b = [0u8; 20];
                    b[12..12 + cb.len()].copy_from_slice(cb);
                    Some(b)
                }
                _ => None,
            }
        };
        let iss20 = |v: &Value| -> Option<[u8; 20]> {
            match v {
                Value::String(_) => Some([0u8; 20]),
                Value::Object(o) => decode_issuer(o.get("issuer")?.as_str()?),
                _ => None,
            }
        };
        if let (Some(p), Some(g)) = (txj.get("TakerPays"), txj.get("TakerGets")) {
            if let (Some(q), Some(pc), Some(gc), Some(pi), Some(gi)) = (
                keylet::offer_quality(p, g), cur20(p), cur20(g), iss20(p), iss20(g),
            ) {
                let base = keylet::book_base(&pc, &gc, &pi, &gi);
                keys.push(hex::encode_upper(keylet::book_dir_key(&base, q).0));
            }
            // The taker's gets-side trust line decides fundedness for IOU
            // sales — mainnet never touches it on a pure placement, so it
            // must be loaded explicitly or available() starves.
            if let (Some(acct), Some(gobj)) = (txj["Account"].as_str().and_then(decode_address), g.as_object()) {
                if let (Some(gi2), Some(gcs)) = (
                    gobj.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                    gobj.get("currency").and_then(|v| v.as_str()),
                ) {
                    let currency = currency_code(gcs);
                    keys.push(hex::encode_upper(keylet::ripple_state_key(&acct, &gi2, &currency).0));
                }
            }
        }
    }
    if txj["TransactionType"].as_str() == Some("NFTokenCreateOffer") {
        if let Some(acct) = txj["Account"].as_str().and_then(decode_address) {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
        if let Some(nft_id) = txj.get("NFTokenID").and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s).ok())
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        {
            let nft_id = Hash256(nft_id);
            let sell = txj.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0) & 1 != 0;
            let root = if sell {
                keylet::nft_sell_offers_key(&nft_id)
            } else {
                keylet::nft_buy_offers_key(&nft_id)
            };
            keys.push(hex::encode_upper(root.0));
        }
    }
    keys
}

/// Load the directory pages a TrustSet's line removal touches. Never walks the
/// chain (gateway issuers have thousands of pages — a full-chain fetch is
/// thousands of sequential RPCs): reads the line's LowNode/HighNode page
/// hints — rippled's own no-walk design — and loads just those pages. The dir
/// ROOT pages are already loaded via `native_read_keys`.
fn load_trustline_hint_pages(state: &mut LedgerState, url: &str, txj: &Value, ledger_index: u32) {
    if txj["TransactionType"].as_str() != Some("TrustSet") {
        return;
    }
    let (Some(a), Some(la)) = (txj["Account"].as_str(), txj.get("LimitAmount")) else { return };
    let (Some(i), Some(c)) = (la["issuer"].as_str(), la["currency"].as_str()) else { return };
    let Some(res) = rpc(url, "ledger_entry", json!({
        "ripple_state": {"accounts": [a, i], "currency": c},
        "ledger_index": ledger_index
    })) else { return };
    let Some(node) = res.get("node") else { return };
    let hint = |v: Option<&Value>| {
        v.and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())))
            .unwrap_or(0)
    };
    let (Some(acct), Some(issuer)) = (decode_address(a), decode_issuer(i)) else { return };
    let (low, high) = if acct < issuer { (acct, issuer) } else { (issuer, acct) };
    for (owner, n) in [(low, hint(node.get("LowNode"))), (high, hint(node.get("HighNode")))] {
        if n != 0 {
            let root = keylet::owner_dir_key(&owner);
            load_object(state, url, &hex::encode_upper(keylet::dir_page_key(&root, n).0), ledger_index);
        }
    }
}

/// Load the order book a cross-currency Payment crosses. Mainnet meta only
/// shows CONSUMED liquidity — the book a failed (or partially-filling) payment
/// walked is invisible, so native read tecPATH_DRY where mainnet delivered.
/// `book_offers` at `ledger_index` is used purely for KEY DISCOVERY (offer
/// indexes, their directory pages, the makers); every object then loads
/// through `ledger_entry` like the rest of the pre-state. `books_seen` dedups
/// per (pays, gets) pair — pre-state is all at seq-1, so one fetch per book
/// per ledger suffices.
fn load_payment_books(
    state: &mut LedgerState,
    url: &str,
    txj: &Value,
    ledger_index: u32,
    books_seen: &mut HashSet<String>,
) {
    // Payment: spend SendMax to deliver Amount. OfferCreate: spend TakerGets
    // to acquire TakerPays — same crossed book, same AMM pair.
    let (spend, want) = match txj["TransactionType"].as_str() {
        Some("Payment") => {
            let Some(sm) = txj.get("SendMax") else { return };
            (sm, &txj["Amount"])
        }
        Some("OfferCreate") => (&txj["TakerGets"], &txj["TakerPays"]),
        _ => return,
    };
    let (sm, amt) = (spend, want);
    let spec = |v: &Value| -> Option<Value> {
        match v {
            Value::String(_) => Some(json!({"currency": "XRP"})),
            Value::Object(o) => Some(json!({
                "currency": o.get("currency")?.clone(),
                "issuer": o.get("issuer")?.clone(),
            })),
            _ => None,
        }
    };
    let (Some(pays_spec), Some(gets_spec)) = (spec(sm), spec(amt)) else { return };
    if pays_spec == gets_spec {
        return;
    }
    let book_id = format!("{pays_spec}|{gets_spec}");
    if !books_seen.insert(book_id) {
        return;
    }
    // AMM pre-state for the pair: the AMM object, its account, and its asset
    // holdings (trust lines) — none of which appear in the meta of a payment
    // that mainnet filled from the pool but native replays as dry.
    if let Some(res) = rpc(url, "ledger_entry", json!({
        "amm": {"asset": pays_spec, "asset2": gets_spec},
        "ledger_index": ledger_index,
    })) {
        if let Some(idx) = res.get("index").and_then(|v| v.as_str()) {
            load_object(state, url, idx, ledger_index);
        }
        if let Some(node) = res.get("node") {
            if let Some(amm_acct) = node.get("Account").and_then(|v| v.as_str()) {
                load_account(state, url, amm_acct, ledger_index);
                if let Some(aid) = decode_address(amm_acct) {
                    for side in [&pays_spec, &gets_spec] {
                        if let (Some(cur), Some(iss)) = (
                            side.get("currency").and_then(|v| v.as_str()),
                            side.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                        ) {
                            if cur != "XRP" {
                                let key = keylet::ripple_state_key(&aid, &iss, &currency_code(cur));
                                load_object(state, url, &hex::encode_upper(key.0), ledger_index);
                            }
                        }
                    }
                }
            }
        }
    }
    let Some(res) = rpc(url, "book_offers", json!({
        "taker_pays": pays_spec,
        "taker_gets": gets_spec,
        "limit": 10,
        "ledger_index": ledger_index,
    })) else { return };
    let Some(offers) = res.get("offers").and_then(|v| v.as_array()) else { return };
    for off in offers.iter().take(10) {
        if let Some(idx) = off.get("index").and_then(|v| v.as_str()) {
            load_object(state, url, idx, ledger_index);
        }
        if let Some(bd) = off.get("BookDirectory").and_then(|v| v.as_str()) {
            load_object(state, url, bd, ledger_index);
        }
        // Maker funding: available() reads the maker's AccountRoot (XRP
        // sales) or gets-side trust line (IOU sales) — neither appears in a
        // walked-past meta.
        let Some(maker) = off.get("Account").and_then(|v| v.as_str()) else { continue };
        load_account(state, url, maker, ledger_index);
        if let Some(g) = off.get("TakerGets").and_then(|v| v.as_object()) {
            if let (Some(mid), Some(gi), Some(gc)) = (
                decode_address(maker),
                g.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                g.get("currency").and_then(|v| v.as_str()),
            ) {
                let key = keylet::ripple_state_key(&mid, &gi, &currency_code(gc));
                load_object(state, url, &hex::encode_upper(key.0), ledger_index);
            }
        }
    }
}

/// Recursively collect every `"issuer": "r…"` value in a transaction.
fn collect_issuers(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(i) = m.get("issuer").and_then(|x| x.as_str()) {
                if i.starts_with('r') {
                    out.push(i.to_string());
                }
            }
            m.values().for_each(|x| collect_issuers(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| collect_issuers(x, out)),
        _ => {}
    }
}

fn build_txfields(txjson: &Value) -> Option<TxFields> {
    let account = decode_address(txjson["Account"].as_str()?)?;
    let tx_type = txjson["TransactionType"].as_str()?.to_string();
    let fee = txjson["Fee"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let sequence = txjson["Sequence"].as_u64().unwrap_or(0) as u32;
    let ticket_seq = txjson.get("TicketSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
    let last_ledger_seq = txjson.get("LastLedgerSequence").and_then(|v| v.as_u64()).map(|v| v as u32);
    let mut fields = txjson.clone();
    for k in ACCOUNT_FIELDS {
        if let Some(a) = fields.get(*k).and_then(|v| v.as_str()) {
            if a.starts_with('r') {
                if let Some(id) = decode_address(a) {
                    fields[*k] = json!(hex::encode(id));
                }
            }
        }
    }
    Some(TxFields { account, tx_type, fee, sequence, ticket_seq, last_ledger_seq, fields })
}

/// Native per-tx apply, mirroring `apply.rs::apply_transaction_set`'s per-tx
/// branching exactly, but RETURNING (ter_string, mods) instead of committing —
/// so the harness can build the per-tx mutation set. Caller threads the mods
/// forward via `apply_modifications`.
fn native_apply_one(state: &LedgerState, tx: &TxFields) -> (String, HashMap<Hash256, SandboxEntry>) {
    let transactor = match get_transactor(&tx.tx_type) {
        Some(t) => t,
        None => {
            let mut sb = Sandbox::new(state);
            let r = apply_common(tx, &mut sb);
            if r.is_success() {
                return (TxResult::Unsupported.code_str().to_string(), sb.into_modifications());
            }
            return (r.code_str().to_string(), HashMap::new());
        }
    };
    // Phase 1: preflight
    let preflight = transactor.preflight(tx);
    if !preflight.is_success() {
        if preflight.is_claimed() {
            let mut sb = Sandbox::new(state);
            let common = apply_common(tx, &mut sb);
            if common.is_success() {
                return (preflight.code_str().to_string(), sb.into_modifications());
            }
            return (common.code_str().to_string(), HashMap::new());
        }
        return (preflight.code_str().to_string(), HashMap::new());
    }
    // Phase 2+3+4: preclaim → common → do_apply
    let mut sb = Sandbox::new(state);
    let preclaim = transactor.preclaim(tx, &sb);
    if !preclaim.is_success() && !preclaim.is_claimed() {
        return (preclaim.code_str().to_string(), HashMap::new());
    }
    if !preclaim.is_success() {
        let common = apply_common(tx, &mut sb);
        if common.is_success() {
            return (preclaim.code_str().to_string(), sb.into_modifications());
        }
        return (common.code_str().to_string(), HashMap::new());
    }
    let common = apply_common(tx, &mut sb);
    if !common.is_success() {
        return (common.code_str().to_string(), HashMap::new());
    }
    let snap = sb.snapshot();
    let applied = transactor.do_apply(tx, &mut sb);
    if applied.is_success() {
        (TxResult::Success.code_str().to_string(), sb.into_modifications())
    } else if applied.is_claimed() {
        sb.restore_snapshot(snap);
        (applied.code_str().to_string(), sb.into_modifications())
    } else {
        (applied.code_str().to_string(), HashMap::new())
    }
}

/// (hex_upper key, kind byte) set with no-op-Modified filtering (semantic JSON
/// compare of pre vs post), matching the FFI leg's `build_ours_mutation_set`.
fn native_mutset(
    state: &LedgerState,
    mods: &HashMap<Hash256, SandboxEntry>,
) -> HashSet<(String, u8)> {
    let mut set = HashSet::new();
    for (key, entry) in mods {
        let (kind, is_mod, new_bytes): (u8, bool, Option<&Vec<u8>>) = match entry {
            SandboxEntry::Created(d) => (0, false, Some(d)),
            SandboxEntry::Modified(d) => (1, true, Some(d)),
            SandboxEntry::Deleted => (2, false, None),
        };
        if is_mod {
            // drop no-op modifies (post == pre, semantically) — rippled meta omits them
            if let (Some(new), Some(old)) = (new_bytes, state.state_map.lookup(key)) {
                let pn: Option<Value> = serde_json::from_slice(new).ok();
                let po: Option<Value> = serde_json::from_slice(old).ok();
                if pn.is_some() && pn == po {
                    continue;
                }
            }
        }
        set.insert((hex::encode_upper(key.0), kind));
    }
    set
}

struct TypeAgg {
    attempted: u32,
    matched: u32,
    diverge_ter: u32,
    diverge_mut: u32,
    skip_stub: u32,
    skip_unsupported: u32,
}
impl TypeAgg {
    fn new() -> Self {
        Self { attempted: 0, matched: 0, diverge_ter: 0, diverge_mut: 0, skip_stub: 0, skip_unsupported: 0 }
    }
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: differential_probe <blobs.txt> <expected.json> [--rpc URL] [--json]");
        return 2;
    }
    let rpc_url = args.iter().position(|a| a == "--rpc")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| DEFAULT_RPC.to_string());
    let want_json = args.iter().any(|a| a == "--json");

    let expected_json = match std::fs::read_to_string(&args[2]) {
        Ok(s) => s,
        Err(e) => { eprintln!("expected file: {e}"); return 2; }
    };
    let exp: Value = match serde_json::from_str(&expected_json) {
        Ok(v) => v,
        Err(e) => { eprintln!("expected json parse: {e}"); return 2; }
    };
    let hdr = &exp["header"];
    let seq = match hdr["ledger_seq"].as_u64() {
        Some(s) => s as u32,
        None => { eprintln!("missing ledger_seq"); return 2; }
    };
    let txmap = match exp["txs"].as_object() {
        Some(m) => m,
        None => { eprintln!("missing txs"); return 2; }
    };
    // per-tx JSON form (added by fetch_ledger_fixture.py); required for native leg
    let txjson_map = exp["tx_json"].as_object();
    if txjson_map.is_none() {
        eprintln!("fixture lacks 'tx_json' — re-mint with the updated fetch_ledger_fixture.py");
        return 2;
    }
    let txjson_map = txjson_map.unwrap();

    // canonical order by TransactionIndex-equivalent: the tx_json carries "meta"?
    // simplest deterministic order — sort by the tx json's "Sequence" is wrong across
    // accounts; use the order in tx_json_order if present, else the map iteration is
    // non-deterministic. fetch_ledger_fixture.py emits tx_order (hashes in ledger order).
    let order: Vec<String> = exp["tx_order"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_else(|| txjson_map.keys().cloned().collect());

    // Build native base state at seq-1 from account_info of every involved account.
    let header = LedgerHeader {
        sequence: seq.saturating_sub(1),
        total_coins: hdr["total_drops"].as_u64().unwrap_or(100_000_000_000_000_000),
        parent_hash: Hash256([0; 32]),
        transaction_hash: Hash256([0; 32]),
        account_hash: Hash256([0; 32]),
        parent_close_time: hdr["parent_close_time"].as_u64().unwrap_or(0) as u32,
        close_time: hdr["parent_close_time"].as_u64().unwrap_or(0) as u32,
        close_time_resolution: 10,
        close_flags: 0,
    };
    let mut state = LedgerState::new_unverified(header);

    eprintln!("Loading involved accounts at #{} …", seq - 1);
    let mut seen: HashSet<String> = HashSet::new();
    for h in &order {
        let Some(txj) = txjson_map.get(h) else { continue };
        for k in std::iter::once("Account").chain(ACCOUNT_FIELDS.iter().copied()) {
            if let Some(a) = txj.get(k).and_then(|v| v.as_str()) {
                if a.starts_with('r') && seen.insert(a.to_string()) {
                    load_account(&mut state, &rpc_url, a, seq - 1);
                }
            }
        }
        // Every ISSUER named anywhere in the tx (Amount/SendMax/TakerPays/
        // TakerGets/LimitAmount/Asset…). Their AccountRoots carry TickSize
        // and TransferRate, which change offer placement and payment
        // amounts — and a mainnet meta never modifies them, so the oracle
        // cannot correct their absence.
        let mut issuers: Vec<String> = Vec::new();
        collect_issuers(txj, &mut issuers);
        for a in issuers {
            if seen.insert(a.clone()) {
                load_account(&mut state, &rpc_url, &a, seq - 1);
            }
        }
    }
    // Load the pre-state of every object each tx MODIFIED or DELETED (existed at
    // seq-1). Created(0) nodes didn't exist yet — native creates them. This
    // stops native phantom-creating objects it couldn't see.
    let mut obj_seen: HashSet<String> = HashSet::new();
    for h in &order {
        for node in txmap[h]["nodes"].as_array().into_iter().flatten() {
            let (Some(key), Some(kind)) = (node[0].as_str(), node[1].as_u64()) else { continue };
            if kind == 0 {
                continue; // Created mid-ledger — not pre-state
            }
            if obj_seen.insert(key.to_string()) {
                load_object(&mut state, &rpc_url, key, seq - 1);
            }
        }
    }
    // Also load the pre-state of objects native READS but mainnet may not have
    // MODIFIED (so they're absent from affected-nodes) — e.g. a TrustSet on an
    // already-existing line that mainnet no-op'd, or a directory ROOT page
    // native must walk. Without these, native phantom-creates them. Keys are
    // computed from the tx (transactor-specific read-set).
    let mut books_seen: HashSet<String> = HashSet::new();
    for h in &order {
        let Some(txj) = txjson_map.get(h) else { continue };
        for key_hex in native_read_keys(txj) {
            if obj_seen.insert(key_hex.clone()) {
                load_object(&mut state, &rpc_url, &key_hex, seq - 1);
            }
        }
        load_trustline_hint_pages(&mut state, &rpc_url, txj, seq - 1);
        load_payment_books(&mut state, &rpc_url, txj, seq - 1, &mut books_seen);
    }
    eprintln!("Loaded {} pre-state objects.", obj_seen.len());

    // Per-tx native replay + compare to mainnet.
    let mut agg: HashMap<String, TypeAgg> = HashMap::new();
    let mut per_tx: Vec<Value> = Vec::new();
    let mut any_diverge = false;

    for h in &order {
        let Some(txj) = txjson_map.get(h) else { continue };
        let tx_type = txj["TransactionType"].as_str().unwrap_or("?").to_string();
        let net = &txmap[h];
        let net_ter = net["ter"].as_str().unwrap_or("?").to_string();
        let net_mut: HashSet<(String, u8)> = net["nodes"].as_array()
            .map(|a| a.iter().filter_map(|n| Some((n[0].as_str()?.to_string(), n[1].as_u64()? as u8))).collect())
            .unwrap_or_default();

        let e = agg.entry(tx_type.clone()).or_insert_with(TypeAgg::new);

        if get_transactor(&tx_type).is_none() {
            e.skip_unsupported += 1;
            per_tx.push(json!({"hash": h, "type": tx_type, "verdict": "SKIP-UNSUPPORTED"}));
            continue;
        }
        if STUB_TYPES.contains(&tx_type.as_str()) {
            e.skip_stub += 1;
            per_tx.push(json!({"hash": h, "type": tx_type, "verdict": "SKIP-STUB"}));
            continue;
        }
        let Some(txf) = build_txfields(txj) else {
            e.skip_unsupported += 1;
            per_tx.push(json!({"hash": h, "type": tx_type, "verdict": "SKIP-PARSE"}));
            continue;
        };

        let (our_ter, mods) = native_apply_one(&state, &txf);
        let our_mut = native_mutset(&state, &mods);
        // DX_DUMP=<hash prefix>: print the VALUES this tx wrote. The mutation
        // set compares keys only, so a value-level divergence (an offer
        // residual, a line balance) is otherwise invisible — quality is the
        // only value that leaks into a key, via the book page.
        if let Ok(want) = std::env::var("DX_DUMP") {
            if !want.is_empty() && h.starts_with(&want) {
                eprintln!("=== DX_DUMP {h} ({tx_type}) our_ter={our_ter}");
                let mut ks: Vec<_> = mods.iter().collect();
                ks.sort_by_key(|(k, _)| hex::encode_upper(k.0));
                for (k, ent) in ks {
                    let (kind, bytes) = match ent {
                        SandboxEntry::Created(b) => ("CREATED", Some(b)),
                        SandboxEntry::Modified(b) => ("MODIFIED", Some(b)),
                        SandboxEntry::Deleted => ("DELETED", None),
                    };
                    let body = bytes
                        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                        .map(|v| serde_json::to_string(&v).unwrap_or_default())
                        .unwrap_or_default();
                    eprintln!("  {} {kind} {}", hex::encode_upper(k.0), &body[..body.len().min(420)]);
                }
            }
        }
        // ORACLE THREADING: after committing native's mods (which keeps
        // directory Indexes continuity — meta FinalFields OMIT the Indexes
        // array), overlay mainnet's actual post-state (meta NewFields /
        // FinalFields from the fixture) MERGED over the current bytes:
        // oracle wins per-field, fields the meta doesn't carry survive.
        // Plain-replace would wipe no-op-touched nodes (whose meta has no
        // FinalFields at all) down to empty shells. This bounds cascade
        // contamination: every field the meta records is corrected to truth
        // before the next tx.
        let _ = apply_modifications(&mut state, mods);
        if let Some(nodes) = net["nodes"].as_array() {
            let mut overlay: HashMap<Hash256, SandboxEntry> = HashMap::new();
            for n in nodes {
                let (Some(key), Some(kind)) = (n[0].as_str(), n[1].as_u64()) else { continue };
                let Ok(kb) = hex::decode(key) else { continue };
                if kb.len() != 32 { continue; }
                let mut k = [0u8; 32];
                k.copy_from_slice(&kb);
                let hk = Hash256(k);
                if kind == 2 {
                    overlay.insert(hk, SandboxEntry::Deleted);
                    continue;
                }
                let Some(post) = n.get(2).filter(|v| v.is_object()) else { continue };
                let mut post = post.clone();
                hexify_addresses(&mut post);
                let mut base: Value = state.state_map.lookup(&hk)
                    .and_then(|b| serde_json::from_slice(b).ok())
                    .unwrap_or_else(|| json!({}));
                if let (Some(bo), Some(po)) = (base.as_object_mut(), post.as_object()) {
                    for (fk, fv) in po {
                        bo.insert(fk.clone(), fv.clone());
                    }
                }
                if let Ok(bytes) = serde_json::to_vec(&base) {
                    overlay.insert(hk, SandboxEntry::Modified(bytes));
                }
            }
            let _ = apply_modifications(&mut state, overlay);
        }

        e.attempted += 1;
        let ter_ok = our_ter == net_ter;
        let mut_ok = our_mut == net_mut;
        let verdict = if ter_ok && mut_ok {
            e.matched += 1; "MATCH"
        } else if !ter_ok {
            e.diverge_ter += 1; any_diverge = true; "DIVERGE-TER"
        } else {
            e.diverge_mut += 1; any_diverge = true; "DIVERGE-MUT"
        };
        // FULL keys: book directory pages of one book share a 48-hex prefix
        // (book_base || quality), so a truncated key cannot distinguish a
        // page-quality shift from a delete/create of the same page.
        let missing: Vec<String> = net_mut.difference(&our_mut)
            .map(|(k, b)| format!("{k}:{b}")).take(8).collect();
        let extra: Vec<String> = our_mut.difference(&net_mut)
            .map(|(k, b)| format!("{k}:{b}")).take(8).collect();
        per_tx.push(json!({
            "hash": h, "type": tx_type, "verdict": verdict,
            "our_ter": our_ter, "net_ter": net_ter,
            "our_muts": our_mut.len(), "net_muts": net_mut.len(),
            "missing_in_ours": missing, "extra_in_ours": extra,
        }));
    }

    // Report
    if want_json {
        let types: Vec<Value> = agg.iter().map(|(t, a)| json!({
            "type": t, "attempted": a.attempted, "matched": a.matched,
            "diverge_ter": a.diverge_ter, "diverge_mut": a.diverge_mut,
            "skip_stub": a.skip_stub, "skip_unsupported": a.skip_unsupported,
        })).collect();
        println!("{}", json!({"ledger_seq": seq, "per_type": types, "per_tx": per_tx}));
    } else {
        println!("\n=== native conformance for #{seq} ===");
        let mut names: Vec<&String> = agg.keys().collect();
        names.sort();
        for t in names {
            let a = &agg[t];
            println!("  {:<22} attempted={:<3} MATCH={:<3} DIVERGE-TER={:<3} DIVERGE-MUT={:<3} stub={:<3} unsup={}",
                t, a.attempted, a.matched, a.diverge_ter, a.diverge_mut, a.skip_stub, a.skip_unsupported);
        }
        println!("  --- per-tx divergences ---");
        for r in &per_tx {
            let v = r["verdict"].as_str().unwrap_or("");
            if v.starts_with("DIVERGE") {
                println!("  {} {} {}   our_ter={} net_ter={} our_muts={} net_muts={} missing={:?} extra={:?}",
                    v, r["type"].as_str().unwrap_or(""), &r["hash"].as_str().unwrap_or("")[..12],
                    r["our_ter"].as_str().unwrap_or(""), r["net_ter"].as_str().unwrap_or(""),
                    r["our_muts"], r["net_muts"], r["missing_in_ours"], r["extra_in_ours"]);
            }
        }
    }

    let total_attempted: u32 = agg.values().map(|a| a.attempted).sum();
    let total_matched: u32 = agg.values().map(|a| a.matched).sum();
    eprintln!("SUMMARY: {total_matched}/{total_attempted} attempted txs MATCH mainnet (native engine)");
    if total_attempted == 0 {
        return 2;
    }
    if any_diverge { 1 } else { 0 }
}
