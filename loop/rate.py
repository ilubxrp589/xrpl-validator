#!/usr/bin/env python3
"""Native-engine divergence rate on UNSELECTED fresh ledgers.

The score that tracks engine correctness. Read this, not the count of
divergences left in a batch of scout-finds.

Why it matters: scout-finds are saved *because* they diverged, so they are the
hard residue by construction and barely move when a common root cause is fixed.
Measured 2026-07-30 on 26 such ledgers, five verified fixes moved 30 -> 28
divergences. The same fixes, measured against every ledger the scout probed,
took the rate from 16.90 to 6.80 per 1000 tx in a week. Same work, two
populations, opposite conclusions — and the unselected one is the honest one.

`native_probes` records EVERY probe (ts, seq, txs, matched, verdict), clean
runs included, which is what makes it unbiased. Usage:

    python3 rate.py [--days N] [--by day|week]
"""
import argparse
import collections
import datetime
import os
import sqlite3

DB = os.path.join(os.path.dirname(os.path.abspath(__file__)), "scout.sqlite")


def bucket(ts, by):
    d = datetime.date(*(int(x) for x in ts[:10].split("-")))
    if by == "week":
        d -= datetime.timedelta(days=d.weekday())
    return d.isoformat()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=0, help="only the last N days")
    ap.add_argument("--by", choices=("day", "week"), default="week")
    args = ap.parse_args()

    con = sqlite3.connect(DB)
    rows = list(con.execute(
        "SELECT ts, txs, matched FROM native_probes "
        "WHERE txs IS NOT NULL AND matched IS NOT NULL AND txs > 0 ORDER BY ts"
    ))
    # A probe that matched NOTHING did not find engine divergences — it failed to
    # hydrate (wrong RPC for the ledger's age, node mid-restart, window rolled).
    # Counting those as divergences swamps the metric: on 2026-08-03 two such
    # probes (0/191 and 0/48) turned a true 1.86/1000 into a reported 19.33.
    dropped = [r for r in rows if r[2] == 0 and r[1] > 5]
    rows = [r for r in rows if not (r[2] == 0 and r[1] > 5)]
    if args.days:
        cutoff = (datetime.date.today() - datetime.timedelta(days=args.days)).isoformat()
        rows = [r for r in rows if r[0][:10] >= cutoff]
    if not rows:
        print("no probe data")
        return

    buckets = collections.OrderedDict()
    for ts, txs, matched in rows:
        b = buckets.setdefault(bucket(ts, args.by), [0, 0, 0, 0])
        b[0] += 1
        b[1] += txs
        b[2] += txs - matched
        b[3] += 1 if txs == matched else 0

    print("NATIVE ENGINE on unselected fresh ledgers")
    print(f"{args.by:<12}{'ledgers':>8}{'txs':>9}{'div':>6}{'per1000':>9}{'clean%':>8}")
    for k, (n, t, d, c) in buckets.items():
        print(f"{k:<12}{n:>8}{t:>9}{d:>6}{d * 1000 / t:>9.2f}{c * 100 / n:>7.0f}%")

    n = sum(b[0] for b in buckets.values())
    t = sum(b[1] for b in buckets.values())
    d = sum(b[2] for b in buckets.values())
    c = sum(b[3] for b in buckets.values())
    print(f"{'ALL':<12}{n:>8}{t:>9}{d:>6}{d * 1000 / t:>9.2f}{c * 100 / n:>7.0f}%")
    if dropped:
        print(f"\n({len(dropped)} probe(s) excluded: matched 0 of everything = "
              f"hydration failure, not engine divergence)")


if __name__ == "__main__":
    main()
