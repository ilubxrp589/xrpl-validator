'use client';

import { useState } from 'react';
import { Copy, CheckCircle2 } from 'lucide-react';
import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt, truncHash } from '@/lib/utils';
import { histPercentile } from '@/lib/types';

function fmtCompact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toLocaleString();
}

export function HeroBanner() {
  const { data, loading } = useValidatorData();
  const [copied, setCopied] = useState(false);

  if (loading || !data) {
    return (
      <section className="flex min-h-[340px] items-center justify-center bg-halcyon-bg">
        <div className="animate-pulse font-mono text-sm text-halcyon-muted">
          Loading validator data...
        </div>
      </section>
    );
  }

  const { engine, stateHash } = data;
  const ffi = engine.ffi_verifier;

  // Agreement calculation
  const attempted = ffi.live_apply_attempted || 0;
  const agreement =
    attempted > 0
      ? ((ffi.live_apply_ok + ffi.live_apply_claimed) / attempted) * 100
      : 0;
  const agreementStr = agreement.toFixed(2) + '%';
  const isPerfect = agreement >= 99.99;

  // Hash match — use the server-side consecutive_matches counter as the
  // source of truth. It does case-insensitive comparison internally.
  // Don't compare the hex strings client-side (computed_hash is lowercase
  // from hex::encode, network_hash is uppercase from rippled — strict
  // equality would always fail and show a false "MISMATCH").
  const hashMatched = stateHash.consecutive_matches > 0;
  const displayHash = stateHash.computed_hash || '—';

  // Compute time
  const computeMs = (stateHash.compute_time_secs * 1000).toFixed(1);

  // P50 from histogram
  const p50 = histPercentile(
    ffi.apply_duration_buckets_ms,
    ffi.apply_duration_count,
    0.5,
  );

  async function copyHash() {
    if (!stateHash.computed_hash) return;
    try {
      await navigator.clipboard.writeText(stateHash.computed_hash);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // clipboard API may be unavailable
    }
  }

  return (
    <section className="bg-halcyon-bg px-4 pb-8 pt-10">
      <div className="mx-auto max-w-7xl">
        {/* ---- Agreement hero number ---- */}
        <div className="flex flex-col items-center gap-2 pb-8">
          <span
            data-chaos-trigger
            className={`animate-pulse-glow cursor-default select-none font-mono text-7xl font-bold tabular-nums text-glow md:text-8xl ${
              isPerfect
                ? 'text-halcyon-accent drop-shadow-[0_0_24px_#00FF9F60]'
                : 'text-amber-400 drop-shadow-[0_0_24px_#F59E0B60]'
            }`}
          >
            {agreementStr}
          </span>
          <span className="text-xs font-medium uppercase tracking-widest text-halcyon-muted">
            Mainnet Agreement
          </span>
        </div>

        {/* ---- Sub-cards grid ---- */}
        <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
          {/* Card 1: Current ledger */}
          <div className="rounded-lg border border-halcyon-border bg-halcyon-card p-4">
            <p className="mb-1 text-xs font-medium uppercase tracking-wide text-halcyon-muted">
              Current Ledger
            </p>
            <p className="font-mono text-2xl font-bold tabular-nums text-white">
              #{fmt(engine.ledger_seq)}
            </p>
            <p className="tooltip-trigger mt-1 flex items-center gap-1.5 text-xs text-halcyon-accent">
              <CheckCircle2 className="h-3 w-3" />
              verified via FFI
              <span className="tooltip-content">Fast Foreign Interface — Rust + libxrpl direct ledger access (no JSON-RPC lag)</span>
            </p>
          </div>

          {/* Card 2: Object count + wallet count */}
          <div className="rounded-lg border border-halcyon-border bg-halcyon-card p-4">
            <p className="mb-1 text-xs font-medium uppercase tracking-wide text-halcyon-muted">
              State Objects
            </p>
            <p className="font-mono text-2xl font-bold tabular-nums text-white">
              {fmtCompact(stateHash.db_entries)}
            </p>
            <p className="mt-1 text-xs text-halcyon-text">
              <span className="font-mono tabular-nums text-halcyon-accent">
                {fmtCompact(stateHash.wallet_count)}
              </span>{' '}
              wallets tracked
            </p>
          </div>

          {/* Card 3: Hash match + compute time + hash */}
          <div className="rounded-lg border border-halcyon-border bg-halcyon-card p-4">
            <p className="mb-1 text-xs font-medium uppercase tracking-wide text-halcyon-muted">
              Hash Verification
            </p>
            <div className="flex items-center gap-2">
              <span
                className={`font-mono text-sm font-semibold ${
                  hashMatched ? 'text-halcyon-accent' : 'text-halcyon-danger'
                }`}
              >
                {hashMatched ? 'MATCH' : 'MISMATCH'}
              </span>
              <span className="text-xs text-halcyon-muted">
                {computeMs}ms &middot; p50 {p50}
              </span>
            </div>
            <div className="mt-2 flex items-center gap-1.5">
              <code className="truncate font-mono text-xs tabular-nums text-halcyon-text">
                {truncHash(displayHash, 24)}
              </code>
              <button
                onClick={copyHash}
                className="flex-shrink-0 rounded p-1 text-halcyon-muted transition-colors hover:bg-halcyon-border hover:text-white"
                aria-label="Copy hash"
              >
                {copied ? (
                  <CheckCircle2 className="h-3.5 w-3.5 text-halcyon-accent" />
                ) : (
                  <Copy className="h-3.5 w-3.5" />
                )}
              </button>
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}
