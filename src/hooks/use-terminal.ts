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
// * `refit()` is exposed so callers can imperatively re-measure + repaint the
//   terminal — used on tab activation (the host doesn't change size on tab
//   switch, so ResizeObserver wouldn't otherwise fire) and after web fonts
//   resolve. It runs `fit()` + `term.refresh()` to recover from any stale
//   renderer state (e.g. measurements taken pre-font-load or while the
//   panel was `visibility: hidden`).

import { FitAddon } from '@xterm/addon-fit';
import { Terminal } from '@xterm/xterm';
import { useCallback, useEffect, useMemo, useRef } from 'react';

import {
  onSessionOutput,
  sessionInput,
  sessionResize,
  subSessionInput,
  subSessionResize,
} from '@/lib/tauri-bridge';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SessionId, SubSessionId } from '@/types/arborist';

/** Time the ResizeObserver waits before firing `fit()` + resize. */
const RESIZE_DEBOUNCE_MS = 50;

/**
 * Which underlying Tauri commands a registry entry's input/resize map to.
 * Sessions and sub-sessions share the registry (UUID id-space is global)
 * but route their I/O through different commands.
 */
type IoKind = 'session' | 'subsession';

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
  /** Discriminator that picks the input/resize command pair. */
  ioKind: IoKind;
}

const registry = new Map<string, RegistryEntry>();

let outputUnlisten: Promise<() => void> | null = null;
let storeUnsubscribe: (() => void) | null = null;
let subStoreUnsubscribe: (() => void) | null = null;
let fontsReadyAttached = false;

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
      // Tier-4 unread indicator. `noteUnread` is a no-op for unknown ids
      // (e.g. sub-session output, which shares the `session://output`
      // channel) and for already-flagged sessions, so this stays cheap on
      // the hot path. Skip outright for non-session entries to make the
      // intent obvious and keep the store untouched on sub-session traffic.
      if (entry.ioKind === 'session') {
        useSessionStore.getState().actions.noteUnread(payload.sessionId);
      }
    });
  }

  if (storeUnsubscribe === null) {
    let previousIds = new Set<SessionId>(useSessionStore.getState().sessions.map((s) => s.id));
    storeUnsubscribe = useSessionStore.subscribe((state) => {
      const currentIds = new Set<SessionId>(state.sessions.map((s) => s.id));
      for (const id of previousIds) {
        if (!currentIds.has(id) && registry.get(id)?.ioKind === 'session') {
          disposeTerminal(id);
        }
      }
      previousIds = currentIds;
    });
  }

  if (subStoreUnsubscribe === null) {
    let previousIds = new Set<SubSessionId>(
      useSubSessionStore.getState().subSessions.map((s) => s.id),
    );
    subStoreUnsubscribe = useSubSessionStore.subscribe((state) => {
      const currentIds = new Set<SubSessionId>(state.subSessions.map((s) => s.id));
      for (const id of previousIds) {
        if (!currentIds.has(id) && registry.get(id)?.ioKind === 'subsession') {
          disposeTerminal(id);
        }
      }
      previousIds = currentIds;
    });
  }

  // Best-effort: once web fonts have settled, refit every attached terminal.
  // Initial fits taken before the monospace font fully loaded can produce
  // wrong cell metrics, leaving the renderer "squished" until the next
  // window resize. Guarded — the FontFaceSet API isn't available in older
  // WebViews and may not exist in jsdom.
  if (
    !fontsReadyAttached &&
    typeof document !== 'undefined' &&
    'fonts' in document &&
    document.fonts &&
    typeof document.fonts.ready === 'object'
  ) {
    fontsReadyAttached = true;
    void document.fonts.ready
      .then(() => {
        for (const [id, entry] of registry) {
          if (entry.wrapper && entry.wrapper.isConnected) {
            refitEntry(id, entry);
          }
        }
      })
      .catch(() => {
        // ignore — refit is best-effort
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

function createEntry(id: string, ioKind: IoKind): RegistryEntry {
  const term = new Terminal({
    scrollback: 5000,
    fontFamily: 'monospace',
    cursorBlink: true,
    allowProposedApi: false,
  });
  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);

  term.onData((data) => {
    const sendInput =
      ioKind === 'session'
        ? sessionInput({ sessionId: id, data })
        : subSessionInput({ id: id as SubSessionId, data });
    void sendInput.catch((err: unknown) => {
      const message = err instanceof Error ? err.message : String(err);
      console.warn(`[use-terminal] ${ioKind} input(${id}) failed: ${message}`);
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
    ioKind,
  };
}

function getOrCreate(id: string, ioKind: IoKind): RegistryEntry {
  let entry = registry.get(id);
  if (!entry) {
    entry = createEntry(id, ioKind);
    registry.set(id, entry);
  } else if (entry.ioKind !== ioKind) {
    // Defensive: a UUID collision across the session and sub-session id
    // spaces would be a load-bearing bug — both routes share the registry,
    // and the input/resize callbacks are baked into the entry on creation.
    // Surface it loudly rather than silently mis-routing input.
    throw new Error(
      `[use-terminal] id ${id} already registered as ${entry.ioKind}, requested ${ioKind}`,
    );
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

/**
 * Re-measure + repaint a single terminal. Safe to call on an unattached or
 * zero-size terminal (no-ops). Only emits `sessionResize` when cols/rows
 * have actually changed since the last successful fit. Always calls
 * `term.refresh()` when the renderer has a non-zero viewport — this is the
 * recovery path for stale canvas state after a visibility transition or a
 * pre-font-load initial measurement.
 */
function refitEntry(id: string, entry: RegistryEntry): void {
  if (!entry.wrapper || !entry.wrapper.isConnected) return;
  try {
    entry.fitAddon.fit();
  } catch {
    // fit() throws on zero-size hosts (e.g. an ancestor is display:none).
    // Bail without clearing any pending debounced fit — that fit was
    // queued for a reason (a real ResizeObserver tick) and may still be
    // wanted once the host is sized again. The next observer tick when
    // the host gains a non-zero rect will also reschedule.
    return;
  }

  // Successful fit — we own the freshly-measured state, so any pending
  // debounced fit is now redundant. Clear it before any further bail-outs
  // so the invariant "successful fit ⇒ no stale debounce" always holds.
  if (entry.resizeTimer !== null) {
    clearTimeout(entry.resizeTimer);
    entry.resizeTimer = null;
  }

  const cols = entry.term.cols;
  const rows = entry.term.rows;
  if (cols <= 0 || rows <= 0) return;

  // Force the renderer to repaint the visible viewport. xterm's canvas
  // renderer can hold stale state when the element transitioned from
  // visibility:hidden → visible, or when fit() computed the same dims as
  // last time and skipped the implicit refresh. `refresh()` is bounded
  // (viewport only, not scrollback) so this is cheap.
  try {
    entry.term.refresh(0, rows - 1);
  } catch {
    // Ignore — xterm versions without refresh() are extremely old.
  }

  if (cols === entry.lastCols && rows === entry.lastRows) return;
  entry.lastCols = cols;
  entry.lastRows = rows;
  const sendResize =
    entry.ioKind === 'session'
      ? sessionResize({ sessionId: id, cols, rows })
      : subSessionResize({ id: id as SubSessionId, cols, rows });
  void sendResize.catch((err: unknown) => {
    const message = err instanceof Error ? err.message : String(err);
    console.warn(`[use-terminal] ${entry.ioKind} resize(${id}) failed: ${message}`);
  });
}

function attachToHost(id: string, entry: RegistryEntry, host: HTMLDivElement): void {
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

  // Synchronous initial fit — don't rely on ResizeObserver's first tick
  // (which races with font loading and can leave the renderer in a stale
  // state if it fires too early).
  refitEntry(id, entry);

  if (typeof ResizeObserver !== 'undefined') {
    const observer = new ResizeObserver(() => {
      if (entry.resizeTimer !== null) clearTimeout(entry.resizeTimer);
      entry.resizeTimer = setTimeout(() => refitEntry(id, entry), RESIZE_DEBOUNCE_MS);
    });
    observer.observe(host);
    entry.observer = observer;
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
  /**
   * Imperatively re-measure + repaint the terminal. Use after a parent
   * visibility transition (e.g. tab activation) — the host's CSS box
   * doesn't change size on `visibility: hidden` ↔ `visible`, so
   * ResizeObserver wouldn't fire on its own. No-op if the terminal isn't
   * attached or has zero dimensions.
   */
  refit: () => void;
}

export function useTerminal(sessionId: SessionId): UseTerminalApi {
  return useTerminalInternal(sessionId, 'session');
}

export function useSubTerminal(subSessionId: SubSessionId): UseTerminalApi {
  return useTerminalInternal(subSessionId, 'subsession');
}

function useTerminalInternal(id: string, ioKind: IoKind): UseTerminalApi {
  ensureGlobalSubscriptions();

  const idRef = useRef(id);
  idRef.current = id;
  const kindRef = useRef(ioKind);
  kindRef.current = ioKind;

  const attach = useCallback((el: HTMLDivElement) => {
    const currentId = idRef.current;
    const entry = getOrCreate(currentId, kindRef.current);
    attachToHost(currentId, entry, el);
  }, []);

  const detach = useCallback(() => {
    const entry = registry.get(idRef.current);
    if (!entry) return;
    detachFromHost(entry);
  }, []);

  const focus = useCallback(() => {
    const entry = registry.get(idRef.current);
    entry?.term.focus();
  }, []);

  const refit = useCallback(() => {
    const currentId = idRef.current;
    const entry = registry.get(currentId);
    if (!entry) return;
    refitEntry(currentId, entry);
  }, []);

  // Eagerly create the terminal so `session://output` events are buffered
  // by xterm even before `attach` runs.
  useEffect(() => {
    getOrCreate(id, ioKind);
  }, [id, ioKind]);

  return useMemo(() => ({ attach, detach, focus, refit }), [attach, detach, focus, refit]);
}

export function disposeTerminal(id: string): void {
  const entry = registry.get(id);
  if (!entry) return;
  detachFromHost(entry);
  entry.fitAddon.dispose();
  entry.term.dispose();
  registry.delete(id);
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
  if (subStoreUnsubscribe) {
    try {
      subStoreUnsubscribe();
    } catch {
      // ignore
    }
    subStoreUnsubscribe = null;
  }
  fontsReadyAttached = false;
}

/** Test-only: peek at the registry. */
export function __getTerminalRegistryForTests(): ReadonlyMap<string, RegistryEntry> {
  return registry;
}
