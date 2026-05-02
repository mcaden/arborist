// SubTerminalView — mirrors `TerminalView` for terminal-kind sub-sessions.
// Hosts an xterm.js Terminal in the main area for a sub-session. Lifecycle
// rules match `TerminalView`: the underlying Terminal lives in the
// `use-terminal` registry and survives hide/show; this component only
// owns DOM mount/unmount and focus on activation.
//
// Exit / error UX: when a terminal sub-session transitions into `exited`
// or `error` we **clear** the xterm scrollback and overlay a "Relaunch /
// Close" pane on top of the (still-mounted) terminal host. This:
//
//   * removes any final stale prompt / "exit" echo so the user isn't
//     looking at a frozen-but-live-looking shell;
//   * gives an in-pane affordance to restart the same id (preserves
//     position in sidebar, parent-session binding, label) without
//     requiring a sidebar round-trip;
//   * does **not** unmount the xterm Terminal — the registry instance
//     survives so a relaunch reuses the same `cols`/`rows` measured at
//     mount time, avoiding a flash-of-default-size on respawn.
//
// We also clear once more on the inverse transition (exited/error →
// starting) to defend against a backend race where a stray output byte
// from the just-killed PTY arrives after the new spawn begins (per
// rubber-duck critique). Only one of the two clears typically observes
// any content; the second is cheap insurance.

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
  const showRelaunchOverlay = isExitedStatus(status);

  // Track previous status so we can detect transitions and clear the
  // scrollback exactly once per edge (avoid clearing every render). The
  // ref starts undefined so a first-mount with status already in
  // exited/error counts as the entering edge — handles
  // restore-on-launch where the registry's terminal may have leftover
  // scrollback from before a window reload.
  const prevStatusRef = useRef<SubSessionStatus | undefined>(undefined);
  useEffect(() => {
    const prev = prevStatusRef.current;
    prevStatusRef.current = status;
    if (status === undefined) return;
    const enteringExited = !isExitedStatus(prev) && isExitedStatus(status);
    const leavingExitedToStarting = isExitedStatus(prev) && status === 'starting';
    if (enteringExited || leavingExitedToStarting) {
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
  // Skip focusing when the relaunch overlay is up so the keyboard lands
  // on the dialog buttons, not a defunct xterm.
  useEffect(() => {
    if (!isActive) return;
    const handle = requestAnimationFrame(() => {
      refit();
      if (!showRelaunchOverlay) {
        focus();
      }
    });
    return () => cancelAnimationFrame(handle);
  }, [isActive, refit, focus, showRelaunchOverlay]);

  const handleRelaunch = (): void => {
    void subActions.relaunch(subSessionId);
  };

  const handleClose = (): void => {
    void subActions.close(subSessionId);
  };

  return (
    <div
      role="tabpanel"
      aria-label={sub ? `Terminal for ${sub.label}` : 'Sub-session terminal'}
      className="relative h-full w-full bg-black p-2"
    >
      <div ref={containerRef} className="h-full w-full bg-black" />
      {showRelaunchOverlay && (
        <div
          role="dialog"
          aria-label="Sub-session ended"
          className="absolute inset-0 flex items-center justify-center bg-black/80 p-4"
        >
          <div className="max-w-md rounded-md border border-slate-700 bg-slate-900 p-5 text-slate-100 shadow-lg">
            <h3 className="mb-2 text-base font-semibold">
              {status === 'error' ? 'Sub-session ended with an error' : 'Sub-session ended'}
            </h3>
            <p className="mb-4 text-sm text-slate-300">
              {sub?.label
                ? `“${sub.label}” is no longer running.`
                : 'This sub-session is no longer running.'}{' '}
              Relaunch to start it again, or close to remove this tab.
            </p>
            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={handleClose}
                className="rounded-md border border-slate-600 bg-slate-800 px-3 py-1.5 text-sm text-slate-100 hover:bg-slate-700 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500"
              >
                Close
              </button>
              <button
                type="button"
                onClick={handleRelaunch}
                autoFocus
                className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-400"
              >
                Relaunch
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
