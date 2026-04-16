'use client';

import { useValidatorData } from '@/hooks/use-validator-data';

interface ParsedTx {
  ledger: string;
  type: string;
  ter: string;
  hash: string;
  ms: string;
  mut: string;
}

/** Parse one `recent_tx_samples` line into fields.
 *  Format: "L{seq} {tx_type}/{ter_name} {short_hash} {ms}ms mut={N}" */
function parseSample(line: string): ParsedTx | null {
  const parts = line.split(' ');
  if (parts.length < 5) return null;
  const ledger = parts[0];
  const [type, ter] = parts[1].split('/');
  const hash = parts[2];
  const msMatch = parts[3].match(/^(\d+)ms$/);
  const mutMatch = parts[4].match(/^mut=(\d+)$/);
  if (!type || !ter || !msMatch || !mutMatch) return null;
  return { ledger, type, ter, hash, ms: msMatch[1], mut: mutMatch[1] };
}

function terColor(ter: string): string {
  if (ter === 'tesSUCCESS') return 'text-halcyon-accent';
  if (ter.startsWith('tec')) return 'text-cyan-400';
  return 'text-halcyon-danger';
}

function typeIcon(type: string): string {
  switch (type) {
    case 'Payment':
      return '→';
    case 'OfferCreate':
      return '+';
    case 'OfferCancel':
      return '−';
    case 'TrustSet':
      return '⊸';
    case 'AccountSet':
      return '⚙';
    case 'TicketCreate':
      return '✦';
    case 'NFTokenCreateOffer':
    case 'NFTokenAcceptOffer':
      return '◆';
    case 'AMMDeposit':
    case 'AMMWithdraw':
      return '⇌';
    default:
      return '·';
  }
}

export function TxFeed() {
  const { data, loading } = useValidatorData();

  if (loading || !data) {
    return (
      <section className="rounded-lg border border-halcyon-border bg-halcyon-card p-4">
        <div className="h-40 animate-pulse" />
      </section>
    );
  }

  const ffi = data.engine.ffi_verifier;
  const samples = ffi.recent_tx_samples ?? [];
  // Show most recent first (buffer pushes append — last element is newest)
  const parsed = samples
    .slice()
    .reverse()
    .map(parseSample)
    .filter((t): t is ParsedTx => t !== null)
    .slice(0, 20);

  return (
    <section className="rounded-lg border border-halcyon-border bg-halcyon-card">
      <header className="flex items-center justify-between border-b border-halcyon-border px-4 py-2.5">
        <div className="flex items-center gap-2">
          <span className="relative flex h-2 w-2">
            <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-halcyon-accent opacity-75" />
            <span className="relative inline-flex h-2 w-2 rounded-full bg-halcyon-accent" />
          </span>
          <h2 className="font-mono text-xs font-semibold uppercase tracking-widest text-white">
            Live TX Feed
          </h2>
          <span className="text-[10px] uppercase tracking-wider text-halcyon-muted">
            from FFI engine · last {parsed.length || 50}
          </span>
        </div>
        <span className="font-mono text-[10px] uppercase tracking-wider text-halcyon-muted">
          {ffi.live_apply_attempted.toLocaleString()} total applied
        </span>
      </header>

      {parsed.length === 0 ? (
        <div className="p-6 text-center font-mono text-xs text-halcyon-muted">
          waiting for ws_sync to start applying txs…
        </div>
      ) : (
        <div className="max-h-[420px] overflow-y-auto">
          <table className="w-full border-collapse font-mono text-[11px] tabular-nums">
            <thead className="sticky top-0 bg-halcyon-card/95 backdrop-blur-sm">
              <tr className="border-b border-halcyon-border/60 text-left text-[9px] uppercase tracking-wider text-halcyon-muted">
                <th className="px-3 py-2 font-medium">Ledger</th>
                <th className="px-2 py-2 font-medium"></th>
                <th className="px-2 py-2 font-medium">Type</th>
                <th className="px-2 py-2 font-medium">Result</th>
                <th className="px-2 py-2 font-medium">Hash</th>
                <th className="px-2 py-2 text-right font-medium">ms</th>
                <th className="px-3 py-2 text-right font-medium">Δ</th>
              </tr>
            </thead>
            <tbody>
              {parsed.map((tx, i) => (
                <tr
                  key={`${tx.ledger}-${tx.hash}-${i}`}
                  className="border-b border-halcyon-border/20 transition-colors hover:bg-halcyon-border/20"
                >
                  <td className="px-3 py-1.5 text-halcyon-text">{tx.ledger}</td>
                  <td className="px-2 py-1.5 text-center text-halcyon-muted">
                    {typeIcon(tx.type)}
                  </td>
                  <td className="px-2 py-1.5 text-halcyon-text">{tx.type}</td>
                  <td className={`px-2 py-1.5 font-semibold ${terColor(tx.ter)}`}>
                    {tx.ter}
                  </td>
                  <td className="px-2 py-1.5 text-halcyon-muted">{tx.hash}</td>
                  <td className="px-2 py-1.5 text-right text-halcyon-text">
                    {tx.ms}
                  </td>
                  <td className="px-3 py-1.5 text-right text-halcyon-muted">
                    {tx.mut}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
