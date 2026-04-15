#!/usr/bin/env python3
"""Tiny HTTP sidecar that serves live system metrics from /proc.
Runs on port 3779 alongside the validator on 3777.
No dependencies beyond Python 3 stdlib.

Usage:
  nohup python3 system-metrics.py > /tmp/sysmetrics.log 2>&1 &

Endpoints:
  GET /metrics  → JSON with cpu_pct, ram_used_gb, ram_total_gb,
                   disk_read_mbs, disk_write_mbs, net_in_mbs, net_out_mbs
"""

import http.server
import json
import time
import os

PORT = 3779
INTERVAL = 1.0  # internal sample rate for deltas
VALIDATOR_PID = None  # auto-detected

# ---------------------------------------------------------------------------
# /proc readers
# ---------------------------------------------------------------------------

def read_cpu():
    """Returns (total_jiffies, idle_jiffies) from /proc/stat."""
    with open('/proc/stat') as f:
        for line in f:
            if line.startswith('cpu '):
                parts = line.split()[1:]
                vals = [int(x) for x in parts]
                total = sum(vals)
                idle = vals[3] + (vals[4] if len(vals) > 4 else 0)  # idle + iowait
                return total, idle
    return 0, 0


def read_mem():
    """Returns (used_gb, total_gb)."""
    info = {}
    with open('/proc/meminfo') as f:
        for line in f:
            parts = line.split()
            if len(parts) >= 2:
                info[parts[0].rstrip(':')] = int(parts[1])  # kB
    total = info.get('MemTotal', 0)
    avail = info.get('MemAvailable', info.get('MemFree', 0))
    used = total - avail
    return used / 1048576, total / 1048576  # kB → GB


def find_validator_pid():
    """Find the live_viewer binary PID (not the bash wrapper)."""
    import subprocess
    try:
        # pgrep -x matches the exact process name, not the bash -c wrapper
        out = subprocess.check_output(['pgrep', '-x', 'live_viewer'], text=True).strip()
        pids = [int(p) for p in out.split('\n') if p]
        return pids[0] if pids else None
    except Exception:
        pass
    # Fallback: search broadly and pick the highest PID (child, not wrapper)
    try:
        out = subprocess.check_output(['pgrep', '-f', 'live_viewer'], text=True).strip()
        pids = sorted([int(p) for p in out.split('\n') if p])
        return pids[-1] if pids else None
    except Exception:
        return None


def read_process_cpu(pid):
    """Read per-process CPU from /proc/PID/stat. Returns (utime+stime) in jiffies."""
    try:
        with open(f'/proc/{pid}/stat') as f:
            parts = f.read().split()
            utime = int(parts[13])
            stime = int(parts[14])
            return utime + stime
    except Exception:
        return 0


def read_process_mem(pid):
    """Read RSS from /proc/PID/status in GB."""
    try:
        with open(f'/proc/{pid}/status') as f:
            for line in f:
                if line.startswith('VmRSS:'):
                    return int(line.split()[1]) / 1048576  # kB → GB
    except Exception:
        pass
    return 0.0


def read_disk():
    """Returns (read_bytes, write_bytes) summed across all sd*/nvme* devices."""
    rb = wb = 0
    with open('/proc/diskstats') as f:
        for line in f:
            parts = line.split()
            if len(parts) < 14:
                continue
            name = parts[2]
            # Only count whole devices, not partitions
            if any(name.startswith(p) for p in ('sd', 'nvme', 'vd')):
                if name[-1].isdigit() and not name.startswith('nvme'):
                    continue  # skip partitions like sda1
                # For nvme, skip partition entries like nvme0n1p1
                if 'p' in name and name.startswith('nvme'):
                    continue
                rb += int(parts[5]) * 512   # sectors read → bytes
                wb += int(parts[9]) * 512   # sectors written → bytes
    return rb, wb


def read_net():
    """Returns (rx_bytes, tx_bytes) summed across non-lo interfaces."""
    rx = tx = 0
    with open('/proc/net/dev') as f:
        for line in f:
            if ':' not in line:
                continue
            iface, data = line.split(':', 1)
            iface = iface.strip()
            if iface == 'lo':
                continue
            parts = data.split()
            rx += int(parts[0])
            tx += int(parts[8])
    return rx, tx


# ---------------------------------------------------------------------------
# Delta tracker
# ---------------------------------------------------------------------------

class Sampler:
    def __init__(self):
        self.prev_cpu = read_cpu()
        self.prev_disk = read_disk()
        self.prev_net = read_net()
        self.prev_time = time.monotonic()
        # Validator process tracking
        self.validator_pid = find_validator_pid()
        self.prev_proc_cpu = read_process_cpu(self.validator_pid) if self.validator_pid else 0
        self.num_cores = os.cpu_count() or 1
        # Cached results
        self.cpu_pct = 0.0
        self.proc_cpu_pct = 0.0
        self.proc_ram_gb = 0.0
        self.disk_read_mbs = 0.0
        self.disk_write_mbs = 0.0
        self.net_in_mbs = 0.0
        self.net_out_mbs = 0.0

    def sample(self):
        now = time.monotonic()
        dt = now - self.prev_time
        if dt < 0.1:
            return
        self.prev_time = now

        # System-wide CPU
        cur_cpu = read_cpu()
        d_total = cur_cpu[0] - self.prev_cpu[0]
        d_idle = cur_cpu[1] - self.prev_cpu[1]
        self.cpu_pct = ((d_total - d_idle) / max(d_total, 1)) * 100
        self.prev_cpu = cur_cpu

        # Validator process CPU (% of ALL cores, like top shows)
        if self.validator_pid:
            cur_proc = read_process_cpu(self.validator_pid)
            d_proc = cur_proc - self.prev_proc_cpu
            self.prev_proc_cpu = cur_proc
            # jiffies are in clock ticks (usually 100/sec)
            hz = 100.0
            self.proc_cpu_pct = (d_proc / hz / dt) * 100
            self.proc_ram_gb = read_process_mem(self.validator_pid)
        else:
            # Try to find it again (may have restarted)
            self.validator_pid = find_validator_pid()

        # Disk
        cur_disk = read_disk()
        self.disk_read_mbs = (cur_disk[0] - self.prev_disk[0]) / dt / 1048576
        self.disk_write_mbs = (cur_disk[1] - self.prev_disk[1]) / dt / 1048576
        self.prev_disk = cur_disk

        # Network
        cur_net = read_net()
        self.net_in_mbs = (cur_net[0] - self.prev_net[0]) / dt / 1048576
        self.net_out_mbs = (cur_net[1] - self.prev_net[1]) / dt / 1048576
        self.prev_net = cur_net

    def to_json(self):
        ram_used, ram_total = read_mem()
        return {
            'cpu_pct': round(self.cpu_pct, 1),
            'validator_cpu_pct': round(self.proc_cpu_pct, 1),
            'ram_used_gb': round(ram_used, 1),
            'ram_total_gb': round(ram_total, 1),
            'validator_ram_gb': round(self.proc_ram_gb, 1),
            'disk_read_mbs': round(self.disk_read_mbs, 1),
            'disk_write_mbs': round(self.disk_write_mbs, 1),
            'net_in_mbs': round(self.net_in_mbs, 1),
            'net_out_mbs': round(self.net_out_mbs, 1),
            'cores': self.num_cores,
            'validator_pid': self.validator_pid,
        }


sampler = Sampler()


# ---------------------------------------------------------------------------
# HTTP handler
# ---------------------------------------------------------------------------

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/metrics' or self.path == '/':
            sampler.sample()
            body = json.dumps(sampler.to_json()).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_error(404)

    def log_message(self, format, *args):
        pass  # silent


if __name__ == '__main__':
    # Do an initial sample so the first request has deltas
    time.sleep(1)
    sampler.sample()
    print(f'[system-metrics] Serving on :{PORT}')
    srv = http.server.HTTPServer(('0.0.0.0', PORT), Handler)
    srv.serve_forever()
