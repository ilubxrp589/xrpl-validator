#!/usr/bin/env python3
"""Fetch mainnet ledgers with their full metadata, as answer keys (independence tasks 2.1, 2.5, 2.6).

Each ledger is fetched twice from a public full-history server (s2.ripple.com by default; never
.39's xrpld, whose ledger drive is worn): once with transactions + expand as JSON, once with
binary:true (tx_blob + meta hex, the byte-exact record). One request at a time, at least 1 s apart.
On a rate limit (HTTP 429, slowDown, tooBusy) or a network error it backs off and retries at most
twice, then skips the ledger and logs it.

A ledger is kept only when both answers are validated and name the same ledger hash, list the same
transactions, and the binary transactions and metadata rebuild the header's transaction_hash (the
transaction tree root). The header carries no transaction count, so the root stands in for the count
check, and it is the stronger check.

  python3 scripts/fetch_meta_ledgers.py --out DIR [--rpc URL] (SEQ... | --seqs-from-vectors DIR)
Writes DIR/json/<seq>.json and DIR/bin/<seq>.json (the RPC result objects) and appends to
DIR/fetch.log. A ledger already in both folders is not fetched again. Keep DIR out of git.
"""
import argparse
import hashlib
import json
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

RPC = "https://s2.ripple.com:51234"
GAP = 1.0  # seconds from the end of one call to the start of the next
BACKOFF = (10, 30)  # seconds before the first and the second retry
BUSY = {"slowDown", "tooBusy"}
ZERO = bytes(32)


def sha512half(*parts):
    return hashlib.sha512(b"".join(parts)).digest()[:32]


def vl(n):
    """A serialized length prefix."""
    if n <= 192:
        return bytes([n])
    if n <= 12480:
        n -= 193
        return bytes([193 + (n >> 8), n & 0xFF])
    if n <= 918744:
        n -= 12481
        return bytes([241 + (n >> 16), (n >> 8) & 0xFF, n & 0xFF])
    raise ValueError(f"length {n} is too long for a length prefix")


def tx_tree(txs):
    """(root, txids) of the SHAMap over (tx_blob, meta) pairs: the header's transaction_hash."""
    leaves = {}
    for blob, meta in txs:
        txid = sha512half(b"TXN\x00", blob)
        leaves[txid] = sha512half(b"SND\x00", vl(len(blob)), blob, vl(len(meta)), meta, txid)

    def node(keys, depth):
        if len(keys) == 1 and depth > 0:
            return leaves[keys[0]]
        children = [ZERO] * 16
        for nibble in range(16):
            shift = 4 if depth % 2 == 0 else 0
            branch = [k for k in keys if (k[depth // 2] >> shift) & 0x0F == nibble]
            if branch:
                children[nibble] = node(branch, depth + 1)
        return sha512half(b"MIN\x00", *children)

    keys = sorted(leaves)
    return (node(keys, 0) if keys else ZERO), keys


class Server:
    def __init__(self, url):
        self.url, self.last = url, 0.0

    def ledger(self, params):
        """(result, None), or (None, why) after the retries."""
        body = json.dumps({"method": "ledger", "params": [params]}).encode()
        why = "?"
        for attempt in range(len(BACKOFF) + 1):
            if attempt:
                time.sleep(BACKOFF[attempt - 1])
            time.sleep(max(0.0, self.last + GAP - time.monotonic()))
            try:
                req = urllib.request.Request(self.url, data=body, headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(req, timeout=120) as r:
                    result = json.loads(r.read()).get("result", {})
            except urllib.error.HTTPError as e:
                why = f"HTTP {e.code}"
                if e.code in (429, 503):
                    continue
                return None, why
            except (urllib.error.URLError, TimeoutError, OSError, ValueError) as e:
                why = f"network: {e}"
                continue
            finally:
                self.last = time.monotonic()
            if result.get("status") == "success":
                return result, None
            why = result.get("error", "no status")
            if why not in BUSY:
                return None, why
        return None, f"{why}, after {len(BACKOFF)} retries"


def verify(j, b):
    """None when the two answers agree with each other and with the header, else what's wrong."""
    if not (j.get("validated") and b.get("validated")):
        return "not validated"
    if str(j.get("ledger_hash", "")).upper() != str(b.get("ledger_hash", "")).upper():
        return "the JSON and binary answers name different ledgers"
    jtx = j.get("ledger", {}).get("transactions", [])
    btx = b.get("ledger", {}).get("transactions", [])
    if any(not isinstance(t.get("metaData") or t.get("meta"), dict) for t in jtx):
        return "a JSON transaction without metadata"
    try:
        pairs = [(bytes.fromhex(t["tx_blob"]), bytes.fromhex(t.get("meta") or t["metaData"])) for t in btx]
    except (KeyError, TypeError, ValueError):
        return "a binary transaction without tx_blob and meta"
    root, txids = tx_tree(pairs)
    header = str(j.get("ledger", {}).get("transaction_hash", "")).upper()
    if root.hex().upper() != header:
        return f"tx tree root {root.hex().upper()[:12]} != header {header[:12]} ({len(pairs)} txs)"
    if sorted(t.get("hash", "").upper() for t in jtx) != [k.hex().upper() for k in txids]:
        return f"the JSON lists {len(jtx)} transactions, the binary {len(btx)}"
    return None


def write(path, result):
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(result))
    tmp.rename(path)


def seqs_from_vectors(folder):
    """The mainnet ledgers named in the vector file names (testnet and devnet ones left out)."""
    seqs = set()
    for path in Path(folder).glob("*.json"):
        m = re.search(r"_(\d{9})\.json$", path.name)
        if m and "testnet" not in path.name and "devnet" not in path.name:
            seqs.add(int(m.group(1)))
    return sorted(seqs)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", required=True)
    p.add_argument("--rpc", default=RPC)
    p.add_argument("--seqs-from-vectors")
    p.add_argument("seqs", nargs="*", type=int)
    a = p.parse_args()
    seqs = sorted(set(a.seqs) | set(seqs_from_vectors(a.seqs_from_vectors) if a.seqs_from_vectors else []))
    if not seqs:
        p.error("no ledgers to fetch")
    out = Path(a.out).expanduser()
    (out / "json").mkdir(parents=True, exist_ok=True)
    (out / "bin").mkdir(parents=True, exist_ok=True)
    server, kept, skipped = Server(a.rpc), 0, 0
    with open(out / "fetch.log", "a") as log:
        def note(line):
            line = f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {line}"
            print(line, flush=True)
            log.write(line + "\n")
            log.flush()

        note(f"start: {len(seqs)} ledgers from {a.rpc}")
        for seq in seqs:
            jpath, bpath = out / "json" / f"{seq}.json", out / "bin" / f"{seq}.json"
            if jpath.exists() and bpath.exists():
                kept += 1
                continue
            j, why = server.ledger({"ledger_index": seq, "transactions": True, "expand": True, "binary": False})
            b = None
            if j is not None:
                b, why = server.ledger({"ledger_index": seq, "transactions": True, "expand": True, "binary": True})
                why = f"binary: {why}" if b is None else verify(j, b)
            else:
                why = f"json: {why}"
            if why:
                skipped += 1
                note(f"{seq} SKIP {why}")
                continue
            write(jpath, j)
            write(bpath, b)
            kept += 1
            note(f"{seq} OK {len(j['ledger']['transactions'])} txs, root {j['ledger']['transaction_hash'][:12]}")
        note(f"done: {kept} kept, {skipped} skipped")
    return 0 if kept else 1


if __name__ == "__main__":
    sys.exit(main())
