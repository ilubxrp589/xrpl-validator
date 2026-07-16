//! Pure logic for prefetching an account's NFTokenPage set so
//! `RpcProvider::succ` can answer NFT page-walk probes.
//!
//! Why this exists: rippled locates the page holding an NFToken with
//! `view.succ(first.key, nftpage_max(owner).key.next())` (see `nft.cpp`
//! `getPageForToken` / `findToken`). Two properties make that probe
//! unservable by the cold `ledger_data` walk in `succ_walk`:
//!
//! 1. `first` is `nftpage(nftpage_min(owner), nftokenID)` — a key derived
//!    from the token's low 96 bits, which almost never exists as a ledger
//!    object. Clio-backed endpoints reject any `ledger_data` marker that is
//!    not an EXISTING key at the requested ledger (`markerDoesNotExist`),
//!    so the walk cannot even start. This is the same wall `offer_books`
//!    hits for book bases.
//! 2. The book prefetch is blind here: NFT pages are not directory keys and
//!    `book_offers` never surfaces them, so a walk that consults the
//!    prefetched book view only will not see NFT pages at all.
//!
//! The fix mirrors `offer_books`: prefetch the owner's pages via
//! `account_objects` (pinned to the pre-ledger index) and answer probes
//! from that set. NFTokenPage keys are OWNER-PREFIXED — the top 160 bits
//! are the owner's AccountID and the low 96 bits are the page tag — so a
//! probe key identifies its owner directly.
//!
//! The exclusive upper bound is load-bearing (6893bbe lesson):
//! `nftpage_max(owner).next()` is `owner+1 || 0*96`, which IS a real key
//! whenever the next account by ID owns NFT pages. Answering the interval
//! as CLOSED would surface that foreign account's first page as this
//! owner's, which is exactly the "NFT found in incorrect page" invariant
//! failure. Every range query here is the open interval (key, last).
//!
//! Everything is bytes/json in → data out, so it is unit-testable offline;
//! the HTTP side lives in `ffi_engine::RpcProvider`.

use std::collections::BTreeSet;

/// rippled's `keylet::nftpage_min(owner)`: the owner's AccountID in the top
/// 160 bits, low 96 bits zeroed.
pub fn nftpage_min(owner: &[u8; 20]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..20].copy_from_slice(owner);
    out
}

/// rippled's `keylet::nftpage_max(owner)`: the owner's AccountID in the top
/// 160 bits, low 96 bits all ones.
pub fn nftpage_max(owner: &[u8; 20]) -> [u8; 32] {
    let mut out = [0xFFu8; 32];
    out[..20].copy_from_slice(owner);
    out
}

/// `nftpage_max(owner).next()` — the EXCLUSIVE upper bound rippled passes to
/// succ. This is `owner+1 || 0*96`, i.e. the next account's `nftpage_min`,
/// and it is a real key whenever that account owns NFT pages. `None` when
/// the owner is the all-ones AccountID and the bound would wrap past the
/// end of the keyspace (then no bound can exclude anything above).
pub fn nftpage_max_next(owner: &[u8; 20]) -> Option<[u8; 32]> {
    let mut next = *owner;
    for byte in next.iter_mut().rev() {
        let (v, carry) = byte.overflowing_add(1);
        *byte = v;
        if !carry {
            return Some(nftpage_min(&next));
        }
    }
    None
}

/// The owner AccountID a page-keyspace key belongs to (its top 160 bits).
pub fn owner_of_page_key(key: &[u8; 32]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(&key[..20]);
    out
}

/// Is the open interval (key, last) confined to `owner`'s NFT page range?
///
/// This is the guard that decides whether the prefetched page set is a
/// COMPLETE view of the interval, i.e. whether we may answer authoritatively
/// instead of walking. It holds exactly for rippled's page-walk probe shape:
/// `key` inside the owner's range, `last` no higher than
/// `nftpage_max(owner).next()`. An unbounded probe (`last == None`) can run
/// off the end of the owner's keyspace into unrelated objects, so it is NOT
/// answerable here — the caller must fall back to the walk.
pub fn interval_within_owner(owner: &[u8; 20], key: &[u8; 32], last: Option<&[u8; 32]>) -> bool {
    if owner_of_page_key(key) != *owner {
        return false;
    }
    match (last, nftpage_max_next(owner)) {
        // Owner sits at the very top of the keyspace: nothing above its
        // range exists, so any bound keeps the interval inside it.
        (_, None) => true,
        (Some(l), Some(bound)) => l.as_slice() <= bound.as_slice(),
        (None, Some(_)) => false,
    }
}

/// A prefetched owner's NFTokenPage set. `complete` means `account_objects`
/// provably returned every page (the walk ended with no marker), so "no
/// candidate" is an authoritative "no successor" rather than a truncation
/// artifact. An owner with NO pages at all is a legitimate complete set:
/// answering `None` there is what lets a first-ever mint place its page.
#[derive(Debug)]
pub struct OwnerNftPages {
    pub owner: [u8; 20],
    pub pages: BTreeSet<[u8; 32]>,
    pub complete: bool,
}

/// Outcome of answering succ from a prefetched page set.
#[derive(Debug, PartialEq, Eq)]
pub enum NftSuccAnswer {
    /// Smallest page key in the open interval (key, last).
    Found([u8; 32]),
    /// The prefetch held every page of this owner and nothing qualifies.
    NoneAuthoritative,
    /// Truncated prefetch, or the probe is not confined to the owner's
    /// range — the caller must fall back to the walk.
    Unknown,
}

/// Answer `succ(key, last)` from a prefetched page set. The interval is
/// OPEN on both ends: strictly greater than `key`, strictly less than
/// `last` (see module doc — `last` can equal a real foreign key).
pub fn succ_from_pages(
    pages: &OwnerNftPages,
    key: &[u8; 32],
    last: Option<&[u8; 32]>,
) -> NftSuccAnswer {
    use std::ops::Bound;
    if !interval_within_owner(&pages.owner, key, last) {
        return NftSuccAnswer::Unknown;
    }
    let upper = match last {
        Some(l) => Bound::Excluded(*l),
        None => Bound::Unbounded,
    };
    if let Some(cand) = pages.pages.range((Bound::Excluded(*key), upper)).next() {
        return NftSuccAnswer::Found(*cand);
    }
    if pages.complete {
        NftSuccAnswer::NoneAuthoritative
    } else {
        NftSuccAnswer::Unknown
    }
}

/// Read a serialized field header. Returns (type_code, field_code,
/// bytes_consumed). Same encoding as `offer_books::read_field_header`.
fn read_field_header(data: &[u8], pos: usize) -> Option<(u8, u8, usize)> {
    if pos >= data.len() {
        return None;
    }
    let tag = data[pos];
    let type_high = tag >> 4;
    let field_low = tag & 0x0F;
    if type_high != 0 && field_low != 0 {
        Some((type_high, field_low, 1))
    } else if type_high == 0 && field_low == 0 {
        if pos + 2 >= data.len() {
            return None;
        }
        Some((data[pos + 1], data[pos + 2], 3))
    } else if type_high == 0 {
        if pos + 1 >= data.len() {
            return None;
        }
        Some((data[pos + 1], field_low, 2))
    } else {
        if pos + 1 >= data.len() {
            return None;
        }
        Some((type_high, data[pos + 1], 2))
    }
}

/// Decode a variable-length length prefix. Returns (payload_len,
/// prefix_bytes_consumed).
fn read_vl(data: &[u8], pos: usize) -> Option<(usize, usize)> {
    let b1 = *data.get(pos)? as usize;
    if b1 <= 192 {
        Some((b1, 1))
    } else if b1 <= 240 {
        let b2 = *data.get(pos + 1)? as usize;
        Some((193 + (b1 - 193) * 256 + b2, 2))
    } else if b1 <= 254 {
        let b2 = *data.get(pos + 1)? as usize;
        let b3 = *data.get(pos + 2)? as usize;
        Some((12481 + (b1 - 241) * 65536 + b2 * 256 + b3, 3))
    } else {
        None
    }
}

/// Skip one serialized field VALUE of the given type. `None` on truncation
/// or an unknown type — the caller must abandon the scan rather than guess
/// at field boundaries.
fn skip_value(data: &[u8], pos: usize, type_code: u8) -> Option<usize> {
    let end = match type_code {
        1 => pos + 2,
        2 => pos + 4,
        3 => pos + 8,
        4 => pos + 16,
        5 => pos + 32,
        6 => {
            let b = *data.get(pos)?;
            // 0x80 → 48-byte IOU; 0x20 (without 0x80) → 33-byte MPT amount;
            // else 8-byte XRP.
            pos + if b & 0x80 != 0 {
                48
            } else if b & 0x20 != 0 {
                33
            } else {
                8
            }
        }
        7 | 8 | 19 => {
            let (len, consumed) = read_vl(data, pos)?;
            pos + consumed + len
        }
        14 => return skip_object(data, pos),
        15 => return skip_array(data, pos),
        16 => pos + 1,
        17 => pos + 20,
        18 => return skip_path_set(data, pos),
        _ => return None,
    };
    if end > data.len() {
        None
    } else {
        Some(end)
    }
}

/// Skip STObject contents up to and including the ObjectEndMarker (0xE1).
fn skip_object(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let (type_code, field_code, header_len) = read_field_header(data, pos)?;
        pos += header_len;
        if type_code == 14 && field_code == 1 {
            return Some(pos);
        }
        pos = skip_value(data, pos, type_code)?;
    }
}

/// Skip STArray contents up to and including the ArrayEndMarker (0xF1).
fn skip_array(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let (type_code, field_code, header_len) = read_field_header(data, pos)?;
        pos += header_len;
        if type_code == 15 && field_code == 1 {
            return Some(pos);
        }
        if type_code != 14 {
            return None;
        }
        pos = skip_object(data, pos)?;
    }
}

/// Skip a serialized PathSet body: 0xFF separates paths, 0x00 terminates
/// the set, other leading bytes are STPathElement type bits (0x01 account /
/// 0x10 currency / 0x20 issuer, 20 bytes each, in that order).
fn skip_path_set(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let flags = *data.get(pos)?;
        pos += 1;
        match flags {
            0x00 => return Some(pos),
            0xFF => {}
            _ => {
                if flags & !0x31 != 0 {
                    return None;
                }
                for bit in [0x01u8, 0x10, 0x20] {
                    if flags & bit != 0 {
                        pos += 20;
                        if pos > data.len() {
                            return None;
                        }
                    }
                }
            }
        }
    }
}

/// TransactionTypes whose apply path walks an account's NFT pages.
/// NFTokenMint (25) inserts a token; NFTokenBurn (26) removes one;
/// NFTokenCreateOffer (27) / NFTokenCancelOffer (28) / NFTokenAcceptOffer
/// (29) locate one to validate or transfer it.
fn is_nft_tx_type(tt: u16) -> bool {
    (25..=29).contains(&tt)
}

/// The accounts whose NFT pages a tx blob may walk: every AccountID-typed
/// (type 8) field it carries — Account, Owner, Issuer, Destination. The
/// zero account is dropped (it is a placeholder, never a page owner).
///
/// Over-collecting is safe and deliberate: an account with no pages costs
/// one `account_objects` call and yields an authoritative empty set. Under-
/// collecting silently drops us back to the cold walk. `NFTokenAcceptOffer`
/// is the known gap — the token owner lives in the referenced offer object,
/// not in the tx — so it is covered only when the tx names the owner.
///
/// Returns empty for non-NFT tx types and for anything malformed; prefetch
/// is best-effort and a skipped prefetch is exactly the pre-existing
/// behaviour (cold, never wrong).
pub fn parse_nft_page_owners(tx: &[u8]) -> Vec<[u8; 20]> {
    parse_nft_page_owners_inner(tx).unwrap_or_default()
}

fn parse_nft_page_owners_inner(tx: &[u8]) -> Option<Vec<[u8; 20]>> {
    let mut pos = 0usize;
    let mut tt_matched = false;
    let mut owners: Vec<[u8; 20]> = Vec::new();
    while pos < tx.len() {
        let (type_code, field_code, header_len) = read_field_header(tx, pos)?;
        pos += header_len;
        if type_code == 1 && field_code == 2 {
            let bytes = tx.get(pos..pos + 2)?;
            if !is_nft_tx_type(u16::from_be_bytes([bytes[0], bytes[1]])) {
                return None;
            }
            tt_matched = true;
            pos += 2;
            continue;
        }
        if type_code == 8 {
            // AccountID fields are VL-encoded; only the canonical 20-byte
            // form denotes an account.
            let (len, consumed) = read_vl(tx, pos)?;
            if len == 20 {
                let bytes = tx.get(pos + consumed..pos + consumed + 20)?;
                let mut id = [0u8; 20];
                id.copy_from_slice(bytes);
                if id != [0u8; 20] && !owners.contains(&id) {
                    owners.push(id);
                }
            }
            pos += consumed + len;
            continue;
        }
        pos = skip_value(tx, pos, type_code)?;
    }
    if !tt_matched {
        return None;
    }
    Some(owners)
}

/// Parse an `account_objects` response into (NFTokenPage keys, marker).
///
/// `Err` on any body-level error or malformed entry — a silently dropped
/// page could make succ return a WRONG key, which is worse than failing the
/// fetch (the caller treats `Err` as retryable and stores nothing).
/// Non-NFTokenPage entries are ignored rather than rejected: the `type`
/// filter is server-side and we do not want a lenient server to hard-fail
/// the prefetch.
pub fn parse_account_objects_pages(
    body: &serde_json::Value,
) -> Result<(Vec<[u8; 32]>, Option<serde_json::Value>), String> {
    if let Some(err) = body["result"]["error"].as_str() {
        return Err(err.to_string());
    }
    let objects = body["result"]["account_objects"]
        .as_array()
        .ok_or_else(|| "no_account_objects_array".to_string())?;
    let mut keys = Vec::with_capacity(objects.len());
    for obj in objects {
        if obj["LedgerEntryType"].as_str() != Some("NFTokenPage") {
            continue;
        }
        let idx_hex = obj["index"]
            .as_str()
            .ok_or_else(|| "page_missing_index".to_string())?;
        let bytes = hex::decode(idx_hex).map_err(|_| format!("bad_index_hex: {idx_hex}"))?;
        if bytes.len() != 32 {
            return Err(format!("index_len_{}: {idx_hex}", bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        keys.push(arr);
    }
    let marker = match &body["result"]["marker"] {
        serde_json::Value::Null => None,
        m => Some(m.clone()),
    };
    Ok((keys, marker))
}

/// Walk `account_objects` pages for one owner, collecting every
/// NFTokenPage key. `fetch` maps an optional marker to a parsed response
/// (`None` = fetch failed after retries).
///
/// Returns `None` when any fetch fails — a partial set with `complete:
/// true` would be a silently wrong authoritative answer, and the caller
/// storing nothing leaves succ on its pre-existing fallback. `complete` is
/// true only when the walk ended on a marker-less page within `max_pages`.
pub fn collect_owner_pages<F>(
    mut fetch: F,
    owner: &[u8; 20],
    max_pages: usize,
) -> Option<OwnerNftPages>
where
    F: FnMut(Option<&serde_json::Value>) -> Option<(Vec<[u8; 32]>, Option<serde_json::Value>)>,
{
    let mut pages = BTreeSet::new();
    let mut marker: Option<serde_json::Value> = None;
    for _ in 0..max_pages {
        let (keys, next) = fetch(marker.as_ref())?;
        pages.extend(keys);
        match next {
            None => {
                return Some(OwnerNftPages { owner: *owner, pages, complete: true });
            }
            Some(m) => marker = Some(m),
        }
    }
    // Ran out of page budget with a marker still pending: the set is a
    // prefix, so "no candidate" must stay Unknown.
    Some(OwnerNftPages { owner: *owner, pages, complete: false })
}

// Offline tests live in crates/xrpl-node/tests/succ_walk_offline.rs — an
// integration target, because the gate runner only executes test targets
// (in-module #[cfg(test)] tests never run there). Everything the tests need
// is part of this module's public API.
