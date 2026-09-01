#!/usr/bin/env python3
"""fetch_tx_bundle.py — build a byte-exact per-tx vector bundle.

The millisecond drill for PRE-OK value divergences: a single transaction's
REAL pre-images (every Modified/Deleted node at the parent ledger, plus the
owner-directory ROOTS the meta never shows, plus the parent hash pseudo-
accounts derive from), the real tx JSON, and the canonical post bytes of
chosen target keys. A test hydrates the pre-images the mirror way, applies
the tx through the full pipeline, and byte-compares the targets — red is a
reproducer, green is a regression pin (see amm_create_vector.rs /
escrow_create_vector.rs, findings 45-47).

Usage: fetch_tx_bundle.py <tx_hash_prefix> <ledger_seq> <out.json> [target_key ...]
       (no targets → every Created/Modified node becomes a target)
"""
import hashlib
import json
import sys
import urllib.request

RPC = "http://127.0.0.1:5005/"


def rpc(method, params):
    req = urllib.request.Request(
        RPC,
        data=json.dumps({"method": method, "params": [params]}).encode(),
        headers={"Content-Type": "application/json"},
    )
    return json.load(urllib.request.urlopen(req, timeout=30))["result"]


def main():
    pfx, seq, out = sys.argv[1].upper(), int(sys.argv[2]), sys.argv[3]
    want_targets = [t.upper() for t in sys.argv[4:]]

    led = rpc("ledger", {"ledger_index": seq, "transactions": True, "expand": True})
    txs = [t for t in led["ledger"]["transactions"] if t.get("hash", "").startswith(pfx)]
    if len(txs) != 1:
        sys.exit(f"tx prefix {pfx} matched {len(txs)} txs in #{seq}")
    tx = txs[0]
    meta = tx["metaData"]

    # A target is only honest when THIS tx is the key's LAST toucher in the
    # ledger — `ledger_entry@seq` is the post-LEDGER image, and the bot txs
    # this drill exists for fire several times per ledger (#106688646's
    # offer read back a later tx's PreviousTxnID). Same on the PRE side: an
    # EARLIER toucher makes the parent-ledger image stale for this tx.
    my_index = meta["TransactionIndex"]
    touched_later, touched_earlier = set(), set()
    for other in led["ledger"]["transactions"]:
        om = other.get("metaData", {})
        oi = om.get("TransactionIndex")
        if oi is None or oi == my_index:
            continue
        for n in om.get("AffectedNodes", []):
            for v in n.values():
                (touched_later if oi > my_index else touched_earlier).add(v["LedgerIndex"])

    pre, targets = {}, {}
    for n in meta["AffectedNodes"]:
        for kind, v in n.items():
            li = v["LedgerIndex"]
            if kind != "CreatedNode":
                r = rpc("ledger_entry", {"index": li, "ledger_index": seq - 1, "binary": True})
                if r.get("node_binary"):
                    pre[li] = r["node_binary"]
                if li in touched_earlier:
                    print(f"WARN: pre {li[:16]}… touched by an EARLIER tx — parent image is stale for this tx")
            if kind != "DeletedNode" and (not want_targets or li in want_targets):
                if li in touched_later:
                    print(f"note: target {li[:16]}… dropped (touched by a LATER tx; post-ledger image is not this tx's)")
                    continue
                r = rpc("ledger_entry", {"index": li, "ledger_index": seq, "binary": True})
                if r.get("node_binary"):
                    targets[li] = r["node_binary"]

    # Owner-dir ROOTS for every account named anywhere in the tx (dir inserts
    # READ the root they don't always write — the AMMCreate bundle lesson).
    accts = set()

    def walk(v):
        if isinstance(v, str) and v.startswith("r") and 25 <= len(v) <= 35:
            accts.add(v)
        elif isinstance(v, dict):
            for x in v.values():
                walk(x)
        elif isinstance(v, list):
            for x in v:
                walk(x)

    walk({k: v for k, v in tx.items() if k != "metaData"})

    ALPHABET = "rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz"

    def acct_id(addr):
        n = 0
        for ch in addr:
            n = n * 58 + ALPHABET.index(ch)
        raw = n.to_bytes((n.bit_length() + 7) // 8, "big")
        # Leading alphabet-zero chars ('r') encode leading zero bytes.
        leading = len(addr) - len(addr.lstrip("r"))
        raw = b"\x00" * leading + raw
        return raw[1:21].hex().upper()

    def fetch_key(idx):
        r = rpc("ledger_entry", {"index": idx, "ledger_index": seq - 1, "binary": True})
        return r.get("node_binary")

    # Accounts hiding inside touched OBJECTS (an NFT offer's Owner = the
    # seller whose page chain the owns-token walk reads; a line's peer; …),
    # plus the READ-ONLY structures the meta never shows: the quality page a
    # PARTIALLY consumed offer sits in (its BookDirectory), and the AMM
    # object behind a pool fill (the pseudo account root's AMMID).
    book_dirs, amm_ids = set(), set()
    for li in list(pre):
        try:
            rj = rpc("ledger_entry", {"index": li, "ledger_index": seq - 1})
            node = rj.get("node") or {}
            walk(node)
            if node.get("BookDirectory"):
                book_dirs.add(node["BookDirectory"].upper())
            if node.get("AMMID"):
                amm_ids.add(node["AMMID"].upper())
        except Exception:
            pass

    for idx in sorted(amm_ids):
        try:
            nb = fetch_key(idx)
            if nb and idx not in pre:
                pre[idx] = nb
        except Exception:
            pass

    # An OfferCreate READS book heads it never writes: the direct book's tip
    # prices strand admission (multi-strand vs single), and the two XRP
    # bridge legs compete with it — a bundle without them walks a different
    # topology than the live mirror did (#106688646 crossed on mainnet,
    # tecKILLED in its first bundle). Seed the tip offers of every book the
    # walk can consult; their pages and owners ride the existing sweeps.
    if tx.get("TransactionType") == "OfferCreate":
        def cur(v):
            if isinstance(v, dict):
                return {"currency": v["currency"], "issuer": v["issuer"]}
            return {"currency": "XRP"}
        gets, pays, xrp = cur(tx.get("TakerGets")), cur(tx.get("TakerPays")), {"currency": "XRP"}
        pairs = [(gets, pays), (pays, gets)]
        if gets != xrp and pays != xrp:
            pairs += [(gets, xrp), (xrp, gets), (xrp, pays), (pays, xrp)]
        for tg, tp in pairs:
            try:
                r = rpc("book_offers", {
                    "taker_gets": tg, "taker_pays": tp,
                    "ledger_index": seq - 1, "limit": 8,
                })
                for o in r.get("offers", []):
                    oidx = o.get("index", "").upper()
                    if oidx and oidx not in pre:
                        ob = fetch_key(oidx)
                        if ob:
                            pre[oidx] = ob
                    walk(o)
                    if o.get("BookDirectory"):
                        book_dirs.add(o["BookDirectory"].upper())
            except Exception:
                pass

    # Book pages: the walk enters the page whether or not the tx writes it
    # (the offer_ulp bundle went tecKILLED for want of one). Hydrate the
    # page chain and every offer it lists; listed owners join the account
    # sweep so their funding state hydrates too.
    def dir_page(root_hex, n):
        # keylet::page — page 0 IS the root; page n hashes the DIR_NODE
        # space ('d' = 0x0064) with the root and the page number.
        if n == 0:
            return root_hex.upper()
        h = hashlib.sha512(b"\x00d" + bytes.fromhex(root_hex) + n.to_bytes(8, "big"))
        return h.digest()[:32].hex().upper()

    for bd in sorted(book_dirs):
        n, seen = 0, set()
        try:
            while n not in seen:
                seen.add(n)
                page = dir_page(bd, n)
                nb = fetch_key(page)
                if not nb:
                    break
                if page not in pre:
                    pre[page] = nb
                rj = rpc("ledger_entry", {"index": page, "ledger_index": seq - 1})
                node = rj.get("node") or {}
                for oidx in node.get("Indexes", []):
                    oidx = oidx.upper()
                    if oidx in pre:
                        continue
                    ob = fetch_key(oidx)
                    if ob:
                        pre[oidx] = ob
                        oj = rpc("ledger_entry", {"index": oidx, "ledger_index": seq - 1})
                        walk(oj.get("node") or {})
                nxt = node.get("IndexNext")
                if not nxt or int(str(nxt), 16) == 0:
                    break
                n = int(str(nxt), 16)
        except Exception:
            pass

    for a in sorted(accts):
        try:
            r = rpc("ledger_entry", {"directory": {"owner": a}, "ledger_index": seq - 1, "binary": True})
            if r.get("node_binary") and r.get("index") not in pre:
                pre[r["index"]] = r["node_binary"]
            # The account roots themselves are read by nearly every transactor.
            r2 = rpc("ledger_entry", {"account_root": a, "ledger_index": seq - 1, "binary": True})
            if r2.get("node_binary") and r2.get("index") not in pre:
                pre[r2["index"]] = r2["node_binary"]
            # NFT page chain: the token walk ENTERS at the FFFF… page and
            # follows PreviousPageMin — pages the meta never touches are
            # still read (the F48 bundle went tecNO_PERMISSION for want of
            # them: an unhydrated page makes the owns-token check vacuous).
            page = acct_id(a) + "F" * 24
            for _ in range(100000):
                nb = fetch_key(page)
                if not nb:
                    break
                if page not in pre:
                    pre[page] = nb
                rj = rpc("ledger_entry", {"index": page, "ledger_index": seq - 1})
                prev = (rj.get("node") or {}).get("PreviousPageMin")
                if not prev:
                    break
                page = prev.upper()
        except Exception:
            pass

    parent = rpc("ledger", {"ledger_index": seq - 1})["ledger"]
    cur = rpc("ledger", {"ledger_index": seq})["ledger"]
    tx.pop("metaData", None)
    bundle = {
        "tx": tx,
        "tx_index_note": meta.get("TransactionIndex"),
        "seq": seq,
        "parent_hash": cur["parent_hash"],
        "parent_close_time": int(parent["close_time"]),
        "total_coins": int(parent["total_coins"]),
        "pre": pre,
        "expect": targets,
    }
    with open(out, "w") as f:
        json.dump(bundle, f, indent=1)
    print(f"{pfx}@{seq}: pre={len(pre)} targets={len(targets)} accts={len(accts)} -> {out}")


if __name__ == "__main__":
    main()
