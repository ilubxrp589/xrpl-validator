'use client';

import { useEffect, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { ExternalLink, GitCommit, ScrollText } from 'lucide-react';

type Commit = {
  sha: string;
  sha_full: string;
  date: string | null;
  headline: string;
  body: string;
  url: string;
};

type Props = {
  progressMd: string;
  progressMtime: string;
};

type Tab = 'progress' | 'changelog';

export function LogsClient({ progressMd, progressMtime }: Props) {
  const [tab, setTab] = useState<Tab>('progress');
  const [commits, setCommits] = useState<Commit[] | null>(null);
  const [commitsError, setCommitsError] = useState<string | null>(null);
  const [commitsLoading, setCommitsLoading] = useState(false);

  useEffect(() => {
    if (tab !== 'changelog' || commits !== null || commitsLoading) return;
    setCommitsLoading(true);
    fetch('/api/commits')
      .then(async (r) => {
        const body = await r.json().catch(() => ({}));
        if (!r.ok) throw new Error(body.error || `HTTP ${r.status}`);
        return body.commits as Commit[];
      })
      .then((cs) => setCommits(cs))
      .catch((err) => setCommitsError(String(err.message || err)))
      .finally(() => setCommitsLoading(false));
  }, [tab, commits, commitsLoading]);

  return (
    <div>
      {/* Tab header */}
      <div className="mb-6 flex border-b border-halcyon-border">
        <TabButton
          active={tab === 'progress'}
          onClick={() => setTab('progress')}
          icon={<ScrollText className="h-3.5 w-3.5" />}
          label="Progress"
          sub="hand-curated"
        />
        <TabButton
          active={tab === 'changelog'}
          onClick={() => setTab('changelog')}
          icon={<GitCommit className="h-3.5 w-3.5" />}
          label="Changelog"
          sub="live from GitHub"
        />
      </div>

      {/* Tab body */}
      {tab === 'progress' && (
        <ProgressView md={progressMd} mtime={progressMtime} />
      )}
      {tab === 'changelog' && (
        <ChangelogView
          commits={commits}
          loading={commitsLoading}
          error={commitsError}
        />
      )}
    </div>
  );
}

function TabButton({
  active,
  onClick,
  icon,
  label,
  sub,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  sub: string;
}) {
  return (
    <button
      onClick={onClick}
      className={`-mb-px flex items-center gap-2 border-b-2 px-4 py-3 text-sm font-medium transition-colors ${
        active
          ? 'border-halcyon-accent text-halcyon-accent'
          : 'border-transparent text-halcyon-muted hover:text-halcyon-text'
      }`}
    >
      {icon}
      <span className="font-headline uppercase tracking-wider">{label}</span>
      <span className="hidden text-[10px] font-normal normal-case text-halcyon-muted sm:inline">
        · {sub}
      </span>
    </button>
  );
}

function ProgressView({ md, mtime }: { md: string; mtime: string }) {
  return (
    <div>
      {mtime && (
        <div className="mb-4 rounded-md border border-halcyon-border bg-halcyon-card px-3 py-2 text-xs text-halcyon-muted">
          <span className="uppercase tracking-wider">File last modified:</span>{' '}
          <span className="font-mono text-halcyon-text">{mtime}</span>
          <span className="mx-2">·</span>
          Narrative, manually updated. For a canonical commit-level view, switch
          to the Changelog tab.
        </div>
      )}
      <article className="prose-halcyon">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          components={{
            h1: (props) => (
              <h1
                className="mt-8 mb-4 font-headline text-2xl font-bold tracking-tight text-halcyon-accent first:mt-0"
                {...props}
              />
            ),
            h2: (props) => (
              <h2
                className="mt-8 mb-3 border-b border-halcyon-border pb-1 font-headline text-xl font-semibold tracking-tight text-halcyon-text"
                {...props}
              />
            ),
            h3: (props) => (
              <h3
                className="mt-6 mb-2 font-headline text-base font-semibold tracking-tight text-halcyon-text"
                {...props}
              />
            ),
            p: (props) => (
              <p className="mb-4 text-sm leading-relaxed text-halcyon-text" {...props} />
            ),
            ul: (props) => (
              <ul className="mb-4 list-disc space-y-1.5 pl-6 text-sm text-halcyon-text" {...props} />
            ),
            ol: (props) => (
              <ol className="mb-4 list-decimal space-y-1.5 pl-6 text-sm text-halcyon-text" {...props} />
            ),
            li: (props) => <li className="leading-relaxed" {...props} />,
            a: (props) => (
              <a
                className="font-medium text-halcyon-accent underline decoration-halcyon-accent/40 underline-offset-2 hover:decoration-halcyon-accent"
                target="_blank"
                rel="noopener noreferrer"
                {...props}
              />
            ),
            code: ({ className, children, ...rest }) => {
              const isInline = !className;
              if (isInline) {
                return (
                  <code
                    className="rounded bg-halcyon-border/50 px-1 py-0.5 font-mono text-[0.85em] text-halcyon-accent"
                    {...rest}
                  >
                    {children}
                  </code>
                );
              }
              return (
                <code className={`${className} font-mono text-xs`} {...rest}>
                  {children}
                </code>
              );
            },
            pre: (props) => (
              <pre
                className="mb-4 overflow-x-auto rounded-md border border-halcyon-border bg-halcyon-card p-3 text-xs leading-relaxed text-halcyon-text"
                {...props}
              />
            ),
            table: (props) => (
              <div className="mb-4 overflow-x-auto rounded-md border border-halcyon-border">
                <table
                  className="min-w-full divide-y divide-halcyon-border text-sm"
                  {...props}
                />
              </div>
            ),
            thead: (props) => (
              <thead className="bg-halcyon-card text-halcyon-muted" {...props} />
            ),
            th: (props) => (
              <th
                className="px-3 py-2 text-left font-headline text-xs font-semibold uppercase tracking-wider"
                {...props}
              />
            ),
            td: (props) => (
              <td className="px-3 py-2 align-top text-halcyon-text" {...props} />
            ),
            hr: () => <hr className="my-8 border-halcyon-border" />,
            blockquote: (props) => (
              <blockquote
                className="mb-4 border-l-2 border-halcyon-accent/50 pl-4 italic text-halcyon-muted"
                {...props}
              />
            ),
            strong: (props) => (
              <strong className="font-semibold text-white" {...props} />
            ),
          }}
        >
          {md}
        </ReactMarkdown>
      </article>
    </div>
  );
}

function ChangelogView({
  commits,
  loading,
  error,
}: {
  commits: Commit[] | null;
  loading: boolean;
  error: string | null;
}) {
  if (loading) {
    return (
      <div className="space-y-3">
        {[...Array(6)].map((_, i) => (
          <div
            key={i}
            className="h-20 animate-pulse rounded-md border border-halcyon-border bg-halcyon-card"
          />
        ))}
      </div>
    );
  }

  if (error) {
    return (
      <div className="rounded-md border border-halcyon-danger/30 bg-halcyon-card px-4 py-6 text-sm text-halcyon-danger">
        Failed to fetch commits: <span className="font-mono">{error}</span>
        <div className="mt-2 text-xs text-halcyon-muted">
          GitHub&apos;s unauthenticated API is rate-limited to 60 requests/hour per
          IP. Try again in a few minutes, or view directly on GitHub.
        </div>
      </div>
    );
  }

  if (!commits || commits.length === 0) {
    return (
      <div className="rounded-md border border-halcyon-border bg-halcyon-card px-4 py-6 text-sm text-halcyon-muted">
        No commits returned.
      </div>
    );
  }

  // Group by date for readability.
  const groups = new Map<string, Commit[]>();
  for (const c of commits) {
    const day = c.date ? c.date.slice(0, 10) : 'unknown';
    if (!groups.has(day)) groups.set(day, []);
    groups.get(day)!.push(c);
  }

  return (
    <div>
      <div className="mb-4 rounded-md border border-halcyon-border bg-halcyon-card px-3 py-2 text-xs text-halcyon-muted">
        Fetched live from GitHub (<code className="font-mono text-halcyon-text">ilubxrp589/xrpl-validator</code>),
        cached 5&nbsp;min. Every sha links to the commit — nothing here can drift
        from the repo.
      </div>
      <div className="space-y-6">
        {Array.from(groups.entries()).map(([day, items]) => (
          <section key={day}>
            <h3 className="mb-2 font-mono text-xs uppercase tracking-wider text-halcyon-muted">
              {day}
            </h3>
            <ul className="space-y-2">
              {items.map((c) => (
                <CommitEntry key={c.sha_full} c={c} />
              ))}
            </ul>
          </section>
        ))}
      </div>
    </div>
  );
}

function CommitEntry({ c }: { c: Commit }) {
  const [expanded, setExpanded] = useState(false);
  const hasBody = c.body.length > 0;

  // Color-tag conventional-commit types for quick scanning.
  const match = c.headline.match(/^(\w+)(\([^)]+\))?:/);
  const type = match?.[1]?.toLowerCase() ?? '';
  const tagColor =
    type === 'fix'
      ? 'text-halcyon-danger'
      : type === 'feat'
      ? 'text-halcyon-accent'
      : type === 'test'
      ? 'text-yellow-400'
      : type === 'docs' || type === 'chore'
      ? 'text-halcyon-muted'
      : 'text-halcyon-text';

  return (
    <li className="rounded-md border border-halcyon-border bg-halcyon-card transition-colors hover:border-halcyon-accent/30">
      <div className="flex items-start gap-3 p-3">
        <a
          href={c.url}
          target="_blank"
          rel="noopener noreferrer"
          className="mt-0.5 shrink-0 rounded border border-halcyon-border bg-halcyon-bg px-2 py-0.5 font-mono text-[11px] text-halcyon-accent transition-colors hover:border-halcyon-accent hover:bg-halcyon-accent/10"
          aria-label="Open commit on GitHub"
        >
          {c.sha}
        </a>
        <div className="min-w-0 flex-1">
          <div className={`text-sm leading-snug ${tagColor}`}>
            <span className="font-medium">{c.headline}</span>
          </div>
          {hasBody && (
            <button
              onClick={() => setExpanded((v) => !v)}
              className="mt-1 text-[11px] uppercase tracking-wider text-halcyon-muted hover:text-halcyon-accent"
            >
              {expanded ? '− hide details' : '+ show details'}
            </button>
          )}
          {expanded && hasBody && (
            <pre className="mt-2 whitespace-pre-wrap break-words border-l-2 border-halcyon-border pl-3 font-mono text-[11px] leading-relaxed text-halcyon-muted">
              {c.body}
            </pre>
          )}
        </div>
        <a
          href={c.url}
          target="_blank"
          rel="noopener noreferrer"
          className="shrink-0 rounded p-1 text-halcyon-muted transition-colors hover:text-halcyon-accent"
          aria-label="Open on GitHub"
        >
          <ExternalLink className="h-3.5 w-3.5" />
        </a>
      </div>
    </li>
  );
}
