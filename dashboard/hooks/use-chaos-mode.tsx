'use client';

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from 'react';

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

interface ChaosCtxValue {
  active: boolean;
  toggle: () => void;
}

const ChaosCtx = createContext<ChaosCtxValue>({
  active: false,
  toggle: () => {},
});

export function useChaosMode() {
  return useContext(ChaosCtx);
}

// ---------------------------------------------------------------------------
// Overlay — renders only when chaos is active
// ---------------------------------------------------------------------------

const CHAOS_STYLES = `
@keyframes chaos-bg {
  0%, 100% { background: transparent; }
  50% { background: rgba(34, 0, 0, 0.3); }
}

@keyframes glitch {
  0% { transform: translate(0); }
  20% { transform: translate(-2px, 1px); }
  40% { transform: translate(2px, -1px); }
  60% { transform: translate(-1px, 2px); }
  80% { transform: translate(1px, -2px); }
  100% { transform: translate(0); }
}

@keyframes chaos-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.6; }
}

.chaos-active .text-halcyon-accent {
  color: #ff0044 !important;
}

.chaos-active .bg-halcyon-accent {
  background-color: #ff0044 !important;
}

.chaos-active * {
  animation: glitch 0.3s infinite;
}

.chaos-overlay-bg {
  animation: chaos-bg 2s ease-in-out infinite;
}

.chaos-banner-text {
  animation: chaos-pulse 1s ease-in-out infinite;
}
`;

export function ChaosOverlay({ toggle }: { toggle: () => void }) {
  return (
    <>
      <style dangerouslySetInnerHTML={{ __html: CHAOS_STYLES }} />

      {/* Full-screen overlay — pointer-events-none so dashboard stays usable */}
      <div
        className="chaos-overlay-bg fixed inset-0 z-[9998] pointer-events-none"
        style={{
          backgroundImage:
            'repeating-linear-gradient(0deg, rgba(255,0,0,0.03) 0px, rgba(255,0,0,0.03) 1px, transparent 1px, transparent 3px)',
        }}
      />

      {/* Top banner */}
      <div className="fixed top-0 left-0 right-0 z-[9999] flex items-center justify-between px-4 py-2 pointer-events-none"
        style={{ backgroundColor: 'rgba(34, 0, 0, 0.9)', borderBottom: '1px solid #FF3366' }}
      >
        <span className="chaos-banner-text font-mono text-sm font-bold uppercase tracking-wider"
          style={{ color: '#FF3366' }}
        >
          CHAOS MODE ENABLED &mdash; SYSTEM UNSTABLE
        </span>
        <button
          onClick={toggle}
          className="pointer-events-auto rounded border px-3 py-1 font-mono text-xs font-bold uppercase tracking-wider transition-colors hover:bg-red-900/50"
          style={{ color: '#FF3366', borderColor: '#FF3366' }}
        >
          EXIT CHAOS
        </button>
      </div>
    </>
  );
}

// ---------------------------------------------------------------------------
// Konami code sequence
// ---------------------------------------------------------------------------

const KONAMI = [
  'ArrowUp', 'ArrowUp',
  'ArrowDown', 'ArrowDown',
  'ArrowLeft', 'ArrowRight',
  'ArrowLeft', 'ArrowRight',
  'KeyB', 'KeyA',
];

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export function ChaosProvider({ children }: { children: ReactNode }) {
  const [active, setActive] = useState(false);
  const toggle = useCallback(() => setActive((prev) => !prev), []);

  // ------ Konami code listener ------
  useEffect(() => {
    let pos = 0;

    function onKey(e: KeyboardEvent) {
      const code = e.code;
      if (code === KONAMI[pos]) {
        pos++;
        if (pos === KONAMI.length) {
          setActive((prev) => !prev);
          pos = 0;
        }
      } else {
        // Reset but check if the failed key is the start of a new sequence
        pos = code === KONAMI[0] ? 1 : 0;
      }
    }

    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // ------ Triple-click on [data-chaos-trigger] elements ------
  useEffect(() => {
    let clickCount = 0;
    let clickTimer: ReturnType<typeof setTimeout> | null = null;

    function onClick(e: MouseEvent) {
      const target = e.target as HTMLElement | null;
      if (!target?.closest('[data-chaos-trigger]')) return;

      clickCount++;

      if (clickTimer) clearTimeout(clickTimer);
      clickTimer = setTimeout(() => {
        clickCount = 0;
      }, 500);

      if (clickCount >= 3) {
        clickCount = 0;
        if (clickTimer) clearTimeout(clickTimer);
        setActive((prev) => !prev);
      }
    }

    window.addEventListener('click', onClick);
    return () => {
      window.removeEventListener('click', onClick);
      if (clickTimer) clearTimeout(clickTimer);
    };
  }, []);

  return (
    <ChaosCtx.Provider value={{ active, toggle }}>
      {active && <ChaosOverlay toggle={toggle} />}
      <div className={active ? 'chaos-active' : ''}>{children}</div>
    </ChaosCtx.Provider>
  );
}
