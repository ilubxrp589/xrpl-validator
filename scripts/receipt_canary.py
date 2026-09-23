#!/usr/bin/env python3
"""Daily receipt canary: proves a zero-receipt soak means zero divergences.

A soak counts as clean when the live shadow wrote no receipts for 24 h. That
only means something if the shadow was comparing the whole time, if its
compare would have flagged a divergence, and if the watchers would have seen
the line. This check proves all three on the live process, once a day:

  1. LIVENESS - /api/engine: the shadow is enabled and hydrated, and its
     ledger/tx/key counters advanced since the last run (a frozen or dropped
     mirror writes no receipts either).
  2. SENSITIVITY - touch the shadow's trigger file. On the next ledger it
     plants one extra drop on an AccountRoot the FFI leg agreed on, just
     before the real compare (native_shadow.rs, arm_canary). The compare must
     flag it: a {"canary": {"detected": true}} line in the receipt log, and
     the trigger consumed.
  3. ALARM PATH - auto_triage (the 5-minute receipt watcher) must log the
     canary line as CANARY.

One Telegram line either way. Everything that counts receipts skips
`canary` lines (`grep -vc '"canary"'`).

Env: SHADOW_SSH (ssh target, default loop-m3060), SHADOW_RECEIPTS,
SHADOW_CANARY (trigger path), TRIAGE_LOG (auto_triage's log on this host),
CANARY_STATE, CANARY_PLANT_WAIT / CANARY_TRIAGE_WAIT (seconds).
--liveness-only: step 1 alone (no plant), for a live_viewer without the canary.
--no-telegram: print the report instead of sending it.
"""
import json, os, subprocess, sys, time, urllib.request

HOST = os.environ.get("SHADOW_SSH", "loop-m3060")
RECEIPTS = os.environ.get("SHADOW_RECEIPTS", "/mnt/xrpl-data/native_shadow.jsonl")
TRIGGER = os.environ.get("SHADOW_CANARY", "/mnt/xrpl-data/native_shadow.canary")
TRIAGE_LOG = os.path.expanduser(os.environ.get("TRIAGE_LOG", "~/xrpl-ops/auto_triage.log"))
STATE = os.path.expanduser(os.environ.get("CANARY_STATE", "~/.receipt-canary-state.json"))
CREDS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "copilot", "config.local.json")
PLANT_WAIT = int(os.environ.get("CANARY_PLANT_WAIT", "600"))
TRIAGE_WAIT = int(os.environ.get("CANARY_TRIAGE_WAIT", "480"))


def ssh(cmd, timeout=30):
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", HOST, cmd],
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout


def tg(msg):
    print(msg)
    if "--no-telegram" in sys.argv:
        return
    try:
        t = json.load(open(CREDS)).get("alerts", {}).get("telegram") or {}
        if not t.get("token"):
            return
        req = urllib.request.Request(
            f"https://api.telegram.org/bot{t['token']}/sendMessage",
            data=json.dumps({"chat_id": t["chatId"], "text": msg}).encode(),
            headers={"Content-Type": "application/json"},
        )
        urllib.request.urlopen(req, timeout=15)
    except Exception:
        pass


def canary_lines():
    """Every canary line in the receipt log (receipts are rare; the file is small)."""
    out = []
    for ln in ssh(f"grep '\"canary\"' {RECEIPTS} 2>/dev/null").splitlines():
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        if isinstance(d.get("canary"), dict):
            out.append(d)
    return out


def main():
    try:
        prev = json.load(open(STATE))
    except (OSError, ValueError):
        prev = {}
    fails, notes = [], []

    # 1. Liveness.
    try:
        ns = json.loads(ssh("curl -s --max-time 5 localhost:3777/api/engine")).get("native_shadow") or {}
    except ValueError:
        ns = {}
    if not ns:
        tg("🔴 RECEIPT CANARY FAILED — the engine API did not answer (live_viewer down?). "
           "A zero-receipt soak is not evidence until this passes.")
        return 1
    if not ns.get("enabled"):
        fails.append("shadow not enabled")
    if not ns.get("hydrated"):
        fails.append("mirror not hydrated")
    led, txs = int(ns.get("ledgers", 0)), int(ns.get("txs_applied", 0))
    restarted = bool(prev) and led < int(prev.get("ledgers", 0))
    dled = led - int(prev.get("ledgers", 0)) if prev and not restarted else led
    dtxs = txs - int(prev.get("txs_applied", 0)) if prev and not restarted else txs
    if prev and dled <= 0:
        fails.append(f"no ledgers compared since the last run (counter stuck at {led:,})")
    if restarted:
        notes.append("process restarted since the last run")
    receipts = ssh(f"grep -vc '\"canary\"' {RECEIPTS} 2>/dev/null").strip() or "?"

    # 2. Sensitivity: plant, and wait for the compare's verdict.
    seen, took, line = {d["seq"] for d in canary_lines()}, None, None
    if "--liveness-only" not in sys.argv:
        ssh(f"touch {TRIGGER}")
        t0 = time.time()
        while time.time() - t0 < PLANT_WAIT and line is None:
            time.sleep(10)
            fresh = [d for d in canary_lines() if d["seq"] not in seen]
            line = fresh[-1] if fresh else None
        took = int(time.time() - t0)
        if line is None:
            ssh(f"rm -f {TRIGGER}")
            fails.append(f"no canary line within {PLANT_WAIT}s of arming (trigger withdrawn)")
        else:
            c = line["canary"]
            if not c.get("detected"):
                fails.append(f"compare BLIND at #{line['seq']}: planted a drop on {str(c.get('key'))[:12]} and it was not flagged")
            if ssh(f"test -e {TRIGGER} && echo armed").strip() == "armed":
                fails.append("trigger not consumed after the plant")

    # 3. Alarm path: auto_triage must have read the line.
    triaged = None
    if line is not None:
        t1, tag = time.time(), f"{line['seq']} CANARY"
        while time.time() - t1 < TRIAGE_WAIT:
            try:
                if any(ln.startswith(tag) for ln in open(TRIAGE_LOG)):
                    triaged = int(time.time() - t1)
                    break
            except OSError:
                pass
            time.sleep(15)
        if triaged is None:
            fails.append(f"auto_triage did not log the canary within {TRIAGE_WAIT}s ({TRIAGE_LOG})")

    json.dump({"ledgers": led, "txs_applied": txs, "keys_compared": int(ns.get("keys_compared", 0)),
               "t": int(time.time()), "ok": not fails}, open(STATE, "w"))
    live = f"shadow compared {dled:,} ledgers / {dtxs:,} txs since the last run ({receipts} receipts in the log)"
    if fails:
        tg("🔴 RECEIPT CANARY FAILED — " + "; ".join(fails) + f". {live}. "
           "A zero-receipt soak is not evidence until this passes.")
        return 1
    if line is None:
        tg(f"🐤 receipt canary (liveness only) OK — {live}" + (f"; {', '.join(notes)}" if notes else ""))
        return 0
    tg(f"🐤 receipt canary OK — {live}; planted 1 drop at #{line['seq']:,} ({took}s after arming), "
       f"the compare flagged it, auto_triage logged it {triaged}s later"
       + (f"; {', '.join(notes)}" if notes else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
