#!/bin/bash
# Independence Step 8's switch: a cold start with no .39. Fetch a validated ledger's whole state from mainnet peers
# (peer_state_sync, every node proven), turn it into the store (leaves_to_rocks), then start live_viewer warm with
# XRPL_LEDGER_SOURCE=peers: it verifies the root against the peers' account hash and catches up from peers.
#   scripts/peer_cold_start.sh DATA_DIR [BIN_DIR]
# DATA_DIR is the validator's XRPL_DATA_DIR; its sync/state.rocks must not exist yet. BIN_DIR holds the two tools
# (default: target/release). peer_state_sync writes about 5 GB to DATA_DIR/peer-state; it is kept for the record.
set -euo pipefail
DATA=${1:?DATA_DIR}; BIN=${2:-target/release}
# The validator list is checked against the same publisher pin the live validator uses.
LIVE_LAUNCHER=${LIVE_LAUNCHER:-$HOME/run-prod-320.sh}
if [ -z "${XRPL_UNL_PINNED_KEY:-}" ]; then
  XRPL_UNL_PINNED_KEY=$(sed -n 's/^export XRPL_UNL_PINNED_KEY="\{0,1\}\([0-9A-Fa-f,]*\).*/\1/p' "$LIVE_LAUNCHER" 2>/dev/null | head -1)
  export XRPL_UNL_PINNED_KEY
fi
[ -e "$DATA/sync/state.rocks" ] && { echo "refusing: $DATA/sync/state.rocks exists (move it aside first)"; exit 1; }
mkdir -p "$DATA/sync" "$DATA/peer-state"
nice -n 19 ionice -c3 "$BIN/peer_state_sync" --out "$DATA/peer-state" --fresh --witness none | tee "$DATA/peer-state/run.log"
grep -q "^ROOT PROVEN" "$DATA/peer-state/run.log" || { echo "the state was not proven: not converting"; exit 1; }
XRPL_DATA_DIR="$DATA" nice -n 19 ionice -c3 "$BIN/leaves_to_rocks" --dir "$DATA/peer-state"
echo "now start live_viewer with XRPL_DATA_DIR=$DATA XRPL_WARM_RESUME=1 XRPL_LEDGER_SOURCE=peers (plus the usual env)"
