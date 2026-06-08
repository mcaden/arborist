// TerminalView — the per-session view that hosts an xterm.js Terminal in
// the main area. The Terminal instance itself is managed by `use-terminal`;
// this component is responsible for:
//   * Mounting / unmounting the Terminal into the DOM.
//   * Auto-focusing the Terminal when this view becomes visible.
//   * Rendering an error / exited overlay with a Restart button (see docs/product.md).

import { useEffect, useRef } from 'react';

import { ensureShellCommandTrusted } from '@/lib/shell-command-trust';
import { formatError, sessionRestart } from '@/lib/tauri-bridge';
import { measureInitialPtyDimensions, useTerminal } from '@/hooks/use-terminal';
import { useSessionById, useSessionActions, useStatusMessage } from '@/store/session-store';
import { selectIsSwitching, useWorkspaceSwitchUiStore } from '@/store/workspace-switch-ui-store';
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
  const statusMessage = useStatusMessage(sessionId);
  const sessionActions = useSessionActions();
  const { attach, detach, focus, refit, getDimensions } = useTerminal(sessionId);
  const isSwitching = useWorkspaceSwitchUiStore(selectIsSwitching);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    attach(el);
    return () => {
      detach();
    };
  }, [attach, detach]);

  // When this view becomes the active tab, the host's CSS box doesn't
  // change size (we hide inactive panels with `visibility: hidden`, which
  // preserves layout) — so ResizeObserver wouldn't fire. Re-measure and
  // repaint the renderer here to recover from any stale state, then steal
  // focus. rAF ensures the visibility:visible style has been applied
  // before we measure / focus the textarea (visibility:hidden elements
  // are unfocusable). The cleanup cancels the frame so a rapid tab
  // switch can't focus a now-inactive terminal.
  //
  // Skip focus while a workspace switch is in flight: the overlay
  // covers the terminal, and stealing focus into a now-`inert` subtree
  // both fights the overlay (which should hold focus for a11y) and
  // can race with the imminent terminal teardown when the new
  // workspace's session list lands. `refit()` still runs inside the
  // same rAF for consistent measurement timing — we only suppress the
  // `focus()` call, not the renderer recovery.
  useEffect(() => {
    if (!isActive) return;
    const handle = requestAnimationFrame(() => {
      refit();
      if (!isSwitching) {
        focus();
      }
    });
    return () => cancelAnimationFrame(handle);
  }, [isActive, isSwitching, refit, focus]);

  const status = session?.status;
  const showOverlay = status === 'error' || status === 'exited';

  const handleRestart = (): void => {
    const dims = getDimensions() ?? measureInitialPtyDimensions();
    sessionActions.prepareForRestart(sessionId);
    (async () => {
      const trusted = await ensureShellCommandTrusted({ kind: 'sessionRestart', sessionId });
      if (!trusted) return;
      await sessionRestart({ sessionId, cols: dims.cols, rows: dims.rows });
    })().catch((err: unknown) => {
      const message = formatError(err);
      console.warn(`[TerminalView] session_restart(${sessionId}) failed: ${message}`);
    });
  };

  return (
    <div role="tabpanel" aria-label={session ? `Terminal for ${session.label}` : 'Terminal'} className="relative h-full w-full bg-black p-2">
      <div ref={containerRef} className="h-full w-full bg-black" />
      {showOverlay && (
        <div role="alert" className="pointer-events-none absolute inset-0 flex items-center justify-center bg-black/70">
          <div className="pointer-events-auto flex max-w-md flex-col items-center gap-3 rounded border border-slate-700 bg-slate-900 p-4 text-slate-100 shadow-lg">
            <p className="text-sm">{status === 'error' ? 'Session encountered an error.' : 'Session exited.'}</p>
            {statusMessage && (
              <p data-testid="terminal-status-message" className="max-w-full break-words text-center text-xs text-slate-300">
                {statusMessage}
              </p>
            )}
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
