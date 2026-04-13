'use client';

import { useEffect, useRef, useState } from 'react';
import { useValidatorData } from '@/hooks/use-validator-data';

// ---------------------------------------------------------------------------
// Arc geometry helpers
// ---------------------------------------------------------------------------

const RADIUS = 60;
const CX = 70;
const CY = 70;
const STROKE_WIDTH = 10;
const MAX_TPS = 50;

// 270-degree arc from 135deg to 405deg (i.e. -225deg gap at the bottom)
const START_ANGLE = 135; // degrees
const END_ANGLE = 405; // degrees
const ARC_SPAN = END_ANGLE - START_ANGLE; // 270

function polarToCartesian(cx: number, cy: number, r: number, angleDeg: number) {
  const rad = ((angleDeg - 90) * Math.PI) / 180;
  return {
    x: cx + r * Math.cos(rad),
    y: cy + r * Math.sin(rad),
  };
}

function describeArc(cx: number, cy: number, r: number, startDeg: number, endDeg: number) {
  const start = polarToCartesian(cx, cy, r, endDeg);
  const end = polarToCartesian(cx, cy, r, startDeg);
  const largeArc = endDeg - startDeg > 180 ? 1 : 0;
  return `M ${start.x} ${start.y} A ${r} ${r} 0 ${largeArc} 0 ${end.x} ${end.y}`;
}

// Full background arc path
const BG_ARC = describeArc(CX, CY, RADIUS, START_ANGLE, END_ANGLE);

// Circumference of the 270-degree arc (for dasharray)
const ARC_LENGTH = (ARC_SPAN / 360) * 2 * Math.PI * RADIUS;

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function Speedometer() {
  const { data } = useValidatorData();
  const [tps, setTps] = useState(0);

  const prevSeq = useRef<number>(0);
  const prevTime = useRef<number>(0);
  const prevTxCount = useRef<number>(0);
  const tpsWindow = useRef<number[]>([]);

  useEffect(() => {
    if (!data) return;

    const seq = data.engine.ledger_seq;
    const txCount = data.engine.round_tx_count;
    const now = Date.now();

    if (prevSeq.current > 0 && seq > prevSeq.current) {
      const intervalMs = now - prevTime.current;
      if (intervalMs > 0) {
        const instantTps = (txCount / intervalMs) * 1000;
        tpsWindow.current = [...tpsWindow.current.slice(-9), instantTps];
        const avg =
          tpsWindow.current.reduce((s, v) => s + v, 0) /
          tpsWindow.current.length;
        setTps(Math.min(avg, MAX_TPS));
      }
    }

    prevSeq.current = seq;
    prevTime.current = now;
    prevTxCount.current = txCount;
  }, [data]);

  // Compute the value arc dash
  const ratio = Math.min(tps / MAX_TPS, 1);
  const dashLen = ratio * ARC_LENGTH;
  const dashGap = ARC_LENGTH - dashLen;

  return (
    <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-4 card-hover flex flex-col items-center">
      <svg viewBox="0 0 140 140" width={140} height={140}>
        <defs>
          <filter id="glow">
            <feGaussianBlur stdDeviation="3" />
            <feMerge>
              <feMergeNode />
              <feMergeNode in="SourceGraphic" />
            </feMerge>
          </filter>
        </defs>

        {/* Background arc */}
        <path
          d={BG_ARC}
          fill="none"
          stroke="#1A1A1A"
          strokeWidth={STROKE_WIDTH}
          strokeLinecap="round"
        />

        {/* Value arc */}
        <path
          d={BG_ARC}
          fill="none"
          stroke="#00FF9F"
          strokeWidth={STROKE_WIDTH}
          strokeLinecap="round"
          strokeDasharray={`${dashLen} ${dashGap}`}
          filter="url(#glow)"
          style={{ transition: 'stroke-dasharray 0.5s ease' }}
        />

        {/* Center text — TPS value */}
        <text
          x={CX}
          y={CY - 2}
          textAnchor="middle"
          dominantBaseline="central"
          className="font-mono text-2xl font-bold"
          fill="#00FF9F"
          style={{ fontSize: 28 }}
        >
          {tps.toFixed(1)}
        </text>

        {/* Label */}
        <text
          x={CX}
          y={CY + 20}
          textAnchor="middle"
          dominantBaseline="central"
          className="font-mono"
          fill="#666666"
          style={{ fontSize: 10, letterSpacing: '0.1em' }}
        >
          TX/SEC
        </text>
      </svg>
    </div>
  );
}
