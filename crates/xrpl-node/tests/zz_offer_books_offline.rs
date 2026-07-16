//! Offline tests for `xrpl_node::offer_books` — the pure logic behind the
//! book_offers-based succ() prefetch on the standalone-RPC apply path.
//!
//! (The `zz_` prefix keeps this target LAST in cargo's alphabetical run
//! order: the remote unit runner only echoes the tail of its log, so this
//! is the one slot where per-test results stay visible to the loop agent.)
//!
//! No network, no ffi feature. The load-bearing test is
//! `crossed_book_base_matches_mainnet_deleted_dir`: it runs the whole pure
//! chain (binary tx scan → book orientation → sha512-half book keylet)
//! against a REAL mainnet vector — the BF6C928F OfferCreate from ledger
//! 103515367 and the book DirectoryNode its crossing deleted
//! (CFEC8953…A600, from the tx's mainnet AffectedNodes).

use serde_json::json;
use xrpl_node::offer_books::{
    book_base, crossing_books, encode_account_id, in_book_range, issue_to_params,
    parse_book_offers_dirs, parse_offer_create, succ_from_book, BookDirs, BookSuccAnswer, Issue,
};

/// Same ledger data file the (read-only) oracle test uses.
const LEDGER_BLOBS_TSV: &str = include_str!("data/l103515367_blobs.txt");

/// The book DirectoryNode BF6C928F's crossing deleted on mainnet — its
/// first 24 bytes are the crossed book's base prefix, its low 8 bytes the
/// quality of the consumed offer.
const CROSSED_DIR_HEX: &str = "CFEC8953D22B1B50F20F46ECBCD51990A26BDB71951ED8265A1AA92902C9A600";

/// BF6C928F's TakerPays: RLUSD (non-standard 20-byte currency code) issued
/// by rMxCKbEDwqr76QuheSUMdEGf4B9xJ8m5De.
const RLUSD_CURRENCY_HEX: &str = "524C555344000000000000000000000000000000";
const RLUSD_ISSUER_HEX: &str = "E5E961C6A025C9404AA7B662DD1DF975BE75D13E";

fn hex32(s: &str) -> [u8; 32] {
    let v = hex::decode(s).expect("hex");
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    a
}

fn hex20(s: &str) -> [u8; 20] {
    let v = hex::decode(s).expect("hex");
    let mut a = [0u8; 20];
    a.copy_from_slice(&v);
    a
}

fn blob(idx: u32) -> Vec<u8> {
    for line in LEDGER_BLOBS_TSV.lines().filter(|l| !l.is_empty()) {
        let mut parts = line.splitn(2, '\t');
        let i: u32 = parts.next().unwrap().parse().unwrap();
        if i == idx {
            return hex::decode(parts.next().unwrap()).unwrap();
        }
    }
    panic!("tx index {idx} not found in data file");
}

fn rlusd() -> Issue {
    Issue::iou(hex20(RLUSD_CURRENCY_HEX), hex20(RLUSD_ISSUER_HEX))
}

// ---- parse_offer_create ----

#[test]
fn parse_extracts_bf6c928f_book_sides() {
    let books = parse_offer_create(&blob(19)).expect("tx 19 is an OfferCreate");
    assert_eq!(books.taker_pays, rlusd(), "TakerPays must be RLUSD IOU");
    assert_eq!(books.taker_gets, Issue::xrp(), "TakerGets must be XRP");
}

#[test]
fn parse_rejects_non_offer_create() {
    // Minimal Payment header: TransactionType (0x12) = 0.
    assert!(parse_offer_create(&[0x12, 0x00, 0x00]).is_none());
    // Truncated / garbage input.
    assert!(parse_offer_create(&[]).is_none());
    assert!(parse_offer_create(&[0x12, 0x00]).is_none());
    // OfferCreate type but truncated before the amounts.
    assert!(parse_offer_create(&[0x12, 0x00, 0x07, 0x64, 0xD4]).is_none());
}

// ---- book_base / crossing_books: the mainnet ground-truth vector ----

#[test]
fn crossed_book_base_matches_mainnet_deleted_dir() {
    let crossed_dir = hex32(CROSSED_DIR_HEX);
    // BF6C928F pays RLUSD, gets XRP → the offers it crosses give RLUSD and
    // want XRP: book {in: XRP, out: RLUSD}.
    let base = book_base(&Issue::xrp(), &rlusd());
    assert_eq!(base[24..], [0u8; 8], "book base must have quality bits zeroed");
    assert_eq!(
        hex::encode_upper(&base[..24]),
        hex::encode_upper(&crossed_dir[..24]),
        "book_base({{in: XRP, out: RLUSD}}) must be the prefix of the \
         mainnet-deleted book DirectoryNode CFEC8953…"
    );
    assert!(in_book_range(&base, &crossed_dir));
}

#[test]
fn crossing_books_covers_the_crossed_book_for_bf6c928f() {
    let books = parse_offer_create(&blob(19)).expect("OfferCreate");
    let crossed_dir = hex32(CROSSED_DIR_HEX);
    let bases: Vec<[u8; 32]> = crossing_books(&books.taker_pays, &books.taker_gets)
        .iter()
        .map(|(i, o)| book_base(i, o))
        .collect();
    assert!(
        bases.iter().any(|b| in_book_range(b, &crossed_dir)),
        "prefetch list must include the crossed book; computed bases: {:?}",
        bases.iter().map(|b| hex::encode_upper(b)).collect::<Vec<_>>()
    );
    // One side is XRP → no autobridge legs, just the two orientations.
    assert_eq!(bases.len(), 2);
}

#[test]
fn crossing_books_adds_bridge_legs_for_iou_iou() {
    let a = Issue::iou([1u8; 20], [2u8; 20]);
    let b = Issue::iou([3u8; 20], [4u8; 20]);
    let books = crossing_books(&a, &b);
    assert_eq!(books.len(), 4);
    // Bridge legs: {in: gets, out: XRP} and {in: XRP, out: pays}.
    assert_eq!(books[2], (b.clone(), Issue::xrp()));
    assert_eq!(books[3], (Issue::xrp(), a.clone()));
}

// ---- encode_account_id ----

#[test]
fn encodes_genesis_account_id() {
    // Well-known vector: the XRPL genesis account.
    let id = hex20("B5F762798A53D543A014CAF8B297CFF8F2F937E8");
    assert_eq!(encode_account_id(&id), "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh");
}

#[test]
fn encode_roundtrips_with_engine_decode() {
    for id in [hex20(RLUSD_ISSUER_HEX), [0u8; 20], [0xFF; 20]] {
        let addr = encode_account_id(&id);
        assert_eq!(
            xrpl_node::engine::decode_address(&addr),
            Some(id),
            "decode(encode(id)) must roundtrip for {addr}"
        );
    }
}

// ---- issue_to_params ----

#[test]
fn issue_params_xrp_has_no_issuer() {
    assert_eq!(issue_to_params(&Issue::xrp()), json!({"currency": "XRP"}));
}

#[test]
fn issue_params_iou_uses_hex_currency_and_classic_issuer() {
    let p = issue_to_params(&rlusd());
    assert_eq!(p["currency"], json!(RLUSD_CURRENCY_HEX));
    assert_eq!(
        p["issuer"],
        json!(encode_account_id(&hex20(RLUSD_ISSUER_HEX)))
    );
}

// ---- parse_book_offers_dirs ----

fn offers_body(dirs: &[&str]) -> serde_json::Value {
    let offers: Vec<serde_json::Value> = dirs
        .iter()
        .map(|d| json!({"BookDirectory": d, "index": "00"}))
        .collect();
    json!({"result": {"offers": offers, "ledger_index": 103515366, "status": "success"}})
}

#[test]
fn parse_offers_sorts_and_dedups_dirs() {
    let d_hi = "FF".repeat(32);
    let d_lo = "AA".repeat(32);
    let (dirs, count) =
        parse_book_offers_dirs(&offers_body(&[&d_hi, &d_lo, &d_hi])).expect("parses");
    assert_eq!(count, 3, "raw offer count must be preserved for completeness checks");
    assert_eq!(dirs.len(), 2);
    assert!(dirs[0] < dirs[1]);
}

#[test]
fn parse_offers_empty_book_is_ok() {
    let (dirs, count) = parse_book_offers_dirs(&offers_body(&[])).expect("parses");
    assert!(dirs.is_empty());
    assert_eq!(count, 0);
}

#[test]
fn parse_offers_surfaces_errors() {
    let body = json!({"result": {"error": "lgrNotFound", "status": "error"}});
    assert_eq!(parse_book_offers_dirs(&body).unwrap_err(), "lgrNotFound");
    // Missing offers array.
    assert!(parse_book_offers_dirs(&json!({"result": {}})).is_err());
    // Offer without BookDirectory — must not be silently dropped.
    let body = json!({"result": {"offers": [{"index": "00"}]}});
    assert!(parse_book_offers_dirs(&body).is_err());
    // Malformed dir hex.
    let body = json!({"result": {"offers": [{"BookDirectory": "ZZ"}]}});
    assert!(parse_book_offers_dirs(&body).is_err());
}

// ---- in_book_range / succ_from_book ----

/// Key with a given 24-byte prefix filler and quality byte pattern.
fn dir_key(prefix: u8, quality: u8) -> [u8; 32] {
    let mut k = [prefix; 32];
    k[24..].fill(quality);
    k
}

fn book(prefix: u8, qualities: &[u8], complete: bool) -> BookDirs {
    BookDirs {
        base: dir_key(prefix, 0),
        dirs: qualities.iter().map(|q| dir_key(prefix, *q)).collect(),
        complete,
    }
}

#[test]
fn in_book_range_is_prefix_match() {
    let base = dir_key(0xCF, 0);
    assert!(in_book_range(&base, &dir_key(0xCF, 0x5A)));
    assert!(!in_book_range(&base, &dir_key(0xCE, 0x5A)));
}

#[test]
fn succ_from_book_finds_best_dir_from_base() {
    let b = book(0xCF, &[0x30, 0x50], true);
    // First probe: key = base → the best (lowest quality) dir.
    assert_eq!(
        succ_from_book(&b, &b.base, None),
        BookSuccAnswer::Found(dir_key(0xCF, 0x30))
    );
    // Next probe: from the first dir to the second.
    assert_eq!(
        succ_from_book(&b, &dir_key(0xCF, 0x30), None),
        BookSuccAnswer::Found(dir_key(0xCF, 0x50))
    );
}

#[test]
fn succ_from_book_upper_bound_is_exclusive() {
    let b = book(0xCF, &[0x30], true);
    // A dir equal to `last` must NOT be returned (open interval).
    assert_eq!(
        succ_from_book(&b, &b.base, Some(&dir_key(0xCF, 0x30))),
        BookSuccAnswer::NoneAuthoritative
    );
    // Strictly inside the bound is fine.
    assert_eq!(
        succ_from_book(&b, &b.base, Some(&dir_key(0xCF, 0x31))),
        BookSuccAnswer::Found(dir_key(0xCF, 0x30))
    );
}

#[test]
fn succ_from_book_complete_vs_truncated_exhaustion() {
    // Complete book: past the last dir means an authoritative "no successor".
    let complete = book(0xCF, &[0x30], true);
    assert_eq!(
        succ_from_book(&complete, &dir_key(0xCF, 0x30), None),
        BookSuccAnswer::NoneAuthoritative
    );
    // Truncated prefetch: a later dir may exist — caller must fall back.
    let truncated = book(0xCF, &[0x30], false);
    assert_eq!(
        succ_from_book(&truncated, &dir_key(0xCF, 0x30), None),
        BookSuccAnswer::Unknown
    );
    // But a candidate WITHIN the truncated list is still authoritative
    // (book_offers pages are a prefix of the true dir sequence).
    assert_eq!(
        succ_from_book(&truncated, &truncated.base, None),
        BookSuccAnswer::Found(dir_key(0xCF, 0x30))
    );
}

#[test]
fn succ_from_book_empty_complete_book_is_none() {
    let b = book(0xCF, &[], true);
    assert_eq!(
        succ_from_book(&b, &b.base, None),
        BookSuccAnswer::NoneAuthoritative
    );
}
