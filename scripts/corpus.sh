#!/bin/bash
# corpus.sh — replay every fixture ledger in crates/xrpl-node/tests/data
# through differential_probe and total the MATCH/attempted counts.
#
# This is the regression gate for native-engine changes: the total must not
# drop. Run it before and after any change to the crossing/path/AMM engine.
#
#   scripts/corpus.sh [outfile]        # default: corpus.txt at the repo root
#
# The probe needs the `ffi` feature:
#   (cd crates/xrpl-node && cargo build --features ffi --bin differential_probe)
#
# NOTE: differential_probe prints its SUMMARY line to STDERR. Capturing only
# stdout yields NO-SUMMARY for every fixture.
cd "$(dirname "$0")/.." || exit 1
P=target/debug/differential_probe
D=crates/xrpl-node/tests/data
OUT=${1:-corpus.txt}
[ -x "$P" ] || { echo "missing $P — build it with --features ffi" >&2; exit 2; }
: > "$OUT"
tot_m=0; tot_a=0
for ex in "$D"/l*_expected.json; do
  s=$(basename "$ex" | sed -E "s/^l([0-9]+)_expected.json$/\1/")
  bl="$D/l${s}_blobs.txt"
  [ -f "$bl" ] || { echo "[$s] MISSING-BLOBS" >> "$OUT"; continue; }
  out=$(timeout 900 "$P" "$bl" "$ex" 2>&1)
  sum=$(echo "$out" | grep "^SUMMARY:" | tail -1)
  m=$(echo "$sum" | sed -E "s#^SUMMARY: ([0-9]+)/([0-9]+).*#\1#")
  a=$(echo "$sum" | sed -E "s#^SUMMARY: ([0-9]+)/([0-9]+).*#\2#")
  if [ -z "$m" ]; then echo "[$s] NO-SUMMARY" >> "$OUT"; continue; fi
  tot_m=$((tot_m+m)); tot_a=$((tot_a+a))
  if [ "$m" != "$a" ]; then
    echo "[$s] $m/$a  *** DIVERGENT ***" >> "$OUT"
    echo "$out" | grep -E "DIVERGE-(TER|MUT) [A-Z]" >> "$OUT"
  else
    echo "[$s] $m/$a" >> "$OUT"
  fi
done
echo "CORPUS TOTAL: $tot_m/$tot_a" >> "$OUT"
cat "$OUT"
