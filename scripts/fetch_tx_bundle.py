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

Same-ledger staleness: the bot txs this drill exists for fire several
times per ledger, so a key touched by an EARLIER tx has a parent image that
is NOT this tx's pre-image (#106674447's leg-B maker line was credited 10
RLUSD by tx#5 before the crossing read it). Such keys are rebuilt from the
last earlier toucher's BINARY meta — ModifiedNode.FinalFields is the node
byte-exact minus the three fields metadata never carries (LedgerEntryType,
and the thread PreviousTxnID/PreviousTxnLgrSeq the toucher stamped with its
own hash and this ledger). A CreatedNode's NewFields omits default-valued
fields (Flags=0, BookNode=0, …) so it cannot be rebuilt without the type
template — that stays a WARN.

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


# ── minimal XRPL binary walker: just enough to slice a meta's FinalFields ──
# Field header: high nibble = type, low nibble = field; a zero nibble means
# the code follows in the next byte. Every field's raw bytes are kept as-is,
# so the rebuilt node is byte-exact by construction — nothing re-serializes.
_FIXED = {1: 2, 2: 4, 3: 8, 4: 16, 5: 32, 9: 12, 16: 1, 17: 20, 21: 24, 26: 20}


def _vl(b, i):
    n = b[i]
    i += 1
    if n <= 192:
        return n, i
    if n <= 240:
        n2 = b[i]
        return 193 + (n - 193) * 256 + n2, i + 1
    n2, n3 = b[i], b[i + 1]
    return 12481 + (n - 241) * 65536 + n2 * 256 + n3, i + 2


def _hdr(b, i):
    t, f = b[i] >> 4, b[i] & 0x0F
    i += 1
    if t == 0:
        t = b[i]
        i += 1
    if f == 0:
        f = b[i]
        i += 1
    return t, f, i


def _skip(b, i, t):
    """Index just past a value of ST type t that starts at i."""
    if t in _FIXED:
        return i + _FIXED[t]
    if t == 6:  # Amount: IOU 48 (0x80), MPT 33 (0x20), XRP 8
        return i + (48 if b[i] & 0x80 else 33 if b[i] & 0x20 else 8)
    if t in (7, 8, 19):  # Blob / AccountID / Vector256: VL-prefixed
        n, i = _vl(b, i)
        return i + n
    if t == 14:  # STObject: fields until the 0xE1 end marker
        while b[i] != 0xE1:
            ft, _, i = _hdr(b, i)
            i = _skip(b, i, ft)
        return i + 1
    if t == 15:  # STArray: (object header, object body) until 0xF1
        while b[i] != 0xF1:
            _, _, i = _hdr(b, i)
            i = _skip(b, i, 14)
        return i + 1
    if t == 24:  # Issue: XRP currency alone, else currency + issuer
        return i + (20 if b[i:i + 20] == bytes(20) else 40)
    if t == 25:  # XChainBridge: door, issue, door, issue
        for sub in (8, 24, 8, 24):
            i = _skip(b, i, sub)
        return i
    if t == 18:  # PathSet: steps (type byte + 20-byte parts) / 0xFF / 0x00
        while True:
            st = b[i]
            i += 1
            if st == 0x00:
                return i
            if st == 0xFF:
                continue
            i += 20 * (bool(st & 0x01) + bool(st & 0x10) + bool(st & 0x20))
    raise ValueError(f"unknown ST type {t}")


def _obj(b, i):
    """[(type, field, start, stop)] of the object body at i, and the index past its 0xE1."""
    out = []
    while b[i] != 0xE1:
        s = i
        t, f, i = _hdr(b, i)
        i = _skip(b, i, t)
        out.append((t, f, s, i))
    return out, i + 1


def rebuild_post_image(meta_hex, li, tx_hash, seq):
    """(kind, hex) — the byte-exact image of key `li` AFTER the tx whose
    binary meta this is: its ModifiedNode.FinalFields plus LedgerEntryType
    (0x11, from the node) and the thread stamp PreviousTxnLgrSeq (0x25 =
    this ledger) / PreviousTxnID (0x55 = this tx), in canonical (type,
    field) order. hex is None unless the node is a ModifiedNode."""
    b = bytes.fromhex(meta_hex)
    i, top = 0, []
    while i < len(b):
        s = i
        t, f, i = _hdr(b, i)
        i = _skip(b, i, t)
        top.append((t, f, s, i))
    for t, f, s, _ in top:
        if (t, f) != (15, 8):  # AffectedNodes
            continue
        _, _, j = _hdr(b, s)
        while b[j] != 0xF1:
            _, nf, j = _hdr(b, j)  # 3 Created / 4 Deleted / 5 Modified
            fields, j = _obj(b, j)
            fm = {(t2, f2): (s2, e2) for t2, f2, s2, e2 in fields}
            if (5, 6) not in fm:  # LedgerIndex
                continue
            if b[fm[(5, 6)][1] - 32:fm[(5, 6)][1]].hex().upper() != li:
                continue
            kind = {3: "CreatedNode", 4: "DeletedNode", 5: "ModifiedNode"}.get(nf, f"node{nf}")
            if kind != "ModifiedNode" or (1, 1) not in fm or (14, 7) not in fm:
                return kind, None
            _, _, k = _hdr(b, fm[(14, 7)][0])  # into FinalFields
            inner, _ = _obj(b, k)
            raw = {(t3, f3): b[s3:e3] for t3, f3, s3, e3 in inner}
            raw.setdefault((1, 1), b[fm[(1, 1)][0]:fm[(1, 1)][1]])
            raw.setdefault((2, 5), b"\x25" + seq.to_bytes(4, "big"))
            raw.setdefault((5, 5), b"\x55" + bytes.fromhex(tx_hash))
            return kind, b"".join(raw[key] for key in sorted(raw)).hex().upper()
    return None, None


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
    touched_later = set()
    earlier = {}  # key → (index, kind, hash) of its LAST toucher before this tx
    for other in led["ledger"]["transactions"]:
        om = other.get("metaData", {})
        oi = om.get("TransactionIndex")
        if oi is None or oi == my_index:
            continue
        for n in om.get("AffectedNodes", []):
            for kind, v in n.items():
                li = v["LedgerIndex"]
                if oi > my_index:
                    touched_later.add(li)
                elif li not in earlier or earlier[li][0] < oi:
                    earlier[li] = (oi, kind, other["hash"].upper())

    pre, targets = {}, {}
    for n in meta["AffectedNodes"]:
        for kind, v in n.items():
            li = v["LedgerIndex"]
            if kind != "CreatedNode":
                r = rpc("ledger_entry", {"index": li, "ledger_index": seq - 1, "binary": True})
                if r.get("node_binary"):
                    pre[li] = r["node_binary"]
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

    # Asset/Asset2 transactors (AMMBid/Vote/Deposit/Withdraw/Delete) read the
    # AMM object, the bidder/voter's LP line and every slot-holder's LP line —
    # none of which a tec's meta shows (it touches only the fee). The F49
    # drill fetched these by hand; do it always.
    if "Asset" in tx and "Asset2" in tx:
        def asset_param(a):
            if a.get("currency") == "XRP" and not a.get("issuer"):
                return {"currency": "XRP"}
            return {"currency": a["currency"], "issuer": a["issuer"]}
        try:
            r = rpc("ledger_entry", {
                "amm": {"asset": asset_param(tx["Asset"]), "asset2": asset_param(tx["Asset2"])},
                "ledger_index": seq - 1,
            })
            node = r.get("node") or {}
            idx = (r.get("index") or "").upper()
            if idx:
                nb = fetch_key(idx)
                if nb and idx not in pre:
                    pre[idx] = nb
            walk(node)
            lp = node.get("LPTokenBalance") or {}
            lp_cur, lp_iss = lp.get("currency"), lp.get("issuer")
            holders = {tx.get("Account")}
            for vs in node.get("VoteSlots") or []:
                holders.add((vs.get("VoteEntry") or {}).get("Account"))
            holders.add((node.get("AuctionSlot") or {}).get("Account"))
            for h in holders:
                if not h or not lp_cur or not lp_iss:
                    continue
                try:
                    rr = rpc("ledger_entry", {
                        "ripple_state": {"currency": lp_cur, "accounts": [h, lp_iss]},
                        "ledger_index": seq - 1, "binary": True,
                    })
                    li = (rr.get("index") or "").upper()
                    if rr.get("node_binary") and li and li not in pre:
                        pre[li] = rr["node_binary"]
                except Exception:
                    pass
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
                    "ledger_index": seq - 1, "limit": 32,
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

    # Same-ledger staleness (see the module docstring): every key the bundle
    # carries — written by this tx OR merely read by its walk — whose last
    # earlier toucher MODIFIED it gets that toucher's post-image; one the
    # toucher DELETED leaves the bundle (it does not exist for this tx); one
    # the toucher CREATED stays a WARN (NewFields omit defaults). This tx's
    # own node-level PreviousTxnID must name that toucher — the check that
    # the "last toucher" bookkeeping and the rebuild agree.
    my_prev = {v["LedgerIndex"]: (v.get("PreviousTxnID") or "").upper()
               for n in meta["AffectedNodes"] for v in n.values()}
    meta_keys = {v["LedgerIndex"] for n in meta["AffectedNodes"]
                 for kind, v in n.items() if kind != "CreatedNode"}
    bin_meta = {}
    for li in sorted(set(pre) | meta_keys):
        if li not in earlier:
            continue
        oi, kind, h = earlier[li]
        where = "parent image is stale" if li in pre else "absent from the bundle"
        if kind == "DeletedNode":
            if pre.pop(li, None) is not None:
                print(f"note: pre {li[:16]}… dropped (deleted by earlier tx#{oi} {h[:8]})")
            continue
        if kind == "CreatedNode":
            print(f"WARN: pre {li[:16]}… CREATED by earlier tx#{oi} {h[:8]} — NewFields omit defaults, not rebuilt; {where}")
            continue
        if my_prev.get(li) and my_prev[li] != h:
            print(f"WARN: pre {li[:16]}… thread names {my_prev[li][:8]} but last earlier toucher is tx#{oi} {h[:8]} — not rebuilt; {where}")
            continue
        try:
            if h not in bin_meta:
                r = rpc("tx", {"transaction": h, "binary": True})
                bin_meta[h] = r.get("meta") if isinstance(r.get("meta"), str) else r.get("meta_blob")
            k2, img = rebuild_post_image(bin_meta[h], li, h, seq)
        except Exception as e:  # noqa: BLE001 — a parse failure must not poison the bundle
            k2, img = f"parse error: {e}", None
        if img:
            pre[li] = img
            print(f"note: pre {li[:16]}… rebuilt from earlier tx#{oi} {h[:8]} FinalFields ({len(img) // 2} bytes)")
        else:
            print(f"WARN: pre {li[:16]}… touched by earlier tx#{oi} {h[:8]} as {k2} — not rebuilt; {where}")

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
