// TerminalView — the per-session view that hosts an xterm.js Terminal in
// the main area. The Terminal instance itself is managed by `use-terminal`;
// this component is responsible for:
//   * Mounting / unmounting the Terminal into the DOM.
//   * Auto-focusing the Terminal when this view becomes visible.
//   * Rendering an error / exited overlay with a Restart button (SPEC C-04
//     / L-03).

import { useEffect, useRef } from 'react';

import { sessionRestart } from '@/lib/tauri-bridge';
import { useTerminal } from '@/hooks/use-terminal';
import { useSessionById } from '@/store/session-store';
import type { SessionId } from '@/types/arborist';

interface TerminalViewProps {
  sessionId: SessionId;
  /**
   * Whether this view is the currently-visible session. Hidden views still
   * keep their Terminal attached (so output keeps flowing into the
   * scrollback buffer) but are not focused and skip the visible-state
   * effects.
   */
  isActive: boolean;
}

export function TerminalView({ sessionId, isActive }: TerminalViewProps): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const session = useSessionById(sessionId);
  const { attach, detach, focus } = useTerminal(sessionId);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    attach(el);
    return () => {
      detach();
    };
  }, [attach, detach]);

  // Steal focus to the terminal whenever this view becomes the active tab.
  useEffect(() => {
    if (isActive) focus();
  }, [isActive, focus]);

  const status = session?.status;
  const showOverlay = status === 'error' || status === 'exited';

  const handleRestart = (): void => {
    void sessionRestart({ sessionId }).catch((err: unknown) => {
      const message = err instanceof Error ? err.message : String(err);
      console.warn(`[TerminalView] session_restart(${sessionId}) failed: ${message}`);
    });
  };

  return (
    <div
      role="tabpanel"
      aria-label={session ? `Terminal for ${session.label}` : 'Terminal'}
      className="relative h-full w-full"
    >
      <div ref={containerRef} className="h-full w-full bg-black" />
      {showOverlay && (
        <div
          role="alert"
          className="pointer-events-none absolute inset-0 flex items-center justify-center bg-black/70"
        >
          <div className="pointer-events-auto flex flex-col items-center gap-3 rounded border border-slate-700 bg-slate-900 p-4 text-slate-100 shadow-lg">
            <p className="text-sm">
              {status === 'error' ? 'Session encountered an error.' : 'Session exited.'}
            </p>
            <button
              type="button"
              onClick={handleRestart}
              className="rounded bg-blue-600 px-3 py-1 text-sm font-medium text-white hover:bg-blue-500 focus:outline-none focus:ring-2 focus:ring-blue-400"
            >
              Restart
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
