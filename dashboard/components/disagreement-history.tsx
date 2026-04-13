'use client';

import { AlertTriangle, CheckCircle2 } from 'lucide-react';
import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';

// ---------------------------------------------------------------------------
// Parse a diverged_tx_samples entry
// Format: "OfferCreate/terPRE_SEQ C286F04DB93BF00D..."
// ---------------------------------------------------------------------------

interface ParsedDisagreement {
  txType: string;
  ter: string;
  hash: string;
}

function parseSample(raw: string): ParsedDisagreement | null {
  const spaceIdx = raw.indexOf(' ');
  if (spaceIdx === -1) return null;
  const typeAndTer = raw.slice(0, spaceIdx);
  const hash = raw.slice(spaceIdx + 1).trim();
  const slashIdx = typeAndTer.indexOf('/');
  if (slashIdx === -1) return null;
  return {
    txType: typeAndTer.slice(0, slashIdx),
    ter: typeAndTer.slice(slashIdx + 1),
    hash,
  };
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function DisagreementHistory() {
  const { data, loading } = useValidatorData();

  if (loading || !data) {
    return (
      <div className="h-[300px] animate-pulse rounded-lg border border-halcyon-border bg-halcyon-card" />
    );
  }

  const f = data.engine.ffi_verifier;
  const totalDiverged = f.live_apply_diverged;
  const samples = f.diverged_tx_samples ?? [];
  const parsed = samples.map(parseSample).filter(Boolean) as ParsedDisagreement[];

  const countColor = totalDiverged > 0 ? 'text-halcyon-danger' : 'text-halcyon-accent';

  return (
    <div className="card-hover rounded-lg border border-halcyon-border bg-halcyon-card p-4">
      {/* Header */}
      <div className="flex items-start justify-between">
        <p className="font-mono text-xs uppercase tracking-wider text-halcyon-muted">
          Last Disagreements
        </p>
        <p className={`font-mono text-2xl font-bold tabular-nums ${countColor}`}>
          {fmt(totalDiverged)}
        </p>
      </div>

      {/* Content */}
      <div className="mt-4">
        {totalDiverged === 0 && parsed.length === 0 ? (
          /* No disagreements at all */
          <div className="flex flex-col items-center justify-center py-8 gap-2">
            <CheckCircle2 className="h-6 w-6 text-halcyon-accent" />
            <p className="font-mono text-sm text-halcyon-accent">
              No disagreements recorded
            </p>
          </div>
        ) : parsed.length === 0 && totalDiverged > 0 ? (
          /* Disagreements exist but samples are unavailable */
          <div className="flex flex-col items-center justify-center py-8 gap-2">
            <AlertTriangle className="h-6 w-6 text-halcyon-danger" />
            <p className="font-mono text-sm text-halcyon-danger">
              {fmt(totalDiverged)} disagreements recorded (samples unavailable)
            </p>
          </div>
        ) : (
          /* Timeline of parsed samples */
          <div className="max-h-[240px] overflow-y-auto">
            {parsed.map((entry, idx) => (
              <div key={idx} className="relative flex gap-3">
                {/* Timeline line + dot */}
                <div className="flex flex-col items-center">
                  <div className="mt-1 h-2 w-2 flex-shrink-0 rounded-full bg-halcyon-danger shadow-[0_0_6px_#FF3366]" />
                  {idx < parsed.length - 1 && (
                    <div className="w-[2px] flex-1 bg-halcyon-border" />
                  )}
                </div>

                {/* Content */}
                <div className="pb-4">
                  <p className="font-mono text-xs text-halcyon-muted">
                    Ledger #{fmt(data.engine.ledger_seq)}
                  </p>
                  <p className="font-mono text-sm text-halcyon-danger">
                    {entry.txType}/{entry.ter}
                  </p>
                  <p className="font-mono text-[10px] text-halcyon-muted/60">
                    {entry.hash.length > 16 ? entry.hash.slice(0, 16) + '...' : entry.hash}
                  </p>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
