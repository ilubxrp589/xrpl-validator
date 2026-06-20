// /copilot/investigate — Route Handler (NOT a Next rewrite).
//
// Investigate mode runs the copilot over the validator source for ~1-2 minutes.
// Next's rewrite proxy resets that long upstream socket (ECONNRESET), so we proxy
// it here in a handler instead, which holds the connection until the service replies.
// Read-only and PIN-gated on the copilot service itself; we just forward the body.

import { NextRequest, NextResponse } from 'next/server';

export const dynamic = 'force-dynamic';
export const maxDuration = 300; // allow the long investigate run

const COPILOT = process.env.COPILOT_API || 'http://localhost:3780';

export async function POST(req: NextRequest) {
  const body = await req.text();
  try {
    const r = await fetch(`${COPILOT}/investigate`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body,
    });
    return new NextResponse(await r.text(), { status: r.status, headers: { 'content-type': 'application/json' } });
  } catch (e) {
    return NextResponse.json({ error: `investigate proxy failed: ${String((e as Error)?.message || e)}` }, { status: 502 });
  }
}
