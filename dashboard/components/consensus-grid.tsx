'use client';

import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';

const CHIP_COUNT = 35;
const chips = Array.from({ length: CHIP_COUNT }, (_, i) => `V${String(i + 1).padStart(2, '0')}`);

export function ConsensusGrid() {
  const { data } = useValidatorData();

  const agreementCount = data?.consensus.monitor.agreement.count ?? 0;
  const unlSize = data?.consensus.unl_size ?? CHIP_COUNT;
  const totalProposals = data?.consensus.monitor.total_proposals ?? 0;
  const phase = data?.consensus.phase ?? '—';

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

      {/* ---- Chip grid ---- */}
      <div className="grid grid-cols-5 gap-2 md:grid-cols-7">
        {chips.map((label, idx) => {
          const active = idx < agreementCount;
          return (
            <div
              key={label}
              className={`flex h-[50px] items-center justify-center rounded-md border font-mono text-xs transition-all duration-200 ${
                active
                  ? 'border-halcyon-accent text-halcyon-accent'
                  : 'border-halcyon-border text-halcyon-muted'
              }`}
              style={{
                backgroundColor: '#070710',
                boxShadow: active ? '0 0 8px #00FF9F40' : 'none',
              }}
            >
              {label}
            </div>
          );
        })}
      </div>

      {/* ---- Bottom badges ---- */}
      <div className="mt-5 flex items-center gap-3">
        <span className="rounded-full border border-halcyon-accent/30 bg-halcyon-card px-3 py-1 font-mono text-xs uppercase text-halcyon-accent">
          FFI Enabled
        </span>
        <span className="rounded-full border border-halcyon-accent/30 bg-halcyon-card px-3 py-1 font-mono text-xs uppercase text-halcyon-accent">
          Rust + C++
        </span>
      </div>
    </div>
  );
}
