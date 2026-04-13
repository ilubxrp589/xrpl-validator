'use client';

import { useValidatorData } from '@/hooks/use-validator-data';
import { ExternalLink } from 'lucide-react';

export function Footer() {
  const { data } = useValidatorData();

  const ago = data
    ? Math.max(0, Math.round((Date.now() - data.updatedAt) / 1000))
    : null;

  return (
    <footer className="border-t border-halcyon-border bg-halcyon-bg/80 backdrop-blur-sm px-6 py-3 mt-8">
      <div className="max-w-7xl mx-auto flex flex-col sm:flex-row justify-between items-center gap-2 text-xs font-mono text-halcyon-muted">
        <div className="flex items-center gap-4">
          <span>
            Last updated:{' '}
            <span className="text-halcyon-text">
              {ago !== null ? `${ago}s ago` : '—'}
            </span>
          </span>
          <span className="hidden sm:inline text-halcyon-border">|</span>
          <span className="hidden sm:inline">
            Built with &#10084;&#65039; on Rust + libxrpl FFI
          </span>
        </div>
        <div className="flex items-center gap-4">
          <a
            href="/api/engine"
            target="_blank"
            rel="noopener noreferrer"
            className="flex items-center gap-1 hover:text-halcyon-accent transition-colors"
          >
            Raw API
            <ExternalLink className="h-3 w-3" />
          </a>
          <a
            href="https://github.com/ilubxrp589/xrpl-validator"
            target="_blank"
            rel="noopener noreferrer"
            className="flex items-center gap-1 hover:text-halcyon-accent transition-colors"
          >
            GitHub
            <ExternalLink className="h-3 w-3" />
          </a>
        </div>
      </div>
    </footer>
  );
}
