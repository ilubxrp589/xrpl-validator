// /api/commits — live fetch from GitHub's REST API, cached 5 minutes.
// No auth, so rate-limited to 60/hour per IP — the 5-min cache keeps us
// well under that for any realistic page-view volume.

import { NextResponse } from 'next/server';

const REPO = 'ilubxrp589/xrpl-validator';

export const revalidate = 300; // 5 minutes

type GithubCommit = {
  sha: string;
  commit: {
    author: { name: string; date: string } | null;
    message: string;
  };
  html_url: string;
};

export async function GET() {
  try {
    const res = await fetch(
      `https://api.github.com/repos/${REPO}/commits?per_page=60`,
      {
        headers: {
          Accept: 'application/vnd.github+json',
          'User-Agent': 'halcyon-dashboard',
        },
        next: { revalidate: 300 },
      }
    );

    if (!res.ok) {
      const body = await res.text().catch(() => '');
      return NextResponse.json(
        {
          error: `GitHub returned ${res.status}`,
          detail: body.slice(0, 200),
        },
        { status: 502 }
      );
    }

    const raw = (await res.json()) as GithubCommit[];
    const commits = raw.map((c) => {
      const msg = c.commit.message;
      const firstLine = msg.split('\n', 1)[0];
      const rest = msg.slice(firstLine.length).trim();
      return {
        sha: c.sha.slice(0, 7),
        sha_full: c.sha,
        date: c.commit.author?.date ?? null,
        headline: firstLine,
        body: rest,
        url: c.html_url,
      };
    });

    return NextResponse.json(
      { commits },
      {
        headers: {
          // Cache at CDN/proxy for 5 min, allow stale-while-revalidate.
          'Cache-Control': 's-maxage=300, stale-while-revalidate=600',
        },
      }
    );
  } catch (err) {
    return NextResponse.json(
      { error: 'fetch failed', detail: String(err).slice(0, 200) },
      { status: 502 }
    );
  }
}
