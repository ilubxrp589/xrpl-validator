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
    parse_book_offers_dirs, parse_offer_create, parse_payment_books, parse_payment_paths,
    path_conversion_books, payment_path_books, succ_from_book, BookDirs, BookSuccAnswer, Issue,
    PathStep,
};

/// Same ledger data files the (read-only) oracle tests use.
const LEDGER_BLOBS_TSV: &str = include_str!("data/l103515367_blobs.txt");
const L105091578_BLOBS_TSV: &str = include_str!("data/l105091578_blobs.txt");

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

fn blob_from(tsv: &str, idx: u32) -> Vec<u8> {
    for line in tsv.lines().filter(|l| !l.is_empty()) {
        let mut parts = line.splitn(2, '\t');
        let i: u32 = parts.next().unwrap().parse().unwrap();
        if i == idx {
            return hex::decode(parts.next().unwrap()).unwrap();
        }
    }
    panic!("tx index {idx} not found in data file");
}

fn blob(idx: u32) -> Vec<u8> {
    blob_from(LEDGER_BLOBS_TSV, idx)
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

// ---- parse_payment_books ----

/// Tx idx 4 of ledger 103515367: a self-payment converting XRP (SendMax)
/// into BEAR (Amount) — pure book crossing, no trust-line path.
const BEAR_CURRENCY_HEX: &str = "4245415200000000000000000000000000000000";
const BEAR_ISSUER_HEX: &str = "708481E67A7828B24F8345AD162CCB40FCCB53B7";

fn bear() -> Issue {
    Issue::iou(hex20(BEAR_CURRENCY_HEX), hex20(BEAR_ISSUER_HEX))
}

#[test]
fn payment_books_xrp_to_iou_conversion() {
    let books = parse_payment_books(&blob(4)).expect("tx 4 is a cross-currency Payment");
    // Source asset (SendMax) becomes taker_gets, delivered asset (Amount)
    // becomes taker_pays — the OfferCreate the conversion is equivalent to.
    assert_eq!(books.taker_gets, Issue::xrp(), "SendMax is XRP");
    assert_eq!(books.taker_pays, bear(), "Amount is BEAR IOU");
    // The consumed offers give BEAR and want XRP → book {in: XRP, out: BEAR}
    // must be in the prefetch list crossing_books derives.
    let target = book_base(&Issue::xrp(), &bear());
    let bases: Vec<[u8; 32]> = crossing_books(&books.taker_pays, &books.taker_gets)
        .iter()
        .map(|(i, o)| book_base(i, o))
        .collect();
    assert!(bases.contains(&target), "prefetch must cover book {{in: XRP, out: BEAR}}");
}

#[test]
fn payment_books_iou_to_xrp_conversion() {
    // Tx idx 20: SendMax "666" IOU, Amount XRP (partial payment w/ DeliverMin
    // — neither flag changes the books). DeliverMin (6,10) must be skipped
    // without confusing the scan.
    let books = parse_payment_books(&blob(20)).expect("tx 20 is a cross-currency Payment");
    let sixsixsix = Issue::iou(
        hex20("0000000000000000000000003636360000000000"),
        hex20("2AF38C752F7FE95F6B308589C4A2A026014A16CB"),
    );
    assert_eq!(books.taker_gets, sixsixsix, "SendMax is the 666 IOU");
    assert_eq!(books.taker_pays, Issue::xrp(), "Amount is XRP");
    // One side XRP → two book orientations, no bridge legs.
    assert_eq!(crossing_books(&books.taker_pays, &books.taker_gets).len(), 2);
}

#[test]
fn payment_books_none_without_sendmax() {
    // Tx idx 54: IOU Amount (TAZZ) but no SendMax — direct rippling; books
    // are only reachable via explicit Paths, which the scan does not cover.
    assert!(parse_payment_books(&blob(54)).is_none());
    // Tx idx 57: plain XRP→XRP payment (with a Memo after the fixed fields).
    assert!(parse_payment_books(&blob(57)).is_none());
}

#[test]
fn payment_books_none_when_same_issue() {
    // Synthetic Payment with SendMax == Amount (same currency+issuer): a
    // same-issue "conversion" crosses no book.
    let mut iou48 = vec![0xD4u8; 8]; // value bytes, high bit set → IOU form
    iou48.extend_from_slice(&[9u8; 20]); // currency
    iou48.extend_from_slice(&[7u8; 20]); // issuer
    let mut tx = vec![0x12, 0x00, 0x00]; // TransactionType = Payment
    tx.push(0x61); // Amount (6,1)
    tx.extend_from_slice(&iou48);
    tx.push(0x69); // SendMax (6,9)
    tx.extend_from_slice(&iou48);
    assert!(parse_payment_books(&tx).is_none());
    // Same for the degenerate XRP→XRP-with-SendMax shape.
    let mut tx = vec![0x12, 0x00, 0x00];
    tx.push(0x61);
    tx.extend_from_slice(&[0x40, 0, 0, 0, 0, 0, 0, 0x64]);
    tx.push(0x69);
    tx.extend_from_slice(&[0x40, 0, 0, 0, 0, 0, 0, 0x64]);
    assert!(parse_payment_books(&tx).is_none());
}

#[test]
fn payment_books_rejects_non_payment_and_malformed() {
    // OfferCreate must not parse as a payment (and vice versa — tx 4 is a
    // Payment, so parse_offer_create must reject it).
    assert!(parse_payment_books(&blob(19)).is_none());
    assert!(parse_offer_create(&blob(4)).is_none());
    assert!(parse_payment_books(&[]).is_none());
    assert!(parse_payment_books(&[0x12, 0x00]).is_none());
    // Payment type but truncated inside the Amount field.
    assert!(parse_payment_books(&[0x12, 0x00, 0x00, 0x61, 0xD4]).is_none());
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

// ---- parse_payment_paths / path_conversion_books / payment_path_books ----
//
// Real vectors: the F3 cross-currency Payments of mainnet ledger 105091578
// whose meta under-emits without per-hop book prefetch (ours=6/7 net=8).

/// USDC as issued by rHqE…: the intermediate hop of both F3 payments.
const USDC_CURRENCY_HEX: &str = "5553444300000000000000000000000000000000";
const USDC_ISSUER_HEX: &str = "ACF3278B42F2ACC71989C99661DAB9AF8C081981";

fn usdc() -> Issue {
    Issue::iou(hex20(USDC_CURRENCY_HEX), hex20(USDC_ISSUER_HEX))
}

/// The book bases a pathed payment's prefetch covers.
fn path_book_bases(tx: &[u8]) -> Vec<[u8; 32]> {
    payment_path_books(tx).iter().map(|(i, o)| book_base(i, o)).collect()
}

#[test]
fn pathed_payment_rlusd_to_xrp_via_usdc() {
    // l105091578 idx 1: SendMax RLUSD, Amount XRP, Paths [[USDC@ACF3…]].
    // The strand is RLUSD→USDC→XRP: two books, neither of which is the
    // direct RLUSD↔XRP pair the SendMax/Amount prefetch already covers.
    let tx = blob_from(L105091578_BLOBS_TSV, 1);
    let p = parse_payment_paths(&tx).expect("pathed Payment must parse");
    assert_eq!(p.amount, Issue::xrp(), "Amount is XRP");
    assert_eq!(p.send_max, Some(rlusd()), "SendMax is RLUSD");
    assert_eq!(
        p.paths,
        vec![vec![PathStep {
            account: None,
            currency: Some(hex20(USDC_CURRENCY_HEX)),
            issuer: Some(hex20(USDC_ISSUER_HEX)),
        }]],
        "one path, one currency+issuer step (USDC)"
    );
    let bases = path_book_bases(&tx);
    assert!(
        bases.contains(&book_base(&rlusd(), &usdc())),
        "hop 1 book {{in: RLUSD, out: USDC}} must be prefetched"
    );
    assert!(
        bases.contains(&book_base(&usdc(), &Issue::xrp())),
        "implied terminal book {{in: USDC, out: XRP}} must be prefetched"
    );
}

#[test]
fn pathed_payment_xrp_to_usdc_via_rlusd() {
    // l105091578 idx 2: SendMax XRP, Amount USDC, Paths [[RLUSD@E5E9…]] —
    // the mirror-image strand XRP→RLUSD→USDC.
    let tx = blob_from(L105091578_BLOBS_TSV, 2);
    let p = parse_payment_paths(&tx).expect("pathed Payment must parse");
    assert_eq!(p.amount, usdc());
    assert_eq!(p.send_max, Some(Issue::xrp()));
    let bases = path_book_bases(&tx);
    assert!(bases.contains(&book_base(&Issue::xrp(), &rlusd())));
    assert!(bases.contains(&book_base(&rlusd(), &usdc())));
}

/// Assemble a synthetic Payment blob from parts (canonical field order).
fn synth_payment(amount: &[u8], send_max: Option<&[u8]>, tail: &[u8]) -> Vec<u8> {
    let mut tx = vec![0x12, 0x00, 0x00]; // TransactionType = Payment
    tx.push(0x61); // Amount (6,1)
    tx.extend_from_slice(amount);
    if let Some(sm) = send_max {
        tx.push(0x69); // SendMax (6,9)
        tx.extend_from_slice(sm);
    }
    tx.extend_from_slice(tail);
    tx
}

fn iou_amount_bytes(issue: &Issue) -> Vec<u8> {
    let mut v = vec![0xD4, 0x83, 0x8D, 0x7E, 0xA4, 0xC6, 0x80, 0x00];
    v.extend_from_slice(&issue.currency);
    v.extend_from_slice(&issue.issuer);
    v
}

const XRP_AMOUNT_BYTES: [u8; 8] = [0x40, 0, 0, 0, 0, 0, 0, 0x64];

#[test]
fn path_scan_crosses_vl_fields_memos_and_long_vl() {
    // Account + Destination (VL AccountIDs), then a Memos STArray whose
    // MemoData is 300 bytes (two-byte VL form), THEN the PathSet — the
    // scan must skip all of it without desyncing.
    let mut tail = Vec::new();
    tail.extend_from_slice(&[0x81, 0x14]); // Account (8,1), VL 20
    tail.extend_from_slice(&[0x11; 20]);
    tail.extend_from_slice(&[0x83, 0x14]); // Destination (8,3), VL 20
    tail.extend_from_slice(&[0x22; 20]);
    tail.push(0xF9); // Memos (15,9)
    tail.push(0xEA); // Memo object (14,10)
    tail.extend_from_slice(&[0x7D, 0xC1, 0x6B]); // MemoData (7,13), VL=300
    tail.extend_from_slice(&[0xAB; 300]);
    tail.push(0xE1); // ObjectEndMarker
    tail.push(0xF1); // ArrayEndMarker
    tail.extend_from_slice(&[0x01, 0x12]); // Paths (18,1)
    tail.push(0x30); // step: currency + issuer
    tail.extend_from_slice(&rlusd().currency);
    tail.extend_from_slice(&rlusd().issuer);
    tail.push(0x00); // end of PathSet
    let tx = synth_payment(
        &iou_amount_bytes(&usdc()),
        Some(&XRP_AMOUNT_BYTES),
        &tail,
    );
    let p = parse_payment_paths(&tx).expect("must reach Paths across VL/array fields");
    assert_eq!(p.paths, vec![vec![PathStep {
        account: None,
        currency: Some(rlusd().currency),
        issuer: Some(rlusd().issuer),
    }]]);
    // XRP→RLUSD→USDC: both hop books present.
    let bases = path_book_bases(&tx);
    assert!(bases.contains(&book_base(&Issue::xrp(), &rlusd())));
    assert!(bases.contains(&book_base(&rlusd(), &usdc())));
}

#[test]
fn path_set_multiple_paths_and_xrp_step() {
    // Two paths split by 0xFF: one via RLUSD, one via an explicit XRP step
    // (all-zero currency, no issuer) — books from BOTH must be covered.
    let mut tail = Vec::new();
    tail.extend_from_slice(&[0x01, 0x12]);
    tail.push(0x30);
    tail.extend_from_slice(&rlusd().currency);
    tail.extend_from_slice(&rlusd().issuer);
    tail.push(0xFF); // next path
    tail.push(0x10); // currency-only step: XRP
    tail.extend_from_slice(&[0u8; 20]);
    tail.push(0x00);
    let tx = synth_payment(
        &iou_amount_bytes(&usdc()),
        Some(&iou_amount_bytes(&bear())),
        &tail,
    );
    let p = parse_payment_paths(&tx).expect("parses");
    assert_eq!(p.paths.len(), 2);
    let bases = path_book_bases(&tx);
    // Path 1: BEAR→RLUSD→USDC.
    assert!(bases.contains(&book_base(&bear(), &rlusd())));
    assert!(bases.contains(&book_base(&rlusd(), &usdc())));
    // Path 2: BEAR→XRP→USDC (explicit XRP bridge).
    assert!(bases.contains(&book_base(&bear(), &Issue::xrp())));
    assert!(bases.contains(&book_base(&Issue::xrp(), &usdc())));
}

#[test]
fn account_step_reissues_without_a_book() {
    // XRP → RLUSD@E (book), then ripple through account F (no book, but
    // the running issue becomes RLUSD@F), then implied terminal to
    // USDC@ACF3: the terminal book must key on the REISSUED RLUSD@F.
    let e = rlusd();
    let f = Issue::iou(e.currency, [0xF0; 20]);
    let path = vec![vec![
        PathStep { account: None, currency: Some(e.currency), issuer: Some(e.issuer) },
        PathStep { account: Some(f.issuer), currency: None, issuer: None },
    ]];
    let books = path_conversion_books(&usdc(), Some(&Issue::xrp()), &path);
    assert!(books.contains(&(Issue::xrp(), e.clone())), "hop 1 book");
    assert!(
        books.contains(&(f.clone(), usdc())),
        "terminal book must use the account-step reissued RLUSD@F"
    );
    assert!(
        !books.contains(&(e.clone(), f.clone())),
        "an account step ripples — it is not a book"
    );
}

#[test]
fn issuer_only_step_is_a_book() {
    // USD@A → USD@B via an issuer-only element: same currency, different
    // issuer — that IS an order book in rippled's strand semantics.
    let a = Issue::iou([0x05; 20], [0x0A; 20]);
    let b = Issue::iou([0x05; 20], [0x0B; 20]);
    let path = vec![vec![PathStep {
        account: None,
        currency: None,
        issuer: Some(b.issuer),
    }]];
    let books = path_conversion_books(&b, Some(&a), &path);
    assert!(books.contains(&(a.clone(), b.clone())));
    // Terminal already reached (cur == amount): exactly the one conversion
    // (both orientations).
    assert_eq!(books.len(), 2);
}

#[test]
fn path_books_empty_for_pathless_and_non_payments() {
    // l103515367 idx 4: cross-currency Payment WITHOUT Paths.
    assert!(payment_path_books(&blob(4)).is_empty());
    // idx 19: OfferCreate (has no PathSet; also must not parse as Payment).
    assert!(payment_path_books(&blob(19)).is_empty());
    assert!(parse_payment_paths(&blob(19)).is_none());
    // Degenerate inputs.
    assert!(payment_path_books(&[]).is_empty());
    assert!(payment_path_books(&[0x12, 0x00]).is_empty());
}

#[test]
fn malformed_path_set_yields_no_books() {
    // PathSet header, then a currency+issuer step truncated after 10 bytes.
    let mut tail = vec![0x01, 0x12, 0x30];
    tail.extend_from_slice(&[0x77; 10]);
    let tx = synth_payment(&XRP_AMOUNT_BYTES, Some(&iou_amount_bytes(&bear())), &tail);
    assert!(parse_payment_paths(&tx).is_none());
    assert!(payment_path_books(&tx).is_empty());
    // Unknown step-type bits must abandon the scan, not guess.
    let mut tail = vec![0x01, 0x12, 0x42];
    tail.extend_from_slice(&[0x77; 40]);
    let tx = synth_payment(&XRP_AMOUNT_BYTES, Some(&iou_amount_bytes(&bear())), &tail);
    assert!(parse_payment_paths(&tx).is_none());
    // A PathSet that never terminates (no 0x00) is malformed too.
    let tail = vec![0x01, 0x12];
    let tx = synth_payment(&XRP_AMOUNT_BYTES, Some(&iou_amount_bytes(&bear())), &tail);
    assert!(parse_payment_paths(&tx).is_none());
}
