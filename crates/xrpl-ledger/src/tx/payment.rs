//! Payment transaction — XRP direct payment.
//!
//! The most fundamental transaction type. Moves XRP from one account to another.
//! Can also create new accounts if the amount meets the reserve requirement.
//!
//! # DEAD CODE WARNING
//!
//! This module is **not called** by the live validator. Production transaction
//! application is delegated to rippled's C++ engine via FFI — see
//! `crates/xrpl-ffi/src/lib.rs` and `crates/xrpl-node/src/ffi_engine.rs`.
//!
//! This code is retained as a reference implementation / learning artifact.
//! Tests in this module prove the code works in isolation; they do NOT prove
//! the validator is correct.
//!
//! If you are adding a new amendment or tx type: add it to the FFI path,
//! not here. See `ffi/ARCHITECTURE.md` for the architectural decision record.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

/// Payment transactor.
pub struct PaymentTransactor;

/// A sender's in-flight line captured before an intermediate hop (F113).
struct InflightLine {
    lk: xrpl_core::types::Hash256,
    line_pre: Option<Vec<u8>>,
    owners: [[u8; 20]; 2],
}

impl PaymentTransactor {
    /// Extract the destination account ID from the transaction fields.
    fn destination(tx: &TxFields) -> Option<[u8; 20]> {
        let dest_hex = tx.fields.get("Destination")?.as_str()?;
        let bytes = hex::decode(dest_hex).ok()?;
        if bytes.len() != 20 {
            return None;
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Some(arr)
    }

    /// Every path in `Paths` we can model, in order, as its intermediate
    /// book-hop legs — plus how many paths the transaction actually carried, so
    /// the caller can tell "no paths" from "paths we could model none of".
    ///
    /// rippled builds a STRAND PER PATH and flows them together
    /// (`Flow.cpp`/`toStrands`); we took `paths.first()` and nothing else.
    /// #106156904 341030105165 is what that cost: SendMax 7.203546 USDT for
    /// 0.095 SOL over SIX paths — `[SOL]`, `[XRP,SOL]`, `[XRPS,SOL]`,
    /// `[XAG,SOL]`, `[XRPS,XAG,SOL]`, `[XAG,XRPS,SOL]`. Path 0 is the direct
    /// USDT→SOL book and it is dry, so we returned tecPATH_DRY with one
    /// mutation while mainnet routed path 2 (USDT→XRPS→SOL) for seven.
    /// Finding 113 — the SENDER's in-flight line on an intermediate hop.
    /// Intermediate acquisitions ride through the sender: a hop credits the
    /// sender's line in the hop currency and the next hop debits it, and the
    /// net-zero line drops out of the mutation set. rippled never materialises
    /// that line (BookStep output is held by the strand), so every trace of a
    /// line that did NOT exist before the hop must go — the line itself and
    /// the two owner-directory entries its creation inserted — while a line
    /// that DID exist is put back byte for byte.
    ///
    /// #106730304 B666C9B462C6 (LIQUIDX → BOOT → BITx → FLR → XRP): erasing
    /// the traces by restoring the owners' directory ROOT and LAST PAGE
    /// images from before the hop also wiped the entry the hop had
    /// legitimately added to the FLR issuer's page 0x6ab — the maker's new
    /// FLR line 8EFF74ED — leaving the page one entry short at ledger end.
    /// Removing exactly the temporary line's entries (owner_dir_remove,
    /// rippled's dirDelete semantics) keeps everyone else's.
    fn capture_inflight_line(
        sandbox: &Sandbox,
        acct: &[u8; 20],
        leg: &crate::tx::offer::Leg,
    ) -> Option<InflightLine> {
        if leg.xrp || acct == &leg.issuer {
            return None;
        }
        let lk = keylet::ripple_state_key(acct, &leg.issuer, &leg.cur);
        Some(InflightLine { lk, line_pre: sandbox.read(&lk), owners: [*acct, leg.issuer] })
    }

    fn undo_inflight_lines(sandbox: &mut Sandbox, lines: &[InflightLine]) {
        for l in lines.iter().rev() {
            if std::env::var("DX_UNDO").is_ok() {
                eprintln!(
                    "DX_UNDO {} pre={} now={}",
                    hex::encode(&l.lk.0[..4]),
                    l.line_pre.is_some(),
                    sandbox.read(&l.lk).is_some()
                );
            }
            match &l.line_pre {
                Some(bytes) => sandbox.write(l.lk, bytes.clone()),
                None => {
                    if let Some(line) = crate::tx::offer::json_at(sandbox, &l.lk) {
                        // Finding 178 (#106735554 9BCDD090): a line the PARK
                        // created (line_adjust's trustCreate path) carries its
                        // creator's reserve bit, sits in both owner directories
                        // and charged the creator's OwnerCount; a
                        // `fund_for_trial` fiction is a bare Flags-0 object
                        // that charged nothing. Reverting the park without the
                        // count left rapido one object high on every mixed
                        // path whose book output feeds an account run.
                        let party_low = l.owners[0] < l.owners[1];
                        let reserve_bit: u64 = if party_low { 0x0001_0000 } else { 0x0002_0000 };
                        let charged = line["Flags"].as_u64().unwrap_or(0) & reserve_bit != 0;
                        for owner in &l.owners {
                            crate::ledger::directory::owner_dir_remove(sandbox, owner, &l.lk, None, false);
                        }
                        sandbox.forget(&l.lk);
                        if charged {
                            crate::tx::offer::owner_count_add(sandbox, &l.owners[0], -1);
                        }
                    }
                }
            }
        }
    }

    fn path_chains(tx: &TxFields) -> (Vec<Vec<crate::tx::offer::Leg>>, usize) {
        let Some(paths) = tx.fields.get("Paths").and_then(|p| p.as_array()) else {
            // No `Paths` at all: one empty chain, i.e. the plain direct cross.
            return (vec![Vec::new()], 0);
        };
        // The spend currency (SendMax else Amount) seeds a LEADING
        // issuer-only element — toStrand's running asset starts there.
        let spend_cur: Option<[u8; 20]> = tx
            .fields
            .get("SendMax")
            .or_else(|| tx.fields.get("Amount"))
            .and_then(|a| a.get("currency"))
            .and_then(|c| c.as_str())
            .and_then(|c| {
                let mut c20 = [0u8; 20];
                if c.len() == 40 {
                    c20.copy_from_slice(&hex::decode(c).ok()?);
                } else if c.len() == 3 && c != "XRP" {
                    c20[12..15].copy_from_slice(c.as_bytes());
                } else {
                    return None;
                }
                Some(c20)
            });
        // Finding 140: the SendMax issuer, for the leading account element
        // that names it (see `path_legs`).
        let spend_issuer: Option<[u8; 20]> = tx
            .fields
            .get("SendMax")
            .or_else(|| tx.fields.get("Amount"))
            .and_then(|a| a.get("issuer"))
            .and_then(|c| c.as_str())
            .and_then(crate::tx::offer::decode20);
        let chains = paths
            .iter()
            .filter_map(|p| p.as_array())
            .filter_map(|els| Self::path_legs(els, spend_cur, spend_issuer))
            .collect();
        (chains, paths.len())
    }

    /// Intermediate book-hop legs for ONE path. `Some(vec![])` means no usable
    /// hops; `None` means the path uses account elements (rippling through a
    /// third party) we can't model, and that path is dropped.
    fn path_legs(
        els: &[serde_json::Value],
        spend_cur: Option<[u8; 20]>,
        spend_issuer: Option<[u8; 20]>,
    ) -> Option<Vec<crate::tx::offer::Leg>> {
        let mut legs: Vec<crate::tx::offer::Leg> = Vec::new();
        for el in els {
            let t = el.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
            if t & 0x01 != 0 {
                // An account hop through the ISSUER of the preceding hop's
                // currency is a no-op re-anchor (the value already lives on
                // that issuer's books) — skip it. True rippling through a
                // third party is not modeled.
                let acct = el
                    .get("account")
                    .and_then(|v| v.as_str())
                    .and_then(crate::tx::offer::decode20);
                match (acct, legs.last()) {
                    (Some(a), Some(prev)) if !prev.xrp && a == prev.issuer => continue,
                    // Finding 140 (#106739814 741DD630E126): a LEADING account
                    // element naming the SendMax issuer is the hop toStrand's
                    // normalization inserts on every strand (src → SendMax
                    // issuer, PaySteps.cpp) — `[rMxCK, XRP]` and `[XRP]` build
                    // the SAME strand and `toStrands` keeps one (`hasStrand`).
                    // Modelled as the plain chain here, the chain dedup below
                    // merges it; we used to drop the path from this pipeline
                    // and let the mixed pipeline flow it as a SECOND strand —
                    // two strands where rippled has one, `multiPath` on, and
                    // the RLUSD/XRP pool sized by a Fibonacci slice instead of
                    // the single-path anchored offer: 1981089 drops delivered
                    // for 1981581.
                    (Some(a), None) if spend_issuer == Some(a) => continue,
                    _ => return None,
                }
            }
            let c20 = match el.get("currency").and_then(|v| v.as_str()) {
                Some("XRP") => {
                    legs.push(crate::tx::offer::Leg {
                        xrp: true,
                        cur: [0u8; 20],
                        issuer: [0u8; 20],
                    });
                    continue;
                }
                Some(cur) => {
                    let mut c20 = [0u8; 20];
                    if cur.len() == 40 {
                        let b = hex::decode(cur).ok()?;
                        c20.copy_from_slice(&b);
                    } else if cur.len() == 3 {
                        c20[12..15].copy_from_slice(cur.as_bytes());
                    } else {
                        return None;
                    }
                    c20
                }
                // ISSUER-ONLY element (type 0x20 without 0x10): the running
                // currency carries over and only the issuer changes; toStrand's
                // pairwise emission then builds the SAME-CURRENCY cross-issuer
                // book. #106425462 CF3FFF81: `{issuer: rhub8, type: 32}`
                // between the rvYA and rhub8 account hops makes a
                // USD.rvYA→USD.rhub8 book step that mainnet fills — dropping
                // the path here refused with tecPATH_DRY. An issuer change
                // with no preceding IOU leg stays unmodeled (drop).
                None => match legs.last() {
                    Some(prev) if !prev.xrp => prev.cur,
                    Some(_) => return None,
                    // LEADING issuer-only element: the running currency is
                    // the SPEND currency — toStrand's normalization walks
                    // src → SendMax issuer first, and the pairwise emission
                    // then builds the same-currency cross-issuer book from
                    // the spend issue. #106455036 9D7FB3B1: USDC.rGm7 →
                    // USDC.rcEGRE through an AMM whose pool pairs the two
                    // issuers, found only by the full-ledger replay.
                    None => spend_cur?,
                },
            };
            let iss = el
                .get("issuer")
                .and_then(|v| v.as_str())
                .and_then(crate::tx::offer::decode20)?;
            legs.push(crate::tx::offer::Leg { xrp: false, cur: c20, issuer: iss });
        }
        Some(legs)
    }

    /// True when a chain's LAST transition is a rippling step BETWEEN TWO
    /// GATEWAYS rather than an order book, which makes the path unmodellable.
    ///
    /// `toStrand` appends a terminal book for the delivered asset only when the
    /// CURRENCY changes (libxrpl/tx/paths/PaySteps.cpp:289-300); its own comment
    /// "for offer crossing (only) we do use an offer book even if all that is
    /// changing is the Issue.account". A PAYMENT whose last hop already holds
    /// the delivered currency under a DIFFERENT issuer therefore gets no book
    /// at all: the delivery issuer enters `normPath` as an ACCOUNT element, and
    /// the offer->account transition emits
    /// `DirectStepI(hopIssuer -> deliverIssuer)` (PaySteps.cpp:477), which
    /// requires a trust line between those two gateways. Missing it, toStrand
    /// returns terNO_LINE, the path is dropped, and Payment maps the leftover
    /// ter to tecPATH_DRY.
    ///
    /// #106336831 619718E8 is the specimen: SendMax 50000 drops for
    /// 0.0511423423316901 USD.rhub8 over the one path `[USD/rvYAfWj]`. The two
    /// issuers hold no mutual line, so rippled logs "DirectStepI: No credit
    /// line" and refuses. We crossed a USD.rvYAfWj/USD.rhub8 BOOK instead and
    /// delivered — moving value mainnet never moved. The sender's newly funded
    /// USD line then let the NEXT transaction (1858CEDF, sequence +1) spend a
    /// balance it never had, so one rule accounts for both divergences.
    ///
    /// Only the TERMINAL transition is account-mediated. Between two explicit
    /// path elements rippled builds `make_BookStepII` even when just the issuer
    /// changes (both are offer elements), so intermediate same-currency hops
    /// stay books and are left alone.
    ///
    /// The rippling step itself is deliberately not modelled: with a mutual
    /// line the value crosses 1:1 under the gateway's transfer rate and the
    /// receiving line's limit, never at a book price. Dropping the path refuses
    /// rather than inventing liquidity. A specimen where mainnet DELIVERS
    /// across an inter-gateway line is what would justify building the step.
    fn terminal_is_ripple_step(chain: &[&crate::tx::offer::Leg]) -> bool {
        let n = chain.len();
        if n < 2 {
            return false;
        }
        let (a, b) = (chain[n - 2], chain[n - 1]);
        !a.xrp && !b.xrp && a.cur == b.cur && a.issuer != b.issuer
    }

    /// The account's SIGNED balance on `leg`, from its own perspective
    /// (negative = it owes the issuer). `None` when there is nothing to read a
    /// delta from: the account issues the currency itself, or holds no line.
    fn leg_signed_balance(
        sandbox: &Sandbox,
        id: &[u8; 20],
        leg: &crate::tx::offer::Leg,
    ) -> Option<(bool, crate::tx::offer::Me)> {
        use crate::tx::offer as ox;
        if leg.xrp {
            let a = ox::json_at(sandbox, &keylet::account_root_key(id))?;
            let bal: u128 = a["Balance"].as_str().and_then(|s| s.parse().ok())?;
            return Some((false, (bal, 0)));
        }
        if id == &leg.issuer {
            return None;
        }
        let line = ox::json_at(sandbox, &keylet::ripple_state_key(id, &leg.issuer, &leg.cur))?;
        let (neg, bal) = ox::signed_value(&line["Balance"]);
        // Balance is written from the LOW account's perspective.
        let party_low = id < &leg.issuer;
        let party_holds = if party_low { !neg } else { neg };
        Some((!party_holds, bal))
    }

    /// Give `account` a large balance on `leg` so a REVERSE-pass trial can ask
    /// "how much of this would the next hop need?" — mid-chain the account
    /// holds none of the intermediate currency yet, and the reverse pass is
    /// hypothetical by nature. Only ever called inside a snapshot that is
    /// rolled back. Returns what was granted.
    fn fund_for_trial(
        sandbox: &mut Sandbox,
        account: &[u8; 20],
        leg: &crate::tx::offer::Leg,
        grant: crate::tx::offer::Me,
    ) -> crate::tx::offer::Me {
        use crate::tx::offer as ox;
        // The LIMITS must stay enormous however small the grant is: they bound
        // what the trial may acquire, and sizing them to the grant would cap the
        // very measurement we are taking.
        const LIMIT: &str = "9999999999999999000000000000000000000000";
        // XRP hops need funding too — a path may ripple THROUGH XRP
        // (A -> XRP -> B), and leaving those unfunded made the trial consume
        // nothing, the requirement compute to zero, and the whole strand die.
        // That is what regressed #105091578's three payments to tecPATH_PARTIAL
        // fee-only against mainnet's 8 mutations.
        if leg.xrp {
            let key = keylet::account_root_key(account);
            let Some(mut root) = ox::json_at(sandbox, &key) else {
                return (0, 0);
            };
            // Drops are integral, so an XRP grant is exact at any scale; round
            // UP so a sub-drop grant still funds something rather than zero.
            let drops = ox::me_rescale(grant, 0, true);
            root["Balance"] = serde_json::json!(drops.to_string());
            ox::put_json(sandbox, key, &root);
            return (drops, 0);
        }
        let big = ox::me_to_value_string(grant);
        let BIG: &str = &big;
        let key = keylet::ripple_state_key(account, &leg.issuer, &leg.cur);
        let low = account < &leg.issuer;
        let cur = hex::encode_upper(leg.cur);
        let value = if low { BIG.to_string() } else { format!("-{BIG}") };
        let balance = serde_json::json!({
            "currency": cur, "issuer": "0000000000000000000000000000000000000000", "value": value,
        });
        match ox::json_at(sandbox, &key) {
            // Keep an existing line's settings; only the balance is synthetic.
            Some(mut line) => {
                line["Balance"] = balance;
                ox::put_json(sandbox, key, &line);
            }
            None => {
                let (lo, hi) = if low { (*account, leg.issuer) } else { (leg.issuer, *account) };
                let line = serde_json::json!({
                    "LedgerEntryType": "RippleState",
                    "Flags": 0u64,
                    "Balance": balance,
                    "LowLimit": {"currency": cur, "issuer": hex::encode(lo), "value": LIMIT},
                    "HighLimit": {"currency": cur, "issuer": hex::encode(hi), "value": LIMIT},
                });
                sandbox.write(key, serde_json::to_vec(&line).expect("serializing valid JSON Value"));
            }
        }
        grant
    }

    /// A strand's `qualityUpperBound`: the composition of each hop's best
    /// available quality, in-per-out. Optimistic and INDEPENDENT OF SIZE, which
    /// is exactly why rippled ranks on it — a realised fill moves as a strand's
    /// own pools drain and its fib slice grows, so ranking on that re-orders
    /// candidates for a reason that has nothing to do with which is better.
    ///
    /// `None` when any hop has neither book nor pool: a chain is only as live
    /// as its deadest link.
    fn strand_upper_bound(
        sandbox: &Sandbox,
        taker: &[u8; 20],
        chain: &[&crate::tx::offer::Leg],
        fib: &crate::tx::offer::AmmFib,
        multi: bool,
    ) -> Option<crate::tx::offer::Me> {
        use crate::tx::offer as ox;
        const ONE: ox::Me = (1_000_000_000_000_000, -15);
        let mut acc: ox::Me = ONE;
        for w in chain.windows(2) {
            let tip = ox::hop_tip(sandbox, taker, w[0], w[1], fib.iters, multi, Some(&fib.init))?;
            acc = ox::me_muldiv(acc, tip, ONE, false);
            // THE INTERMEDIATE GATEWAY'S CUT IS PART OF THE BOUND. A payment
            // book step composes its quality with `trIn` — the transfer rate of
            // the book's IN currency when the previous step redeems —
            // `adjustQualityWithFees` (BookStep.cpp:338-360), and that feeds
            // `qualityUpperBound`. Offer crossing deliberately WAIVES it ("assume
            // no fee is charged, or the estimate will no longer be an upper
            // bound", BookStep.cpp:519-524); a payment does not.
            //
            // The walk already charges exactly this (the `hop_rate` below), so
            // omitting it here made the RANKING disagree with the fills — a
            // strand was ordered by a quality it could never realise.
            //
            // #106341834 F8F02C7C: a six-path circular tfLimitQuality payment,
            // 631.89 XLM for 99.477 USD. XAG's issuer charges 1.001, so strand 5
            // (XLM>XAG>XRPS>USD) is really 6.333349113135157 and we bounded it at
            // 6.327022091044109 — exactly 1.001 too good, which put it AHEAD of
            // strand 0 at 6.329113924050633. rippled ranks strand 0 first and it
            // alone covers the whole delivery in ONE iteration; we flowed strand
            // 5 first and then strand 0, touching 8 objects mainnet never did
            // (16 muts against 6) — the XAG and XRPS lines and an XRPS/USD maker.
            //
            // ⚠ It also decides ADMISSION, not just order: rippled drops a strand
            // whose bound misses `limitQuality`, and a bound that is too good
            // admits strands the transaction's own price forbids.
            //
            // ⚠ INCLUDING HOP 0: the FIRST book step's prev is the sender's
            // DirectI, which REDEEMS the spend IOU, so its trIn composes into
            // the bound too. #106455274 13296833 (24.29 USDT → 5.92M PEPE,
            // six strands): every ub ran exactly ×1.002 (the USDT gateway)
            // too good — the ORDER survived (uniform factor) but strands 1
            // and 5 slipped past limitQuality 4.10227e-6 where rippled
            // refuses them (4.1101/4.1049e-6), turning rippled's single-path
            // anchored fill (activeStrands 1, multiPath false, ONE iteration)
            // into our three fib-sliced rounds with different intermediate
            // fills. The walk still charges the rate itself — this is only
            // the estimate.
            if !w[0].xrp && taker != &w[0].issuer {
                let tr = Self::transfer_rate(sandbox, w[0]);
                if std::env::var("DX_UB").is_ok() {
                    eprintln!(
                        "DX_UB trin in={} iss={} r={tr:?}",
                        hex::encode(&w[0].cur[..4]),
                        hex::encode(&w[0].issuer[..4]),
                    );
                }
                if let Some(r) = tr {
                    acc = ox::me_muldiv(acc, (r as u128, 0), (1_000_000_000, 0), false);
                }
            } else if std::env::var("DX_UB").is_ok() {
                eprintln!(
                    "DX_UB trin SKIP in={} xrp={} self_issue={}",
                    hex::encode(&w[0].cur[..4]),
                    w[0].xrp,
                    taker == &w[0].issuer,
                );
            }
        }
        // The strand's FINAL DirectI — the issuer delivering the dst IOU —
        // ISSUES, and an issuing direct step's quality is srcQOut =
        // transferRate(src) (DirectStepI::qualityUpperBound via
        // qualitiesSrcIssues; DirectStep.cpp:804-819). #106455274 13296833:
        // every strand ran a uniform ×1.001 too good after the hop-0 trIn
        // fix — PEPE's issuer rate on the delivery hop, NOT any book trOut
        // (all book tips there pick the AMM and waive). #106455107's XRP
        // destination composes nothing, which is what broke every
        // book-trOut reading of the same numbers. The issuer-as-dst waiver
        // uses the taker as the dst proxy (circular paths).
        if let Some(dst_leg) = chain.last() {
            if !dst_leg.xrp && taker != &dst_leg.issuer {
                if let Some(r) = Self::transfer_rate(sandbox, dst_leg) {
                    acc = ox::me_muldiv(acc, (r as u128, 0), (1_000_000_000, 0), false);
                }
            }
        }
        Some(acc)
    }

    /// The strand's composed average-quality function — rippled's
    /// `QualityFunction` (QualityFunction.cpp): q(out) = b − m·out, with `m`
    /// kept here as a POSITIVE magnitude (rippled stores it negative).
    /// CLOB-like steps are constant (b = 1/rate); a single-path AMM tip
    /// contributes the pool curve from its CURRENT frozen-aware balances and
    /// the taker's effective fee (the AMMTag ctor: m = cfee/pool.in,
    /// b = pool.out·cfee/pool.in). `fold` is rippled's combine —
    /// `m += b·m'; b *= b'` — one half-even Number op each. The gateway trIn
    /// consts and the tail issuer rate sit exactly where the steps put them
    /// (BookPaymentStep::adjustQualityWithFees composes trIn when the prev
    /// step redeems; the last DirectStep's srcQOut is the want_rate). The
    /// AMM-vs-CLOB tip pick mirrors TRYAMM: fee-adjusted spot at-or-better
    /// than the tip, or the fixAMMv1_1 branch where the LIMIT beats the tip
    /// (then the pool emits and the tip never executes). None = a hop with
    /// no liquidity at all — no trim; the pass will answer dry itself.
    fn strand_quality_fn(
        sandbox: &Sandbox,
        taker: &[u8; 20],
        chain: &[&crate::tx::offer::Leg],
        want_rate: Option<u64>,
        thr: crate::tx::offer::Me,
    ) -> Option<(crate::tx::offer::Me, crate::tx::offer::Me)> {
        use crate::tx::amm_swap as am;
        use crate::tx::offer as ox;
        const ONE: ox::Me = (1_000_000_000_000_000, -15);
        fn fold(m: &mut ox::Me, b: &mut ox::Me, qm: ox::Me, qb: ox::Me) {
            if !ox::me_is_zero(qm) {
                *m = am::n_add(*m, am::n_mul(*b, qm, am::Rnd::Near), am::Rnd::Near);
            }
            *b = am::n_mul(*b, qb, am::Rnd::Near);
        }
        let mut m: ox::Me = (0, 0);
        let mut b: ox::Me = ONE;
        for (i, w) in chain.windows(2).enumerate() {
            // Finding 119 (#106732759 34694521561A): the FIRST book hop
            // composes its in-side transfer rate too. rippled's
            // BookPaymentStep::adjustQualityWithFees takes `trIn` whenever the
            // previous step REDEEMS (BookStep.cpp) — and the sender's own
            // DirectStep into the issuer redeems the moment the sender holds
            // the issuer's IOU. The specimen sells USDT (issuer rate 1.001)
            // through two pools for UNI (issuer rate 1.001) under
            // tfLimitQuality: rippled's single-strand `limitOut` trims the
            // last iteration to 9.790866331963915 UNI — the quality function
            // carries 1/1.001 twice, USDT's trIn at hop 0 and UNI's srcQOut at
            // the closing DirectStep — while an `i > 0` gate here dropped the
            // first factor, priced the strand a hair too well (11.64), left
            // the full 10.62 remainder untrimmed, and the pass was "rejected
            // by limitQuality": 3.81 UNI delivered through the pools against
            // mainnet's 13.61.
            if !w[0].xrp && taker != &w[0].issuer {
                if let Some(r) = Self::transfer_rate(sandbox, w[0]) {
                    fold(&mut m, &mut b, (0, 0), am::n_div(ONE, (r as u128, -9), am::Rnd::Near));
                }
            }
            let (lob, amm) = ox::hop_tip_parts(sandbox, taker, w[0], w[1]);
            let mut took_amm = false;
            if let Some((a, pin, pout)) = amm {
                if !ox::me_is_zero(pin) && !ox::me_is_zero(pout) {
                    let cfee = am::n_sub(ONE, am::fee_n(a.tfee), am::Rnd::Near);
                    let use_amm = match lob {
                        None => true,
                        Some(t) => {
                            let sp = am::n_div(
                                am::n_div(pin, pout, am::Rnd::Near),
                                cfee,
                                am::Rnd::Near,
                            );
                            ox::me_cmp(sp, t).is_le() || ox::me_cmp(thr, t).is_lt()
                        }
                    };
                    if use_amm {
                        let qm = am::n_div(cfee, pin, am::Rnd::Near);
                        let qb =
                            am::n_div(am::n_mul(pout, cfee, am::Rnd::Near), pin, am::Rnd::Near);
                        if std::env::var("DX_QF").is_ok() {
                            eprintln!("DX_QF hop={i} amm pin={pin:?} pout={pout:?} tfee={} cfee={cfee:?} qm={qm:?} qb={qb:?}", a.tfee);
                        }
                        fold(&mut m, &mut b, qm, qb);
                        took_amm = true;
                    }
                }
            }
            if !took_amm {
                match lob {
                    Some(t) => fold(&mut m, &mut b, (0, 0), am::n_div(ONE, t, am::Rnd::Near)),
                    None => {
                        if std::env::var("DX_QF").is_ok() {
                            eprintln!("DX_QF hop={i} no lob, no amm -> None");
                        }
                        return None;
                    }
                }
            }
            if std::env::var("DX_QF").is_ok() {
                eprintln!("DX_QF hop={i} lob={lob:?} took_amm={took_amm} m={m:?} b={b:?}");
            }
        }
        if let Some(r) = want_rate {
            fold(&mut m, &mut b, (0, 0), am::n_div(ONE, (r as u128, -9), am::Rnd::Near));
        }
        if std::env::var("DX_QF").is_ok() {
            eprintln!("DX_QF final m={m:?} b={b:?} want_rate={want_rate:?} thr={thr:?}");
        }
        Some((m, b))
    }

    /// rippled `limitOut` (StrandFlow.h:363-420): with ONE active strand and
    /// tfLimitQuality, the iteration's ask becomes the out at which the
    /// strand's AVERAGE quality equals the limit ("reducing the output
    /// increases quality of AMM steps"). `outFromAvgQ` runs every op in
    /// Number's Upward mode — with our positive-magnitude m that is
    /// out = (b − 1/thr)/m, the inversion and the final division rounding
    /// away from zero and the subtraction toward it (Upward on rippled's
    /// negative intermediate). A constant strand (no AMM term) never trims.
    fn strand_limit_out(
        m: crate::tx::offer::Me,
        b: crate::tx::offer::Me,
        thr: crate::tx::offer::Me,
    ) -> Option<crate::tx::offer::Me> {
        use crate::tx::amm_swap as am;
        use crate::tx::offer as ox;
        const ONE: ox::Me = (1_000_000_000_000_000, -15);
        if ox::me_is_zero(m) {
            return None;
        }
        let invq = am::n_div(ONE, thr, am::Rnd::Up);
        if ox::me_cmp(b, invq).is_le() {
            if std::env::var("DX_QF").is_ok() {
                eprintln!("DX_QF limit_out: b={b:?} <= invq={invq:?} -> None");
            }
            return None;
        }
        let diff = am::n_sub(b, invq, am::Rnd::Down);
        let out = am::n_div(diff, m, am::Rnd::Up);
        if std::env::var("DX_QF").is_ok() {
            eprintln!("DX_QF limit_out: b={b:?} invq={invq:?} diff={diff:?} m={m:?} out={out:?}");
        }
        (!ox::me_is_zero(out)).then_some(out)
    }

    /// Reverse-size ONE book hop: the input `consumed` for a target `want`
    /// out, via the grant ladder + refine — extracted verbatim from
    /// `reverse_requirements` so the mixed-strand walker sizes its book
    /// segments with the SAME calibrated instrument. Zero = unmeasurable.
    #[allow(clippy::too_many_arguments)]
    fn size_book_hop(
        tx: &TxFields,
        in_leg: &crate::tx::offer::Leg,
        out_leg: &crate::tx::offer::Leg,
        want: crate::tx::offer::Me,
        threshold: u64,
        single_pass: bool,
        amm_fib: Option<&crate::tx::offer::AmmFib>,
        sandbox: &mut Sandbox,
    ) -> crate::tx::offer::Me {
        use crate::tx::offer as ox;
            let mut consumed = (0u128, 0i32);
            for grant in [(1u128, 6i32), (1, 12), (1, 18)] {
                let (c, granted, rw) = Self::measure_hop(
                    tx, in_leg, out_leg, want, grant, threshold, single_pass, amm_fib, sandbox,
                );
                if std::env::var("DX_SIZE").is_ok() {
                    eprintln!("DX_SIZE rung grant={grant:?} want={want:?} consumed={c:?} granted={granted:?} rw={rw:?}");
                }
                consumed = c;
                // Stop as soon as the GRANT was not the binding constraint —
                // either the full requirement came out, or the trial left some
                // of what it was given unspent, which means liquidity bound it
                // and `consumed` is the real answer.
                //
                // Escalating on `rw != 0` alone is wrong once the pool is
                // sized by fib slices: one slice can never answer the whole
                // requirement, so every grant "fails", the loop runs to 1e18,
                // and differencing a 1e18 balance at 16 significant digits
                // destroys the measurement. #105912291 2AE3693EF556 read back
                // a want_cap of 9.99e75 that way and fell through to the
                // unbounded cap, so hop 0 bought the whole book level (978268
                // drops) to feed a hop that only needed 15508.
                if ox::me_is_zero(rw) || ox::me_cmp(consumed, granted).is_lt() {
                    break;
                }
            }
            // The ladder's smallest rung is 1e6, which is many orders above a
            // small requirement, and a balance carries only 16 SIGNIFICANT
            // digits — so `consumed` comes back QUANTISED. #106148286
            // 4EFC975484E1, a 4-leg mXRP->BitX->BTC->CORE chain of pools: the
            // BTC hop measured (352, -9) = 3.52e-7 where the true figure is
            // ~3.5228e-7. THREE significant digits. The hop before it then
            // bought ~0.1% too little BitX and the chain delivered
            // 2.299039039859 CORE against a DeliverMin of 2.300190679602067 —
            // tecPATH_PARTIAL where mainnet does the whole 2.302493172774841 in
            // ONE flow iteration.
            //
            // So re-measure once against a grant sized to the estimate, which
            // puts the difference back inside the mantissa. Keep the refined
            // figure only if that grant was NOT the binding constraint;
            // otherwise the estimate was low and the ladder's answer stands.
            // 8x is headroom for a quantised estimate that rounded DOWN.
            if !ox::me_is_zero(consumed) {
                let grant = ox::me_muldiv(consumed, (8, 0), (1, 0), true);
                let (refined, granted, rw2) = Self::measure_hop(
                    tx, in_leg, out_leg, want, grant, threshold, single_pass, amm_fib, sandbox,
                );
                if std::env::var("DX_SIZE").is_ok() {
                    eprintln!("DX_SIZE refine grant={grant:?} want={want:?} refined={refined:?} granted={granted:?} rw={rw2:?} keep={}",
                        !ox::me_is_zero(refined) && ox::me_cmp(refined, granted).is_lt());
                }
                if !ox::me_is_zero(refined) && ox::me_cmp(refined, granted).is_lt() {
                    consumed = refined;
                }
            }
        consumed
    }

    /// One reverse-pass trial: fund `in_leg` with `grant`, ask the hop for
    /// `want`, and report `(consumed, granted, remaining_want)`. Always leaves
    /// the sandbox exactly as it found it.
    #[allow(clippy::too_many_arguments)]
    fn measure_hop(
        tx: &TxFields,
        in_leg: &crate::tx::offer::Leg,
        out_leg: &crate::tx::offer::Leg,
        want: crate::tx::offer::Me,
        grant: crate::tx::offer::Me,
        threshold: u64,
        single_pass: bool,
        amm_fib: Option<&crate::tx::offer::AmmFib>,
        sandbox: &mut Sandbox,
    ) -> (crate::tx::offer::Me, crate::tx::offer::Me, crate::tx::offer::Me) {
        use crate::tx::offer as ox;
        let snap = sandbox.snapshot();
        let granted = Self::fund_for_trial(sandbox, &tx.account, in_leg, grant);
        let before = Self::leg_signed_balance(sandbox, &tx.account, in_leg);
        let mut trial_fib = amm_fib.cloned();
        let (rw, rem_in, _) = ox::cross_engine_to(
            &tx.account, &tx.account, want, granted, out_leg, in_leg,
            threshold, threshold, false, false, single_pass, trial_fib.as_mut(), None,
            sandbox, &mut Vec::new(),
        );
        let consumed = match (before, Self::leg_signed_balance(sandbox, &tx.account, in_leg)) {
            (Some((bneg, b)), Some((aneg, a))) => {
                let (dneg, d) = ox::signed_add(bneg, b, !aneg, a); // before - after
                let d = if dneg { (0, 0) } else { d };
                // `granted - rem_in` is the SAME quantity at full precision, and
                // the walk already handed it back — this function was throwing
                // it away and differencing a granted BALANCE instead, which is
                // 16 significant digits TOTAL, so the grant itself eats the low
                // end of the answer.
                //
                // Same defect as `44c20d9`, one level up: that fixed the
                // FORWARD carry, this is the REVERSE pass that sizes it.
                // #105831615 3304A306: the trial consumed 29.12649465384142 and
                // this reported 29.1264946538414 — one digit — so `want_cap`
                // went into the forward pass already short and the hop after it
                // delivered 362674.25930709 against mainnet's 362674.259307105.
                // The reverse pass had mainnet's exact number and lost it here.
                //
                // GUARDED exactly like `44c20d9`: taken only where the two
                // AGREE to 1e-9, since they measure one quantity and should
                // differ solely in the digits the balance dropped.
                let precise = ox::me_sub(granted, rem_in);
                // ...GROSSED UP first. `rem_in` tracks only the NET the walk
                // moved; the input transfer fee is debited SEPARATELY by
                // `line_adjust`, so on a fee-bearing hop `precise` is the net
                // and `d` is the gross and the two differ by the WHOLE FEE —
                // 1e-3 for a 1.001 issuer, a thousand times the 1e-9 threshold
                // below. The guard then rejected the precise value on exactly
                // the hops that needed it, silently disabling this fix.
                //
                // #105795329 ED4F899F: the USD leg needs
                // 220.1943150207048 x 1.001 = 220.4145093357255048 and the
                // differenced balance reported 220.414509335726 — the 16th
                // digit gone, which is the 5.6e-13 that made a capped round
                // fall short of its requirement.
                let precise = match Self::transfer_rate(sandbox, in_leg)
                    .filter(|_| tx.account != in_leg.issuer)
                {
                    Some(r) => ox::mul_ratio(precise, r as u128, 1_000_000_000, true),
                    None => precise,
                };
                // A fill BELOW the granted line's 16-digit ulp is invisible
                // to the balance difference — the line never moves and `d`
                // reads ZERO for a real (tiny) fill. The walk's own
                // accounting (`granted − rem_in`) is the only measurement
                // there, and reporting zero instead tells the ladder the hop
                // has NO liquidity at all: l106267220 round 5's tip offer
                // was ground down to (2 drops, 1e-16); the trial consumed
                // it, `d` read 0, size_book_hop fell to the unbounded
                // sentinel, and the pass bought 653.86 for 2 drops —
                // tecPATH_PARTIAL where mainnet's own iteration simply
                // takes the dust and continues next round.
                let agree = !ox::me_is_zero(d)
                    && !ox::me_is_zero(precise)
                    && {
                        let (hi, lo) = if ox::me_cmp(precise, d).is_gt() { (precise, d) } else { (d, precise) };
                        let diff = ox::me_sub(hi, lo);
                        ox::me_cmp(ox::me_muldiv(diff, (1_000_000_000, 0), (1, 0), false), hi).is_lt()
                    };
                if ox::me_is_zero(d) && !ox::me_is_zero(precise) {
                    precise
                } else if agree {
                    precise
                } else {
                    d
                }
            }
            _ => (0, 0),
        };
        sandbox.restore_snapshot(snap);
        (consumed, granted, rw)
    }

    /// rippled flows a strand in TWO passes: a REVERSE pass walking back from
    /// the requested output to work out how much input each step needs, then a
    /// FORWARD pass bounded by what is actually available (StrandFlow.h
    /// `flow<>`). Ours was forward-only, so an intermediate hop sold its WHOLE
    /// carry rather than buying only what the hop after it could use.
    ///
    /// #105912291 2AE3693EF556: its RLUSD hop consumed Offer 3A3053B3 entire —
    /// all 1.03989 RLUSD — where mainnet takes 0.548387106688172 and leaves the
    /// rest resting, because that is all the DMNDBR leg needed. It is the same
    /// reason mainnet spends 999989 of the 1000000 SendMax instead of all of it.
    ///
    /// Returns `need[i]` = the output hop `i` must produce. The last entry is
    /// the caller's `want_out`; each earlier one is what the hop after it turned
    /// out to consume. A zero means that hop found no liquidity at all.
    ///
    /// `single_pass` MUST be whatever the forward pass will run with. rippled's
    /// rev and fwd passes are two halves of ONE `flow()` call, so a step's `rev`
    /// reports the input for the output that step can actually produce in THIS
    /// pass — not for the whole request. Measuring a whole-book fill and handing
    /// it to a one-level pass sizes the hop before it for output the chain will
    /// never carry; see the divergence note on the call site.
    #[allow(clippy::too_many_arguments)]
    fn reverse_requirements(
        tx: &TxFields,
        chain: &[&crate::tx::offer::Leg],
        want_out: crate::tx::offer::Me,
        threshold: u64,
        single_pass: bool,
        // The flow's AMM fib state. The reverse pass MUST see it: a pool sized
        // by `maxOffer` answers the whole requested output in one go, so the
        // hop before it is told to buy enough to feed all of that. Under
        // multiPath the pool offers ONE slice, and the requirement collapses to
        // what that slice needs. Trials clone it — a sizing probe must not
        // advance the flow-wide counter.
        amm_fib: Option<&crate::tx::offer::AmmFib>,
        sandbox: &mut Sandbox,
    ) -> Vec<crate::tx::offer::Me> {
        use crate::tx::offer as ox;
        let n = chain.len() - 1;
        let mut need = vec![(0u128, 0i32); n];
        need[n - 1] = want_out;
        for i in (1..n).rev() {
            let (in_leg, out_leg) = (chain[i], chain[i + 1]);
            // The grant must be modest. A balance is only 16 significant
            // digits, so funding 1e15 and differencing it destroys everything
            // below the integer: consuming 2.27264565429365 of it leaves
            // 999999999999997.73, which rounds to ...998, and the requirement
            // reads back as a flat 2. That is what regressed #105091578's three
            // payments — hop 0 was told to buy 2 RLUSD where 2.27 was needed,
            // delivered 1.99976 against a DeliverMin of 2.26128, and failed
            // tecPATH_PARTIAL.
            //
            // So start small and escalate only while the trial is INPUT-bound
            // (it could not produce all of need[i]). Each step is 1e6, which
            // keeps the delta within six orders of the grant and so leaves ~10
            // significant digits intact.
            // The hop before this one must deliver enough to cover this hop's
            // input transfer fee as well as its book cost — see `strand_pass`.
            let in_rate = (!in_leg.xrp && tx.account != in_leg.issuer)
                .then(|| Self::transfer_rate(sandbox, in_leg))
                .flatten();
            let consumed =
                Self::size_book_hop(tx, in_leg, out_leg, need[i], threshold, single_pass, amm_fib, sandbox);
            // A requirement we could not MEASURE is not a requirement of zero.
            // The account may issue the hop currency itself (no line to
            // difference), or the trial may have been unable to fund it. Fall
            // back to the old unbounded cap there so the hop behaves exactly as
            // it did before the reverse pass existed, rather than reporting the
            // strand dry.
            need[i - 1] = if ox::me_is_zero(consumed) {
                (9_990_000_000_000_000, 60)
            } else {
                // `consumed` is ALREADY GROSS. `measure_hop` grants a balance
                // and differences it, and the walk debits the input transfer
                // fee per fill (`offer.rs`, BookStep.cpp:770) — so the fee is
                // inside the measurement. Multiplying by the rate here charged
                // it a SECOND time and the rate compounded.
                //
                // #105795329 ED4F899F is the specimen, exact:
                //   hop 1 consumes 220.1943150207048 USD (rPFLkx, 1:1 -> RWA)
                //   walk debits    x1.001 = 220.41450933573  <- mainnet's gross
                //   this grossed   x1.001 = 220.63492384506
                //   excess 220.1943150207048 x 0.001001 = 0.22041450933
                // which is exactly the gap on the AMM's USD line, with the
                // 193872-drop conserved pair on the two AccountRoots being the
                // same excess priced back through the XRP/USD pool.
                //
                // ⚠ The fee is charged in ONE place — the walk. Everything here
                // only SIZES (`avail = carry / rate`, `spend0 / rate`). Adding
                // a debit here because a chain "arrives short" recreates this.
                let _ = in_rate;
                consumed
            };
        }
        need
    }


    /// Quality upper bound of a MIXED strand, composing each book hop's
    /// `hop_tip` (with the intermediate gateway's trIn, exactly as
    /// `strand_upper_bound` does) and each run's srcQOut/dstQIn product.
    fn mixed_upper_bound(
        sandbox: &Sandbox,
        taker: &[u8; 20],
        segs: &[crate::tx::direct_step::SegLayout],
        fib: &crate::tx::offer::AmmFib,
        multi: bool,
    ) -> Option<crate::tx::offer::Me> {
        use crate::tx::direct_step as ds;
        use crate::tx::offer as ox;
        const ONE: ox::Me = (1_000_000_000_000_000, -15);
        let mut acc: ox::Me = ONE;
        let mut first_book = true;
        for (si, seg) in segs.iter().enumerate() {
            match seg {
                ds::SegLayout::Run(hops) => {
                    let after_book = si > 0 && matches!(segs[si - 1], ds::SegLayout::Book { .. });
                    acc = ox::me_muldiv(acc, ds::run_upper_bound(sandbox, hops, after_book), ONE, false);
                }
                ds::SegLayout::Book { from, to } => {
                    let tip = ox::hop_tip(sandbox, taker, from, to, fib.iters, multi, Some(&fib.init))?;
                    acc = ox::me_muldiv(acc, tip, ONE, false);
                    if !first_book && !from.xrp && taker != &from.issuer {
                        if let Some(r) = Self::transfer_rate(sandbox, from) {
                            acc = ox::me_muldiv(acc, (r as u128, 0), (1_000_000_000, 0), false);
                        }
                    }
                    first_book = false;
                }
            }
        }
        Some(acc)
    }

    /// One PASS over a MIXED strand — direct runs composed with book hops
    /// (docs/DIRECTSTEP-DESIGN.md stage 2). Reverse sizing right-to-left
    /// (books via `size_book_hop`, runs via `run_rev`), then a forward pass
    /// that flows value left-to-right: book hops run `cross_engine_to` with
    /// the value carried IN FLIGHT through the sender exactly as the classic
    /// chains do — a run-fed book is FICTION-FUNDED first and every joint
    /// line of the sender is snapshot-restored at the end, so only the run
    /// mutations and the makers' remain. The metas pin that shape:
    /// #106311829 9684A861 has NO sender line at rhub8; the book's output
    /// enters the run at the GATEWAY.
    ///
    /// `rem_in` is GROSS when the strand HEAD is a run (fees live inside
    /// the hops), the classic net-of-spend-rate value when it is a book.
    /// `rem_out` is the NET target when the TAIL is a run, the classic
    /// grossed target when it is a book.
    #[allow(clippy::too_many_arguments)]
    fn mixed_strand_pass(
        tx: &TxFields,
        dest: &[u8; 20],
        segs: &[crate::tx::direct_step::SegLayout],
        rem_in: crate::tx::offer::Me,
        rem_out: crate::tx::offer::Me,
        single_pass: bool,
        mut amm_fib: Option<&mut crate::tx::offer::AmmFib>,
        sandbox: &mut Sandbox,
    ) -> (crate::tx::offer::Me, crate::tx::offer::Me) {
        use crate::tx::direct_step as ds;
        use crate::tx::offer as ox;
        let n = segs.len();
        if n == 0 || ox::me_is_zero(rem_in) || ox::me_is_zero(rem_out) {
            return ((0, 0), (0, 0));
        }
        // Multi-hop: the payment-wide limitQuality is judged by the caller
        // on the pass end-to-end; hops run ungated (`hop_thr` — see
        // strand_pass).
        let thr = u64::MAX;
        // ---- the sender's joint lines. Book-book joints keep the classic
        // in-flight discipline (captured now, restored at the END). A line
        // ADJACENT TO A RUN is restored IMMEDIATELY after the book hop that
        // used it, and never at the end — on a circular payment the tail
        // run's REAL delivery and the fiction ride the SAME line object,
        // and an end-restore erased the delivery with the fiction
        // (#106374244's missing destination-line mutation, 8v9).
        let capture_leg = |sandbox: &mut Sandbox, leg: &ox::Leg| -> Vec<InflightLine> {
            Self::capture_inflight_line(sandbox, &tx.account, leg).into_iter().collect()
        };
        let restore = |sandbox: &mut Sandbox, group: &[InflightLine]| {
            Self::undo_inflight_lines(sandbox, group);
        };
        let mut inflight: Vec<InflightLine> = Vec::new();
        for (i, seg) in segs.iter().enumerate() {
            let ds::SegLayout::Book { from, to } = seg else { continue };
            let fed_by_run = i > 0 && matches!(segs[i - 1], ds::SegLayout::Run(_));
            let feeds_run = i + 1 < n && matches!(segs[i + 1], ds::SegLayout::Run(_));
            if !fed_by_run {
                let mut g = capture_leg(sandbox, from);
                inflight.append(&mut g);
            }
            if !feeds_run && i + 1 < n {
                let mut g = capture_leg(sandbox, to);
                inflight.append(&mut g);
            }
        }
        // ---- REVERSE: what each segment needs at its input for the tail
        // to emit rem_out. Books via the calibrated ladder (reads only —
        // measure_hop snapshots), runs via run_rev (reads only).
        let mut out_target = vec![(0u128, 0i32); n];
        let mut plans: Vec<Option<(Vec<ox::Me>, Vec<bool>)>> = vec![None; n];
        let mut need = rem_out;
        for i in (0..n).rev() {
            out_target[i] = need;
            match &segs[i] {
                ds::SegLayout::Run(hops) => {
                    // A book before the run redeems into it (OwnerPaysFee is
                    // dormant), so hop 0 charges its issuer's rate — see
                    // `hop_qualities`. #106455079 B1017CAA.
                    let after_book = i > 0 && matches!(segs[i - 1], ds::SegLayout::Book { .. });
                    let Some((nin, plan, dirs)) = ds::run_rev(sandbox, hops, need, after_book)
                    else {
                        return ((0, 0), (0, 0));
                    };
                    plans[i] = Some((plan, dirs));
                    need = nin;
                }
                ds::SegLayout::Book { from, to } => {
                    let consumed = Self::size_book_hop(
                        tx, from, to, need, thr, single_pass, amm_fib.as_deref(), sandbox,
                    );
                    need = if ox::me_is_zero(consumed) {
                        (9_990_000_000_000_000, 60) // unmeasurable: unbounded cap
                    } else {
                        consumed
                    };
                }
            }
        }
        // ---- FORWARD from min(reverse ask, the remaining budget).
        let mut carry = if ox::me_cmp(need, rem_in).is_gt() { rem_in } else { need };
        let mut sin: ox::Me = (0, 0);
        let mut sout: ox::Me = (0, 0);
        if std::env::var("DX_MIX").is_ok() {
            eprintln!("DX_MIX fwd start carry={carry:?} need={need:?} rem_in={rem_in:?} rem_out={rem_out:?}");
        }
        for i in 0..n {
            let last = i + 1 == n;
            if std::env::var("DX_MIX").is_ok() {
                eprintln!("DX_MIX seg={i} carry={carry:?}");
            }
            match &segs[i] {
                ds::SegLayout::Run(hops) => {
                    let (plan, dirs) = plans[i].as_ref().expect("rev planned every run");
                    let after_book = i > 0 && matches!(segs[i - 1], ds::SegLayout::Book { .. });
                    let (spent, out) = ds::run_fwd(sandbox, hops, carry, plan, dirs, after_book);
                    if ox::me_is_zero(out) {
                        restore(sandbox, &inflight);
                        return ((0, 0), (0, 0));
                    }
                    if i == 0 {
                        sin = spent;
                    }
                    carry = out;
                    if last {
                        sout = out; // run-tail delivers NET into dst's line
                    }
                }
                ds::SegLayout::Book { from, to } => {
                    let benef = if last { dest } else { &tx.account };
                    let fed_by_run = i > 0 && matches!(segs[i - 1], ds::SegLayout::Run(_));
                    // Intermediate input transfer rate: the walk debits the
                    // fee per fill; sizing divides what can reach a maker
                    // (see strand_pass's hop_rate block).
                    let hop_rate = (i > 0 && !from.xrp && tx.account != from.issuer)
                        .then(|| Self::transfer_rate(sandbox, from))
                        .flatten();
                    // Finding 126 (#106735554 9BCDD090, rapido's RLUSD → USD →
                    // XRP partial payment): the FIFTH floor of the net-division
                    // family (strand_pass's hop-joint, spend0 and live-line
                    // clamp, the hop `avail` — all mulRatio-NEAREST since
                    // e4c1581/a12ffd5). rippled's `limitStepIn` nets the fed
                    // input at `mulRatio(stpAmt.in, QUALITY_ONE, trIn, false)`:
                    // 0.8379734275750757 over 1.0015 has quotient …006|98,
                    // nearest …007, and mainnet hands rPrDM69j's offer
                    // 0.8367183500500007, leaving its TakerPays at …993. This
                    // floor said …006 and the residual read …994.
                    let avail = match hop_rate {
                        Some(r) => ox::mul_ratio(carry, 1_000_000_000, r as u128, false),
                        None => carry,
                    };
                    let feeds_run =
                        i + 1 < n && matches!(segs[i + 1], ds::SegLayout::Run(_));
                    let from_group =
                        fed_by_run.then(|| capture_leg(sandbox, from)).unwrap_or_default();
                    let to_group =
                        feeds_run.then(|| capture_leg(sandbox, to)).unwrap_or_default();
                    if fed_by_run {
                        // Fiction: the run's output "rides through the
                        // sender" for the walk's funding checks; restored
                        // the moment this hop is done.
                        Self::fund_for_trial(sandbox, &tx.account, from, avail);
                    }
                    let before = (!last)
                        .then(|| Self::leg_signed_balance(sandbox, &tx.account, to))
                        .flatten();
                    let _ = crate::tx::amm_swap::take_fwd_excess();
                    crate::tx::amm_swap::set_fwd_gross_in(hop_rate.map(|_| carry));
                    crate::tx::amm_swap::set_sender_hop(i == 0);
                    crate::tx::amm_swap::set_fwd_first(!segs[..i].iter().any(|g| matches!(g, ds::SegLayout::Book { .. }))); // finding 147
                    let (rw, rs, _c) = ox::cross_engine_to(
                        &tx.account, benef, out_target[i], avail, to, from, thr, thr, false,
                        false, single_pass, amm_fib.as_deref_mut(), None, sandbox,
                        &mut Vec::new(),
                    );
                    let excess = crate::tx::amm_swap::take_fwd_excess();
                    crate::tx::amm_swap::set_fwd_gross_in(None);
                    crate::tx::amm_swap::set_sender_hop(false);
                    crate::tx::amm_swap::set_fwd_first(false);
                    if i == 0 {
                        sin = ox::me_sub(avail, rs);
                    }
                    let produced = ox::me_sub(out_target[i], rw);
                    if ox::me_is_zero(produced) {
                        restore(sandbox, &from_group);
                        restore(sandbox, &to_group);
                        restore(sandbox, &inflight);
                        return ((0, 0), (0, 0));
                    }
                    if last {
                        sout = produced;
                    } else {
                        // The carry to the next segment: the balance delta
                        // where measurable, the full-precision walk figure
                        // where they agree (strand_pass's calibration).
                        carry = match (
                            before,
                            Self::leg_signed_balance(sandbox, &tx.account, to),
                        ) {
                            (Some((bneg, b)), Some((aneg, a))) => {
                                let (dneg, d) = ox::signed_add(aneg, a, !bneg, b);
                                let d = if dneg { (0, 0) } else { d };
                                let close = {
                                    let (gneg, gap) =
                                        ox::signed_add(false, produced, true, d);
                                    let _ = gneg;
                                    ox::me_cmp(gap, ox::me_muldiv(produced, (1, 9), (1, 0), true))
                                        .is_le()
                                };
                                if close { produced } else { d }
                            }
                            _ => produced,
                        };
                        // Finding 131: the pool turn's overshoot rides the
                        // carry (see strand_pass).
                        if !ox::me_is_zero(excess) {
                            let whole = ox::signed_add(false, produced, false, excess).1;
                            if ox::me_cmp(carry, whole).is_lt() {
                                carry = whole;
                            }
                        }
                    }
                    // Run-adjacent joints come clean NOW, before the next
                    // run writes the same lines for real.
                    restore(sandbox, &from_group);
                    restore(sandbox, &to_group);
                }
            }
        }
        restore(sandbox, &inflight);
        (sin, sout)
    }

    /// Flow ONE pass of `chain` and report (spent on the first leg, delivered
    /// on the last). With `single_pass` this is rippled's per-strand `flow()`:
    /// a single quality level per book step, so the caller can re-evaluate
    /// which strand is now best (StrandFlow.h:640-756).
    ///
    /// Intermediate value is IN FLIGHT — rippled never rests it on the sender's
    /// trust lines. The lines the chain passes through, and the directory pages
    /// a mid-chain line-creation would touch, are snapshotted and restored
    /// byte-exact: creating and then forgetting a line must leave no dir
    /// droppings, and net-zero routing can otherwise leave 1-ulp dust that the
    /// no-op filter keeps.
    #[allow(clippy::too_many_arguments)]
    fn strand_pass(
        tx: &TxFields,
        dest: &[u8; 20],
        chain: &[&crate::tx::offer::Leg],
        avail_in: crate::tx::offer::Me,
        want_out: crate::tx::offer::Me,
        // (want-issuer rate, NET this round was sized for): the last hop's
        // beneficiary settlement credits the destination NET, per the fwd
        // rev-cache rule — see `benef_net` on `cross_engine_to_net`.
        want_net: Option<(u64, crate::tx::offer::Me)>,
        // The round's remaining GROSS spend cap: hop 0's fill that exhausts
        // the net avail debits it verbatim — see `gets_gross_cap`.
        spend_gross: Option<crate::tx::offer::Me>,
        threshold: u64,
        single_pass: bool,
        mut amm_fib: Option<&mut crate::tx::offer::AmmFib>,
        sandbox: &mut Sandbox,
    ) -> (crate::tx::offer::Me, crate::tx::offer::Me) {
        let _pass_guard = ox::PassGuard; // finding 98: registry cleared on every exit
        use crate::tx::offer as ox;
        let n = chain.len().saturating_sub(1);
        if n == 0 {
            return ((0, 0), (0, 0));
        }
        // tfLimitQuality's threshold is the PAYMENT's — SendMax per Amount. On a
        // one-hop chain that IS the hop's own pair, and gating it is right. On a
        // multi-hop chain it is a different unit per hop, and comparing them is
        // a category error.
        //
        // #106156904 341030105165: SendMax 7.203546 USDT for 0.095 SOL sets the
        // limit at 75.83 USDT/SOL. Mainnet routes USDT→XRPS→SOL, whose second
        // hop prices at 319630 XRPS per SOL — we gated that against 75.83 and
        // it lost by 4215x, so EVERY chain's last hop into SOL carried zero and
        // the payment read tecPATH_DRY. The pool was there; the ruler was wrong.
        //
        // rippled never gates a payment's BookStep on limitQuality. It carries
        // the limit at STRAND level and judges the pass end-to-end
        // ("Path rejected by limitQuality", StrandFlow.h:720) — which the round
        // loop in `apply_path_payment` now does.
        let hop_thr = if n > 1 { u64::MAX } else { threshold };
        let (spend_leg, want_leg) = (chain[0], chain[n]);
        let same = |a: &ox::Leg, b: &ox::Leg| a.xrp == b.xrp && a.cur == b.cur && a.issuer == b.issuer;
        let mut inflight: Vec<InflightLine> = Vec::new();
        for l in chain[1..n]
            .iter()
            .filter(|l| !l.xrp && !same(l, want_leg) && !same(l, spend_leg))
        {
            if let Some(c) = Self::capture_inflight_line(sandbox, &tx.account, l) {
                inflight.push(c);
            }
        }
        // REVERSE pass first: size each hop to what the one after it needs IN
        // THIS PASS. #106143011 5D327F343A54 is what the mode mismatch costs:
        // measuring with the whole book while applying one level per pass told
        // hop 0 (SPY->XRP) to buy 17.623187 XRP — the whole 18.00944 RLUSD
        // request — where hop 1 (XRP->RLUSD) tops out at 1.04447 RLUSD per pass
        // and so absorbs 1.021751 XRP. It bought all of it, exhausting the 100
        // SPY SendMax in ONE round and stranding ~16 XRP in the intermediate;
        // rippled spends 5.790416745276017 SPY for that same first 1.04447 and
        // comes back five more times. Both engines end up spending the whole
        // SendMax — the divergence was never overspending per se, it was
        // spending it all at once and losing the five passes that follow.
        let need =
            Self::reverse_requirements(tx, chain, want_out, hop_thr, single_pass, amm_fib.as_deref(), sandbox);
        let spend_before = Self::leg_signed_balance(sandbox, &tx.account, spend_leg);
        // What hop 0's walk says it consumed, kept for the spend measurement at
        // the bottom of this function.
        let mut spent_precise: Option<crate::tx::offer::Me> = None;
        let mut carry = avail_in;
        // rippled's implied first step (src → SendMax-issuer DirectStep) is
        // fund-limited by the LIVE line balance on every flow iteration. The
        // line re-rounds to 16 digits at each fill write while our remainder
        // accounting is exact, so after enough fills the exact budget can
        // exceed what the line actually holds by a few ulp — and a payment
        // that drains the line must land on EXACTLY zero the way mainnet's
        // does. #106455038 D0326D05 (full-ledger replay): rapido's RLUSD
        // line drains to 0 on mainnet; the exact-remainder budget overdrew
        // it to -6e-13. The clamp compares in NET terms (the walk debits
        // gross = net x rate, so the line's gross capacity divides down).
        if !spend_leg.xrp && tx.account != spend_leg.issuer {
            if let Some((neg, live)) = Self::leg_signed_balance(sandbox, &tx.account, spend_leg) {
                if neg {
                    carry = (0, 0);
                } else {
                    // mulRatio-NEAREST like every other net division — the
                    // FOURTH floor of this family (hop-joint e4c1581, spend0,
                    // now the live-line clamp). #106455142 C0442CB5: the
                    // sender's 2.500399999999944 UNI line nets over 1.001 to
                    // quotient …041|958 — rippled's DirectStepI (and our own
                    // spend0 one screen up) say …042; this floor said …041,
                    // the clamp shaved the walk's budget one ulp under, and
                    // the maker offer's residual read …959 for mainnet's
                    // …958. The D0326D05 drain protection is unchanged — the
                    // clamp still fires when the line is short; only its ulp
                    // now matches rippled's.
                    let live_net = match Self::transfer_rate(sandbox, spend_leg) {
                        Some(r) => ox::mul_ratio(live, 1_000_000_000, r as u128, false),
                        None => live,
                    };
                    if ox::me_cmp(live_net, carry).is_lt() {
                        carry = live_net;
                    }
                }
            }
        }
        for i in 0..n {
            let last = i + 1 == n;
            let benef = if last { dest } else { &tx.account };
            // Each hop buys exactly what the next one needs — no more, which is
            // what leaves a partially-filled offer behind instead of consuming
            // it whole. An intermediate hop that the reverse pass found no
            // liquidity for cannot feed the rest of the chain.
            let want_cap = need[i];
            // The carry is still measured as a balance delta rather than by
            // subtracting from the cap: me_rescale saturates, so a cap far above
            // the fill sizes loses the subtraction entirely (each fill re-pins
            // the remainder at u128::MAX and erases the ones before it).
            let out_leg = chain[i + 1];
            let before = (!last)
                .then(|| Self::leg_signed_balance(sandbox, &tx.account, out_leg))
                .flatten();
            // In a PAYMENT `ownerPaysTransferFee_` is false, so each book step
            // charges the issuer of the currency going IN:
            //   stpAmt.in = mulRatio(ofrAmt.in, trIn, QUALITY_ONE, roundUp)
            // (BookStep.cpp:770, trIn = transferRate(book_.in.account)). To hand
            // a maker `x`, the payer parts with `x * rate` and the difference is
            // destroyed. Hop 0's input is `spend_leg`, already charged once via
            // `spend_rate`, so only the INTERMEDIATES are handled here.
            //
            // ⚠ The DEBIT now belongs to the walk (`offer.rs` charges it per
            // fill, which is where rippled charges it). Only the SIZING stays
            // here: `avail = carry / rate` is what can actually reach a maker.
            // Charging it in both places compounds the rate — #105795329
            // ED4F899F is the specimen, and its arithmetic is exact:
            //   hop 1 consumes  220.1943150207048 USD (rPFLkx, 1:1 -> RWA)
            //   walk debits     x 1.001            = 220.41450933573
            //   this site then  x 1.001 again      = 220.63492384506
            //   excess          220.1943150207048 x 0.001001 = 0.22041450933
            // which is EXACTLY the gap DX_VALCHECK reported on the AMM's USD
            // line, and the 193872-drop pair on the two AccountRoots is the
            // same excess priced back through the XRP/USD pool.
            //
            // Found by applying the habit to this very comment block: it and
            // `offer.rs`'s both state the rule, and neither knew the other
            // existed. `measure_hop` differences a BALANCE, so the walk's debit
            // was already inside `consumed` before the reverse pass grossed it.
            //
            // #105924683 B3AA3ACC is the specimen: a circular
            // tfPartialPayment, XRP -> SGB -> PLX -> Teddy, where SGB's
            // issuer rctArjqVvTHi charges 0.3%. PLX and Teddy charge nothing —
            // which is exactly why their metadata pairs balance and the SGB
            // leg's does not. Uncharged, the whole chain runs 0.296% rich:
            // rippled delivers 630071.89620606 Teddy for the full 145152-drop
            // SendMax, we delivered 631939.64099203 for the same input.
            let hop_rate = (i > 0 && !chain[i].xrp && tx.account != chain[i].issuer)
                .then(|| Self::transfer_rate(sandbox, chain[i]))
                .flatten();
            let in_before = hop_rate
                .and_then(|_| Self::leg_signed_balance(sandbox, &tx.account, chain[i]));
            // Only `carry / rate` can actually reach a maker — and rippled
            // nets it at mulRatio-NEAREST, not floor (DirectStepI fwd:
            // srcToDst = mulRatio(in, QUALITY_ONE, srcQOut, false); the
            // LIMITSTEPIN receipts show the same division inside BookStep).
            // #106455110 16A9323B is the specimen: EUR carry
            // 3.767434803767796 / 1.002 has quotient …155|688 — nearest
            // …156 (rippled's iteration-0 fill), floor said …155, and the
            // one-ulp-low fill left maker offer F732F237's TakerPays
            // residual at …800 for mainnet's …801 (the maker's big EUR line
            // absorbed the twin ulp). The floor was the calibrated partner
            // of the old exact-ceil gross_in; the mulRatio + gross-primary
            // pair (a12ffd5) is the partner nearest belongs to — an
            // in-limited fill debits the gross cap verbatim, so a nearest
            // net can never overdraw the carry.
            let avail = match hop_rate {
                Some(r) => ox::mul_ratio(carry, 1_000_000_000, r as u128, false),
                None => carry,
            };
            // Finding 98: a hop's IN that is not the payment's spend (i > 0)
            // and a hop's OUT that is not the delivery (!last) are pass-through
            // — rippled never lands them on the sender's own line. Register
            // them so the walk's taker-side settlements skip the sender (the
            // makers and pools still move); see `offer::set_passthrough`.
            {
                let mut pass = Vec::new();
                if i > 0 {
                    let l = &chain[i];
                    pass.push((tx.account, ox::Leg { xrp: l.xrp, cur: l.cur, issuer: l.issuer }, ox::PassRole::In));
                }
                if !last {
                    let l = &chain[i + 1];
                    pass.push((tx.account, ox::Leg { xrp: l.xrp, cur: l.cur, issuer: l.issuer }, ox::PassRole::Out));
                }
                ox::set_passthrough(pass);
            }
            let _ = crate::tx::amm_swap::take_fwd_excess();
            crate::tx::amm_swap::set_fwd_gross_in(hop_rate.map(|_| carry));
            crate::tx::amm_swap::set_sender_hop(i == 0);
                    crate::tx::amm_swap::set_fwd_first(i == 0); // finding 147
            let (rw, rs, _c, gross_spent) = ox::cross_engine_to_net(
                &tx.account, benef, want_cap, avail, chain[i + 1], chain[i],
                hop_thr, hop_thr, false, false, single_pass, amm_fib.as_deref_mut(), None,
                if last { want_net } else { None },
                if i == 0 { spend_gross } else { None },
                sandbox, &mut Vec::new(),
            );
            let excess = crate::tx::amm_swap::take_fwd_excess();
            crate::tx::amm_swap::set_fwd_gross_in(None);
            crate::tx::amm_swap::set_sender_hop(false);
                    crate::tx::amm_swap::set_fwd_first(false);
            // Hop 0's input IS the spend leg — `hop_rate` is gated on `i > 0`,
            // so `avail` there is still `avail_in` untouched — and `rs` is the
            // part of it the pass did not spend. The difference is the same
            // quantity the balance delta at the bottom measures, at the walk's
            // own precision rather than the line's.
            if std::env::var("DX_HOP").is_ok() {
                eprintln!(
                    "DX_HOP i={i} in={}{} out={}{} avail={avail:?} rw={rw:?} rs={rs:?} rate={hop_rate:?}",
                    if chain[i].xrp { "XRP".into() } else { hex::encode_upper(&chain[i].cur[..4]) },
                    if chain[i].xrp { String::new() } else { format!("/{}", &hex::encode(chain[i].issuer)[..6]) },
                    if chain[i + 1].xrp { "XRP".into() } else { hex::encode_upper(&chain[i + 1].cur[..4]) },
                    if chain[i + 1].xrp { String::new() } else { format!("/{}", &hex::encode(chain[i + 1].issuer)[..6]) },
                );
            }
            if i == 0 {
                // On a fee-bearing spend leg the walk debits net + fee per
                // fill; `avail - rs` is only the NET, so it can never agree
                // with the gross balance delta and the chooser below fell
                // back to the delta — truncated at the LINE's ulp. rippled's
                // strand reports the sum of its per-fill stpIn (mulRatio
                // gross), which is exactly `in_gross_spent`. #106705935
                // 71D0477C: gross 6379.277459070794 read as 6379.2774590708
                // off a 256853.7283596457 line, and the next round's maker
                // credit landed one ulp low. (finding 89)
                spent_precise = Some(if spend_gross.is_some() { gross_spent } else { ox::me_sub(avail, rs) });
            }
            // rippled's fwd BookStep swallows its WHOLE input. When the want
            // is met (rw == 0) with input left over — the carry overshoot
            // from an upstream AMM's rounded-up out — the excess still goes
            // through this hop's pool: the pool receives it and pays the
            // curve's crumbs. At the LAST hop nobody downstream is credited
            // (the delivery clamps at the want; the issuer nets the
            // difference); a MID hop's crumbs join the taker's line and ride
            // the next carry. #106455266 CF7BAB85: pool 2's OAR line lands
            // +29.71809727 (the full upstream product) and its PLX line
            // pays 70.7902709126 while the dst is credited the 70.790270910…
            // Amount exactly — both VALCHECK deltas are exactly this flush.
            // IOU-only and unrated (the specimen's shape); rated or XRP legs
            // log and skip until a specimen calibrates them.
            if !ox::me_is_zero(rs) && ox::me_is_zero(rw) && hop_rate.is_none() && i > 0 {
                let tiny =
                    ox::me_cmp(ox::me_muldiv(rs, (1_000_000_000, 0), (1, 0), false), avail)
                        .is_lt();
                if tiny {
                    if !chain[i].xrp && !chain[i + 1].xrp {
                        if let Some(a) = crate::tx::amm_swap::discover(
                            sandbox, chain[i], chain[i + 1], &tx.account,
                        ) {
                            let (pin, pout) = crate::tx::amm_swap::pool_balances(
                                sandbox, &a, chain[i + 1], chain[i],
                            );
                            let fo = crate::tx::amm_swap::swap_asset_in(
                                pin, pout, rs, a.tfee, chain[i + 1].xrp,
                            );
                            if !ox::passthrough(&tx.account, chain[i], ox::PassRole::In) {
                                ox::line_adjust(sandbox, &tx.account, chain[i], rs, false);
                            }
                            ox::line_adjust(sandbox, &a.account, chain[i], rs, true);
                            ox::line_adjust(sandbox, &a.account, chain[i + 1], fo, false);
                            if !last && !ox::passthrough(&tx.account, chain[i + 1], ox::PassRole::Out) {
                                ox::line_adjust(sandbox, &tx.account, chain[i + 1], fo, true);
                            }
                            if std::env::var("DX_PAY").is_ok() {
                                eprintln!("DX_PAY hop {i} FLUSH rs={rs:?} out={fo:?} last={last}");
                            }
                        }
                    } else if std::env::var("DX_PAY").is_ok() {
                        eprintln!("DX_PAY hop {i} FLUSH-SKIP xrp-leg rs={rs:?}");
                    }
                }
            }
            // The walk has already debited the fee per fill. Nothing to add.
            //
            // Measured INERT on #105091578, #105923760 and #105795329 — the
            // hit sets are byte-identical with and without it, because an
            // intermediate rides "in flight" through the sender and usually has
            // no line to difference (`in_before` is None). It is removed on
            // PRINCIPLE, not on evidence: the walk is the single owner.
            let _ = in_before;
            carry = match (before, Self::leg_signed_balance(sandbox, &tx.account, out_leg)) {
                (Some((bneg, b)), Some((aneg, a))) => {
                    // delta = after - before; a fall in balance buys nothing.
                    let (dneg, d) = ox::signed_add(aneg, a, !bneg, b);
                    let d = if dneg { (0, 0) } else { d };
                    // `want_cap - rw` measures the SAME quantity at FULL
                    // precision. Differencing a BALANCE cannot: the sum is 16
                    // significant digits, so whatever the sender already holds
                    // of the intermediate eats the low end of the addend.
                    //
                    // #105866303 836CC353, a 3-AMM chain PHNIX -> PLX -> BXE ->
                    // BITX. Hop 0 met its requirement in full (`rw` = 0) yet
                    // the delta read four digits short:
                    //   requirement  81.8718026793484
                    //   measured     81.8718026793
                    // and the shortfall compounds down the chain, leaving the
                    // destination 5.4e-17 under an Amount it must hit exactly.
                    //
                    // GUARDED, not swapped. The delta is here because
                    // `me_rescale` saturates against the unbounded sentinel cap
                    // and the subtraction collapses. So take the precise form
                    // only where the two AGREE — they measure one quantity and
                    // should differ solely in the digits the balance dropped.
                    let from_cap = ox::me_sub(want_cap, rw);
                    let agree = !ox::me_is_zero(d)
                        && !ox::me_is_zero(from_cap)
                        && {
                            let (hi, lo) = if ox::me_cmp(from_cap, d).is_gt() {
                                (from_cap, d)
                            } else {
                                (d, from_cap)
                            };
                            let diff = ox::me_sub(hi, lo);
                            ox::me_cmp(ox::me_muldiv(diff, (1_000_000_000, 0), (1, 0), false), hi)
                                .is_lt()
                        };
                    if ox::me_is_zero(d) && !ox::me_is_zero(from_cap) {
                        // A zero delta with nonzero walk-accounting means the
                        // DELTA is blind, not the hop dry — the measure_hop G1
                        // precedent, with a new face: a SELF-FILL round-trips
                        // the sender's line (maker debit + taker credit
                        // cancel), so the delta is structurally zero however
                        // much value flowed. #106455106 E71A9888: the bot buys
                        // 0.000203 BTC from its OWN resting RLUSD/BTC offer —
                        // four writes, net zero — and reporting carry 0 dried
                        // the chain into tecPATH_DRY where mainnet fills
                        // 10879958 drops and rolls back under DeliverMin as
                        // tecPATH_PARTIAL. rippled never differences balances
                        // here: the steps hand amounts DIRECTLY down the
                        // strand, which is what `from_cap` reproduces.
                        from_cap
                    } else if agree {
                        // Same quantity, two readings. When the DELTA is the
                        // LARGER one the producer genuinely overshot — an
                        // upstream AMM's out rounds UP (fixAMMRounding) and
                        // rippled's fwd hands the WHOLE product to the next
                        // step. #106455266 CF7BAB85: pool 1 emits
                        // 29.71809727 OAR against a rev want of
                        // …26921203; pool 2 receives the full figure
                        // (its OAR line lands +29.71809727 on mainnet).
                        // The smaller-delta direction stays from_cap —
                        // #105866303's truncation calibration.
                        // Same quantity, two readings. The delta is EXACT only when
                        // the pre-balance was ZERO — nothing to eat digits — and
                        // there a LARGER delta is the producer's genuine round-up
                        // (an AMM's out, fixAMMRounding), which rippled's fwd
                        // hands WHOLE to the next step. #106455266 CF7BAB85:
                        // before=0, pool 1 emits 29.71809727 OAR against a rev
                        // want of …26921203 and pool 2 receives the full figure.
                        // With a NONZERO pre-balance the 16-digit sum rounds the
                        // tail EITHER way and a larger delta is noise, not
                        // product: #106455088 823AC88D read +3.74e-16 off a 3.3
                        // balance and the flush pushed the phantom through the
                        // pool — from_cap stands there (#105866303's calibration).
                        if ox::me_is_zero(b) && ox::me_cmp(d, from_cap).is_gt() {
                            d
                        } else {
                            from_cap
                        }
                    } else {
                        d
                    }
                }
                // The account issues the hop currency (no line to read), or this
                // is the LAST hop, whose cap is the real want_out and small
                // enough to subtract exactly.
                _ => ox::me_sub(want_cap, rw),
            };
            // Finding 131: the walk's pool turn reports its overshoot — the
            // input-clamped fill's output beyond the want — and rippled's
            // forward pass hands that whole product on. The delta above
            // sees it only when the sender's pre-balance was zero; an
            // intermediate riding through in flight has no line to
            // difference at all (#106734485: before=None, pool 1 emits
            // 60.6931897 X against a want of 60.69318969933, and pool 2
            // must receive the whole figure).
            if !last && !ox::me_is_zero(excess) {
                let whole = ox::signed_add(false, ox::me_sub(want_cap, rw), false, excess).1;
                if ox::me_cmp(carry, whole).is_lt() {
                    carry = whole;
                }
            }
            if std::env::var("DX_PAY").is_ok() {
                let nm = |l: &ox::Leg| if l.xrp { "XRP".to_string() } else { format!("{}/{}", hex::encode_upper(&l.cur[12..15]), hex::encode(&l.issuer[..4])) };
                eprintln!(
                    "DX_PAY hop {i}/{n} {} -> {} want_cap={want_cap:?} rw={rw:?} carry={carry:?} before={before:?} after={:?}",
                    nm(chain[i]), nm(chain[i + 1]),
                    Self::leg_signed_balance(sandbox, &tx.account, out_leg),
                );
            }
            if ox::me_is_zero(carry) {
                break; // hop dried: nothing delivered
            }
        }
        Self::undo_inflight_lines(sandbox, &inflight);
        // ASK THE WALK what it spent; do not DIFFERENCE the sender's balance.
        // The same defect `44c20d9` fixed for a hop's carry and `45a7092` for
        // the reverse pass sat here too: a balance holds 16 significant digits
        // TOTAL, so whatever the sender already holds of the spend currency eats
        // the low end of the addend.
        //
        // #105831615 8A754FA3, a circular tfPartialPayment spending its whole
        // 339.4046619328438 PLX SendMax into one AMM. The sender holds
        // 280487.9366802742 PLX, whose last place is 1e-10, so before − after
        // reads 339.4046619328 — three digits gone — and 4.38e-11 of the
        // SendMax looks unspent. With one round per strand nothing ever reads
        // that residue, which is why the transaction is byte-exact today; give
        // the strand a second iteration and it buys another 1.1e-11 of the
        // Amount, putting the pool's two lines and the destination's 1 lsd off
        // mainnet. rippled has no such residue: `flow()` subtracts the strand's
        // OWN reported `in` from `remainingIn`, so a SendMax-limited pass ends
        // the loop outright.
        //
        // GUARDED like both predecessors, and for a reason `1f31546` names:
        // `rs` tracks the NET the walk moved while the balance ALSO lost the
        // per-fill input transfer fee (offer.rs:3297), so on a fee-bearing spend
        // leg the two measure different things and the delta is the honest one.
        let spent = match (spend_before, Self::leg_signed_balance(sandbox, &tx.account, spend_leg)) {
            (Some((bneg, b)), Some((aneg, a))) => {
                let (dneg, d) = ox::signed_add(bneg, b, !aneg, a); // before - after
                let d = if dneg { (0, 0) } else { d };
                let agree = |p: ox::Me| {
                    !ox::me_is_zero(d) && !ox::me_is_zero(p) && {
                        let (hi, lo) = if ox::me_cmp(p, d).is_gt() { (p, d) } else { (d, p) };
                        let diff = ox::me_sub(hi, lo);
                        ox::me_cmp(ox::me_muldiv(diff, (1_000_000_000, 0), (1, 0), false), hi).is_lt()
                    }
                };
                // A spend below the line's 16-digit ulp is invisible to the
                // balance difference — d reads ZERO for a real (tiny) spend,
                // the round loop's zero-sin break then kills the flow, and
                // every later round starves. rippled never faces this: its
                // remainder falls by the STAmount spend itself (1e-16 is
                // representable). Trust the walk's own figure when the
                // balance is blind. l106267220 F328A94C round 5: the dust
                // round spends ~5e-22 for its 2 drops; with d=0 the loop
                // broke and rounds 6+ (the real 26794) never ran.
                if ox::me_is_zero(d) {
                    match spent_precise.filter(|p| !ox::me_is_zero(*p)) {
                        Some(p) => p,
                        None => d,
                    }
                } else {
                    match spent_precise.filter(|p| agree(*p)) {
                        Some(p) => p,
                        None => d,
                    }
                }
            }
            // The sender ISSUES what it is spending, so there is no line to
            // difference and no measurement at all — the round loop stops on a
            // zero spend rather than guessing. Left as it was.
            _ => (0, 0),
        };
        (spent, carry)
    }

    /// The issuer's TransferRate, QUALITY_ONE-relative (1e9 = no fee).
    /// `None` for XRP, for an issuer that charges nothing, or when the issuer
    /// account is not hydrated.
    fn transfer_rate(sandbox: &Sandbox, leg: &crate::tx::offer::Leg) -> Option<u64> {
        if leg.xrp {
            return None;
        }
        let root = keylet::account_root_key(&leg.issuer);
        let acct: serde_json::Value = serde_json::from_slice(&sandbox.read(&root)?).ok()?;
        let rate = acct.get("TransferRate")?.as_u64()?;
        (rate > 1_000_000_000).then_some(rate)
    }

    /// Extract the Amount in drops from the transaction fields.
    /// Amount can be a string (XRP drops) or an object (IOU — not handled here).
    fn amount_drops(tx: &TxFields) -> Option<u64> {
        match &tx.fields.get("Amount")? {
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None, // IOU object — not supported yet
        }
    }

    /// Read the reserve base from state, or use default.
    fn reserve_base(sandbox: &Sandbox) -> u64 {
        let fee_key = keylet::fee_settings_key();
        if let Some(data) = sandbox.read(&fee_key) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                if let Some(r) = v.get("ReserveBase").and_then(|r| r.as_u64()) {
                    return r;
                }
                // Newer format uses ReserveBaseDrops
                if let Some(r) = v
                    .get("ReserveBaseDrops")
                    .and_then(|r| r.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    return r;
                }
            }
        }
        // Default: 10 XRP (changed from 1 XRP in 2024)
        // Mainnet base reserve since the 2024 vote — the old 10 XRP default
        // turned real account-creates into phantom tecNO_DST_INSUF_XRP
        // (#105284279 7423D8FA).
        1_000_000
    }

    /// Per-owned-object reserve increment from FeeSettings.
    fn reserve_inc(sandbox: &Sandbox) -> u64 {
        let fee_key = keylet::fee_settings_key();
        if let Some(data) = sandbox.read(&fee_key) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                if let Some(r) = v.get("ReserveIncrement").and_then(|r| r.as_u64()) {
                    return r;
                }
                if let Some(r) = v
                    .get("ReserveIncrementDrops")
                    .and_then(|r| r.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    return r;
                }
            }
        }
        200_000
    }
}

impl Transactor for PaymentTransactor {
    /// val-060: Format validation — no state access.
    fn preflight(&self, tx: &TxFields) -> TxResult {
        // Must be a Payment
        if tx.tx_type != "Payment" {
            return TxResult::Malformed;
        }

        // Fee must be positive
        if tx.fee == 0 {
            return TxResult::BadFee;
        }

        // Destination must be present and valid
        if Self::destination(tx).is_none() {
            return TxResult::Malformed;
        }

        // Amount: XRP drops or an IOU object.
        match Self::amount_drops(tx) {
            Some(amount) => {
                if amount == 0 || amount > 100_000_000_000_000_000 {
                    return TxResult::BadAmount;
                }
            }
            None => {
                if let Some((mid, v)) =
                    tx.fields.get("Amount").and_then(crate::tx::mpt::parse_mpt_amount)
                {
                    // MPT delivery, MPTokensV1 (Payment.cpp:119-233): value
                    // positive and within the signed-64 cap; Paths are
                    // malformed outright; SendMax must be the SAME issuance.
                    // The tfLimitQuality/tfNoRippleDirect refusals share the
                    // XRP-direct tem codes upstream — no specimen pins their
                    // exact strings, so they are refused as plain Malformed.
                    if v == 0 || v > crate::tx::mpt::MAX_MPT_AMOUNT {
                        return TxResult::BadAmount;
                    }
                    if tx.fields.get("Paths").is_some() {
                        return TxResult::Malformed;
                    }
                    if let Some(sm) = tx.fields.get("SendMax") {
                        match crate::tx::mpt::parse_mpt_amount(sm) {
                            Some((sid, sv)) if sid == mid => {
                                if sv == 0 || sv > crate::tx::mpt::MAX_MPT_AMOUNT {
                                    return TxResult::BadAmount;
                                }
                            }
                            _ => return TxResult::Malformed,
                        }
                    }
                    // tfNoRippleDirect (0x10000) | tfLimitQuality (0x40000):
                    // both meaningless for a V1 MPT payment and refused.
                    let fl = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
                    if fl & 0x0005_0000 != 0 {
                        return TxResult::Malformed;
                    }
                } else {
                    // IOU delivery — validated by the engine in do_apply.
                    let ok = tx.fields.get("Amount")
                        .and_then(crate::ledger::keylet::amount_mant_exp)
                        .is_some_and(|(m, _)| m > 0);
                    if !ok {
                        return TxResult::BadAmount;
                    }
                }
            }
        }
        // A SendMax naming an MPT under a non-MPT Amount is malformed
        // (Payment.cpp:146-147).
        if tx.fields.get("Amount").and_then(crate::tx::mpt::parse_mpt_amount).is_none()
            && tx.fields.get("SendMax").and_then(crate::tx::mpt::parse_mpt_amount).is_some()
        {
            return TxResult::Malformed;
        }

        // Can't send to yourself (rippled allows it but it's a no-op)
        // Safe: we already checked destination is Some above
        let dest = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        if dest == tx.account {
            // rippled actually allows this — it's just a fee burn
            // We'll allow it too
        }

        TxResult::Success
    }

    /// val-061: State validation — read-only checks.
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);

        // Sender must exist
        let acct_data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };

        let acct: serde_json::Value = match serde_json::from_slice(&acct_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Check sequence (skip for ticket-based txs)
        if !tx.uses_ticket() {
            let acct_seq = acct["Sequence"].as_u64().unwrap_or(0) as u32;
            if tx.sequence < acct_seq {
                return TxResult::PastSeq;
            }
            if tx.sequence > acct_seq {
                return TxResult::BadSequence;
            }
        }

        // Check LastLedgerSequence
        if let Some(max_ledger) = tx.last_ledger_seq {
            let current_seq = sandbox.base().header.sequence;
            if current_seq > max_ledger {
                return TxResult::MaxLedger;
            }
        }

        // If destination doesn't exist, amount must meet reserve
        let dest = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        let dest_key = keylet::account_root_key(&dest);
        if !sandbox.exists(&dest_key) {
            // rippled tests the AMOUNT TYPE first: a non-XRP payment cannot
            // create an account at all, so it is tecNO_DST whatever its size
            // (Payment.cpp:331-338). Only a NATIVE amount falls through to the
            // reserve test (:351). `amount_drops` yields 0 for an IOU, so
            // testing the reserve first turned every such payment into
            // tecNO_DST_INSUF_XRP — #106143187 `FB93CF580435`.
            if !tx.fields.get("Amount").map(|a| a.is_string()).unwrap_or(false) {
                return TxResult::NoDst;
            }
            // ⚠ NOT modelled: `telNO_DST_PARTIAL` for a tfPartialPayment to a
            // nonexistent account (:340-349). That is a `tel` code — NOT
            // claimed, so the transaction would not appear in the ledger at
            // all — and no failing ledger pins it. Adding it is a different
            // class of change from this one.
            let reserve = Self::reserve_base(sandbox);
            let amount = Self::amount_drops(tx).unwrap_or(0);
            if amount < reserve {
                return TxResult::NoDstInsufXrp;
            }
        } else if tx.fields.get("DestinationTag").is_none() {
            // lsfRequireDestTag on the destination rejects untagged payments.
            let requires_tag = sandbox
                .read(&dest_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                .and_then(|a| a["Flags"].as_u64())
                .map(|f| f & 0x0002_0000 != 0)
                .unwrap_or(false);
            if requires_tag {
                return TxResult::DstTagNeeded;
            }
        }

        // Funding LAST. rippled decides every DESTINATION question in
        // `Payment::preclaim` — tecNO_DST (337), tecNO_DST_INSUF_XRP (360),
        // tecDST_TAG_NEEDED (372) — and only reaches the funding guard in
        // `doApply` (627). Running funding first inverts that, and a ledger
        // where BOTH hold then gets the wrong code: #106136429 `6FF692D40C4F`
        // pays an lsfRequireDestTag destination with no tag while short by
        // exactly 20 drops. Mainnet says tecDST_TAG_NEEDED; we said
        // tecUNFUNDED_PAYMENT.
        //
        // Applies only to a DIRECT full-delivery XRP payment. Cross-currency
        // (SendMax) buys the XRP via paths, and tfPartial delivers what
        // liquidity affords — both resolved in do_apply.
        let partial = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0002_0000 != 0;
        let pure_xrp = tx.fields.get("SendMax").is_none()
            && tx.fields.get("Paths").is_none()
            && tx.fields.get("Amount").map(|a| a.is_string()).unwrap_or(false);
        if pure_xrp && !partial {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let amount = Self::amount_drops(tx).unwrap_or(0);
            // rippled: mPriorBalance < amount + accountReserve(OwnerCount) —
            // the sender's reserve is untouchable (#105035381 D21350B6).
            let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
            let reserve = Self::reserve_base(sandbox)
                .saturating_add(Self::reserve_inc(sandbox).saturating_mul(oc));
            if balance < amount.saturating_add(reserve) {
                return TxResult::UnfundedPayment;
            }
        }

        TxResult::Success
    }

    /// val-062: Apply payment state changes — direct XRP, direct IOU, or
    /// cross-currency via the order books (the crossing engine).
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // rippled's AMMContext lives for the whole flow of ONE transaction.
        let _amm_ctx = crate::tx::amm_swap::AmmCtxGuard::new();
        // Finding 134: the payment's PaymentSandbox opens with the flow.
        crate::tx::offer::owner_count_epoch_start();
        let dest_id = match Self::destination(tx) {
            Some(d) => d,
            None => return TxResult::Malformed,
        };
        // A pseudo-account (AMM, Vault, LoanBroker) cannot receive an ordinary
        // Payment — value enters it only through its own transaction type.
        // rippled: `isPseudoAccount(sleDst) => tecNO_PERMISSION` (Payment.cpp
        // :635), where a pseudo-account is any AccountRoot carrying a
        // designator field (sfAMMID / sfVaultID / sfLoanBrokerID). Applies to
        // every payment variant, so it is checked once here before branching.
        // #105741244 BF560571 pays XPM straight to an AMM account (AMMID set)
        // — mainnet tecNO_PERMISSION, we delivered.
        if let Some(dst) = crate::tx::offer::json_at(sandbox, &keylet::account_root_key(&dest_id)) {
            if ["AMMID", "VaultID", "LoanBrokerID"].iter().any(|f| dst.get(f).is_some()) {
                return TxResult::NoPermission;
            }
        }
        let amt_json = tx.fields.get("Amount").cloned().unwrap_or_default();
        let sendmax = tx.fields.get("SendMax").cloned();
        let partial = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0002_0000 != 0;
        // MPT delivery: V1 never reaches the flow engine — Payment.cpp:449
        // requires `!isDstMPT || mpTokensV2` for the ripple route, so the
        // whole payment is the direct arm in tx::mpt. Deposit auth is applied
        // UNCONDITIONALLY inside it (:533), unlike the reserve-gated XRP form
        // below, which is why this dispatch sits before that block.
        if let Some((mptid, value)) = crate::tx::mpt::parse_mpt_amount(&amt_json) {
            return crate::tx::mpt::apply_mpt_payment(tx, sandbox, &dest_id, mptid, value, partial);
        }
        let cross_currency = match (&sendmax, amt_json.is_string()) {
            (Some(sm), true) => !sm.is_string(),
            (Some(sm), false) => {
                sm.is_string()
                    || sm.get("currency").and_then(|v| v.as_str())
                        != amt_json.get("currency").and_then(|v| v.as_str())
                    || sm.get("issuer").and_then(|v| v.as_str())
                        != amt_json.get("issuer").and_then(|v| v.as_str())
            }
            (None, _) => false,
        } || tx.fields.get("Paths").is_some();

        // DEPOSIT AUTHORIZATION. An account carrying lsfDepositAuth accepts a
        // payment only from ITSELF or from a sender it has preauthorized
        // (CredentialHelpers.cpp `verifyDepositPreauth`):
        //     if (sleDst->isFlag(lsfDepositAuth) && src != dst
        //         && !view.exists(keylet::depositPreauth(dst, src)))
        //         return tecNO_PERMISSION;
        //
        // rippled applies it UNCONDITIONALLY on the "ripple" route —
        // `(hasPaths || sendMax || !dstAmount.native())`, Payment.cpp:451-464 —
        // and on a direct XRP payment only when the amount or the destination's
        // balance exceeds the BASE RESERVE (Payment.cpp:661-668). That
        // carve-out is deliberate and rippled explains it: without it an
        // account that sets the flag and then spends all its XRP "would be
        // unable to acquire more XRP required to pay fees", i.e. wedged.
        //
        // #106309898 03B98013: 0.000074 RPR to radN7hxK9, whose Flags are
        // exactly lsfDepositAuth (0x01000000) and nothing else. Mainnet claims
        // the fee and stops; we had no such check at all and delivered — 3
        // mutations against mainnet's 1. Succeeding where mainnet REFUSES moves
        // value mainnet never moved, which is why this outranked the rest of
        // the batch.
        if let Some(dst) = crate::tx::offer::json_at(sandbox, &keylet::account_root_key(&dest_id)) {
            if dst["Flags"].as_u64().unwrap_or(0) & 0x0100_0000 != 0 && dest_id != tx.account {
                let ripple = cross_currency || !amt_json.is_string() || sendmax.is_some();
                let gated = ripple || {
                    let reserve = crate::ledger::fees::reserve_base(sandbox);
                    let dst_bal =
                        dst["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    Self::amount_drops(tx).unwrap_or(0) > reserve || dst_bal > reserve
                };
                if gated
                    && sandbox
                        .read(&keylet::deposit_preauth_key(&dest_id, &tx.account))
                        .is_none()
                {
                    return TxResult::NoPermission;
                }
            }
        }

        if cross_currency {
            return self.apply_path_payment(tx, sandbox, &amt_json, sendmax.as_ref(), &dest_id, partial);
        }
        if !amt_json.is_string() {
            return self.apply_iou_direct(tx, sandbox, &amt_json, &dest_id, partial);
        }

        let amount = match Self::amount_drops(tx) {
            Some(a) => a,
            None => return TxResult::Malformed,
        };

        // --- Sender side ---
        let sender_key = keylet::account_root_key(&tx.account);
        let sender_data = match sandbox.read(&sender_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut sender: serde_json::Value = match serde_json::from_slice(&sender_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Deduct amount (fee is deducted by apply_common)
        let sender_balance = sender["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if sender_balance < amount {
            return TxResult::UnfundedPayment;
        }

        sender["Balance"] = serde_json::Value::String((sender_balance - amount).to_string());
        sandbox.write(sender_key, serde_json::to_vec(&sender).expect("serializing valid JSON Value"));

        // --- Destination side ---
        let dest_key = keylet::account_root_key(&dest_id);

        if let Some(dest_data) = sandbox.read(&dest_key) {
            // Destination exists — add amount
            let mut dest: serde_json::Value = match serde_json::from_slice(&dest_data) {
                Ok(v) => v,
                Err(_) => return TxResult::Malformed,
            };

            let dest_balance = dest["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            let new_dest_balance = match dest_balance.checked_add(amount) {
                Some(b) => b,
                None => return TxResult::Malformed,
            };
            dest["Balance"] = serde_json::Value::String(new_dest_balance.to_string());
            // Finding 35 (#106644275 F67C91C7-era, dst rMWEptMt): an XRP
            // delivery RE-ARMS the destination's free key reset —
            // "Re-arm the password change fee if we can and need to",
            // Payment.cpp:674-676: clearFlag(lsfPasswordSpent) on the DST.
            // SetRegularKey_test.cpp:98-100 pins it: a fee-0 regkey sets the
            // flag, a later inbound payment clears it.
            const LSF_PASSWORD_SPENT: u64 = 0x0001_0000;
            let dflags = dest["Flags"].as_u64().unwrap_or(0);
            if dflags & LSF_PASSWORD_SPENT != 0 {
                dest["Flags"] = serde_json::json!(dflags & !LSF_PASSWORD_SPENT);
            }
            sandbox.write(dest_key, serde_json::to_vec(&dest).expect("serializing valid JSON Value"));
        } else {
            // Destination doesn't exist — create new AccountRoot
            let reserve = Self::reserve_base(sandbox);
            if amount < reserve {
                return TxResult::NoDstInsufXrp;
            }

            // DeletableAccounts: a fresh account starts at the CREATING
            // ledger's sequence (Change.cpp/View — view.seq()), not 1.
            // #106455036 859EE0EE via the full-ledger replay.
            let new_account = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(dest_id),
                "Balance": amount.to_string(),
                "Sequence": sandbox.base().header.sequence + 1,
                "OwnerCount": 0,
                "Flags": 0,
            });
            sandbox.write(dest_key, serde_json::to_vec(&new_account).expect("serializing valid JSON Value"));
        }

        TxResult::Success
    }
}

impl PaymentTransactor {
    /// Direct same-currency IOU transfer over trust lines. Key-set faithful:
    /// sender/receiver lines adjust (receiver line created when absent),
    /// issuer-side legs settle implicitly. Insufficient holdings fail the
    /// rippled way: nothing delivered -> tecPATH_DRY, partial short-fall
    /// without tfPartialPayment -> tecPATH_PARTIAL (both fee-only).
    fn apply_iou_direct(
        &self,
        tx: &TxFields,
        sandbox: &mut Sandbox,
        amt_json: &serde_json::Value,
        dest: &[u8; 20],
        partial: bool,
    ) -> TxResult {
        use crate::tx::offer as ox;
        let (Some(leg), Some(want)) = (ox::leg_of(amt_json), crate::ledger::keylet::amount_mant_exp(amt_json)) else {
            return TxResult::Malformed;
        };
        // rippled delivers an IOU to the destination by rippling through the
        // issuer, which credits the destination's EXISTING trust line — a
        // payment never opens a trust line for the receiver. If the
        // destination neither issues the currency nor already trusts the
        // issuer, there is no line to credit and the strand is dry (#105797892
        // 41D13D: dest holds no BXE line → mainnet tecPATH_DRY; we phantom-
        // created the line and over-delivered). The sender's own line is
        // handled by available() above; only the receiving side needs a line.
        if dest != &leg.issuer {
            let dest_line = keylet::ripple_state_key(dest, &leg.issuer, &leg.cur);
            if !sandbox.exists(&dest_line) {
                return TxResult::PathDry;
            }
        }
        // Finding 162: the default path ripples through the ISSUER, and
        // checkNoRipple refuses that when the issuer's NoRipple flag sits on
        // both holders' lines (DirectStep.cpp:859) — tecPATH_DRY, fee only.
        if tx.account != leg.issuer
            && dest != &leg.issuer
            && crate::tx::direct_step::check_no_ripple(sandbox, &tx.account, &leg.issuer, dest, &leg.cur)
        {
            return TxResult::PathDry;
        }
        let avail = if tx.account == leg.issuer {
            want // issuers mint their own IOU
        } else {
            ox::available(sandbox, &tx.account, &leg)
        };
        if ox::me_is_zero(avail) {
            return TxResult::PathDry;
        }
        // Rippling from one holder to another goes through the issuer, which
        // charges its TransferRate: the sender parts with `spend` and the
        // destination is credited `spend / rate` (DirectStep.cpp:646,765).
        // The fee applies only between two non-issuers — issuing and
        // redeeming are free.
        //
        // With no SendMax, rippled caps the source at the Amount itself
        // (Payment::doApply's maxSourceAmount defaults to saDstAmount reissued
        // by the sender), so a fee-charging issuer makes the full Amount
        // unreachable and the payment is short by exactly the fee unless
        // tfPartialPayment is set — the standing "SendMax must be
        // Amount × rate" rule. #105772509 FA24C351 and #105784451 B1C3696D
        // are both this: 2.59929213683045/1.06 and 28.2780133567269/1.06 fall
        // under their Amounts, which is mainnet's tecPATH_PARTIAL.
        let rate = if tx.account == leg.issuer || dest == &leg.issuer {
            None
        } else {
            Self::transfer_rate(sandbox, &leg)
        };
        // Source cap: SendMax when given (same currency, or we would not be on
        // this path), else the Amount itself. Paying a fee-charging issuer in
        // full requires SendMax = Amount × rate, and that must still work.
        let cap = tx
            .fields
            .get("SendMax")
            .and_then(crate::ledger::keylet::amount_mant_exp)
            .unwrap_or(want);
        // The final issuer→dest step is capped at what the destination can
        // still receive — `creditLimit(dst,issuer) − heldByDst`, floored at
        // zero (DirectStepI::maxPaymentFlow, DirectStep.cpp:487). A
        // destination already at or over its own trust limit receives nothing,
        // so the strand is dry. `apply_path_payment` has carried this cap since
        // `04a1586`; that commit documented the direct route as the same rule
        // "in principle" and left it alone for want of a failing ledger. These
        // are those ledgers.
        //
        // #105828788 6D342FDE and #105896643 D3FEA91C send JUST1 to
        // destinations holding 1.002263 and 1.040687 against a limit of 0.
        // #105855167 3F56723B sends YZZUF to a DEFAULT-STATE line — limit 0,
        // balance 0 — which `account_lines` does not report at all; only
        // `ledger_entry` by index shows it, which is why the existing
        // "destination must have a line" guard passes and we delivered. All
        // three are 3v1: mainnet claims the fee alone.
        // Finding 157 — `checkFreeze` on the issuer → destination hop. The
        // holder → issuer → holder strand is two steps, so both are subject
        // (the sender's leg is already judged by `available`); a sender or
        // destination that IS the issuer makes a one-step strand, which
        // rippled exempts ("pure issue/redeem can't be frozen"). The
        // destination's own lsfGlobalFreeze, its side of the (issuer,
        // destination) line frozen, or a deep freeze, and the strand is dry.
        // #106753769 DD2CD0BC4C81 (again #106753771, #106753953): rARKjtjX
        // pays ASC/RPR to r37rYnxT, whose root has lsfGlobalFreeze — mainnet
        // returns tecPATH_DRY; we moved the funds across both lines.
        if tx.account != leg.issuer
            && dest != &leg.issuer
            && crate::tx::direct_step::hop_frozen_parts(sandbox, &leg.issuer, dest, &leg.cur)
        {
            return TxResult::PathDry;
        }
        let target = match ox::dest_receivable(sandbox, dest, &leg) {
            Some(r) if ox::me_cmp(r, want).is_lt() => r,
            _ => want,
        };
        if ox::me_is_zero(target) {
            return TxResult::PathDry;
        }
        // What the sender must part with to land `target` on the destination.
        //
        // The fee is charged by the issuer→destination DirectStepI (the
        // previous step redeems, so `qualitiesSrcIssues` sets srcQOut to the
        // issuer's TransferRate) and both passes run rippled's IOUAmount
        // `mulRatio`, NOT a plain ceil/floor: the reverse pass sizes the
        // input with `mulRatio(out, rate, QUALITY_ONE, roundUp=true)`
        // (DirectStep.cpp:537), the forward pass delivers
        // `mulRatio(in, QUALITY_ONE, rate, roundUp=false)` (:646) — whose
        // "round down" is Number's half-even NEAREST of the 18-digit quotient
        // for a positive amount — and `setCacheLimiting` (:575) never lets
        // the forward output exceed the reverse pass's. Finding 96:
        // #106711980 81340311C30C sends 190.36 EUR (GateHub, rate 1.002)
        // under tfPartialPayment: 190.36 / 1.002 = 189.98003992015968… and
        // mainnet delivers 189.9800399201597; the truncating `me_muldiv`
        // delivered …596, one ulp under, on the destination's line.
        let need = match rate {
            Some(r) => ox::mul_ratio(target, r as u128, 1_000_000_000, true),
            None => target,
        };
        let spend = [avail, cap, need]
            .into_iter()
            .reduce(|a, b| if ox::me_cmp(a, b).is_lt() { a } else { b })
            .unwrap_or(want);
        let deliver = match rate {
            Some(r) => {
                let fwd = ox::mul_ratio(spend, 1_000_000_000, r as u128, false);
                if ox::me_cmp(fwd, target).is_gt() { target } else { fwd }
            }
            None => spend,
        };
        if ox::me_is_zero(deliver) {
            return TxResult::PathDry;
        }
        if ox::me_cmp(deliver, want).is_lt() && !partial {
            return TxResult::PathPartial;
        }
        match rate {
            None => ox::move_leg(sandbox, &tx.account, dest, &leg, deliver),
            Some(_) => {
                ox::line_adjust(sandbox, &tx.account, &leg, spend, false);
                ox::line_adjust(sandbox, dest, &leg, deliver, true);
            }
        }
        TxResult::Success
    }

    /// Cross-currency delivery: spend the SendMax side across the order book
    /// to acquire the Amount side (the offer-crossing engine with payment
    /// semantics), then hand the acquisition to the destination. Arb-style
    /// self-payments (Account == Destination) skip the final hop.
    fn apply_path_payment(
        &self,
        tx: &TxFields,
        sandbox: &mut Sandbox,
        amt_json: &serde_json::Value,
        sendmax: Option<&serde_json::Value>,
        dest: &[u8; 20],
        partial: bool,
    ) -> TxResult {
        use crate::tx::offer as ox;
        let Some(sm_json) = sendmax else {
            // Paths without SendMax: spend the Amount currency itself.
            return self.apply_iou_direct(tx, sandbox, amt_json, dest, partial);
        };
        let (Some(want_leg), Some(want0)) = (ox::leg_of(amt_json), crate::ledger::keylet::amount_mant_exp(amt_json)) else {
            return TxResult::Malformed;
        };
        let (Some(spend_leg), Some(spend0)) = (ox::leg_of(sm_json), crate::ledger::keylet::amount_mant_exp(sm_json)) else {
            return TxResult::Malformed;
        };
        // Same rule as the direct case above, and it holds however the value
        // is acquired: every strand ends by rippling the delivered IOU from
        // its issuer to the destination, and that final step is a
        // DirectStepI whose `check` opens with "Since this is a payment a
        // trust line must be present" — no line ⇒ terNO_LINE (DirectStep.cpp
        // :423). Because every strand shares that last step, one missing line
        // invalidates all of them, and a flow with no strands is tecPATH_DRY.
        //
        // Crossing a book or an AMM does NOT earn the destination a line:
        // that is offer crossing, a different step class, which is why a
        // taker gets a new line but a payer never does (#105794129 B405DAF6:
        // self-conversion arb XRP→BIT→JUST1 where the sender held no JUST1
        // line — mainnet tecPATH_DRY, fee only, while we crossed two pools
        // and delivered).
        if !want_leg.xrp && dest != &want_leg.issuer {
            let dest_line = keylet::ripple_state_key(dest, &want_leg.issuer, &want_leg.cur);
            if !sandbox.exists(&dest_line) {
                return TxResult::PathDry;
            }
        }
        // rippled DirectStepI::maxPaymentFlow (DirectStep.cpp:476-488): the
        // strand's last step delivers the IOU issuer→dest, and it can move at
        // most creditLimit(dest,issuer) − heldByDest. A destination already at
        // or over its trust limit therefore receives NOTHING, and since every
        // strand ends in that same step the whole flow is dry (#105740164
        // B7C6328C: the arb bot's JUST1 line is limit 0 while it already holds
        // 1.000383, so mainnet delivers zero — tecPATH_DRY, fee only — where we
        // crossed two AMM pools and over-delivered 39131 JUST1). A destination
        // merely near its limit caps the fill instead of blocking it.
        let recv_cap = ox::dest_receivable(sandbox, dest, &want_leg);
        if matches!(&recv_cap, Some(c) if ox::me_is_zero(*c)) {
            return TxResult::PathDry;
        }
        // Drive the strand toward the smaller of the requested Amount and the
        // destination's remaining capacity; `want0` is kept for the "delivered
        // the full Amount?" partial check further down.
        let want_target = match &recv_cap {
            Some(c) if ox::me_cmp(*c, want0).is_lt() => *c,
            _ => want0,
        };
        // The strand's input is bounded by what the sender actually holds of
        // the SendMax asset (issuer mints freely; XRP is balance-minus-
        // reserve; IOU is the trust-line holding). A sender with nothing to
        // spend is a dry path regardless of book or AMM depth.
        let spend_avail = ox::available(sandbox, &tx.account, &spend_leg);
        if ox::me_is_zero(spend_avail) {
            return TxResult::PathDry;
        }
        let spend0 = if ox::me_cmp(spend_avail, spend0).is_lt() { spend_avail } else { spend0 };
        // INPUT-side TransferRate. A payment sets `ownerPaysTransferFee_ =
        // false`, so `trIn = transferRate(book_.in.account)` — the SENDER pays
        // `amount × rate` and the counterparty receives `amount`
        // (BookStep.cpp:737-739). `apply_iou_direct` has always done this for
        // the same-currency case; the crossing path only ever charged the
        // OUTPUT issuer (`want_rate`) and let the input through free.
        //
        // #105663160 C100D2BF306FD484 is the proof, and it is exact. A circular
        // tfPartialPayment, SendMax 10059 SGB (issuer rctArjqVvTHi,
        // TransferRate 1003000000) for XRP, one AMM counterparty:
        //   mainnet  sender −10059.000   pool +10028.913   (10059/1.003)
        //   ours     sender −10029.051   pool +10029.051   (no fee at all)
        // 30.087 SGB that mainnet destroys and we never charged.
        //
        // So SendMax is a GROSS cap: what can actually reach counterparties is
        // `SendMax / rate`, and the sender's holding is a gross bound too —
        // dividing the min of the two covers both.
        let spend_rate = if tx.account == spend_leg.issuer {
            None
        } else {
            Self::transfer_rate(sandbox, &spend_leg)
        };
        // Direct strands charge the input rate INSIDE the strand (a hop's
        // srcQOut, DirectStep.cpp:764-765), so they spend GROSS — dividing
        // their remainder here would double-charge. Books spend NET (the
        // walk debits per fill). Both remainders are tracked below.
        let spend0_gross = spend0;
        // mulRatio-NEAREST, not floor — the spend-side twin of the hop-joint
        // fix (e4c1581), deferred then for want of a specimen. #106455142
        // C0442CB5 is it: a sender-line drain nets the whole 2.500399999999944
        // UNI balance through the 1.001 issuer; the quotient is …041|958 —
        // rippled's DirectStepI nets it to …042 (nearest), the floor said
        // …041, and the maker's offer residual (wants0 − net) landed …959
        // for mainnet's …958. The gross side is untouched — the drain still
        // debits the balance verbatim (gross-primary), only the NET the
        // maker sees carried the floor's ulp.
        let spend0 = match spend_rate {
            Some(r) => ox::mul_ratio(spend0, 1_000_000_000, r as u128, false),
            None => spend0,
        };
        let snap = sandbox.snapshot();
        // rippled only imposes the SendMax/Amount ratio as a per-offer quality
        // bound when tfLimitQuality is set. Otherwise the book is walked
        // best-first under the aggregate bounds alone (spend ≤ SendMax,
        // deliver ≤ Amount) — arb-style payments use a sentinel-max Amount
        // whose implied ratio would read every book as dry.
        let limit_quality =
            tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0004_0000 != 0;
        let threshold = if limit_quality {
            crate::ledger::keylet::offer_quality(sm_json, amt_json).unwrap_or(u64::MAX)
        } else {
            u64::MAX
        };
        // Multi-hop strands: the FIRST path's elements name the intermediate
        // currencies — one book per adjacent pair. Account elements
        // (rippling) are not modeled; those fall back to the single-book
        // cross. Intermediate acquisitions ride "in flight" through the
        // SENDER: each hop credits the sender and the next debits the exact
        // same amount, so the net-zero line drops out of the mutation set —
        // matching rippled, which never materializes it.
        const TF_NO_RIPPLE_DIRECT: u64 = 0x0001_0000;
        let no_direct =
            tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & TF_NO_RIPPLE_DIRECT != 0;
        let (path_chains, named_paths) = Self::path_chains(tx);
        // PURE-ACCOUNT paths — rippling through accounts with no book hop.
        // `path_legs` has always dropped these (they are invisible to
        // `path_chains`); each now becomes a strand of DirectStepI hops
        // (docs/DIRECTSTEP-DESIGN.md stage 1). Only possible when both ends
        // are the SAME IOU currency: an account element can re-anchor the
        // issuer but never change the currency, and toStrand inserts a book
        // the moment the currency differs. Construction-time checks
        // (terNO_LINE, terNO_AUTH, the dry precheck, the loop dedup) drop a
        // strand exactly where rippled drops the path.
        //
        // #106102038 5B97B89E: CNY through one gateway — src and dst both
        // hold rKiCet lines, two hops, three mutations. #106373989
        // 8CAD0435: USDC.rGm7 → USDC.rcEG over four hops, where rcEG's
        // 1.003 TransferRate is charged inside the strand — which is the
        // whole reason its SendMax runs 0.3% over its Amount.
        let mut dstrands: Vec<Vec<crate::tx::direct_step::DirectHop>> = Vec::new();
        if !spend_leg.xrp && !want_leg.xrp && spend_leg.cur == want_leg.cur {
            for path in tx
                .fields
                .get("Paths")
                .and_then(|p| p.as_array())
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_array())
            {
                let Some(seq) = crate::tx::direct_step::pure_account_sequence(
                    &tx.account,
                    dest,
                    &want_leg.issuer,
                    &spend_leg.issuer,
                    path,
                ) else {
                    continue;
                };
                let built = crate::tx::direct_step::build_direct_strand(sandbox, &seq, &want_leg.cur);
                if std::env::var("DX_PAY").is_ok() {
                    eprintln!(
                        "DX_PAY direct seq={:?} built={}",
                        seq.iter().map(|a| hex::encode(&a[..4])).collect::<Vec<_>>(),
                        built.as_ref().map(|h| h.len() as i64).unwrap_or(-1),
                    );
                }
                if let Some(hops) = built {
                    if !hops.is_empty() && !dstrands.contains(&hops) {
                        dstrands.push(hops);
                    }
                }
            }
        }
        // MIXED paths — direct runs composed with book hops (design stage
        // 2). `mixed_layout` yields None for pure shapes, so there is no
        // overlap with either the leg pipeline or the pure-direct strands;
        // `check_mixed_strand` drops a strand exactly where rippled's
        // construction checks drop the path.
        let mut mstrands: Vec<Vec<crate::tx::direct_step::SegLayout>> = Vec::new();
        for path in tx
            .fields
            .get("Paths")
            .and_then(|p| p.as_array())
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_array())
        {
            let Some(segs) = crate::tx::direct_step::mixed_layout(
                &tx.account, dest, &spend_leg, &want_leg, path,
            ) else {
                continue;
            };
            let ok = crate::tx::direct_step::check_mixed_strand(sandbox, &segs);
            if std::env::var("DX_PAY").is_ok() {
                let shape: Vec<String> = segs
                    .iter()
                    .map(|g| match g {
                        crate::tx::direct_step::SegLayout::Run(h) => format!("Run({})", h.len()),
                        crate::tx::direct_step::SegLayout::Book { from, to } => format!(
                            "Book({}>{})",
                            if from.xrp { "XRP".into() } else { hex::encode(&from.cur[12..15]) },
                            if to.xrp { "XRP".into() } else { hex::encode(&to.cur[12..15]) },
                        ),
                    })
                    .collect();
                eprintln!("DX_PAY mixed shape={shape:?} ok={ok}");
            }
            if ok && !mstrands.contains(&segs) {
                mstrands.push(segs);
            }
        }
        // `path_legs` drops a path we cannot model — an account
        // element that ripples through a THIRD PARTY rather than re-anchoring on
        // the previous leg's issuer. With every named path dropped the fallback
        // below behaves exactly like "no Paths at all" and walks the single
        // default book.
        //
        // That is a harmless under-approximation while the default path is in
        // play, because rippled builds it alongside the named ones anyway. Under
        // tfNoRippleDirect it is not: the flag SUPPRESSES the default path, so
        // the strands named in Paths are the only ones that exist, and falling
        // back substitutes precisely the strand the flag forbids.
        //
        // #106146562 C56D61917E4B: circular tfPartialPayment|tfNoRippleDirect,
        // SendMax 348.6072850702524 RPR for 816316 drops, Paths naming two
        // account hops that hold NO RPR line at all (`account_lines` at
        // #106146561 against the issuer returns nothing for either). Mainnet is
        // tecPATH_DRY, fee only. We ignored the path, crossed the RPR->XRP book
        // against an offer created earlier in the SAME ledger, and returned
        // tesSUCCESS with 3 extra nodes — all belonging to a maker the path
        // never names.
        if no_direct && named_paths > 0 && path_chains.is_empty() && dstrands.is_empty() && mstrands.is_empty() {
            if std::env::var("DX_PAY").is_ok() {
                eprintln!("DX_PAY DRY-EXIT no_direct-all-empty named={named_paths}");
            }
            sandbox.restore_snapshot(snap);
            return TxResult::PathDry;
        }
        // NO-RIPPLE BLOCKS THE STEP OUT OF A BOOK. A `DirectStepI` that
        // immediately follows a BOOK step is refused when the SOURCE side of
        // its trust line carries NoRipple (DirectStep.cpp:440-445):
        //     if (ctx.prevStep->bookStepBook())
        //         if (sleLine->isFlag((src_ > dst_) ? lsfHighNoRipple : lsfLowNoRipple))
        //             return terNO_RIPPLE;
        // Delivering an IOU bought on a book means exactly that shape — book,
        // then issuer -> destination — so the flag on the ISSUER's side of the
        // destination's line decides whether ANY strand can deliver at all.
        // It is a property of the delivery, not of a particular path, which is
        // why it is tested once here rather than per strand.
        //
        // #105985066 F205D076: a circular tfPartialPayment, SendMax 2 XRP for
        // DTCC and NO Paths. The DTCC line carries NoRipple on BOTH sides and
        // the issuer is the LOW account, so rippled logs `toStep failed: -90`
        // (terNO_RIPPLE), "failed to add default path", and returns tecPATH_DRY
        // with nothing attempted. We crossed the XRP/DTCC book, came up under
        // DeliverMin and reported tecPATH_PARTIAL — the right refusal for the
        // wrong reason, off liquidity that was never reachable.
        //
        // ⚠ Only the TERMINAL delivery is modelled here. rippled applies the
        // same rule to any direct step following a book mid-strand; we do not
        // materialise those, so there is nothing else to test yet.
        if !want_leg.xrp && dest != &want_leg.issuer && !(spend_leg.xrp == want_leg.xrp && spend_leg.cur == want_leg.cur && spend_leg.issuer == want_leg.issuer) {
            let lk = keylet::ripple_state_key(&want_leg.issuer, dest, &want_leg.cur);
            if let Some(line) = ox::json_at(sandbox, &lk) {
                let flags = line["Flags"].as_u64().unwrap_or(0);
                // The SOURCE of that step is the ISSUER: low side if its id
                // sorts first, high side otherwise.
                let issuer_low = &want_leg.issuer < dest;
                let bit = if issuer_low { 0x0010_0000 } else { 0x0020_0000 };
                if flags & bit != 0 {
                    if std::env::var("DX_PAY").is_ok() {
                        eprintln!("DX_PAY DRY-EXIT dest-line-noripple");
                    }
                    sandbox.restore_snapshot(snap);
                    return TxResult::PathDry;
                }
            }
        }
        // NO-RIPPLE (OR NO LINE) BLOCKS THE STEP INTO A BOOK. The mirror of
        // the rule above: a BOOK step that follows a DirectStepI is refused
        // when the line between that step's SOURCE and the book's in-issuer
        // is missing, or carries NoRipple on the ISSUER's side
        // (BookStep.cpp, BookStep::check):
        //     if (auto const prev = ctx.prevStep->directStepSrcAcct()) {
        //         auto sle = view.read(keylet::line(*prev, cur, issue.currency));
        //         if (!sle) return terNO_LINE;
        //         if (sle->isFlag((cur > *prev) ? lsfHighNoRipple : lsfLowNoRipple))
        //             return terNO_RIPPLE;
        //     }
        // Every strand we model opens with the same DirectStepI (sender ->
        // spend issuer) and then its first book, so the flag on the ISSUER's
        // side of the SENDER's spend line decides whether ANY strand can
        // leave the sender at all; Payment turns the ter into tecPATH_DRY
        // (Payment.cpp: isTerRetry -> tecPATH_DRY). No book, no check: a
        // same-currency delivery is DirectStepI hops only, and a sender who
        // IS the issuer enters the book without a direct step in front.
        // Strands that ripple through an ACCOUNT hop before their first
        // book (`mstrands`) reach that book from the hop's line, not the
        // sender's, so they are judged by their own checks and the refusal
        // below is only whole-payment when none of them exists.
        //
        // #106722089 819A6BBC (finding 105): a tfPartialPayment self-
        // conversion, SendMax 200000 XA3 for 15.976242 XRP through the
        // XA3/XRP pool the sender dominates (94% of the LP tokens). The
        // sender's XA3 line predates the issuer's DefaultRipple and still
        // carries lsfLowNoRipple on the issuer's (low) side. Mainnet:
        // tecPATH_DRY, fee only. We swapped 199995.93 XA3 through the pool,
        // delivered the whole Amount and wrote three extra nodes.
        if !spend_leg.xrp
            && tx.account != spend_leg.issuer
            && (spend_leg.xrp != want_leg.xrp || spend_leg.cur != want_leg.cur)
            && mstrands.is_empty()
        {
            let lk = keylet::ripple_state_key(&tx.account, &spend_leg.issuer, &spend_leg.cur);
            let blocked = match ox::json_at(sandbox, &lk) {
                None => true,
                Some(line) => {
                    let flags = line["Flags"].as_u64().unwrap_or(0);
                    // The flag belongs to the book's in-ISSUER: low side if
                    // its id sorts first, high side otherwise.
                    let issuer_low = &spend_leg.issuer < &tx.account;
                    let bit = if issuer_low { 0x0010_0000 } else { 0x0020_0000 };
                    flags & bit != 0
                }
            };
            if blocked {
                if std::env::var("DX_PAY").is_ok() {
                    eprintln!("DX_PAY DRY-EXIT spend-line-noripple-into-book");
                }
                sandbox.restore_snapshot(snap);
                return TxResult::PathDry;
            }
        }
        // The delivered IOU is re-issued to the destination by the strand's
        // last step, which charges the issuer's TransferRate (see below), so
        // the books must be worked for `Amount × rate` GROSS for the
        // destination to net `Amount`. Targeting `Amount` gross instead would
        // leave every fee-charging delivery short by the fee — #105776250
        // 184CFAEA delivers inside a 0.1% DeliverMin band against a 1.002
        // issuer, and mainnet fills it.
        let want_rate = match Self::transfer_rate(sandbox, &want_leg) {
            Some(r) if dest != &want_leg.issuer => Some(r),
            _ => None,
        };
        // mulRatio, NOT exact ceil: rippled sizes this in DirectStepI's rev
        // quality math (IOUAmount mulRatio — nearest-16 + bump only on a
        // pre-normalize remainder). The two rounders split BOTH ways and both
        // directions are specimen-pinned on #106455100:
        //   0E7D8887 (XAG 1.001): 67057.3053491705 × 1.001 = …96705|0 — the
        //     quotient is EXACT, so no bump: rippled …967, ceil said …968,
        //     and netting the wrong gross put the dest line one ulp high.
        //   0E04203B (1.002 tail): 1.571350128058821 × 1.002 = …93864|2 —
        //     inexact, nearest …939 THEN bump: rippled …940 (shim STEPREV
        //     receipt), ceil said …939, and the whole rev chain ran one ulp
        //     low (…180/…286 for mainnet's …181/…287) — the E1FA line's ulp.
        let want_gross = match want_rate {
            Some(r) => ox::mul_ratio(want_target, r as u128, 1_000_000_000, true),
            None => want_target,
        };
        // The strand's output belongs to the DESTINATION: crediting the
        // sender first and forwarding would materialize an intermediate
        // trust line rippled never creates (and when the destination is the
        // issuer, the IOU is redeemed, not held).
        if std::env::var("DX_PAY").is_ok() {
            let shape = |c: &Vec<ox::Leg>| {
                c.iter()
                    .map(|l| if l.xrp { "XRP".to_string() } else { hex::encode_upper(&l.cur[12..15]) })
                    .collect::<Vec<_>>()
                    .join(">")
            };
            eprintln!("DX_PAY named={named_paths} modelled={:?} spend0={spend0:?} want_gross={want_gross:?} partial={partial}",
                path_chains.iter().map(shape).collect::<Vec<_>>());
        }
        // One code path for both shapes. With no usable hops the chain is just
        // [spend, want] and the walk below runs exactly one hop against
        // `want_gross` — which is precisely what the separate direct branch
        // used to do. Unifying them is the groundwork for iterating STRANDS:
        // rippled always builds the default path alongside the ones named in
        // Paths and splits the delivery across them (StrandFlow.h:640-756).
        // rippled ALWAYS builds the DEFAULT path alongside the ones named in
        // Paths (unless tfNoRippleDirect) and splits the delivery across the
        // strands, applying the best-quality PASS each round and re-evaluating
        // (StrandFlow.h:640-756). We ran the specified chain alone.
        //
        // #105912291 2AE3693EF556, a circular tfPartialPayment of 1 XRP into
        // DMNDBR via an RLUSD hop: mainnet takes 484095 drops through the
        // direct XRP->DMNDBR path and 515894 through XRP->RLUSD->DMNDBR,
        // summing to the full 3244389.84805814 for 999989 of the 1000000
        // SendMax, and fills Offer 3A3053B3 only PARTIALLY. Pushing everything
        // down the RLUSD strand consumed that offer outright plus a second one
        // and still landed 0.3% short.
        //
        // The rounds must be per-PASS, not per-strand: with no limitQuality a
        // payment strand would otherwise drain the whole SendMax on whichever
        // strand happens to be better and the other would never run at all.
        // That is what `single_pass` buys — one quality level per book step,
        // matching what one `flow()` call does for a strand.
        // ONE STRAND PER NAMED PATH, in `Paths` order — rippled's `toStrands`
        // builds a strand for each and `flow()` runs them together. We used to
        // build exactly one, from `paths.first()`.
        let same = |a: &ox::Leg, b: &ox::Leg| a.xrp == b.xrp && a.cur == b.cur && a.issuer == b.issuer;
        let mut strands: Vec<Vec<&ox::Leg>> = Vec::new();
        let mut have_direct = false;
        for hops in &path_chains {
            // Whether the path's LAST explicit element already IS the
            // delivered issue. Then the cross-issuer transition into it was
            // an explicit OFFER-class element and toStrand builds a real
            // same-currency BOOK (toStep: issuer-only e2 → makeBookStepIi;
            // #106455036's lesson) — the inter-gateway drop below is only
            // for the IMPLIED delivery, where normalization appends the
            // issuer as an ACCOUNT element and the transition needs a
            // gateway-to-gateway line. #106455042 42AD7C62: […, USD/rKiCet,
            // issuer-only(32) rvYA] delivers USD.rvYA across the
            // USD.rKiCet/USD.rvYA book mainnet actually crosses; the
            // calibrated drop specimen 619718E8 names only USD/rvYA and
            // delivers USD.rhub8 — still dropped.
            let names_delivery = hops.last().is_some_and(|h| same(h, &want_leg));
            let mut chain: Vec<&ox::Leg> = std::iter::once(&spend_leg)
                .chain(hops.iter())
                .chain(std::iter::once(&want_leg))
                .collect();
            // A path may name an endpoint currency as a "hop" (e.g. deliver
            // RLUSD via [RLUSD-book, issuer]): same-leg neighbours are a
            // zero-length book — collapse them...
            chain.dedup_by(|a, b| a.xrp == b.xrp && a.cur == b.cur && a.issuer == b.issuer);
            // ...but a same-currency send/receive must still walk one book
            // rather than collapse to nothing.
            if chain.len() < 2 {
                chain = vec![&spend_leg, &want_leg];
            }
            // The delivered currency already in hand under another issuer is a
            // gateway-to-gateway ripple, not a book — rippled drops the path.
            if !names_delivery && Self::terminal_is_ripple_step(&chain) {
                if std::env::var("DX_PAY").is_ok() {
                    eprintln!(
                        "DX_PAY terminal ripple step {} -> {}: path dropped (needs inter-gateway line)",
                        hex::encode_upper(chain[chain.len() - 2].issuer),
                        hex::encode_upper(chain[chain.len() - 1].issuer));
                }
                continue;
            }
            // A named path that collapses to [spend, want] IS the default path,
            // so the block below must not add it a second time. #106156904's
            // path 0 is exactly that: a bare [SOL] element on a USDT->SOL
            // payment.
            if chain.len() == 2 {
                have_direct = true;
            }
            // Two named paths can also collapse onto each other once the
            // endpoint dedup above runs. Flowing the same chain twice would let
            // it compete with itself in the round-robin and double-count its
            // liquidity.
            let dup = strands
                .iter()
                .any(|s| s.len() == chain.len() && s.iter().zip(&chain).all(|(a, b)| same(a, b)));
            if !dup {
                strands.push(chain);
            }
        }
        // …and since stage 1, that default IS BUILDABLE: the same-currency
        // cross-issuer default strand is pure DirectStepI hops
        // (src → [issuers] → dst, no book anywhere), and rippled decides it
        // by the strand's own checks — terNO_LINE refuses, a mutual line
        // delivers. #106065267 98C5B11E: 1700 ZAR, src-issue → dst-issue,
        // NO Paths; mainnet moves the one mutual line (2 mutations) where
        // `terminal_is_ripple_step` used to refuse the shape wholesale.
        // Gated to EXACTLY the shape the classic pipeline refuses, so
        // nothing is modeled twice.
        if !no_direct && Self::terminal_is_ripple_step(&[&spend_leg, &want_leg]) {
            if let Some(seq) = crate::tx::direct_step::pure_account_sequence(
                &tx.account, dest, &want_leg.issuer, &spend_leg.issuer, &[],
            ) {
                if let Some(hops) =
                    crate::tx::direct_step::build_direct_strand(sandbox, &seq, &want_leg.cur)
                {
                    if !hops.is_empty() && !dstrands.contains(&hops) {
                        dstrands.push(hops);
                    }
                }
            }
        }
        // The default path is subject to the same rule: sending one gateway's
        // IOU and delivering another's is `src -> sendMaxIssuer ->
        // deliverIssuer -> dst`, three DirectStepIs, and the middle one needs
        // the inter-gateway line just the same.
        let direct_viable = !Self::terminal_is_ripple_step(&[&spend_leg, &want_leg]);
        if !no_direct && !have_direct && direct_viable {
            strands.insert(0, vec![&spend_leg, &want_leg]);
        }
        // Every named path was unmodellable and the default is suppressed —
        // guarded above for tfNoRippleDirect, so this is the no-Paths shape.
        // A live DIRECT strand is a modeled path: nothing to fall back on.
        if strands.is_empty() && (!dstrands.is_empty() || !mstrands.is_empty()) {
            // flow runs on the direct/mixed strands alone
        } else if strands.is_empty() {
            // Nothing left to fall back ON: either the flag forbids the default
            // path or the default path is itself the gateway ripple we just
            // refused. Substituting it would reinstate exactly the strand
            // rippled proved does not exist.
            if (no_direct && named_paths > 0) || !direct_viable {
                sandbox.restore_snapshot(snap);
                return TxResult::PathDry;
            }
            strands.push(vec![&spend_leg, &want_leg]);
        }
        // One strand is the old behaviour exactly: walk the whole book in a
        // single call, no trial run, no second round that can find anything.
        let n_books = strands.len();
        let n_direct = dstrands.len();
        let total_strands = n_books + n_direct + mstrands.len();
        let multi = total_strands > 1;

        // The payment-wide limitQuality as an Me, for the strand judge below.
        // `None` when tfLimitQuality is unset, which is the common case.
        let thr_me = (threshold != u64::MAX).then(|| ox::rate_me(threshold));
        let mut rem_in = spend0;
        // The gross twin, spent by DIRECT strands (their hop qualities carry
        // the input rate); the two stay consistent through the conversions
        // at the bottom of the round loop.
        let mut rem_in_gross = spend0_gross;
        let mut rem_out = want_gross;
        let mut delivered: ox::Me = (0, 0);
        // NET deliveries from DIRECT strands. A book walk credits the
        // destination GROSS and the post-loop trim carves the issuer's fee
        // off; a direct strand's last hop credits exactly NET (its srcQOut
        // charged the fee one hop earlier and destroyed it), so its
        // deliveries must neither chase the grossed target nor suffer the
        // trim. #106373989: the 1.003 gross-up asked the strand for more
        // than its own SendMax could ever deliver and turned a full
        // delivery into tecPATH_PARTIAL.
        let mut delivered_direct: ox::Me = (0, 0);
        // rippled's driver never carries a RUNNING remainder. Every winning
        // pass lands in a sorted multiset and each iteration re-derives
        //   remainingOut = outReq  − sum(savedOuts)
        //   remainingIn  = sendMax − sum(savedIns)
        // (StrandFlow.h:639-642 flat_multiset, :762-766; the final
        // actualIn/actualOut at :801-802 are the same sums). `sum` folds the
        // set ASCENDING with one half-even 16-digit add per element, and the
        // subtraction rounds the same way — so from the THIRD round on the
        // request derives from the ROUNDED TOTAL, not from a rounded chain
        // of differences (two-round strands cannot tell the forms apart).
        // #105795329 ED4F899F is the receipt (FFI trace, three CLOB/AMM
        // rounds): rippled's iter-2 outReq is 220.1943150207048 −
        // n16(75.87773718335664 ⊕ 52.2281718281718x) = 92.0884060091763,
        // where the running chain says …637/…638 — and only the totals form
        // lands the destination's line on mainnet's …048. Every larger line
        // absorbs the ulp in its own rounding, which is why the census saw
        // exactly one key.
        let mut saved_ins: Vec<ox::Me> = Vec::new();
        let mut saved_outs: Vec<ox::Me> = Vec::new();
        fn strand_sum(saved: &mut Vec<ox::Me>) -> ox::Me {
            saved.sort_by(|a, b| ox::me_cmp(*a, *b));
            let mut tot: ox::Me = (0, 0);
            for s in saved.iter() {
                tot = ox::stamount_signed_add(false, tot, false, *s).1;
            }
            tot
        }
        fn strand_rem(req: ox::Me, saved: &mut Vec<ox::Me>) -> ox::Me {
            let tot = strand_sum(saved);
            let (neg, mag) = ox::stamount_signed_add(false, req, true, tot);
            if neg { (0, 0) } else { mag }
        }
        // The NET mirror of rippled's driver units. Its savedOuts live in
        // outReq units — NET of the want-issuer rate — whatever the strand
        // kind, and remainingOut derives from them; our gross pot drives the
        // sizing machinery, this twin drives the destination credits, the
        // limitQuality judge, and the delivered figure. #106453302 BFC61DEF
        // pins both faces at once: the destination line is the CHRONOLOGICAL
        // chain of per-iteration net credits (…+n0+n1+n2 → …264) while
        // DeliveredAmount is the SORTED-ascending fold of the same set
        // (actualOut, StrandFlow.h:801 → …263).
        let mut rem_out_net = want_target;
        let mut saved_outs_net: Vec<ox::Me> = Vec::new();
        let mut book_net: Vec<ox::Me> = Vec::new();
        // rippled's AMMContext, alive for the whole flow: `multiPath()` is
        // `activeStrands.size() > 1`, so a pool offers FIB SLICES rather than
        // `maxOffer` exactly when we run more than one strand, and the counter
        // advances per iteration that consumed AMM liquidity. Carrying it
        // across rounds is the point — restart it each round and every slice
        // would be the base one, where rippled's grow 1,1,2,3,5,8,13.
        let mut amm_fib = ox::AmmFib::default();
        // This loop is rippled's flow(): it alone counts AMM iterations
        // (`ammContext.update()` once per winning round); the book walks
        // its hops run through must not (finding 107).
        crate::tx::amm_swap::amm_ctx_driver_owned(true);
        // `setMultiPath(activeStrands.size() > 1)` is re-evaluated EVERY
        // iteration (StrandFlow.h:649), and a strand that flowed nothing is
        // not pushed back into `next_` — so a payment whose second strand is
        // dry runs multiPath only until that shows, then sizes the pool by
        // `maxOffer` again. Seeded from `strands.size() > 1` exactly as
        // `Flow.cpp:106` does, and re-read from the trials below, which is the
        // same activateNext-then-setMultiPath ordering the bridged walk uses.
        //
        // #105795073 1F30308A4AD1 is why this cannot just be `multi`: its
        // direct SSH->EUR strand carries nothing, so slicing a lone live
        // strand delivered under DeliverMin and turned a tesSUCCESS into
        // tecPATH_PARTIAL.
        let mut multi_now = multi;
        // Finding 151: rippled's `ActiveStrands` — the candidate set is `next_`,
        // seeded with every strand and rebuilt each iteration from what the
        // previous iteration PUSHED: the strand that produced (unless it went
        // inactive) plus the strands behind it in that iteration's order
        // (StrandFlow.h:730-733). A strand that flowed nothing, failed, or was
        // rejected by limitQuality is simply not pushed and never returns;
        // `activateNext` also drops a strand it cannot bound (or whose bound
        // misses the limit) — but only when more than one strand is pending: a
        // lone pending strand is activated unexamined (StrandFlow.h:475-512).
        //
        // #106743104 F80847602E68 (rLpnXUyv, 1.9M XRPH → RLUSD, 54 iterations):
        // the direct strand goes dry after iteration 49 and rippled never looks
        // at it again, so iterations 50-53 run a LONE strand — single-path
        // pricing, the pool leg taken along the curve (118197.49 XRPH →
        // 969.962495443). We re-admitted the dry strand whenever it could be
        // bounded (every other round), flipped the pool to multi-path pricing
        // at the anchored offer's own ratio, and took 13500.15 → 21.7125
        // instead; the fills, the pool balances and the taker's line all part
        // from there (190 mutations to mainnet's 181).
        let mut next_set: Vec<usize> = (0..total_strands).collect();
        // EVERY strand re-enters, lone or not. This used to be `if multi`,
        // justified by "one call already sized it correctly" — which the AMM
        // once-per-iteration boundary in `cross_engine_to` FALSIFIES: a single
        // strand now returns as soon as it has taken one AMM offer, so the
        // rounds are what fetch the rest of the liquidity rather than a way to
        // interleave rivals. rippled's driver makes no distinction either — it
        // loops `while (remainingOut > 0 && *remainingIn > 0)` over whatever
        // `activateNext` yields, one strand or five (StrandFlow.h:630-730).
        //
        // The loop is bounded by its own remainders, so a pass that leaves
        // nothing to do exits on the first check. #105831615 8A754FA3 is why
        // that has to be exact: measure its spend by differencing a balance and
        // 4.38e-11 of the SendMax reads unspent, which buys a whole extra
        // round — see `8d8d2e6`.
        // rippled's driver has no round cap of its own: it loops `while
        // (remainingOut > 0 && *remainingIn > 0)` and only its safety bound
        // ends it — the 1000th entry is telFAILED_PROCESSING (StrandFlow.h:
        // 606-657), a local failure no validated ledger carries. The `32`
        // this replaces was the multi-strand interleave cap kept when every
        // strand started re-entering (4566c4e); for a lone strand it capped
        // the fills-or-slices at 32. #106693003 E9919AA2 (finding 59): a
        // tfPartialPayment XRP→BCHAMP buy mainnet fills through 33 book offers
        // interleaved with 13 AMM slices; round 32 left 497 XRP of the SendMax
        // unspent, 510958.66 delivered under the 737956.89 DeliverMin —
        // tecPATH_PARTIAL against mainnet's tesSUCCESS. (maxOffersToConsider =
        // 1500 offers stepped across the passes, StrandFlow.h:608/778, is not
        // modelled: the passes do not report their offer counts.)
        let max_tries = 1000usize;
        let mut cur_try = 0usize;
        loop {
            // F81 (payments) — rippled loops `while (remainingOut > 0 &&
            // remainingIn > 0)` on ONE gross remainder (`remainingIn -= f.in`,
            // StrandFlow.h:652); the net twin below is our own running
            // subtraction and can hold a one-ULP residue after the strand has
            // consumed the whole SendMax. #106703062 AC58204A: 1.335056517376953e-6
            // BTC spent in full, net residue 1e-21, 1000 rounds of dust →
            // telFAILED_PROCESSING where mainnet stops after one iteration.
            rem_in_gross = ox::iou_amount(rem_in_gross);
            rem_out = ox::iou_amount(rem_out);
            if ox::me_is_zero(rem_out) || ox::me_is_zero(rem_in) || ox::me_is_zero(rem_in_gross) {
                break;
            }
            cur_try += 1;
            if cur_try >= max_tries {
                sandbox.restore_snapshot(snap);
                return TxResult::FailedProcessing;
            }
            let _round = cur_try - 1;
            // ORDER BY UPPER BOUND, SELECT BY FIRST-TO-SURVIVE — rippled's
            // `ActiveStrands::activateNext` sorts candidates by
            // `qualityUpperBound` best-first and DROPS any whose bound misses
            // limitQuality; the loop then flows them in that order and takes
            // the first whose pass is not rejected (StrandFlow.h:647-722).
            //
            // This used to rank on the REALISED quality of a trial pass, the
            // model `03c2cb9` already refuted for offer crossing — and it fails
            // the same way here, because a realised fill depends on SIZE. On
            // #106156904 the strand actually being used degrades as its own
            // pools drain and its fib slice grows (75.380 -> 75.478 -> 75.712)
            // while an untouched rival barely moves (75.591 -> 75.610 ->
            // 75.666), so round 2 switched strands and we ended up delivering
            // across two where mainnet used one: 12 mutations against 7. Every
            // one of those trials was still INSIDE the 75.827 limit — nothing
            // was rejected, we simply re-ranked on a quantity that moves for
            // the wrong reason. An upper bound does not.
            let order: Vec<usize> = if multi && next_set.len() > 1 {
                let mut c: Vec<(usize, ox::Me)> = next_set
                    .iter()
                    .copied()
                    .filter_map(|i| {
                        if i < n_books {
                            // `activateNext` prices the strands with the PREVIOUS
                            // iteration's `multiPath()` — `setMultiPath` runs after
                            // it — so the bound follows `multi_now` as it stands
                            // here, before this round's order resets it (F107).
                            Self::strand_upper_bound(sandbox, &tx.account, &strands[i], &amm_fib, multi_now)
                        } else if i < n_books + n_direct {
                            crate::tx::direct_step::direct_upper_bound(sandbox, &dstrands[i - n_books])
                        } else {
                            Self::mixed_upper_bound(
                                sandbox, &tx.account, &mstrands[i - n_books - n_direct], &amm_fib, multi_now,
                            )
                        }
                        .filter(|ub| thr_me.is_none_or(|t| ox::me_cmp(*ub, t).is_le()))
                        .map(|ub| (i, ub))
                    })
                    .collect();
                c.sort_by(|a, b| ox::me_cmp(a.1, b.1));
                if std::env::var("DX_PAY").is_ok() {
                    eprintln!("DX_PAY   round={_round} order={:?}", c);
                }
                c.into_iter().map(|(i, _)| i).collect()
            } else if multi {
                // Finding 151: a lone pending strand (or none) is activated
                // without evaluating its bound.
                next_set.clone()
            } else {
                vec![0]
            };
            // The candidates that CLEAR the bound are the active strands, which
            // is what `setMultiPath(activeStrands.size() > 1)` reads.
            multi_now = order.len() > 1;
            // rippled trims the round's ask when ONE strand is active and
            // tfLimitQuality is set: `limitOut` solves the strand's composed
            // quality function for the out at which the AVERAGE quality lands
            // exactly on the limit (StrandFlow.h:676-686 "Limit only if one
            // strand"). #106453302 BFC61DEF is the receipt: iter 2's ask is
            // 0.000169…, not the raw 0.139… remainder — asking for the
            // remainder made us take 402660 drops of a level rippled takes
            // 296796 of, and then ACCEPT the 36188322-drop pass rippled
            // rejects. The trimmed flag also gates the 1e-7 judge forgiveness
            // below, exactly as `adjustedRemOut` does at StrandFlow.h:739-741.
            let mut adjusted_ask = false;
            let mut ask_gross = rem_out;
            let mut ask_net = rem_out_net;
            if let Some(t) = thr_me {
                if order.len() == 1 && order[0] < n_books {
                    if let Some((qm, qb)) = Self::strand_quality_fn(
                        sandbox, &tx.account, &strands[order[0]], want_rate, t,
                    ) {
                        if let Some(lim_net) = Self::strand_limit_out(qm, qb, t) {
                            let lim_gross = match want_rate {
                                Some(r) => ox::mul_ratio(lim_net, r as u128, 1_000_000_000, true),
                                None => lim_net,
                            };
                            if ox::me_cmp(lim_gross, rem_out).is_lt() {
                                ask_gross = lim_gross;
                                ask_net = lim_net;
                                adjusted_ask = true;
                                if std::env::var("DX_PAY").is_ok() {
                                    eprintln!(
                                        "DX_PAY   round={_round} limitOut trim ask={ask_gross:?} (net {lim_net:?}) m={qm:?} b={qb:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            let mut applied: Option<(usize, ox::Me, ox::Me, bool, bool)> = None;
            let mut applied_pos: Option<usize> = None;
            for (pos, &i) in order.iter().enumerate() {
                let try_snap = sandbox.snapshot();
                let mut try_fib = amm_fib.clone();
                // `ammContext.clear()` before every strand execution: the used
                // flag describes THIS strand's pass, not a failed earlier one.
                crate::tx::amm_swap::amm_ctx_clear_used();
                // EVERY strand runs ONE PASS, lone or not — the third and last
                // part of the iteration model, and the same `if multi` gate the
                // rounds carried. A pass ends when an offer is CONSUMED and
                // `flow()` re-enters the strand; without that a single-strand
                // payment walks the whole book in one call and MERGES what
                // rippled does in separate iterations.
                //
                // #105795329 ED4F899F is the specimen and it is exact. rippled:
                //   iter 0  66625978 -> 75.87773718335664
                //   iter 1  45860000 -> 52.22817182817183
                //   iter 2  81002057 -> 92.0884060091763
                // We matched iter 0 and then took the CLOB offer AND the next
                // AMM slice in one round (sin 126862057 = 45860000 + 81002057).
                // The maker's RWA line is debited once per iteration and
                // re-rounds at 1e-10 each time, so THREE debits land on
                // 808582.0813613431 and any two-way split lands on …432 —
                // the count is the whole difference.
                // The NET ask is `ask_net` — the want_target-chained twin the
                // totals machinery keeps (rippled's driver holds remainingOut
                // in NET units). DIVIDING the gross pot back can NEVER
                // recover it: want_gross was rounded UP past Amount × rate,
                // so gross/rate sits above the Amount and ANY rounding of the
                // division lands high. #106455116 F04EF64E: want_gross
                // …2710 / 1.0015 = 2588523229458522.2 — the tail run was
                // asked for …22 (Amount is …21), ran its whole rev/fwd one
                // ulp high, and BOTH the intermediary's line (…50 v …49) and
                // the destination (…22 v …21) came out over. rippled's rev
                // starts from remainingOut = …21 verbatim.
                let (sin, sout, in_gross, out_net) = if i < n_books {
                    let (a, b) = Self::strand_pass(
                        tx, dest, &strands[i], rem_in, ask_gross,
                        want_rate.map(|r| (r, ask_net)),
                        spend_rate.map(|_| rem_in_gross), threshold, true,
                        multi_now.then(|| &mut try_fib), sandbox,
                    );
                    // A book strand's sin is GROSS: the spent chooser
                    // (`1f31546`) deliberately trusts the balance DELTA on a
                    // fee-bearing spend leg, and the line loses net + fee.
                    // Classifying it net over-drained BOTH twins — rem_in by
                    // the fee per round, saved_ins by re-grossing a gross —
                    // and the next round's pool slice came up exactly one
                    // fee short. #106455119 EFFA952F: round-0 sin
                    // 1339.4425643283 (rippled's stpIn verbatim) was
                    // subtracted from the 4005.98 NET pot, the round-1 cap
                    // fell to 2674.54, and the AMM slice delivered 14015687
                    // of mainnet's 14031742 — 16055 drops short. (The
                    // blind-dust branch returns the walk's net instead; at
                    // ~5e-22 against a 16-digit pot the unit slip is
                    // sub-representable either way.)
                    (a, b, true, false)
                } else if i < n_books + n_direct {
                    // The direct strand's tail nets the destination — its
                    // target is the NET remainder; its head spends GROSS.
                    let (a, b) = crate::tx::direct_step::direct_strand_pass(
                        sandbox, &dstrands[i - n_books], rem_in_gross, ask_net,
                    );
                    (a, b, true, true)
                } else {
                    use crate::tx::direct_step::SegLayout;
                    let segs = &mstrands[i - n_books - n_direct];
                    let head_run = matches!(segs.first(), Some(SegLayout::Run(_)));
                    let tail_run = matches!(segs.last(), Some(SegLayout::Run(_)));
                    let m_in = if head_run { rem_in_gross } else { rem_in };
                    let m_out = if tail_run { ask_net } else { rem_out };
                    let (a, b) = Self::mixed_strand_pass(
                        tx, dest, segs, m_in, m_out, true,
                        multi_now.then(|| &mut try_fib), sandbox,
                    );
                    (a, b, head_run, tail_run)
                };
                if std::env::var("DX_PAY").is_ok() {
                    eprintln!("DX_PAY   try round={_round} strand={i} sin={sin:?} sout={sout:?}");
                }
                if ox::me_is_zero(sout) {
                    sandbox.restore_snapshot(try_snap);
                    continue;
                }
                // tfLimitQuality at STRAND level, where rippled keeps it. The
                // hops are no longer gated individually (see `hop_thr` in
                // `strand_pass` — their units are not the payment's), so the
                // limit is enforced on what the pass REALISED end to end. A
                // pass that misses it is rolled back and the next candidate
                // tried: rippled's "Path rejected by limitQuality" does
                // `continue`, not `break` (StrandFlow.h:720).
                if let Some(t) = thr_me {
                    // rippled judges `Quality(f.out, f.in)` — the NET result
                    // (its driver accounts NET; a book strand's sout here is
                    // GROSS of want_rate, and dividing by the gross reads the
                    // quality one fee too good: #106453302 BFC61DEF accepted
                    // the 36188322-drop pass rippled rejects at 1756…>limit).
                    // A miss is FORGIVEN inside 1e-7 relative ONLY when the
                    // ask was limitOut-trimmed (`adjustedRemOut`,
                    // StrandFlow.h:735-742) — the trim aims at the limit
                    // exactly, so round-off may land a hair past it.
                    let net_sout = match want_rate {
                        Some(r) if !out_net => {
                            if ox::me_cmp(sout, ask_gross) == std::cmp::Ordering::Equal {
                                ask_net
                            } else {
                                ox::mul_ratio(sout, 1_000_000_000, r as u128, false)
                            }
                        }
                        _ => sout,
                    };
                    let q = ox::me_muldiv(sin, (1_000_000_000_000_000, -15), net_sout, false);
                    if ox::me_cmp(q, t).is_gt() {
                        let forgiven = adjusted_ask && {
                            let diff = ox::me_sub(q, t);
                            ox::me_cmp(
                                ox::me_muldiv(diff, (10_000_000, 0), (1, 0), false),
                                t,
                            )
                            .is_lt()
                        };
                        if !forgiven {
                            if std::env::var("DX_PAY").is_ok() {
                                eprintln!(
                                    "DX_PAY   strand={i} REJECTED by limitQuality q={q:?} thr={t:?}"
                                );
                            }
                            sandbox.restore_snapshot(try_snap);
                            continue;
                        }
                    }
                }
                amm_fib = try_fib;
                // ammContext.update(): one AMM iteration per WINNING round
                // that used any pool (AMMContext.h; FLOWDRIVER-DESIGN §5.1) —
                // in EITHER offer mode, since `AMMOffer::consume` sets the
                // flag for single-path offers too (finding 107). The fib
                // index IS that counter.
                crate::tx::amm_swap::amm_ctx_update();
                amm_fib.iters = crate::tx::amm_swap::amm_ctx_iters();
                amm_fib.used = false;
                applied = Some((i, sin, sout, in_gross, out_net));
                applied_pos = Some(pos);
                break;
            }
            let Some((pick, sin, sout, in_gross, out_net)) = applied else { break };
            // Finding 151: the producing strand and the strands behind it are
            // what the next iteration activates; everything tried before it
            // flowed nothing and is gone.
            if let Some(pos) = applied_pos {
                next_set = order[pos..].to_vec();
            }
            let _ = pick;
            if std::env::var("DX_PAY").is_ok() {
                eprintln!("DX_PAY round={_round} strand={pick} sin={sin:?} sout={sout:?} rem_in={rem_in:?} rem_out={rem_out:?} multi={multi}");
            }
            // Keep BOTH remainders honest whichever kind spent: a direct
            // strand's sin is GROSS, a book strand's is NET, and the input
            // rate converts between them (net = gross / rate, floor; gross =
            // net × rate, ceil — the sender-parts-with side always rounds
            // against the sender). The multiset entries live in the REQUEST
            // units (savedIns in SendMax units, savedOuts in outReq units),
            // and the remainders re-derive from the totals — see `strand_rem`
            // at the declarations above for the rippled receipts.
            let rate = spend_rate.map(|r| r as u128);
            let in_saved = if in_gross {
                sin
            } else {
                match rate {
                    Some(r) => {
                        if ox::me_cmp(sin, rem_in) == std::cmp::Ordering::Equal {
                            // The walk drained its whole net ask, so its
                            // debits summed to the GROSS cap verbatim
                            // (the gross-primary rule) — record that, not a
                            // re-grossing of the net.
                            rem_in_gross
                        } else {
                            ox::mul_ratio(sin, r, 1_000_000_000, true)
                        }
                    }
                    None => sin,
                }
            };
            saved_ins.push(in_saved);
            rem_in_gross = strand_rem(spend0_gross, &mut saved_ins);
            if rate.is_none() {
                // No spend rate: net IS gross — rippled's own remainder.
                rem_in = rem_in_gross;
            } else {
                // ⚠ SPEND-RATE PAYMENTS KEEP THE EXACT NET REMAINDER. rippled
                // carries ONE gross remainder and re-nets it per iteration;
                // our net twin is a running subtraction calibrated by
                // #105795329's sibling spend-rate payment (1.001 issuer —
                // rounding it moved the maker's line one ulp).
                let net = if in_gross {
                    match rate {
                        Some(r) => ox::me_muldiv(sin, (1_000_000_000, 0), (r, 0), false),
                        None => sin,
                    }
                } else {
                    sin
                };
                rem_in = ox::me_sub(rem_in, net);
            }
            let out_saved = if out_net {
                // sout is NET: the shared gross pot counts its gross
                // equivalent, and the delivery lands in the net pot.
                match want_rate {
                    Some(r) => ox::me_muldiv(sout, (r as u128, 0), (1_000_000_000, 0), true),
                    None => sout,
                }
            } else {
                sout
            };
            saved_outs.push(out_saved);
            rem_out = strand_rem(want_gross, &mut saved_outs);
            // The net twin (see the declarations): the same cache rule the
            // walk's beneficiary settlement applied decides the entry.
            let net_r = if out_net {
                sout
            } else {
                match want_rate {
                    Some(r) => {
                        if pick < n_books
                            && ox::me_cmp(sout, ask_gross) == std::cmp::Ordering::Equal
                        {
                            ask_net
                        } else {
                            ox::mul_ratio(sout, 1_000_000_000, r as u128, false)
                        }
                    }
                    None => sout,
                }
            };
            saved_outs_net.push(net_r);
            rem_out_net = strand_rem(want_target, &mut saved_outs_net);
            // rippled's remainingOut lives NET (savedOuts in outReq units) and
            // every iteration REGROSSES it through the last step's rev pass —
            // mulRatio roundUp on the fresh remainder. A gross-subtraction pot
            // is a different number whenever the per-fill grossings cancel a
            // tail the fresh product keeps: #106644326 431CAAFC iter-5 net
            // 1.442557458781219 × 1.001 = …240000|219, regross …240001, pot
            // …240000 exactly — and the UNI/XRPS rev-sizing amplified that
            // ulp ×19000 into the XRPS line. Derive the pot from the net twin.
            if let Some(r) = want_rate {
                rem_out = ox::mul_ratio(rem_out_net, r as u128, 1_000_000_000, true);
            }
            if out_net {
                delivered_direct = ox::signed_add(false, delivered_direct, false, sout).1;
            } else if want_rate.is_some() && pick < n_books {
                // The walk already credited the destination `net_r` (the
                // benef_net settlement); the tx-level figure folds SORTED
                // post-loop — rippled's actualOut.
                book_net.push(net_r);
            } else {
                delivered = ox::signed_add(false, delivered, false, sout).1;
            }
            // Spend is unmeasurable when the sender ISSUES the currency it is
            // spending — there is no line to difference. `rem_in` would then
            // never fall and the loop would keep buying against a SendMax it
            // cannot account for, so stop after this round instead.
            if ox::me_is_zero(sin) {
                break;
            }
            if std::env::var("DX_PAY").is_ok() {
                eprintln!("DX_PAY round strand={pick} spent={sin:?} got={sout:?} rem_in={rem_in:?} rem_out={rem_out:?}");
            }
        }
        // The strand's last step redeems the delivered IOU from whoever gave
        // it up and re-issues it to the destination, and that step charges the
        // issuer's TransferRate: `DirectStepI::qualitiesSrcIssues` sets
        // srcQOut = transferRate(issuer) whenever the previous step redeems
        // (DirectStep.cpp:765), which for a payment is always — BookStep
        // reports DebtDirection::Redeems while ownerPaysTransferFee is false
        // (BookStep.cpp:146). The destination receives out/rate
        // (DirectStep.cpp:646) and the fee is destroyed rather than paid to
        // anyone, so only the receiving line is trimmed.
        //
        // #105775455 58FEFF8C: the CHIT issuer charges 1.9, so the
        // 1292441.5236935 our crossing pulled out of the pool is really
        // 680232.38 delivered — under the transaction's DeliverMin of
        // 804373.673120612, which is exactly mainnet's tecPATH_PARTIAL.
        //
        // ⚠ The input fee is NOT charged here any more. The WALK owns it —
        // every fill site debits the sender the GROSS directly (CLOB
        // move_leg_gross, AMM consume/consume_fib in_gross_rate, DirectHop
        // rates inside the strand), so a settlement top-up here charged the
        // rate a SECOND time. #106455038 6FC71FB6 (full-ledger replay) is
        // the exact specimen: two strands hand their pools 17.8207228111
        // USDT net, mainnet debits the sender 17.8385435339 = net x 1.001
        // (the Usdt gateway's rate, once), and this site made it
        // net x 1.001^2 = 17.8563820774. The rem_in bookkeeping above stays
        // in NET terms — only the LINE top-up was the double.
        let delivered = match want_rate {
            Some(rate) if !ox::me_is_zero(delivered) => {
                // rippled's fwd DirectStep never round-trips gross→net: it
                // REUSES the rev cache (DirectStep.cpp:492 — the same cache
                // the debt-direction fix drinks from), so when fwd hands the
                // step exactly the in the rev sized, the destination is
                // credited the rev's srcToDst — the NET target — verbatim.
                // #106455063 69530872: Amount 2.212844833396933 × 1.002 =
                // …726866 → mulRatio-up …728 (rippled's own rev in, shim
                // STEPREV receipt); dividing …728 back lands …934, one ulp
                // over the Amount, and the dest line read …063 for mainnet's
                // …062. Full delivery of the sized gross = the cache hit.
                //
                // A PARTIAL delivery misses the cache and rippled recomputes
                // out/rate via mulRatio(…, roundUp = false) — half-even
                // NEAREST, no bump (IOUAmount.cpp:182). The exact floor sat
                // one ulp low whenever the dropped fraction was above one
                // half: #106455062 AF6A3460 (full-ledger replay) —
                // 4369.93132409 SOLO gross over the 1.0001 issuer must
                // deliver …652540, floor said …652539.
                let net = if ox::me_cmp(delivered, want_gross) == std::cmp::Ordering::Equal {
                    want_target
                } else {
                    ox::mul_ratio(delivered, 1_000_000_000, rate as u128, false)
                };
                ox::line_adjust(sandbox, dest, &want_leg, ox::me_sub(delivered, net), false);
                net
            }
            _ => delivered,
        };
        // Book rounds under a want rate already credited the destination NET
        // per iteration; their tx-level total is the SORTED-ascending fold —
        // rippled's actualOut (StrandFlow.h:801). #106453302: the fold gives
        // DeliveredAmount …263 while the line's chronological chain reads
        // …264 — both are mainnet's numbers, from the same set.
        let delivered = ox::signed_add(false, delivered, false, strand_sum(&mut book_net)).1;
        // Direct-strand deliveries are already net — no trim, no division.
        let delivered = ox::signed_add(false, delivered, false, delivered_direct).1;
        // …and the FINAL figure is rippled's actualOut VERBATIM: the sorted
        // 16-digit fold over the whole savedOuts mirror, never an
        // exact-width sum. #106455221 34F37CD0 (no-partial, two strands,
        // four iterations): the exact chain lands 80.905362410300298 —
        // 2e-15 short of the 80.9053624103003 Amount (round 2 an ulp low,
        // round 3 the 16-digit remainder) — and the !partial judge read
        // tecPATH_PARTIAL where mainnet's fold hits the Amount exactly and
        // delivers. The pots above still feed the mixed trim; only the
        // judged/delivered figure is the fold.
        let _ = delivered;
        let delivered = strand_sum(&mut saved_outs_net);
        if ox::me_is_zero(delivered) {
            sandbox.restore_snapshot(snap);
            // The DRIVER's ending (StrandFlow.h:800-840): a flow that moved
            // nothing is tecPATH_PARTIAL unless tfPartialPayment is set —
            // tecPATH_DRY needs `partialPayment && actualOut == 0`.
            // #106071067 C98FD7C2: rippled's own trace is "All strands dry.
            // Total flow: in: 0 out: 0" and mainnet returns tecPATH_PARTIAL;
            // we said DRY. (Pre-loop refusals — no line, no strands — keep
            // their calibrated DRY: rippled fails those before the driver.)
            return if partial { TxResult::PathDry } else { TxResult::PathPartial };
        }
        if !partial && ox::me_cmp(delivered, want0).is_lt() {
            sandbox.restore_snapshot(snap);
            return TxResult::PathPartial;
        }
        // With tfPartialPayment, DeliverMin is the delivery floor: falling
        // short fails fee-only with tecPATH_PARTIAL.
        if partial {
            if let Some(dm) = tx
                .fields
                .get("DeliverMin")
                .and_then(crate::ledger::keylet::amount_mant_exp)
            {
                if ox::me_cmp(delivered, dm).is_lt() {
                    sandbox.restore_snapshot(snap);
                    return TxResult::PathPartial;
                }
            }
        }
        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::{apply_modifications, Sandbox};
    use crate::ledger::state::LedgerState;
    use crate::ledger::transactor::apply_common;
    use xrpl_core::types::Hash256;

    fn make_state() -> LedgerState {
        let header = LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        LedgerState::new_unverified(header)
    }

    fn add_account(state: &mut LedgerState, id: &[u8; 20], balance: u64, seq: u32) {
        let acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": seq,
            "OwnerCount": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
    }

    /// Mainnet #106136429 `6FF692D40C4F`: destination carries
    /// `lsfRequireDestTag`, the payment has no `DestinationTag`, AND the
    /// sender is short by 20 drops — both conditions genuinely hold, so the
    /// ledger only distinguishes the two by CHECK ORDER.
    ///
    /// rippled settles it in `Payment::preclaim`: tecNO_DST (337),
    /// tecNO_DST_INSUF_XRP (360), tecDST_TAG_NEEDED (372) — while the funding
    /// check is in `doApply` (627). Every destination check precedes funding.
    /// Ours ran the funding check first and returned tecUNFUNDED_PAYMENT.
    #[test]
    fn a_destination_requiring_a_tag_is_rejected_before_the_sender_is_checked_for_funds() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state();
        // The real shape: amount + reserve is one fee more than the balance.
        let amount = 2_537_243u64;
        add_account(&mut state, &alice, amount + 1_000_000 - 20, 1);
        add_account(&mut state, &bob, 425_153_027_195, 1);
        // lsfRequireDestTag on the destination.
        {
            let key = keylet::account_root_key(&bob);
            let mut acct: serde_json::Value =
                serde_json::from_slice(&state.state_map.lookup(&key).unwrap().to_vec()).unwrap();
            acct["Flags"] = serde_json::Value::Number(0x0002_0000u64.into());
            state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        }

        let sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "Payment".to_string(),
            fee: 20,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(bob),
                "Amount": amount.to_string(),
                "Flags": 0u64,
            }),
        };
        assert_eq!(
            PaymentTransactor.preclaim(&tx, &sandbox),
            TxResult::DstTagNeeded,
            "the destination check comes first; funding is doApply's job"
        );
    }

    fn read_balance(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        v["Balance"].as_str().unwrap().parse().unwrap()
    }

    fn read_balance_from_state(state: &LedgerState, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = state.state_map.lookup(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(data).unwrap();
        v["Balance"].as_str().unwrap().parse().unwrap()
    }

    fn payment_tx(sender: [u8; 20], dest: [u8; 20], amount: u64, fee: u64, seq: u32) -> TxFields {
        TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee,
            sequence: seq,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": amount.to_string(),
            }),
        }
    }

    #[test]
    fn preflight_valid_payment() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::Success);
    }

    #[test]
    fn preflight_zero_amount() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 0, 12, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::BadAmount);
    }

    #[test]
    fn preflight_zero_fee() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let tx = payment_tx(sender, dest, 1_000_000, 0, 1);
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::BadFee);
    }

    #[test]
    fn preflight_no_destination() {
        let sender = [0x01u8; 20];
        let tx = TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 1,
            last_ledger_seq: None,
            ticket_seq: None,
            fields: serde_json::json!({"Amount": "1000000"}),
        };
        assert_eq!(PaymentTransactor.preflight(&tx), TxResult::Malformed);
    }

    #[test]
    fn preclaim_sender_not_found() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let state = make_state(); // no accounts
        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1);
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::NoAccount);
    }

    #[test]
    fn preclaim_insufficient_balance() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 500_000, 1); // only 0.5 XRP
        add_account(&mut state, &dest, 50_000_000, 1);

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 1); // needs 1M + 12
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::UnfundedPayment);
    }

    #[test]
    fn preclaim_new_dest_below_reserve() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        // dest doesn't exist; 0.5 XRP is below the 1 XRP base reserve

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 500_000, 12, 1);
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::NoDstInsufXrp);
    }

    #[test]
    fn preclaim_past_sequence() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 10); // seq=10
        add_account(&mut state, &dest, 50_000_000, 1);

        let sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 1_000_000, 12, 5); // tx seq=5 < account seq=10
        assert_eq!(PaymentTransactor.preclaim(&tx, &sandbox), TxResult::PastSeq);
    }

    #[test]
    fn do_apply_xrp_to_existing_account() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        add_account(&mut state, &dest, 10_000_000, 1);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            let tx = payment_tx(sender, dest, 5_000_000, 12, 1);

            // Run common (deducts fee, increments seq)
            let common = apply_common(&tx, &mut sandbox);
            assert_eq!(common, TxResult::Success);

            // Run payment apply
            let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
            assert_eq!(result, TxResult::Success);

            // Verify in sandbox
            // Sender: 50M - 12(fee) - 5M(amount) = 44,999,988
            assert_eq!(read_balance(&sandbox, &sender), 44_999_988);
            // Dest: 10M + 5M = 15M
            assert_eq!(read_balance(&sandbox, &dest), 15_000_000);

            sandbox.into_modifications()
        };

        // Commit and verify state changed
        apply_modifications(&mut state, mods).unwrap();
        assert_eq!(read_balance_from_state(&state, &sender), 44_999_988);
        assert_eq!(read_balance_from_state(&state, &dest), 15_000_000);
    }

    #[test]
    fn do_apply_creates_new_account() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 100_000_000, 1);
        // dest doesn't exist

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            let tx = payment_tx(sender, dest, 20_000_000, 12, 1); // 20 XRP > 10 XRP reserve

            let common = apply_common(&tx, &mut sandbox);
            assert_eq!(common, TxResult::Success);

            let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
            assert_eq!(result, TxResult::Success);

            // New account should exist with balance = amount
            assert_eq!(read_balance(&sandbox, &dest), 20_000_000);
            // Sender: 100M - 12 - 20M = 79,999,988
            assert_eq!(read_balance(&sandbox, &sender), 79_999_988);

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();

        // Verify new account was created in state
        let dest_key = keylet::account_root_key(&dest);
        let dest_data = state.state_map.lookup(&dest_key).expect("dest account should exist");
        let dest_obj: serde_json::Value = serde_json::from_slice(dest_data).unwrap();
        assert_eq!(dest_obj["LedgerEntryType"], "AccountRoot");
        // DeletableAccounts: a fresh account's Sequence = the creating
        // ledger's sequence (header is the PARENT, so +1).
        let expect_seq = state.header.sequence + 1;
        assert_eq!(dest_obj["Sequence"], expect_seq);
        assert_eq!(dest_obj["Balance"].as_str().unwrap(), "20000000");
    }

    #[test]
    fn do_apply_new_account_below_reserve_fails() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 100_000_000, 1);

        let mut sandbox = Sandbox::new(&state);
        let tx = payment_tx(sender, dest, 500_000, 12, 1); // 0.5 XRP < 1 XRP reserve

        let result = PaymentTransactor.do_apply(&tx, &mut sandbox);
        assert_eq!(result, TxResult::NoDstInsufXrp);
    }

    #[test]
    fn full_pipeline_preflight_preclaim_apply() {
        let sender = [0xAAu8; 20];
        let dest = [0xBBu8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 200_000_000, 5);
        add_account(&mut state, &dest, 50_000_000, 1);

        let tx = payment_tx(sender, dest, 25_000_000, 15, 5);
        let transactor = PaymentTransactor;

        // Full pipeline
        assert_eq!(transactor.preflight(&tx), TxResult::Success);

        let mods = {
            let mut sandbox = Sandbox::new(&state);
            assert_eq!(transactor.preclaim(&tx, &sandbox), TxResult::Success);
            assert_eq!(apply_common(&tx, &mut sandbox), TxResult::Success);
            assert_eq!(transactor.do_apply(&tx, &mut sandbox), TxResult::Success);

            // Sender: 200M - 15(fee) - 25M = 174,999,985
            assert_eq!(read_balance(&sandbox, &sender), 174_999_985);
            // Dest: 50M + 25M = 75M
            assert_eq!(read_balance(&sandbox, &dest), 75_000_000);
            // Sender sequence: 5 → 6
            let sk = keylet::account_root_key(&sender);
            let sd = sandbox.read(&sk).unwrap();
            let sv: serde_json::Value = serde_json::from_slice(&sd).unwrap();
            assert_eq!(sv["Sequence"], 6);

            sandbox.into_modifications()
        };

        apply_modifications(&mut state, mods).unwrap();
    }

    /// Seed a book where `issuer` (as maker) sells 5 USD for 5 XRP, then
    /// return (state, taker, issuer).
    ///
    /// The taker is given an empty USD line: a payment can only deliver an
    /// IOU into a line that already exists (DirectStep.cpp:423), so without
    /// one every payment below would be tecPATH_DRY before reaching the
    /// behaviour under test. Crossing the book as an OfferCreate would open
    /// the line; paying does not.
    /// A payment's final issuer→dest step is capped at what the destination can
    /// still receive — `creditLimit(dst,issuer) − heldByDst` floored at zero
    /// (DirectStepI::maxPaymentFlow, DirectStep.cpp:487) — so a destination at
    /// or over its own trust limit receives nothing and the strand is dry.
    /// `apply_path_payment` has had this since `04a1586`; the direct route had
    /// not. #105828788 6D342FDE / #105896643 D3FEA91C pay JUST1 to holders of
    /// 1.002263 and 1.040687 against a limit of 0; #105855167 3F56723B pays
    /// YZZUF into a default-state line, limit 0 and balance 0.
    #[test]
    fn a_direct_iou_payment_is_dry_when_the_destination_cannot_receive() {
        let sender = [0x01u8; 20];
        let dest = [0x02u8; 20];
        let issuer = [0x03u8; 20];
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();

        let mut state = make_state();
        for id in [&sender, &dest, &issuer] {
            add_account(&mut state, id, 50_000_000, 1);
        }
        let mut put_line = |state: &mut LedgerState, who: &[u8; 20], bal: &str, limit: &str| {
            let (lo, hi) = if who < &issuer { (*who, issuer) } else { (issuer, *who) };
            // `limit` is the holder's own side; the issuer never extends credit.
            let (lo_lim, hi_lim) = if who < &issuer { (limit, "0") } else { ("0", limit) };
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur),
                            "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &issuer { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": lo_lim},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": hi_lim},
            });
            state
                .state_map
                .insert(keylet::ripple_state_key(who, &issuer, &cur), serde_json::to_vec(&line).unwrap())
                .unwrap();
        };
        put_line(&mut state, &sender, "100", "1000000");
        // Destination trusts the issuer for NOTHING — the default-state line
        // that `account_lines` does not even report.
        put_line(&mut state, &dest, "0", "0");

        let tx = TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1"},
            }),
        };
        assert_eq!(
            PaymentTransactor.do_apply(&tx, &mut Sandbox::new(&state)),
            TxResult::PathDry,
            "a destination that can receive nothing makes the strand dry",
        );

        // Raise only the destination's limit — the same payment now lands.
        put_line(&mut state, &dest, "0", "1000000");
        assert_eq!(
            PaymentTransactor.do_apply(&tx, &mut Sandbox::new(&state)),
            TxResult::Success,
            "and room on the line is all that was missing",
        );
    }

    fn state_with_usd_book() -> (LedgerState, [u8; 20], [u8; 20]) {
        let taker = [0x01u8; 20];
        let issuer = [0x03u8; 20];
        let mut state = make_state();
        add_account(&mut state, &taker, 50_000_000, 1);
        add_account(&mut state, &issuer, 50_000_000, 1);
        {
            let cur = crate::tx::offer::amount_currency20(
                &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
            )
            .unwrap();
            let (lo, hi) = if taker < issuer { (taker, issuer) } else { (issuer, taker) };
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState",
                "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur),
                            "issuer": "0000000000000000000000000000000000000000",
                            "value": "0"},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state
                .state_map
                .insert(keylet::ripple_state_key(&taker, &issuer, &cur), serde_json::to_vec(&line).unwrap())
                .unwrap();
        }

        let mut sandbox = Sandbox::new(&state);
        let offer_tx = TxFields {
            account: issuer,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "5000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
            }),
        };
        assert_eq!(
            crate::tx::offer::OfferCreateTransactor.do_apply(&offer_tx, &mut sandbox),
            TxResult::Success
        );
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();
        (state, taker, issuer)
    }

    /// A multi-strand payment slices its pools instead of draining one strand.
    ///
    /// `AMMContext::multiPath()` is `activeStrands.size() > 1`, and under it
    /// `AMMLiquidity::getOffer` hands back a FIB SLICE off the pool's initial
    /// balances rather than `maxOffer` — so one iteration of a strand moves a
    /// slice, not the whole request, and `flow()` re-picks the best strand for
    /// the next one (StrandFlow.h:640-756). The counter is flow-wide and the
    /// slices grow 1,1,2,3,5,8,13.
    ///
    /// Here the AAA strand is cheaper to begin with, so it wins the early
    /// rounds; its pool is shallow, so as the slices grow it prices itself out
    /// and the direct XRP->BBB pool takes over. Sized by `maxOffer` instead,
    /// the AAA strand answers the WHOLE request in one pass and the direct
    /// pool is never touched at all — which is exactly what
    /// #105912291 2AE3693EF556 did: everything down one strand, consuming
    /// Offer 3A3053B3 outright (mainnet only Modifies it) and spilling into a
    /// second offer, for 10 mutations against 9.
    #[test]
    fn a_multi_strand_payment_slices_its_pools_across_both_strands() {
        let src = [0x01u8; 20];
        let dst = [0x08u8; 20];
        let iss = [0x02u8; 20];
        let mkr = [0x04u8; 20];
        let p_dir = [0x06u8; 20]; // XRP  / BBB
        let p_via = [0x07u8; 20]; // AAA / BBB
        let (mut ca, mut cb) = ([0u8; 20], [0u8; 20]);
        ca[12..15].copy_from_slice(b"AAA");
        cb[12..15].copy_from_slice(b"BBB");

        let mut state = make_state();
        add_account(&mut state, &src, 50_000_000, 1);
        for id in [&dst, &iss, &mkr, &p_via] {
            add_account(&mut state, id, 50_000_000, 1);
        }
        add_account(&mut state, &p_dir, 1_100_000, 1); // the direct pool's XRP side
        let mut line = |state: &mut LedgerState, who: &[u8; 20], cur: &[u8; 20], bal: &str| {
            let (lo, hi) = if who < &iss { (*who, iss) } else { (iss, *who) };
            let v = serde_json::json!({
                "LedgerEntryType": "RippleState", "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur), "issuer": "0000000000000000000000000000000000000000",
                            "value": if who < &iss { bal.to_string() } else { format!("-{bal}") }},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "100000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "100000000"},
            });
            state.state_map.insert(keylet::ripple_state_key(who, &iss, cur), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        line(&mut state, &mkr, &ca, "10000");   // the maker's AAA to sell
        line(&mut state, &p_via, &ca, "1000");  // AAA/BBB pool, 1000 / 1000
        line(&mut state, &p_via, &cb, "1000");
        line(&mut state, &p_dir, &cb, "1000");  // XRP/BBB pool, 1.1 XRP / 1000
        line(&mut state, &dst, &cb, "0");       // the destination's BBB line

        let xrp_leg = crate::tx::offer::leg_of(&serde_json::json!("1")).unwrap();
        let a_leg = crate::tx::offer::leg_of(
            &serde_json::json!({"currency":"AAA","issuer":hex::encode(iss),"value":"1"})).unwrap();
        let b_leg = crate::tx::offer::leg_of(
            &serde_json::json!({"currency":"BBB","issuer":hex::encode(iss),"value":"1"})).unwrap();
        for (acct, l, r) in [(p_dir, &xrp_leg, &b_leg), (p_via, &a_leg, &b_leg)] {
            let amm = serde_json::json!({
                "LedgerEntryType": "AMM", "Account": hex::encode(acct), "TradingFee": 0,
            });
            let k = keylet::amm_key(&l.cur, &l.issuer, &r.cur, &r.issuer);
            state.state_map.insert(k, serde_json::to_vec(&amm).unwrap()).unwrap();
        }

        // The XRP->AAA book: 10000 AAA at 1000 drops each.
        let mut sandbox = Sandbox::new(&state);
        let mk = TxFields {
            account: mkr, tx_type: "OfferCreate".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "10000000",
                "TakerGets": {"currency": "AAA", "issuer": hex::encode(iss), "value": "10000"},
            }),
        };
        assert_eq!(crate::tx::offer::OfferCreateTransactor.do_apply(&mk, &mut sandbox), TxResult::Success);
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        let dir_before = read_bbb(&Sandbox::new(&state), &p_dir, &cb);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: src, tx_type: "Payment".to_string(), fee: 12, sequence: 2,
            ticket_seq: None, last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dst),
                "Amount": {"currency": "BBB", "issuer": hex::encode(iss), "value": "300"},
                "SendMax": "1000000",
                "Flags": 131072u64, // tfPartialPayment
                "Paths": [[{"type": 48, "currency": "AAA", "issuer": hex::encode(iss)}]],
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let dir_after = read_bbb(&sandbox, &p_dir, &cb);
        // Sized by fib slices the strands interleave and the direct pool gives
        // up ~130 BBB across the rounds it wins. Sized by `maxOffer` the AAA
        // strand answers the whole 300 in ONE pass and the direct pool only
        // ever sees a 2e-10 dust remainder, so the bar is a magnitude, not a
        // mere inequality.
        assert!(
            dir_before - dir_after > 1.0,
            "the direct pool must CARRY part of the payment, not just mop up \
             dust: {dir_before} -> {dir_after}",
        );
    }

    /// The absolute BBB a pool account holds.
    fn read_bbb(sandbox: &Sandbox, who: &[u8; 20], cur: &[u8; 20]) -> f64 {
        let iss = [0x02u8; 20];
        sandbox
            .read(&keylet::ripple_state_key(who, &iss, cur))
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["Balance"]["value"].as_str().and_then(|t| t.parse::<f64>().ok()))
            .map(f64::abs)
            .unwrap_or(0.0)
    }

    /// Arb-style conversion: sentinel-max Amount + tfPartialPayment must
    /// deliver whatever SendMax buys off the book (rippled imposes no
    /// SendMax/Amount quality bound without tfLimitQuality).
    #[test]
    fn path_payment_partial_sentinel_amount_delivers() {
        let (state, taker, issuer) = state_with_usd_book();
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(taker),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1000000000"},
                "SendMax": "5000000",
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);
        // Taker spent the 5 XRP on the book.
        assert_eq!(read_balance(&sandbox, &taker), 45_000_000);
        // Taker now holds the acquired USD on its (pre-existing) line.
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        let line = sandbox.read(&keylet::ripple_state_key(&taker, &issuer, &cur)).unwrap();
        let line: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_ne!(line["Balance"]["value"].as_str().unwrap(), "0");
    }

    /// rippled `DirectStepI::maxPaymentFlow` (DirectStep.cpp:487): a
    /// destination already holding at or above its trust limit for the
    /// delivered currency can receive NOTHING, so a cross-currency payment
    /// that would otherwise cross the book to fill it is tecPATH_DRY, fee only
    /// — mainnet #105740164 B7C6328C, the arb bot whose JUST1 line is limit 0
    /// while already holding 1.000383. Raising that same line's limit lets the
    /// identical payment deliver, proving the book liquidity was present and it
    /// is the trust ceiling — not a dry book — that blocks the fill.
    /// A payment delivering a currency the last hop already holds under a
    /// DIFFERENT issuer gets no terminal book from `toStrand` — it gets
    /// `DirectStepI(hopIssuer -> deliverIssuer)`, a ripple between the two
    /// gateways (PaySteps.cpp:289-300, :477). Only the TERMINAL transition
    /// is account-mediated; an issuer-only change between two explicit path
    /// elements is still `make_BookStepII`.
    #[test]
    fn terminal_issuer_change_is_a_gateway_ripple_not_a_book() {
        let usd = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode([0x03u8; 20]), "value": "1"}),
        )
        .unwrap();
        let eur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "EUR", "issuer": hex::encode([0x03u8; 20]), "value": "1"}),
        )
        .unwrap();
        let iou = |cur: [u8; 20], issuer: u8| crate::tx::offer::Leg {
            xrp: false,
            cur,
            issuer: [issuer; 20],
        };
        let xrp = crate::tx::offer::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] };

        // #106336831 619718E8's shape: XRP -> USD.gwA -> USD.gwB. The last
        // transition changes only the issuer, so rippled needs a line BETWEEN
        // the gateways and drops the path without one.
        let (a, b) = (iou(usd, 0x0a), iou(usd, 0x0b));
        assert!(PaymentTransactor::terminal_is_ripple_step(&[&xrp, &a, &b]));
        // Two-leg default path, same rule: src -> sendMaxIssuer ->
        // deliverIssuer -> dst is three direct steps, not a book.
        assert!(PaymentTransactor::terminal_is_ripple_step(&[&a, &b]));

        // A real currency change terminates in a book — leave it alone.
        let e = iou(eur, 0x0b);
        assert!(!PaymentTransactor::terminal_is_ripple_step(&[&xrp, &a, &e]));
        // ...as does anything crossing to or from XRP.
        assert!(!PaymentTransactor::terminal_is_ripple_step(&[&a, &xrp]));
        assert!(!PaymentTransactor::terminal_is_ripple_step(&[&xrp, &a]));
        // An issuer-only change in the MIDDLE stays a book: both ends are
        // offer elements there, so only the final pair is inspected.
        assert!(!PaymentTransactor::terminal_is_ripple_step(&[&a, &b, &e]));
    }

    #[test]
    fn path_payment_dry_when_destination_at_trust_limit() {
        let taker = [0x01u8; 20];
        let issuer = [0x03u8; 20];
        assert!(taker < issuer, "taker must be the LOW account so LowLimit is its limit");

        // A USD book (issuer sells 5 USD for 5 XRP) plus a taker USD line that
        // already holds 1 USD, its own limit set to `taker_limit`.
        let build = |taker_limit: &str| -> LedgerState {
            let mut state = make_state();
            add_account(&mut state, &taker, 50_000_000, 1);
            add_account(&mut state, &issuer, 50_000_000, 1);
            let cur = crate::tx::offer::amount_currency20(
                &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
            )
            .unwrap();
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState",
                "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur),
                            "issuer": "0000000000000000000000000000000000000000",
                            "value": "1"},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(taker), "value": taker_limit},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(issuer), "value": "1000000"},
            });
            state
                .state_map
                .insert(keylet::ripple_state_key(&taker, &issuer, &cur), serde_json::to_vec(&line).unwrap())
                .unwrap();
            let mut sandbox = Sandbox::new(&state);
            let offer_tx = TxFields {
                account: issuer,
                tx_type: "OfferCreate".to_string(),
                fee: 12,
                sequence: 2,
                ticket_seq: None,
                last_ledger_seq: None,
                fields: serde_json::json!({
                    "TakerPays": "5000000",
                    "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                }),
            };
            assert_eq!(
                crate::tx::offer::OfferCreateTransactor.do_apply(&offer_tx, &mut sandbox),
                TxResult::Success
            );
            let mods = sandbox.into_modifications();
            apply_modifications(&mut state, mods).unwrap();
            state
        };

        let pay = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(taker),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1000000000"},
                "SendMax": "5000000",
                "Flags": 131072u64, // tfPartialPayment
            }),
        };

        // Limit 0 while already holding 1 USD ⇒ receives nothing ⇒ dry, and the
        // sender spends none of its XRP (do_apply mutates nothing).
        let s_capped = build("0");
        let mut sb = Sandbox::new(&s_capped);
        assert_eq!(PaymentTransactor.do_apply(&pay, &mut sb), TxResult::PathDry);
        assert_eq!(read_balance(&sb, &taker), 50_000_000);

        // Same book, same payment, generous limit ⇒ the book fills it and the
        // sender spends its 5 XRP.
        let s_open = build("1000000");
        let mut sb2 = Sandbox::new(&s_open);
        assert_eq!(PaymentTransactor.do_apply(&pay, &mut sb2), TxResult::Success);
        assert_eq!(read_balance(&sb2, &taker), 45_000_000);
    }

    /// A direct IOU payment can only be delivered to a destination that
    /// already trusts the issuer — rippled never opens the receiver's trust
    /// line. No line ⇒ tecPATH_DRY, not a phantom-created line + delivery
    /// (mainnet #105797892 41D13D: dest held no BXE line).
    #[test]
    fn direct_iou_payment_requires_destination_line() {
        let sender = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let dest_noline = [0x03u8; 20];
        let dest_trusts = [0x05u8; 20];
        let mut state = make_state();
        add_account(&mut state, &sender, 50_000_000, 1);
        add_account(&mut state, &issuer, 50_000_000, 1);
        add_account(&mut state, &dest_noline, 50_000_000, 1);
        add_account(&mut state, &dest_trusts, 50_000_000, 1);

        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        // Insert a trust line `a`↔issuer with `a` holding +value USD.
        let add_line = |state: &mut LedgerState, a: &[u8; 20], value: &str| {
            let key = keylet::ripple_state_key(a, &issuer, &cur);
            let (lo, hi) = if a < &issuer { (*a, issuer) } else { (issuer, *a) };
            let low_bal = if a < &issuer { value.to_string() } else { format!("-{value}") };
            let line = serde_json::json!({
                "LedgerEntryType": "RippleState",
                "Flags": 0x0001_0000u64,
                "Balance": {"currency": hex::encode_upper(cur),
                            "issuer": "0000000000000000000000000000000000000000",
                            "value": low_bal},
                "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "1000000"},
                "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "1000000"},
            });
            state.state_map.insert(key, serde_json::to_vec(&line).unwrap()).unwrap();
        };
        // Sender holds 100 USD to spend; only dest_trusts has a USD line.
        add_line(&mut state, &sender, "100");
        add_line(&mut state, &dest_trusts, "0");

        let pay = |dest: [u8; 20]| TxFields {
            account: sender,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(dest),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "25"},
            }),
        };

        // Destination without a USD line: dry, and no line phantom-created.
        let mut sb = Sandbox::new(&state);
        assert_eq!(PaymentTransactor.do_apply(&pay(dest_noline), &mut sb), TxResult::PathDry);
        assert!(!sb.exists(&keylet::ripple_state_key(&dest_noline, &issuer, &cur)));

        // Destination that already trusts the issuer: delivered.
        let mut sb2 = Sandbox::new(&state);
        assert_eq!(PaymentTransactor.do_apply(&pay(dest_trusts), &mut sb2), TxResult::Success);
    }

    /// Mainnet tx AAA6EB389D3A… (ledger 105035381): when the Destination IS
    /// the issuer of the delivered currency, the strand's output goes
    /// straight there and the IOU is redeemed — rippled never materializes a
    /// trust line for the sender in between. We used to route through the
    /// sender, leaving a zero-balance line (plus its directory pages and
    /// OwnerCount bumps) that mainnet's meta has no trace of.
    #[test]
    fn path_payment_to_issuer_creates_no_sender_line() {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let maker = [0x04u8; 20];
        let mut state = make_state();
        add_account(&mut state, &taker, 50_000_000, 1);
        add_account(&mut state, &maker, 50_000_000, 1);
        add_account(&mut state, &issuer, 50_000_000, 1);

        // Maker holds USD and sells 5 USD for 5 XRP.
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        let mkey = keylet::ripple_state_key(&maker, &issuer, &cur);
        let (lo, hi) = if maker < issuer { (maker, issuer) } else { (issuer, maker) };
        let line = serde_json::json!({
            "LedgerEntryType": "RippleState",
            "Flags": 0x0001_0000u64,
            "Balance": {"currency": hex::encode_upper(cur),
                        "issuer": "0000000000000000000000000000000000000000",
                        "value": if maker < issuer { "100" } else { "-100" }},
            "LowLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(lo), "value": "0"},
            "HighLimit": {"currency": hex::encode_upper(cur), "issuer": hex::encode(hi), "value": "0"},
        });
        state.state_map.insert(mkey, serde_json::to_vec(&line).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let offer_tx = TxFields {
            account: maker,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": "5000000",
                "TakerGets": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
            }),
        };
        assert_eq!(
            crate::tx::offer::OfferCreateTransactor.do_apply(&offer_tx, &mut sandbox),
            TxResult::Success
        );
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Pay USD to the ISSUER, sourcing it from the book with XRP.
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(issuer),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1000000000"},
                "SendMax": "5000000",
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // No trust line for the sender: the IOU never rests with them.
        assert!(!sandbox.exists(&keylet::ripple_state_key(&taker, &issuer, &cur)));
        // The maker's holding fell — that IS the redemption.
        let ml = crate::tx::offer::json_at(&sandbox, &mkey).expect("maker line");
        assert_ne!(ml["Balance"]["value"].as_str(), Some("100"));
    }

    /// A sender holding NONE of the SendMax currency is a dry strand no
    /// matter how deep the book is: fee-only tecPATH_DRY (mainnet arb bots
    /// hit this constantly — SendMax in a currency they hold zero of).
    #[test]
    fn path_payment_unfunded_sendmax_is_dry() {
        let taker = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let maker = [0x04u8; 20];
        let mut state = make_state();
        add_account(&mut state, &taker, 50_000_000, 1);
        add_account(&mut state, &maker, 50_000_000, 1);

        // Maker sells 5 XRP for 5 USD — plenty of liquidity for the taker.
        let mut sandbox = Sandbox::new(&state);
        let offer_tx = TxFields {
            account: maker,
            tx_type: "OfferCreate".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "TakerPays": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "TakerGets": "5000000",
            }),
        };
        assert_eq!(
            crate::tx::offer::OfferCreateTransactor.do_apply(&offer_tx, &mut sandbox),
            TxResult::Success
        );
        let mods = sandbox.into_modifications();
        apply_modifications(&mut state, mods).unwrap();

        // Taker spends USD they do not hold (no trust line at all).
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(taker),
                "Amount": "100000000000000000",
                "SendMax": {"currency": "USD", "issuer": hex::encode(issuer), "value": "5"},
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::PathDry);
        // Fee-only: taker's XRP untouched, maker's offer untouched.
        assert_eq!(read_balance(&sandbox, &taker), 50_000_000);
        assert!(sandbox.exists(&keylet::offer_key(&maker, 2)));
    }

    /// DeliverMin above what the book can produce: fee-only tecPATH_PARTIAL,
    /// not tecPATH_DRY, and no state mutation survives.
    #[test]
    fn path_payment_deliver_min_short_fails_partial() {
        let (state, taker, issuer) = state_with_usd_book();
        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: taker,
            tx_type: "Payment".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Destination": hex::encode(taker),
                "Amount": {"currency": "USD", "issuer": hex::encode(issuer), "value": "1000000000"},
                "SendMax": "5000000",
                "DeliverMin": {"currency": "USD", "issuer": hex::encode(issuer), "value": "10"},
                "Flags": 131072u64,
            }),
        };
        assert_eq!(PaymentTransactor.do_apply(&tx, &mut sandbox), TxResult::PathPartial);
        // Rolled back: XRP untouched, and the taker's line still holds nothing.
        assert_eq!(read_balance(&sandbox, &taker), 50_000_000);
        let cur = crate::tx::offer::amount_currency20(
            &serde_json::json!({"currency": "USD", "issuer": hex::encode(issuer), "value": "1"}),
        )
        .unwrap();
        let line = sandbox.read(&keylet::ripple_state_key(&taker, &issuer, &cur)).unwrap();
        let line: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(line["Balance"]["value"].as_str().unwrap(), "0");
    }
}
