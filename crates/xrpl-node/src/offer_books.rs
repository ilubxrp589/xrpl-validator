//! Pure logic for prefetching offer books so `RpcProvider::succ` can answer
//! book-directory probes against Clio-backed endpoints.
//!
//! Why this exists: rippled's apply path finds the best quality directory of
//! an order book via `ReadView::succ(bookBase, bookEnd)`. A book base key has
//! its low 64 quality bits zeroed, so it NEVER exists as a ledger object —
//! and Clio (which serves s1/s2.ripple.com) only accepts a `ledger_data`
//! marker that is an EXISTING ledger-object key at the requested ledger
//! (otherwise it answers `markerDoesNotExist`). A cold `ledger_data` walk
//! seeded with the book base therefore cannot serve the first probe of a
//! book. Instead, the apply loop prefetches the books an OfferCreate can
//! cross via the `book_offers` RPC (pinned to the pre-ledger index): every
//! returned offer carries its `BookDirectory` key, which is exactly the set
//! of directory keys `succ` must surface for that book's quality range.
//!
//! Everything here is bytes/json in → data out, so it is unit-testable
//! offline; the HTTP side lives in `ffi_engine::RpcProvider`.

use sha2::{Digest, Sha256, Sha512};
use std::collections::BTreeSet;

/// One side of an order book: a 160-bit currency code and its issuer.
/// XRP is the all-zero currency with the all-zero issuer (rippled's
/// `xrpIssue()` convention — the zero account is part of the book keylet
/// preimage, so it must be preserved here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub currency: [u8; 20],
    pub issuer: [u8; 20],
}

impl Issue {
    pub fn xrp() -> Self {
        Self { currency: [0u8; 20], issuer: [0u8; 20] }
    }

    pub fn iou(currency: [u8; 20], issuer: [u8; 20]) -> Self {
        Self { currency, issuer }
    }

    pub fn is_xrp(&self) -> bool {
        self.currency == [0u8; 20]
    }
}

/// The two amounts of an OfferCreate that define which books it can cross.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferBooks {
    pub taker_pays: Issue,
    pub taker_gets: Issue,
}

/// Read a serialized field header. Returns (type_code, field_code,
/// bytes_consumed). Mirrors `engine::read_field_header` (kept local so this
/// module stays self-contained and offline-testable).
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

/// Extract the Issue from a serialized Amount field body.
fn parse_amount_issue(bytes: &[u8]) -> Option<Issue> {
    match bytes.len() {
        8 => Some(Issue::xrp()),
        48 => {
            let mut currency = [0u8; 20];
            currency.copy_from_slice(&bytes[8..28]);
            let mut issuer = [0u8; 20];
            issuer.copy_from_slice(&bytes[28..48]);
            Some(Issue::iou(currency, issuer))
        }
        _ => None,
    }
}

/// Scan a binary tx blob; if it is an OfferCreate, return its TakerPays /
/// TakerGets issues. Returns `None` for any other tx type or on malformed
/// input — the caller simply skips book prefetch then.
///
/// The scan relies on canonical field order (sorted by type code, then field
/// code): TransactionType (UInt16, 1/2) and the Amounts (6/4 TakerPays,
/// 6/5 TakerGets) all precede the first VL-encoded field (type 7+), so only
/// fixed-width types ever need skipping.
pub fn parse_offer_create(tx: &[u8]) -> Option<OfferBooks> {
    let mut pos = 0usize;
    let mut is_offer_create = false;
    let mut pays: Option<Issue> = None;
    let mut gets: Option<Issue> = None;
    while pos < tx.len() {
        let (type_code, field_code, header_len) = read_field_header(tx, pos)?;
        pos += header_len;
        if type_code > 6 {
            break; // amounts region is over — canonical order
        }
        let value_len = match type_code {
            1 => 2,
            2 => 4,
            3 => 8,
            4 => 16,
            5 => 32,
            6 => {
                if pos >= tx.len() {
                    return None;
                }
                if tx[pos] & 0x80 != 0 {
                    48
                } else {
                    8
                }
            }
            _ => return None,
        };
        if pos + value_len > tx.len() {
            return None;
        }
        match (type_code, field_code) {
            (1, 2) => {
                let tt = u16::from_be_bytes([tx[pos], tx[pos + 1]]);
                if tt != 7 {
                    return None; // not OfferCreate
                }
                is_offer_create = true;
            }
            (6, 4) => pays = parse_amount_issue(&tx[pos..pos + value_len]),
            (6, 5) => gets = parse_amount_issue(&tx[pos..pos + value_len]),
            _ => {}
        }
        pos += value_len;
        if pays.is_some() && gets.is_some() {
            break;
        }
    }
    if !is_offer_create {
        return None;
    }
    Some(OfferBooks { taker_pays: pays?, taker_gets: gets? })
}

/// rippled's `getBookBase(Book{in, out})`: sha512-half of
/// (u16 'B', in.currency, out.currency, in.account, out.account) with the
/// low 64 quality bits zeroed — the start of the book's directory range.
pub fn book_base(book_in: &Issue, book_out: &Issue) -> [u8; 32] {
    let mut h = Sha512::new();
    h.update([0x00, 0x42]); // LedgerNameSpace::BOOK_DIR ('B') as u16 BE
    h.update(book_in.currency);
    h.update(book_out.currency);
    h.update(book_in.issuer);
    h.update(book_out.issuer);
    let digest = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest[..32]);
    out[24..].fill(0);
    out
}

/// The books an OfferCreate(taker_pays, taker_gets) can cross, as (in, out)
/// pairs. An offer with (Pays=P, Gets=G) is FILED in book {in: P, out: G};
/// the offers it CROSSES give P and want G, i.e. book {in: G, out: P}.
/// Both orientations are included (the direct book is cheap insurance), plus
/// the two XRP-bridged legs when neither side is XRP (autobridging).
pub fn crossing_books(taker_pays: &Issue, taker_gets: &Issue) -> Vec<(Issue, Issue)> {
    let mut books = vec![
        (taker_gets.clone(), taker_pays.clone()), // crossed (reversed) book
        (taker_pays.clone(), taker_gets.clone()), // book the offer files into
    ];
    if !taker_pays.is_xrp() && !taker_gets.is_xrp() {
        books.push((taker_gets.clone(), Issue::xrp())); // gets → XRP leg
        books.push((Issue::xrp(), taker_pays.clone())); // XRP → pays leg
    }
    books
}

/// Encode a 20-byte account ID as a classic address (base58check, ripple
/// alphabet, version byte 0x00) — `book_offers` requires issuers in this form.
pub fn encode_account_id(id: &[u8; 20]) -> String {
    const ALPHABET: &[u8] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let mut payload = Vec::with_capacity(25);
    payload.push(0u8);
    payload.extend_from_slice(id);
    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);
    // Little-endian base-58 digits, processing payload MSB-first.
    let mut digits: Vec<u8> = Vec::new();
    for &byte in &payload {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            let v = ((*d as u32) << 8) + carry;
            *d = (v % 58) as u8;
            carry = v / 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let leading_zeros = payload.iter().take_while(|&&b| b == 0).count();
    let mut out = String::with_capacity(leading_zeros + digits.len());
    for _ in 0..leading_zeros {
        out.push(ALPHABET[0] as char);
    }
    for &d in digits.iter().rev() {
        out.push(ALPHABET[d as usize] as char);
    }
    out
}

/// JSON form of one book side for `book_offers` params. XRP has no issuer;
/// IOU currencies always use the 40-hex form (accepted for every currency,
/// and required for non-standard codes like RLUSD's `524C5553...`).
pub fn issue_to_params(issue: &Issue) -> serde_json::Value {
    if issue.is_xrp() {
        serde_json::json!({ "currency": "XRP" })
    } else {
        serde_json::json!({
            "currency": hex::encode_upper(issue.currency),
            "issuer": encode_account_id(&issue.issuer),
        })
    }
}

/// Parse a `book_offers` response into (sorted deduped BookDirectory keys,
/// raw offer count). `Err` on any body-level error or malformed offer — a
/// silently dropped directory could make succ return a WRONG key.
pub fn parse_book_offers_dirs(
    body: &serde_json::Value,
) -> Result<(Vec<[u8; 32]>, usize), String> {
    if let Some(err) = body["result"]["error"].as_str() {
        return Err(err.to_string());
    }
    let offers = body["result"]["offers"]
        .as_array()
        .ok_or_else(|| "no_offers_array".to_string())?;
    let mut dirs = Vec::with_capacity(offers.len());
    for offer in offers {
        let dir_hex = offer["BookDirectory"]
            .as_str()
            .ok_or_else(|| "offer_missing_book_directory".to_string())?;
        let bytes = hex::decode(dir_hex).map_err(|_| format!("bad_dir_hex: {dir_hex}"))?;
        if bytes.len() != 32 {
            return Err(format!("dir_len_{}: {dir_hex}", bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        dirs.push(arr);
    }
    let count = offers.len();
    dirs.sort_unstable();
    dirs.dedup();
    Ok((dirs, count))
}

/// A prefetched book: its base key plus the directory keys `book_offers`
/// surfaced. `complete` means the response provably held the whole book
/// (offer count safely below the requested page limit), so "no candidate"
/// is an authoritative "no successor" rather than a truncation artifact.
#[derive(Debug)]
pub struct BookDirs {
    pub base: [u8; 32],
    pub dirs: BTreeSet<[u8; 32]>,
    pub complete: bool,
}

/// Does `key` fall inside this book's quality range [base, base + 2^64)?
/// Directory keys share the base's 24-byte prefix; only the low 64 quality
/// bits vary.
pub fn in_book_range(base: &[u8; 32], key: &[u8; 32]) -> bool {
    base[..24] == key[..24]
}

/// Outcome of answering succ from a prefetched book.
#[derive(Debug, PartialEq, Eq)]
pub enum BookSuccAnswer {
    /// Smallest directory key in the open interval (key, last).
    Found([u8; 32]),
    /// The prefetch held the whole book and nothing qualifies.
    NoneAuthoritative,
    /// Truncated prefetch and the probe is past every known dir — a later
    /// directory may exist; the caller must fall back to a resumed walk.
    Unknown,
}

/// Answer `succ(key, last)` from a prefetched book. Because `book_offers`
/// returns offers best-quality-first, the dir set is a PREFIX of the book's
/// true directory sequence — any candidate found inside it is correct even
/// when the prefetch was truncated; only "no candidate" is ambiguous then.
pub fn succ_from_book(
    book: &BookDirs,
    key: &[u8; 32],
    last: Option<&[u8; 32]>,
) -> BookSuccAnswer {
    use std::ops::Bound;
    let upper = match last {
        Some(l) => Bound::Excluded(*l),
        None => Bound::Unbounded,
    };
    if let Some(cand) = book.dirs.range((Bound::Excluded(*key), upper)).next() {
        return BookSuccAnswer::Found(*cand);
    }
    if book.complete {
        BookSuccAnswer::NoneAuthoritative
    } else {
        BookSuccAnswer::Unknown
    }
}

// Offline tests live in crates/xrpl-node/tests/zz_offer_books_offline.rs —
// an integration target, because the gate runner only executes test targets.
