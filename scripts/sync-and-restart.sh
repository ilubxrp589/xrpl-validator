#!/bin/bash
# Monitor the sync process and restart the validator with live engine when done.
# Usage: nohup bash scripts/sync-and-restart.sh &

SYNC_PID=$(pgrep -f "sync_ledger" | head -1)
if [ -z "$SYNC_PID" ]; then
  echo "No sync_ledger process found. Exiting."
  exit 1
fi

echo "Watching sync_ledger PID $SYNC_PID..."
echo "Will restart xrpl-validator with live engine when sync completes."

# Wait for sync to finish
while kill -0 "$SYNC_PID" 2>/dev/null; do
  OBJECTS=$(wc -l < /mnt/xrpl-data/sync/objects.jsonl 2>/dev/null || echo 0)
  SLED_SIZE=$(du -sh /mnt/xrpl-data/sync/state.sled/db 2>/dev/null | cut -f1)
  echo "[$(date '+%H:%M:%S')] Sync running — JSONL: ${OBJECTS} lines, Sled: ${SLED_SIZE}"
  sleep 60
done

echo ""
echo "[$(date '+%H:%M:%S')] Sync complete!"
echo "Restarting validator with live engine enabled..."

# Delete the old PM2 process (which has XRPL_NO_ENGINE set)
pm2 delete xrpl-validator 2>/dev/null

# Start fresh WITHOUT the env var — live engine will open sled
pm2 start /home/localai/Desktop/xrpl-validator/target/release/live_viewer \
  --name xrpl-validator \
  --cwd /home/localai/Desktop/xrpl-validator

pm2 save

echo "[$(date '+%H:%M:%S')] Validator restarted with live engine."
echo "Check: curl -s http://localhost:3777/api/engine | python3 -c \"import sys,json; print(json.load(sys.stdin)['live_engine'])\""
