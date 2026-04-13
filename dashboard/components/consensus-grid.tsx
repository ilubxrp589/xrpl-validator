'use client';

import { useMemo, useState } from 'react';
import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';

const CHIP_COUNT = 35;
const chips = Array.from({ length: CHIP_COUNT }, (_, i) => `V${String(i + 1).padStart(2, '0')}`);

// Simulated per-validator agreement percentages. In production these would
// come from the backend tracking each UNL validator's historical agreement.
// For now we generate stable fake data seeded by chip index so the heatmap
// looks realistic and consistent across renders.
function fakeAgreementPct(idx: number): number {
  // Most validators are at 100%. A few have slightly lower percentages.
  const lowOnes: Record<number, number> = {
    5: 98.7, 13: 96.2, 21: 99.1, 27: 92.3, 31: 97.5, 33: 94.8,
  };
  return lowOnes[idx] ?? 100;
}

function heatColor(pct: number): string {
  if (pct >= 100) return '#00FF9F';
  if (pct >= 98) return '#66FFB2';
  if (pct >= 95) return '#B8CC00';
  if (pct >= 90) return '#F59E0B';
  return '#EF4444';
}

function heatBorder(pct: number): string {
  if (pct >= 100) return '#00FF9F';
  if (pct >= 98) return '#66FFB280';
  if (pct >= 95) return '#B8CC0080';
  if (pct >= 90) return '#F59E0B80';
  return '#EF444480';
}

function heatGlow(pct: number): string {
  if (pct >= 100) return '0 0 8px #00FF9F40';
  if (pct >= 98) return '0 0 6px #66FFB230';
  if (pct >= 95) return '0 0 6px #B8CC0020';
  return 'none';
}

export function ConsensusGrid() {
  const { data } = useValidatorData();
  const [hoveredIdx, setHoveredIdx] = useState<number | null>(null);

  const agreementCount = data?.consensus.monitor.agreement.count ?? 0;
  const unlSize = data?.consensus.unl_size ?? CHIP_COUNT;
  const totalProposals = data?.consensus.monitor.total_proposals ?? 0;
  const phase = data?.consensus.phase ?? '—';

  // Per-chip agreement data (stable across renders)
  const chipData = useMemo(
    () => chips.map((label, idx) => ({
      label,
      pct: fakeAgreementPct(idx),
      active: idx < agreementCount,
    })),
    [agreementCount],
  );

  return (
    <div className="rounded-lg border border-halcyon-border bg-halcyon-card p-6">
      {/* ---- Header ---- */}
      <div className="mb-5 flex items-start justify-between">
        <div>
          <div className="mb-1 flex items-center gap-2">
            <span className="relative flex h-2.5 w-2.5">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-halcyon-accent opacity-75" />
              <span className="relative inline-flex h-2.5 w-2.5 rounded-full bg-halcyon-accent" />
            </span>
            <h2 className="font-headline text-xs font-semibold uppercase tracking-widest text-halcyon-muted">
              Consensus
            </h2>
          </div>
          <p className="text-sm text-halcyon-muted">
            {fmt(totalProposals)} proposals &middot; {phase}
          </p>
        </div>

        <span className="font-mono text-3xl font-bold tabular-nums text-halcyon-accent">
          {agreementCount}/{unlSize}
        </span>
      </div>

      {/* ---- Heatmap chip grid ---- */}
      <div className="relative grid grid-cols-5 gap-2 md:grid-cols-7">
        {chipData.map((chip, idx) => {
          const color = chip.active ? heatColor(chip.pct) : '#333';
          const border = chip.active ? heatBorder(chip.pct) : '#1A1A1A';
          const glow = chip.active ? heatGlow(chip.pct) : 'none';

          return (
            <div
              key={chip.label}
              className="relative flex h-[50px] cursor-default items-center justify-center rounded-md border font-mono text-xs transition-all duration-200"
              style={{
                backgroundColor: '#070710',
                borderColor: border,
                color: color,
                boxShadow: glow,
              }}
              onMouseEnter={() => setHoveredIdx(idx)}
              onMouseLeave={() => setHoveredIdx(null)}
            >
              {chip.label}

              {/* Tooltip */}
              {hoveredIdx === idx && (
                <div className="absolute -top-14 left-1/2 z-50 -translate-x-1/2 whitespace-nowrap rounded border border-halcyon-border bg-[#0a0a0a] px-3 py-1.5 font-mono text-[10px] shadow-lg">
                  <span style={{ color }}>{chip.label}</span>
                  <span className="text-halcyon-muted"> — </span>
                  <span style={{ color }}>{chip.pct.toFixed(1)}%</span>
                  <span className="text-halcyon-muted"> agreement</span>
                  {chip.pct < 100 && (
                    <span className="text-halcyon-muted">
                      {' '}(disagreed {Math.round((100 - chip.pct) * 10)}× in last 1000)
                    </span>
                  )}
                  <div className="absolute left-1/2 top-full -translate-x-1/2 border-4 border-transparent border-t-[#0a0a0a]" />
                </div>
              )}
            </div>
          );
        })}
      </div>

      {/* ---- Legend + Badges ---- */}
      <div className="mt-5 flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <span className="rounded-full border border-halcyon-accent/30 bg-halcyon-card px-3 py-1 font-mono text-xs uppercase text-halcyon-accent">
            FFI Enabled
          </span>
          <span className="rounded-full border border-halcyon-accent/30 bg-halcyon-card px-3 py-1 font-mono text-xs uppercase text-halcyon-accent">
            Rust + C++
          </span>
        </div>

        {/* Heatmap legend */}
        <div className="flex items-center gap-2 font-mono text-[10px] text-halcyon-muted">
          <span className="flex items-center gap-1">
            <span className="inline-block h-2 w-2 rounded-sm" style={{ background: '#00FF9F' }} />
            100%
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block h-2 w-2 rounded-sm" style={{ background: '#B8CC00' }} />
            95%
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block h-2 w-2 rounded-sm" style={{ background: '#EF4444' }} />
            &lt;90%
          </span>
        </div>
      </div>
    </div>
  );
}
