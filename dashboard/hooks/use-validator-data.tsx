'use client';

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import type {
  ConsensusData,
  EngineData,
  StateHashStatus,
  ValidatorSnapshot,
} from '@/lib/types';

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

interface ValidatorCtx {
  /** Latest combined snapshot (null until first fetch). */
  data: ValidatorSnapshot | null;
  /** True while the very first fetch is in flight. */
  loading: boolean;
  /** Connection status shown in the navbar. */
  status: 'connecting' | 'live' | 'stale' | 'error';
}

const Ctx = createContext<ValidatorCtx>({
  data: null,
  loading: true,
  status: 'connecting',
});

export function useValidatorData() {
  return useContext(Ctx);
}

// ---------------------------------------------------------------------------
// Provider — polls 3 endpoints, merges into one snapshot, throttles at 50ms
// ---------------------------------------------------------------------------

const POLL_INTERVAL = 3500; // ~1 per mainnet ledger close
const STALE_THRESHOLD = 15_000; // 15s without a fresh ledger → "stale"

/**
 * Real-time data provider. Currently uses HTTP polling against the three
 * existing validator API endpoints (/api/engine, /api/consensus,
 * /api/state-hash). Designed to be replaced with a single WebSocket
 * connection once the backend exposes one — the component API stays the
 * same, only this file changes.
 *
 * Throttle: we poll every ~3.5s (one mainnet ledger). Client-side
 * animations (number counting, sparkline interpolation) use their own
 * requestAnimationFrame / 50ms cadence against the latest snapshot.
 */
export function ValidatorProvider({ children }: { children: ReactNode }) {
  const [snapshot, setSnapshot] = useState<ValidatorSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [status, setStatus] = useState<ValidatorCtx['status']>('connecting');
  const lastSeq = useRef(0);
  const lastUpdateMs = useRef(0);

  const fetchAll = useCallback(async () => {
    try {
      const [engRes, consRes, shRes] = await Promise.all([
        fetch('/api/engine'),
        fetch('/api/consensus'),
        fetch('/api/state-hash'),
      ]);
      if (!engRes.ok || !consRes.ok || !shRes.ok) {
        setStatus('error');
        return;
      }
      const [engine, consensus, stateHash] = (await Promise.all([
        engRes.json(),
        consRes.json(),
        shRes.json(),
      ])) as [EngineData, ConsensusData, StateHashStatus];

      const now = Date.now();
      const newSeq = engine.ledger_seq || 0;

      // Only mark "live" if the ledger has actually advanced recently.
      if (newSeq > lastSeq.current) {
        lastSeq.current = newSeq;
        lastUpdateMs.current = now;
        setStatus('live');
      } else if (now - lastUpdateMs.current > STALE_THRESHOLD) {
        setStatus('stale');
      }

      setSnapshot({ engine, consensus, stateHash, updatedAt: now });
      setLoading(false);
    } catch {
      setStatus('error');
    }
  }, []);

  useEffect(() => {
    fetchAll(); // fire immediately
    const id = setInterval(fetchAll, POLL_INTERVAL);
    return () => clearInterval(id);
  }, [fetchAll]);

  return (
    <Ctx.Provider value={{ data: snapshot, loading, status }}>
      {children}
    </Ctx.Provider>
  );
}
