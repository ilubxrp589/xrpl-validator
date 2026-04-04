import json
import time
import threading
import websocket
from pathlib import Path

# ================== CONFIG ==================
WS_URL = "ws://127.0.0.1:6006"
NUM_WORKERS = 4
BATCH_SIZE = 2048
OUTPUT_DIR = Path("xrpl_clean_snapshot")
RECV_TIMEOUT = 30
# ===========================================

OUTPUT_DIR.mkdir(exist_ok=True)

def make_ws():
    ws = websocket.create_connection(WS_URL, timeout=30)
    ws.settimeout(RECV_TIMEOUT)
    return ws

# Pin ledger
print(f"Connecting to {WS_URL}...", flush=True)
ws0 = make_ws()
ws0.send(json.dumps({"command": "ledger_data", "binary": True, "limit": 1, "ledger_index": "validated"}))
resp = json.loads(ws0.recv())
if "result" in resp:
    resp = resp["result"]
LEDGER_INDEX = resp["ledger_index"]
ws0.close()
print(f"Pinned to ledger {LEDGER_INDEX}", flush=True)

# Split keyspace
ranges = []
chunk = 256 // NUM_WORKERS
for i in range(NUM_WORKERS):
    start_byte = i * chunk
    end_byte = 0xFF if i == NUM_WORKERS - 1 else (i + 1) * chunk - 1
    ranges.append((start_byte, end_byte))
    print(f"  Worker {i}: 0x{start_byte:02X} - 0x{end_byte:02X}", flush=True)

lock = threading.Lock()
global_objects = [0]
global_batches = [0]
start_time = time.time()

def worker(worker_id, start_byte, end_byte):
    start_key = bytearray(32)
    start_key[0] = start_byte
    marker = start_key.hex()

    local_objects = 0
    local_batches = 0
    retries = 0

    ws = make_ws()

    while True:
        cmd = {
            "command": "ledger_data",
            "binary": True,
            "limit": BATCH_SIZE,
            "ledger_index": LEDGER_INDEX,
            "marker": marker,
        }
        try:
            ws.send(json.dumps(cmd))
            raw = ws.recv()
        except Exception as e:
            retries += 1
            if retries > 20:
                print(f"  [W{worker_id}] Too many errors, stopping", flush=True)
                break
            print(f"  [W{worker_id}] Error ({e}), reconnecting...", flush=True)
            time.sleep(2)
            try:
                ws.close()
            except:
                pass
            try:
                ws = make_ws()
            except:
                pass
            continue

        retries = 0
        result = json.loads(raw)
        if "result" in result:
            result = result["result"]

        if "error" in result:
            err = result.get("error", "")
            print(f"  [W{worker_id}] Error: {err}", flush=True)
            time.sleep(1)
            continue

        state = result.get("state", [])

        # Filter to our range
        filtered = []
        past_end = False
        for obj in state:
            idx = obj.get("index", "")
            if len(idx) >= 2:
                fb = int(idx[0:2], 16)
                if fb > end_byte:
                    past_end = True
                    break
            filtered.append(obj)

        local_batches += 1
        batch_file = OUTPUT_DIR / f"w{worker_id}_batch_{local_batches:06d}.json"
        with open(batch_file, "w") as f:
            json.dump(filtered, f)

        local_objects += len(filtered)
        with lock:
            global_objects[0] += len(filtered)
            global_batches[0] += 1
            total = global_objects[0]

            if global_batches[0] % 20 == 0:
                elapsed = time.time() - start_time
                rate = total / max(elapsed, 1)
                remaining = (18800000 - total) / max(rate, 1)
                pct = total / 18800000 * 100
                print(f"  {total:,} ({pct:.1f}%) -- {rate:.0f}/s -- ~{remaining/60:.0f}min left", flush=True)

        if past_end:
            break

        marker = result.get("marker")
        if not marker:
            break

    try:
        ws.close()
    except:
        pass
    print(f"  [W{worker_id}] Done: {local_objects:,} objects", flush=True)

print(f"\nStarting {NUM_WORKERS} parallel workers on localhost...", flush=True)
threads = []
for i, (sb, eb) in enumerate(ranges):
    t = threading.Thread(target=worker, args=(i, sb, eb), daemon=True)
    threads.append(t)
    t.start()
    time.sleep(0.2)

for t in threads:
    t.join()

elapsed = time.time() - start_time
total = global_objects[0]
print(f"\nDONE! {total:,} objects from ledger {LEDGER_INDEX} in {elapsed:.0f}s ({total/max(elapsed,1):.0f}/s)", flush=True)
