// Sidebar debug button that helps diagnose terminal-fit issues. On click:
//
//   1. Captures a `before` snapshot of every attached terminal — host /
//      wrapper / .xterm-screen rects, term cols/rows, last cols/rows
//      reported to the backend, computed cell size, host visibility and a
//      few ancestors, plus environment context (DPR, fonts state,
//      visibility, focus).
//   2. Forces `fit()` + `refresh()` on every attached terminal via
//      `forceRefitAllTerminals()` so the user can keep working without
//      having to reload.
//   3. Captures an `after` snapshot.
//   4. Copies `{ before, after, sessions }` as pretty-printed JSON to the
//      clipboard so it can be pasted into a debug session.
//
// Visual feedback is a transient state on the button label ("Copied ✓" or
// "Copy failed"). No toast system in v1.

import { useCallback, useEffect, useRef, useState } from 'react';

import {
  captureTerminalDebugSnapshot,
  forceRefitAllTerminals,
  type TerminalDebugSnapshot,
} from '@/hooks/use-terminal';
import { useActiveSessionId, useSessions } from '@/store/session-store';

type ButtonState = 'idle' | 'copied' | 'error';

interface DebugBundle {
  before: TerminalDebugSnapshot;
  after: TerminalDebugSnapshot;
  sessions: Array<{
    id: string;
    label: string;
    status: string;
    tabIndex: number;
    isActive: boolean;
  }>;
  userAgent: string | null;
}

async function writeToClipboard(text: string): Promise<void> {
  // Modern clipboard API first; fall back to a transient textarea selection
  // for environments where navigator.clipboard isn't available (older
  // WebView2 builds, jsdom).
  if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }
  if (typeof document === 'undefined') {
    throw new Error('clipboard unavailable: no document');
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  try {
    ta.select();
    const ok = document.execCommand?.('copy');
    if (!ok) throw new Error('execCommand("copy") returned false');
  } finally {
    ta.remove();
  }
}

export function FitDebugButton(): JSX.Element {
  const sessions = useSessions();
  const activeId = useActiveSessionId();
  const [state, setState] = useState<ButtonState>('idle');
  const resetTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mountedRef = useRef<boolean>(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (resetTimer.current !== null) {
        clearTimeout(resetTimer.current);
        resetTimer.current = null;
      }
    };
  }, []);

  // Guarded against a click → clipboard-resolve → component-unmount race:
  // the clipboard promise is async, and if the user dismisses the sidebar
  // (or the app navigates) before it settles, `setState` would fire on an
  // unmounted component AND schedule a leaked timer.
  const flash = useCallback((next: 'copied' | 'error') => {
    if (!mountedRef.current) return;
    setState(next);
    if (resetTimer.current !== null) clearTimeout(resetTimer.current);
    resetTimer.current = setTimeout(() => {
      resetTimer.current = null;
      if (mountedRef.current) setState('idle');
    }, 2000);
  }, []);

  const onClick = useCallback(() => {
    const before = captureTerminalDebugSnapshot();
    try {
      forceRefitAllTerminals();
    } catch (err) {
      // Swallow — refit is best-effort. Still capture the after-snapshot so
      // we know fit() threw.
      console.warn('[FitDebugButton] forceRefitAllTerminals threw:', err);
    }
    const after = captureTerminalDebugSnapshot();
    const bundle: DebugBundle = {
      before,
      after,
      sessions: sessions.map((s) => ({
        id: s.id,
        label: s.label,
        status: s.status,
        tabIndex: s.tabIndex,
        isActive: s.id === activeId,
      })),
      userAgent: typeof navigator !== 'undefined' ? navigator.userAgent : null,
    };
    void writeToClipboard(JSON.stringify(bundle, null, 2))
      .then(() => flash('copied'))
      .catch((err: unknown) => {
        console.warn('[FitDebugButton] clipboard write failed:', err);
        flash('error');
      });
  }, [sessions, activeId, flash]);

  const label = state === 'copied' ? 'Copied ✓' : state === 'error' ? 'Copy failed' : 'Fit';
  const title =
    'Force-fit every terminal and copy a debug snapshot of layout state to the clipboard.';

  // Accessibility: the visible `<span>` text doubles as the button's
  // accessible name (no `aria-label`, which would mask it). The label
  // span is `aria-live="polite"` + `aria-atomic="true"` so screen
  // readers announce the transient "Copied ✓" / "Copy failed" status
  // without spamming on every paint. `title` provides the long
  // hover/tooltip description without affecting the SR name.
  return (
    <button
      type="button"
      data-testid="fit-debug-button"
      title={title}
      onClick={onClick}
      className="flex items-center gap-1 rounded px-2 py-1.5 text-xs text-slate-600 hover:bg-slate-200 dark:text-slate-300 dark:hover:bg-slate-800"
    >
      <span aria-hidden="true">🔧</span>
      <span aria-live="polite" aria-atomic="true">
        {label}
      </span>
    </button>
  );
}
