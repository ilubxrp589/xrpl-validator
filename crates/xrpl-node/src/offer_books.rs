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
//! book. Instead, the apply loop prefetches the books a tx can cross —
//! OfferCreates and cross-currency Payments (SendMax issue ≠ Amount issue,
//! plus every conversion hop implied by an explicit Paths field — see
//! `payment_path_books`) — via the `book_offers` RPC (pinned to the
//! pre-ledger index): every returned offer carries its `BookDirectory`
//! key, which is exactly the set of directory keys `succ` must surface for
//! that book's quality range.
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

/// Scan a binary tx blob for TransactionType == `want_tt` and the issues of
/// the two Amount fields `field_a` / `field_b`. Returns `None` for any other
/// tx type, on malformed input, or when either amount is absent — the caller
/// simply skips book prefetch then.
///
/// The scan relies on canonical field order (sorted by type code, then field
/// code): TransactionType (UInt16, 1/2) and every Amount (type 6) precede the
/// first VL-encoded field (type 7+), so only fixed-width types ever need
/// skipping.
fn scan_two_amounts(
    tx: &[u8],
    want_tt: u16,
    field_a: u8,
    field_b: u8,
) -> Option<(Issue, Issue)> {
    let mut pos = 0usize;
    let mut tt_matched = false;
    let mut a: Option<Issue> = None;
    let mut b: Option<Issue> = None;
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
                if tt != want_tt {
                    return None;
                }
                tt_matched = true;
            }
            (6, f) if f == field_a => a = parse_amount_issue(&tx[pos..pos + value_len]),
            (6, f) if f == field_b => b = parse_amount_issue(&tx[pos..pos + value_len]),
            _ => {}
        }
        pos += value_len;
        if a.is_some() && b.is_some() {
            break;
        }
    }
    if !tt_matched {
        return None;
    }
    Some((a?, b?))
}

/// Scan a binary tx blob; if it is an OfferCreate, return its TakerPays /
/// TakerGets issues.
pub fn parse_offer_create(tx: &[u8]) -> Option<OfferBooks> {
    let (pays, gets) = scan_two_amounts(tx, 7, 4, 5)?;
    Some(OfferBooks { taker_pays: pays, taker_gets: gets })
}

/// Scan a binary tx blob; if it is a Payment whose source asset (SendMax,
/// 6/9) differs from its delivered asset (Amount, 6/1), return the
/// equivalent OfferCreate view: converting S into D consumes offers with
/// TakerPays == S / TakerGets == D — exactly what OfferCreate(TakerPays: D,
/// TakerGets: S) crosses, so `crossing_books` applies verbatim (both direct
/// orientations plus the XRP-bridged legs).
///
/// Returns `None` for same-issue payments and payments without SendMax:
/// those ripple directly and touch a book only through an explicit Paths
/// field, which `payment_path_books` covers separately (a skipped prefetch
/// just leaves succ() on its pre-existing fallback — never wrong, only
/// cold).
pub fn parse_payment_books(tx: &[u8]) -> Option<OfferBooks> {
    let (amount, send_max) = scan_two_amounts(tx, 0, 1, 9)?;
    if amount == send_max {
        return None;
    }
    Some(OfferBooks { taker_pays: amount, taker_gets: send_max })
}

/// One element of an STPath: any subset of {account, currency, issuer}
/// per the STPathElement type bits (0x01 / 0x10 / 0x20).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathStep {
    pub account: Option<[u8; 20]>,
    pub currency: Option<[u8; 20]>,
    pub issuer: Option<[u8; 20]>,
}

/// The Paths-relevant fields of a Payment tx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentPaths {
    pub amount: Issue,
    pub send_max: Option<Issue>,
    pub paths: Vec<Vec<PathStep>>,
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

/// Skip one serialized field VALUE of the given type, returning the
/// position just past it. `None` on truncation or an unknown type — the
/// caller must abandon the scan (never guess at field boundaries).
fn skip_value(data: &[u8], pos: usize, type_code: u8) -> Option<usize> {
    let end = match type_code {
        1 => pos + 2,
        2 => pos + 4,
        3 => pos + 8,
        4 => pos + 16,
        5 => pos + 32,
        6 => {
            let b = *data.get(pos)?;
            // 0x80 → 48-byte IOU; 0x20 (without 0x80) → 33-byte MPT
            // amount; else 8-byte XRP.
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
            return Some(pos); // ObjectEndMarker
        }
        pos = skip_value(data, pos, type_code)?;
    }
}

/// Skip STArray contents up to and including the ArrayEndMarker (0xF1).
/// Each element is an object-typed field whose contents run to 0xE1.
fn skip_array(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let (type_code, field_code, header_len) = read_field_header(data, pos)?;
        pos += header_len;
        if type_code == 15 && field_code == 1 {
            return Some(pos); // ArrayEndMarker
        }
        if type_code != 14 {
            return None;
        }
        pos = skip_object(data, pos)?;
    }
}

/// Read a 20-byte chunk at `*pos`, advancing it.
fn take20(data: &[u8], pos: &mut usize) -> Option<[u8; 20]> {
    let bytes = data.get(*pos..*pos + 20)?;
    let mut out = [0u8; 20];
    out.copy_from_slice(bytes);
    *pos += 20;
    Some(out)
}

/// Parse a serialized PathSet body: steps with STPathElement type bits
/// (0x01 account, 0x10 currency, 0x20 issuer; fields appear in that
/// order), 0xFF between paths, 0x00 terminates the set.
fn parse_path_set(data: &[u8], mut pos: usize) -> Option<Vec<Vec<PathStep>>> {
    let mut paths = Vec::new();
    let mut cur: Vec<PathStep> = Vec::new();
    loop {
        let flags = *data.get(pos)?;
        pos += 1;
        match flags {
            0x00 => {
                if !cur.is_empty() {
                    paths.push(cur);
                }
                return Some(paths);
            }
            0xFF => paths.push(std::mem::take(&mut cur)),
            _ => {
                if flags & !0x31 != 0 {
                    return None; // unknown step-type bits
                }
                let mut step = PathStep::default();
                if flags & 0x01 != 0 {
                    step.account = Some(take20(data, &mut pos)?);
                }
                if flags & 0x10 != 0 {
                    step.currency = Some(take20(data, &mut pos)?);
                }
                if flags & 0x20 != 0 {
                    step.issuer = Some(take20(data, &mut pos)?);
                }
                cur.push(step);
            }
        }
    }
}

/// Scan a binary tx blob; if it is a Payment WITH an explicit Paths field,
/// return Amount/SendMax issues plus the decoded paths. Unlike
/// `scan_two_amounts` this walker crosses the VL region (SigningPubKey,
/// TxnSignature, Account, Destination) and any STArray (Memos, Signers) to
/// reach PathSet (type 18) — canonical field order puts it after every
/// other Payment field. `None` for non-Payments, path-less payments, or
/// anything malformed (the caller simply skips path prefetch then).
pub fn parse_payment_paths(tx: &[u8]) -> Option<PaymentPaths> {
    let mut pos = 0usize;
    let mut tt_is_payment = false;
    let mut amount: Option<Issue> = None;
    let mut send_max: Option<Issue> = None;
    while pos < tx.len() {
        let (type_code, field_code, header_len) = read_field_header(tx, pos)?;
        pos += header_len;
        match (type_code, field_code) {
            (1, 2) => {
                let bytes = tx.get(pos..pos + 2)?;
                if u16::from_be_bytes([bytes[0], bytes[1]]) != 0 {
                    return None; // not a Payment
                }
                tt_is_payment = true;
                pos += 2;
            }
            (6, 1) | (6, 9) => {
                let end = skip_value(tx, pos, 6)?;
                let issue = parse_amount_issue(&tx[pos..end]);
                if field_code == 1 {
                    amount = issue;
                } else {
                    send_max = issue;
                }
                pos = end;
            }
            (18, 1) => {
                if !tt_is_payment {
                    return None;
                }
                let paths = parse_path_set(tx, pos)?;
                return Some(PaymentPaths { amount: amount?, send_max, paths });
            }
            _ => pos = skip_value(tx, pos, type_code)?,
        }
    }
    None
}

/// Push the (from, to) conversion book plus its reverse orientation
/// (cheap insurance, mirroring `crossing_books`), deduped.
fn push_conversion(out: &mut Vec<(Issue, Issue)>, from: &Issue, to: &Issue) {
    for pair in [(from.clone(), to.clone()), (to.clone(), from.clone())] {
        if !out.contains(&pair) {
            out.push(pair);
        }
    }
}

/// The (in, out) books a pathed Payment's strands can consume. For each
/// path, walk the issue sequence: source asset (SendMax, else Amount) →
/// each path step → delivered asset (Amount, the implied terminal step).
/// A step with currency/issuer bits that changes the running issue is a
/// book step: converting X into Y consumes offers with TakerPays == X /
/// TakerGets == Y, i.e. book {in: X, out: Y}. An account-only step ripples
/// (no book) but re-issues the running IOU through that account, which
/// matters for any later hop's book keylet. Issuer defaulting for
/// currency-only steps is heuristic (explicit issuer, else step account,
/// else previous issuer) — a wrong guess only prefetches a book nobody
/// probes, leaving succ() on its fallback exactly as before.
pub fn path_conversion_books(
    amount: &Issue,
    send_max: Option<&Issue>,
    paths: &[Vec<PathStep>],
) -> Vec<(Issue, Issue)> {
    let source = send_max.unwrap_or(amount).clone();
    let mut out = Vec::new();
    for path in paths {
        let mut cur = source.clone();
        for step in path {
            let next = if let Some(c) = step.currency {
                if c == [0u8; 20] {
                    Issue::xrp()
                } else {
                    let issuer = step.issuer.or(step.account).unwrap_or(cur.issuer);
                    Issue::iou(c, issuer)
                }
            } else if cur.is_xrp() {
                cur.clone()
            } else if let Some(who) = step.issuer.or(step.account) {
                Issue::iou(cur.currency, who)
            } else {
                cur.clone()
            };
            // Book step ⇔ no account bit (the spec makes account exclusive
            // with currency/issuer; issuer-only elements are books too).
            if step.account.is_none() && next != cur {
                push_conversion(&mut out, &cur, &next);
            }
            cur = next;
        }
        if cur != *amount {
            push_conversion(&mut out, &cur, amount);
        }
    }
    out
}

/// Convenience: the path-implied books of a tx blob, or empty when it is
/// not a pathed Payment (or is malformed — prefetch is best-effort).
pub fn payment_path_books(tx: &[u8]) -> Vec<(Issue, Issue)> {
    match parse_payment_paths(tx) {
        Some(p) => path_conversion_books(&p.amount, p.send_max.as_ref(), &p.paths),
        None => Vec::new(),
    }
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
