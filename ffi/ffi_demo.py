#!/usr/bin/env python3
"""Live demo of the FFI pipeline.

Runs the libxrpl test_shim binary in a loop and pretty-prints stats:
 - tx parsed count
 - preflight tesSUCCESS count
 - apply tesSUCCESS count
 - calls/second
 - latency (p50/p99)

Usage: python3 ffi_demo.py
"""
import json
import subprocess
import sys
import time

SHIM_BIN = "./build/test_shim"

def colored(text, code):
    return f"\033[{code}m{text}\033[0m"

def bar(pct, width=25):
    filled = int(pct / 100 * width)
    return "[" + "█" * filled + "░" * (width - filled) + "]"

def run_once():
    """Run test_shim once, parse output, return (parsed_ok, preflight_ok, apply_ok, elapsed_ms)."""
    t0 = time.perf_counter()
    try:
        r = subprocess.run([SHIM_BIN], capture_output=True, text=True, timeout=10)
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return (False, False, False, 0)
    elapsed_ms = (time.perf_counter() - t0) * 1000
    output = r.stdout

    parsed = "Tx type: Payment" in output
    preflight = "preflight result: TER=0 (tesSUCCESS)" in output
    apply_ok = "apply result: TER=0 (tesSUCCESS) applied=true" in output
    return (parsed, preflight, apply_ok, elapsed_ms)

def render(stats):
    out = []
    out.append(colored("═══ XRPL libxrpl FFI — Live Apply Demo ═══", "1;35"))
    out.append("")

    total = stats['iterations']
    parsed = stats['parsed']
    preflight = stats['preflight']
    applied = stats['applied']

    # BIG STATUS BANNER — the one thing to watch
    all_ok = (parsed == total and preflight == total and applied == total and total > 0)
    if all_ok:
        banner = colored(" ✓ FFI HEALTHY ", "1;42;97")
        msg = colored("All 3 libxrpl stages returning tesSUCCESS on every call", "32")
    elif total == 0:
        banner = colored(" … STARTING ", "1;43;30")
        msg = "Waiting for first iteration..."
    else:
        banner = colored(" ✗ FFI BROKEN ", "1;41;97")
        msg = colored("One or more stages failed — check test_shim output", "31")
    out.append(f"  {banner}  {msg}")
    out.append("")
    out.append(colored("What this demo proves:", "1"))
    out.append("  • Our C++ shim can link against libxrpl.a (746MB static lib from rippled)")
    out.append("  • libxrpl's production transactors validate + apply a real mainnet Payment")
    out.append("  • State lookups go through OUR callback (Rust would own RocksDB in production)")
    out.append("  • Full pipeline: parse tx → preflight → preclaim → doApply → TER=tesSUCCESS")
    out.append("")

    # Stage counters
    def row(name, count, color):
        pct = (count / total * 100) if total else 0
        return f"  {name:12s} {colored(f'{count:,}', color):25s} {colored(bar(pct), color)} {colored(f'{pct:5.1f}%', color)}"

    out.append(colored("── Stages (each iteration runs all three) ──", "1;36"))
    out.append(row("tx parsed",  parsed,     "32" if parsed == total else "31"))
    out.append(row("preflight",  preflight,  "32" if preflight == total else "31"))
    out.append(row("apply",      applied,    "32" if applied == total else "31"))
    out.append("")

    # Throughput
    elapsed = time.time() - stats['start']
    tps = total / elapsed if elapsed > 0 else 0
    out.append(colored("── Throughput ──", "1;33"))
    out.append(f"  elapsed:     {elapsed:.1f}s")
    out.append(f"  iterations:  {colored(f'{total:,}', '36')} × 3 stages = {colored(f'{total*3:,}', '36')} libxrpl calls")
    out.append(f"  rate:        {colored(f'{tps:.1f} iter/s', '36')} ({tps*3:.0f} libxrpl calls/s)")
    out.append("")

    # Latency
    if stats['latencies']:
        lats = sorted(stats['latencies'])
        n = len(lats)
        p50 = lats[n // 2]
        p99 = lats[min(n - 1, int(n * 0.99))]
        avg = sum(lats) / n
        out.append(colored("── Latency (per iteration, all 3 stages via subprocess) ──", "1;34"))
        out.append(f"  avg:  {colored(f'{avg:6.2f}ms', '36')}  p50: {colored(f'{p50:6.2f}ms', '36')}  p99: {colored(f'{p99:6.2f}ms', '36')}")
        out.append(colored("  (real in-process FFI would be ~1000x faster — no subprocess overhead)", "90"))
        out.append("")

    # Static test tx info (honest: same tx every iteration)
    out.append(colored("── Test transaction (hardcoded in test_shim) ──", "1;37"))
    out.append("  source:   mainnet ledger 103354511 (referral reward)")
    out.append("  type:     Payment (XRP)")
    out.append("  hash:     39077702C3FCE0DDC5693065FC0DA35576E4D011...33ABA")
    out.append("  amount:   10 drops | fee: 15 drops")
    out.append(colored("  note:     same tx every iteration — proves pipeline, not diversity", "90"))

    return "\n".join(out)

def main():
    print(colored("Starting FFI demo — running libxrpl through our shim in a loop", "1;35"))
    print()

    stats = {
        'iterations': 0,
        'parsed': 0,
        'preflight': 0,
        'applied': 0,
        'latencies': [],
        'start': time.time(),
    }
    try:
        while True:
            parsed, pf, ap, ms = run_once()
            stats['iterations'] += 1
            if parsed: stats['parsed'] += 1
            if pf:     stats['preflight'] += 1
            if ap:     stats['applied'] += 1
            stats['latencies'].append(ms)
            if len(stats['latencies']) > 1000:
                stats['latencies'] = stats['latencies'][-1000:]

            print("\033[2J\033[H", end="")
            print(render(stats))
            print(f"\n\033[90m↻ Ctrl-C to exit | {time.strftime('%H:%M:%S')}\033[0m")
            time.sleep(0.05)  # ~20 iter/s
    except KeyboardInterrupt:
        print("\nexited")
        print(f"\nFinal: {stats['applied']:,}/{stats['iterations']:,} successful xrpl::apply() calls via FFI")

if __name__ == "__main__":
    main()
