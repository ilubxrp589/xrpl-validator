'use client';

import { useState, useCallback } from 'react';
import { Copy, CheckCircle2, ExternalLink } from 'lucide-react';

// ---------------------------------------------------------------------------
// Static validator identity — these values don't change at runtime.
// ---------------------------------------------------------------------------

const DOMAIN = 'halcyon-names.io';
const SIGNING_KEY = '0225BBFD07C6BC2A89F735EB649983196EF5F7B46F35';
const MASTER_KEY = 'ed9f67a12666a4225381f728c46765798951010368a5';
const MANIFEST_INFO = '233 bytes \u00B7 Seq 1';

// ---------------------------------------------------------------------------
// Clipboard helper per field
// ---------------------------------------------------------------------------

function CopyButton({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(value).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [value]);

  return (
    <button
      type="button"
      onClick={handleCopy}
      className="ml-2 inline-flex h-6 w-6 flex-shrink-0 items-center justify-center rounded text-halcyon-muted transition-colors hover:text-halcyon-accent"
      aria-label="Copy to clipboard"
    >
      {copied ? (
        <CheckCircle2 className="h-3.5 w-3.5 text-halcyon-accent" />
      ) : (
        <Copy className="h-3.5 w-3.5" />
      )}
    </button>
  );
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function IdentityCard() {
  return (
    <div className="rounded-lg border border-halcyon-border bg-halcyon-card p-6">
      <div className="grid grid-cols-1 gap-6 md:grid-cols-2">
        {/* Domain */}
        <div>
          <p className="mb-1 font-mono text-xs uppercase tracking-wider text-halcyon-muted">
            Domain
          </p>
          <div className="flex items-center gap-2">
            <a
              href={`https://${DOMAIN}`}
              target="_blank"
              rel="noopener noreferrer"
              className="font-mono text-sm text-white transition-colors hover:text-halcyon-accent"
            >
              {DOMAIN}
            </a>
            <span className="inline-flex items-center gap-1 rounded-full border border-halcyon-accent/30 bg-halcyon-accent/10 px-2 py-0.5 text-[10px] font-semibold uppercase text-halcyon-accent">
              <CheckCircle2 className="h-3 w-3" />
              Verified
            </span>
            <ExternalLink className="h-3 w-3 text-halcyon-muted" />
          </div>
        </div>

        {/* Manifest */}
        <div>
          <p className="mb-1 font-mono text-xs uppercase tracking-wider text-halcyon-muted">
            Manifest
          </p>
          <p className="font-mono text-sm text-white">{MANIFEST_INFO}</p>
        </div>

        {/* Signing Key */}
        <div>
          <p className="mb-1 font-mono text-xs uppercase tracking-wider text-halcyon-muted">
            Signing Key
          </p>
          <div className="flex items-center">
            <span className="break-all font-mono text-sm text-white">
              {SIGNING_KEY}
            </span>
            <CopyButton value={SIGNING_KEY} />
          </div>
        </div>

        {/* Master Public Key */}
        <div>
          <p className="mb-1 font-mono text-xs uppercase tracking-wider text-halcyon-muted">
            Master Public Key
          </p>
          <div className="flex items-center">
            <span className="break-all font-mono text-sm text-white">
              {MASTER_KEY}
            </span>
            <CopyButton value={MASTER_KEY} />
          </div>
        </div>
      </div>
    </div>
  );
}
