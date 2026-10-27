#!/bin/bash
# The twin: a peer-fed copy of the validator that signs and sends nothing, run beside the live one to prove Step 4's
# peer switch without touching it (docs/superpowers/plans/2026-10-27-step4-peer-switch.md, Task 13).
#   scripts/twin/run-twin.sh DATA_DIR BINARY
# DATA_DIR: the twin's own data directory, with a state from scripts/peer_cold_start.sh. BINARY: its live_viewer.
# Nothing is shared with the live validator: its own XRPL_DATA_DIR (every file it writes follows it), API port 3787,
# no seed (XRPL_OBSERVE=1), and no .39: the RPC/WS URLs point at a closed local port, and peer mode refuses RPC
# anyway and counts any attempt (ledger_source.rpc_refused). Memory capped (TWIN_MEMORY_MAX, default 52G; "none" on
# a host of its own), lowest CPU and disk priority. Stop it with: kill $(cat DATA_DIR/twin.pid)
set -euo pipefail
DATA=$(realpath -m "${1:?DATA_DIR}"); BIN=$(realpath "${2:?BINARY}")
LIVE_LAUNCHER=${LIVE_LAUNCHER:-$HOME/run-prod-320.sh}
case "$DATA" in
  /mnt/xrpl-data|/mnt/xrpl-data/*|"$HOME/xrpl-data"|"$HOME/xrpl-data"/*) echo "refusing: $DATA is the live validator's data"; exit 1;;
esac
[ -e "$DATA/validator_seed.hex" ] && { echo "refusing: $DATA holds a validator seed"; exit 1; }
[ -d "$DATA/sync/state.rocks" ] || { echo "no state in $DATA: run scripts/peer_cold_start.sh $DATA first"; exit 1; }
if [ -f "$DATA/twin.pid" ] && kill -0 "$(cat "$DATA/twin.pid")" 2>/dev/null; then echo "the twin already runs (pid $(cat "$DATA/twin.pid"))"; exit 1; fi
PIN=${TWIN_UNL_PINNED_KEY:-$(sed -n 's/^export XRPL_UNL_PINNED_KEY="\{0,1\}\([0-9A-Fa-f,]*\).*/\1/p' "$LIVE_LAUNCHER" 2>/dev/null | head -1)}
[ -n "$PIN" ] || { echo "no validator-list publisher pin: set TWIN_UNL_PINNED_KEY"; exit 1; }

export XRPL_DATA_DIR="$DATA" FFI_PORT=3787 XRPL_OBSERVE=1 XRPL_LEDGER_SOURCE=peers XRPL_WARM_RESUME=1 XRPL_MAX_PEERS=40
export XRPL_RPC_URL=http://127.0.0.1:9 XRPL_RPC_URLS=http://127.0.0.1:9 XRPL_WS_URL=ws://127.0.0.1:9
export XRPL_FFI_DIVERGENCE_LOG="$DATA/divergences.jsonl" XRPL_FFI_MUTATION_DIVERGENCE_LOG="$DATA/mutation_divergences.jsonl"
export XRPL_FFI_SHADOW_LOG="$DATA/shadow.jsonl" XRPL_FFI_SILENT_DIVERGENCE_LOG="$DATA/silent_divergences.jsonl"
# The live validator's engine switches (run-prod-320.sh), less the consensus recorder (disk) and the witness.
export XRPL_FLOW_ENGINE=port XRPL_WSSYNC_TXGATE=enforce XRPL_NATIVE_WRITER=1 XRPL_STATE_ROCKS_CONSULT_BUDGET=0
export XRPL_NATIVE_SHADOW=1 XRPL_SHADOW_HYDRATE_MIN_GB=16 XRPL_NATIVE_META=1 XRPL_NATIVE_ORDER=on
export MALLOC_CONF=background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0
export _RJEM_MALLOC_CONF=background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0
export XRPL_SIG_ENFORCE=1 XRPL_UNL_PINNED_KEY="$PIN" XRPL_UNL_ENFORCE=1 XRPL_UNL_REFRESH=1
export XRPL_PEER_FETCH=1 XRPL_OWN_VALIDATED=1 XRPL_OWN_HASH=1 XRPL_SIGN_OWN=1
# The 30-day count's setting: what we cannot make goes unsigned, never signed with the quorum's hash.
export XRPL_OWN_BREAK=${TWIN_OWN_BREAK:-off}
export XRPL_PEER_PONG=1 XRPL_NODE_IDENTITY=1 XRPL_PEER_PRIVATE=1 XRPL_CLOCK_CHECK=1 XRPL_CLOCK_GATE=1 XRPL_VOTE_PENDING_ONLY=1

CAP=${TWIN_MEMORY_MAX:-52G}
cd "$DATA"
if [ "$CAP" = none ]; then
  nohup nice -n 19 ionice -c3 "$BIN" >> "$DATA/twin.log" 2>&1 < /dev/null &
else
  command -v systemd-run >/dev/null || { echo "no systemd-run for the memory cap: set TWIN_MEMORY_MAX=none only on a host of its own"; exit 1; }
  nohup systemd-run --user --scope --quiet -p MemoryMax="$CAP" -p MemorySwapMax=0 nice -n 19 ionice -c3 "$BIN" >> "$DATA/twin.log" 2>&1 < /dev/null &
fi
echo $! > "$DATA/twin.pid"
sleep 3
kill -0 "$(cat "$DATA/twin.pid")" 2>/dev/null || { echo "the twin did not stay up:"; tail -20 "$DATA/twin.log"; exit 1; }
echo "twin up: pid $(cat "$DATA/twin.pid"), API http://127.0.0.1:3787, log $DATA/twin.log, memory cap $CAP"
