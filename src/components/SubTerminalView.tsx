// SubTerminalView — mirrors `TerminalView` for terminal-kind sub-sessions.
// Hosts an xterm.js Terminal in the main area for a sub-session. Lifecycle
// rules match `TerminalView`: the underlying Terminal lives in the
// `use-terminal` registry and survives hide/show; this component only
// owns DOM mount/unmount and focus on activation.
//
// Exit / error UX: when a terminal sub-session transitions into `exited`
// or `error`, we render a slim non-modal status bar at the bottom of the
// pane offering Relaunch / Close. This is **deliberately not a dialog**:
//
//   * the bar is part of the panel chrome — no backdrop, no centred
//     card, no border — so it doesn't read as a modal interruption;
//   * the xterm scrollback stays visible above it (we do *not* clear
//     on the entering-exited edge — the user wants to see the final
//     "exit" echo, error message, or whatever the shell printed last);
//   * the still-visible scrollback dims to `opacity-50` so the pane
//     reads as inert at a glance — preserved for read but obviously
//     not interactive;
//   * Relaunch / Close are inline-text buttons, not big modal buttons,
//     and live in the same row as the status text.
//
// We DO clear the scrollback on the inverse transition (exited/error →
// starting) so a relaunch starts fresh — and to defend against a backend
// race where a stray output byte from the just-killed PTY arrives after
// the new spawn begins (per rubber-duck critique).
//
// The xterm Terminal is never unmounted on status change; the registry
// instance survives so a relaunch reuses the same `cols`/`rows` measured
// at mount time, avoiding a flash-of-default-size on respawn.

import { useEffect, useRef } from 'react';

import { useSubTerminal } from '@/hooks/use-terminal';
import { useSubSessionActions, useSubSessionById } from '@/store/sub-session-store';
import type { SubSessionId, SubSessionStatus } from '@/types/arborist';

interface SubTerminalViewProps {
  subSessionId: SubSessionId;
  /**
   * Whether this view is the currently-visible pane. Hidden views still
   * keep their Terminal attached so output keeps flowing into the
   * scrollback buffer, mirroring `TerminalView`'s contract.
   */
  isActive: boolean;
}

function isExitedStatus(status: SubSessionStatus | undefined): boolean {
  return status === 'exited' || status === 'error';
}

export function SubTerminalView({ subSessionId, isActive }: SubTerminalViewProps): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sub = useSubSessionById(subSessionId);
  const { attach, detach, focus, refit, clear } = useSubTerminal(subSessionId);
  const subActions = useSubSessionActions();

  const status = sub?.status;
  const showExitedBar = isExitedStatus(status);

  // Track previous status so we can clear the scrollback on the
  // exited/error → starting edge exactly once. Starting from
  // `undefined` means the *first* render with an already-starting
  // status (rare, but possible during restore-on-launch) won't fire a
  // spurious clear. We deliberately do NOT clear on the entering-exited
  // edge — the user wants to see the shell's final output (exit echo,
  // error message, etc.).
  const prevStatusRef = useRef<SubSessionStatus | undefined>(undefined);
  useEffect(() => {
    const prev = prevStatusRef.current;
    prevStatusRef.current = status;
    if (status === undefined) return;
    if (isExitedStatus(prev) && status === 'starting') {
      clear();
    }
  }, [status, clear]);

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
  // While the exit status bar is up the underlying shell is dead, so
  // keyboard focus stays on the bar's Relaunch button instead of the
  // defunct xterm.
  useEffect(() => {
    if (!isActive) return;
    const handle = requestAnimationFrame(() => {
      refit();
      if (!showExitedBar) {
        focus();
      }
    });
    return () => cancelAnimationFrame(handle);
  }, [isActive, refit, focus, showExitedBar]);

  const handleRelaunch = (): void => {
    void subActions.relaunch(subSessionId);
  };

  const handleClose = (): void => {
    void subActions.close(subSessionId);
  };

  const exitedSummary = (() => {
    if (status === 'error') {
      return sub?.label
        ? `“${sub.label}” ended with an error.`
        : 'Sub-session ended with an error.';
    }
    return sub?.label ? `“${sub.label}” ended.` : 'Sub-session ended.';
  })();

  return (
    <div
      role="tabpanel"
      aria-label={sub ? `Terminal for ${sub.label}` : 'Sub-session terminal'}
      className="relative flex h-full w-full flex-col bg-black p-2"
    >
      <div
        ref={containerRef}
        data-testid="sub-terminal-host"
        className={`min-h-0 flex-1 bg-black transition-opacity duration-150 ${
          showExitedBar ? 'opacity-50' : ''
        }`}
      />
      {showExitedBar && (
        <div
          role="status"
          aria-live="polite"
          aria-label="Sub-session ended"
          className="mt-1 flex items-center gap-2 border-t border-slate-800 bg-black px-2 py-1 font-mono text-xs"
        >
          <span
            aria-hidden="true"
            className={status === 'error' ? 'text-red-400' : 'text-slate-500'}
          >
            ●
          </span>
          <span className="min-w-0 flex-1 truncate text-slate-400">{exitedSummary}</span>
          <button
            type="button"
            onClick={handleRelaunch}
            autoFocus
            className="rounded px-2 py-0.5 text-sky-400 transition-colors hover:bg-slate-900 hover:text-sky-300 focus:outline-none focus-visible:ring-1 focus-visible:ring-sky-500"
          >
            Relaunch
          </button>
          <button
            type="button"
            onClick={handleClose}
            className="rounded px-2 py-0.5 text-slate-400 transition-colors hover:bg-slate-900 hover:text-slate-200 focus:outline-none focus-visible:ring-1 focus-visible:ring-sky-500"
          >
            Close
          </button>
        </div>
      )}
    </div>
  );
}
