'use client';

import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';
import { histPercentile } from '@/lib/types';

export function MetricsGrid() {
  const { data, loading } = useValidatorData();

  if (loading || !data) {
    return (
      <div className="flex flex-wrap gap-2">
        {Array.from({ length: 6 }).map((_, i) => (
          <div key={i} className="h-8 w-32 animate-pulse rounded border border-halcyon-border bg-halcyon-card" />
        ))}
      </div>
    );
  }

  const f = data.engine.ffi_verifier;
  const attempted = f.live_apply_attempted || 1;
  const ok = f.live_apply_ok;
  const claimed = f.live_apply_claimed;
  const diverged = f.live_apply_diverged;
  const successPct = ((ok / attempted) * 100).toFixed(1);
  const claimedPct = ((claimed / attempted) * 100).toFixed(1);

  const median = histPercentile(f.apply_duration_buckets_ms, f.apply_duration_count, 0.5);
  const p95 = histPercentile(f.apply_duration_buckets_ms, f.apply_duration_count, 0.95);
  const p99 = histPercentile(f.apply_duration_buckets_ms, f.apply_duration_count, 0.99);

  const metrics = [
    { label: 'TXs Applied', value: fmt(attempted), color: '#00FF9F' },
    { label: 'Disagreements', value: fmt(diverged), color: diverged > 0 ? '#FF3366' : '#00FF9F' },
    { label: 'Median', value: median, color: '#00FF9F' },
    { label: 'p95', value: p95, color: '#F59E0B' },
    { label: 'p99', value: p99, color: '#F97316' },
    { label: 'Success', value: `${successPct}%`, color: '#00FF9F' },
    { label: 'TEC*', value: `${claimedPct}%`, color: '#F59E0B' },
  ];

  return (
    <div className="flex flex-wrap gap-2">
      {metrics.map((m) => (
        <div
          key={m.label}
          className="card-hover flex items-center gap-2 rounded border border-halcyon-border bg-halcyon-card px-3 py-1.5"
        >
          <span className="font-mono text-[10px] uppercase tracking-wider text-halcyon-muted">
            {m.label}
          </span>
          <span className="font-mono text-sm font-bold tabular-nums" style={{ color: m.color }}>
            {m.value}
          </span>
        </div>
      ))}
    </div>
  );
}
