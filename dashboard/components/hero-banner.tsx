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

  // VALAUDIT Phase 3 (va-03) signing-gate state.
  //   ACTIVE  — gate is open, validator signing on its own merit
  //   WARMUP  — bootstrapping; <3 consecutive matches, gate refusing
  //   ALERT   — gate closed mid-session OR zero_hash > 0 (anomalous)
  const skipNotReady = stateHash.validations_skipped_not_ready ?? 0;
  const skipZeroHash = stateHash.validations_skipped_zero_hash ?? 0;
  const gateState: 'active' | 'warmup' | 'alert' =
    skipZeroHash > 0
      ? 'alert'
      : stateHash.ready_to_sign
      ? 'active'
      : 'warmup';

  // Agreement calculation
  const attempted = ffi.live_apply_attempted || 0;
  const agreement =
    attempted > 0
      ? ((ffi.live_apply_ok + ffi.live_apply_claimed) / attempted) * 100
      : 0;
  const agreementStr = agreement.toFixed(2) + '%';
  const isPerfect = agreement >= 99.99;

  // Hash comparison state — three distinct UI states:
  //   PENDING   — no comparison has run yet (post-wipe, bootstrapping)
  //   MATCH     — at least one comparison has run and the latest matched
  //   MISMATCH  — at least one comparison has run and the latest diverged
  const hashComparisons = stateHash.total_matches + stateHash.total_mismatches;
  const hashState: 'pending' | 'match' | 'mismatch' =
    hashComparisons === 0
      ? 'pending'
      : stateHash.consecutive_matches > 0
      ? 'match'
      : 'mismatch';
  const displayHash = stateHash.computed_hash || '—';
  const stage3Enabled = ffi.stage3_enabled === true;

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
        {/* ---- VALAUDIT Phase 3 Signing Gate banner ---- */}
        <div className="mb-3 flex justify-center">
          {gateState === 'active' ? (
            <div
              className="flex items-center gap-3 rounded-lg border-2 border-halcyon-accent bg-halcyon-accent/10 px-5 py-2 text-halcyon-accent shadow-[0_0_24px_#00FF9F40]"
              title="Validator refuses to sign until its independently-computed state hash matches the network's for 3 consecutive ledgers (VALAUDIT Phase 3)"
            >
              <span className="relative flex h-2.5 w-2.5">
                <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-halcyon-accent opacity-75" />
                <span className="relative inline-flex h-2.5 w-2.5 rounded-full bg-halcyon-accent" />
              </span>
              <span className="font-mono text-sm font-bold uppercase tracking-widest">
                ✓ Signing Gate Active
              </span>
              <span className="text-[10px] font-medium uppercase tracking-wide text-halcyon-accent/70">
                {fmtCompact(stateHash.consecutive_matches)} consecutive matches
                {skipNotReady > 0 && ` · ${skipNotReady} warmup skipped`}
              </span>
            </div>
          ) : gateState === 'warmup' ? (
            <div
              className="flex items-center gap-3 rounded-lg border border-amber-400 bg-amber-400/10 px-5 py-2 text-amber-400"
              title="Validator is gating signing — needs 3 consecutive state-hash matches before it will sign"
            >
              <span className="h-2 w-2 rounded-full bg-amber-400 animate-pulse" />
              <span className="font-mono text-xs uppercase tracking-widest font-semibold">
                Signing Gate Warmup · {stateHash.consecutive_matches}/3 matches
              </span>
            </div>
          ) : (
            <div
              className="flex items-center gap-3 rounded-lg border-2 border-halcyon-danger bg-halcyon-danger/10 px-5 py-2 text-halcyon-danger"
              title="Signing gate alert — anomalous skip events. Investigate logs."
            >
              <span className="h-2.5 w-2.5 rounded-full bg-halcyon-danger animate-ping" />
              <span className="font-mono text-sm font-bold uppercase tracking-widest">
                ⚠ Gate Alert · zero_hash {skipZeroHash}
              </span>
            </div>
          )}
        </div>

        {/* ---- Stage 3 banner — big + obvious ---- */}
        <div className="mb-6 flex justify-center">
          {stage3Enabled ? (
            <div className="flex items-center gap-3 rounded-lg border-2 border-halcyon-accent bg-halcyon-accent/10 px-5 py-2 text-halcyon-accent shadow-[0_0_24px_#00FF9F40]">
              <span className="relative flex h-2.5 w-2.5">
                <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-halcyon-accent opacity-75" />
                <span className="relative inline-flex h-2.5 w-2.5 rounded-full bg-halcyon-accent" />
              </span>
              <span className="font-mono text-sm font-bold uppercase tracking-widest">
                ★ Stage 3 Active ★
              </span>
              <span className="text-[10px] font-medium uppercase tracking-wide text-halcyon-accent/70">
                FFI overlay writing state.rocks
              </span>
            </div>
          ) : (
            <div className="flex items-center gap-3 rounded-lg border border-halcyon-border bg-halcyon-card px-5 py-2 text-halcyon-muted">
              <span className="h-2 w-2 rounded-full bg-halcyon-muted" />
              <span className="font-mono text-xs uppercase tracking-widest">
                Stage 3 inactive · validation window
              </span>
            </div>
          )}
        </div>

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
                  hashState === 'match'
                    ? 'text-halcyon-accent'
                    : hashState === 'pending'
                    ? 'text-amber-400'
                    : 'text-halcyon-danger'
                }`}
              >
                {hashState === 'match'
                  ? 'MATCH'
                  : hashState === 'pending'
                  ? 'PENDING'
                  : 'MISMATCH'}
              </span>
              <span className="text-xs text-halcyon-muted">
                {hashState === 'pending'
                  ? 'waiting for first ledger check'
                  : `${computeMs}ms · p50 ${p50}`}
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
