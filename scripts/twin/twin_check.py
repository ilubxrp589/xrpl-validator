#!/usr/bin/env python3
"""The twin's check: a peer-fed live_viewer that signs nothing, beside the live validator (Step 4 plan, Task 13).

    twin_check.py --twin http://127.0.0.1:3787 --live http://127.0.0.1:3777 [--state FILE]

Prints one line and exits 0 when the twin is healthy, 1 naming each problem:
- it is not on the peer source, or not observing (then stop it);
- rpc_refused > 0: some code path still asked .39;
- our own hash differed from the network's on more ledgers than at the last check (--state keeps the count);
- it is more than 10 ledgers behind the live validator.
Holds and keyed fallbacks are reported, not judged: a hold is a ledger peers had not delivered yet.
"""
import argparse
import json
import sys
import urllib.request

MAX_LAG = 10


def fetch_json(url):
    with urllib.request.urlopen(url, timeout=10) as r:
        return json.load(r)


def judge(twin, live, prev):
    """The problems (empty: healthy), the one-line summary, and the state to keep."""
    src, oh, obs = twin.get("ledger_source", {}), twin.get("own_hash", {}), twin.get("observe", {})
    problems = []
    if src.get("mode") != "peers":
        problems.append(f"not on the peer source (mode {src.get('mode')!r})")
    if not obs.get("observe"):
        problems.append("not observing: stop it now")
    if src.get("rpc_refused", 0) > 0:
        problems.append(f"rpc_refused {src['rpc_refused']}: a path still asks .39")
    differ, prev_differ = oh.get("differ", 0), (prev or {}).get("differ", 0)
    if differ > prev_differ:
        problems.append(f"own hash differed on {differ - prev_differ} more ledger(s) ({differ} in all): drill them")
    twin_seq, live_seq = oh.get("last_seq", 0), live.get("own_hash", {}).get("last_seq", 0)
    lag = live_seq - twin_seq
    if lag > MAX_LAG:
        problems.append(f"{lag} ledgers behind the live validator")
    line = (
        f"twin #{twin_seq} (live #{live_seq}, lag {lag}) | from peers {src.get('from_peers', 0)} holds {src.get('holds', 0)} "
        f"keyed {src.get('keyed_fallbacks', 0)} chain {src.get('chain_walks', 0)} rpc_refused {src.get('rpc_refused', 0)} | "
        f"own {oh.get('own', 0)}/{oh.get('ledgers', 0)} differ {differ} streak {oh.get('streak', 0)} | "
        f"would sign {obs.get('would_sign', 0)} (own {obs.get('would_sign_own', 0)})"
    )
    return problems, line, {"differ": differ}


def main(argv=None, fetch=fetch_json):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--twin", default="http://127.0.0.1:3787")
    ap.add_argument("--live", default="http://127.0.0.1:3777")
    ap.add_argument("--state")
    a = ap.parse_args(argv)
    prev = None
    if a.state:
        try:
            with open(a.state) as f:
                prev = json.load(f)
        except (OSError, ValueError):
            prev = None
    try:
        twin = fetch(a.twin.rstrip("/") + "/api/engine")
        live = fetch(a.live.rstrip("/") + "/api/engine")
    except Exception as e:  # noqa: BLE001 - one line, whatever broke
        print(f"TWIN PROBLEM: no answer ({e})")
        return 1
    problems, line, keep = judge(twin, live, prev)
    if a.state:
        with open(a.state, "w") as f:
            json.dump(keep, f)
    print(("TWIN OK: " if not problems else "TWIN PROBLEM: " + "; ".join(problems) + " | ") + line)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
