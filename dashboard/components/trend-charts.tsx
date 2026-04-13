'use client';

import {
  AreaChart,
  Area,
  XAxis,
  YAxis,
  Tooltip,
  ResponsiveContainer,
  CartesianGrid,
} from 'recharts';
import { useValidatorData } from '@/hooks/use-validator-data';

interface LedgerTime {
  seq: string;
  ms: number;
  txs: number;
}

interface WalletPoint {
  seq: string;
  count: number;
}

export function TrendCharts() {
  const { data } = useValidatorData();

  // Wallet growth — from wallet_history ([seq, count][] pairs)
  const rawWallets = data?.stateHash.wallet_history ?? [];
  const walletData: WalletPoint[] = rawWallets.slice(-100).map(([seq, count]) => ({
    seq: `#${seq}`,
    count,
  }));

  // Per-ledger apply time — from sync_log (last 100 ledgers, each with
  // actual round time and tx count). This shows REAL variation per ledger
  // instead of flat cumulative percentiles.
  const syncLog = data?.stateHash.sync_log ?? [];
  const ledgerTimes: LedgerTime[] = syncLog
    .slice(0, 100)
    .reverse()
    .map((entry) => ({
      seq: `#${entry.seq}`,
      ms: Math.round(entry.time_secs * 1000),
      txs: entry.txs,
    }));

  const chartStyle = {
    fontSize: 10,
    fontFamily: 'IBM Plex Mono, monospace',
  };

  return (
    <section className="grid grid-cols-1 md:grid-cols-2 gap-4">
      {/* Wallet growth */}
      <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-4">
        <h3 className="text-xs font-mono text-halcyon-muted uppercase tracking-wider mb-3">
          Wallet Growth (recent)
        </h3>
        <div className="h-40">
          <ResponsiveContainer width="100%" height="100%">
            <AreaChart data={walletData} style={chartStyle}>
              <CartesianGrid strokeDasharray="3 3" stroke="#1A1A1A" />
              <XAxis dataKey="seq" tick={{ fill: '#666' }} interval="preserveStartEnd" />
              <YAxis
                tick={{ fill: '#666' }}
                domain={['dataMin - 10', 'dataMax + 10']}
                tickFormatter={(v: number) => v.toLocaleString()}
              />
              <Tooltip
                contentStyle={{ background: '#111', border: '1px solid #1A1A1A', borderRadius: 6, fontSize: 11, fontFamily: 'IBM Plex Mono' }}
                labelStyle={{ color: '#666' }}
                formatter={(value: number) => [value.toLocaleString(), 'Wallets']}
              />
              <Area type="monotone" dataKey="count" fill="#7C4DFF20" stroke="#7C4DFF" strokeWidth={1.5} name="count" />
            </AreaChart>
          </ResponsiveContainer>
        </div>
      </div>

      {/* Per-ledger apply time */}
      <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-4">
        <h3 className="text-xs font-mono text-halcyon-muted uppercase tracking-wider mb-3">
          Ledger Apply Time (last {ledgerTimes.length} ledgers)
        </h3>
        <div className="h-40">
          <ResponsiveContainer width="100%" height="100%">
            <AreaChart data={ledgerTimes} style={chartStyle}>
              <CartesianGrid strokeDasharray="3 3" stroke="#1A1A1A" />
              <XAxis dataKey="seq" tick={{ fill: '#666' }} interval="preserveStartEnd" />
              <YAxis tick={{ fill: '#666' }} tickFormatter={(v: number) => `${v}ms`} />
              <Tooltip
                contentStyle={{ background: '#111', border: '1px solid #1A1A1A', borderRadius: 6, fontSize: 11, fontFamily: 'IBM Plex Mono' }}
                labelStyle={{ color: '#666' }}
                formatter={(value: number, name: string) =>
                  name === 'ms' ? [`${value}ms`, 'Apply Time'] : [value, 'Txs']
                }
              />
              <Area type="monotone" dataKey="ms" fill="#00FF9F20" stroke="#00FF9F" strokeWidth={1.5} name="ms" />
            </AreaChart>
          </ResponsiveContainer>
        </div>
      </div>
    </section>
  );
}
