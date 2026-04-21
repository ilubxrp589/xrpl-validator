// /logs — Progress journal + live changelog.
//
// Two tabs:
//   1. PROGRESS — hand-written narrative from PROGRESS.md at the repo root.
//   2. CHANGELOG — live commits fetched from GitHub via /api/commits.
//
// PROGRESS is clearly labeled as narrative (last-updated date shown). CHANGELOG
// is rendered from git history via GitHub's API — there's no middleman to drift.

import fs from 'fs';
import path from 'path';
import { Navbar } from '@/components/navbar';
import { Footer } from '@/components/footer';
import { LogsClient } from './LogsClient';

// Always render fresh on server so PROGRESS.md edits show up without a rebuild.
export const dynamic = 'force-dynamic';

function readProgressMd(): { content: string; mtime: string } {
  // Dashboard is at repo-root/dashboard/; PROGRESS.md is one level up.
  const candidates = [
    path.join(process.cwd(), '..', 'PROGRESS.md'),
    path.join(process.cwd(), 'PROGRESS.md'),
  ];
  for (const p of candidates) {
    try {
      const stat = fs.statSync(p);
      const content = fs.readFileSync(p, 'utf8');
      const mtime = stat.mtime.toISOString().slice(0, 10);
      return { content, mtime };
    } catch {
      // try next candidate
    }
  }
  return {
    content:
      '_PROGRESS.md not found. Expected at repo root (one level above `dashboard/`)._',
    mtime: '',
  };
}

export default function LogsPage() {
  const { content, mtime } = readProgressMd();

  return (
    <div className="flex min-h-screen flex-col bg-halcyon-bg text-halcyon-text">
      <Navbar />
      <main className="mx-auto w-full max-w-5xl flex-1 px-4 py-8">
        <header className="mb-8 border-b border-halcyon-border pb-6">
          <h1 className="font-headline text-3xl font-bold tracking-tight text-halcyon-accent">
            Logs
          </h1>
          <p className="mt-2 max-w-2xl text-sm text-halcyon-muted">
            Progress journal (hand-curated narrative) and changelog (live from
            GitHub commits). No middleman — claims anchor to commit hashes,
            commits go straight to the repo.
          </p>
        </header>
        <LogsClient progressMd={content} progressMtime={mtime} />
      </main>
      <Footer />
    </div>
  );
}
