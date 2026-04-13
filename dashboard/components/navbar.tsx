'use client';

import { CheckCircle2, Radio, ExternalLink } from 'lucide-react';
import { useValidatorData } from '@/hooks/use-validator-data';
import { fmt } from '@/lib/utils';

const STATUS_LABEL: Record<string, string> = {
  connecting: 'Connecting',
  live: 'Live',
  stale: 'Stale',
  error: 'Error',
};

const STATUS_DOT: Record<string, string> = {
  connecting: 'bg-yellow-400',
  live: 'bg-halcyon-accent',
  stale: 'bg-yellow-500',
  error: 'bg-halcyon-danger',
};

export function Navbar() {
  const { data, loading, status } = useValidatorData();
  const ledgerSeq = data?.engine.ledger_seq ?? 0;

  return (
    <header className="sticky top-0 z-50 h-14 border-b border-halcyon-border bg-halcyon-bg/80 backdrop-blur-md">
      <nav className="mx-auto flex h-full max-w-7xl items-center justify-between px-4">
        {/* ---- Left: brand + domain badge ---- */}
        <div className="flex items-center gap-3">
          <span className="font-headline text-lg font-bold tracking-tight text-halcyon-accent">
            HALCYON XRPL
          </span>
          <a
            href="https://halcyon-names.io"
            target="_blank"
            rel="noopener noreferrer"
            className="hidden items-center gap-1.5 rounded-full border border-halcyon-border bg-halcyon-card px-2.5 py-0.5 text-xs text-halcyon-text transition-colors hover:border-halcyon-accent/40 sm:flex"
          >
            <CheckCircle2 className="h-3 w-3 text-halcyon-accent" />
            <span className="font-mono">halcyon-names.io</span>
            <ExternalLink className="h-3 w-3 text-halcyon-muted" />
          </a>
        </div>

        {/* ---- Center: live status pill ---- */}
        <div className="flex items-center gap-2">
          <div className="flex items-center gap-2 rounded-full border border-halcyon-border bg-halcyon-card px-3 py-1">
            <span
              className={`h-2 w-2 rounded-full ${STATUS_DOT[status]} ${
                status === 'live' ? 'animate-pulse' : ''
              }`}
            />
            <span className="text-xs font-medium text-halcyon-text">
              {STATUS_LABEL[status]}
            </span>
            {!loading && ledgerSeq > 0 && (
              <>
                <span className="text-halcyon-muted">|</span>
                <span className="font-mono text-xs tabular-nums text-white">
                  Ledger #{fmt(ledgerSeq)}
                </span>
              </>
            )}
          </div>
        </div>

        {/* ---- Right: version badge + live indicator ---- */}
        <div className="flex items-center gap-3">
          <span className="hidden rounded border border-halcyon-border bg-halcyon-card px-2 py-0.5 font-mono text-[10px] text-halcyon-muted sm:inline-block">
            LIBXRPL 3.2.0-b0
          </span>
          <div className="flex items-center gap-1.5">
            <Radio
              className={`h-4 w-4 ${
                status === 'live' ? 'text-halcyon-accent' : 'text-halcyon-muted'
              }`}
            />
            <span
              className={`h-2.5 w-2.5 rounded-full ${
                status === 'live'
                  ? 'animate-pulse bg-halcyon-accent shadow-[0_0_6px_#00FF9F80]'
                  : 'bg-halcyon-muted'
              }`}
            />
          </div>
        </div>
      </nav>
    </header>
  );
}
