'use client';

import { useEffect, useRef, useState } from 'react';

// ---------------------------------------------------------------------------
// Random-walk helpers
// ---------------------------------------------------------------------------

function clamp(v: number, lo: number, hi: number) {
  return Math.max(lo, Math.min(hi, v));
}

function drift(current: number, step: number, lo: number, hi: number) {
  return clamp(current + (Math.random() - 0.5) * 2 * step, lo, hi);
}

// ---------------------------------------------------------------------------
// Sparkline canvas drawing
// ---------------------------------------------------------------------------

const ACCENT = '#00FF9F';
const CYAN = '#00f0ff';
const PURPLE = '#a855f7';

function drawSparkline(
  canvas: HTMLCanvasElement,
  points: number[],
  lo: number,
  hi: number,
  color: string,
  offsetX = 0,
  width?: number,
) {
  const ctx = canvas.getContext('2d');
  if (!ctx || points.length < 2) return;

  const dpr = window.devicePixelRatio || 1;
  const w = width ?? canvas.width / dpr;
  const h = canvas.height / dpr;

  const range = hi - lo || 1;
  const step = w / (points.length - 1);

  // Line path
  ctx.save();
  ctx.translate(offsetX, 0);
  ctx.beginPath();
  for (let i = 0; i < points.length; i++) {
    const x = i * step;
    const y = h - ((points[i] - lo) / range) * h;
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }

  // Fill gradient below
  const grad = ctx.createLinearGradient(0, 0, 0, h);
  grad.addColorStop(0, color + '33'); // ~20%
  grad.addColorStop(1, 'transparent');

  ctx.save();
  ctx.lineTo((points.length - 1) * step, h);
  ctx.lineTo(0, h);
  ctx.closePath();
  ctx.fillStyle = grad;
  ctx.fill();
  ctx.restore();

  // Stroke with glow
  ctx.beginPath();
  for (let i = 0; i < points.length; i++) {
    const x = i * step;
    const y = h - ((points[i] - lo) / range) * h;
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.strokeStyle = color;
  ctx.lineWidth = 2;
  ctx.shadowBlur = 8;
  ctx.shadowColor = color;
  ctx.stroke();
  ctx.restore();
}

function clearCanvas(canvas: HTMLCanvasElement) {
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  const dpr = window.devicePixelRatio || 1;
  ctx.clearRect(0, 0, canvas.width / dpr, canvas.height / dpr);
}

// ---------------------------------------------------------------------------
// Sparkline card sub-component
// ---------------------------------------------------------------------------

interface SparkCardProps {
  label: string;
  valueText: string;
  canvasRef: React.RefObject<HTMLCanvasElement | null>;
}

function SparkCard({ label, valueText, canvasRef }: SparkCardProps) {
  return (
    <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-3 card-hover">
      <p className="text-[10px] font-mono uppercase tracking-wider text-halcyon-muted">
        {label}
      </p>
      <p className="font-mono text-lg font-bold tabular-nums text-halcyon-accent mt-0.5">
        {valueText}
      </p>
      <canvas
        ref={canvasRef}
        width={360}
        height={120}
        className="mt-1"
        style={{ width: 180, height: 60 }}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

const HISTORY = 30;
const INTERVAL_MS = 3000;

export function NodeResources() {
  const cpuRef = useRef<HTMLCanvasElement>(null);
  const ramRef = useRef<HTMLCanvasElement>(null);
  const diskRef = useRef<HTMLCanvasElement>(null);
  const netRef = useRef<HTMLCanvasElement>(null);

  const cpuPts = useRef<number[]>([40]);
  const ramPts = useRef<number[]>([6.8]);
  const diskPts = useRef<number[]>([120]);
  const netInPts = useRef<number[]>([8]);
  const netOutPts = useRef<number[]>([12]);

  const [cpu, setCpu] = useState(40);
  const [ram, setRam] = useState(6.8);
  const [disk, setDisk] = useState(120);
  const [netIn, setNetIn] = useState(8);
  const [netOut, setNetOut] = useState(12);

  useEffect(() => {
    function tick() {
      // Drift values
      const newCpu = drift(cpuPts.current[cpuPts.current.length - 1], 3, 5, 95);
      const newRam = drift(ramPts.current[ramPts.current.length - 1], 0.2, 2, 14);
      const newDisk = drift(diskPts.current[diskPts.current.length - 1], 20, 10, 500);
      const newIn = drift(netInPts.current[netInPts.current.length - 1], 1.5, 0.5, 40);
      const newOut = drift(netOutPts.current[netOutPts.current.length - 1], 1.5, 0.5, 40);

      // Push and trim
      cpuPts.current = [...cpuPts.current.slice(-(HISTORY - 1)), newCpu];
      ramPts.current = [...ramPts.current.slice(-(HISTORY - 1)), newRam];
      diskPts.current = [...diskPts.current.slice(-(HISTORY - 1)), newDisk];
      netInPts.current = [...netInPts.current.slice(-(HISTORY - 1)), newIn];
      netOutPts.current = [...netOutPts.current.slice(-(HISTORY - 1)), newOut];

      setCpu(newCpu);
      setRam(newRam);
      setDisk(newDisk);
      setNetIn(newIn);
      setNetOut(newOut);

      // Draw sparklines
      if (cpuRef.current) {
        clearCanvas(cpuRef.current);
        drawSparkline(cpuRef.current, cpuPts.current, 0, 100, ACCENT);
      }
      if (ramRef.current) {
        clearCanvas(ramRef.current);
        drawSparkline(ramRef.current, ramPts.current, 0, 16, ACCENT);
      }
      if (diskRef.current) {
        clearCanvas(diskRef.current);
        drawSparkline(diskRef.current, diskPts.current, 0, 600, ACCENT);
      }
      if (netRef.current) {
        clearCanvas(netRef.current);
        const halfW = 90;
        drawSparkline(netRef.current, netInPts.current, 0, 50, CYAN, 0, halfW);
        drawSparkline(netRef.current, netOutPts.current, 0, 50, PURPLE, halfW, halfW);
      }
    }

    // Initial draw
    tick();
    const id = setInterval(tick, INTERVAL_MS);
    return () => clearInterval(id);
  }, []);

  // Set up HiDPI scaling once for each canvas
  useEffect(() => {
    const canvases = [cpuRef.current, ramRef.current, diskRef.current, netRef.current];
    const dpr = window.devicePixelRatio || 1;
    for (const c of canvases) {
      if (!c) continue;
      const ctx = c.getContext('2d');
      if (!ctx) continue;
      ctx.scale(dpr, dpr);
    }
  }, []);

  return (
    <div>
      <p className="text-[10px] font-mono uppercase tracking-widest text-halcyon-muted mb-2">
        Live Node Resources
      </p>
      <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
        <SparkCard
          label="CPU"
          valueText={`${cpu.toFixed(1)}%`}
          canvasRef={cpuRef}
        />
        <SparkCard
          label="RAM"
          valueText={`${ram.toFixed(1)} GB`}
          canvasRef={ramRef}
        />
        <SparkCard
          label="Disk I/O"
          valueText={`${disk.toFixed(0)} MB/s`}
          canvasRef={diskRef}
        />
        <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-3 card-hover">
          <p className="text-[10px] font-mono uppercase tracking-wider text-halcyon-muted">
            Network
          </p>
          <p className="font-mono text-lg font-bold tabular-nums text-halcyon-accent mt-0.5">
            In: {netIn.toFixed(1)} MB/s | Out: {netOut.toFixed(1)} MB/s
          </p>
          <canvas
            ref={netRef}
            width={360}
            height={120}
            className="mt-1"
            style={{ width: 180, height: 60 }}
          />
        </div>
      </div>
    </div>
  );
}
