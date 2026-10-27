#!/bin/bash
# The twin's hourly check, on the twin's host: one line an hour to DATA_DIR/twin_check.log (twin_check.py against
# the live validator on :3777). Ends when the twin stops; start it again after a twin restart.
#   setsid scripts/twin/twin-watch.sh DATA_DIR > /dev/null 2>&1 < /dev/null &
DATA=${1:?DATA_DIR}; HERE=$(dirname "$(readlink -f "$0")")
echo $$ > "$DATA/twin-watch.pid"
while kill -0 "$(cat "$DATA/twin.pid" 2>/dev/null)" 2>/dev/null; do
  echo "$(date '+%F %T') $(python3 "$HERE/twin_check.py" --state "$DATA/twin_check.json" 2>&1)" >> "$DATA/twin_check.log"
  sleep 3600
done
echo "$(date '+%F %T') the twin is not running: the watch ends" >> "$DATA/twin_check.log"
