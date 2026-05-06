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
    # FFI stats come from the integrated validator now, not a separate sidecar
    ffi = eng.get("ffi_verifier", {}) if isinstance(eng, dict) else {}

    out = []
    out.append(colored("═══ XRPL Validator — Live Status ═══", "1;36"))

    # Stage 3 banner — highly visible so operators know when the FFI overlay
    # is the source of truth for state.rocks.
    stage3 = ffi.get("stage3_enabled", False) if isinstance(ffi, dict) else False
    if stage3:
        out.append(colored("  ★ STAGE 3: ACTIVE — FFI overlay writing state.rocks ★", "1;30;42"))
    else:
        out.append(colored("  STAGE 3: inactive (shadow-only Phase A)", "90"))
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

    # FFI (libxrpl integration — integrated into validator)
    if isinstance(ffi, dict) and ffi.get("enabled") is not False:
        out.append(colored("── libxrpl FFI Engine ──", "1;35"))
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
        out.append(f"  libxrpl {colored(ver, '36')}  |  {ledgers} ledgers applied")
        out.append(f"  {colored('MAINNET AGREEMENT:', '1')} {colored(f'{agreed_pct:.2f}%', agreed_color)}   ({agreed:,}/{apply_attempted:,}  diverged={apply_diverged})")
        out.append(f"    tesSUCCESS: {colored(f'{apply_ok:,}', '32')} ({ok_pct:.1f}%)  +  tec* claimed: {colored(f'{apply_claimed:,}', '36')}  |  last: {colored(apply_last_ter, '32' if apply_last_ter == 'tesSUCCESS' else '33')} {apply_ms}ms")
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
            out.append(colored("    DIVERGED:", "1;31"))
            for k, v in top_d:
                out.append(f"      {k:<38s} {colored(str(v), '31')}")
        # Silent divergences (our tesSUCCESS/tec* vs different network result)
        silent_total = ffi.get("live_apply_silent_diverged", 0)
        silent_pairs = ffi.get("silent_diverged_by_pair", {})
        if silent_total or silent_pairs:
            out.append(colored(f"    SILENT DIVERGED: {silent_total:,}", "1;33"))
            for k, v in sorted(silent_pairs.items(), key=lambda x: -x[1])[:6]:
                out.append(f"      {k:<46s} {colored(str(v), '33')}")
        # Mutation-list divergences (TER agrees but mutation set differs — BF6C928F class)
        mut_total = ffi.get("live_apply_mutation_diverged", 0)
        mut_types = ffi.get("mutation_diverged_by_type", {})
        if mut_total or mut_types:
            out.append(colored(f"    MUTATION DIVERGED: {mut_total:,}", "1;35"))
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
            out.append(f"    {colored(f'STATE.ROCKS MISSES: {db_f:,} / {fb_total:,} ({pct:.2f}%)', color)}")
            for k, v in sorted(fb_types.items(), key=lambda x: -x[1])[:8]:
                out.append(f"      {k:<46s} {colored(str(v), '36')}")
    elif isinstance(ffi, dict) and ffi.get("enabled") is False:
        out.append(colored("── libxrpl FFI Engine ──", "1;35"))
        out.append(f"  {colored('DISABLED', '33')} — {ffi.get('note', 'build with --features ffi')}")
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

    # State hash tracking
    out.append(colored("── State Hash ──", "1;34"))
    matches = sh.get("total_matches", 0) if isinstance(sh, dict) else 0
    mismatches = sh.get("total_mismatches", 0) if isinstance(sh, dict) else 0
    consec = sh.get("consecutive_matches", 0) if isinstance(sh, dict) else 0
    out.append(f"  {colored(f'{matches:,}', '32')} matches | {colored(f'{mismatches:,}', '31' if mismatches else '32')} mismatches | {consec} consecutive")
    # VALAUDIT Phase 3 (va-03) signing-gate counters
    skip_nr = sh.get("validations_skipped_not_ready", 0) if isinstance(sh, dict) else 0
    skip_zh = sh.get("validations_skipped_zero_hash", 0) if isinstance(sh, dict) else 0
    skip_total = skip_nr + skip_zh
    if skip_total > 0:
        # ~3 skips at warmup is normal; persistent growth is the signal
        skip_color = "33" if skip_nr <= 5 and skip_zh == 0 else "31"
        out.append(f"  {colored(f'VALIDATIONS SKIPPED: {skip_total:,}', skip_color)}  (not_ready: {skip_nr:,}, zero_hash: {skip_zh:,})")

    # Shadow state hash (FFI-derived)
    sha_att = ffi.get("shadow_hash_attempted", 0) if isinstance(ffi, dict) else 0
    if sha_att > 0:
        sha_m = ffi.get("shadow_hash_matched", 0)
        sha_mm = ffi.get("shadow_hash_mismatched", 0)
        sha_pct = sha_m / sha_att * 100 if sha_att else 0
        sha_color = "32" if sha_pct >= 99.9 else ("33" if sha_pct >= 90 else "31")
        out.append(colored("── Shadow Hash (FFI-derived state) ──", "1;35"))
        out.append(f"  {colored(f'{sha_m}/{sha_att}', sha_color)} matched ({sha_pct:.1f}%)  |  {colored(f'{sha_mm}', '31' if sha_mm else '32')} mismatched")
        last = ffi.get("shadow_hash_last", "")[:24]
        net = ffi.get("shadow_hash_last_network", "")[:24]
        if last:
            out.append(f"  ours:    {colored(last, '36')}...")
            out.append(f"  network: {colored(net, '36')}...")

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
