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
ledger's BINARY metas, two ways that cross-check each other:
  BACKWARD — the last earlier toucher's ModifiedNode.FinalFields is the node
    byte-exact minus the three fields metadata never carries (LedgerEntryType
    and the thread PreviousTxnID/PreviousTxnLgrSeq it stamped with its own
    hash and this ledger). Thread-only touches (threadTx on an owner's
    AccountRoot) carry no fields: walk back to the nearest full image, or the
    parent ledger's, and re-stamp. A CreatedNode's NewFields omits default-
    valued fields (Flags=0, LowNode=0, …) so it cannot stand alone.
  FORWARD — the first toucher at or after this tx (this tx itself when it
    writes the key) records in PreviousFields exactly what it changed, so its
    FinalFields with those put back is the image just before it = this tx's
    pre-image; no such toucher → the post-ledger image. Its one ambiguity is
    a field that toucher ADDED (in FinalFields, not PreviousFields), so when
    the key was CREATED in this ledger every field beyond the creator's
    NewFields must be default-valued, else the key is refused.
  DIRECTORY PAGES are the exception to both: sfIndexes is sMD_Never, so no
    meta ever carries a page's entries. Their entries are REPLAYED instead:
    every object an earlier tx created/deleted names its pages (OwnerNode,
    BookNode, LowNode/HighNode, …), owner pages take sorted inserts and book
    pages appends (rippled dirInsert vs dirAppend), removals keep order. The
    same replay continued through the later txs must land on the post-ledger
    page, or the page is refused.
Keys this tx writes that are NOT stale get the forward derivation (and pages
the entry replay) anyway, as a self-check against the parent ledger.

Usage: fetch_tx_bundle.py <tx_hash_prefix> <ledger_seq> <out.json> [target_key ...]
       (no targets → every Created/Modified node becomes a target)
"""
import bisect
import hashlib
import json
import sys
import urllib.request

import os
RPC = os.environ.get("XRPL_RPC", "http://127.0.0.1:5005/")


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


def _fields_of(b, i, end):
    """{(type, field): raw header+value bytes} of the fields in b[i:end)."""
    out = {}
    while i < end:
        s = i
        t, f, i = _hdr(b, i)
        i = _skip(b, i, t)
        out[(t, f)] = b[s:i]
    return out


def meta_node(meta_hex, li):
    """The AffectedNode of key `li` in a binary meta, as {"kind", "node",
    "finals", "prevs", "news"} with the field maps as {(type, field): raw
    header+value bytes} (None for an absent sub-object). finals is None on
    a thread-only touch: rippled's threadTx stamps an owner's AccountRoot
    with the new PreviousTxnID/PreviousTxnLgrSeq and records nothing else.
    None when the key is not in this meta."""
    b = bytes.fromhex(meta_hex)
    top = _fields_of(b, 0, len(b))
    if (15, 8) not in top:  # AffectedNodes
        return None
    arr = top[(15, 8)]
    _, _, j = _hdr(arr, 0)
    while arr[j] != 0xF1:
        _, nf, j = _hdr(arr, j)  # 3 CreatedNode / 4 DeletedNode / 5 ModifiedNode
        fields, j = _obj(arr, j)
        fm = {(t, f): arr[s:e] for t, f, s, e in fields}
        if (5, 6) not in fm or fm[(5, 6)][-32:].hex().upper() != li:  # LedgerIndex
            continue

        def sub(code):  # a nested STObject's own fields: header, body, 0xE1
            if code not in fm:
                return None
            fb = fm[code]
            _, _, k = _hdr(fb, 0)
            return _fields_of(fb, k, len(fb) - 1)

        return {"kind": {3: "CreatedNode", 4: "DeletedNode", 5: "ModifiedNode"}.get(nf, f"node{nf}"),
                "node": fm, "finals": sub((14, 7)), "prevs": sub((14, 6)), "news": sub((14, 8))}
    return None


def stamped(fields, let_raw, tx_hash, seq):
    """Raw field map of a node: LedgerEntryType (0x11) supplied when missing,
    and — when tx_hash is given — the thread stamp PreviousTxnLgrSeq (0x25) /
    PreviousTxnID (0x55) REPLACED by (seq, tx_hash). Every other field's
    bytes pass through untouched — nothing re-serializes."""
    raw = dict(fields)
    if let_raw and (1, 1) not in raw:
        raw[(1, 1)] = let_raw
    if tx_hash:
        raw[(2, 5)] = b"\x25" + seq.to_bytes(4, "big")
        raw[(5, 5)] = b"\x55" + bytes.fromhex(tx_hash)
    return raw


def canonical(raw):
    """Node bytes from a raw field map, (type, field) order, upper hex."""
    return b"".join(raw[k] for k in sorted(raw)).hex().upper()


# rippled's CreatedNode NewFields omit every field at its default (STBase::
# isDefault), so an object rebuilt from them lacks the REQUIRED fields the
# real SLE serializes at zero — an Offer's BookNode "0" (9 bytes), Flags 0,
# an AccountRoot's OwnerCount 0. Only fields the transactor ALWAYS sets can be
# put back (an absent OPTIONAL field is indistinguishable from a default).
# (finding 143's vector: the in-ledger bid 8F73 came back 9 bytes short.)
_ALWAYS_SET = {
    0x006F: [(2, 2), (3, 3), (3, 4)],  # Offer: Flags, BookNode, OwnerNode
    0x0072: [(2, 2), (3, 7), (3, 8)],  # RippleState: Flags, LowNode, HighNode
    0x0064: [(2, 2)],  # DirectoryNode: Flags
    0x0061: [(2, 2), (2, 13)],  # AccountRoot: Flags, OwnerCount
    0x0043: [(2, 2), (3, 4), (3, 9)],  # Check: Flags, OwnerNode, DestinationNode
    0x0075: [(2, 2), (3, 4)],  # Escrow: Flags, OwnerNode (DestinationNode is optional)
    0x0078: [(2, 2), (3, 4)],  # PayChannel: Flags, OwnerNode
    0x0054: [(2, 2), (3, 4)],  # Ticket: Flags, OwnerNode
    0x0070: [(2, 2), (3, 4)],  # DepositPreauth: Flags, OwnerNode
    0x0037: [(2, 2), (3, 4), (3, 12)],  # NFTokenOffer: Flags, OwnerNode, NFTokenOfferNode
}


def readd_defaults(raw):
    """Put back the always-set fields NewFields dropped as defaults."""
    let = raw.get((1, 1))
    if not let or len(let) != 3:
        return raw
    for ty, code in _ALWAYS_SET.get(int.from_bytes(let[1:3], "big"), []):
        if (ty, code) in raw:
            continue
        hdr = bytes([(ty << 4) | code]) if code < 16 else bytes([ty << 4, code])
        raw[(ty, code)] = hdr + bytes(4 if ty == 2 else 8)
    return raw


def _is_default(t, raw):
    """Would rippled's STBase::isDefault omit this field from NewFields?"""
    _, _, i = _hdr(raw, 0)
    v = raw[i:]
    if t == 6:  # Amount: only native (XRP) zero is default
        return v == b"\x40" + bytes(7)
    if t == 8:  # AccountID: VL(20) + zero account
        return v == b"\x14" + bytes(20)
    if t in (7, 19):  # Blob / Vector256: empty
        return v == b"\x00"
    if t == 14:
        return v == b"\xe1"
    if t == 15:
        return v == b"\xf1"
    return not any(v)


def _vl_encode(n):
    if n <= 192:
        return bytes([n])
    if n <= 12480:
        n -= 193
        return bytes([193 + (n >> 8), n & 0xFF])
    n -= 12481
    return bytes([241 + (n >> 16), (n >> 8) & 0xFF, n & 0xFF])


def indexes_of(raw):
    """Entries of a raw sfIndexes field (header + VL + 32-byte keys)."""
    _, _, i = _hdr(raw, 0)
    n, i = _vl(raw, i)
    data = raw[i:i + n]
    return [data[k:k + 32].hex().upper() for k in range(0, n, 32)]


def indexes_raw(header_raw, entries):
    """A raw sfIndexes field with these entries, header bytes reused."""
    _, _, i = _hdr(header_raw, 0)
    return header_raw[:i] + _vl_encode(32 * len(entries)) + b"".join(bytes.fromhex(e) for e in entries)


LET_DIR_NODE = 0x0064

# Where an object lives: (page-number field, owner-account field). Offers and
# trust lines are special-cased (book page / two owners); SignerList has no
# account field (its owner is the tx account); NFTokenOffer's token directory
# is not modelled — the post-ledger replay check refuses a page it disturbs.
_OWNER_HINTS = {
    "Ticket": [("OwnerNode", "Account")],
    "Escrow": [("OwnerNode", "Account"), ("DestinationNode", "Destination")],
    "PayChannel": [("OwnerNode", "Account"), ("DestinationNode", "Destination")],
    "Check": [("OwnerNode", "Account"), ("DestinationNode", "Destination")],
    "DepositPreauth": [("OwnerNode", "Account")],
    "NFTokenOffer": [("OwnerNode", "Owner")],
    "DID": [("OwnerNode", "Account")],
    "Oracle": [("OwnerNode", "Owner")],
    "AMM": [("OwnerNode", "Account")],
    "MPTokenIssuance": [("OwnerNode", "Issuer")],
    "MPToken": [("OwnerNode", "Account")],
    "Credential": [("IssuerNode", "Issuer"), ("SubjectNode", "Subject")],
    "PermissionedDomain": [("OwnerNode", "Owner")],
    "Vault": [("OwnerNode", "Owner")],
    "Delegate": [("OwnerNode", "Account")],
    "Bridge": [("OwnerNode", "Account")],
    "XChainOwnedClaimID": [("OwnerNode", "Account")],
    "XChainOwnedCreateAccountClaimID": [("OwnerNode", "Account")],
}


def created_consistent(raw, news):
    """None if `raw` can be the object NewFields describes: every NewFields
    field byte-identical, every extra (non-thread) field default-valued —
    the fields rippled omits from NewFields. Else the reason."""
    for k, v in news.items():
        if raw.get(k) != v:
            return f"field {k} differs from the creator's NewFields"
    for k, v in raw.items():
        if k in news or k in ((1, 1), (2, 5), (5, 5)):
            continue
        if not _is_default(k[0], v):
            return f"extra non-default field {k} (added after creation?)"
    return None

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
    my_index, my_hash = meta["TransactionIndex"], tx["hash"].upper()
    touchers = {}  # key → [(index, kind, hash), …] ascending, this tx included
    for other in led["ledger"]["transactions"]:
        om = other.get("metaData", {})
        oi = om.get("TransactionIndex")
        if oi is None:
            continue
        for n in om.get("AffectedNodes", []):
            for kind, v in n.items():
                touchers.setdefault(v["LedgerIndex"], []).append((oi, kind, other["hash"].upper()))
    for chain in touchers.values():
        chain.sort()
    touched_later = {li for li, ch in touchers.items() if ch[-1][0] > my_index}

    pre, targets = {}, {}
    for n in meta["AffectedNodes"]:
        for kind, v in n.items():
            li = v["LedgerIndex"]
            if kind != "CreatedNode":
                r = rpc("ledger_entry", {"index": li, "ledger_index": seq - 1, "binary": True})
                if r.get("node_binary"):
                    pre[li] = r["node_binary"]
            if kind == "DeletedNode" and (not want_targets or li in want_targets):
                # Deletion pin (finding 158): an EMPTY expectation tells the
                # harnesses the apply must delete this object.
                targets[li] = ""
                continue
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

    # The pool account's whole owner directory and every LPToken line on it:
    # fixAMMv1_1's isOnlyLiquidityProvider (AMMWithdraw, finding 63) walks the
    # AMM's owner dir counting LP lines — with the other LPs' lines absent the
    # probe declares the withdrawer the sole LP and refuses with
    # tecAMM_INVALID_TOKENS (#106702692 6F7C52C0, live tesSUCCESS).
    if tx.get("TransactionType", "").startswith("AMM") and tx.get("Asset") is not None and tx.get("Asset2") is not None:
        try:
            def ap2(x):
                return {"currency": "XRP"} if x.get("currency") == "XRP" and not x.get("issuer") else {"currency": x["currency"], "issuer": x["issuer"]}
            r = rpc("ledger_entry", {"amm": {"asset": ap2(tx["Asset"]), "asset2": ap2(tx["Asset2"])}, "ledger_index": seq - 1})
            pool = (r.get("node") or {}).get("Account")
            if pool:
                d = rpc("account_objects", {"account": pool, "ledger_index": seq - 1, "limit": 400})
                for o in d.get("account_objects", []):
                    oi = (o.get("index") or "").upper()
                    if oi and oi not in pre:
                        nb = fetch_key(oi)
                        if nb:
                            pre[oi] = nb
                            walk(o)
                od = rpc("ledger_entry", {"directory": {"owner": pool}, "ledger_index": seq - 1, "binary": True})
                oi = (od.get("index") or "").upper()
                if od.get("node_binary") and oi and oi not in pre:
                    pre[oi] = od["node_binary"]
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

    # The sender's OWN trust lines for every currency the tx names — read by
    # preclaim (auth: checkAcceptAsset on TakerPays; funds: accountFunds on
    # TakerGets / SendMax) and never written by a tec (fee-only) result, so
    # a tec specimen's meta cannot surface them. #106698333 314D2290
    # (tecNO_AUTH) probed tecNO_LINE without the taker's GTA6 line.
    for f_ in ("TakerPays", "TakerGets", "Amount", "SendMax", "DeliverMin", "Amount2", "LimitAmount"):
        v = tx.get(f_)
        if not isinstance(v, dict) or not v.get("issuer") or v.get("currency") == "XRP":
            continue
        try:
            rr = rpc("ledger_entry", {
                "ripple_state": {"currency": v["currency"], "accounts": [tx["Account"], v["issuer"]]},
                "ledger_index": seq - 1, "binary": True,
            })
            li = (rr.get("index") or "").upper()
            if rr.get("node_binary") and li and li not in pre:
                pre[li] = rr["node_binary"]
            accts.add(v["issuer"])
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

    def owner_dir(addr):
        return hashlib.sha512(b"\x00O" + bytes.fromhex(acct_id(addr))).digest()[:32].hex().upper()

    def dir_ops(txj):
        """[(page_key | None, 'add' | 'del', entry_key, sorted_insert)] for
        every directory entry a tx's created/deleted objects imply. A None
        page marks a placement this script does not model."""
        ops = []
        for n in txj.get("metaData", {}).get("AffectedNodes", []):
            for kind, v in n.items():
                if kind == "ModifiedNode":
                    continue
                fl = (v.get("NewFields") if kind == "CreatedNode" else v.get("FinalFields")) or {}
                let, key, op = v.get("LedgerEntryType"), v["LedgerIndex"].upper(), "add" if kind == "CreatedNode" else "del"

                def page(field):  # NewFields omits a zero page number: absent = root page
                    return int(str(fl.get(field, "0")), 16)

                spots = []
                try:
                    if let == "Offer":
                        spots.append((owner_dir(fl["Account"]), page("OwnerNode"), True))
                        spots.append((fl["BookDirectory"].upper(), page("BookNode"), False))
                    elif let == "RippleState":
                        spots.append((owner_dir(fl["LowLimit"]["issuer"]), page("LowNode"), True))
                        spots.append((owner_dir(fl["HighLimit"]["issuer"]), page("HighNode"), True))
                    elif let == "SignerList":
                        spots.append((owner_dir(txj["Account"]), page("OwnerNode"), True))
                    elif let in _OWNER_HINTS:
                        for node_f, acct_f in _OWNER_HINTS[let]:
                            a = fl.get(acct_f)
                            if not a or (node_f == "DestinationNode" and a == fl.get("Account")):
                                continue
                            spots.append((owner_dir(a), page(node_f), True))
                    elif let in ("DirectoryNode", "NFTokenPage", "AccountRoot", "Amendments", "FeeSettings", "NegativeUNL", "LedgerHashes"):
                        continue  # not directory entries
                    else:
                        spots.append(None)
                except (KeyError, ValueError, TypeError):
                    spots.append(None)
                for sp in spots:
                    ops.append((None if sp is None else dir_page(sp[0], sp[1]), op, key, sp[2] if sp else None))
        return ops

    def replay_dir(entries, ops, page_key):
        """Entries after rippled's dirInsert/dirAppend/dirRemove for the ops
        that hit this page; None when an op is inconsistent with them."""
        entries = list(entries)
        for pk, op, key, sorted_insert in ops:
            if pk != page_key:
                continue
            if op == "del":
                if key not in entries:
                    return None
                entries.remove(key)  # order preserved
            else:
                if key in entries:
                    return None
                if sorted_insert:  # dirInsert: sort the page, insert in order
                    entries.sort()
                    entries.insert(bisect.bisect_left(entries, key), key)
                else:  # dirAppend (book pages): FIFO
                    entries.append(key)
        return entries

    ops_by_index = {}
    for other in led["ledger"]["transactions"]:
        oi = other.get("metaData", {}).get("TransactionIndex")
        if oi is not None:
            ops_by_index[oi] = dir_ops(other)

    # TOP OF THE BOOKS the crossing reads: the direct book and both XRP-bridge
    # books, whether or not any offer on them is touched. `book_offers` names
    # the best offers; their BookDirectory pages then join `book_dirs` and the
    # page sweep below pulls every offer on them. #106701383 644B3509: the
    # XRP→USD book's tip keeps rippled's bridge strand ACTIVE (multi-path fib
    # slices through eight iterations); without it the probe ran single-path.
    if tx.get("TransactionType") in ("OfferCreate", "Payment"):
        def leg_of(v):
            return {"currency": "XRP"} if isinstance(v, str) else {"currency": v["currency"], "issuer": v["issuer"]}
        gets = tx.get("TakerGets") if tx.get("TransactionType") == "OfferCreate" else tx.get("SendMax", tx.get("Amount"))
        pays = tx.get("TakerPays") if tx.get("TransactionType") == "OfferCreate" else tx.get("Amount")
        if gets is not None and pays is not None:
            g, p_ = leg_of(gets), leg_of(pays); xrp = {"currency": "XRP"}
            books = [(g, p_)]
            if g != xrp and p_ != xrp:
                books += [(g, xrp), (xrp, p_)]
            # EXPLICIT PATHS: every currency hop names a book the strand reads,
            # and an account hop re-issues the held currency (rippled's toStrand
            # sets curIssue.account from the step). #106758148 0638A180
            # (XRP->PKEG->FARM) and #106766924 DA053F84 (USDC->PLX->XLM->XRP):
            # neither hop's book was in the bundle and the probe read the
            # strand DRY while rippled filled it.
            for path in tx.get("Paths") or []:
                cur_leg = g
                for step in path:
                    nxt = None
                    if "currency" in step:
                        if step["currency"] == "XRP":
                            nxt = xrp
                        elif step.get("issuer"):
                            nxt = {"currency": step["currency"], "issuer": step["issuer"]}
                    elif step.get("account") and cur_leg != xrp:
                        nxt = {"currency": cur_leg["currency"], "issuer": step["account"]}
                    if nxt is not None and nxt != cur_leg:
                        if nxt["currency"] != cur_leg["currency"] and (cur_leg, nxt) not in books:
                            books.append((cur_leg, nxt))
                        cur_leg = nxt
                if cur_leg != p_ and cur_leg["currency"] != p_["currency"] and (cur_leg, p_) not in books:
                    books.append((cur_leg, p_))
            # DEPTH: a long sweep walks PAST every level the meta names and
            # then anchors the pool at the first level it never consumed —
            # a read-only tip no meta records. #106743104 F8084760 (1.9M XRPH
            # → RLUSD, 54 iterations): the XRPH/XRP levels 5103CE…5111805A
            # were all consumed and rippled priced iterations 50-53 against
            # 511FF98B; a 12-offer seed lacked it, our walk read the book
            # empty, and the bridge strand went unboundable — dropped, where
            # rippled ran it alone (tecPATH_PARTIAL vs tesSUCCESS). Two pages
            # of 100 — the marker continues where the first page ended.
            for tg, tp in books:
                try:
                    marker = None
                    for _page in range(2):
                        q = {"taker_gets": tp, "taker_pays": tg, "ledger_index": seq - 1, "limit": 100}
                        if marker is not None:
                            q["marker"] = marker
                        r = rpc("book_offers", q)
                        for o in r.get("offers", []):
                            oi = (o.get("index") or "").upper()
                            if oi and oi not in pre:
                                nb = fetch_key(oi)
                                if nb:
                                    pre[oi] = nb
                                    walk(o)
                            if o.get("BookDirectory"):
                                book_dirs.add(o["BookDirectory"].upper())
                        marker = r.get("marker")
                        if not marker:
                            break
                except Exception:
                    pass

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
                    if oidx not in pre:
                        ob = fetch_key(oidx)
                        if ob:
                            pre[oidx] = ob
                    oj = rpc("ledger_entry", {"index": oidx, "ledger_index": seq - 1}).get("node") or {}
                    walk(oj)
                    # The maker's FUNDING line: `accountFunds(owner, TakerGets)`
                    # decides whether the offer is live at all. Without it a
                    # book head reads as unfunded and the whole leg vanishes
                    # (#106701383 644B3509: the XRP→USD dust tip 97C11ACC kept
                    # rippled's bridge strand active; ours saw no leg).
                    tgets = oj.get("TakerGets")
                    if isinstance(tgets, dict) and oj.get("Account") and tgets.get("issuer") != oj.get("Account"):
                        rl = rpc("ledger_entry", {"ripple_state": {"currency": tgets["currency"], "accounts": [oj["Account"], tgets["issuer"]]},
                                                  "ledger_index": seq - 1, "binary": True})
                        li4 = (rl.get("index") or "").upper()
                        if rl.get("node_binary") and li4 and li4 not in pre:
                            pre[li4] = rl["node_binary"]
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

    # AMM objects the CROSSING may read but no meta names: a pool fill touches
    # the pseudo-account's lines and root, never the AMM object itself, and
    # the root only enters `pre` late (via the lines' peers) — after the
    # AMMID scan above ran. Fetch by PAIR instead: the direct book's pool and
    # the two XRP-bridge pools (#106701467 DAE80780: mainnet fills an FLR/USD
    # offer from the FLR/USD pool; the bundle lacked the object, so the probe
    # could only see the bridged strand and rested the offer whole).
    def amm_pair(a, b):
        def ap(x):
            return {"currency": "XRP"} if x.get("currency") == "XRP" and not x.get("issuer") else {"currency": x["currency"], "issuer": x["issuer"]}
        try:
            r = rpc("ledger_entry", {"amm": {"asset": ap(a), "asset2": ap(b)},
                                     "ledger_index": seq - 1, "binary": True})
            idx = (r.get("index") or "").upper()
            if r.get("node_binary") and idx and idx not in pre:
                pre[idx] = r["node_binary"]
                rj = rpc("ledger_entry", {"index": idx, "ledger_index": seq - 1})
                acct = (rj.get("node") or {}).get("Account")
                if acct:
                    accts.add(acct)
                    # (`accts` was already swept above — fetch the root and owner
                    # directory here.) A pool's XRP side IS its root balance.
                    for q_ in ({"account_root": acct}, {"directory": {"owner": acct}}):
                        rr = rpc("ledger_entry", dict(q_, ledger_index=seq - 1, binary=True))
                        li3 = (rr.get("index") or "").upper()
                        if rr.get("node_binary") and li3 and li3 not in pre:
                            pre[li3] = rr["node_binary"]
                    # The pool's own lines: a swap reads and writes them, and a
                    # bridge leg cannot be PRICED without the pool's balances.
                    for x in (a, b):
                        if x.get("currency") != "XRP" and x.get("issuer"):
                            rl = rpc("ledger_entry", {"ripple_state": {"currency": x["currency"], "accounts": [acct, x["issuer"]]},
                                                      "ledger_index": seq - 1, "binary": True})
                            li2 = (rl.get("index") or "").upper()
                            if rl.get("node_binary") and li2 and li2 not in pre:
                                pre[li2] = rl["node_binary"]
        except Exception:
            pass
    legs = []
    for f_ in ("TakerPays", "TakerGets", "Amount", "SendMax", "DeliverMin"):
        v = tx.get(f_)
        if v is None:
            continue
        legs.append({"currency": "XRP"} if isinstance(v, str) else {"currency": v["currency"], "issuer": v["issuer"]})
    # The pools of every explicit-path hop join the pair sweep (same specimens).
    for path in tx.get("Paths") or []:
        for step in path:
            if step.get("currency") == "XRP":
                leg = {"currency": "XRP"}
            elif step.get("currency") and step.get("issuer"):
                leg = {"currency": step["currency"], "issuer": step["issuer"]}
            else:
                continue
            if leg not in legs:
                legs.append(leg)
    if tx.get("TransactionType") in ("OfferCreate", "Payment") and len(legs) >= 2:
        xrp = {"currency": "XRP"}
        for i in range(len(legs)):
            for j in range(i + 1, len(legs)):
                if legs[i] != legs[j]:
                    amm_pair(legs[i], legs[j])
            if legs[i] != xrp:
                amm_pair(legs[i], xrp)
    # Second AMMID pass over everything gathered since the first scan.
    for li in list(pre):
        try:
            node = rpc("ledger_entry", {"index": li, "ledger_index": seq - 1}).get("node") or {}
            if node.get("AMMID") and node["AMMID"].upper() not in pre:
                nb = fetch_key(node["AMMID"].upper())
                if nb:
                    pre[node["AMMID"].upper()] = nb
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

    def node_of(h, li):
        if h not in bin_meta:
            r = rpc("tx", {"transaction": h, "binary": True})
            bin_meta[h] = r.get("meta") if isinstance(r.get("meta"), str) else r.get("meta_blob")
        return meta_node(bin_meta[h], li)

    def backward(li, before):
        """(raw | None, provenance): the image AFTER the last earlier toucher."""
        _, _, last = before[-1]
        hops = []
        for oi2, _, h2 in reversed(before):
            rec = node_of(h2, li)
            if rec is None or rec["kind"] != "ModifiedNode":
                return None, f"tx#{oi2} {h2[:8]} {rec['kind'] if rec else 'absent from its meta'}"
            if rec["finals"] is not None:
                via = f"tx#{oi2} {h2[:8]} FinalFields"
                if hops:
                    via += f" + thread-only tx#{','.join(map(str, hops))}"
                return stamped(rec["finals"], rec["node"].get((1, 1)), last, seq), via
            hops.append(oi2)
        base = pre.get(li) or fetch_key(li)
        if not base:
            return None, "thread-only touches of a key the parent ledger lacks"
        b = bytes.fromhex(base)
        return stamped(_fields_of(b, 0, len(b)), None, last, seq), f"parent image + thread-only tx#{','.join(map(str, hops))}"

    def forward(li, after, last):
        """(raw | None, provenance, own): the image BEFORE the first toucher at
        or after this tx — its FinalFields with PreviousFields put back —
        stamped with the last earlier toucher; no such toucher → the
        post-ledger image. own = True when that toucher is this tx."""
        for oi2, _, h2 in after:
            rec = node_of(h2, li)
            if rec is None:
                return None, f"tx#{oi2} {h2[:8]} absent from its meta", False
            if rec["kind"] == "CreatedNode":
                return None, f"tx#{oi2} {h2[:8]} re-created it (deleted in between)", False
            if rec["finals"] is None:
                continue  # thread-only: content unchanged, stamp overridden below
            raw = dict(rec["finals"])
            raw.update(rec["prevs"] or {})
            if rec["kind"] == "ModifiedNode":  # node-level thread = the pre values
                for k in ((2, 5), (5, 5)):
                    if k in rec["node"]:
                        raw[k] = rec["node"][k]
            own = h2 == my_hash
            who = "this tx's" if own else f"tx#{oi2} {h2[:8]}"
            return stamped(raw, rec["node"].get((1, 1)), last, seq), f"{who} FinalFields+PreviousFields", own
        post = rpc("ledger_entry", {"index": li, "ledger_index": seq, "binary": True}).get("node_binary")
        if not post:
            return None, "no post-ledger image", False
        b = bytes.fromhex(post)
        return stamped(_fields_of(b, 0, len(b)), None, last, seq), "post-ledger image", False

    def is_dir_image(hexs):
        return hexs[:6].upper() == "110064"  # LedgerEntryType == DirectoryNode

    def ops_between(lo, hi):
        return [op for oi in sorted(ops_by_index) if lo <= oi < hi for op in ops_by_index[oi]]

    def post_entries(li):
        post = rpc("ledger_entry", {"index": li, "ledger_index": seq, "binary": True}).get("node_binary")
        if not post:
            return None
        pf = _fields_of(bytes.fromhex(post), 0, len(bytes.fromhex(post)))
        return indexes_of(pf[(19, 1)]) if (19, 1) in pf else []

    def page_pre(li, before):
        """(hex | None, provenance) for a directory page: the last earlier
        toucher's FinalFields carries every field but sfIndexes; the entries
        are the parent page's replayed through the earlier txs' directory
        ops, and that replay continued through the later txs must reproduce
        the post-ledger page."""
        base = pre.get(li) or fetch_key(li)
        if not base:
            return None, "page absent from the parent ledger (created in-ledger)"
        pf = _fields_of(bytes.fromhex(base), 0, len(bytes.fromhex(base)))
        if (19, 1) not in pf:
            return None, "parent page carries no sfIndexes"
        back, via = backward(li, before)
        if back is None:
            return None, via
        honest = replay_dir(indexes_of(pf[(19, 1)]), ops_between(0, my_index), li)
        if honest is None:
            return None, "earlier directory ops inconsistent with the parent page"
        end = replay_dir(honest, ops_between(my_index, 10 ** 9), li)
        want = post_entries(li)
        if want is None:
            want = []  # page deleted later: the replay must have emptied it
        if end != want:
            return None, f"entry replay does not reach the post-ledger page ({len(end) if end is not None else '?'} vs {len(want)} entries)"
        raw = dict(back)
        raw[(19, 1)] = indexes_raw(pf[(19, 1)], honest)
        return canonical(raw), f"{via} + {len(honest)} entries replayed (post-ledger page reproduced)"

    own_ok, own_bad, page_ok, page_bad = 0, [], 0, []
    for li in sorted(set(pre) | meta_keys):
        chain = touchers.get(li, [])
        before = [t for t in chain if t[0] < my_index]
        after = [t for t in chain if t[0] >= my_index]
        if not before:
            # Not stale. This tx's own meta must reproduce the parent image, and
            # a page's entry replay the post-ledger page — the free self-checks
            # that the derivations (and the walker) are right.
            if li in meta_keys and li in pre:
                try:
                    if is_dir_image(pre[li]):
                        pf = _fields_of(bytes.fromhex(pre[li]), 0, len(bytes.fromhex(pre[li])))
                        end = replay_dir(indexes_of(pf[(19, 1)]) if (19, 1) in pf else [], ops_between(my_index, 10 ** 9), li)
                        want = post_entries(li)
                        if end == (want if want is not None else []):
                            page_ok += 1
                        else:
                            page_bad.append(li)
                    else:
                        raw, via, own = forward(li, after, None)
                        if own and raw is not None:
                            if canonical(raw) == pre[li]:
                                own_ok += 1
                            else:
                                own_bad.append(li)
                except Exception as e:  # noqa: BLE001
                    print(f"WARN: self-check {li[:16]}… {e}")
            continue
        oi, kind, h = before[-1]
        where = "parent image is stale" if li in pre else "absent from the bundle"
        if kind == "DeletedNode":
            if pre.pop(li, None) is not None:
                print(f"note: pre {li[:16]}… dropped (deleted by earlier tx#{oi} {h[:8]})")
            continue
        if my_prev.get(li) and my_prev[li] != h:
            print(f"WARN: pre {li[:16]}… thread names {my_prev[li][:8]} but last earlier toucher is tx#{oi} {h[:8]} — not rebuilt; {where}")
            continue
        if li in pre and is_dir_image(pre[li]):
            try:
                img, via = page_pre(li, before)
            except Exception as e:  # noqa: BLE001
                img, via = None, f"parse error: {e}"
            if img:
                pre[li] = img
                print(f"note: page {li[:16]}… rebuilt from {via} ({len(img) // 2} bytes)")
            else:
                print(f"WARN: page {li[:16]}… touched by earlier tx#{oi} {h[:8]} — not rebuilt ({via}); parent image is stale")
            continue
        try:
            back, via_b = (None, f"tx#{oi} {h[:8]} CreatedNode") if kind == "CreatedNode" else backward(li, before)
            fwd, via_f, _ = forward(li, after, h)
            if kind == "CreatedNode" and fwd is not None:
                why = created_consistent(fwd, node_of(h, li)["news"] or {})
                if why:
                    print(f"WARN: pre {li[:16]}… forward image ({via_f}) is not the object tx#{oi} {h[:8]} created: {why} — not rebuilt; {where}")
                    fwd = None
        except Exception as e:  # noqa: BLE001 — a parse failure must not poison the bundle
            back, fwd, via_b, via_f = None, None, f"parse error: {e}", ""
        if back is not None and fwd is not None and canonical(back) != canonical(fwd):
            print(f"WARN: pre {li[:16]}… backward ({via_b}) and forward ({via_f}) images disagree — backward used")
        raw = back if back is not None else fwd
        if raw is not None:
            pre[li] = canonical(raw)
            via = via_b if back is not None else via_f
            agree = " (forward agrees)" if back is not None and fwd is not None else ""
            print(f"note: pre {li[:16]}… rebuilt from {via}{agree} ({len(pre[li]) // 2} bytes)")
        else:
            print(f"WARN: pre {li[:16]}… touched by earlier tx#{oi} {h[:8]} — not rebuilt (backward: {via_b}; forward: {via_f}); {where}")
    # IN-LEDGER CREATIONS the walk reads but no meta of ours names: an offer
    # placed earlier in this ledger sits in a quality page that may not exist
    # in the parent ledger at all, and neither does the offer. The parent
    # fetches above can't see them; the touchers can. Rebuild an absent key
    # from its own creation (NewFields, stamped) or its last earlier
    # modification (FinalFields), and a page created in-ledger from the
    # creating tx's entries replayed through the earlier directory ops —
    # checked, as page_pre is, by replaying on to the post-ledger page.
    # #106701372 8BFFFACE: rpiFwLYi's RLUSD/XRP offer 7C87A968 was created at
    # tx#75 and consumed at tx#100; without its page the walk crossed nothing.
    def ensure_key(li, depth=0):
        if li in pre or depth > 3:
            return li in pre
        chain = touchers.get(li, [])
        before = [t for t in chain if t[0] < my_index]
        if not before:
            nb = fetch_key(li)
            if nb:
                pre[li] = nb
            return li in pre
        oi, kind, h = before[-1]
        if kind == "DeletedNode":
            return False
        try:
            rec = node_of(h, li)
            if rec is None:
                return False
            if kind == "CreatedNode":
                raw = dict(rec["news"] or {})
                if not raw:
                    return False
                raw = readd_defaults(stamped(raw, rec["node"].get((1, 1)), h, seq))
            else:
                raw, _via = backward(li, before)
                if raw is None:
                    return False
            let_raw = bytes(rec["node"].get((1, 1)) or b"")
            if (19, 1) in raw or let_raw[-2:] == b"\x00\x64":
                return False  # pages go through ensure_page
            pre[li] = canonical(raw)
            print(f"note: pre {li[:16]}… rebuilt from in-ledger tx#{oi} {h[:8]} {kind} ({len(pre[li]) // 2} bytes)")
            return True
        except Exception as e:  # noqa: BLE001
            print(f"WARN: in-ledger rebuild {li[:16]}… {e}")
            return False

    def ensure_page(bd):
        """A quality page created earlier in this ledger: entries = the
        creating tx's NewFields replayed through the earlier ops; other fields
        from the last earlier toucher. Verified against the post-ledger page."""
        if bd in pre:
            return
        chain = touchers.get(bd, [])
        before = [t for t in chain if t[0] < my_index]
        if not before or before[0][1] != "CreatedNode":
            return
        try:
            ci, _, ch = before[0]
            crec = node_of(ch, bd)
            if crec is None or not crec["news"]:
                return
            if crec["node"].get((1, 1)) != b"\x11" + LET_DIR_NODE.to_bytes(2, "big"):
                return  # not a page (an in-ledger created Offer, say): ensure_key rebuilds it
            # sfIndexes is sMD_Never: a created page's entries are the creating
            # tx's own directory ops replayed on an EMPTY page, then the later
            # earlier txs' ops.
            honest = replay_dir([], ops_between(ci, my_index), bd)
            if honest is None:
                print(f"WARN: page {bd[:16]}… created in-ledger at tx#{ci}: earlier ops inconsistent — not rebuilt")
                return
            end = replay_dir(list(honest), ops_between(my_index, 10 ** 9), bd)
            want = post_entries(bd)
            if end != (want if want is not None else []):
                print(f"WARN: page {bd[:16]}… created in-ledger at tx#{ci}: replay misses the post-ledger page — not rebuilt")
                return
            raw = dict(crec["news"])
            if len(before) > 1:
                back, _ = backward(bd, before)
                if back is not None:
                    raw = dict(back)
            raw = {k: v for k, v in raw.items() if k != (19, 1)}
            raw.setdefault((2, 2), bytes.fromhex("2200000000"))  # sfFlags is soeREQUIRED on every ledger entry; NewFields omit the zero
            raw = stamped(raw, crec["node"].get((1, 1)), before[-1][2], seq) if len(before) == 1 else raw
            raw[(19, 1)] = indexes_raw(bytes.fromhex("0113") + b"\x20" + bytes(32), honest)
            pre[bd] = canonical(raw)
            print(f"note: page {bd[:16]}… rebuilt from in-ledger creation tx#{ci} {ch[:8]} + {len(honest)} entries replayed (post-ledger page reproduced)")
            for e in honest:
                ensure_key(e.upper(), 1)
        except Exception as e:  # noqa: BLE001
            print(f"WARN: in-ledger page {bd[:16]}… {e}")

    # A page CREATED earlier in this ledger that the meta walk seated from
    # FinalFields/NewFields carries no sfIndexes (sMD_Never) — an EMPTY page,
    # which is exactly what q63 509460161CB0@106711585 crossed nothing on
    # (the maker's mirror offer sat on a page created by tx#4). Replay it
    # from empty through the earlier txs' directory ops instead.
    for li in list(pre):
        try:
            b = bytes.fromhex(pre[li])
            if b[:3] != bytes.fromhex("110064") or (19, 1) in _fields_of(b, 0, len(b)):
                continue
            chain = touchers.get(li, [])
            before = [t for t in chain if t[0] < my_index]
            if before and before[0][1] == "CreatedNode":
                del pre[li]
                ensure_page(li)
                if li not in pre:
                    print(f"WARN: in-ledger created page {li[:16]}… could not be replayed — left absent")
        except Exception as e:  # noqa: BLE001
            print(f"WARN: in-ledger page pass {li[:16]}… {e}")

    # Offers rebuilt from in-ledger images carry their BookDirectory in the
    # raw (field 5/16); the seq-1 JSON scan above never saw them.
    for li in list(pre):
        try:
            b = bytes.fromhex(pre[li])
            if b[:3] != bytes.fromhex("11006F"):
                continue
            pf = _fields_of(b, 0, len(b))
            bd = pf.get((5, 16))
            if bd is None:
                continue
            bd = (bd.hex().upper() if isinstance(bd, (bytes, bytearray)) else str(bd).upper())[-64:]  # strip the field header
            if bd not in pre:
                # A page CREATED earlier in this ledger must go through the
                # entry replay first: ensure_key would seat the creating tx's
                # NewFields image, which never carries sfIndexes (sMD_Never),
                # and an empty page is exactly what q63 509460161CB0@106711585
                # crossed nothing on. ensure_page is a no-op for any other page.
                ensure_page(bd)
            if bd not in pre:
                if not ensure_key(bd) or is_dir_image(pre.get(bd, "")) is False:
                    ensure_page(bd)
        except Exception:
            pass

    if own_ok or own_bad or page_ok or page_bad:
        print(f"self-check: own meta reproduces the parent image for {own_ok}/{own_ok + len(own_bad)} non-stale objects this tx writes; "
              f"entry replay reproduces the post-ledger page for {page_ok}/{page_ok + len(page_bad)} pages")
        for li in own_bad[:5]:
            print(f"WARN: self-check {li[:16]}… own-meta image != parent image")
        for li in page_bad[:5]:
            print(f"WARN: self-check page {li[:16]}… entry replay != post-ledger page")

    # EXTRA_KEYS=<file>: keys the caller knows the walk will need beyond what
    # the meta names (deep book levels, pools on the path, their owners) —
    # every one seated the honest way: parent image when nothing earlier in
    # the ledger touched it, otherwise rebuilt through the in-ledger touchers.
    # #106723025 62498920 (finding 107): a tec specimen whose meta names the
    # fee alone, while the walk it must reproduce runs 30 book levels deep.
    # UNTOUCHED_TARGETS=<file>: keys the transaction must leave ALONE — seated
    # exactly like EXTRA_KEYS and then pinned in `expect` at their pre-image
    # (probe_bundle / run_bundle pass an unwritten expectation that equals the
    # seated bytes). Finding 143: the taker's own bid beyond a later ask's
    # limit is never named by the ask's meta, so a meta-only target list was
    # green at HEAD while the live engine deleted the bid.
    extra_file = os.environ.get("EXTRA_KEYS")
    ut_file = os.environ.get("UNTOUCHED_TARGETS")
    ut_keys = []
    if ut_file:
        with open(ut_file) as uf:
            ut_keys = [k.strip().upper() for k in uf if k.strip()]
    if extra_file or ut_keys:
        n_before = len(pre)
        extra_keys = list(ut_keys)
        if extra_file:
            with open(extra_file) as ef:
                extra_keys += [k.strip().upper() for k in ef if k.strip()]
        extra_keys = list(dict.fromkeys(extra_keys))
        for k in extra_keys:
            if k in pre:
                continue
            before = [t for t in touchers.get(k, []) if t[0] < my_index]
            if before:
                # A page that exists at the parent but was touched earlier in
                # this ledger: parent entries + the earlier ops (page_pre).
                # An in-ledger CREATED page: ensure_page. Anything else: the
                # last earlier toucher's image (ensure_key).
                hexs, _why = page_pre(k, before)
                if hexs is not None:
                    pre[k] = hexs
                    continue
                ensure_page(k)
                if k not in pre:
                    ensure_key(k)
            else:
                ensure_key(k)
        print(f"extra keys: {len(extra_keys)} requested, {len(pre) - n_before} seated")
        pinned = 0
        for k in ut_keys:
            if k in pre:
                targets[k] = pre[k]
                pinned += 1
            else:
                print(f"WARN: untouched target {k[:16]}… could not be seated")
        if ut_keys:
            print(f"untouched targets: {len(ut_keys)} requested, {pinned} pinned")

    parent = rpc("ledger", {"ledger_index": seq - 1})["ledger"]
    cur = rpc("ledger", {"ledger_index": seq})["ledger"]
    tx.pop("metaData", None)
    bundle = {
        "tx": tx,
        # The ledger's verdict — a tec specimen (fee-only) is pinned against
        # THIS, not against tesSUCCESS (probe_bundle / run_bundle read it).
        "result": meta.get("TransactionResult"),
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
