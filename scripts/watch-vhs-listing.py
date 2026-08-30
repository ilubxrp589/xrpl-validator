#!/usr/bin/env python3
"""Watch VHS (data.xrpl.org) for our validator appearing in the list.

We are currently NOT listed (manifest rejected: master==ephemeral). The day
VHS accepts us — and especially the day server_version renders (HV 1.0.0
teaser follow-up) — ping Telegram once per state change. Cron: */30.
"""
import json, os, urllib.request

KEY = "nHUcQnmgbUEZTk8jRAXm1M2L2ZBFXt3MSPqJhWWiRSsvkTYjLzXi"
STAMP = os.path.expanduser("~/.vhs-listing-state.json")
CREDS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "copilot", "config.local.json")


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
        with urllib.request.urlopen("https://data.xrpl.org/v1/network/validators", timeout=20) as r:
            vals = json.load(r).get("validators", [])
    except Exception:
        return  # transient; next tick retries
    ours = next((v for v in vals if KEY in (v.get("validation_public_key"), v.get("master_key"), v.get("signing_key"))), None)
    state = {"listed": bool(ours), "server_version": (ours or {}).get("server_version")}
    try:
        prev = json.load(open(STAMP))
    except Exception:
        prev = {}
    if state != prev:
        json.dump(state, open(STAMP, "w"))
        if state["listed"] and not prev.get("listed"):
            tg(f"🎉 VHS now LISTS our validator! server_version={state['server_version']!r} domain={(ours or {}).get('domain')!r}")
        if state["server_version"] and state["server_version"] != prev.get("server_version"):
            tg(f"🛰 VHS server_version renders: {state['server_version']} — the teaser follow-up is live-able.")
        if prev.get("listed") and not state["listed"]:
            tg("⚠️ VHS de-listed our validator (was listed, now absent).")


if __name__ == "__main__":
    main()
