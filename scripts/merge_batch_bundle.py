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

Two further keys are written for the harness:

  inner_hashes   = each inner bundle's own transaction hash, in the order
                   the inner bundles were given on the command line, which
                   must be the outer's RawTransactions (= TransactionIndex)
                   order. `run_batch_bundle` pairs these with the engine's
                   per-inner touched-key sets to thread each object with
                   the id of the inner that last touched it.
  inner_results  = each inner entry's own recorded TransactionResult, in
                   the same order, which the harness asserts against
                   `tx::batch::take_inner_results()`.

Both lists cover only the inners the LEDGER FILED — a mode that stopped
early, or an inner that failed without claiming, leaves no entry to fetch
a bundle for — so they can be SHORTER than the outer's RawTransactions,
never longer.

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
    # A ledger files at most one entry per RawTransactions element, so more
    # inner bundles than the outer has inners means the wrong bundles were
    # named on the command line — and every later pairing would be off by one.
    n_raw = len(outer["tx"].get("RawTransactions", []))
    assert len(inners) <= n_raw, (
        f"{len(inners)} inner bundles but the outer carries {n_raw} RawTransactions"
    )
    merged["pre"], merged["expect"] = pre, expect
    merged["inner_hashes"] = [b["tx"]["hash"] for b in inners]
    merged["inner_results"] = [b["result"] for b in inners]
    json.dump(merged, open(out, "w"))
    print(f"{out}: pre={len(pre)} expect={len(expect)} inners={len(inners)}")

if __name__ == "__main__":
    main()
