// SubTerminalView — mirrors `TerminalView` for terminal-kind sub-sessions.
// Hosts an xterm.js Terminal in the main area for a sub-session. Lifecycle
// rules match `TerminalView`: the underlying Terminal lives in the
// `use-terminal` registry and survives hide/show; this component only
// owns DOM mount/unmount, focus on activation, and the exited overlay.
//
// Differences vs `TerminalView`:
//   * Reads from `useSubSessionById` / `useSubStatusMessage`.
//   * Uses `useSubTerminal` so input/resize commands target the
//     `subsession_*` IPC handlers instead of the parent-session ones.
//   * No Restart button — terminal sub-sessions are single-shot in v1
//     (CONTEXT_MENU_PLAN.md, MainArea section). The overlay offers Close
//     instead, which removes the sub-tab and the registry entry.

import { useEffect, useRef } from 'react';

import { useSubTerminal } from '@/hooks/use-terminal';
import {
  useSubSessionActions,
  useSubSessionById,
  useSubStatusMessage,
} from '@/store/sub-session-store';
import type { SubSessionId } from '@/types/arborist';

interface SubTerminalViewProps {
  subSessionId: SubSessionId;
  /**
   * Whether this view is the currently-visible pane. Hidden views still
   * keep their Terminal attached so output keeps flowing into the
   * scrollback buffer, mirroring `TerminalView`'s contract.
   */
  isActive: boolean;
}

export function SubTerminalView({ subSessionId, isActive }: SubTerminalViewProps): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sub = useSubSessionById(subSessionId);
  const statusMessage = useSubStatusMessage(subSessionId);
  const subActions = useSubSessionActions();
  const { attach, detach, focus, refit } = useSubTerminal(subSessionId);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    attach(el);
    return () => {
      detach();
    };
  }, [attach, detach]);

  // Same rationale as TerminalView: visibility:hidden panels don't fire
  // ResizeObserver, so re-measure + steal focus on the activation edge.
  useEffect(() => {
    if (!isActive) return;
    const handle = requestAnimationFrame(() => {
      refit();
      focus();
    });
    return () => cancelAnimationFrame(handle);
  }, [isActive, refit, focus]);

  const status = sub?.status;
  const showOverlay = status === 'error' || status === 'exited';

  const handleClose = (): void => {
    void subActions.close(subSessionId).catch((err: unknown) => {
      const message = err instanceof Error ? err.message : String(err);
      console.warn(`[SubTerminalView] close(${subSessionId}) failed: ${message}`);
    });
  };

  return (
    <div
      role="tabpanel"
      aria-label={sub ? `Terminal for ${sub.label}` : 'Sub-session terminal'}
      className="relative h-full w-full bg-black p-2"
    >
      <div ref={containerRef} className="h-full w-full bg-black" />
      {showOverlay && (
        <div
          role="alert"
          className="pointer-events-none absolute inset-0 flex items-center justify-center bg-black/70"
        >
          <div className="pointer-events-auto flex max-w-md flex-col items-center gap-3 rounded border border-slate-700 bg-slate-900 p-4 text-slate-100 shadow-lg">
            <p className="text-sm">
              {status === 'error' ? 'Sub-session encountered an error.' : 'Sub-session exited.'}
            </p>
            {statusMessage && (
              <p
                data-testid="sub-terminal-status-message"
                className="max-w-full break-words text-center text-xs text-slate-300"
              >
                {statusMessage}
              </p>
            )}
            <button
              type="button"
              onClick={handleClose}
              className="rounded bg-slate-700 px-3 py-1 text-sm font-medium text-white hover:bg-slate-600 focus:outline-none focus:ring-2 focus:ring-slate-400"
            >
              Close
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
