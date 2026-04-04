import json
import time
from pathlib import Path

try:
    import rocksdb
except ImportError:
    print("Installing python-rocksdb...")
    import subprocess
    subprocess.check_call(["pip3", "install", "--break-system-packages", "python-rocksdb"])
    import rocksdb

DB_PATH = "/mnt/xrpl-data/sync/state.rocks"
SNAPSHOT_DIR = Path("xrpl_clean_snapshot")

# Open RocksDB
opts = rocksdb.Options()
opts.create_if_missing = True
opts.max_open_files = 5000
opts.write_buffer_size = 64 * 1024 * 1024  # 64MB write buffer
db = rocksdb.DB(DB_PATH, opts)

batch_files = sorted(SNAPSHOT_DIR.glob("w*_batch_*.json"))
if not batch_files:
    batch_files = sorted(SNAPSHOT_DIR.glob("batch_*.json"))

print(f"Importing {len(batch_files)} batch files into {DB_PATH}...", flush=True)

total = 0
start = time.time()

for i, bf in enumerate(batch_files):
    with open(bf) as f:
        state = json.load(f)

    batch = rocksdb.WriteBatch()
    for obj in state:
        index_hex = obj.get("index", "")
        data_hex = obj.get("data", "")
        if len(index_hex) == 64 and data_hex:
            key = bytes.fromhex(index_hex)
            val = bytes.fromhex(data_hex)
            batch.put(key, val)
            total += 1

    db.write(batch)

    if (i + 1) % 100 == 0 or i < 5:
        elapsed = time.time() - start
        rate = total / max(elapsed, 1)
        print(f"  {i+1}/{len(batch_files)} files -- {total:,} objects -- {rate:.0f}/s -- {elapsed:.0f}s", flush=True)

elapsed = time.time() - start
print(f"\nDONE! Imported {total:,} objects in {elapsed:.0f}s ({total/max(elapsed,1):.0f}/s)", flush=True)

# Write sync marker so validator knows bulk sync is complete
marker_path = "/mnt/xrpl-data/sync/sync_complete.marker"
with open(marker_path, "w") as f:
    f.write(str(total))
print(f"Wrote sync marker: {marker_path}", flush=True)
