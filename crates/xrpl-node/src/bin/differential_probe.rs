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


/// Fee-only stub transactors (misc.rs `stub_transactor!`): wired but do_apply
/// makes zero type-specific state changes. Reported as SKIP-STUB so the map is
/// honest rather than showing them as ordinary logic divergences.
/// Empty since 2026-08-21: every transactor is real. Kept so a future
/// amendment's placeholder has somewhere honest to live.
const STUB_TYPES: &[&str] = &[];

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

/// Transparent disk cache for RPC results. Every query targets a fixed
/// historical `ledger_index`, so responses are immutable across runs — the
/// bottleneck is round-trip latency, not compute. Cache dir defaults to
/// ~/loop/dxcache and is overridable via DX_CACHE (empty string disables).
fn cache_dir() -> Option<std::path::PathBuf> {
    match std::env::var("DX_CACHE") {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(std::path::PathBuf::from(s)),
        Err(_) => {
            let home = std::env::var("HOME").ok()?;
            Some(std::path::PathBuf::from(home).join("loop/dxcache"))
        }
    }
}

/// Cache key for one RPC lookup.
///
/// ⚠ The SERVER is part of the key. One cache directory is shared by every
/// probe run on the box, and they do not all talk to the same node: the
/// nightly scout hydrates from our own .39 (`runner.sh diff <seq>
/// http://10.0.0.39:5005`) while re-probes of older finds must use s2, whose
/// ~17k-ledger window .39 has long since rolled past. Keying on
/// method+params alone let one server's answers be served for the other's
/// question, and for PAGINATED calls that is corrupting rather than merely
/// stale: `account_objects` markers are issued BY a server and meaningless to
/// another, so a .39 marker replayed against s2 (Clio) returns nothing and the
/// walk stops early. #105838164 lost 358 of a seller's 758 NFT pages that way
/// and reported a phantom NFTokenAcceptOffer tecNO_ENTRY; with the server in
/// the key the same ledger is 49/49 clean.
fn cache_key(url: &str, method: &str, params: &Value) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    method.hash(&mut h);
    serde_json::to_string(params).unwrap_or_default().hash(&mut h);
    format!("{method}-{:016x}", h.finish())
}

/// Fetches that failed on the NETWORK, as opposed to objects that genuinely do
/// not exist. Every caller treats `None` as "absent", so without this a dropped
/// fetch becomes an incomplete pre-state and then a plausible-looking
/// divergence — and these results feed `loop/rate.py`, the honest score.
/// See the same fix on the FFI leg (`ffi_engine::RPC_EXHAUSTED`), prompted by
/// #106099077 where a phantom `tecPATH_DRY` came with a triage note blaming the
/// offer-book walk; the fixture replayed CLEAN on re-run.
static RPC_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn rpc(url: &str, method: &str, params: Value) -> Option<Value> {
    let dir = cache_dir();
    let path = dir.as_ref().map(|d| d.join(cache_key(url, method, &params)));
    let dxlog = std::env::var("DX_RPCLOG").is_ok();
    let brief = || {
        let mut b = String::new();
        for k in ["account", "index", "ledger_index", "marker"] {
            if let Some(v) = params.get(k) {
                b.push_str(&format!("{k}={} ", v.to_string().chars().take(24).collect::<String>()));
            }
        }
        b
    };
    if let Some(p) = &path {
        if let Ok(bytes) = std::fs::read(p) {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if dxlog {
                    let err = v.get("result").and_then(|r| r.get("error")).and_then(|e| e.as_str()).unwrap_or("-");
                    eprintln!("DX_RPCLOG {method} {} CACHE-HIT err={err}", brief());
                }
                return Some(v);
            }
        }
    }
    if dxlog {
        eprintln!("DX_RPCLOG {method} {} CACHE-MISS -> live", brief());
    }
    // One shared client: building one per call defeated connection reuse,
    // so every live call paid TCP (and TLS, on the fallback) setup.
    static CLIENT: std::sync::OnceLock<Option<reqwest::blocking::Client>> = std::sync::OnceLock::new();
    let client = match CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .ok()
    }) {
        Some(c) => c.clone(),
        None => {
            RPC_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
    };
    // Retry a transient failure before giving up. `None` from this function is
    // indistinguishable downstream from "the object is not there" — every
    // caller does `else { return }` — so one dropped fetch silently hydrates an
    // incomplete pre-state and the replay then diverges for real-looking
    // reasons. `entryNotFound` does NOT come through here: rippled answers it
    // with a result carrying an `error` field, so it returns Some.
    let mut body = None;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(250 * attempt));
        }
        if let Ok(resp) = client
            .post(url)
            .json(&json!({"method": method, "params": [params]}))
            .send()
        {
            if let Ok(v) = resp.json::<Value>() {
                body = Some(v);
                break;
            }
        }
    }
    let Some(body) = body else {
        RPC_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    };
    let mut result = body["result"].clone();
    // XRPL_PROBE_FALLBACK=<url>: a PRE-WINDOW ledger answers `lgrNotFound` on
    // the primary (.39's online_delete floor moves forward); retry those on a
    // full-history server. Spot-probe tool only — gates leave it unset (public
    // infra is slow, rate-limited, and a gate must not depend on it).
    // `internal` belongs with lgrNotFound: rippled MASKS the not-in-window
    // answer for non-admin clients — .39 tells an admin curl lgrNotFound and
    // tells m3060 `internal` for the same account_info@106275594 (and public
    // s2 says `internal` where Clio-backed xrplcluster serves the data). The
    // lgrNotFound-only trigger meant the fallback NEVER fired from the gates,
    // which is the whole 18-fixture "deterministic" hist residue.
    if matches!(
        result.get("error").and_then(|e| e.as_str()),
        Some("lgrNotFound") | Some("internal")
    ) {
        if let Ok(fb) = std::env::var("XRPL_PROBE_FALLBACK") {
            if !fb.is_empty() && fb != url {
                // Public full-history infra sheds load under fleet pressure: a
                // single dropped attempt leaves the primary's lgrNotFound in
                // place and hydration silently thins (l106281182 re-probed
                // 52/57 while sweep workers shared the s2 pipe — 2428
                // permanently-uncacheable misses). Retry like the primary
                // does, and treat rate-limit answers (slowDown/tooBusy — a
                // well-formed JSON error) as transient too. lgrNotFound FROM
                // the full-history server is settled: the seq itself is bad.
                for attempt in 0..3u64 {
                    if attempt > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(500 * attempt));
                    }
                    if let Ok(resp) = client
                        .post(&fb)
                        .json(&json!({"method": method, "params": [params]}))
                        .send()
                    {
                        if let Ok(v) = resp.json::<Value>() {
                            let r = v["result"].clone();
                            let settled = match r.get("error").and_then(|e| e.as_str()) {
                                None => !r.is_null(),
                                Some("entryNotFound") | Some("actNotFound") | Some("lgrNotFound") => true,
                                Some(_) => false,
                            };
                            result = r;
                            if settled {
                                break;
                            }
                        }
                    }
                }
                if dxlog {
                    let err = result.get("error").and_then(|e| e.as_str()).unwrap_or("-");
                    eprintln!("DX_RPCLOG {method} {} FALLBACK-> err={err}", brief());
                }
            }
        }
    }
    if dxlog {
        // Live-call outcome, fallback or not — the receipt that showed the
        // fallback never firing while .39 demonstrably answers lgrNotFound.
        let err = result.get("error").and_then(|e| e.as_str()).unwrap_or("-");
        eprintln!("DX_RPCLOG {method} {} LIVE-> err={err}", brief());
    }
    // Cache successful lookups AND the two DETERMINISTIC negatives: for a
    // fixed ledger_index, `entryNotFound` and `actNotFound` are as immutable
    // as any answer — the object simply is not in that ledger. Leaving them
    // uncached made every loader miss a LIVE round-trip on every run: a
    // fully warm single-fixture probe measured 77s wall / 8s CPU — 90%
    // network wait — and the gates inherited it (52-min flowdrv2).
    // `lgrNotFound` stays uncached: it depends on the server's history
    // window (and on whether XRPL_PROBE_FALLBACK was set), not the ledger.
    // Other errors stay uncached so a transient failure can't poison the
    // key.
    if let (Some(p), Some(dir)) = (&path, &dir) {
        let err_name = result.get("error").and_then(|e| e.as_str());
        let cacheable = match err_name {
            None => !result.is_null(),
            Some("entryNotFound") | Some("actNotFound") => true,
            Some(_) => false,
        };
        if cacheable {
            let _ = std::fs::create_dir_all(dir);
            if let Ok(bytes) = serde_json::to_vec(&result) {
                let _ = std::fs::write(p, bytes);
            }
        }
    }
    Some(result)
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

/// Load an account's full NFTokenPage chain at `ledger_index` via
/// account_objects — the native page-walk (max page → PreviousPageMin) needs
/// every page present, and mainnet meta only carries the pages a tx touched.
fn load_nft_pages(state: &mut LedgerState, url: &str, addr: &str, ledger_index: u32) {
    let mut marker: Option<Value> = None;
    // Mint farms run to hundreds of pages (752 seen on an xrp.cafe claim
    // account) — paginate until the marker runs dry.
    for _ in 0..60 {
        let mut params = json!({"account": addr, "ledger_index": ledger_index,
               "type": "nft_page", "limit": 400});
        if let Some(m) = &marker {
            params["marker"] = m.clone();
        }
        let Some(res) = rpc(url, "account_objects", params) else { return };
        for obj in res["account_objects"].as_array().into_iter().flatten() {
            let Some(idx) = obj["index"].as_str() else { continue };
            let Ok(kb) = hex::decode(idx) else { continue };
            if kb.len() != 32 { continue; }
            let mut node = obj.clone();
            hexify_addresses(&mut node);
            let mut k = [0u8; 32];
            k.copy_from_slice(&kb);
            let _ = state.state_map.insert(Hash256(k), serde_json::to_vec(&node).unwrap_or_default());
        }
        marker = res.get("marker").filter(|m| !m.is_null()).cloned();
        if marker.is_none() {
            return;
        }
    }
}

/// NFT-page pre-state for the tx types that walk pages: mint/modify/burn need
/// the owner's chain; accept needs both parties' — the counterparty is only
/// discoverable through the offer SLE, fetched here (cached) pre-hexify so
/// its Owner is still base58 for account_objects.
fn load_nft_pages_for_tx(state: &mut LedgerState, url: &str, txj: &Value, ledger_index: u32) {
    match txj["TransactionType"].as_str() {
        Some("NFTokenMint") => {
            if let Some(a) = txj["Account"].as_str() {
                load_nft_pages(state, url, a, ledger_index);
            }
            if let Some(i) = txj.get("Issuer").and_then(|v| v.as_str()) {
                load_nft_pages(state, url, i, ledger_index);
                // ...and the issuer's ACCOUNT ROOT, which carries the
                // `NFTokenMinter` that authorises minting on their behalf.
                // Pages are not enough, and the issuer is not the submitter, so
                // the involved-account loader never fetches it. 26 sweep
                // specimens turned on this one object.
                load_account(state, url, i, ledger_index);
            }
        }
        Some("NFTokenCancelOffer") => {
            // Every id in sfNFTokenOffers: preclaim reads each to decide whether
            // the submitter may cancel it (owner / destination / expired), and
            // an id it cannot read is SKIPPED — so an unhydrated offer makes the
            // permission check pass everything. #106029108 is the specimen.
            if let Some(ids) = txj.get("NFTokenOffers").and_then(|v| v.as_array()) {
                for id in ids.iter().filter_map(|v| v.as_str()) {
                    load_object(state, url, id, ledger_index);
                }
            }
        }
        Some("NFTokenModify") | Some("NFTokenBurn") | Some("NFTokenCreateOffer") => {
            // The ISSUER's AccountRoot, for `NFTokenModify`'s authorisation
            // check: it reads the issuer's `sfNFTokenMinter`, and the issuer is
            // encoded in the NFTokenID (bytes 4..24) rather than named as a
            // transaction field — so the involved-account loader never sees it.
            // Without this the minter test reads nothing and passes everything.
            // #106374615 is the specimen (issuer rK8PZ2r6…, minter rKqqb5QZ…,
            // submitter rLGHuf12… — three different accounts).
            if let Some(idh) = txj.get("NFTokenID").and_then(|v| v.as_str()) {
                if idh.len() == 64 {
                    if let Ok(ib) = hex::decode(&idh[8..48]) {
                        if ib.len() == 20 {
                            let mut id = [0u8; 20];
                            id.copy_from_slice(&ib);
                            let k = keylet::account_root_key(&id);
                            load_object(state, url, &hex::encode_upper(k.0), ledger_index);
                        }
                    }
                }
            }
            // The seller (sfOwner, or the account for a sell offer) must own
            // the token — preclaim walks their pages for the existence check.
            //
            // ...and the CREATOR too, which `Owner`-or-`Account` skipped. On a
            // BUY offer `Owner` is the token's current holder and `Account` is
            // the party who will RECEIVE the token, so it is the creator's page
            // chain that the later accept needs. The accept cannot recover it
            // either: an offer CREATED in this same ledger does not exist at
            // seq-1, so the `ledger_entry` lookup that normally discovers the
            // counterparty finds nothing and silently loads no pages.
            //
            // #106297794 4EF69B14: tx 41 is the buy offer from
            // rhtijaf8wXPM (10 pages), tx 56 the brokered accept. With neither
            // path loading them, `find_page` could not read the max page,
            // `page_insert` fell through to "no pages yet" and CREATED one —
            // mainnet Modifies the existing 81E4939BB07D… page instead. Same
            // result and the same mutation count, one key apart.
            let mut loaded: Vec<&str> = Vec::new();
            for f in ["Owner", "Account"] {
                let Some(o) = txj.get(f).and_then(|v| v.as_str()) else { continue };
                if loaded.contains(&o) {
                    continue;
                }
                loaded.push(o);
                load_nft_pages(state, url, o, ledger_index);
            }
        }
        Some("NFTokenAcceptOffer") => {
            if let Some(a) = txj["Account"].as_str() {
                load_nft_pages(state, url, a, ledger_index);
            }
            for f in ["NFTokenSellOffer", "NFTokenBuyOffer"] {
                let Some(idx) = txj.get(f).and_then(|v| v.as_str()) else { continue };
                let Some(res) = rpc(url, "ledger_entry",
                    json!({"index": idx, "ledger_index": ledger_index})) else { continue };
                if let Some(owner) = res["node"]["Owner"].as_str() {
                    load_nft_pages(state, url, owner, ledger_index);
                    // ...and the owner's ACCOUNT ROOT. The offer's owner is not
                    // named anywhere in the transaction, so the involved-account
                    // loader never sees it — yet `NFTokenAcceptOffer::preclaim`
                    // weighs `accountFunds(bo.Owner)` against the offer, and a
                    // balance nobody loaded reads as "no opinion". That is how
                    // the funds check silently did nothing on first attempt.
                    load_account(state, url, owner, ledger_index);
                }
            }
        }
        _ => {}
    }
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
    // A Payment to an lsfDepositAuth destination is refused unless a
    // DepositPreauth(dst, src) object exists — and that object is READ, never
    // written, so a payment it ALLOWS touches it in no metadata and the
    // modified/deleted loader never fetches it.
    //
    // ⚠ This is the vacuous-check trap INVERTED, and the dangerous direction:
    // an unhydrated preauth does not silently disable the gate, it makes the
    // gate FIRE on a payment mainnet allows. Hydrate it with the check, never
    // after.
    if txj["TransactionType"].as_str() == Some("Payment") {
        if let (Some(src), Some(dst)) = (
            txj["Account"].as_str().and_then(decode_address),
            txj["Destination"].as_str().and_then(decode_address),
        ) {
            keys.push(hex::encode_upper(keylet::deposit_preauth_key(&dst, &src).0));
        }
    }
    // NFTokenAcceptOffer names its offers by INDEX, and the same trap the
    // credential block below describes applies with full force: a `tec` that
    // claims nothing but the fee MODIFIES neither offer, so the
    // modified/deleted loader hydrates neither, and the engine is asked to
    // judge a transaction whose offers it cannot see. It can only answer
    // tecOBJECT_NOT_FOUND.
    //
    // #106295345 A197E2D3 and six siblings across the 2026-08-14 fresh sweep
    // were all this blind spot — mainnet tecINSUFFICIENT_FUNDS against our
    // tecOBJECT_NOT_FOUND — and no engine change could have fixed them while
    // the offers were absent from the sandbox. They were 7 of the 13
    // divergences in that sweep, the single largest cluster.
    if txj["TransactionType"].as_str() == Some("NFTokenAcceptOffer") {
        for f in ["NFTokenBuyOffer", "NFTokenSellOffer"] {
            if let Some(idx) = txj.get(f).and_then(|v| v.as_str()) {
                keys.push(idx.to_ascii_uppercase());
            }
        }
    }
    // Credential transactors read keylet::credential(subject, issuer, type)
    // before doing anything. When the answer is "it already exists" the
    // transaction is fee-only, so the object never appears in mainnet's
    // affected-nodes and would go unhydrated — native would then see no
    // duplicate and create one (#105784451 8AA123A9, #105784776 07452ED7,
    // both tecDUPLICATE on mainnet).
    if matches!(
        txj["TransactionType"].as_str(),
        Some("CredentialCreate") | Some("CredentialAccept") | Some("CredentialDelete")
    ) {
        // The credential joins BOTH owner directories, so both roots must be
        // present for dir_insert to find the real chain instead of inventing a
        // root. #105909285 68872086F0B4 lands on a TAIL page of the issuer's
        // multi-page dir (1D0093BB, IndexPrevious 3).
        for f in ["Account", "Subject", "Issuer"] {
            if let Some(a) = txj.get(f).and_then(|v| v.as_str()).and_then(decode_address) {
                keys.push(hex::encode_upper(keylet::owner_dir_key(&a).0));
            }
        }
        // Whichever party the transaction does not name is the sender:
        // CredentialCreate carries Subject and issues as Account, while
        // CredentialAccept carries Issuer and is accepted by the Subject.
        let account = txj["Account"].as_str().and_then(decode_address);
        let issuer = txj
            .get("Issuer")
            .and_then(|v| v.as_str())
            .and_then(decode_address)
            .or(account);
        let subject = txj
            .get("Subject")
            .and_then(|v| v.as_str())
            .and_then(decode_address)
            .or(account);
        if let (Some(subject), Some(issuer), Some(ct)) = (
            subject,
            issuer,
            txj.get("CredentialType").and_then(|v| v.as_str()),
        ) {
            let ct_bytes = hex::decode(ct).unwrap_or_else(|_| ct.as_bytes().to_vec());
            let mut buf = Vec::with_capacity(2 + 20 + 20 + ct_bytes.len());
            buf.extend_from_slice(&[0x00, 0x44]); // LedgerNameSpace::Credential
            buf.extend_from_slice(&subject);
            buf.extend_from_slice(&issuer);
            buf.extend_from_slice(&ct_bytes);
            keys.push(hex::encode_upper(
                xrpl_ledger::shamap::hash::sha512_half(&buf).0,
            ));
        }
    }
    // Both rest an NFTokenOffer in the sender's owner directory — a mint only
    // when it carries `Amount` (featureNFTokenMintOffer), CreateOffer always.
    // The dir ROOT has to be here for load_owner_dir_tail to read its
    // IndexPrevious and pull the tail page; without the root the tail lookup
    // silently no-ops and native invents a fresh root. #105815415 D44804DCF372:
    // mainnet Modifies rKqqb5QZ's tail 215726948B, we Created root 8329A644.
    if matches!(
        txj["TransactionType"].as_str(),
        Some("NFTokenCreateOffer") | Some("NFTokenMint")
    ) {
        if let Some(acct) = txj["Account"].as_str().and_then(decode_address) {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
    }
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
        // PATH HOPS. We hydrated only the Amount and SendMax currencies, never
        // the intermediate ones named in `Paths` — so a payment that ripples
        // through a third currency saw no trust line for that hop, ran
        // trustCreate for a line that already exists, and invented a directory
        // page to hold it.
        //
        // #105831615 D58D4A73BA32 pays itself PHNIX from XRP via a BTC hop
        // (rBitcoiN). Mainnet's 6 nodes are ALL Modified — no CreatedNode at
        // all — while we Created 26F61E51, a DirectoryNode whose single entry
        // is the sender's existing BTC line 506D09E5. The same phantom key
        // turned up in four payments across three ledgers, which is what marked
        // it as a harness gap rather than engine arithmetic.
        if let Some(paths) = txj.get("Paths").and_then(|v| v.as_array()) {
            for path in paths.iter().filter_map(|p| p.as_array()) {
                for step in path {
                    let Some(iss) = step
                        .get("issuer")
                        .and_then(|v| v.as_str())
                        .and_then(decode_issuer)
                    else { continue };
                    keys.push(hex::encode_upper(keylet::owner_dir_key(&iss).0));
                    let Some(cur) = step.get("currency").and_then(|v| v.as_str()) else { continue };
                    let c = currency_code(cur);
                    // The hop can be held by either end of the payment, and by
                    // an account the step names outright.
                    for who in [acct, dest, step.get("account").and_then(|v| v.as_str()).and_then(decode_address)]
                        .into_iter()
                        .flatten()
                    {
                        keys.push(hex::encode_upper(keylet::ripple_state_key(&who, &iss, &c).0));
                        keys.push(hex::encode_upper(keylet::owner_dir_key(&who).0));
                    }
                }
            }
        }
    }
    if txj["TransactionType"].as_str() == Some("TicketCreate") {
        if let Some(acct) = txj["Account"].as_str().and_then(decode_address) {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
    }
    if matches!(txj["TransactionType"].as_str(), Some("OracleSet") | Some("OracleDelete")) {
        if let (Some(acct), Some(doc)) = (
            txj["Account"].as_str().and_then(decode_address),
            txj.get("OracleDocumentID").and_then(|v| v.as_u64()),
        ) {
            keys.push(hex::encode_upper(keylet::oracle_key(&acct, doc as u32).0));
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
    }
    if txj["TransactionType"].as_str() == Some("EscrowCreate") {
        // An escrow is listed in the sender's owner directory, the
        // destination's, and — for an IOU — the ISSUER's. Every one of those
        // roots has to be present or the append invents a fresh directory
        // (load_owner_dir_tail then fetches the tail page). #105823810
        // 6AB38288 escrows STSH whose issuer has a MULTI-PAGE owner dir:
        // without it we Created root D95F419E where mainnet Modified page
        // FC62D551. Same shape as the CheckCash gap fixed in 7eb0dca.
        for f in ["Account", "Destination"] {
            if let Some(a) = txj[f].as_str().and_then(decode_address) {
                keys.push(hex::encode_upper(keylet::owner_dir_key(&a).0));
            }
        }
        if let Some(iss) = txj
            .get("Amount")
            .and_then(|a| a.get("issuer"))
            .and_then(|v| v.as_str())
            .and_then(decode_issuer)
        {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&iss).0));
            // The SENDER's and DESTINATION's trust lines for the escrowed
            // IOU: escrowCreatePreclaimHelper reads both (tecNO_LINE, the
            // frozen tests, and the funds test), and a fee-only refusal's
            // meta touches neither. Without this the sender's 1e9 holding
            // read as NOTHING and the engine answered tecUNFUNDED_PAYMENT
            // where mainnet's refusal was the doApply time rule
            // (#106261496 C9BB730F and 13 siblings).
            if let Some(cur) = txj
                .get("Amount")
                .and_then(|a| a.get("currency"))
                .and_then(|v| v.as_str())
            {
                for f in ["Account", "Destination"] {
                    if let Some(who) = txj[f].as_str().and_then(decode_address) {
                        keys.push(hex::encode_upper(
                            keylet::ripple_state_key(&who, &iss, &currency_code(cur)).0,
                        ));
                    }
                }
            }
        }
    }
    if txj["TransactionType"].as_str() == Some("CheckCash") {
        if let Some(cid) = txj.get("CheckID").and_then(|v| v.as_str()) {
            keys.push(cid.to_uppercase());
        }
        if let Some(acct) = txj["Account"].as_str().and_then(decode_address) {
            keys.push(hex::encode_upper(keylet::owner_dir_key(&acct).0));
        }
        // The cashed IOU's issuer co-owns the new trust line, so its owner
        // directory root must be loaded too — otherwise the append invents a
        // fresh directory (load_owner_dir_tail then loads its last page).
        for f in ["DeliverMin", "Amount"] {
            if let Some(iss) = txj.get(f)
                .and_then(|a| a.get("issuer"))
                .and_then(|v| v.as_str())
                .and_then(decode_issuer)
            {
                keys.push(hex::encode_upper(keylet::owner_dir_key(&iss).0));
            }
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
        // The ISSUERS' owner dirs too, not just the account's. A crossing that
        // hands the taker an IOU for the first time creates a trust line, which
        // is inserted into BOTH sides' directories — and an IOU issuer's is
        // enormous. #105920238 483655735D99, #105923760 D62C0C4874E8 and
        // #105936921 8D89EF468CFB (three different ledgers) all Created the
        // SAME root 72D151A0 — rMxCKbED, the RLUSD issuer, whose directory runs
        // to IndexPrevious 0xe92, i.e. 3730 pages — where mainnet Modified a
        // tail page. A key that is constant across unrelated ledgers is always
        // this: our view of a multi-page directory, never engine arithmetic.
        for f in ["TakerPays", "TakerGets"] {
            if let Some(iss) = txj
                .get(f)
                .and_then(|a| a.get("issuer"))
                .and_then(|v| v.as_str())
                .and_then(decode_issuer)
            {
                keys.push(hex::encode_upper(keylet::owner_dir_key(&iss).0));
            }
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
                let domain = txj.get("DomainID").and_then(|v| v.as_str())
                    .and_then(|s| hex::decode(s).ok())
                    .filter(|b| b.len() == 32)
                    .map(|b| {
                        let mut d = [0u8; 32];
                        d.copy_from_slice(&b);
                        Hash256(d)
                    });
                let base = match &domain {
                    Some(d) => keylet::book_base_domain(&pc, &gc, &pi, &gi, d),
                    None => keylet::book_base(&pc, &gc, &pi, &gi),
                };
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

/// Load the LAST page of an owner directory about to receive a new object. A tx
/// that merely appends a trust line lands it on the dir's last page
/// (root.IndexPrevious), which the meta never carries — without it dir_insert
/// cannot see the existing (multi-page) directory and invents a fresh root. The
/// dir ROOTS are already loaded via native_read_keys. #105798519 CheckCash
/// 8FBBA125: the cashed SAM line appends to the SAM issuer's 58-page dir (root
/// D2215EC9, last page BA1033D3); mainnet Modifies BA1033D3, we Created D2215EC9.
/// The whole owner-directory CHAIN plus every object it lists.
///
/// `AccountDelete::preclaim` decides tecHAS_OBLIGATIONS by walking the
/// directory and reading each entry's LedgerEntryType — and a refusal touches
/// NOTHING, so neither the pages nor the objects appear in mainnet's metadata
/// and the modified/deleted loader fetches none of them. #106322004 77C5E61D
/// is that: OwnerCount 0 with one Escrow, invisible to us, so we deleted an
/// account mainnet keeps.
fn load_owner_dir_chain(state: &mut LedgerState, url: &str, owner: &[u8; 20], ledger_index: u32) {
    let root = keylet::owner_dir_key(owner);
    let mut key = root;
    for _ in 0..64 {
        let idx_hex = hex::encode_upper(key.0);
        let Some(res) = rpc(url, "ledger_entry",
            json!({"index": idx_hex, "ledger_index": ledger_index})) else { return };
        let Some(node) = res.get("node") else { return };
        let mut page = node.clone();
        hexify_addresses(&mut page);
        let _ = state.state_map.insert(key, serde_json::to_vec(&page).unwrap_or_default());
        for i in node["Indexes"].as_array().into_iter().flatten() {
            if let Some(s) = i.as_str() {
                load_object(state, url, s, ledger_index);
            }
        }
        let next = node
            .get("IndexNext")
            .and_then(|v| {
                v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
            })
            .unwrap_or(0);
        if next == 0 {
            return;
        }
        key = keylet::dir_page_key(&root, next);
    }
}

fn load_owner_dir_tail(state: &mut LedgerState, url: &str, txj: &Value, ledger_index: u32) {
    if txj["TransactionType"].as_str() == Some("AccountDelete") {
        if let Some(a) = txj["Account"].as_str().and_then(decode_address) {
            load_owner_dir_chain(state, url, &a, ledger_index);
        }
    }
    let mut owners: Vec<[u8; 20]> = Vec::new();
    if txj["TransactionType"].as_str() == Some("EscrowCreate") {
        for f in ["Account", "Destination"] {
            if let Some(a) = txj[f].as_str().and_then(decode_address) {
                owners.push(a);
            }
        }
        if let Some(iss) = txj
            .get("Amount")
            .and_then(|a| a.get("issuer"))
            .and_then(|v| v.as_str())
            .and_then(decode_issuer)
        {
            owners.push(iss);
        }
    }
    // Credentials append to BOTH parties' owner directories, and an issuer's
    // can be many pages long — only the tail receives the insert.
    if matches!(
        txj["TransactionType"].as_str(),
        Some("CredentialCreate") | Some("CredentialAccept") | Some("CredentialDelete")
    ) {
        for f in ["Account", "Subject", "Issuer"] {
            if let Some(a) = txj.get(f).and_then(|v| v.as_str()).and_then(decode_address) {
                owners.push(a);
            }
        }
    }
    // A crossing OfferCreate can hand the taker an IOU it has never held,
    // creating a trust line that appends to the ISSUER's owner directory —
    // which for a real issuer is thousands of pages long. Only the tail page
    // (root.IndexPrevious) receives it, and the meta never carries that page.
    if txj["TransactionType"].as_str() == Some("OfferCreate") {
        if let Some(a) = txj["Account"].as_str().and_then(decode_address) {
            owners.push(a);
        }
        for f in ["TakerPays", "TakerGets"] {
            if let Some(iss) = txj
                .get(f)
                .and_then(|a| a.get("issuer"))
                .and_then(|v| v.as_str())
                .and_then(decode_issuer)
            {
                owners.push(iss);
            }
        }
    }
    // A mint carrying `Amount` rests a sell offer (featureNFTokenMintOffer),
    // and NFTokenCreateOffer always rests one — both append to the owner's
    // directory. #105815415 D44804DCF372: rKqqb5QZ is a prolific minter whose
    // owner dir is multi-page (root 8329A644, IndexNext aee9 / IndexPrevious
    // d2eb); mainnet Modifies the tail 215726948B, we Created the root.
    if matches!(
        txj["TransactionType"].as_str(),
        Some("NFTokenCreateOffer") | Some("NFTokenMint")
    ) {
        if let Some(a) = txj["Account"].as_str().and_then(decode_address) {
            owners.push(a);
        }
    }
    if txj["TransactionType"].as_str() == Some("CheckCash") {
        if let Some(a) = txj["Account"].as_str().and_then(decode_address) {
            owners.push(a);
        }
        for f in ["DeliverMin", "Amount"] {
            if let Some(iss) = txj.get(f)
                .and_then(|a| a.get("issuer"))
                .and_then(|v| v.as_str())
                .and_then(decode_issuer)
            {
                owners.push(iss);
            }
        }
    }
    for owner in owners {
        let root = keylet::owner_dir_key(&owner);
        let last = state.state_map.lookup(&root).and_then(|b| {
            serde_json::from_slice::<Value>(b).ok().and_then(|v| {
                v.get("IndexPrevious").and_then(|p| {
                    p.as_u64().or_else(|| p.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                })
            })
        });
        if let Some(n) = last.filter(|n| *n != 0) {
            let tail = keylet::dir_page_key(&root, n);
            load_object(state, url, &hex::encode_upper(tail.0), ledger_index);
            // ...and the page BEFORE the tail. If this tx empties the tail it
            // is DELETED (`ApplyView::dirRemove` erases any empty non-root
            // page and relinks), and the following insert lands on whatever is
            // the tail THEN. Unknown, we invent it — losing however many
            // entries it really holds.
            //
            // #105886344 E430B7E92B22: rGV6cX cancels its own offer, the only
            // entry on tail page 0x12f2e (890F7D4B), and rests a new one.
            // rippled erases 0x12f2e, finds page 0x12f2d (828BB495) FULL at 32
            // entries, and so allocates 0x12f2e again — the same key, which
            // the state table folds into a plain Modify. We had never loaded
            // 0x12f2d, so we appended into a phantom empty page and emitted it
            // Created, plus a Deleted tail and a Modified root: 9 mutations
            // against 7. `parity_probe` stays CLEAN throughout because libxrpl
            // reads through a callback view and just fetches the page.
            let prev = state.state_map.lookup(&tail).and_then(|b| {
                serde_json::from_slice::<Value>(b).ok().and_then(|v| {
                    v.get("IndexPrevious").and_then(|p| {
                        p.as_u64().or_else(|| p.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                    })
                })
            });
            if let Some(m) = prev.filter(|m| *m != 0 && *m != n) {
                let pk = hex::encode_upper(keylet::dir_page_key(&root, m).0);
                load_object(state, url, &pk, ledger_index);
            }
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
    // Payment: spend SendMax to deliver Amount — possibly through the FIRST
    // path's intermediate currencies (multi-hop strand: one book per adjacent
    // pair). OfferCreate: spend TakerGets to acquire TakerPays.
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
    // ONE CHAIN PER NAMED PATH. This built its chain from `paths.first()`
    // alone, mirroring the engine's own old limitation — so once the engine
    // started flowing a strand per path, every strand past the first replayed
    // against a pre-state that had no book and no POOL for its hops, and read
    // as dry no matter what the engine did.
    //
    // #106156904 341030105165: SendMax 7.203546 USDT for 0.095 SOL over six
    // paths. Mainnet routes path 2, USDT->XRPS->SOL, and both hops are AMM
    // POOLS (rLxvTCgtXMdA fee 204, rK7EaS5bTdZc fee 402) — which is why its
    // meta carries seven RippleStates and not one Offer. Neither pool was
    // hydrated, `amm_swap::discover` returned None, and DX_AMM printed nothing
    // at all for the transaction.
    let mut chains: Vec<Vec<Value>> = Vec::new();
    match txj["TransactionType"].as_str() {
        Some("Payment") => {
            let Some(sm) = txj.get("SendMax") else { return };
            let (Some(s), Some(w)) = (spec(sm), spec(&txj["Amount"])) else { return };
            // PURE-ACCOUNT paths (rippling): the engine's DirectStepI hops
            // read every MUTUAL line along the account sequence plus each
            // account's TransferRate, and a fee-only refusal's meta touches
            // none of them. The sequence is computed by the SAME normalizer
            // the engine uses (direct_step::pure_account_sequence), so the
            // check and its hydration cannot drift.
            {
                let iou_iss = |v: &Value| -> Option<[u8; 20]> {
                    v.get("issuer").and_then(|i| i.as_str()).and_then(decode_address)
                };
                let iou_cur = |v: &Value| -> Option<String> {
                    v.get("currency").and_then(|c| c.as_str()).map(str::to_string)
                };
                if let (Some(src), Some(dst), Some(smi), Some(di), Some(c1), Some(c2)) = (
                    txj["Account"].as_str().and_then(decode_address),
                    txj["Destination"].as_str().and_then(decode_address),
                    iou_iss(sm),
                    iou_iss(&txj["Amount"]),
                    iou_cur(sm),
                    iou_cur(&txj["Amount"]),
                ) {
                    if c1 == c2 {
                        let cur20 = currency_code(&c1);
                        // The DEFAULT strand's account sequence too (els = []):
                        // a no-Paths same-currency payment rests entirely on
                        // the mutual lines of src/[issuers]/dst, and a
                        // fee-only refusal's meta names none of them.
                        let empty: Vec<Value> = Vec::new();
                        let default_and_named = std::iter::once(empty.as_slice()).chain(
                            txj["Paths"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|p| p.as_array())
                                .map(|v| v.as_slice()),
                        );
                        for p in default_and_named {
                            let Some(seq) = xrpl_ledger::tx::direct_step::pure_account_sequence(
                                &src, &dst, &di, &smi, p,
                            ) else {
                                continue;
                            };
                            for a in &seq {
                                // By ledger index — no base58 round-trip.
                                let k = hex::encode_upper(keylet::account_root_key(a).0);
                                load_object(&mut *state, url, &k, ledger_index);
                            }
                            for wpair in seq.windows(2) {
                                let k = hex::encode_upper(
                                    keylet::ripple_state_key(&wpair[0], &wpair[1], &cur20).0,
                                );
                                load_object(&mut *state, url, &k, ledger_index);
                            }
                        }
                    }
                }
            }
            // MIXED paths (runs + books): hydrate every run hop's mutual
            // line and both accounts, and push each book hop's spec pair
            // into `chains` so the book/AMM loader below fetches the
            // RE-ANCHORED books — the ones the runs walk the value into
            // (#106311829: the real crossed book is USD.rvYAfWj, an issuer
            // that appears nowhere in the tx as a currency element's
            // issuer). Same normalizer as the engine.
            {
                use xrpl_ledger::tx::direct_step as ds;
                let leg_of_spec = |v: &Value| -> Option<xrpl_ledger::tx::offer::Leg> {
                    match v {
                        Value::String(_) => Some(xrpl_ledger::tx::offer::Leg {
                            xrp: true, cur: [0u8; 20], issuer: [0u8; 20],
                        }),
                        Value::Object(o) => {
                            let c = o.get("currency")?.as_str()?;
                            let iss = o.get("issuer")?.as_str().and_then(decode_address)?;
                            Some(xrpl_ledger::tx::offer::Leg {
                                xrp: false, cur: currency_code(c), issuer: iss,
                            })
                        }
                        _ => None,
                    }
                };
                let spec_of_leg = |l: &xrpl_ledger::tx::offer::Leg| -> Value {
                    if l.xrp {
                        json!({"currency": "XRP"})
                    } else {
                        let c = std::str::from_utf8(&l.cur[12..15])
                            .ok()
                            .filter(|s| s.chars().all(|ch| ch.is_ascii_alphanumeric()) && l.cur[..12] == [0u8; 12] && l.cur[15..] == [0u8; 5])
                            .map(str::to_string)
                            .unwrap_or_else(|| hex::encode_upper(l.cur));
                        json!({"currency": c, "issuer": ds::encode_address(&l.issuer)})
                    }
                };
                if let (Some(src), Some(dst), Some(sl), Some(wl)) = (
                    txj["Account"].as_str().and_then(decode_address),
                    txj["Destination"].as_str().and_then(decode_address),
                    leg_of_spec(sm),
                    leg_of_spec(&txj["Amount"]),
                ) {
                    for p in txj["Paths"].as_array().into_iter().flatten().filter_map(|p| p.as_array()) {
                        let Some(segs) = ds::mixed_layout(&src, &dst, &sl, &wl, p) else {
                            continue;
                        };
                        for seg in &segs {
                            match seg {
                                ds::SegLayout::Run(hops) => {
                                    for h in hops {
                                        for a in [&h.src, &h.dst] {
                                            let k = hex::encode_upper(keylet::account_root_key(a).0);
                                            load_object(&mut *state, url, &k, ledger_index);
                                        }
                                        let k = hex::encode_upper(
                                            keylet::ripple_state_key(&h.src, &h.dst, &h.cur).0,
                                        );
                                        load_object(&mut *state, url, &k, ledger_index);
                                    }
                                }
                                ds::SegLayout::Book { from, to } => {
                                    chains.push(vec![spec_of_leg(from), spec_of_leg(to)]);
                                }
                            }
                        }
                    }
                }
            }
            let paths = txj["Paths"].as_array().filter(|p| !p.is_empty());
            match paths {
                Some(ps) => {
                    for p in ps.iter().filter_map(|p| p.as_array()) {
                        let mut chain = vec![s.clone()];
                        for el in p {
                            match el.get("currency") {
                                Some(cur) if cur == "XRP" => {
                                    chain.push(json!({"currency": "XRP"}));
                                }
                                Some(cur) => {
                                    if let Some(iss) = el.get("issuer") {
                                        chain.push(
                                            json!({"currency": cur.clone(), "issuer": iss.clone()}),
                                        );
                                    }
                                }
                                // ISSUER-ONLY element (type 32): the running
                                // currency carries over — the engine now builds
                                // the same-currency cross-issuer book for it,
                                // and skipping it here left that book (and its
                                // pool) unhydrated, reading as EMPTY: CF3FFF81
                                // stayed tecPATH_DRY after the engine fix
                                // because the USD.rvYA→USD.rhub8 book never
                                // loaded.
                                None => {
                                    let prev_cur = chain
                                        .last()
                                        .and_then(|c| c.get("currency"))
                                        .filter(|c| c.as_str() != Some("XRP"))
                                        .cloned();
                                    if let (Some(iss), Some(pc)) = (el.get("issuer"), prev_cur) {
                                        chain.push(json!({"currency": pc, "issuer": iss.clone()}));
                                    }
                                }
                            }
                        }
                        chain.push(w.clone());
                        chains.push(chain);
                    }
                    // ...AND THE DEFAULT PATH. rippled builds the default
                    // strand ALONGSIDE every named path unless tfNoRippleDirect
                    // (0x00010000), so `SendMax -> Amount` is a book the
                    // payment can consume even when `Paths` routes elsewhere.
                    // Deriving chains from `Paths` alone missed it, and an
                    // unfetched book reads as EMPTY rather than as unknown:
                    // `strand_upper_bound` returns None for a chain with no book
                    // or pool, so the default strand is dropped from the round
                    // ordering and its liquidity is silently invisible.
                    //
                    // #105973456 E2DBEAA1: a circular tfPartialPayment routing
                    // FUZZY > XRP > EVR by Paths, no tfNoRippleDirect. Mainnet
                    // ALSO fills from the direct FUZZY/EVR book — maker
                    // rNomEcvKP4E5dA, whose FUZZY and EVR lines are exactly the
                    // two nodes we were missing (8 mutations against 10). Our
                    // DX_PAY showed `order=[(1, …)]`: the strand was built and
                    // then dropped for want of a book nobody fetched.
                    let no_direct = txj["Flags"].as_u64().unwrap_or(0) & 0x0001_0000 != 0;
                    if !no_direct {
                        chains.push(vec![s.clone(), w.clone()]);
                    }
                }
                None => chains.push(vec![s, w]),
            }
        }
        Some("OfferCreate") => {
            let (Some(s), Some(w)) = (spec(&txj["TakerGets"]), spec(&txj["TakerPays"])) else { return };
            chains.push(vec![s, w]);
        }
        _ => return,
    }
    // `books_seen` dedups across chains, so paths that share a hop pair — and
    // they usually do, every one of #106156904's six ends in SOL — cost one
    // load between them, not one each.
    for chain in &chains {
        for pair in chain.windows(2) {
            load_book_pair(state, url, &pair[0], &pair[1], ledger_index, books_seen);
            // IOU↔IOU pairs can autobridge through XRP — preload both bridge books.
            let xrp = json!({"currency": "XRP"});
            if pair[0] != xrp && pair[1] != xrp {
                load_book_pair(state, url, &pair[0], &xrp, ledger_index, books_seen);
                load_book_pair(state, url, &xrp, &pair[1], ledger_index, books_seen);
            }
        }
    }
}

fn load_book_pair(
    state: &mut LedgerState,
    url: &str,
    pays_spec: &Value,
    gets_spec: &Value,
    ledger_index: u32,
    books_seen: &mut HashSet<String>,
) {
    let (pays_spec, gets_spec) = (pays_spec.clone(), gets_spec.clone());
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
        "limit": 50,
        "ledger_index": ledger_index,
    })) else { return };
    let Some(offers) = res.get("offers").and_then(|v| v.as_array()) else { return };
    let mut pages_seen: HashSet<String> = HashSet::new();
    for off in offers.iter().take(50) {
        if let Some(idx) = off.get("index").and_then(|v| v.as_str()) {
            load_object(state, url, idx, ledger_index);
        }
        if let Some(bd) = off.get("BookDirectory").and_then(|v| v.as_str()) {
            load_object(state, url, bd, ledger_index);
            // `book_offers` OMITS fully-unfunded offers, but the page's
            // Indexes still carry them and the walk's stream REAPS them on
            // contact (offer deleted, page freed, owner root modified).
            // #106030404 50E1F824: rGodbj1's zero-USDC offer sits FIRST on
            // the tip's page; mainnet deletes it during tip-finding, and
            // with the object unhydrated our walk skipped it silently
            // (7v11). Load every entry on each touched page — plus the
            // maker funding available() needs to JUDGE it dead.
            if pages_seen.insert(bd.to_string()) {
                if let Some(pres) = rpc(url, "ledger_entry",
                    json!({"index": bd, "ledger_index": ledger_index})) {
                    let entries: Vec<String> = pres
                        .get("node")
                        .and_then(|n| n.get("Indexes"))
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()
                        })
                        .unwrap_or_default();
                    for ent in entries.iter().take(24) {
                        let Some(ores) = rpc(url, "ledger_entry",
                            json!({"index": ent, "ledger_index": ledger_index})) else { continue };
                        let Some(onode) = ores.get("node") else { continue };
                        load_object(state, url, ent, ledger_index);
                        let Some(mk) = onode.get("Account").and_then(|v| v.as_str()) else { continue };
                        load_account(state, url, mk, ledger_index);
                        if let Some(g) = onode.get("TakerGets").and_then(|v| v.as_object()) {
                            if let (Some(mid), Some(gi), Some(gc)) = (
                                decode_address(mk),
                                g.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                                g.get("currency").and_then(|v| v.as_str()),
                            ) {
                                let key = keylet::ripple_state_key(&mid, &gi, &currency_code(gc));
                                load_object(state, url, &hex::encode_upper(key.0), ledger_index);
                            }
                        }
                        // The auth reap's reads: `require_auth_known` judges a
                        // maker by its TakerPays-side line (the asset it
                        // RECEIVES) plus that leg's issuer root. Nothing else
                        // needs either, so an unhydrated line reads as
                        // tecNO_LINE where mainnet holds an authorized line
                        // (l106588526 FCFDBF76048F: 4 phantom reap writes).
                        if let Some(p) = onode.get("TakerPays").and_then(|v| v.as_object()) {
                            if let Some(pi) = p.get("issuer").and_then(|v| v.as_str()) {
                                load_account(state, url, pi, ledger_index);
                                if let (Some(mid), Some(pid), Some(pc)) = (
                                    decode_address(mk),
                                    decode_issuer(pi),
                                    p.get("currency").and_then(|v| v.as_str()),
                                ) {
                                    let key = keylet::ripple_state_key(&mid, &pid, &currency_code(pc));
                                    load_object(state, url, &hex::encode_upper(key.0), ledger_index);
                                }
                            }
                        }
                            // The reap's third write: the maker's OWNER
                            // DIRECTORY page (delete_maker_offer removes the
                            // entry) — invisible unless hydrated. Root page plus
                            // the OwnerNode-hinted page for multi-page dirs.
                            if let Some(mid) = decode_address(mk) {
                                let droot = keylet::owner_dir_key(&mid);
                                load_object(state, url, &hex::encode_upper(droot.0), ledger_index);
                                if let Some(hint) = onode.get("OwnerNode").and_then(|v| v.as_str())
                                    .and_then(|h| u64::from_str_radix(h, 16).ok())
                                    .filter(|h| *h != 0)
                                {
                                    let dpk = keylet::dir_page_key(&droot, hint);
                                    load_object(state, url, &hex::encode_upper(dpk.0), ledger_index);
                                }
                            }
                    }
                }
            }
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
        // See the level-scan twin above: the auth reap reads the maker's
        // TakerPays-side line and that issuer's root.
        if let Some(p) = off.get("TakerPays").and_then(|v| v.as_object()) {
            if let Some(pi) = p.get("issuer").and_then(|v| v.as_str()) {
                load_account(state, url, pi, ledger_index);
                if let (Some(mid), Some(pid), Some(pc)) = (
                    decode_address(maker),
                    decode_issuer(pi),
                    p.get("currency").and_then(|v| v.as_str()),
                ) {
                    let key = keylet::ripple_state_key(&mid, &pid, &currency_code(pc));
                    load_object(state, url, &hex::encode_upper(key.0), ledger_index);
                }
            }
        }
    }
    // A level whose EVERY offer is unfunded never appears in `book_offers` at
    // all — no offer names its page, and the stream-reaps rippled performs on
    // it (offer + page deleted, owner root modified) are invisible to a
    // sandbox that never loaded them. #106030404 50E1F824: rGodbj1's
    // zero-funded level sits one rate behind the fully-consumed tip; mainnet
    // trims-and-consumes the tip, keeps stepping (BookStep.cpp:1062 returns
    // fullyConsumed), and reaps it. Enumerate the book's pages PAST the best
    // known one with a seeded `ledger_data` marker (the marker must be an
    // EXISTING key — the tip's own page is) and hydrate what the walk could
    // step onto.
    let mut best_bd: Option<String> = None;
    for off in offers.iter().take(50) {
        if let Some(bd) = off.get("BookDirectory").and_then(|v| v.as_str()) {
            if best_bd.as_deref().map_or(true, |b| bd < b) {
                best_bd = Some(bd.to_string());
            }
        }
    }
    if let Some(bd0) = best_bd {
        let base24 = bd0[..48].to_string();
        // Follow ledger_data markers until the key order leaves the book —
        // a single 24-row call missed #106433073 462DE605: the RLUSD/XRP
        // book is bot-deep and the crossed page sat 50 LEVELS past the
        // parent tip, so the walk saw clob=None and tecKILLED an IoC
        // mainnet fills. Capped at 400 pages; the cap is LOGGED, never
        // silent.
        // Seed at the BOOK BASE, not the best book_offers page: book_offers
        // hides EXPIRED offers exactly like unfunded ones, and when the
        // expired level is the TIP it sorts BEFORE every visible page — a
        // forward walk seeded at best_bd can never reach it. #106586388
        // CE78DA6B: two expired 20-XRP offers at the book tip; mainnet reaps
        // both on encounter, our sandbox never loaded them (5v9 muts, engine
        // innocent). The old "marker must be an EXISTING key" constraint is
        // FALSE on xrpld 3.3.0 — a synthetic base24+zeros marker seeks fine.
        let mut marker = serde_json::Value::String(format!("{base24}0000000000000000"));
        // Ancient-fixture fallback: the base-seeded request is a NEW cache key,
        // and for ledgers older than .39's (rebuilt, shallow) store it can only
        // miss-then-lgrNotFound — while the OLD best_bd-seeded request's dxcache
        // entries still hold months of history. First-call failure ⇒ reseed at
        // bd0 and let the cache serve the walk. (2026-08-27: histsweep went
        // 36/426 divergent on exactly this — phantom DRY/NO_DST from empty
        // books on pre-rebuild fixtures.)
        let mut seeded_fallback = false;
        let mut pages_done = 0usize;
        // Per-book budget for OFFER hydration beyond the first 24 pages: a
        // page object rides the ledger_data batch for free, but each offer
        // costs ledger_entry + maker root + line + dir (~4 RPC). The first
        // deepbook gate ran unbudgeted and crawled (~65s/fixture cold).
        //
        // DX_DEEPBOOK=1 lifts both caps for one-off full-state reproduction
        // of replay-only divergences (#106455229: the dusting path lived
        // past the budget, so the capped probe was clean BY LUCK). Gates
        // never set it — default behavior is identical.
        let deep = std::env::var("DX_DEEPBOOK").is_ok();
        let page_cap: usize = if deep { 100_000 } else { 400 };
        let mut deep_offer_budget: i32 = if deep { i32::MAX } else { 256 };
        'sweep: while pages_done < page_cap {
            let Some(res) = rpc(url, "ledger_data", json!({
                "ledger_index": ledger_index,
                "marker": marker.clone(),
                "limit": 96,
                "binary": false,
            })) else {
                if !seeded_fallback && pages_done == 0 {
                    seeded_fallback = true;
                    marker = serde_json::Value::String(bd0.clone());
                    continue;
                }
                break;
            };
            let batch = res.get("state").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if batch.is_empty() {
                if !seeded_fallback && pages_done == 0 {
                    seeded_fallback = true;
                    marker = serde_json::Value::String(bd0.clone());
                    continue;
                }
                break;
            }
            for e in &batch {
                let Some(idx) = e.get("index").and_then(|v| v.as_str()) else { continue };
                if !idx.starts_with(&base24) {
                    break 'sweep; // key order: past the book's range
                }
                pages_done += 1;
                if pages_done >= page_cap {
                    eprintln!("HYDRATE-CAP book {base24} truncated at {page_cap} pages");
                }
                if e.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("DirectoryNode") {
                    continue;
                }
                let Ok(kb) = hex::decode(idx) else { continue };
                let Ok(karr) = <[u8; 32]>::try_from(kb.as_slice()) else { continue };
                let mut node = e.clone();
                hexify_addresses(&mut node);
                if std::env::var("DX_RM").is_ok() {
                    eprintln!("DX_BOOKWALK page {} entries={}", &idx[48..], e.get("Indexes").and_then(|v| v.as_array()).map_or(0, |a| a.len()));
                }
                let _ = state.state_map.insert(Hash256(karr), serde_json::to_vec(&node).unwrap_or_default());
                let entries: Vec<String> = e
                    .get("Indexes")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                for ent in entries.iter().take(24) {
                    if pages_done > 24 {
                        if deep_offer_budget <= 0 {
                            if deep_offer_budget == 0 {
                                eprintln!("HYDRATE-CAP book {base24} deep-offer budget spent");
                                deep_offer_budget = -1;
                            }
                            continue;
                        }
                        deep_offer_budget -= 1;
                    }
                    let ores_opt = rpc(url, "ledger_entry",
                        json!({"index": ent, "ledger_index": ledger_index}));
                    if std::env::var("DX_RM").is_ok() {
                        eprintln!("DX_BOOKWALK offer {} fetched={} node={}",
                            &ent[..16],
                            ores_opt.is_some(),
                            ores_opt.as_ref().and_then(|o| o.get("node")).is_some());
                    }
                    let Some(ores) = ores_opt else { continue };
                    let Some(onode) = ores.get("node") else { continue };
                    // Insert directly — load_object would re-fetch the same
                    // entry a second time.
                    if let Ok(okb) = hex::decode(ent) {
                        if let Ok(okarr) = <[u8; 32]>::try_from(okb.as_slice()) {
                            let mut on = onode.clone();
                            hexify_addresses(&mut on);
                            let _ = state.state_map.insert(
                                Hash256(okarr),
                                serde_json::to_vec(&on).unwrap_or_default(),
                            );
                        }
                    }
                    let Some(mk) = onode.get("Account").and_then(|v| v.as_str()) else { continue };
                    load_account(state, url, mk, ledger_index);
                    if let Some(g) = onode.get("TakerGets").and_then(|v| v.as_object()) {
                        if let (Some(mid), Some(gi), Some(gc)) = (
                            decode_address(mk),
                            g.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                            g.get("currency").and_then(|v| v.as_str()),
                        ) {
                            let key = keylet::ripple_state_key(&mid, &gi, &currency_code(gc));
                            load_object(state, url, &hex::encode_upper(key.0), ledger_index);
                        }
                    }
                    // See the level-scan twin above: the auth reap reads the
                    // maker's TakerPays-side line and that issuer's root.
                    if let Some(p) = onode.get("TakerPays").and_then(|v| v.as_object()) {
                        if let Some(pi) = p.get("issuer").and_then(|v| v.as_str()) {
                            load_account(state, url, pi, ledger_index);
                            if let (Some(mid), Some(pid), Some(pc)) = (
                                decode_address(mk),
                                decode_issuer(pi),
                                p.get("currency").and_then(|v| v.as_str()),
                            ) {
                                let key = keylet::ripple_state_key(&mid, &pid, &currency_code(pc));
                                load_object(state, url, &hex::encode_upper(key.0), ledger_index);
                            }
                        }
                    }
                        // The reap's third write: the maker's OWNER
                        // DIRECTORY page (delete_maker_offer removes the
                        // entry) — invisible unless hydrated. Root page plus
                        // the OwnerNode-hinted page for multi-page dirs.
                        if let Some(mid) = decode_address(mk) {
                            let droot = keylet::owner_dir_key(&mid);
                            load_object(state, url, &hex::encode_upper(droot.0), ledger_index);
                            if let Some(hint) = onode.get("OwnerNode").and_then(|v| v.as_str())
                                .and_then(|h| u64::from_str_radix(h, 16).ok())
                                .filter(|h| *h != 0)
                            {
                                let dpk = keylet::dir_page_key(&droot, hint);
                                load_object(state, url, &hex::encode_upper(dpk.0), ledger_index);
                            }
                        }
                }
            }
            match res.get("marker") {
                Some(m) => marker = m.clone(),
                None => break,
            }
        }
    }
}

/// OfferCancel pre-state: the canceled offer object, its BookDirectory page
/// (the quality level it rests on) and the owner-dir hint page. NOTHING else
/// hydrates a book an OfferCancel touches — payments and OfferCreates load
/// books by SPEC, and a cancel names only (account, seq). #106030993
/// D63363BB: the pre-existing page CE26A7EB…EFF7 held the canceled offer;
/// unhydrated, a same-rate placement earlier in the ledger re-created the
/// page WITHOUT the old entry, the cancel's removal no-opped, and mainnet's
/// page deletion went missing (5v6).
fn load_offer_cancel_prestate(state: &mut LedgerState, url: &str, txj: &Value, ledger_index: u32) {
    if txj["TransactionType"].as_str() != Some("OfferCancel") {
        return;
    }
    let (Some(acct), Some(seq)) = (
        txj["Account"].as_str().and_then(decode_address),
        txj["OfferSequence"].as_u64(),
    ) else {
        return;
    };
    let okey = keylet::offer_key(&acct, seq as u32);
    let khex = hex::encode_upper(okey.0);
    load_object(state, url, &khex, ledger_index);
    let Some(res) = rpc(url, "ledger_entry",
        json!({"index": &khex, "ledger_index": ledger_index})) else { return };
    let Some(node) = res.get("node") else { return };
    if let Some(bd) = node.get("BookDirectory").and_then(|v| v.as_str()) {
        load_object(state, url, bd, ledger_index);
    }
    let droot = keylet::owner_dir_key(&acct);
    load_object(state, url, &hex::encode_upper(droot.0), ledger_index);
    if let Some(hint) = node.get("OwnerNode").and_then(|v| v.as_str())
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .filter(|h| *h != 0)
    {
        let pk = keylet::dir_page_key(&droot, hint);
        load_object(state, url, &hex::encode_upper(pk.0), ledger_index);
    }
}

/// MPT pre-state: an MPT amount names an issuance, and the engine reads the
/// MPTokenIssuance object plus each endpoint's MPToken — none of which a
/// fee-only tec meta ever carries (3EC225FD's tecINSUFFICIENT_FUNDS needs
/// the holder's zero-balance MPToken to exist to be REACHED). Loads for
/// Payment (Amount) and Clawback (Amount + Holder).
fn load_mpt_prestate(state: &mut LedgerState, url: &str, txj: &Value, ledger_index: u32) {
    let mpt_id = |v: &Value| -> Option<[u8; 24]> {
        let bytes = hex::decode(v.get("mpt_issuance_id")?.as_str()?).ok()?;
        bytes.as_slice().try_into().ok()
    };
    // Amount-embedded id (Payment/Clawback) or the lifecycle txs' top-level
    // MPTokenIssuanceID (Destroy/Set/Authorize).
    let top_id = || -> Option<[u8; 24]> {
        let bytes = hex::decode(txj.get("MPTokenIssuanceID")?.as_str()?).ok()?;
        bytes.as_slice().try_into().ok()
    };
    let Some(id) = txj.get("Amount").and_then(|a| mpt_id(a)).or_else(top_id) else { return };
    let ikey = keylet::mpt_issuance_key(&id);
    load_object(state, url, &hex::encode_upper(ikey.0), ledger_index);
    let mut parties: Vec<[u8; 20]> = Vec::new();
    if let Some(a) = txj["Account"].as_str().and_then(decode_address) {
        parties.push(a);
    }
    for f in ["Destination", "Holder"] {
        if let Some(a) = txj[f].as_str().and_then(decode_address) {
            parties.push(a);
        }
    }
    for p in parties {
        let tkey = keylet::mptoken_key(&ikey, &p);
        load_object(state, url, &hex::encode_upper(tkey.0), ledger_index);
        load_object(
            state,
            url,
            &hex::encode_upper(keylet::account_root_key(&p).0),
            ledger_index,
        );
    }
}

/// PayChannel pre-state: hydration is meta-driven, and a Fund/Claim's meta
/// never touches the channel DESTINATION's AccountRoot — but the engine's
/// dst-exists check (tecNO_DST) reads it. Gate 'paychan' caught the miss:
/// B169CB4A (105949459) funded a channel whose dst root was simply unloaded,
/// so we refused with tecNO_DST where mainnet funded. Load the channel, both
/// parties' roots, both owner-dir roots, and the OwnerNode/DestinationNode
/// hint pages (the close path unlinks through them).
fn load_paychan_prestate(state: &mut LedgerState, url: &str, txj: &Value, ledger_index: u32) {
    let tt = txj["TransactionType"].as_str();
    if !matches!(tt, Some("PaymentChannelClaim") | Some("PaymentChannelFund")) {
        return;
    }
    let Some(chan) = txj["Channel"].as_str() else { return };
    load_object(state, url, chan, ledger_index);
    let Some(res) = rpc(url, "ledger_entry",
        json!({"index": chan, "ledger_index": ledger_index})) else { return };
    let Some(node) = res.get("node") else { return };
    for (acct_field, hint_field) in [("Account", "OwnerNode"), ("Destination", "DestinationNode")] {
        let Some(acct) = node.get(acct_field).and_then(|v| v.as_str()).and_then(decode_address)
        else {
            continue;
        };
        let akey = keylet::account_root_key(&acct);
        load_object(state, url, &hex::encode_upper(akey.0), ledger_index);
        let droot = keylet::owner_dir_key(&acct);
        load_object(state, url, &hex::encode_upper(droot.0), ledger_index);
        if let Some(hint) = node.get(hint_field).and_then(|v| v.as_str())
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .filter(|h| *h != 0)
        {
            let pk = keylet::dir_page_key(&droot, hint);
            load_object(state, url, &hex::encode_upper(pk.0), ledger_index);
        }
    }
}

/// AMM Deposit/Withdraw/Vote pre-state: the pool account's owner-directory
/// ROOT (the dir walk starts there; meta only carries the touched page) plus
/// the depositor's dir root and both parties' account roots. For AMMVote the
/// load matters even though a vote moves nothing: the not-an-LP refusal
/// (tecAMM_INVALID_TOKENS) reads the pool object and the voter's LPToken
/// line, and a fee-only tec touches neither, so no other loader fetches them.
fn load_amm_prestate(state: &mut LedgerState, url: &str, txj: &Value, ledger_index: u32) {
    let tt = txj["TransactionType"].as_str();
    if !matches!(
        tt,
        Some("AMMDeposit") | Some("AMMWithdraw") | Some("AMMCreate") | Some("AMMVote")
    ) {
        return;
    }
    if let Some(acct) = txj["Account"].as_str().and_then(decode_address) {
        let droot = keylet::owner_dir_key(&acct);
        let k = hex::encode_upper(droot.0);
        load_object(state, url, &k, ledger_index);
        // A withdraw that pays an asset the LP holds no line for CREATES the
        // line, and dirAdd appends its entry to the directory's TAIL page
        // (the root's IndexPrevious). With only the root hydrated we re-
        // created the root instead: #106131297 4FD1D57A modified rJf7VZxB's
        // tail page 733BF5D7 on mainnet while we invented a fresh root
        // (9v9, one swap).
        if let Some(res) = rpc(url, "ledger_entry",
            json!({"index": &k, "ledger_index": ledger_index})) {
            if let Some(prev) = res.get("node").and_then(|n| n.get("IndexPrevious"))
                .and_then(|v| v.as_str())
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .filter(|p| *p != 0)
            {
                let pk = keylet::dir_page_key(&droot, prev);
                load_object(state, url, &hex::encode_upper(pk.0), ledger_index);
            }
        }
    }
    if tt == Some("AMMCreate") {
        // No pool exists yet — the creator's and asset issuers' dir roots are
        // the walk anchors for the new trust lines.
        for f in ["Amount", "Amount2"] {
            if let Some(iss) = txj.get(f)
                .and_then(|v| v.get("issuer"))
                .and_then(|v| v.as_str())
                .and_then(decode_issuer)
            {
                let k = hex::encode_upper(keylet::owner_dir_key(&iss).0);
                load_object(state, url, &k, ledger_index);
            }
        }
        // "No pool exists yet" is exactly what preclaim must be able to
        // DISPROVE: a pair that already has a pool refuses with
        // tecDUPLICATE (#106118993 D98E76D5), and a fee-only meta never
        // names the standing pool. Ask for it; absence is the normal case.
        let asset_of = |v: &Value| -> Value {
            if v.is_string() {
                json!({"currency": "XRP"})
            } else {
                json!({"currency": v.get("currency").cloned().unwrap_or(Value::Null),
                       "issuer": v.get("issuer").cloned().unwrap_or(Value::Null)})
            }
        };
        if let (Some(a1), Some(a2)) = (txj.get("Amount"), txj.get("Amount2")) {
            if let Some(res) = rpc(url, "ledger_entry", json!({
                "amm": {"asset": asset_of(a1), "asset2": asset_of(a2)},
                "ledger_index": ledger_index,
            })) {
                if let Some(idx) = res["node"]["index"].as_str() {
                    let idx = idx.to_string();
                    load_object(state, url, &idx, ledger_index);
                }
            }
        }
        // The creator's own trust line for each IOU side: the funds check
        // reads it, and a fee-only refusal touches nothing (#106071927
        // 3CCED155 — same lesson as the escrow and deposit lines).
        if let Some(who) = txj["Account"].as_str().and_then(decode_address) {
            for f in ["Amount", "Amount2"] {
                let Some(a) = txj.get(f) else { continue };
                let (Some(cur), Some(iss)) = (
                    a.get("currency").and_then(|v| v.as_str()),
                    a.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
                ) else { continue };
                if cur == "XRP" {
                    continue;
                }
                let k = hex::encode_upper(
                    keylet::ripple_state_key(&who, &iss, &currency_code(cur)).0,
                );
                load_object(state, url, &k, ledger_index);
            }
        }
        return;
    }
    let (Some(a1), Some(a2)) = (txj.get("Asset"), txj.get("Asset2")) else { return };
    // A payout that CREATES a trust line dirAdds into BOTH parties'
    // directories — and the counterparty of an asset line is the asset's
    // ISSUER, whose directory can run to hundreds of pages. #106131297
    // 4FD1D57A: the LP held no OAR line; mainnet appends the new line to
    // OAR-issuer rJf7VZxB's TAIL page 733BF5D7 — we, with the issuer's dir
    // unhydrated, invented a fresh root. Load each issuer's root + tail.
    for a in [a1, a2] {
        let Some(iss) = a.get("issuer").and_then(|v| v.as_str()).and_then(decode_address) else {
            continue;
        };
        let droot = keylet::owner_dir_key(&iss);
        let k = hex::encode_upper(droot.0);
        load_object(state, url, &k, ledger_index);
        if let Some(dres) = rpc(url, "ledger_entry",
            json!({"index": &k, "ledger_index": ledger_index})) {
            if let Some(prev) = dres.get("node").and_then(|n| n.get("IndexPrevious"))
                .and_then(|v| v.as_str())
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .filter(|p| *p != 0)
            {
                let pk = keylet::dir_page_key(&droot, prev);
                load_object(state, url, &hex::encode_upper(pk.0), ledger_index);
            }
        }
    }
    let Some(res) = rpc(url, "ledger_entry", json!({
        "amm": {"asset": a1, "asset2": a2},
        "ledger_index": ledger_index,
    })) else { return };
    let Some(amm_acct) = res["node"]["Account"].as_str() else { return };
    load_account(state, url, amm_acct, ledger_index);
    if let Some(aid) = decode_address(amm_acct) {
        let adroot = keylet::owner_dir_key(&aid);
        let k = hex::encode_upper(adroot.0);
        load_object(state, url, &k, ledger_index);
        // …and the pool dir's TAIL page: a withdraw that CREATES a trust
        // line dirAdds into the LAST page of BOTH owners' directories, and
        // an AMM's directory runs to hundreds of pages. #106131297
        // 4FD1D57A: mainnet appends to the pool's tail 733BF5D7; with only
        // the root absent-or-bare we re-created the root instead.
        if let Some(dres) = rpc(url, "ledger_entry",
            json!({"index": &k, "ledger_index": ledger_index})) {
            if let Some(prev) = dres.get("node").and_then(|n| n.get("IndexPrevious"))
                .and_then(|v| v.as_str())
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .filter(|p| *p != 0)
            {
                let pk = keylet::dir_page_key(&adroot, prev);
                load_object(state, url, &hex::encode_upper(pk.0), ledger_index);
            }
        }
    }
    // The AMM ledger object itself — needed so preclaim can read the pool and
    // run the reserve/funds check (else a Deposit/Withdraw whose pool no other
    // tx in the ledger touched falls through to a phantom tecNO_ENTRY, #105783986).
    if let Some(idx) = res["node"]["index"].as_str() {
        load_object(state, url, idx, ledger_index);
    }
    // The POOL's own trust line for each non-XRP asset. `load_account` above
    // gives the AMM's AccountRoot — its XRP side only — so an IOU pool balance
    // read as ZERO, and `checkAmount` then rejects any withdrawal of it with
    // tecAMM_BALANCE before the LP checks are ever reached. A fee-only `tec`
    // touches neither line, so the modified/deleted loader never fetches them.
    // #106308202 4D96E855 is that: mainnet tecAMM_INVALID_TOKENS, ours
    // tecAMM_BALANCE — the right family of refusal for entirely the wrong
    // reason.
    if let Some(amm_id) = decode_address(amm_acct) {
        for f in ["Asset", "Asset2"] {
            let Some(a) = txj.get(f) else { continue };
            let (Some(cur), Some(iss)) = (
                a.get("currency").and_then(|v| v.as_str()),
                a.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
            ) else { continue };
            if cur == "XRP" {
                continue;
            }
            let k = hex::encode_upper(
                keylet::ripple_state_key(&amm_id, &iss, &currency_code(cur)).0,
            );
            load_object(state, url, &k, ledger_index);
        }
    }
    // The DEPOSITOR's own trust line for each non-XRP deposited amount. The
    // engine's funds test (rippled's `balance`/`checkBalance` lambda) reads
    // it, and a line the sandbox lacks reads as HOLDING NOTHING — so a funded
    // depositor gets condemned with tecUNFUNDED_AMM before the verdict
    // mainnet actually reached. #105869720 878CD973 (tecAMM_FAILED) and
    // #105893158 85C32164 (tecINSUF_RESERVE_LINE) both regressed exactly that
    // way when the check landed without this loader: their old code paths
    // returned before ever touching the line, so nothing else fetches it.
    // Same lesson as the pool lines above: widen what the engine reads,
    // widen what the probe hydrates — in ONE change.
    if let Some(dep) = txj["Account"].as_str().and_then(decode_address) {
        for f in ["Amount", "Amount2"] {
            let Some(a) = txj.get(f) else { continue };
            let (Some(cur), Some(iss)) = (
                a.get("currency").and_then(|v| v.as_str()),
                a.get("issuer").and_then(|v| v.as_str()).and_then(decode_issuer),
            ) else { continue };
            if cur == "XRP" {
                continue;
            }
            let k = hex::encode_upper(
                keylet::ripple_state_key(&dep, &iss, &currency_code(cur)).0,
            );
            load_object(state, url, &k, ledger_index);
        }
    }
    // The depositor's LPToken trust line, so the reserve check knows whether
    // this is a first-time deposit (ownerCountAdj = 1) or a repeat LP (0).
    if let (Some(dep), Some(amm_id), Some(lp_cur)) = (
        txj["Account"].as_str().and_then(decode_address),
        decode_address(amm_acct),
        res["node"]["LPTokenBalance"]["currency"].as_str(),
    ) {
        if let Ok(cb) = hex::decode(lp_cur) {
            if let Ok(cur) = <[u8; 20]>::try_from(cb.as_slice()) {
                let lk = hex::encode_upper(keylet::ripple_state_key(&dep, &amm_id, &cur).0);
                load_object(state, url, &lk, ledger_index);
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
    // Pseudo-transactions carry Account: "" and Fee: "0" — the zero account.
    let account = match txjson["Account"].as_str()? {
        "" => [0u8; 20],
        a => decode_address(a)?,
    };
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
    // Pseudo-transactions (UNLModify/SetFee/EnableAmendment): consensus
    // injects them with no Account, no Fee, no Sequence — rippled's Change
    // transactor overrides the fee/sequence machinery away, so skip
    // preclaim/apply_common entirely (there is no account to charge).
    if xrpl_ledger::tx::dispatch::is_pseudo(&tx.tx_type) {
        let pf = transactor.preflight(tx);
        if !pf.is_success() {
            return (pf.code_str().to_string(), HashMap::new());
        }
        let mut sb = Sandbox::new(state);
        let applied = transactor.do_apply(tx, &mut sb);
        if applied.is_success() {
            return (TxResult::Success.code_str().to_string(), sb.into_modifications());
        }
        return (applied.code_str().to_string(), HashMap::new());
    }
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
        // Success-only (Transactor.cpp:660; tec rolls the stamp back).
        xrpl_ledger::ledger::transactor::stamp_account_txn_id(tx, &mut sb);
        (TxResult::Success.code_str().to_string(), sb.into_modifications())
    } else if applied.is_claimed() {
        // See apply.rs: tecKILLED carries OfferCreate's stale-offer cleanup,
        // which the transactor has already rolled its fills back around.
        if applied != TxResult::Killed {
            sb.restore_snapshot(snap);
        }
        (applied.code_str().to_string(), sb.into_modifications())
    } else {
        (applied.code_str().to_string(), HashMap::new())
    }
}

/// Re-spell engine-internal JSON into the canonical forms the binary codec
/// demands, changing NO values: u64 fields to full-width hex (accepting
/// numbers, unpadded hex, and — for the MPT amount family, which rippled
/// renders base-10 — decimal strings); 40-hex account ids to base58.
fn canon_for_encode(v: &mut Value) {
    const U64_HEX: &[&str] = &[
        "OwnerNode", "BookNode", "LowNode", "HighNode", "DestinationNode",
        "IndexNext", "IndexPrevious", "XChainClaimID", "XChainAccountCreateCount",
        "XChainAccountClaimCount", "ReferenceCount", "NFTokenOfferNode", "IssuerNode",
        "AssetPrice",
    ];
    const U64_DEC: &[&str] = &["MaximumAmount", "OutstandingAmount", "MPTAmount", "LockedAmount"];
    const ACCTS: &[&str] = &[
        "Account", "Owner", "Destination", "Issuer", "RegularKey", "Authorize",
        "Unauthorize", "NFTokenMinter", "Holder", "OtherChainSource",
        "AttestationSignerAccount", "AttestationRewardAccount", "LockingChainDoor",
        "IssuingChainDoor", "issuer",
    ];
    match v {
        Value::Array(a) => {
            for e in a {
                canon_for_encode(e);
            }
        }
        Value::Object(o) => {
            for (name, val) in o.iter_mut() {
                if U64_HEX.contains(&name.as_str()) {
                    let n = val.as_u64().or_else(|| {
                        val.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())
                    });
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if U64_DEC.contains(&name.as_str()) {
                    let n = val
                        .as_u64()
                        .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()));
                    if let Some(n) = n {
                        *val = Value::String(format!("{n:016X}"));
                    }
                } else if ACCTS.contains(&name.as_str()) {
                    if let Some(s) = val.as_str() {
                        if s.len() == 40 {
                            if let Ok(b) = hex::decode(s) {
                                if let Ok(arr) = <[u8; 20]>::try_from(b.as_slice()) {
                                    *val = Value::String(
                                        xrpl_core::AccountId::from_bytes(arr).to_address(),
                                    );
                                }
                            }
                        }
                    }
                } else {
                    canon_for_encode(val);
                }
            }
        }
        _ => {}
    }
}

/// Canonicalise the u64 directory/node POINTER fields before a semantic
/// compare. XRPL JSON renders them as HEX STRINGS (`"IndexNext":"12f2e"`)
/// while our writers emit plain numbers (`77614`) — the same value spelled two
/// ways, and a spelling is not a mutation.
///
/// #105886344 E430B7E92B22: rippled erases the emptied owner-dir tail page and
/// re-creates it (`ApplyStateTable.cpp:153` drops any ModifiedNode whose
/// content equals the original, so the neighbours it relinked and restored
/// vanish from the meta). We relink and restore the same way, but wrote
/// `IndexNext` back as a number where the pre-state held a hex string, so the
/// no-op filter did not fire and we emitted the previous page and the root as
/// spurious Modifies.
///
/// Readers already tolerate both forms (`dirnum` parses either), so this is
/// purely about comparing like with like.
fn canon_ptrs(v: &mut Value) {
    const PTRS: [&str; 7] = [
        "IndexNext",
        "IndexPrevious",
        "OwnerNode",
        "BookNode",
        "HighNode",
        "LowNode",
        "DestinationNode",
    ];
    let Some(obj) = v.as_object_mut() else { return };
    for k in PTRS {
        if let Some(f) = obj.get_mut(k) {
            let n = f
                .as_u64()
                .or_else(|| f.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()));
            if let Some(n) = n {
                *f = Value::from(n);
            }
        }
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
            // drop no-op modifies (post == pre, semantically) — rippled meta
            // omits them. Threading is stripped on BOTH sides: a
            // threadOwners refresh (PreviousTxn*-only Modified) filters out
            // of OUR set exactly as the expected-side thread filter drops
            // mainnet's, while the STATE keeps the stamp for DX_THREADCHECK.
            if let (Some(new), Some(old)) = (new_bytes, state.state_map.lookup(key)) {
                let mut pn: Option<Value> = serde_json::from_slice(new).ok();
                let mut po: Option<Value> = serde_json::from_slice(old).ok();
                for v in [pn.as_mut(), po.as_mut()].into_iter().flatten() {
                    canon_ptrs(v);
                    if let Some(o) = v.as_object_mut() {
                        o.remove("PreviousTxnID");
                        o.remove("PreviousTxnLgrSeq");
                    }
                }
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
    // `--rpc` states a PREFERENCE, not a pin: `select_rpc` below falls back to
    // s2 for any ledger the preferred node has already rolled past.
    let rpc_pref = args.iter().position(|a| a == "--rpc")
        .and_then(|i| args.get(i + 1).cloned());
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
    // Every hydration targets seq-1, so the node is chosen once, per run.
    let rpc_url = xrpl_node::rpc_select::select_rpc(rpc_pref, seq - 1);
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
    // parent_hash feeds the AMMCreate account derivation (ripesha over
    // sha512half(prefix ‖ parentHash ‖ ammKeylet)).
    let parent_hash = hdr["parent_hash"].as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .map(Hash256)
        .unwrap_or(Hash256([0; 32]));
    let header = LedgerHeader {
        sequence: seq.saturating_sub(1),
        total_coins: hdr["total_drops"].as_u64().unwrap_or(100_000_000_000_000_000),
        parent_hash,
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
                // An issuer that turns out to be an AMM pseudo-account drags
                // its pool object in too: TrustSet's destination rules need
                // the AMM's LPTokenBalance (currency + emptiness) to judge a
                // new line toward it, and a fee-only refusal's meta touches
                // neither object. #106250239 2BC6AFA8 is a PLHINX line
                // opened at a pool account.
                if let Some(id) = decode_address(&a) {
                    let root = Sandbox::new(&state)
                        .read(&keylet::account_root_key(&id))
                        .and_then(|d| serde_json::from_slice::<Value>(&d).ok());
                    if let Some(ammid) =
                        root.as_ref().and_then(|r| r.get("AMMID")).and_then(|v| v.as_str())
                    {
                        let ammid = ammid.to_string();
                        load_object(&mut state, &rpc_url, &ammid, seq - 1);
                    }
                }
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
    // DX_THREADCHECK: every key still live that any tx materially wrote —
    // verified against the true post-state after the loop.
    let mut thread_touched_keys: HashSet<Hash256> = HashSet::new();
    for h in &order {
        let Some(txj) = txjson_map.get(h) else { continue };
        for key_hex in native_read_keys(txj) {
            if obj_seen.insert(key_hex.clone()) {
                load_object(&mut state, &rpc_url, &key_hex, seq - 1);
            }
        }
        load_trustline_hint_pages(&mut state, &rpc_url, txj, seq - 1);
        load_owner_dir_tail(&mut state, &rpc_url, txj, seq - 1);
        load_payment_books(&mut state, &rpc_url, txj, seq - 1, &mut books_seen);
        load_nft_pages_for_tx(&mut state, &rpc_url, txj, seq - 1);
        load_amm_prestate(&mut state, &rpc_url, txj, seq - 1);
        load_offer_cancel_prestate(&mut state, &rpc_url, txj, seq - 1);
        load_paychan_prestate(&mut state, &rpc_url, txj, seq - 1);
        load_mpt_prestate(&mut state, &rpc_url, txj, seq - 1);
    }
    // FLAG-LEDGER OPEN: rotate the NegativeUNL pending fields into
    // DisabledValidators before any transaction applies — a ledger-level
    // action outside every tx meta (Ledger::updateNegativeUNL).
    if seq % 256 == 0 {
        let nk = keylet::negative_unl_key();
        let khex = hex::encode_upper(nk.0);
        if obj_seen.insert(khex.clone()) {
            load_object(&mut state, &rpc_url, &khex, seq - 1);
        }
        if let Some(bytes) = state.state_map.lookup(&nk).map(|b| b.to_vec()) {
            match xrpl_ledger::tx::pseudo::rotate_negative_unl(&bytes, seq as u32) {
                Some(Some(nb)) => {
                    let _ = state.state_map.insert(nk, nb);
                }
                Some(None) => {
                    let _ = state.state_map.delete(&nk);
                }
                None => {}
            }
        }
    }
    // FeeSettings (fixed key): reserve checks read it; mainnet meta never
    // carries it. The NegativeUNL and Amendments singletons ride along for
    // the flag-ledger pseudo-transactions.
    for k in [
        keylet::fee_settings_key(),
        keylet::negative_unl_key(),
        keylet::amendments_key(),
    ] {
        let khex = hex::encode_upper(k.0);
        if obj_seen.insert(khex.clone()) {
            load_object(&mut state, &rpc_url, &khex, seq - 1);
        }
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
        // Mainnet's meta lists nodes rippled merely TOUCHED — a ModifiedNode
        // carrying neither FinalFields nor PreviousFields, only a refreshed
        // PreviousTxnID (e.g. an issuer AccountRoot peeked during
        // trustCreate). They record no state delta, and native does not model
        // PreviousTxnID, so they are dropped here — symmetric with the
        // no-op-Modified filtering `native_mutset` already applies to OUR
        // side. Matching them would require an engine to bump a field rippled
        // does not bump.
        // Keys mainnet merely TOUCHED: a ModifiedNode carrying neither
        // FinalFields nor PreviousFields, only a refreshed PreviousTxnID
        // (e.g. an issuer AccountRoot peeked during trustCreate). They record
        // no state delta and native does not model PreviousTxnID, so the key
        // is excluded from BOTH sides — dropping it from mainnet's set alone
        // would turn a legitimate native write into a phantom "extra".
        let touched_only: HashSet<String> = net["nodes"].as_array()
            .map(|a| {
                a.iter()
                    .filter(|n| n[1].as_u64() == Some(1))
                    .filter(|n| {
                        !n.get(2)
                            .and_then(|f| f.as_object())
                            .map(|o| o.keys().any(|k| k != "LedgerEntryType"))
                            .unwrap_or(false)
                    })
                    .filter_map(|n| n[0].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // ...and the OTHER threading shape: a ModifiedNode carrying FULL
        // FinalFields but null PreviousFields — rippled fills PreviousFields
        // only when a field actually changed, so FinalFields == the base
        // object means the write was a threading refresh (threadOwners:
        // trustDelete threads BOTH owners' roots; #106065267 F9E4D516 redeems
        // EXP to its issuer, the zeroed line is deleted, and the ISSUER's
        // AccountRoot appears Modified with every field identical to the
        // parent ledger — only PreviousTxnID moved). The fixture format keeps
        // FinalFields, so nullness of PreviousFields is lost; equality
        // against the base recovers it exactly. Symmetric with
        // native_mutset's no-op-Modified filtering on OUR side.
        let thread_touched: HashSet<String> = net["nodes"].as_array()
            .map(|a| {
                a.iter()
                    .filter(|n| n[1].as_u64() == Some(1))
                    .filter_map(|n| {
                        let k = n[0].as_str()?;
                        let exp = n.get(2)?.clone();
                        let kb: [u8; 32] =
                            hex::decode(k).ok().and_then(|b| b.as_slice().try_into().ok())?;
                        let old = state.state_map.lookup(&Hash256(kb))?;
                        let mut po: Value = serde_json::from_slice(&old).ok()?;
                        let mut pe = exp;
                        canon_ptrs(&mut po);
                        canon_ptrs(&mut pe);
                        // fixture FinalFields carry base58 addresses; the
                        // engine stores hex — same rewrite hydration applies.
                        hexify_addresses(&mut pe);
                        hexify_addresses(&mut po);
                        let strip = |v: &Value| -> Option<serde_json::Map<String, Value>> {
                            Some(
                                v.as_object()?
                                    .iter()
                                    .filter(|(k, _)| {
                                        !k.starts_with("PreviousTxn") && k.as_str() != "index"
                                    })
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect(),
                            )
                        };
                        let (se, so) = (strip(&pe)?, strip(&po)?);
                        if std::env::var("DX_THREAD").is_ok() && se != so {
                            let d: Vec<String> = se.iter()
                                .filter(|(fk, fv)| so.get(*fk) != Some(*fv))
                                .map(|(fk, fv)| format!("{fk}: exp={fv} base={:?}", so.get(fk)))
                                .chain(so.keys().filter(|fk| !se.contains_key(*fk))
                                    .map(|fk| format!("{fk}: only-in-base")))
                                .collect();
                            eprintln!("DX_THREAD {} differs: {:?}", &k[..16], d);
                        }
                        (se == so).then(|| k.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let net_mut: HashSet<(String, u8)> = net["nodes"].as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|n| Some((n[0].as_str()?.to_string(), n[1].as_u64()? as u8)))
                    .filter(|(k, b)| !touched_only.contains(k) && !(*b == 1 && thread_touched.contains(k)))
                    .collect()
            })
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

        // Engine traces (DX_AMM, DX_BRIDGE) are emitted deep in the ledger
        // crate with no tx context; this marks which tx they belong to.
        if std::env::var("DX_AMM").is_ok() || std::env::var("DX_BRIDGE").is_ok() {
            eprintln!("DX_TX {h} {tx_type}");
        }
        // Same per-tx receipt arming as state_replay: DX_REPLAY_TX prefix +
        // DX_REPLAY_SET list — receipts for ONE tx out of a whole fixture.
        let dx_armed = std::env::var("DX_REPLAY_TX")
            .map(|p| !p.is_empty() && h.starts_with(&p.to_uppercase()))
            .unwrap_or(false);
        if dx_armed {
            eprintln!("DX_REPLAY armed for {h} {tx_type}");
            if let Ok(list) = std::env::var("DX_REPLAY_SET") {
                for v in list.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                    std::env::set_var(v, "1");
                }
            }
        }
        let (our_ter, mut mods) = native_apply_one(&state, &txf);
        if dx_armed {
            if let Ok(list) = std::env::var("DX_REPLAY_SET") {
                for v in list.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                    std::env::remove_var(v);
                }
            }
        }
        // Thread every materially-changed write with this tx's hash + the
        // ledger seq (rippled ApplyStateTable). The meta's FinalFields never
        // carry PreviousTxn*, so the per-tx compares are unaffected and the
        // truth-overlay below preserves the stamps; DX_THREADCHECK verifies
        // them against the real post-state at fixture end.
        xrpl_ledger::ledger::threading::stamp_threading(
            &mut mods,
            &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
            h,
            seq as u32,
        );
        if std::env::var("DX_THREADCHECK").is_ok() || std::env::var("DX_BYTECHECK").is_ok() {
            for (k, ent) in mods.iter() {
                if !matches!(ent, SandboxEntry::Deleted) {
                    thread_touched_keys.insert(*k);
                } else {
                    thread_touched_keys.remove(k);
                }
            }
        }
        let our_mut: HashSet<(String, u8)> = native_mutset(&state, &mods)
            .into_iter()
            .filter(|(k, _)| !touched_only.contains(k))
            .collect();
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
        // DX_VALCHECK=<rel-tol|1>: compare the VALUES we wrote against
        // mainnet's FinalFields, for every node BOTH sides mutated.
        //
        // The mutation set compares KEYS ONLY, so a value violation is
        // invisible to it however severe. #106146362 75511674AD58 wrote a book
        // maker's trust line to -0.35248615830377 — a non-issuer minting the
        // currency it was selling — and the probe reported that as nothing more
        // than "5 missing nodes". The keys it DID emit were all correct.
        //
        // Opt-in, and it changes no verdict: this reports, it does not judge.
        // The engine has been calibrated against key-level matching throughout,
        // so switching the verdict to values would surface an unknown backlog
        // all at once. Measure the backlog first.
        if std::env::var("DX_VALCHECK").is_ok() {
            // EXACT, because the target is BYTE PARITY. A validator serialises
            // the stored decimal into the SLE and hashes it: one digit apart is
            // a different state hash and a diverged node, however small the
            // number. There is no economically-negligible difference here.
            //
            // This replaced an f64 comparison against a 1e-12 tolerance, which
            // could not certify that. Measured 2026-08-11 on ten ledgers the
            // census called CLEAN: tightening to 1e-18 surfaced three real
            // 1-ulp differences it had been hiding (e.g. 0.1141930002244327 vs
            // ...26). And f64 cannot go tighter — a 16-digit IOU mantissa sits
            // at its 2.2e-16 noise floor, and above 2^53 drops it is blind
            // outright: `9007199254740993 == 9007199254740992` in f64, so a
            // one-drop error on a >9e9 XRP balance was structurally invisible.
            //
            // `DX_VALCHECK_MIN=<rel>` still filters by relative size, for
            // triage only — it never widens what counts as a difference.
            let triage: Option<f64> =
                std::env::var("DX_VALCHECK_MIN").ok().and_then(|s| s.parse().ok());
            /// The stored value as an exact canonical (negative, mantissa,
            /// exponent), trailing zeros stripped so mainnet's
            /// "1373308620221269e-4" and our "137330862022.1269" — the same
            /// number, different serialisations — compare EQUAL.
            fn canon(v: &Value) -> Option<(bool, u128, i32)> {
                let s = match v {
                    Value::String(s) => s.as_str(),
                    Value::Object(_) => v.get("value")?.as_str()?,
                    _ => return None,
                };
                let s = s.trim();
                let (neg, s) = match s.strip_prefix('-') {
                    Some(r) => (true, r),
                    None => (false, s.strip_prefix('+').unwrap_or(s)),
                };
                let (mant_str, exp10) = match s.find(['e', 'E']) {
                    Some(i) => (&s[..i], s[i + 1..].parse::<i32>().ok()?),
                    None => (s, 0),
                };
                let (int_part, frac) = match mant_str.find('.') {
                    Some(i) => (&mant_str[..i], &mant_str[i + 1..]),
                    None => (mant_str, ""),
                };
                let digits = format!("{int_part}{frac}");
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let mut mant: u128 = digits.parse().ok()?;
                if mant == 0 {
                    return Some((false, 0, 0));
                }
                let mut exp = exp10 - frac.len() as i32;
                // Canonicalise to STAmount precision. An IOU mantissa is
                // EXACTLY 16 significant digits, so that is the form the SLE
                // serialises and the state hash covers — comparing raw JSON
                // instead reports differences the ledger cannot even represent.
                //
                // Our sandbox carries `Me` values wider than that: measured
                // 2026-08-11, five of six raw-string differences on
                // supposedly-clean ledgers were ours holding 17-20 digits
                // where mainnet holds 16, e.g. 1908.0660169997755673 against
                // 1908.066016999776 — the SAME STAmount. Only
                // 0.1141930002244327 vs ...26 was a real one-ulp difference.
                const HI: u128 = 10_000_000_000_000_000; // 1e16
                while mant >= HI {
                    let (q, r) = (mant / 10, mant % 10);
                    // half-even, matching Number's default rounding
                    mant = if r > 5 || (r == 5 && q % 2 == 1) { q + 1 } else { q };
                    exp += 1;
                }
                while mant % 10 == 0 && mant > 0 {
                    mant /= 10;
                    exp += 1;
                }
                if mant == 0 {
                    return Some((false, 0, 0));
                }
                Some((neg, mant, exp))
            }
            fn shown(v: &Value) -> String {
                match v {
                    Value::String(s) => s.clone(),
                    Value::Object(_) => {
                        v.get("value").and_then(|x| x.as_str()).unwrap_or("?").to_string()
                    }
                    _ => "?".to_string(),
                }
            }
            let mut net_fields: HashMap<String, &Value> = HashMap::new();
            if let Some(nodes) = net["nodes"].as_array() {
                for n in nodes {
                    if let (Some(k), Some(f)) = (n[0].as_str(), n.get(2)) {
                        if f.is_object() {
                            net_fields.insert(k.to_string(), f);
                        }
                    }
                }
            }
            for (k, ent) in mods.iter() {
                let bytes = match ent {
                    SandboxEntry::Created(b) | SandboxEntry::Modified(b) => b,
                    SandboxEntry::Deleted => continue,
                };
                let key = hex::encode_upper(k.0);
                let Some(nf) = net_fields.get(&key).copied() else { continue };
                let Ok(ours) = serde_json::from_slice::<Value>(bytes) else { continue };
                let kind = ours.get("LedgerEntryType").and_then(|v| v.as_str()).unwrap_or("");
                // Only the value-bearing fields. Pointers and flags are already
                // covered by the key-level compare and by `canon_ptrs`.
                let fields: &[&str] = match kind {
                    "RippleState" | "AccountRoot" => &["Balance"],
                    "Offer" => &["TakerGets", "TakerPays"],
                    _ => continue,
                };
                for f in fields {
                    let (Some(a), Some(b)) = (canon(&ours[*f]), canon(&nf[*f])) else { continue };
                    if a == b {
                        continue;
                    }
                    if let Some(min) = triage {
                        // Relative size, computed only to FILTER the report.
                        let val = |(n, m, e): (bool, u128, i32)| {
                            let x = m as f64 * 10f64.powi(e);
                            if n { -x } else { x }
                        };
                        let (x, y) = (val(a), val(b));
                        let scale = x.abs().max(y.abs()).max(1e-30);
                        if (x - y).abs() / scale < min {
                            continue;
                        }
                    }
                    eprintln!(
                        "DX_VALCHECK {h} {tx_type} {key} {kind}.{f} ours={} net={}",
                        shown(&ours[*f]),
                        shown(nf.get(*f).unwrap_or(&Value::Null))
                    );
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
        // The cap is a DISPLAY limit, and at 8 it SATURATES — #106348756 reports
        // "missing: 8" for a 16-node gap, which reads as a count and is not one.
        // DX_KEYS raises it for an investigation; the default keeps gate output
        // byte-identical.
        let kcap: usize = std::env::var("DX_KEYS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        let missing: Vec<String> = net_mut.difference(&our_mut)
            .map(|(k, b)| format!("{k}:{b}")).take(kcap).collect();
        let extra: Vec<String> = our_mut.difference(&net_mut)
            .map(|(k, b)| format!("{k}:{b}")).take(kcap).collect();
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
        if std::env::var("DX_THREADCHECK").is_ok() {
            // Compare our stamped threading against the REAL post-state at
            // this ledger — the fixture metas cannot carry it, ledger_entry
            // can. Objects overwritten by later txs hold the LAST stamp, the
            // same object the state hash serializes.
            let (mut checked, mut mismatch) = (0u32, 0u32);
            for k in &thread_touched_keys {
                let Some(bytes) = state.state_map.lookup(k) else { continue };
                let Ok(ours) = serde_json::from_slice::<Value>(&bytes) else { continue };
                let (Some(oid), Some(oseq)) = (
                    ours.get("PreviousTxnID").and_then(|v| v.as_str()),
                    ours.get("PreviousTxnLgrSeq").and_then(|v| v.as_u64()),
                ) else { continue };
                let khex = hex::encode_upper(k.0);
                let Some(res) = rpc(&rpc_url, "ledger_entry",
                    json!({"index": &khex, "ledger_index": seq})) else { continue };
                let Some(node) = res.get("node") else { continue };
                let (nid, nseq) = (
                    node.get("PreviousTxnID").and_then(|v| v.as_str()).unwrap_or(""),
                    node.get("PreviousTxnLgrSeq").and_then(|v| v.as_u64()).unwrap_or(0),
                );
                checked += 1;
                if !oid.eq_ignore_ascii_case(nid) || oseq != nseq {
                    mismatch += 1;
                    println!("THREADCHECK-MISMATCH {khex} ours=({oid},{oseq}) net=({nid},{nseq})");
                }
            }
            println!("THREADCHECK checked={checked} mismatch={mismatch}");
        }
        if std::env::var("DX_BYTECHECK").is_ok() {
            // BYTE census: serialize every surviving touched node through the
            // canonical codec and compare against the real post-state blob
            // (`ledger_entry binary=true`). The prep pass re-SPELLS values the
            // engine stores in tolerated-but-noncanonical JSON forms (numeric
            // dir pointers, unpadded/decimal u64s, hex account ids) without
            // changing any value — so a mismatch here is REAL drift: a wrong
            // value, a missing or extra field, a wrong flag.
            let (mut checked, mut mismatch, mut encfail) = (0u32, 0u32, 0u32);
            for k in &thread_touched_keys {
                let Some(bytes) = state.state_map.lookup(k) else { continue };
                let Ok(mut ours) = serde_json::from_slice::<Value>(&bytes) else { continue };
                canon_for_encode(&mut ours);
                let khex = hex::encode_upper(k.0);
                let Some(res) = rpc(&rpc_url, "ledger_entry",
                    json!({"index": &khex, "ledger_index": seq, "binary": true})) else { continue };
                let Some(net_hex) = res.get("node_binary").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Ok(net) = hex::decode(net_hex) else { continue };
                checked += 1;
                if let Ok(want) = std::env::var("DX_BYTEDUMP") {
                    if !want.is_empty() && khex.starts_with(&want.to_uppercase()) {
                        println!("BYTEDUMP-OURS {khex} {}", serde_json::to_string(&ours).unwrap_or_default());
                        if let Some(nj) = rpc(&rpc_url, "ledger_entry",
                            json!({"index": &khex, "ledger_index": seq})).and_then(|r| r.get("node").cloned()) {
                            println!("BYTEDUMP-NET  {khex} {}", serde_json::to_string(&nj).unwrap_or_default());
                        }
                    }
                }
                match xrpl_core::codec::encode::encode_transaction_json(&ours, false) {
                    Err(e) => {
                        encfail += 1;
                        println!("BYTECHECK-ENCODE-FAIL {khex} {e:?}");
                    }
                    Ok(enc) => {
                        if enc != net {
                            mismatch += 1;
                            let ofs = enc
                                .iter()
                                .zip(net.iter())
                                .position(|(a, b)| a != b)
                                .unwrap_or_else(|| enc.len().min(net.len()));
                            let ctx = |b: &[u8]| {
                                let lo = ofs.saturating_sub(8);
                                let hi = (ofs + 24).min(b.len());
                                hex::encode_upper(&b[lo.min(b.len())..hi])
                            };
                            let ty = ours
                                .get("LedgerEntryType")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            println!(
                                "BYTECHECK-MISMATCH {khex} ty={ty} ofs={ofs} ours_len={} net_len={} ours=..{} net=..{}",
                                enc.len(), net.len(), ctx(&enc), ctx(&net)
                            );
                        }
                    }
                }
            }
            println!("BYTECHECK checked={checked} mismatch={mismatch} encfail={encfail}");
        }
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
    let failed = RPC_FAILED.load(std::sync::atomic::Ordering::Relaxed);
    if failed > 0 {
        eprintln!(
            "PROBE: HYDRATION-FAILED ({failed} fetch(es) failed after retries) \
             — verdict withheld, the pre-state is incomplete"
        );
        return 3;
    }
    if any_diverge { 1 } else { 0 }
}

#[cfg(test)]
mod cache_key_tests {
    use super::cache_key;
    use serde_json::json;

    /// The RPC server must be part of the cache key. One dxcache directory is
    /// shared by every probe run on the box and they do not all talk to the
    /// same node — the scout hydrates from .39, re-probes of older finds must
    /// use s2. Serving one server's answer for the other's question corrupts
    /// PAGINATED calls in particular, because `account_objects` markers are
    /// issued by a server and meaningless to another.
    #[test]
    fn cache_key_separates_servers() {
        let params = json!({"account": "rAAA", "ledger_index": 1, "type": "nft_page"});
        assert_ne!(
            cache_key("https://s2.ripple.com:51234", "account_objects", &params),
            cache_key("http://10.0.0.39:5005", "account_objects", &params),
            "two servers must not share a cache entry",
        );
        // Same server, different marker, still distinct.
        let mut p2 = params.clone();
        p2["marker"] = json!("abc");
        assert_ne!(
            cache_key("https://s2.ripple.com:51234", "account_objects", &params),
            cache_key("https://s2.ripple.com:51234", "account_objects", &p2),
        );
    }
}
