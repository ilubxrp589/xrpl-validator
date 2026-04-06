# Deploy Halcyon XRPL Validator

Run the FFI-integrated validator alongside an official rippled node. Requires a rippled instance accessible via RPC (your own or a trusted one).

## Requirements

- **OS:** Ubuntu 22.04+ / Debian 12+
- **CPU:** x86_64, 4+ cores (Ryzen 5500 or better recommended)
- **RAM:** 8GB minimum, 16GB+ recommended
- **Disk:** 10GB free (state.rocks ~5GB + build artifacts)
- **Network:** Access to a rippled RPC endpoint (port 5005) and WS (port 6006)

## 1. Install dependencies

```bash
sudo apt update && sudo apt install -y \
    build-essential gcc g++ cmake pkg-config \
    libssl-dev bison flex protobuf-compiler \
    python3 python3-pip python3-venv git curl

# Rust (if not installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Conan 2.x (C++ package manager for libxrpl deps)
pip3 install --user --break-system-packages "conan>=2.0,<3"
export PATH=$HOME/.local/bin:$PATH

# Conan profile
conan profile detect --force
# Fix C++ standard to 20
sed -i 's/compiler.cppstd=gnu17/compiler.cppstd=20/' ~/.conan2/profiles/default

# Add Ripple's conan remote (patched recipes)
conan remote add --index 0 xrplf https://conan.ripplex.io --force
```

## 2. Clone the repo

```bash
git clone https://github.com/ilubxrp589/xrpl-validator.git
cd xrpl-validator

# Clone rippled source (for libxrpl headers + build)
mkdir -p ffi/vendor
cd ffi/vendor
git clone https://github.com/XRPLF/rippled.git
cd rippled
git checkout c0ee813   # pinned commit (3.2.0-b0)
cd ../../..
```

## 3. Build libxrpl

```bash
cd ffi/vendor/rippled
mkdir -p .build && cd .build

# Install C++ dependencies (~10-20 min first time, cached after)
conan install .. --output-folder . --build missing --settings build_type=Release

# Configure cmake
cmake -DCMAKE_TOOLCHAIN_FILE:FILEPATH=build/generators/conan_toolchain.cmake \
      -DCMAKE_BUILD_TYPE=Release -Dxrpld=OFF -Dtests=OFF ..

# Build libxrpl (~5-15 min depending on CPU)
cmake --build . --target xrpl.libxrpl --target xrpl.libpb -j$(nproc)

cd ../../../..
```

## 4. Build the C++ FFI shim

```bash
cd ffi
mkdir -p build && cd build
cmake -DCMAKE_BUILD_TYPE=Release ..
cmake --build . -j$(nproc)

# Verify: should print "No errors detected"
./test_shim
cd ../..
```

## 5. Build the Rust validator

```bash
cargo build --release -p xrpl-node --features ffi --bin live_viewer
```

Binary at `target/release/live_viewer` (~40MB).

## 6. Configure

Set your rippled RPC endpoint. Replace `YOUR_RIPPLED_IP` with your rippled node's IP:

```bash
export XRPL_RPC_URL=http://YOUR_RIPPLED_IP:5005
export XRPL_WS_URL=ws://YOUR_RIPPLED_IP:6006
export XRPL_RPC_URLS=http://YOUR_RIPPLED_IP:5005
```

Create the data directory:

```bash
sudo mkdir -p /mnt/xrpl-data/sync
sudo chown $USER:$USER /mnt/xrpl-data/sync
```

## 7. Launch

```bash
# First run: bulk syncs ~18.8M objects from rippled (~2 min)
nohup ./target/release/live_viewer > /tmp/validator.log 2>&1 < /dev/null & disown

# Monitor sync progress
tail -f /tmp/validator.log | grep -E '\[sync\]|\[state-hash\]|MATCH|DONE'
```

## 8. Verify

```bash
# Check the dashboard
curl -s http://localhost:3777/api/engine | python3 -c "
import sys, json
d = json.load(sys.stdin)
f = d.get('ffi_verifier', {})
att = f.get('live_apply_attempted', 0)
ok = f.get('live_apply_ok', 0) + f.get('live_apply_claimed', 0)
div = f.get('live_apply_diverged', 0)
pct = ok/att*100 if att else 0
print(f'FFI: {ok}/{att} ({pct:.2f}%) diverged={div}')
"

# Should show: FFI: X/X (100.00%) diverged=0
```

Web dashboard at `http://YOUR_IP:3777`

## Restart procedure

**Any restart requires a state wipe + resync:**

```bash
killall -9 live_viewer
rm -rf /mnt/xrpl-data/sync/state.rocks \
       /mnt/xrpl-data/sync/state.rocks.engine \
       /mnt/xrpl-data/sync/sync_complete.marker \
       /mnt/xrpl-data/sync/dl_done.txt \
       /mnt/xrpl-data/sync/leaf_cache.bin

# Relaunch (bulk sync takes ~2 min)
nohup ./target/release/live_viewer > /tmp/validator.log 2>&1 < /dev/null & disown
```

## Endpoints

| Endpoint | Description |
|----------|-------------|
| `http://localhost:3777` | Web dashboard |
| `http://localhost:3777/api/engine` | JSON: FFI stats, ledger, state |
| `http://localhost:3777/api/consensus` | JSON: consensus tracking |
| `http://localhost:3777/api/state-hash` | JSON: state hash, sync status |
| `http://localhost:3778/metrics` | Prometheus OpenMetrics (if ffi_test sidecar running) |

## Architecture

```
┌─────────────────────────────────────────┐
│         Halcyon XRPL Validator          │
│                                         │
│  ┌──────────┐  ┌──────────────────────┐ │
│  │   Rust   │  │   libxrpl (C++ FFI)  │ │
│  │          │  │                      │ │
│  │ Peers    │  │ preflight()          │ │
│  │ Consensus│──│ apply()              │ │
│  │ State DB │  │ mutations            │ │
│  │ SHAMap   │  │ 100% mainnet match   │ │
│  │ RPC/WS   │  │                      │ │
│  └──────────┘  └──────────────────────┘ │
│                                         │
│  State: RocksDB (18.8M objects)         │
│  Hash:  SHAMap Merkle (sub-20ms)        │
└─────────────────────────────────────────┘
         │                    │
         ▼                    ▼
    rippled RPC          XRPL Network
    (state sync)         (peer protocol)
```

## Troubleshooting

**State hash mismatches after restart:** Always wipe + resync (see restart procedure above).

**rippled "Server is overloaded":** The validator has built-in retry with exponential backoff. If persistent, check rippled's memory/load and consider adding your machine's IP to rippled's admin list in `rippled.cfg`.

**Build fails on conan install:** Ensure `conan profile show` shows `compiler.cppstd=20` and gcc >= 11.

**Build fails on "libxrpl_shim.a not found":** Run steps 3 and 4 first — the Rust build depends on the C++ shim.
