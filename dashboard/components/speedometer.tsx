'use client';

import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';

const BATTLE_TARGET = 50_000;

export function Speedometer() {
  const { data } = useValidatorData();

  const ffi = data?.engine.ffi_verifier;
  const applied = ffi?.ledgers_applied ?? 0;
  const shadowMatched = ffi?.shadow_hash_matched ?? 0;
  const shadowAttempted = ffi?.shadow_hash_attempted ?? 0;
  const shadowMiss = ffi?.shadow_hash_mismatched ?? 0;
  const diverged = ffi?.live_apply_diverged ?? 0;
  const dbHits = ffi?.db_hits ?? 0;
  const rpcFallbacks = ffi?.db_rpc_fallbacks ?? 0;
  const dbTotal = dbHits + rpcFallbacks;
  const dbHitPct = dbTotal > 0 ? ((dbHits / dbTotal) * 100).toFixed(1) : '—';
  const battlePct = Math.min((shadowAttempted / BATTLE_TARGET) * 100, 100);
  const etaDays = shadowAttempted > 0 ? Math.max(0, Math.ceil((BATTLE_TARGET - shadowAttempted) / 17000)) : 0;

  // Uptime
  const startMs = data?.engine.start_time_ms ?? 0;
  const uptimeSec = startMs > 0 ? Math.floor((Date.now() - startMs) / 1000) : 0;
  const uptimeH = Math.floor(uptimeSec / 3600);
  const uptimeM = Math.floor((uptimeSec % 3600) / 60);
  const uptimeStr = uptimeH > 0 ? `${uptimeH}h ${uptimeM}m` : `${uptimeM}m`;

  return (
    <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-4 card-hover">
      {/* Shadow Hash — the big number */}
      <div className="flex items-center justify-between mb-3">
        <span className="font-mono text-[10px] uppercase tracking-widest text-halcyon-muted">
          Shadow Hash
        </span>
        <span className={`font-mono text-xs font-bold ${shadowMiss === 0 ? 'text-halcyon-accent' : 'text-halcyon-danger'}`}>
          {shadowMiss === 0 ? '100% MATCH' : `${shadowMiss} MISS`}
        </span>
      </div>
      <div className="flex items-baseline gap-1">
        <span className="font-mono text-2xl font-bold tabular-nums text-halcyon-accent">
          {fmt(shadowMatched)}
        </span>
        <span className="font-mono text-sm text-halcyon-muted">/ {fmt(shadowAttempted)}</span>
      </div>

      {/* Key stats */}
      <div className="mt-3 grid grid-cols-2 gap-x-4 gap-y-1.5">
        <div>
          <span className="font-mono text-[9px] text-halcyon-muted block">FFI Diverged</span>
          <span className={`font-mono text-sm font-bold tabular-nums ${diverged > 0 ? 'text-amber-400' : 'text-halcyon-accent'}`}>
            {diverged}
          </span>
        </div>
        <div>
          <span className="font-mono text-[9px] text-halcyon-muted block">DB Hit Rate</span>
          <span className="font-mono text-sm font-bold tabular-nums text-halcyon-accent">{dbHitPct}%</span>
        </div>
        <div>
          <span className="font-mono text-[9px] text-halcyon-muted block">Verified</span>
          <span className="font-mono text-sm font-bold tabular-nums text-white">{fmt(shadowAttempted)}</span>
        </div>
        <div>
          <span className="font-mono text-[9px] text-halcyon-muted block">Uptime</span>
          <span className="font-mono text-sm font-bold tabular-nums text-white">{uptimeStr}</span>
        </div>
      </div>

      {/* Battle test progress */}
      {applied > 0 && (
        <div className="mt-3 border-t border-halcyon-border pt-3">
          <div className="flex items-center justify-between mb-1.5">
            <span className="flex items-center gap-1.5">
              <span className="relative flex h-1.5 w-1.5">
                <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-halcyon-accent opacity-75" />
                <span className="relative inline-flex h-1.5 w-1.5 rounded-full bg-halcyon-accent" />
              </span>
              <span className="font-mono text-[9px] uppercase tracking-widest text-halcyon-muted">Validation Window</span>
            </span>
            <span className="font-mono text-[10px] tabular-nums text-halcyon-accent">
              {(shadowAttempted / 1000).toFixed(1)}K / 50K · ~{etaDays}d
            </span>
          </div>
          <div className="h-1.5 w-full rounded-full bg-halcyon-border overflow-hidden">
            <div className="h-full rounded-full" style={{
              width: `${battlePct}%`,
              background: 'linear-gradient(90deg, #00FF9F, #00f0ff)',
              boxShadow: '0 0 6px #00FF9F60',
              transition: 'width 500ms ease-out',
            }} />
          </div>
        </div>
      )}
    </div>
  );
}
