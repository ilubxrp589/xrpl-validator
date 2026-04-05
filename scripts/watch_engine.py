#!/usr/bin/env python3
"""Watch validator live: tx_engine + consensus agreement + state hash.
Usage: python3 watch_engine.py
"""
import json, os, sys, time, urllib.request

BASE = os.environ.get("VALIDATOR_BASE", "http://localhost:3777")
# Override with FFI_BASE env var — defaults to m3060 (100%-agreement sidecar).
# Set FFI_BASE=http://10.0.0.39:3778 for localai.
FFI_BASE = os.environ.get("FFI_BASE", "http://10.0.0.97:3778")

def fetch(path, base=None):
    try:
        url = (base or BASE) + path
        with urllib.request.urlopen(url, timeout=3) as r:
            return json.loads(r.read())
    except Exception as e:
        return {"error": str(e)}

def colored(text, code):
    return f"\033[{code}m{text}\033[0m"

def bar(pct, width=20):
    filled = int(pct / 100 * width)
    return "[" + "█" * filled + "░" * (width - filled) + "]"

def render():
    eng = fetch("/api/engine")
    cons = fetch("/api/consensus")
    sh = fetch("/api/state-hash")
    ffi = fetch("/api/ffi-status", base=FFI_BASE)

    out = []
    out.append(colored("═══ XRPL Validator — Live Status ═══", "1;36"))
    out.append("")

    # Ledger + sync
    ledger = eng.get("ledger_seq", "?")
    matches = sh.get("total_matches", 0) if isinstance(sh, dict) else 0
    mismatches = sh.get("total_mismatches", 0) if isinstance(sh, dict) else 0
    consec = sh.get("consecutive_matches", 0) if isinstance(sh, dict) else 0
    ready = sh.get("ready_to_sign", False) if isinstance(sh, dict) else False
    ready_str = colored("✓ VERIFIED", "1;32") if ready else colored("follower", "33")
    out.append(f"{colored('Ledger:', '1')}         #{ledger}  state={ready_str}")
    out.append(f"{colored('State hash:', '1')}    {colored(f'{matches:,}', '32')} matches | {colored(f'{mismatches:,}', '31' if mismatches else '32')} miss | {consec} consecutive")
    out.append("")

    # Consensus (Phase A)
    # FFI (libxrpl integration)
    if isinstance(ffi, dict):
        out.append(colored("── libxrpl FFI (live mainnet tx → rippled C++ engine) ──", "1;35"))
        if ffi.get("enabled") is False:
            out.append(f"  {colored('DISABLED', '33')} — {ffi.get('note', 'build with --features ffi')}")
        else:
            ver = ffi.get("libxrpl_version", "?")
            parsed = ffi.get("live_txs_parsed", 0)
            pf_ok = ffi.get("live_txs_preflight_ok", 0)
            pf_fail = ffi.get("live_txs_preflight_fail", 0)
            live_seq = ffi.get("live_ledger_seq", 0)
            last_type = ffi.get("live_last_type", "?")
            last_ter = ffi.get("live_last_ter", "?")
            pct = (pf_ok / parsed * 100) if parsed else 0
            out.append(f"  libxrpl:     {colored(ver, '36')}")
            out.append(f"  live ledger: #{live_seq}")
            out.append(f"  txs through FFI: {colored(f'{parsed:,}', '36')}  |  preflight OK: {colored(f'{pf_ok:,}', '32')}  fail: {colored(f'{pf_fail:,}', '31' if pf_fail else '32')}  ({pct:.1f}%)")
            out.append(f"  last tx:     {last_type}  TER={colored(last_ter, '32' if last_ter == 'tesSUCCESS' else '33')}")
            types = ffi.get("live_types_seen", {})
            if types:
                top = sorted(types.items(), key=lambda x: -x[1])[:6]
                type_line = "  types:       " + " | ".join(f"{k}:{colored(str(v), '36')}" for k, v in top)
                out.append(type_line)
            # Full apply stats (RPC-fetched state)
            apply_attempted = ffi.get("live_apply_attempted", 0)
            apply_ok = ffi.get("live_apply_ok", 0)
            apply_claimed = ffi.get("live_apply_claimed", 0)
            apply_diverged = ffi.get("live_apply_diverged", 0)
            apply_ms = ffi.get("live_apply_last_ms", 0)
            apply_last_ter = ffi.get("live_apply_last_ter", "?")
            apply_muts = ffi.get("live_apply_last_mutations", 0)
            agreed = apply_ok + apply_claimed
            agreed_pct = (agreed / apply_attempted * 100) if apply_attempted else 0
            ok_pct = (apply_ok / apply_attempted * 100) if apply_attempted else 0
            out.append("")
            out.append(colored("  Full apply() on sampled live txs (RPC pre-state):", "1"))
            agreed_color = "1;32" if agreed_pct >= 99.9 else ("33" if agreed_pct >= 95 else "31")
            out.append(f"    {colored('MAINNET AGREEMENT:', '1')} {colored(f'{agreed_pct:.2f}%', agreed_color)}   ({agreed:,}/{apply_attempted:,}  diverged={apply_diverged})")
            out.append(f"      └─ tesSUCCESS: {colored(f'{apply_ok:,}', '32')} ({ok_pct:.1f}%)  +  tec* claimed-by-mainnet: {colored(f'{apply_claimed:,}', '36')}")
            out.append(f"    last apply: TER={colored(apply_last_ter, '32' if apply_last_ter == 'tesSUCCESS' else ('36' if apply_last_ter.startswith('tec') else '33'))}  muts={apply_muts}  {apply_ms}ms")
            ters = ffi.get("live_apply_ter_counts", {})
            if ters:
                top_t = sorted(ters.items(), key=lambda x: -x[1])[:5]
                t_line = "    TER codes: " + " | ".join(f"{k}:{colored(str(v), '36')}" for k, v in top_t)
                out.append(t_line)
            # Per-(type, TER) divergence buckets
            by_type = ffi.get("live_diverged_by_type", {})
            if by_type:
                top_d = sorted(by_type.items(), key=lambda x: -x[1])[:8]
                out.append(colored("    Top diverged (tx_type/TER):", "1;33"))
                for k, v in top_d:
                    out.append(f"      {k:<38s} {colored(str(v), '31')}")
            # Self-test (tiny line)
            checks = ffi.get("health_checks", 0)
            passed = ffi.get("health_passed", 0)
            out.append("")
            out.append(f"  self-test:   {colored(f'{passed}/{checks}', '32' if passed == checks and checks > 0 else '31')}  (hardcoded tx every 500ms, sanity check)")
        out.append("")

    out.append(colored("── Consensus Monitor (Phase A) ──", "1;35"))
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

    # tx_engine verification (kept for visibility, marked deprecated)
    out.append(colored("── Tx Engine (verification, deprecated for FFI) ──", "1;33"))
    t = eng.get("tx_engine", {}) if isinstance(eng, dict) else {}
    total = t.get("total_txs", 0)
    matched = t.get("matched", 0)
    mm = t.get("mismatched", 0)
    sk = t.get("skipped", 0)
    cov = t.get("coverage_pct", 0)
    lg = t.get("ledgers_processed", 0)
    out.append(f"{colored('txs:', '1')}           total={total:,}  matched={colored(f'{matched:,}', '32')}  miss={colored(f'{mm:,}', '31' if mm else '32')}  skip={sk:,}")
    out.append(f"{colored('coverage:', '1')}      {colored(f'{cov:.2f}%', '36')}  ({lg} ledgers)")
    out.append("")

    # Current round stats
    out.append(colored("── Current Round ──", "1;34"))
    rt_total = t.get("round_total", 0)
    rt_matched = t.get("round_matched", 0)
    rt_mm = t.get("round_mismatched", 0)
    out.append(f"  total={rt_total}  matched={colored(str(rt_matched), '32')}  mismatched={colored(str(rt_mm), '31' if rt_mm else '32')}")
    out.append("")

    # Top types
    out.append(colored("── Top Tx Types (by volume) ──", "1;37"))
    types = sorted(t.get("type_stats", {}).items(), key=lambda x: -x[1].get("attempted", 0))[:10]
    for name, s in types:
        att = s.get("attempted", 0)
        m = s.get("matched", 0)
        miss = s.get("mismatched", 0)
        rate = m / att * 100 if att else 0
        rc = "32" if rate >= 99.9 else "33" if rate >= 90 else "31"
        out.append(f"  {name:22s} {m:>6}/{att:<6} miss={miss:<3} {colored(f'{rate:5.1f}%', rc)}")

    return "\n".join(out)

def main():
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
