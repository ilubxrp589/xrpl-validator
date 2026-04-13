'use client';

import {
  AlertTriangle,
  Gauge,
  PieChart,
  Timer,
  Zap,
  type LucideIcon,
} from 'lucide-react';
import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';
import { histPercentile } from '@/lib/types';

// ---------------------------------------------------------------------------
// Card helper
// ---------------------------------------------------------------------------

interface MetricCardProps {
  title: string;
  value: string;
  subValue?: string;
  icon: LucideIcon;
  color: string;
  subColor?: string;
}

function MetricCard({
  title,
  value,
  subValue,
  icon: Icon,
  color,
  subColor,
}: MetricCardProps) {
  return (
    <div className="card-hover relative overflow-hidden rounded-lg border border-halcyon-border bg-halcyon-card p-4">
      {/* Icon — top-right, muted */}
      <Icon
        className="absolute right-3 top-3 h-5 w-5 opacity-30"
        style={{ color }}
      />

      {/* Title */}
      <p className="font-mono text-xs uppercase tracking-wider text-halcyon-muted">
        {title}
      </p>

      {/* Primary value */}
      <p
        className="mt-1.5 font-mono text-2xl font-bold tabular-nums"
        style={{ color }}
      >
        {value}
      </p>

      {/* Optional sub-value (used by Success Rate) */}
      {subValue && (
        <p
          className="mt-0.5 font-mono text-sm tabular-nums"
          style={{ color: subColor ?? color }}
        >
          {subValue}
        </p>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Grid
// ---------------------------------------------------------------------------

export function MetricsGrid() {
  const { data, loading } = useValidatorData();

  if (loading || !data) {
    return (
      <div className="grid grid-cols-2 gap-3 md:grid-cols-3">
        {Array.from({ length: 6 }).map((_, i) => (
          <div
            key={i}
            className="h-[100px] animate-pulse rounded-lg border border-halcyon-border bg-halcyon-card"
          />
        ))}
      </div>
    );
  }

  const f = data.engine.ffi_verifier;

  const attempted = f.live_apply_attempted || 1; // guard div-by-zero
  const ok = f.live_apply_ok;
  const claimed = f.live_apply_claimed;
  const diverged = f.live_apply_diverged;

  const successPct = ((ok / attempted) * 100).toFixed(1);
  const claimedPct = ((claimed / attempted) * 100).toFixed(1);

  const median = histPercentile(
    f.apply_duration_buckets_ms,
    f.apply_duration_count,
    0.5,
  );
  const p95 = histPercentile(
    f.apply_duration_buckets_ms,
    f.apply_duration_count,
    0.95,
  );
  const p99 = histPercentile(
    f.apply_duration_buckets_ms,
    f.apply_duration_count,
    0.99,
  );

  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-3">
      {/* 1 — TXs Applied via FFI */}
      <MetricCard
        title="TXs Applied via FFI"
        value={fmt(f.live_apply_attempted)}
        icon={Zap}
        color="#00FF9F"
      />

      {/* 2 — Disagreements */}
      <MetricCard
        title="Disagreements"
        value={fmt(diverged)}
        icon={AlertTriangle}
        color={diverged > 0 ? '#FF3366' : '#00FF9F'}
      />

      {/* 3 — Median Apply Time */}
      <MetricCard
        title="Median Apply Time"
        value={median}
        icon={Timer}
        color="#00FF9F"
      />

      {/* 4 — 95th % Apply Time */}
      <MetricCard
        title="95th % Apply Time"
        value={p95}
        icon={Gauge}
        color="#F59E0B"
      />

      {/* 5 — 99th % Apply Time */}
      <MetricCard
        title="99th % Apply Time"
        value={p99}
        icon={Gauge}
        color="#F97316"
      />

      {/* 6 — Success Rate */}
      <MetricCard
        title="Success Rate"
        value={`${successPct}% tesSUCCESS`}
        subValue={`${claimedPct}% TEC*`}
        icon={PieChart}
        color="#00FF9F"
        subColor="#F59E0B"
      />
    </div>
  );
}
