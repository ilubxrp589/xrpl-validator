#!/usr/bin/env python3
"""Hourly shadow-scout: the live-shadow twin of the probe scout's 🧬 alerts.

Reads m3060's native_shadow.jsonl for receipts newer than the last run and
telegrams a classified digest: PRE-OK byte-diffs are engine findings (the
fix queue), STALE[..] are mirror-input events, ter-mismatches are always
urgent. Also verifies the validator is ADVANCING (the 2026-08-31 watcher gap:
death and divergence were covered, a frozen sync was not).

State: ~/.shadow-scout-state.json   Timer: hourly at :20 (clear of 03:00-15).
"""
import json, os, subprocess, urllib.request

M3060 = "m3060@10.0.0.97"
JSONL = "/mnt/xrpl-data/native_shadow.jsonl"
STAMP = os.path.expanduser("~/.shadow-scout-state.json")
CREDS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "copilot", "config.local.json")


def ssh(cmd, timeout=25):
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", M3060, cmd],
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout


def tg(msg):
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


def main():
    try:
        prev = json.load(open(STAMP))
    except Exception:
        prev = {"last_seq": 0, "last_synced": 0}

    # Advancement check — a frozen sync is an outage the divergence view
    # cannot see.
    synced = 0
    try:
        api = json.loads(ssh("curl -s --max-time 5 localhost:3777/api/engine"))
        synced = int(api.get("last_synced") or api.get("ledger_seq") or 0)
    except Exception:
        pass
    if synced and prev.get("last_synced") and synced <= prev["last_synced"]:
        tg(f"🔴 shadow-scout: validator NOT ADVANCING — last_synced still #{synced} an hour later")
    elif not synced:
        tg("🔴 shadow-scout: engine API unreachable on m3060 (process down?)")

    # New receipts since the last run.
    raw = ssh(f"tail -c 400000 {JSONL} 2>/dev/null")
    engine, mirror, ter = [], [], []
    top = prev.get("last_seq", 0)
    for line in raw.splitlines():
        try:
            d = json.loads(line)
        except Exception:
            continue
        seq = d.get("seq", 0)
        if seq <= prev.get("last_seq", 0):
            continue
        top = max(top, seq)
        for t in d.get("ter_mismatch", []):
            ter.append(f"#{seq} {str(t)[:120]}")
        for b in d.get("byte_diff", []):
            b = str(b)
            if "PRE-OK" in b:
                engine.append(f"#{seq} {b[:110]}")
            elif "STALE[" in b or "PRE-UNKNOWN" in b:
                mirror.append(f"#{seq} {b[:110]}")

    if ter or engine or mirror:
        parts = []
        if ter:
            parts.append("🔴 TER mismatches:\n" + "\n".join(ter[:4]))
        if engine:
            parts.append("🧬 ENGINE findings (PRE-OK):\n" + "\n".join(engine[:4]))
        if mirror:
            parts.append(f"🌫 mirror-input events ×{len(mirror)}:\n" + "\n".join(mirror[:3]))
        tg("shadow-scout (hourly):\n" + "\n\n".join(parts))

    json.dump({"last_seq": top, "last_synced": synced or prev.get("last_synced", 0)}, open(STAMP, "w"))


if __name__ == "__main__":
    main()
