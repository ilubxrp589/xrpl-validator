'use client';

import { useEffect, useRef, useState, useCallback } from 'react';

const ACCENT = '#00FF9F';
const CYAN = '#00f0ff';
const PURPLE = '#a855f7';
const HISTORY = 30;
const POLL_MS = 100;
const SMOOTH_WINDOW = 8;

/** Exponential moving average — smooth a single new value against the last. */
function ema(prev: number, cur: number): number {
  const alpha = 2 / (SMOOTH_WINDOW + 1);
  return prev === 0 ? cur : prev + alpha * (cur - prev);
}

interface SysMetrics {
  cpu_pct: number;
  validator_cpu_pct: number;
  ram_used_gb: number;
  ram_total_gb: number;
  validator_ram_gb: number;
  disk_read_mbs: number;
  disk_write_mbs: number;
  net_in_mbs: number;
  net_out_mbs: number;
  cores: number;
  validator_pid: number | null;
}

function drawSparkline(
  ctx: CanvasRenderingContext2D,
  points: number[],
  lo: number,
  hi: number,
  color: string,
  x0: number,
  y0: number,
  w: number,
  h: number,
) {
  if (points.length < 2) return;
  const range = hi - lo || 1;
  const step = w / (points.length - 1);

  ctx.save();
  ctx.beginPath();
  for (let i = 0; i < points.length; i++) {
    const x = x0 + i * step;
    const y = y0 + h - ((points[i] - lo) / range) * h;
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  }
  const grad = ctx.createLinearGradient(0, y0, 0, y0 + h);
  grad.addColorStop(0, color + '33');
  grad.addColorStop(1, 'transparent');
  ctx.save();
  ctx.lineTo(x0 + (points.length - 1) * step, y0 + h);
  ctx.lineTo(x0, y0 + h);
  ctx.closePath();
  ctx.fillStyle = grad;
  ctx.fill();
  ctx.restore();

  ctx.beginPath();
  for (let i = 0; i < points.length; i++) {
    const x = x0 + i * step;
    const y = y0 + h - ((points[i] - lo) / range) * h;
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  }
  ctx.strokeStyle = color;
  ctx.lineWidth = 2;
  ctx.shadowBlur = 8;
  ctx.shadowColor = color;
  ctx.stroke();
  ctx.restore();
}

function sizeAndDraw(
  canvas: HTMLCanvasElement | null,
  points: number[],
  lo: number,
  hi: number,
  color: string,
  points2?: number[],
  color2?: string,
) {
  if (!canvas) return;
  const dpr = window.devicePixelRatio || 1;
  const parent = canvas.parentElement;
  if (!parent) return;
  const w = parent.clientWidth;
  const h = 50;
  canvas.style.width = `${w}px`;
  canvas.style.height = `${h}px`;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  drawSparkline(ctx, points, lo, hi, color, 0, 0, w, h);
  if (points2 && color2) {
    drawSparkline(ctx, points2, lo, hi, color2, 0, 0, w, h);
  }
}

export function NodeResources() {
  const cpuRef = useRef<HTMLCanvasElement>(null);
  const ramRef = useRef<HTMLCanvasElement>(null);
  const diskRef = useRef<HTMLCanvasElement>(null);
  const netRef = useRef<HTMLCanvasElement>(null);

  const cpuPts = useRef<number[]>([]);
  const ramPts = useRef<number[]>([]);
  const diskPts = useRef<number[]>([]);
  const netInPts = useRef<number[]>([]);
  const netOutPts = useRef<number[]>([]);

  const [m, setM] = useState<SysMetrics | null>(null);

  const drawAll = useCallback(() => {
    const pad = (arr: number[], floor: number) => Math.max(...arr, floor) * 1.3;
    sizeAndDraw(cpuRef.current, cpuPts.current, 0, 100, ACCENT);
    sizeAndDraw(ramRef.current, ramPts.current, 0, pad(ramPts.current, 1), ACCENT);
    sizeAndDraw(diskRef.current, diskPts.current, 0, pad(diskPts.current, 1), ACCENT);
    sizeAndDraw(netRef.current, netInPts.current, 0, pad([...netInPts.current, ...netOutPts.current], 1), CYAN, netOutPts.current, PURPLE);
  }, [m?.ram_total_gb]);

  useEffect(() => {
    async function poll() {
      try {
        const res = await fetch('/system/metrics');
        if (!res.ok) return;
        const data: SysMetrics = await res.json();
        setM(data);

        const lastCpu = cpuPts.current[cpuPts.current.length - 1] ?? 0;
        const lastDisk = diskPts.current[diskPts.current.length - 1] ?? 0;
        const lastIn = netInPts.current[netInPts.current.length - 1] ?? 0;
        const lastOut = netOutPts.current[netOutPts.current.length - 1] ?? 0;

        cpuPts.current = [...cpuPts.current.slice(-(HISTORY - 1)), ema(lastCpu, Math.min(data.validator_cpu_pct, 100))];
        ramPts.current = [...ramPts.current.slice(-(HISTORY - 1)), data.validator_ram_gb];
        diskPts.current = [...diskPts.current.slice(-(HISTORY - 1)), ema(lastDisk, data.disk_read_mbs + data.disk_write_mbs)];
        netInPts.current = [...netInPts.current.slice(-(HISTORY - 1)), ema(lastIn, data.net_in_mbs)];
        netOutPts.current = [...netOutPts.current.slice(-(HISTORY - 1)), ema(lastOut, data.net_out_mbs)];

        drawAll();
      } catch { /* silent */ }
    }

    poll();
    const id = setInterval(poll, POLL_MS);
    window.addEventListener('resize', drawAll);
    return () => { clearInterval(id); window.removeEventListener('resize', drawAll); };
  }, [drawAll]);

  return (
    <div>
      <p className="text-[10px] font-mono uppercase tracking-widest text-halcyon-muted mb-2">
        Live Node Resources · m3060
      </p>
      <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
        <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-3 card-hover overflow-hidden">
          <p className="text-[10px] font-mono uppercase tracking-wider text-halcyon-muted">CPU (validator)</p>
          <p className="font-mono text-lg font-bold tabular-nums text-halcyon-accent">
            {m ? `${Math.min(m.validator_cpu_pct, 100).toFixed(1)}%` : '—'}
          </p>
          {m && <p className="text-[9px] font-mono text-halcyon-muted">{m.cpu_pct}% system · {m.cores} cores</p>}
          <canvas ref={cpuRef} className="mt-1 block w-full" />
        </div>
        <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-3 card-hover overflow-hidden">
          <p className="text-[10px] font-mono uppercase tracking-wider text-halcyon-muted">RAM (validator)</p>
          <p className="font-mono text-lg font-bold tabular-nums text-halcyon-accent">
            {m ? `${m.validator_ram_gb} GB` : '—'}
          </p>
          {m && <p className="text-[9px] font-mono text-halcyon-muted">{m.ram_used_gb} / {m.ram_total_gb} GB system</p>}
          <canvas ref={ramRef} className="mt-1 block w-full" />
        </div>
        <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-3 card-hover overflow-hidden">
          <p className="text-[10px] font-mono uppercase tracking-wider text-halcyon-muted">Disk I/O</p>
          <p className="font-mono text-lg font-bold tabular-nums text-halcyon-accent">
            {m ? `${(m.disk_read_mbs + m.disk_write_mbs).toFixed(1)} MB/s` : '—'}
          </p>
          <canvas ref={diskRef} className="mt-1 block w-full" />
        </div>
        <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-3 card-hover overflow-hidden">
          <p className="text-[10px] font-mono uppercase tracking-wider text-halcyon-muted">Network</p>
          <div className="flex items-baseline gap-2 mt-0.5">
            <span className="font-mono text-sm font-bold tabular-nums" style={{ color: CYAN }}>
              {m ? m.net_in_mbs.toFixed(1) : '—'}
            </span>
            <span className="text-[9px] font-mono" style={{ color: CYAN }}>IN</span>
            <span className="text-halcyon-muted text-[10px]">/</span>
            <span className="font-mono text-sm font-bold tabular-nums" style={{ color: PURPLE }}>
              {m ? m.net_out_mbs.toFixed(1) : '—'}
            </span>
            <span className="text-[9px] font-mono" style={{ color: PURPLE }}>OUT</span>
            <span className="text-[9px] text-halcyon-muted">MB/s</span>
          </div>
          <canvas ref={netRef} className="mt-1 block w-full" />
        </div>
      </div>
    </div>
  );
}
