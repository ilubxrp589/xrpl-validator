//! The INDEPENDENCE page (key `i`): the road to a fully independent validator (docs/INDEPENDENCE-PLAN.md), live.
//! Each step with what the running validator says about it: its switches (the process environment), its
//! counters (/api/engine, /api/amendments, /metrics) and the files it keeps beside the receipts.

use super::*;

/// What the slower sources say, read every few seconds by the collector.
#[derive(Clone, Default)]
pub struct Ind {
    pub amend: Value,
    pub metrics: BTreeMap<String, f64>,
    /// The validator's switches, from its environment.
    pub switches: BTreeMap<String, String>,
    /// The last validator list that verified: publisher, sequence, when (unix).
    pub unl_good: Option<(String, u64, u64)>,
    /// The consensus round record: its size and its last write.
    pub rounds: Option<(u64, SystemTime)>,
    /// Lines in each difference log (native_meta, native_order, peer_fetch), with the size they were counted at.
    pub logs: BTreeMap<&'static str, (u64, usize)>,
    pub read: bool,
}

const SWITCHES: [&str; 10] = [
    "XRPL_SIG_ENFORCE", "XRPL_UNL_ENFORCE", "XRPL_UNL_PINNED_KEY", "XRPL_UNL_STATE", "XRPL_NATIVE_META",
    "XRPL_NATIVE_META_LOG", "XRPL_CONSENSUS_RECORD", "XRPL_CONSENSUS_RECORD_PATH", "XRPL_NATIVE_ORDER", "XRPL_PEER_FETCH",
];

/// One read of everything the page shows beyond /api/engine. `prev` saves recounting logs that did not grow.
pub fn read(cfg: &Cfg, vproc: Option<&ValidatorProc>, prev: &Ind) -> Ind {
    let mut ind = Ind { read: true, ..Ind::default() };
    ind.amend = get_json(&format!("{}/api/amendments", cfg.base)).unwrap_or(Value::Null);
    if let Some(text) = ureq::get(&format!("{}/metrics", cfg.base)).timeout(Duration::from_secs(2)).call().ok().and_then(|r| r.into_string().ok()) {
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            if let Some((k, v)) = line.rsplit_once(' ') {
                if let Ok(x) = v.trim().parse::<f64>() {
                    ind.metrics.insert(k.trim().to_string(), x);
                }
            }
        }
    }
    if let Some(p) = vproc {
        for k in SWITCHES {
            if let Some(v) = p.env(k) {
                ind.switches.insert(k.to_string(), v);
            }
        }
    }
    let data = std::path::Path::new(&cfg.receipts).parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let path = |env: &str, file: &str| ind.switches.get(env).cloned().unwrap_or_else(|| data.join(file).display().to_string());
    let unl = path("XRPL_UNL_STATE", "unl-last-good.json");
    if let Some(v) = fs::read_to_string(&unl).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        ind.unl_good = Some((s(&v, &["publisher"]).to_string(), u(&v, &["sequence"]), u(&v, &["written_unix"])));
    }
    let rounds = path("XRPL_CONSENSUS_RECORD_PATH", "consensus_rounds.jsonl");
    ind.rounds = fs::metadata(&rounds).ok().and_then(|m| Some((m.len(), m.modified().ok()?)));
    for (key, env, file) in [
        ("native_meta", "XRPL_NATIVE_META_LOG", "native_meta.jsonl"),
        ("native_order", "XRPL_NATIVE_ORDER_LOG", "native_order.jsonl"),
        ("peer_fetch", "XRPL_PEER_FETCH_LOG", "peer_fetch.jsonl"),
    ] {
        let f = path(env, file);
        let Ok(m) = fs::metadata(&f) else { continue };
        let lines = match prev.logs.get(key) {
            Some((size, n)) if *size == m.len() => *n,
            _ => fs::read(&f).map(|b| b.iter().filter(|c| **c == b'\n').count()).unwrap_or(0),
        };
        ind.logs.insert(key, (m.len(), lines));
    }
    ind
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Live,
    Shadow,
    Built,
    Started,
    Later,
}

fn chip(st: St) -> (&'static str, Color) {
    let p = pal();
    match st {
        St::Live => ("LIVE", p.good),
        St::Shadow => ("SHADOW", p.feed),
        St::Built => ("BUILT · OFF", p.warn),
        St::Started => ("STARTED", p.violet),
        St::Later => ("LATER", p.dim),
    }
}

struct Step {
    n: u8,
    name: &'static str,
    st: St,
    lines: Vec<Line<'static>>,
    /// The step's headline number, drawn big under its lines when there is room, with its caption.
    hero: Option<(u64, String, Color)>,
}

fn on(ind: &Ind, k: &str) -> bool {
    ind.switches.get(k).is_some_and(|v| v == "1")
}

fn kv(k: &str, v: Vec<Span<'static>>) -> Line<'static> {
    let mut l = vec![Span::styled(format!(" {k:<11}"), Style::new().fg(pal().violet).add_modifier(Modifier::BOLD))];
    l.extend(v);
    Line::from(l)
}

fn note(t: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::raw(" "), Span::styled(t.into(), Style::new().fg(pal().dim).add_modifier(Modifier::ITALIC))])
}

/// A count of differences: green at zero, red otherwise.
fn zero_good(n: u64) -> Span<'static> {
    let p = pal();
    if n == 0 { bold("0", p.good) } else { bold(commas(n), p.bad) }
}

fn exact_line(what: &str, ok: u64, total: u64) -> Line<'static> {
    let p = pal();
    let c = if total == 0 { p.dim } else if ok == total { p.good } else { p.warn };
    kv(what, vec![bold(commas(ok), c), dim(format!(" of {}", commas(total))), dim(if total > 0 { format!("  ({:.2}%)", pct(ok, total)) } else { String::new() })])
}

fn steps(sn: &Snap) -> Vec<Step> {
    let p = pal();
    let ind = &sn.ind;
    let e = &sn.eng;
    let now_unix = sn.at.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let m = |k: &str| ind.metrics.get(k).copied().unwrap_or(0.0) as u64;
    let logs = |k: &str| ind.logs.get(k).map(|(_, n)| *n);
    let mut out = Vec::new();

    // 1 · switch on what's built
    {
        let g = get(&ind.amend, &["guard"]);
        let status = s(g, &["status"]);
        let gaps = get(g, &["gap_enabled"]).as_array().map_or(0, |a| a.len());
        let unknown = get(g, &["majority_unknown"]).as_array().map_or(0, |a| a.len());
        let sig_on = on(ind, "XRPL_SIG_ENFORCE") || m("xrpl_sig_enforce") == 1;
        let unl_on = on(ind, "XRPL_UNL_ENFORCE");
        let pinned = ind.switches.get("XRPL_UNL_PINNED_KEY").map_or(0, |v| v.split(',').filter(|k| !k.is_empty()).count());
        let mut lines = vec![kv(
            "amendments",
            vec![
                bold(if status.is_empty() { "?".to_string() } else { status.to_string() }, if status == "ok" { p.good } else { p.warn }),
                dim(format!(" · {} on · {} known · ", u(g, &["enabled"]), u(g, &["known"]))),
                zero_good(gaps as u64),
                dim(" gaps"),
            ],
        )];
        if unknown > 0 {
            lines.push(kv("", vec![bold(format!("{unknown} with majority the engine lacks"), p.bad)]));
        }
        lines.push(kv(
            "signatures",
            vec![
                ink(commas(m("xrpl_proposal_sig_ok_total") + m("xrpl_validation_sig_ok_total"))),
                dim(" verified · "),
                zero_good(m("xrpl_proposal_sig_fail_total") + m("xrpl_validation_sig_fail_total")),
                dim(" failed"),
            ],
        ));
        lines.push(kv(
            "  enforce",
            if sig_on {
                vec![bold("ON", p.good), dim(format!(" · bad ones dropped ({} so far)", commas(m("xrpl_proposal_sig_dropped_total"))))]
            } else {
                vec![bold("off", p.warn), dim(" · counted, not dropped")]
            },
        ));
        let unl_size = u(&sn.cons, &["unl_size"]);
        let list = match &ind.unl_good {
            Some((_, seq, at)) => {
                let secs = now_unix.saturating_sub(*at);
                let when = if secs >= 3600 { format!("{} h", secs / 3600) } else { format!("{} min", secs / 60) };
                format!("{unl_size} validators · list #{seq}, {when} ago")
            }
            None => format!("{unl_size} validators"),
        };
        lines.push(kv("UNL", vec![ink(list)]));
        lines.push(kv(
            "  fails",
            if unl_on {
                vec![bold("CLOSED", p.good), dim(format!(" · {pinned} publisher key{} pinned", if pinned == 1 { "" } else { "s" }))]
            } else {
                vec![bold("open", p.warn), dim(" · an unverified list is still used")]
            },
        ));
        let st = if status.is_empty() { St::Built } else { St::Live };
        let verified = m("xrpl_proposal_sig_ok_total") + m("xrpl_validation_sig_ok_total");
        let hero = (verified > 0).then(|| (verified, "peer signatures checked".to_string(), p.good));
        out.push(Step { n: 1, name: "SWITCH ON WHAT'S BUILT", st, lines, hero });
    }

    // 2 · our own transaction records
    {
        let nm = get(e, &["native_meta"]);
        let enabled = b(nm, &["enabled"]);
        let mut lines = Vec::new();
        if enabled {
            lines.push(exact_line("records", u(nm, &["exact"]), u(nm, &["records"])));
            lines.push(kv("differ", vec![zero_good(u(nm, &["differ"])), dim(" · missing "), zero_good(u(nm, &["missing"]))]));
            let trees = u(nm, &["tree_match"]) + u(nm, &["tree_differ"]) + u(nm, &["tree_incomplete"]);
            lines.push(exact_line("trees", u(nm, &["tree_match"]), trees));
            lines.push(kv("receipts", vec![zero_good(logs("native_meta").unwrap_or(0) as u64), dim(" lines in native_meta.jsonl")]));
        } else {
            lines.push(kv("switch", vec![bold("XRPL_NATIVE_META off", p.warn)]));
        }
        lines.push(note("our metadata for every tx, byte for byte, and its tree"));
        let hero = enabled.then(|| (u(nm, &["exact"]), "of our own records byte-exact".to_string(), p.feed));
        out.push(Step { n: 2, name: "OUR OWN TRANSACTION RECORDS", st: if enabled { St::Shadow } else { St::Built }, lines, hero });
    }

    // 3 · our own order and results
    {
        let no = get(e, &["native_order"]);
        let mode = s(no, &["mode"]);
        let mut lines = Vec::new();
        let st = match mode {
            "shadow" => St::Shadow,
            "on" => St::Live,
            _ => St::Built,
        };
        if mode == "shadow" || mode == "on" {
            lines.push(kv("mode", vec![bold(mode.to_string(), p.feed), dim(if mode == "shadow" { " · applied our way, undone, compared" } else { " · our order is the apply" })]));
            lines.push(exact_line("ledgers", u(no, &["exact"]), u(no, &["ledgers"])));
            lines.push(kv(
                "differ",
                vec![
                    dim("order "), zero_good(u(no, &["order_differ"])),
                    dim(" · results "), zero_good(u(no, &["result_differ"])),
                    dim(" · bytes "), zero_good(u(no, &["bytes_differ"])),
                    dim(" · total XRP "), zero_good(u(no, &["coins_differ"])),
                ],
            ));
            lines.push(kv("passes", vec![ink(commas(u(no, &["retried"]))), dim(format!(" applied after pass 1 · {} attempts", commas(u(no, &["attempts"]))))]));
            lines.push(kv("receipts", vec![zero_good(logs("native_order").unwrap_or(0) as u64), dim(" lines in native_order.jsonl")]));
        } else if no.is_null() {
            lines.push(kv("build", vec![bold("not in this build", p.warn), dim(" (cycle 173)")]));
        } else {
            lines.push(kv("switch", vec![bold("XRPL_NATIVE_ORDER off", p.warn)]));
        }
        lines.push(note("rippled's canonical order + 3 retry passes, from the set alone"));
        let hero = matches!(st, St::Shadow | St::Live).then(|| (u(no, &["exact"]), "ledgers built exactly as mainnet did".to_string(), p.feed));
        out.push(Step { n: 3, name: "OUR OWN ORDER AND RESULTS", st, lines, hero });
    }

    // 4 · ledgers from peers
    {
        let pf = get(e, &["peer_fetch"]);
        let enabled = b(pf, &["enabled"]);
        let mut lines = Vec::new();
        if enabled {
            lines.push(kv("fetched", vec![bold(commas(u(pf, &["fetched"])), p.feed), dim(format!(" proven to the ledger hash · {} peers", u(pf, &["peers"])))]));
            lines.push(kv(
                "vs .39",
                vec![bold(commas(u(pf, &["matched"])), p.good), dim(" the same byte for byte · "), zero_good(u(pf, &["differ"])), dim(" differ · "), zero_good(u(pf, &["failed"])), dim(" failed")],
            ));
            let per = if u(pf, &["fetched"]) > 0 { u(pf, &["requests"]) as f64 / u(pf, &["fetched"]) as f64 } else { 0.0 };
            lines.push(kv("requests", vec![ink(commas(u(pf, &["requests"]))), dim(format!(" · {per:.1} a ledger · {} skipped", commas(u(pf, &["skipped"]))))]));
            lines.push(kv("receipts", vec![zero_good(logs("peer_fetch").unwrap_or(0) as u64), dim(" lines in peer_fetch.jsonl")]));
        } else if pf.is_null() {
            lines.push(kv("build", vec![bold("not in this build", p.warn), dim(" (cycle 173)")]));
        } else {
            lines.push(kv("switch", vec![bold("XRPL_PEER_FETCH off", p.warn)]));
        }
        // 4.5: "validated" from our own count of trusted validations.
        let ov = get(e, &["own_validated"]);
        if b(ov, &["enabled"]) {
            let lead = get(ov, &["lead_ms_median"]).as_i64();
            lines.push(kv(
                "validated",
                vec![
                    bold(commas(u(ov, &["agree"])), p.good),
                    dim(" ours = .39's · "),
                    zero_good(u(ov, &["differ"])),
                    dim(" differ · "),
                    zero_good(u(ov, &["short"])),
                    dim(" short"),
                    dim(match lead {
                        Some(ms) if ms >= 0 => format!(" · {ms} ms ahead"),
                        Some(ms) => format!(" · {} ms behind", -ms),
                        None => String::new(),
                    }),
                ],
            ));
        }
        lines.push(kv("source", vec![ink(".39"), dim(" · peers are the witness until 4.3")]));
        let hero = enabled.then(|| (u(pf, &["matched"]), "ledgers from peers, same as .39's".to_string(), p.feed));
        out.push(Step { n: 4, name: "LEDGERS FROM PEERS", st: if enabled { St::Shadow } else { St::Built }, lines, hero });
    }

    // 5 · build at the live edge, sign our own hash
    {
        let mut lines = vec![kv("close time", vec![bold("ported", p.good), dim(" · 0 misses in 3,806 headers")])];
        let recording = ind.rounds.as_ref().and_then(|(_, t)| sn.at?.duration_since(*t).ok()).filter(|d| d.as_secs() < 60);
        lines.push(kv(
            "recorder",
            match (&ind.rounds, recording) {
                (Some((size, _)), Some(d)) => vec![bold("ON", p.good), dim(format!(" · {:.1} MB · last round {} ago", *size as f64 / 1e6, ago(d.as_secs())))],
                (Some((size, _)), None) => vec![bold("idle", p.warn), dim(format!(" · {:.1} MB, nothing new in a minute", *size as f64 / 1e6))],
                _ => vec![bold(if on(ind, "XRPL_CONSENSUS_RECORD") { "starting" } else { "off" }, p.warn)],
            },
        ));
        lines.push(kv("signing", vec![ink("the network's hash"), dim(format!(" · {} in a row", commas(u(&sn.sh, &["consecutive_matches"]))))]));
        lines.push(note("next: sign our own hash when it equals the UNL's"));
        let hero = Some((u(&sn.sh, &["consecutive_matches"]), "state-hash matches in a row".to_string(), p.violet));
        out.push(Step { n: 5, name: "BUILD AND SIGN OUR OWN HASH", st: St::Started, lines, hero });
    }

    // 6 · a Rust-only build
    {
        let fv = get(e, &["ffi_verifier"]);
        let lines = vec![
            kv("C++ core", vec![ink("linked"), dim(format!(" · {} txs cross-checked", commas(u(fv, &["apply_duration_count"]))))]),
            kv("writer", vec![bold("Rust", p.rust), dim(format!(" · {} ledgers written", commas(u(e, &["native_shadow", "writer", "native"]))))]),
            note("then: no libxrpl in the build (a test oracle only)"),
        ];
        let hero = Some((u(e, &["native_shadow", "writer", "native"]), "ledgers our Rust engine wrote".to_string(), p.rust));
        out.push(Step { n: 6, name: "A RUST-ONLY BUILD", st: St::Later, lines, hero });
    }
    out
}

fn draw_road(f: &mut Frame, r: Rect, st: &[Step], sn: &Snap) {
    let p = pal();
    let block = panel(vec![bold("THE ROAD TO INDEPENDENCE", p.pink), dim(" · every new path ships off, runs in shadow against mainnet, then switches on")], p.pink);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let n = st.len() + 1;
    let slot = (w / n).max(8);
    let reached = st.iter().rposition(|s| matches!(s.st, St::Live | St::Shadow)).map_or(0, |i| i + 1);
    let mut track = Vec::new();
    let mut names = Vec::new();
    let mut states = Vec::new();
    for (i, s) in st.iter().enumerate() {
        let (word, c) = chip(s.st);
        let seg = if i + 1 <= reached { p.good } else { p.dim };
        track.push(bold(format!("{:━<width$}", format!("━{}━", if matches!(s.st, St::Live | St::Shadow) { "●" } else { "○" }), width = slot), seg));
        names.push(bold(format!("{:<width$}", format!(" {} {}", s.n, short(s.name)), width = slot), c));
        states.push(sp(format!("{:<width$}", format!("   {word}"), width = slot), c));
    }
    track.push(bold("★", p.gold));
    names.push(bold(" INDEPENDENT", p.gold));
    states.push(dim("   the goal"));
    let days = 0;
    let goal = Line::from(vec![
        dim(" Done = 30 days signing a hash we computed ourselves, from peer data: "),
        bold(format!("{days} of 30 days"), p.gold),
        dim(format!(" · today: the network's hash, after 3 matches ({} in a row)", commas(u(&sn.sh, &["consecutive_matches"])))),
    ]);
    f.render_widget(Paragraph::new(vec![Line::from(track), Line::from(names), Line::from(states), goal]), inner);
}

/// A step's name, short enough for the road.
fn short(name: &str) -> &'static str {
    match name {
        "SWITCH ON WHAT'S BUILT" => "SWITCH ON",
        "OUR OWN TRANSACTION RECORDS" => "OWN RECORDS",
        "OUR OWN ORDER AND RESULTS" => "OWN ORDER",
        "LEDGERS FROM PEERS" => "FROM PEERS",
        "BUILD AND SIGN OUR OWN HASH" => "OWN HASH",
        _ => "RUST ONLY",
    }
}

fn draw_step(f: &mut Frame, r: Rect, s: &Step) {
    let p = pal();
    let (word, c) = chip(s.st);
    let block = panel(
        vec![bold(format!("STEP {} · {}", s.n, s.name), c), Span::raw(" "), Span::styled(format!(" {word} "), Style::new().fg(p.bg).bg(c).add_modifier(Modifier::BOLD))],
        c,
    );
    let inner = block.inner(r);
    f.render_widget(block, r);
    let need = s.lines.len() as u16;
    let parts = match &s.hero {
        Some(_) if inner.height >= need + 5 => Layout::vertical([Constraint::Min(need), Constraint::Length(4)]).split(inner),
        _ => Layout::vertical([Constraint::Min(0), Constraint::Length(0)]).split(inner),
    };
    f.render_widget(Paragraph::new(s.lines.clone()).wrap(Wrap { trim: false }), parts[0]);
    if let (Some((n, caption, color)), true) = (&s.hero, parts[1].height >= 4) {
        let d = big_digits(&commas(*n));
        let mut hero: Vec<Line<'static>> = d.iter().map(|row| Line::from(vec![Span::raw("  "), bold(row.clone(), *color)])).collect();
        hero.push(Line::from(vec![Span::raw("  "), dim(caption.clone())]));
        f.render_widget(Paragraph::new(hero), parts[1]);
    }
}

fn draw_borrowed(f: &mut Frame, r: Rect, st: &[Step]) {
    let p = pal();
    let state = |n: u8| st.iter().find(|s| s.n == n).map_or(St::Later, |s| s.st);
    let row = |what: &str, from: &str, n: u8| {
        let (word, c) = chip(state(n));
        Line::from(vec![
            Span::raw(" "),
            ink(format!("{what:<58}")),
            dim(format!("{from:<34}")),
            bold(format!("ends at step {n}  "), c),
            sp(word, c),
        ])
    };
    let lines = vec![
        row("each ledger's transactions, their records and the header", "the stock xrpld on .39", 4),
        row("the transaction records and the transaction tree", ".39's metadata", 2),
        row("the order the transactions apply in", ".39's position numbers", 3),
        row("the ledger hash it signs", "the network's announcement", 5),
        row("libxrpl at build time", "the ffi feature", 6),
    ];
    let block = panel(vec![bold("STILL BORROWED", p.warn), dim(" · and the step that ends each")], p.warn);
    f.render_widget(Paragraph::new(lines).block(block), r);
}

pub fn draw(f: &mut Frame, g: &Shared, paused: bool) {
    let p = pal();
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(p.bg)), area);
    let sn = &g.snap;
    let st = steps(sn);
    let rows = Layout::vertical([
        Constraint::Length(1),  // header
        Constraint::Length(6),  // the road
        Constraint::Fill(1),   // steps 1-3
        Constraint::Fill(1),   // steps 4-6
        Constraint::Length(7), // still borrowed
        Constraint::Length(1), // reading …
        Constraint::Length(1), // footer
    ])
    .split(area);
    draw_header(f, rows[0], sn);
    draw_road(f, rows[1], &st, sn);
    for (row, chunk) in [(rows[2], &st[..3]), (rows[3], &st[3..])] {
        let cols = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(row);
        for (c, s) in cols.iter().zip(chunk) {
            draw_step(f, *c, s);
        }
    }
    draw_borrowed(f, rows[4], &st);
    if !sn.ind.read {
        f.render_widget(Paragraph::new(dim(" reading the validator …")), rows[5]);
    }
    draw_footer(f, rows[6], sn, paused);
}
