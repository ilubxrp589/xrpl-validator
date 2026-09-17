#!/usr/bin/env python3
"""merge_batch_bundle.py <outer.json> <inner1.json> [<inner2.json> …] <out.json>

Merges the bundle of a Batch OUTER transaction with the bundles of its
INNER entries (each produced by fetch_tx_bundle.py on that entry's own
hash) into one bundle the vector harness can replay through the native
BatchTransactor:

  tx      = the outer's tx (it carries RawTransactions)
  result  = the outer's recorded TransactionResult

fetch_tx_bundle.py gives each entry its own pre-image at that entry's own
position in the ledger's apply order — an inner's "pre" already reflects
whatever the outer (and any earlier inner) did to a key it shares with
them, because that inner truly does apply after them in the ledger's tx
set. So bundles are combined in APPLY ORDER — outer first, then inners in
RawTransactions (= TransactionIndex) order — with two different rules for
the two dicts:

  pre     = FIRST occurrence wins. The earliest bundle to mention a key
            holds that key's PRE-BATCH image: the outer's own pre for a key
            the outer itself touches, or the first inner's pre for a key
            only inners touch. A later bundle's "pre" for the same key is
            not a fresh fact — it is that key's MID-batch image (post
            earlier touchers), which is exactly what "expect" is for. This
            is why two bundles' pre-images legitimately DISAGREE on a
            shared key — that disagreement is expected, not an error, and
            is no longer treated as a conflict.
  expect  = LAST occurrence wins. A key touched by several bundles takes
            the image from the LAST toucher in apply order, which is the
            post-batch image.

The one thing that IS still fatal is an inner bundle fetched from a
different ledger than the outer (`seq` mismatch) — that means the bundles
don't describe the same Batch at all.
"""
import json, sys

def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    paths, out = sys.argv[1:-1], sys.argv[-1]
    outer = json.load(open(paths[0]))
    inners = [json.load(open(p)) for p in paths[1:]]
    merged = {k: outer[k] for k in ("seq", "parent_close_time", "total_coins", "parent_hash", "tx", "result")}
    pre, expect = dict(outer.get("pre", {})), dict(outer.get("expect", {}))
    for b in inners:
        if b["seq"] != outer["seq"]:
            sys.exit(f"inner from ledger {b['seq']} but outer from {outer['seq']}")
        for k, v in b.get("pre", {}).items():
            pre.setdefault(k, v)
        for k, v in b.get("expect", {}).items():
            expect[k] = v
    merged["pre"], merged["expect"] = pre, expect
    merged["inner_hashes"] = [b["tx"]["hash"] for b in inners]
    merged["inner_results"] = [b["result"] for b in inners]
    json.dump(merged, open(out, "w"))
    print(f"{out}: pre={len(pre)} expect={len(expect)} inners={len(inners)}")

if __name__ == "__main__":
    main()
