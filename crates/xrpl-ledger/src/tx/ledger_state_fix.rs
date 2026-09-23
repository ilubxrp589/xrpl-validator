//! LedgerStateFix — the paid repair transaction (rippled
//! `transactors/system/LedgerStateFix.cpp`, 3.4.0). Anyone may submit one;
//! the fee is one owner reserve (`calculateOwnerReserveFee`, like
//! AccountDelete), which the common fee path charges from the tx's `Fee`.
//!
//! Finding 359 (desk check against the 3.4.0 transaction list, 2026-09-22):
//! the type was never dispatched, so every mainnet LedgerStateFix — valid
//! since fixNFTokenPageLinks, enabled — would have applied as
//! `tecUNSUPPORTED` against the network's code: a guaranteed receipt.
//!
//! Two fix types, each carrying exactly one fix-specific field:
//!   1 NfTokenPageLink  (`Owner`): `nft::repairNFTokenDirectoryLinks` —
//!     relink the owner's NFTokenPage chain, move a last page that does not
//!     sit at the max key onto it. Nothing to repair → tecFAILED_PROCESSING.
//!   2 BookExchangeRate (`BookDirectory`, fixCleanup3_2_0): rewrite a book
//!     directory root's `ExchangeRate` to the quality its key encodes.

use crate::ledger::amendments;
use crate::ledger::nftpage::{max_page_key, page_key};
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use serde_json::Value;
use xrpl_core::types::Hash256;

const NFTOKEN_PAGE_LINK: u64 = 1;
const BOOK_EXCHANGE_RATE: u64 = 2;

fn fix_type(tx: &TxFields) -> Option<u64> {
    tx.fields.get("LedgerFixType").and_then(Value::as_u64)
}

fn owner(tx: &TxFields) -> Option<[u8; 20]> {
    crate::tx::offer::decode20(tx.fields.get("Owner")?.as_str()?)
}

fn book_directory(tx: &TxFields) -> Option<Hash256> {
    let b = hex::decode(tx.fields.get("BookDirectory")?.as_str()?).ok()?;
    Some(Hash256(b.try_into().ok()?))
}

/// `view.read(Keylet(type, key))`: absent when the key holds another type.
fn read_typed(sb: &Sandbox, key: &Hash256, ty: &str) -> Option<Value> {
    let v: Value = serde_json::from_slice(&sb.read(key)?).ok()?;
    (v["LedgerEntryType"].as_str() == Some(ty)).then_some(v)
}

fn write(sb: &mut Sandbox, key: Hash256, v: &Value) {
    sb.write(key, serde_json::to_vec(v).unwrap_or_default());
}

/// `getQuality`: a book directory key's low 64 bits, big-endian.
fn key_quality(key: &Hash256) -> u64 {
    let mut q = [0u8; 8];
    q.copy_from_slice(&key.0[24..]);
    u64::from_be_bytes(q)
}

fn exchange_rate(dir: &Value) -> Option<u64> {
    let v = dir.get("ExchangeRate")?;
    v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()).or_else(|| v.as_u64())
}

pub struct LedgerStateFixTransactor;

impl Transactor for LedgerStateFixTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        // tefINVALID_LEDGER_FIX_TYPE / temINVALID: neither lands in a ledger.
        let has = |f: &str| tx.fields.get(f).is_some();
        match fix_type(tx) {
            Some(NFTOKEN_PAGE_LINK) if has("Owner") && !has("BookDirectory") => TxResult::Success,
            Some(BOOK_EXCHANGE_RATE) if has("BookDirectory") && !has("Owner") => TxResult::Success,
            Some(NFTOKEN_PAGE_LINK | BOOK_EXCHANGE_RATE) => TxResult::InvalidTx,
            _ => TxResult::Malformed,
        }
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        match fix_type(tx) {
            Some(NFTOKEN_PAGE_LINK) => {
                let Some(o) = owner(tx) else { return TxResult::Malformed };
                if sandbox.read(&keylet::account_root_key(&o)).is_none() {
                    return TxResult::ObjectNotFound;
                }
                TxResult::Success
            }
            Some(BOOK_EXCHANGE_RATE) => {
                // preflight's temDISABLED, judged here where the rules are readable.
                if !amendments::fix_cleanup_3_2_0(sandbox) {
                    return TxResult::Malformed;
                }
                let Some(k) = book_directory(tx) else { return TxResult::Malformed };
                let Some(dir) = read_typed(sandbox, &k, "DirectoryNode") else {
                    return TxResult::ObjectNotFound;
                };
                // Only a book's first page carries ExchangeRate; a correct one
                // has nothing to fix.
                match exchange_rate(&dir) {
                    Some(r) if r != key_quality(&k) => TxResult::Success,
                    _ => TxResult::NoPermission,
                }
            }
            _ => TxResult::Malformed,
        }
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        match fix_type(tx) {
            Some(NFTOKEN_PAGE_LINK) => {
                let Some(o) = owner(tx) else { return TxResult::Malformed };
                if repair_nftoken_directory_links(sandbox, &o) {
                    TxResult::Success
                } else {
                    TxResult::TecFailedProcessing
                }
            }
            Some(BOOK_EXCHANGE_RATE) => {
                let Some(k) = book_directory(tx) else { return TxResult::Malformed };
                let Some(mut dir) = read_typed(sandbox, &k, "DirectoryNode") else {
                    return TxResult::ObjectNotFound;
                };
                dir["ExchangeRate"] = Value::String(format!("{:016x}", key_quality(&k)));
                write(sandbox, k, &dir);
                TxResult::Success
            }
            _ => TxResult::Malformed,
        }
    }
}

/// `uint256::next()`.
fn next_key(k: &Hash256) -> Hash256 {
    let mut b = k.0;
    for byte in b.iter_mut().rev() {
        let (v, carry) = byte.overflowing_add(1);
        *byte = v;
        if !carry {
            break;
        }
    }
    Hash256(b)
}

/// `ReadView::succ(after, before)`: the first key strictly inside the OPEN
/// interval (after, before). `keys` is sorted ascending.
fn succ(keys: &[Hash256], after: &Hash256, before: &Hash256) -> Option<Hash256> {
    keys.iter().find(|k| k.0 > after.0 && k.0 < before.0).copied()
}

fn link(page: &Value, field: &str) -> Option<Hash256> {
    let b = hex::decode(page.get(field)?.as_str()?).ok()?;
    Some(Hash256(b.try_into().ok()?))
}

fn set_link(page: &mut Value, field: &str, to: &Hash256) {
    page[field] = Value::String(hex::encode_upper(to.0));
}

fn drop_field(page: &mut Value, field: &str) {
    if let Some(o) = page.as_object_mut() {
        o.remove(field);
    }
}

/// rippled `nft::repairNFTokenDirectoryLinks` (NFTokenHelpers.cpp:640-758),
/// step for step, including its walk: every successor is taken with
/// `succ(page.key().next(), last.next())`, so the search starts one PAST
/// key+1, and a missing successor falls back to peeking the max page.
/// Returns whether anything was repaired.
pub fn repair_nftoken_directory_links(sb: &mut Sandbox, owner: &[u8; 20]) -> bool {
    let first_bound = page_key(owner, &[0u8; 12]);
    let last = max_page_key(owner);
    let past_last = next_key(&last);
    // The owner's page keys are the owner id plus a 96-bit bound.
    let keys = sb.keys_with_prefix(&owner[..]);
    let mut did = false;

    let mut page_k = succ(&keys, &first_bound, &past_last).unwrap_or(last);
    let Some(mut page) = read_typed(sb, &page_k, "NFTokenPage") else { return false };

    if page_k == last {
        // The only page: it carries no links at all.
        let (next, prev) = (page.get("NextPageMin").is_some(), page.get("PreviousPageMin").is_some());
        if next || prev {
            drop_field(&mut page, "PreviousPageMin");
            drop_field(&mut page, "NextPageMin");
            write(sb, page_k, &page);
            did = true;
        }
        return did;
    }

    // The first page has no previous link.
    if page.get("PreviousPageMin").is_some() {
        drop_field(&mut page, "PreviousPageMin");
        write(sb, page_k, &page);
        did = true;
    }

    let mut reached_last: Option<Value> = None;
    loop {
        let next_k = succ(&keys, &next_key(&page_k), &past_last).unwrap_or(last);
        let Some(mut next) = read_typed(sb, &next_k, "NFTokenPage") else { break };
        if link(&page, "NextPageMin") != Some(next_k) {
            set_link(&mut page, "NextPageMin", &next_k);
            write(sb, page_k, &page);
            did = true;
        }
        if link(&next, "PreviousPageMin") != Some(page_k) {
            set_link(&mut next, "PreviousPageMin", &page_k);
            write(sb, next_k, &next);
            did = true;
        }
        if next_k == last {
            reached_last = Some(next);
            break;
        }
        page = next;
        page_k = next_k;
    }

    let Some(mut last_page) = reached_last else {
        // `page` is the owner's last page but it does not sit at the max key:
        // its tokens move to a fresh max page (the owner count is unchanged —
        // one page added, one removed), which inherits the previous link.
        let mut moved = serde_json::json!({
            "LedgerEntryType": "NFTokenPage",
            "Flags": 0,
            "NFTokens": page.get("NFTokens").cloned().unwrap_or_else(|| Value::Array(vec![])),
        });
        if let Some(prev_k) = link(&page, "PreviousPageMin") {
            set_link(&mut moved, "PreviousPageMin", &prev_k);
            // rippled throws when the previous page is missing ("cannot be
            // repaired"); the transaction then fails without a ledger entry.
            let Some(mut prev) = read_typed(sb, &prev_k, "NFTokenPage") else { return did };
            set_link(&mut prev, "NextPageMin", &last);
            write(sb, prev_k, &prev);
        }
        sb.delete(page_k);
        write(sb, last, &moved);
        return true;
    };

    // The last page has no next link.
    if last_page.get("NextPageMin").is_some() {
        drop_field(&mut last_page, "NextPageMin");
        write(sb, last, &last_page);
        did = true;
    }
    did
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::amendments::FIX_CLEANUP_3_2_0;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::keylet;
    use crate::ledger::nftpage::{max_page_key, page_key};
    use crate::ledger::sandbox::SandboxEntry;
    use crate::ledger::state::LedgerState;
    use crate::tx::dispatch::apply_on_sandbox;
    use serde_json::{json, Value};
    use xrpl_core::types::Hash256;

    const SENDER: [u8; 20] = [0x11; 20];
    const OWNER: [u8; 20] = [0x22; 20];
    const FEE: u64 = 200_000; // one owner reserve, as rippled charges

    fn state(with_fix_cleanup_3_2_0: bool) -> LedgerState {
        let mut st = LedgerState::new_unverified(LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        });
        for id in [SENDER, OWNER] {
            let acct = json!({"LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": "1000000000",
                              "Sequence": 5, "OwnerCount": 3, "Flags": 0});
            st.state_map.insert(keylet::account_root_key(&id), serde_json::to_vec(&acct).unwrap()).unwrap();
        }
        if with_fix_cleanup_3_2_0 {
            let am = json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": [FIX_CLEANUP_3_2_0]});
            st.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&am).unwrap()).unwrap();
        }
        st
    }

    fn put(st: &mut LedgerState, k: Hash256, v: Value) {
        st.state_map.insert(k, serde_json::to_vec(&v).unwrap()).unwrap();
    }

    fn fix_tx(fields: Value) -> TxFields {
        let mut f = json!({"TransactionType": "LedgerStateFix"});
        for (k, v) in fields.as_object().unwrap() {
            f[k] = v.clone();
        }
        TxFields {
            account: SENDER,
            tx_type: "LedgerStateFix".into(),
            fee: FEE,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: f,
            inner_batch: false,
        }
    }

    fn read(sb: &Sandbox, k: &Hash256) -> Option<Value> {
        sb.read(k).map(|b| serde_json::from_slice(&b).unwrap())
    }

    fn sender_balance(sb: &Sandbox) -> u64 {
        read(sb, &keylet::account_root_key(&SENDER)).unwrap()["Balance"].as_str().unwrap().parse().unwrap()
    }

    /// Only the fee and the sequence moved: a claimed tec.
    fn assert_fee_only(sb: &Sandbox) {
        assert_eq!(sender_balance(sb), 1_000_000_000 - FEE);
        let touched: Vec<_> = sb.modifications().keys().copied().collect();
        assert_eq!(touched, vec![keylet::account_root_key(&SENDER)], "a tec writes the fee and sequence only");
    }

    fn book_dir_key(quality: u64) -> Hash256 {
        let mut k = [0x5Au8; 32];
        k[24..].copy_from_slice(&quality.to_be_bytes());
        Hash256(k)
    }

    fn book_root(key: &Hash256, rate: Option<u64>) -> Value {
        let mut v = json!({"LedgerEntryType": "DirectoryNode", "Flags": 0, "RootIndex": hex::encode_upper(key.0),
                           "Indexes": [hex::encode_upper([0x77u8; 32])],
                           "TakerPaysCurrency": "0".repeat(40), "TakerPaysIssuer": "0".repeat(40),
                           "TakerGetsCurrency": format!("{:0>40}", "5553440000000000"), "TakerGetsIssuer": hex::encode(OWNER)});
        if let Some(r) = rate {
            v["ExchangeRate"] = json!(format!("{r:016x}"));
        }
        v
    }

    #[test]
    fn book_exchange_rate_rewrites_a_wrong_root_rate_to_the_key_quality() {
        let q = 0x5A1C_6BF5_2634_0000u64;
        let k = book_dir_key(q);
        let mut st = state(true);
        put(&mut st, k, book_root(&k, Some(q + 7)));
        let mut sb = Sandbox::new(&st);
        let (r, applied) = apply_on_sandbox(&fix_tx(json!({"LedgerFixType": 2, "BookDirectory": hex::encode_upper(k.0)})), &mut sb);
        assert_eq!((r.code_str(), applied), ("tesSUCCESS", true));
        let dir = read(&sb, &k).unwrap();
        assert_eq!(u64::from_str_radix(dir["ExchangeRate"].as_str().unwrap(), 16).unwrap(), q);
        assert_eq!(dir["Indexes"], book_root(&k, None)["Indexes"], "only the rate changes");
        assert_eq!(sender_balance(&sb), 1_000_000_000 - FEE);
    }

    #[test]
    fn book_exchange_rate_on_a_correct_root_is_no_permission() {
        let q = 0x5A1C_6BF5_2634_0000u64;
        let k = book_dir_key(q);
        let mut st = state(true);
        put(&mut st, k, book_root(&k, Some(q)));
        let mut sb = Sandbox::new(&st);
        let (r, applied) = apply_on_sandbox(&fix_tx(json!({"LedgerFixType": 2, "BookDirectory": hex::encode_upper(k.0)})), &mut sb);
        assert_eq!((r.code_str(), applied), ("tecNO_PERMISSION", true));
        assert_fee_only(&sb);
    }

    #[test]
    fn book_exchange_rate_on_a_page_without_a_rate_is_no_permission() {
        let k = book_dir_key(42);
        let mut st = state(true);
        put(&mut st, k, book_root(&k, None));
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&fix_tx(json!({"LedgerFixType": 2, "BookDirectory": hex::encode_upper(k.0)})), &mut sb);
        assert_eq!(r.code_str(), "tecNO_PERMISSION");
        assert_fee_only(&sb);
    }

    #[test]
    fn book_exchange_rate_on_a_missing_or_non_directory_key_is_object_not_found() {
        let mut st = state(true);
        let mut sb = Sandbox::new(&st);
        let (r, _) = apply_on_sandbox(&fix_tx(json!({"LedgerFixType": 2, "BookDirectory": hex::encode_upper(book_dir_key(9).0)})), &mut sb);
        assert_eq!(r.code_str(), "tecOBJECT_NOT_FOUND");
        assert_fee_only(&sb);
        // A key holding some other entry type reads as absent through a typed keylet.
        let acct_key = keylet::account_root_key(&OWNER);
        drop(sb);
        put(&mut st, book_dir_key(10), json!({"LedgerEntryType": "Offer", "Flags": 0}));
        for k in [acct_key, book_dir_key(10)] {
            let mut sb = Sandbox::new(&st);
            let (r, _) = apply_on_sandbox(&fix_tx(json!({"LedgerFixType": 2, "BookDirectory": hex::encode_upper(k.0)})), &mut sb);
            assert_eq!(r.code_str(), "tecOBJECT_NOT_FOUND");
        }
    }

    #[test]
    fn book_exchange_rate_needs_fix_cleanup_3_2_0() {
        let q = 77u64;
        let k = book_dir_key(q);
        let mut st = state(false);
        put(&mut st, k, book_root(&k, Some(q + 1)));
        let mut sb = Sandbox::new(&st);
        let (r, applied) = apply_on_sandbox(&fix_tx(json!({"LedgerFixType": 2, "BookDirectory": hex::encode_upper(k.0)})), &mut sb);
        assert!(!applied && !r.is_claimed(), "temDISABLED never lands: {r:?}");
        assert!(sb.modifications().is_empty());
    }

    #[test]
    fn malformed_fix_fields_never_land() {
        let st = state(true);
        for f in [
            json!({"LedgerFixType": 1}),                                                     // no Owner
            json!({"LedgerFixType": 2}),                                                     // no BookDirectory
            json!({"LedgerFixType": 1, "Owner": hex::encode(OWNER), "BookDirectory": hex::encode_upper([1u8; 32])}),
            json!({"LedgerFixType": 3, "Owner": hex::encode(OWNER)}),                        // unknown type (tef)
            json!({"Owner": hex::encode(OWNER)}),                                            // no type
        ] {
            let mut sb = Sandbox::new(&st);
            let (r, applied) = apply_on_sandbox(&fix_tx(f.clone()), &mut sb);
            assert!(!applied && !r.is_claimed(), "{f} → {r:?}");
            assert!(sb.modifications().is_empty(), "{f}");
        }
    }

    // ---- NFTokenPage links ----

    fn pk(bound: u8) -> Hash256 {
        page_key(&OWNER, &[bound; 12])
    }

    fn page(tokens: u8, prev: Option<Hash256>, next: Option<Hash256>) -> Value {
        let mut v = json!({"LedgerEntryType": "NFTokenPage", "Flags": 0,
                           "NFTokens": [{"NFToken": {"NFTokenID": hex::encode_upper([tokens; 32])}}]});
        if let Some(p) = prev {
            v["PreviousPageMin"] = json!(hex::encode_upper(p.0));
        }
        if let Some(n) = next {
            v["NextPageMin"] = json!(hex::encode_upper(n.0));
        }
        v
    }

    fn link(v: &Value, f: &str) -> Option<Hash256> {
        let b = hex::decode(v.get(f)?.as_str()?).ok()?;
        Some(Hash256(b.try_into().ok()?))
    }

    fn run_owner(st: &LedgerState, owner: [u8; 20]) -> (String, Sandbox<'_>) {
        let mut sb = Sandbox::new(st);
        let (r, _) = apply_on_sandbox(&fix_tx(json!({"LedgerFixType": 1, "Owner": hex::encode(owner)})), &mut sb);
        (r.code_str().to_string(), sb)
    }

    #[test]
    fn nft_page_link_for_a_missing_owner_is_object_not_found() {
        let st = state(true);
        let (r, sb) = run_owner(&st, [0x33; 20]);
        assert_eq!(r, "tecOBJECT_NOT_FOUND");
        assert_fee_only(&sb);
    }

    #[test]
    fn nft_page_link_with_nothing_to_repair_is_failed_processing() {
        // No pages at all, then a healthy three-page chain: rippled's repair
        // reports didRepair=false and LedgerStateFix answers tecFAILED_PROCESSING.
        let mut st = state(true);
        let (r, sb) = run_owner(&st, OWNER);
        assert_eq!(r, "tecFAILED_PROCESSING");
        assert_fee_only(&sb);
        drop(sb);
        let (a, b, m) = (pk(0x10), pk(0x80), max_page_key(&OWNER));
        put(&mut st, a, page(1, None, Some(b)));
        put(&mut st, b, page(2, Some(a), Some(m)));
        put(&mut st, m, page(3, Some(b), None));
        let (r, sb) = run_owner(&st, OWNER);
        assert_eq!(r, "tecFAILED_PROCESSING");
        assert_fee_only(&sb);
    }

    #[test]
    fn nft_page_link_single_page_drops_its_stray_links() {
        let mut st = state(true);
        let m = max_page_key(&OWNER);
        put(&mut st, m, page(3, Some(pk(0x10)), Some(pk(0x20))));
        let (r, sb) = run_owner(&st, OWNER);
        assert_eq!(r, "tesSUCCESS");
        let p = read(&sb, &m).unwrap();
        assert!(p.get("PreviousPageMin").is_none() && p.get("NextPageMin").is_none(), "{p}");
    }

    #[test]
    fn nft_page_link_relinks_a_broken_chain() {
        let mut st = state(true);
        let (a, b, m) = (pk(0x10), pk(0x80), max_page_key(&OWNER));
        put(&mut st, a, page(1, Some(pk(0x01)), None)); // first page: stray prev, missing next
        put(&mut st, b, page(2, Some(pk(0x05)), Some(m))); // wrong prev
        put(&mut st, m, page(3, None, Some(pk(0x99)))); // missing prev, stray next
        let (r, sb) = run_owner(&st, OWNER);
        assert_eq!(r, "tesSUCCESS");
        let (pa, pb, pm) = (read(&sb, &a).unwrap(), read(&sb, &b).unwrap(), read(&sb, &m).unwrap());
        assert_eq!((link(&pa, "PreviousPageMin"), link(&pa, "NextPageMin")), (None, Some(b)));
        assert_eq!((link(&pb, "PreviousPageMin"), link(&pb, "NextPageMin")), (Some(a), Some(m)));
        assert_eq!((link(&pm, "PreviousPageMin"), link(&pm, "NextPageMin")), (Some(b), None));
        assert_eq!(pa["NFTokens"], page(1, None, None)["NFTokens"], "tokens untouched");
    }

    #[test]
    fn nft_page_link_moves_a_last_page_that_is_not_at_the_max_key() {
        let mut st = state(true);
        let (a, b, m) = (pk(0x10), pk(0x80), max_page_key(&OWNER));
        put(&mut st, a, page(1, None, Some(b)));
        put(&mut st, b, page(2, Some(a), Some(pk(0x90)))); // the real last page, at the wrong key
        let (r, sb) = run_owner(&st, OWNER);
        assert_eq!(r, "tesSUCCESS");
        assert!(matches!(sb.modifications().get(&b), Some(SandboxEntry::Deleted)), "the misplaced page is erased");
        let pm = read(&sb, &m).expect("its tokens now live on the max page");
        assert_eq!(pm["NFTokens"], page(2, None, None)["NFTokens"]);
        assert_eq!((link(&pm, "PreviousPageMin"), link(&pm, "NextPageMin")), (Some(a), None));
        assert_eq!(link(&read(&sb, &a).unwrap(), "NextPageMin"), Some(m));
        assert_eq!(pm["LedgerEntryType"], "NFTokenPage");
    }

    #[test]
    fn nft_page_link_skips_a_page_one_past_the_current_key() {
        // rippled walks with succ(page.key().next(), …): the open interval
        // starts one PAST key+1, so a page at exactly key+1 is never visited.
        let mut st = state(true);
        let a = pk(0x10);
        let mut a1 = a;
        a1.0[31] = a1.0[31].wrapping_add(1);
        let m = max_page_key(&OWNER);
        put(&mut st, a, page(1, None, Some(m)));
        put(&mut st, a1, page(2, None, None));
        put(&mut st, m, page(3, Some(a), None));
        let (r, _sb) = run_owner(&st, OWNER);
        assert_eq!(r, "tecFAILED_PROCESSING", "a → max is already consistent once key+1 is skipped");
    }
}
