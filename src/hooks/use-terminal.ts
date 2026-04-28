// Per-session xterm.js registry + React hook (Phase 11).
//
// Design notes
// ------------
// * One `Terminal` instance per `sessionId` lives in a **module-scoped Map**
//   (`registry`) so it survives any React component unmount — including the
//   tab-switch case (SPEC T-03). The hook returned by `useTerminal()` only
//   ever attaches / detaches a terminal from the DOM; it never disposes.
// * Disposal is tied to *session lifetime*, not component lifetime. A single
//   subscription to the session store fires `disposeTerminal()` when an id
//   disappears from `sessions`.
// * Output bypasses Zustand entirely (see DESIGN §5.2): a single
//   `onSessionOutput` listener is attached at boot via the explicit
//   `initTerminalRouter()` (called from `App.tsx`) and demuxes by
//   `sessionId` to the relevant `Terminal`. The lazy fallback inside
//   `useTerminal` remains for safety so any code path that creates a
//   terminal without going through boot still sees output.
// * `attach` is idempotent: a second call with the same host element is a
//   no-op; a call with a different host implicitly re-parents.
// * `ResizeObserver` is debounced ~50 ms, then drives `fitAddon.fit()` →
//   `session_resize`. The observer is owned per-attachment (i.e. lives until
//   `detach()`); the terminal itself outlives it.

import { FitAddon } from '@xterm/addon-fit';
import { Terminal } from '@xterm/xterm';
import { useCallback, useEffect, useMemo, useRef } from 'react';

import { onSessionOutput, sessionInput, sessionResize } from '@/lib/tauri-bridge';
import { useSessionStore } from '@/store/session-store';
import type { SessionId } from '@/types/arborist';

/** Time the ResizeObserver waits before firing `fit()` + `sessionResize`. */
const RESIZE_DEBOUNCE_MS = 50;

interface RegistryEntry {
  term: Terminal;
  fitAddon: FitAddon;
  /** Wrapper element actually mounted into the host. */
  wrapper: HTMLDivElement | null;
  /** Host element passed to `attach`. */
  host: HTMLDivElement | null;
  observer: ResizeObserver | null;
  resizeTimer: ReturnType<typeof setTimeout> | null;
  /** Last cols/rows reported to the backend; suppresses duplicate calls. */
  lastCols: number;
  lastRows: number;
}

const registry = new Map<SessionId, RegistryEntry>();

let outputUnlisten: Promise<() => void> | null = null;
let storeUnsubscribe: (() => void) | null = null;

function ensureGlobalSubscriptions(): void {
  if (outputUnlisten === null) {
    outputUnlisten = onSessionOutput((payload) => {
      const entry = registry.get(payload.sessionId);
      if (!entry) {
        // Race: output for a session not (yet) tracked. Tolerable — the
        // session-create handshake is synchronous so the only way to land
        // here is for output to arrive after the terminal has been
        // disposed, which by definition no one cares about.
        if (typeof console !== 'undefined') {
          console.debug(`[use-terminal] dropping output for unknown session ${payload.sessionId}`);
        }
        return;
      }
      entry.term.write(payload.data);
      // Tier-4 unread indicator. `noteUnread` is a no-op when the session
      // is active or already flagged, so this stays cheap on the hot path.
      useSessionStore.getState().actions.noteUnread(payload.sessionId);
    });
  }

  if (storeUnsubscribe === null) {
    let previousIds = new Set<SessionId>(useSessionStore.getState().sessions.map((s) => s.id));
    storeUnsubscribe = useSessionStore.subscribe((state) => {
      const currentIds = new Set<SessionId>(state.sessions.map((s) => s.id));
      for (const id of previousIds) {
        if (!currentIds.has(id) && registry.has(id)) {
          disposeTerminal(id);
        }
      }
      previousIds = currentIds;
    });
  }
}

/**
 * Explicit boot-time entry point that wires the global `session://output`
 * listener and the session-store subscription that disposes terminals when
 * their session is removed. Idempotent: a second call does nothing. Called
 * from `App.tsx` once hydrate completes; the lazy fallback inside
 * `useTerminal` remains as a safety net for tests / non-boot code paths.
 */
export function initTerminalRouter(): void {
  ensureGlobalSubscriptions();
}

function createEntry(sessionId: SessionId): RegistryEntry {
  const term = new Terminal({
    scrollback: 5000,
    fontFamily: 'monospace',
    cursorBlink: true,
    allowProposedApi: false,
  });
  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);

  term.onData((data) => {
    void sessionInput({ sessionId, data }).catch((err: unknown) => {
      const message = err instanceof Error ? err.message : String(err);
      console.warn(`[use-terminal] session_input(${sessionId}) failed: ${message}`);
    });
  });

  return {
    term,
    fitAddon,
    wrapper: null,
    host: null,
    observer: null,
    resizeTimer: null,
    lastCols: 0,
    lastRows: 0,
  };
}

function getOrCreate(sessionId: SessionId): RegistryEntry {
  let entry = registry.get(sessionId);
  if (!entry) {
    entry = createEntry(sessionId);
    registry.set(sessionId, entry);
  }
  return entry;
}

function teardownObserver(entry: RegistryEntry): void {
  if (entry.observer) {
    entry.observer.disconnect();
    entry.observer = null;
  }
  if (entry.resizeTimer !== null) {
    clearTimeout(entry.resizeTimer);
    entry.resizeTimer = null;
  }
}

function attachToHost(sessionId: SessionId, entry: RegistryEntry, host: HTMLDivElement): void {
  if (entry.host === host && entry.wrapper && entry.wrapper.isConnected) {
    return;
  }

  // Re-parent if attached to a different host.
  if (entry.wrapper && entry.wrapper.parentElement) {
    entry.wrapper.parentElement.removeChild(entry.wrapper);
  }
  teardownObserver(entry);

  const wrapper = entry.wrapper ?? document.createElement('div');
  wrapper.style.width = '100%';
  wrapper.style.height = '100%';
  host.appendChild(wrapper);

  const isNewWrapper = entry.wrapper === null;
  entry.wrapper = wrapper;
  entry.host = host;

  if (isNewWrapper) {
    entry.term.open(wrapper);
  }

  const fire = (): void => {
    entry.resizeTimer = null;
    try {
      entry.fitAddon.fit();
    } catch {
      // fit() throws if the wrapper has zero dimensions (e.g. while the
      // host is `display: none`). Silently skip — the next observer tick
      // will retry once the host is sized.
      return;
    }
    const cols = entry.term.cols;
    const rows = entry.term.rows;
    if (cols <= 0 || rows <= 0) return;
    if (cols === entry.lastCols && rows === entry.lastRows) return;
    entry.lastCols = cols;
    entry.lastRows = rows;
    void sessionResize({ sessionId, cols, rows }).catch((err: unknown) => {
      const message = err instanceof Error ? err.message : String(err);
      console.warn(`[use-terminal] session_resize(${sessionId}) failed: ${message}`);
    });
  };

  if (typeof ResizeObserver !== 'undefined') {
    const observer = new ResizeObserver(() => {
      if (entry.resizeTimer !== null) clearTimeout(entry.resizeTimer);
      entry.resizeTimer = setTimeout(fire, RESIZE_DEBOUNCE_MS);
    });
    observer.observe(host);
    entry.observer = observer;
  } else {
    // Fallback for jsdom and other no-ResizeObserver environments: fire
    // once synchronously so tests can still drive the resize path via
    // container dimension setters.
    fire();
  }
}

function detachFromHost(entry: RegistryEntry): void {
  teardownObserver(entry);
  if (entry.wrapper && entry.wrapper.parentElement) {
    entry.wrapper.parentElement.removeChild(entry.wrapper);
  }
  entry.host = null;
  // Keep `entry.wrapper` so `attach` can re-parent without re-running
  // `term.open`, which would re-initialise xterm's renderer state.
}

export interface UseTerminalApi {
  attach: (el: HTMLDivElement) => void;
  detach: () => void;
  focus: () => void;
}

export function useTerminal(sessionId: SessionId): UseTerminalApi {
  ensureGlobalSubscriptions();

  const sessionIdRef = useRef(sessionId);
  sessionIdRef.current = sessionId;

  const attach = useCallback((el: HTMLDivElement) => {
    const id = sessionIdRef.current;
    const entry = getOrCreate(id);
    attachToHost(id, entry, el);
  }, []);

  const detach = useCallback(() => {
    const entry = registry.get(sessionIdRef.current);
    if (!entry) return;
    detachFromHost(entry);
  }, []);

  const focus = useCallback(() => {
    const entry = registry.get(sessionIdRef.current);
    entry?.term.focus();
  }, []);

  // Eagerly create the terminal so `session://output` events are buffered
  // by xterm even before `attach` runs.
  useEffect(() => {
    getOrCreate(sessionId);
  }, [sessionId]);

  return useMemo(() => ({ attach, detach, focus }), [attach, detach, focus]);
}

export function disposeTerminal(sessionId: SessionId): void {
  const entry = registry.get(sessionId);
  if (!entry) return;
  detachFromHost(entry);
  entry.fitAddon.dispose();
  entry.term.dispose();
  registry.delete(sessionId);
}

/**
 * Test-only: clear the registry and tear down both global subscriptions so
 * the next hook use re-initialises from scratch. Production code never calls
 * this — terminals are disposed individually via the session-store
 * subscription.
 */
export function __resetTerminalRegistryForTests(): void {
  for (const [, entry] of registry) {
    detachFromHost(entry);
    try {
      entry.fitAddon.dispose();
    } catch {
      // ignore
    }
    try {
      entry.term.dispose();
    } catch {
      // ignore
    }
  }
  registry.clear();

  if (outputUnlisten) {
    const pending = outputUnlisten;
    outputUnlisten = null;
    void pending.then((unlisten) => {
      try {
        unlisten();
      } catch {
        // ignore
      }
    });
  }
  if (storeUnsubscribe) {
    try {
      storeUnsubscribe();
    } catch {
      // ignore
    }
    storeUnsubscribe = null;
  }
}

/** Test-only: peek at the registry. */
export function __getTerminalRegistryForTests(): ReadonlyMap<SessionId, RegistryEntry> {
  return registry;
}
