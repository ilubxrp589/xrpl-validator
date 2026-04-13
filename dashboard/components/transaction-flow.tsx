'use client';

import { useEffect, useRef } from 'react';
import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';

// ---------------------------------------------------------------------------
// Tx-type color map
// ---------------------------------------------------------------------------

const TX_COLORS: Record<string, string> = {
  Payment: '#7C4DFF',
  OfferCreate: '#00B0FF',
  OfferCancel: '#00FF9F',
  TrustSet: '#FFD600',
  AccountSet: '#FF6D00',
  NFTokenCreateOffer: '#E040FB',
  TicketCreate: '#80CBC4',
  CheckCash: '#4FC3F7',
};

const DEFAULT_TX_COLOR = '#6E7681';

function txColor(type: string): string {
  return TX_COLORS[type] ?? DEFAULT_TX_COLOR;
}

// ---------------------------------------------------------------------------
// Rolling window for Avg Txn/Ledger and TPS
// ---------------------------------------------------------------------------

interface RoundSample {
  count: number;
  intervalMs: number;
}

const MAX_WINDOW = 60;

// ---------------------------------------------------------------------------
// Header stat box
// ---------------------------------------------------------------------------

function StatBox({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded border border-halcyon-border bg-[#070710] px-3 py-2">
      <p className="font-mono text-[10px] uppercase tracking-wider text-halcyon-muted">
        {label}
      </p>
      <p className="mt-0.5 font-mono text-sm font-bold tabular-nums text-white">
        {value}
      </p>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Transaction Flow component
// ---------------------------------------------------------------------------

export function TransactionFlow() {
  const { data, loading } = useValidatorData();

  // Rolling window state — persists across renders.
  const windowRef = useRef<RoundSample[]>([]);
  const lastSeqRef = useRef<number>(0);
  const lastRenderMs = useRef<number>(Date.now());

  // Derived rolling stats
  const avgRef = useRef<number>(0);
  const tpsRef = useRef<number>(0);

  // Update rolling window when the ledger sequence changes.
  useEffect(() => {
    if (!data) return;

    const f = data.engine.ffi_verifier;
    const seq = f.round_ledger_seq;
    const now = Date.now();

    if (seq > 0 && seq !== lastSeqRef.current) {
      // Capture the previous round's data.
      if (lastSeqRef.current > 0) {
        const intervalMs = now - lastRenderMs.current;
        const sample: RoundSample = {
          count: f.round_tx_count,
          intervalMs: Math.max(intervalMs, 1),
        };
        const win = windowRef.current;
        win.push(sample);
        if (win.length > MAX_WINDOW) win.shift();

        // Recompute running averages.
        const totalTxs = win.reduce((s, r) => s + r.count, 0);
        const totalMs = win.reduce((s, r) => s + r.intervalMs, 0);
        avgRef.current = win.length > 0 ? totalTxs / win.length : 0;
        tpsRef.current = totalMs > 0 ? (totalTxs / totalMs) * 1000 : 0;
      }

      lastSeqRef.current = seq;
      lastRenderMs.current = now;
    }
  }, [data]);

  if (loading || !data) {
    return (
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-5">
          {Array.from({ length: 5 }).map((_, i) => (
            <div
              key={i}
              className="h-[56px] animate-pulse rounded border border-halcyon-border bg-[#070710]"
            />
          ))}
        </div>
        <div className="h-[200px] animate-pulse rounded-lg border border-halcyon-border bg-halcyon-card" />
      </div>
    );
  }

  const f = data.engine.ffi_verifier;

  // Fees display: prefer round_fees_drops if present, else lifetime in XRP.
  const feesDisplay =
    f.round_fees_drops > 0
      ? `${fmt(f.round_fees_drops)} drops`
      : `${(data.engine.lifetime_burned / 1e6).toFixed(4)} XRP`;

  // Build sorted tx-type entries (count > 0, descending, limit 8).
  const txTypes = Object.entries(f.round_tx_types)
    .filter(([, count]) => count > 0)
    .sort((a, b) => b[1] - a[1])
    .slice(0, 8);

  const maxCount = txTypes.length > 0 ? txTypes[0][1] : 1;

  return (
    <div className="space-y-3">
      {/* ---- Header bar — 5 stats ---- */}
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-5">
        <StatBox label="Ledger #" value={fmt(f.round_ledger_seq)} />
        <StatBox label="Tx Count" value={fmt(f.round_tx_count)} />
        <StatBox label="Avg Txn/Ledger" value={avgRef.current.toFixed(1)} />
        <StatBox label="Txn/Sec" value={tpsRef.current.toFixed(2)} />
        <StatBox label="Fees Burned" value={feesDisplay} />
      </div>

      {/* ---- Transaction type bars ---- */}
      <div className="rounded-lg border border-halcyon-border bg-halcyon-card p-4">
        <p className="mb-3 font-mono text-xs uppercase tracking-wider text-halcyon-muted">
          Transaction Types (Current Ledger)
        </p>

        {txTypes.length === 0 ? (
          <p className="py-4 text-center font-mono text-sm text-halcyon-muted">
            No transactions in current round
          </p>
        ) : (
          <div className="flex flex-col gap-1.5">
            {txTypes.map(([type, count]) => {
              const pct = Math.max((count / maxCount) * 100, 2); // min 2% width (~4px at 200px)
              const color = txColor(type);
              const truncName =
                type.length > 18 ? type.slice(0, 16) + '..' : type;

              return (
                <div key={type} className="flex items-center gap-2">
                  {/* Label */}
                  <span className="w-[130px] shrink-0 truncate font-mono text-xs text-halcyon-text sm:w-[160px]">
                    {truncName}
                  </span>

                  {/* Bar */}
                  <div className="relative flex-1">
                    <div
                      className="flex h-7 items-center rounded-sm px-2"
                      style={{
                        width: `${pct}%`,
                        minWidth: '4px',
                        backgroundColor: color,
                        opacity: 0.85,
                      }}
                    >
                      {pct > 15 && (
                        <span className="font-mono text-xs font-medium text-black">
                          {fmt(count)}
                        </span>
                      )}
                    </div>
                  </div>

                  {/* Count — shown outside bar when bar is narrow */}
                  {pct <= 15 && (
                    <span
                      className="font-mono text-xs tabular-nums"
                      style={{ color }}
                    >
                      {fmt(count)}
                    </span>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
