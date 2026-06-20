'use client';

import { useEffect, useRef, useState } from 'react';

// Advisory, read-only validator copilot. Verdict chip polls /copilot/health
// (free, deterministic — no model call). Asking a question POSTs /copilot,
// which runs Opus 4.8 via the account-auth service and answers from a live
// snapshot of the node. It can never mutate anything.

const VERDICT_COLOR: Record<string, string> = {
  HEALTHY: '#00FF9F',
  WATCH: '#f5a623',
  SYNCING: '#00f0ff',
  DEGRADED: '#ff4d4d',
  HALT_SUSPECTED: '#ff4d4d',
  UNREACHABLE: '#8a8f98',
};

type Msg = { role: 'user' | 'assistant'; content: string };

const SUGGESTIONS = [
  'Is the validator healthy?',
  'Any divergences or state-hash issues?',
  'Is it keeping up with the network edge?',
];

// Investigate mode reads the validator SOURCE (Read/Grep/Glob only) to chase root
// causes — slower, one-shot, no chat history.
const INVESTIGATE_SUGGESTIONS = [
  'Why does engine.ledger_seq freeze while round_ledger_seq advances?',
  'Where is the state-hash comparison done in the code?',
  'What triggers a halt-on-mismatch?',
];

type Mode = 'ask' | 'investigate';

export function CopilotPanel() {
  const [verdict, setVerdict] = useState<{ verdict: string; one_liner: string } | null>(null);
  const [msgs, setMsgs] = useState<Msg[]>([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [pin, setPin] = useState('');
  const [mode, setMode] = useState<Mode>('ask');
  const endRef = useRef<HTMLDivElement>(null);

  useEffect(() => { setPin(sessionStorage.getItem('copilot_pin') || ''); }, []);
  const savePin = (v: string) => { setPin(v); try { sessionStorage.setItem('copilot_pin', v); } catch { /* ignore */ } };

  useEffect(() => {
    let alive = true;
    const pull = async () => {
      try {
        const r = await fetch('/copilot/health');
        if (!r.ok) return;
        const d = await r.json();
        if (alive) setVerdict({ verdict: d.verdict, one_liner: d.one_liner });
      } catch { /* ignore */ }
    };
    pull();
    const id = setInterval(pull, 10_000);
    return () => { alive = false; clearInterval(id); };
  }, []);

  useEffect(() => { endRef.current?.scrollIntoView({ behavior: 'smooth' }); }, [msgs, busy]);

  async function ask(q: string) {
    const question = q.trim();
    if (!question || busy) return;
    setInput('');
    const history = msgs.map((m) => ({ role: m.role, content: m.content }));
    setMsgs((m) => [...m, { role: 'user', content: question }]);
    setBusy(true);
    try {
      const r = await fetch('/copilot', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-copilot-pin': pin },
        body: JSON.stringify({ question, history, pin }),
      });
      if (r.status === 401) {
        setMsgs((m) => [...m, { role: 'assistant', content: '🔒 PIN required or invalid — enter the copilot PIN (top right), then ask again.' }]);
        return;
      }
      if (r.status === 429) {
        setMsgs((m) => [...m, { role: 'assistant', content: '⏳ Rate limit reached — give it a moment.' }]);
        return;
      }
      const d = await r.json();
      setMsgs((m) => [...m, { role: 'assistant', content: d.reply || d.error || '(no reply)' }]);
    } catch (e) {
      setMsgs((m) => [...m, { role: 'assistant', content: `request failed: ${String(e)}` }]);
    } finally {
      setBusy(false);
    }
  }

  async function investigate(q: string) {
    const question = q.trim();
    if (!question || busy) return;
    setInput('');
    setMsgs((m) => [...m, { role: 'user', content: question }]);
    setBusy(true);
    try {
      const r = await fetch('/copilot/investigate', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-copilot-pin': pin },
        body: JSON.stringify({ question, pin }),
      });
      if (r.status === 401) {
        setMsgs((m) => [...m, { role: 'assistant', content: '🔒 PIN required or invalid — enter the copilot PIN (top right), then try again.' }]);
        return;
      }
      if (r.status === 429) { setMsgs((m) => [...m, { role: 'assistant', content: '⏳ Rate limit reached — give it a moment.' }]); return; }
      if (r.status === 503) { setMsgs((m) => [...m, { role: 'assistant', content: 'Investigate mode is not configured on the copilot service (no codeRoot).' }]); return; }
      const d = await r.json();
      setMsgs((m) => [...m, { role: 'assistant', content: d.reply || d.error || '(no reply)' }]);
    } catch (e) {
      setMsgs((m) => [...m, { role: 'assistant', content: `request failed: ${String(e)}` }]);
    } finally {
      setBusy(false);
    }
  }

  const submit = (q: string) => (mode === 'investigate' ? investigate(q) : ask(q));

  const vColor = verdict ? VERDICT_COLOR[verdict.verdict] || VERDICT_COLOR.UNREACHABLE : VERDICT_COLOR.UNREACHABLE;

  return (
    <div>
      <p className="text-[10px] font-mono uppercase tracking-widest text-halcyon-muted mb-2">
        Validator Copilot · advisory · read-only
      </p>
      <div className="bg-halcyon-card border border-halcyon-border rounded-lg p-4 flex flex-col h-[360px] card-hover">
        <div className="flex items-center justify-between mb-3 gap-2">
          <span className="text-[11px] font-mono uppercase tracking-wider text-halcyon-muted">Opus 4.8</span>
          <div className="flex items-center gap-2">
            <input
              type="password"
              value={pin}
              onChange={(e) => savePin(e.target.value)}
              placeholder="PIN"
              aria-label="Copilot PIN"
              className="w-16 rounded border border-halcyon-border bg-halcyon-bg/60 px-2 py-0.5 text-[11px] font-mono text-halcyon-text outline-none focus:border-halcyon-accent/50"
            />
            {verdict && (
              <span
                title={verdict.one_liner}
                className="text-[10px] font-mono font-bold uppercase tracking-wider px-2 py-0.5 rounded-full border"
                style={{ color: vColor, borderColor: vColor + '55', backgroundColor: vColor + '15' }}
              >
                ● {verdict.verdict}
              </span>
            )}
          </div>
        </div>

        <div className="flex-1 overflow-y-auto space-y-2.5 pr-1 text-[13px] font-mono leading-relaxed">
          {msgs.length === 0 && !busy && (
            <div className="text-halcyon-muted space-y-2">
              <p>{mode === 'investigate'
                ? 'Investigate root causes by reading the validator source (read-only). Slower, one-shot.'
                : 'Ask about health, divergences, sync, lag, or recovery.'}</p>
              <div className="flex flex-col gap-1.5">
                {(mode === 'investigate' ? INVESTIGATE_SUGGESTIONS : SUGGESTIONS).map((s) => (
                  <button
                    key={s}
                    onClick={() => submit(s)}
                    className="text-left text-[12px] text-halcyon-accent/80 hover:text-halcyon-accent border border-halcyon-border rounded px-2 py-1 card-hover"
                  >
                    {s}
                  </button>
                ))}
              </div>
            </div>
          )}
          {msgs.map((m, i) => (
            <div key={i} className={m.role === 'user' ? 'text-right' : 'text-left'}>
              <span
                className={`inline-block whitespace-pre-wrap rounded-lg px-3 py-2 max-w-[92%] text-left ${
                  m.role === 'user'
                    ? 'bg-halcyon-border/40 text-halcyon-text'
                    : 'bg-halcyon-accent/10 text-halcyon-text'
                }`}
              >
                {m.content}
              </span>
            </div>
          ))}
          {busy && <p className="text-halcyon-muted animate-pulse">{mode === 'investigate' ? 'copilot is reading the source… (can take a minute)' : 'copilot is reading the node…'}</p>}
          <div ref={endRef} />
        </div>

        <div className="mt-3 flex items-center gap-1 text-[10px] font-mono uppercase tracking-wider">
          {(['ask', 'investigate'] as Mode[]).map((m) => (
            <button
              key={m}
              onClick={() => setMode(m)}
              disabled={busy}
              className={`rounded px-2 py-0.5 border disabled:opacity-40 ${
                mode === m
                  ? 'border-halcyon-accent/50 bg-halcyon-accent/15 text-halcyon-accent'
                  : 'border-halcyon-border text-halcyon-muted hover:text-halcyon-text'
              }`}
            >
              {m}
            </button>
          ))}
          <span className="text-halcyon-muted/70 normal-case tracking-normal ml-1">
            {mode === 'investigate' ? 'reads source · read-only' : 'reads live node'}
          </span>
        </div>

        <div className="mt-2 flex gap-2">
          <input
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && submit(input)}
            placeholder={mode === 'investigate' ? 'investigate the source…' : 'ask the copilot…'}
            disabled={busy}
            className="flex-1 rounded-lg border border-halcyon-border bg-halcyon-bg/60 px-3 py-2 text-[13px] font-mono text-halcyon-text outline-none focus:border-halcyon-accent/50 disabled:opacity-50"
          />
          <button
            onClick={() => submit(input)}
            disabled={busy}
            className="rounded-lg border border-halcyon-accent/40 bg-halcyon-accent/15 px-4 py-2 text-[12px] font-mono font-bold uppercase tracking-wider text-halcyon-accent hover:bg-halcyon-accent/25 disabled:opacity-40"
          >
            {mode === 'investigate' ? 'Dig' : 'Ask'}
          </button>
        </div>
      </div>
    </div>
  );
}
