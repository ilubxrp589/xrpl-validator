#!/usr/bin/env python3
"""Watch validator live: tx_engine + consensus agreement + state hash.
Usage: python3 watch_engine.py
"""
import json, os, sys, time, urllib.request

BASE = os.environ.get("VALIDATOR_BASE", "http://localhost:3777")
# Override with FFI_BASE env var to point at the node's FFI sidecar.
FFI_BASE = os.environ.get("FFI_BASE", "http://127.0.0.1:3778")
# Upstream xrpld that ws-sync hydrates from. A stall or version skew here is
# invisible in the panels below, which is exactly how a 320-ledger ws-sync gap
# and a 3.2.1-vs-3.3.0 peer break went unnoticed on 2026-08-17.
def _upstream_rpc():
    """UPSTREAM_RPC / XRPL_RPC_URL from our environment; else borrow XRPL_RPC_URL
    from the running validator's environment (same host, same user — the
    watcher is normally run beside it); else the local default."""
    v = os.environ.get("UPSTREAM_RPC") or os.environ.get("XRPL_RPC_URL")
    if v:
        return v
    try:
        for pid in os.listdir("/proc"):
            if not pid.isdigit():
                continue
            try:
                env = open(f"/proc/{pid}/environ", "rb").read()
            except OSError:
                continue
            for kv in env.split(b"\0"):
                if kv.startswith(b"XRPL_RPC_URL="):
                    return kv.split(b"=", 1)[1].decode(errors="replace").split(",")[0]
    except OSError:
        pass
    return "http://localhost:5005"

UPSTREAM_RPC = _upstream_rpc()
# Engine receipts (ter_mismatch / byte_diff per ledger) — on the validator host.
SHADOW_RECEIPTS = os.environ.get("SHADOW_RECEIPTS", "/mnt/xrpl-data/native_shadow.jsonl")

def fetch(path, base=None):
    try:
        url = (base or BASE) + path
        with urllib.request.urlopen(url, timeout=3) as r:
            return json.loads(r.read())
    except Exception as e:
        return {"error": str(e)}

def rpc(method, base=None, timeout=3):
    """POST a JSON-RPC command to an xrpld node."""
    try:
        body = json.dumps({"method": method, "params": [{}]}).encode()
        req = urllib.request.Request(
            base or UPSTREAM_RPC, data=body,
            headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read()).get("result", {})
    except Exception as e:
        return {"error": str(e)}

def colored(text, code):
    return f"\033[{code}m{text}\033[0m"

def bar(pct, width=20):
    filled = int(pct / 100 * width)
    return "[" + "█" * filled + "░" * (width - filled) + "]"

def subtitle(text):
    """Dim one-line explainer under a section header."""
    return colored(f"   {text}", "90")

def tag_ours():
    return colored("[OURS — Rust]", "2;32")

def tag_rippled(note="rippled"):
    return colored(f"[{note}]", "1;94")

def verdict_line(sh, ffi, up, stage3):
    """One line that answers 'is everything OK?' — same signals as the panels."""
    problems = []
    if not up:
        problems.append("upstream xrpld unreachable")
    elif up.get("server_state") not in ("full", "proposing", "validating"):
        problems.append(f"upstream not serving ({up.get('server_state', '?')})")
    mism = sh.get("total_mismatches", 0) if isinstance(sh, dict) else 0
    if mism:
        problems.append(f"{mism} state-hash MISMATCH")
    div = ffi.get("live_apply_diverged", 0) if isinstance(ffi, dict) else 0
    if div:
        problems.append(f"{div} tx diverged")
    sha_mm = ffi.get("shadow_hash_mismatched", 0) if isinstance(ffi, dict) else 0
    if sha_mm:
        problems.append(f"{sha_mm} shadow-hash mismatch")
    if problems:
        return colored("  ✗ ATTENTION: " + "; ".join(problems), "1;31")
    consec = sh.get("consecutive_matches", 0) if isinstance(sh, dict) else 0
    ready = sh.get("ready_to_sign", False) if isinstance(sh, dict) else False
    mode = "engine-written state (Stage 3)" if stage3 else "shadow mode"
    if ready:
        return colored(f"  ✓ ALL GOOD — signing, {consec:,} clean ledgers in a row, {mode}", "1;32")
    return colored(f"  … warming up — verifying ledgers before signing resumes ({mode})", "33")

def render():
    eng = fetch("/api/engine")
    cons = fetch("/api/consensus")
    sh = fetch("/api/state-hash")
    up = rpc("server_info").get("info", {})
    # FFI stats come from the integrated validator now, not a separate sidecar
    ffi = eng.get("ffi_verifier", {}) if isinstance(eng, dict) else {}

    out = []
    out.append(colored("═══ XRPL Validator — Live Status ═══", "1;36"))

    # Stage 3 banner — highly visible so operators know when the FFI overlay
    # is the source of truth for state.rocks.
    stage3 = ffi.get("stage3_enabled", False) if isinstance(ffi, dict) else False
    if stage3:
        out.append(colored("  ★ STAGE 3: ACTIVE — our engine WRITES the ledger database ★", "1;30;42"))
        out.append(subtitle("state.rocks bytes come from our own tx engine, not copied from rippled"))
    else:
        out.append(colored("  STAGE 3: inactive — shadow mode (we verify but rippled's bytes are used)", "90"))
    out.append(verdict_line(sh, ffi, up, stage3))
    out.append(subtitle(f"who owns what: {tag_ours()}" + colored(" = our Rust code   ", "90")
               + tag_rippled("RIPPLED") + colored(" = rippled's code (their node, or their C++ core linked into ours)", "90")))
    out.append("")

    # Ledger + sync
    ledger = eng.get("ledger_seq", "?")
    matches = sh.get("total_matches", 0) if isinstance(sh, dict) else 0
    mismatches = sh.get("total_mismatches", 0) if isinstance(sh, dict) else 0
    consec = sh.get("consecutive_matches", 0) if isinstance(sh, dict) else 0
    ready = sh.get("ready_to_sign", False) if isinstance(sh, dict) else False
    ready_str = colored("✓ VERIFIED — signing validations", "1;32") if ready else colored("follower — watching, not signing yet", "33")
    out.append(f"{colored('Ledger:', '1')}         #{ledger}  {ready_str}")
    out.append(f"{colored('State hash:', '1')}    {colored(f'{matches:,}', '32')} matched | {colored(f'{mismatches:,}', '31' if mismatches else '32')} missed | {consec:,} in a row  {tag_ours()}")
    out.append(subtitle("after every ledger: does our whole database hash to the same root as mainnet? (our own Rust SHAMap computes this)"))
    out.append("")

    # ── Upstream xrpld ──────────────────────────────────────────────────────
    # ws-sync hydrates from this node. If it stalls, the validator keeps
    # SIGNING (from the VERIFIED source) while ws-sync silently holds position
    # and the gap grows — nothing else on this screen would show it.
    out.append(colored(f"── Upstream xrpld ({UPSTREAM_RPC}) ──", "1;36") + "  " + tag_rippled("RIPPLED — their software, our data feed"))
    out.append(subtitle("the rippled node we pull ledgers from — our data source, NOT our engine"))
    if not up:
        out.append(f"  {colored('UNREACHABLE', '1;31')} — ws-sync has no upstream; it will fail over or stall")
    else:
        st = up.get("server_state", "?")
        serving = st in ("full", "proposing", "validating")
        st_col = "1;32" if serving else "1;31"
        up_build = up.get("build_version", "?")
        peers = up.get("peers", "?")
        out.append(f"  build {colored(up_build, '36')}  |  state={colored(st, st_col)}  |  peers={peers}")
        cl = up.get("complete_ledgers", "empty")
        cl_col = "32" if cl not in ("empty", None, "") else "1;31"
        out.append(f"  complete_ledgers: {colored(str(cl), cl_col)}")
        if not serving:
            out.append(f"  {colored('NOT SERVING — ws-sync cannot fill its gap until this reaches full', '1;31')}")
            # Recovery progress (2026-08-26, post disk-full outage): a detached
            # node closes a LOCAL genesis chain (closed seq ~200) and nothing
            # else on this panel moves. Anchor moment = closed_seq jumping to
            # the network's ~106M range; validated appearing = full imminent.
            try:
                stint_s = int(up.get("server_state_duration_us", 0)) // 1_000_000
                stint = f"{stint_s // 60}m{stint_s % 60:02d}s in {st}"
            except (TypeError, ValueError):
                stint = f"in {st}"
            closed = (up.get("closed_ledger") or {}).get("seq")
            vseq = (up.get("validated_ledger") or {}).get("seq")
            prev = globals().setdefault("_up_recovery_prev", {})
            delta = ""
            if isinstance(closed, int):
                if isinstance(prev.get("closed"), int) and closed >= prev["closed"]:
                    delta = f" (+{closed - prev['closed']}/refresh)"
                prev["closed"] = closed
            if isinstance(closed, int) and closed > 1_000_000:
                anchor = colored("ANCHORED to network chain ✓", "1;32")
            else:
                anchor = colored("local chain — NOT anchored yet", "33")
            out.append(f"  recovery: {stint} | closed #{closed}{delta} | {anchor}")
            if vseq:
                out.append(f"  validated #{vseq} — {colored('acquisition complete, full imminent', '1;32')}")

        # ws-sync lag = upstream validated ledger MINUS the last ledger ws-sync
        # actually hashed/wrote. Use /api/state-hash's ledger_seq, NOT
        # /api/engine's: the latter is the engine's live tip (which keeps
        # advancing from the VERIFIED source even while ws-sync is wedged), so
        # it would read ~0 during exactly the failure this panel exists to catch.
        # Derived here because there is no ws-sync lag endpoint
        # (/api/sync-status is bulk-sync object counts, not ledger lag).
        up_seq = (up.get("validated_ledger") or {}).get("seq")
        ws_seq = sh.get("ledger_seq") if isinstance(sh, dict) else None
        try:
            lag = max(0, int(up_seq) - int(ws_seq))
        except (TypeError, ValueError):
            lag = None
        if lag is None:
            out.append(f"  {colored('ws-sync lag:', '1')}   {colored('unknown', '33')} (upstream has no validated ledger yet)")
        else:
            lag_col = "32" if lag <= 5 else ("33" if lag <= 50 else "1;31")
            note = "" if lag <= 5 else ("  ← catching up" if lag <= 50 else "  ← ws-sync is BEHIND / holding position")
            out.append(f"  {colored('ws-sync lag:', '1')}   {colored(f'{lag} ledgers', lag_col)}{colored(note, lag_col)}"
                       f"   (wrote #{ws_seq}, upstream #{up_seq})")
            # A gap only closes if upstream still HAS the ledger ws-sync needs.
            # After a restart xrpld resumes near the tip and backfills backwards;
            # if its low end never reaches ws_seq the gap is unfillable and the
            # only recovery is a validator wipe+resync.
            try:
                low = int(str(cl).split("-")[0])
                if lag > 5 and ws_seq is not None and low > int(ws_seq):
                    out.append(f"  {colored(f'UPSTREAM MISSING #{ws_seq} — backfilling down to it ({low - int(ws_seq)} to go)', '1;33')}")
            except (TypeError, ValueError, IndexError):
                pass
    out.append("")

    # FFI (libxrpl integration — integrated into validator)
    if isinstance(ffi, dict) and ffi.get("enabled") is not False:
        out.append(colored("── Transaction Engine (libxrpl via FFI) ──", "1;35") + "  " + tag_rippled("RIPPLED'S C++ CORE — inside our process"))
        out.append(subtitle("rippled's own apply code, statically linked, driven by our Rust — the part the native Rust engine replaces next"))
        ver = ffi.get("libxrpl_version", "?")
        apply_attempted = ffi.get("live_apply_attempted", 0)
        apply_ok = ffi.get("live_apply_ok", 0)
        apply_claimed = ffi.get("live_apply_claimed", 0)
        apply_diverged = ffi.get("live_apply_diverged", 0)
        apply_ms = ffi.get("live_apply_last_ms", 0)
        apply_last_ter = ffi.get("live_apply_last_ter", "?")
        ledgers = ffi.get("ledgers_applied", 0)
        agreed = apply_ok + apply_claimed
        agreed_pct = (agreed / apply_attempted * 100) if apply_attempted else 0
        ok_pct = (apply_ok / apply_attempted * 100) if apply_attempted else 0
        agreed_color = "1;32" if agreed_pct >= 99.9 else ("33" if agreed_pct >= 95 else "31")
        # libxrpl here is the STATICALLY LINKED lib built from
        # xrpl-3.2.0-build/ffi/vendor/rippled — it does NOT follow apt upgrades
        # of the upstream xrpld package. Skew means the differential gate is
        # scoring us against a different rippled than mainnet is running, so
        # surface it rather than letting it drift silently.
        net_ver = up.get("build_version") if isinstance(up, dict) else None
        if net_ver and ver != "?" and ver != net_ver:
            skew = colored(f"  ⚠ SKEW: network/upstream is {net_ver}", "1;33")
        else:
            skew = ""
        out.append(f"  libxrpl {colored(ver, '36')} (linked, not apt)  |  {ledgers} ledgers applied{skew}")
        out.append(f"  {colored('MAINNET AGREEMENT:', '1')} {colored(f'{agreed_pct:.2f}%', agreed_color)}   ({agreed:,}/{apply_attempted:,}  diverged={apply_diverged})")
        out.append(f"    tesSUCCESS: {colored(f'{apply_ok:,}', '32')} ({ok_pct:.1f}%)  +  tec* claimed: {colored(f'{apply_claimed:,}', '36')}  |  last: {colored(apply_last_ter, '32' if apply_last_ter == 'tesSUCCESS' else '33')} {apply_ms}ms")
        out.append(subtitle("claimed = the tx failed on mainnet and we failed it the exact same way (fee still burns)"))
        # Top tx types by volume
        types = ffi.get("apply_by_type", {})
        if types:
            top = sorted(types.items(), key=lambda x: -x[1])[:6]
            type_line = "    types: " + " | ".join(f"{k}:{colored(str(v), '36')}" for k, v in top)
            out.append(type_line)
        # TER breakdown
        ters = ffi.get("live_apply_ter_counts", {})
        if ters:
            top_t = sorted(ters.items(), key=lambda x: -x[1])[:5]
            t_line = "    TERs:  " + " | ".join(f"{k}:{colored(str(v), '36')}" for k, v in top_t)
            out.append(t_line)
        # Divergences (should be empty)
        by_type = ffi.get("live_diverged_by_type", {})
        if by_type:
            top_d = sorted(by_type.items(), key=lambda x: -x[1])[:5]
            out.append(colored("    DIVERGED (we disagreed with mainnet — real findings):", "1;31"))
            for k, v in top_d:
                out.append(f"      {k:<38s} {colored(str(v), '31')}")
        # Silent divergences (our tesSUCCESS/tec* vs different network result)
        silent_total = ffi.get("live_apply_silent_diverged", 0)
        silent_pairs = ffi.get("silent_diverged_by_pair", {})
        if silent_total or silent_pairs:
            out.append(colored(f"    SILENT DIVERGED: {silent_total:,} (result code differs though both sides applied)", "1;33"))
            for k, v in sorted(silent_pairs.items(), key=lambda x: -x[1])[:6]:
                out.append(f"      {k:<46s} {colored(str(v), '33')}")
        # Mutation-list divergences (TER agrees but mutation set differs — BF6C928F class)
        mut_total = ffi.get("live_apply_mutation_diverged", 0)
        mut_types = ffi.get("mutation_diverged_by_type", {})
        if mut_total or mut_types:
            out.append(colored(f"    MUTATION DIVERGED: {mut_total:,} (same result code, different ledger objects touched)", "1;35"))
            for k, v in sorted(mut_types.items(), key=lambda x: -x[1])[:6]:
                out.append(f"      {k:<46s} {colored(str(v), '35')}")
        # RPC fallbacks by SLE type — keys our state.rocks is missing
        db_h = ffi.get("db_hits", 0)
        db_f = ffi.get("db_rpc_fallbacks", 0)
        fb_total = db_h + db_f
        fb_types = ffi.get("db_fallback_by_le_type", {})
        if fb_total > 0:
            pct = db_f / fb_total * 100 if fb_total else 0
            color = "32" if pct < 1 else ("33" if pct < 5 else "36")
            out.append(f"    {colored(f'STATE.ROCKS MISSES: {db_f:,} / {fb_total:,} ({pct:.2f}%)', color)} {colored('— keys our DB lacked, fetched over RPC instead', '90')}")
            for k, v in sorted(fb_types.items(), key=lambda x: -x[1])[:8]:
                out.append(f"      {k:<46s} {colored(str(v), '36')}")
    elif isinstance(ffi, dict) and ffi.get("enabled") is False:
        out.append(colored("── libxrpl FFI Engine ──", "1;35"))
        out.append(f"  {colored('DISABLED', '33')} — {ffi.get('note', 'build with --features ffi')}")
    out.append("")

    # Stage 4 Phase A: the native Rust engine shadowing the C++ core in-process.
    ns = eng.get("native_shadow", {}) if isinstance(eng, dict) else {}
    if isinstance(ns, dict) and ns.get("enabled"):
        out.append(colored("── Native Engine Shadow (Stage 4) ──", "1;32") + "  " + tag_ours())
        out.append(subtitle("our own Rust tx engine applies every ledger beside the C++ core — do the overlays agree byte-for-byte?"))
        if not ns.get("hydrated"):
            gaps = ns.get("skipped_gap", 0)
            out.append(f"  {colored('hydrating…', '33')} (mirror loads from state.rocks on the next steady ledger; gaps so far: {gaps})")
        led = ns.get("ledgers", 0)
        if led:
            fm = ns.get("full_match", 0)
            dv = ns.get("overlay_diverged", 0)
            pct = fm / led * 100 if led else 0
            col = "1;32" if dv == 0 else "1;31"
            out.append(f"  {colored('OVERLAY AGREEMENT:', '1')} {colored(f'{pct:.2f}%', col)}   ({fm:,}/{led:,} ledgers  diverged={dv})")
            ta = ns.get("txs_applied", 0); tm = ns.get("ter_matched", 0); tmm = ns.get("ter_mismatched", 0)
            out.append(f"    txs applied: {colored(f'{ta:,}', '36')}  ter-match: {colored(f'{tm:,}', '32')}  ter-miss: {colored(str(tmm), '31' if tmm else '32')}  |  last apply: {ns.get('apply_ms_last', 0)}ms")
            km = ns.get("key_missing", 0); ke = ns.get("key_extra", 0); kb = ns.get("byte_mismatch", 0)
            if km or ke or kb:
                out.append(f"    {colored(f'key diffs: missing={km} extra={ke} bytes={kb}', '1;31')}")
            # WHICH transactions are ter-missing. The counter alone cannot say,
            # and a wrong result code writes identical state, so the overlay
            # never shows it (F241: 292 CheckCash rode through cycle 108 green).
            # The engine logs them to the receipt file marked ter_only=true.
            if tmm:
                try:
                    import collections, json as _j
                    pairs = collections.Counter(); seen = 0
                    # Only THIS run's receipts: the file is append-only across
                    # deploys, and a `***` engine receipt carries its ter_mismatch
                    # without the ter_only flag — count both kinds, from the
                    # first ledger this process shadowed onward.
                    try:
                        run_start = int(ledger) - int(led) - 2
                    except (TypeError, ValueError):
                        run_start = 0
                    with open(SHADOW_RECEIPTS, "rb") as fh:
                        try: fh.seek(-2_000_000, 2)
                        except OSError: fh.seek(0)
                        for ln in fh.read().splitlines()[1:]:
                            if b'"ter_mismatch":[]' in ln or b'"ter_mismatch": []' in ln: continue
                            try: d = _j.loads(ln)
                            except Exception: continue
                            if int(d.get("seq", 0)) < run_start: continue
                            for e in d.get("ter_mismatch") or []:
                                head, _, rest = e.partition(" ")
                                ty = head.split(":")[1] if ":" in head else "?"
                                vs = rest.split(" STALE")[0].split(" FIELDS")[0].strip()
                                pairs[(ty, vs)] += 1; seen += 1
                    if pairs:
                        out.append(f"    {colored('ter-miss breakdown:', '1;31')} {seen:,} logged")
                        for (ty, vs), n in pairs.most_common(4):
                            out.append(f"      {colored(f'{n:>5}', '31')}  {ty}  {vs}")
                    else:
                        out.append(f"    {colored('ter-miss breakdown: none logged', '33')} (this run; needs the ter_only build, deploy109+)")
                except FileNotFoundError:
                    out.append(f"    {colored('ter-miss breakdown: receipts file not on this host', '33')} ({SHADOW_RECEIPTS}; run beside the validator or set SHADOW_RECEIPTS)")
                except Exception as _e:
                    out.append(f"    ter-miss breakdown unavailable: {_e}")
        hy_n = ns.get("hydrate_objects", 0)
        if hy_n:
            out.append(subtitle(f"mirror: {hy_n:,} objects, hydrated in {ns.get('hydrate_ms',0)//1000}s ({ns.get('hydrate_decode_err',0)} undecodable)"))
        out.append("")

    out.append(colored("── Consensus Monitor ──", "1;35") + "  " + tag_ours())
    out.append(subtitle("listening to the trusted validators' votes — do they agree, and on what?"))
    monitor = cons.get("monitor", {}) if isinstance(cons, dict) else {}
    unl_size = cons.get("unl_size", 0) if isinstance(cons, dict) else 0
    tracked = monitor.get("tracked_validators", 0)
    total_props = monitor.get("total_proposals", 0)
    out.append(f"{colored('UNL:', '1')}           {unl_size} trusted validators | {tracked} currently tracked")
    out.append(f"{colored('Proposals:', '1')}     {total_props:,} received total")
    agreement = monitor.get("agreement")
    if agreement:
        count = agreement.get("count", 0)
        pct = agreement.get("pct", 0) * 100
        tx_hash = agreement.get("tx_hash", "")[:16]
        seq = agreement.get("propose_seq_max", 0)
        color = "32" if pct >= 80 else "33" if pct >= 60 else "31"
        out.append(f"{colored('Agreement:', '1')}     {colored(f'{count}/{unl_size}', color)} on {tx_hash}...  seq={seq}")
        out.append(f"               {colored(bar(pct), color)} {colored(f'{pct:.0f}%', color)}")
    else:
        out.append(f"{colored('Agreement:', '1')}     (none yet)")
    phase = cons.get("phase", "?") if isinstance(cons, dict) else "?"
    mempool = cons.get("mempool_size", 0) if isinstance(cons, dict) else 0
    candidate = cons.get("candidate_set_size", 0) if isinstance(cons, dict) else 0
    rounds = cons.get("establish_rounds", 0) if isinstance(cons, dict) else 0
    out.append(f"{colored('State:', '1')}         phase={phase}  mempool={mempool}  candidate={candidate}  rounds={rounds}")
    out.append("")

    # State hash tracking
    out.append(colored("── State Hash (detail) ──", "1;34") + "  " + tag_ours())
    out.append(subtitle("the signing gate: we only sign a ledger our own database independently hashed to"))
    matches = sh.get("total_matches", 0) if isinstance(sh, dict) else 0
    mismatches = sh.get("total_mismatches", 0) if isinstance(sh, dict) else 0
    consec = sh.get("consecutive_matches", 0) if isinstance(sh, dict) else 0
    out.append(f"  {colored(f'{matches:,}', '32')} matches | {colored(f'{mismatches:,}', '31' if mismatches else '32')} mismatches | {consec} consecutive")
    # VALAUDIT Phase 3 (va-03) signing-gate counters
    skip_nr = sh.get("validations_skipped_not_ready", 0) if isinstance(sh, dict) else 0
    skip_zh = sh.get("validations_skipped_zero_hash", 0) if isinstance(sh, dict) else 0
    skip_total = skip_nr + skip_zh
    if skip_total > 0:
        # not_ready up to ~100 = normal warmup (hasher build window swallows
        # incoming ledgers). zero_hash ALWAYS anomalous (StatusChange without
        # ledger_hash means an upstream feed problem). Operators read the
        # absolute number — colors reflect "warmup-band" vs "always wrong".
        skip_color = "31" if skip_zh > 0 else ("33" if skip_nr <= 100 else "1;31")
        out.append(f"  {colored(f'VALIDATIONS SKIPPED: {skip_total:,}', skip_color)}  (not_ready: {skip_nr:,}, zero_hash: {skip_zh:,})")

    # Shadow state hash (FFI-derived)
    sha_att = ffi.get("shadow_hash_attempted", 0) if isinstance(ffi, dict) else 0
    if sha_att > 0:
        sha_m = ffi.get("shadow_hash_matched", 0)
        sha_mm = ffi.get("shadow_hash_mismatched", 0)
        sha_pct = sha_m / sha_att * 100 if sha_att else 0
        sha_color = "32" if sha_pct >= 99.9 else ("33" if sha_pct >= 90 else "31")
        out.append(colored("── Shadow Hash (engine-computed state root) ──", "1;35") + "  " + colored("[C++ output, checked by our Rust]", "95"))
        out.append(subtitle("the engine's independently computed state root vs the network's — the proof behind Stage 3"))
        out.append(f"  {colored(f'{sha_m}/{sha_att}', sha_color)} matched ({sha_pct:.1f}%)  |  {colored(f'{sha_mm}', '31' if sha_mm else '32')} mismatched")
        last = ffi.get("shadow_hash_last", "")[:24]
        net = ffi.get("shadow_hash_last_network", "")[:24]
        if last:
            out.append(f"  ours:    {colored(last, '36')}...")
            out.append(f"  network: {colored(net, '36')}...")

    return "\n".join(out)

def main():
    if "--once" in sys.argv:
        print(render())
        return
    try:
        while True:
            print("\033[2J\033[H", end="")
            print(render())
            print(f"\n\033[90m↻ 1s refresh | Ctrl-C to exit | {time.strftime('%H:%M:%S')}\033[0m")
            time.sleep(1)
    except KeyboardInterrupt:
        print("\nexited")

if __name__ == "__main__":
    main()
