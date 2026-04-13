'use client';

import { useEffect, useRef, useState } from 'react';
import {
  AreaChart,
  Area,
  LineChart,
  Line,
  XAxis,
  YAxis,
  Tooltip,
  ResponsiveContainer,
  CartesianGrid,
} from 'recharts';
import { useValidatorData } from '@/hooks/use-validator-data';
import { histPercentile } from '@/lib/types';

interface AgreementPoint {
  time: string;
  pct: number;
}

interface LatencyPoint {
  time: string;
  p50: number;
  p95: number;
  p99: number;
}

const MAX_POINTS = 200; // ~12 minutes at 3.5s/ledger

export function TrendCharts() {
  const { data } = useValidatorData();
  const [agreement, setAgreement] = useState<AgreementPoint[]>([]);
  const [latency, setLatency] = useState<LatencyPoint[]>([]);
  const lastSeq = useRef(0);

  useEffect(() => {
    if (!data) return;
    const f = data.engine.ffi_verifier;
    const seq = f.round_ledger_seq;
    if (seq === lastSeq.current || seq === 0) return;
    lastSeq.current = seq;

    const now = new Date();
    const time = `${now.getHours().toString().padStart(2, '0')}:${now.getMinutes().toString().padStart(2, '0')}`;

    const att = f.live_apply_attempted;
    const ok = f.live_apply_ok + f.live_apply_claimed;
    const pct = att > 0 ? (ok / att) * 100 : 100;

    setAgreement((prev) => {
      const next = [...prev, { time, pct }];
      return next.length > MAX_POINTS ? next.slice(-MAX_POINTS) : next;
    });

    const buckets = f.apply_duration_buckets_ms;
    const count = f.apply_duration_count;
    const parseMs = (s: string) => {
      if (s === '>1s') return 1000;
      if (s === '—') return 0;
      return parseInt(s, 10) || 0;
    };

    setLatency((prev) => {
      const next = [
        ...prev,
        {
          time,
          p50: parseMs(histPercentile(buckets, count, 0.5)),
          p95: parseMs(histPercentile(buckets, count, 0.95)),
          p99: parseMs(histPercentile(buckets, count, 0.99)),
        },
      ];
      return next.length > MAX_POINTS ? next.slice(-MAX_POINTS) : next;
    });
  }, [data]);

  const chartStyle = {
    fontSize: 10,
    fontFamily: 'IBM Plex Mono, monospace',
  };

  return (
    <section className="grid grid-cols-1 md:grid-cols-2 gap-4">
      {/* Agreement trend */}
      <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-4">
        <h3 className="text-xs font-mono text-halcyon-muted uppercase tracking-wider mb-3">
          Agreement % (rolling)
        </h3>
        <div className="h-40">
          <ResponsiveContainer width="100%" height="100%">
            <LineChart data={agreement} style={chartStyle}>
              <CartesianGrid strokeDasharray="3 3" stroke="#1A1A1A" />
              <XAxis dataKey="time" tick={{ fill: '#666' }} interval="preserveStartEnd" />
              <YAxis domain={[99, 100.1]} tick={{ fill: '#666' }} tickFormatter={(v) => `${v}%`} />
              <Tooltip
                contentStyle={{ background: '#111', border: '1px solid #1A1A1A', borderRadius: 6, fontSize: 11, fontFamily: 'IBM Plex Mono' }}
                labelStyle={{ color: '#666' }}
              />
              <Line type="monotone" dataKey="pct" stroke="#00FF9F" strokeWidth={2} dot={false} name="Agreement %" />
            </LineChart>
          </ResponsiveContainer>
        </div>
      </div>

      {/* Latency trend */}
      <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-4">
        <h3 className="text-xs font-mono text-halcyon-muted uppercase tracking-wider mb-3">
          Apply Latency Percentiles (rolling)
        </h3>
        <div className="h-40">
          <ResponsiveContainer width="100%" height="100%">
            <AreaChart data={latency} style={chartStyle}>
              <CartesianGrid strokeDasharray="3 3" stroke="#1A1A1A" />
              <XAxis dataKey="time" tick={{ fill: '#666' }} interval="preserveStartEnd" />
              <YAxis tick={{ fill: '#666' }} tickFormatter={(v) => `${v}ms`} />
              <Tooltip
                contentStyle={{ background: '#111', border: '1px solid #1A1A1A', borderRadius: 6, fontSize: 11, fontFamily: 'IBM Plex Mono' }}
                labelStyle={{ color: '#666' }}
              />
              <Area type="monotone" dataKey="p99" fill="#FF636340" stroke="#FF6363" strokeWidth={1} name="p99" />
              <Area type="monotone" dataKey="p95" fill="#FFD60040" stroke="#FFD600" strokeWidth={1} name="p95" />
              <Area type="monotone" dataKey="p50" fill="#00FF9F40" stroke="#00FF9F" strokeWidth={1.5} name="p50 (median)" />
            </AreaChart>
          </ResponsiveContainer>
        </div>
      </div>
    </section>
  );
}
