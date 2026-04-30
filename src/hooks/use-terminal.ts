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
  /** Capture-phase listener installed on `host` to forward `paste` events. */
  pasteListener: ((event: ClipboardEvent) => void) | null;
  /** Capture-phase listener installed on `host` to intercept Shift+Enter. */
  keydownListener: ((event: KeyboardEvent) => void) | null;
  /** Last cols/rows reported to the backend; suppresses duplicate calls. */
  lastCols: number;
  lastRows: number;
}

const registry = new Map<SessionId, RegistryEntry>();

let outputUnlisten: Promise<() => void> | null = null;
let storeUnsubscribe: (() => void) | null = null;
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
    pasteListener: null,
    keydownListener: null,
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

function teardownPasteListener(entry: RegistryEntry): void {
  if (entry.pasteListener && entry.host) {
    entry.host.removeEventListener('paste', entry.pasteListener as EventListener, true);
  }
  entry.pasteListener = null;
}

function teardownKeydownListener(entry: RegistryEntry): void {
  if (entry.keydownListener && entry.host) {
    entry.host.removeEventListener('keydown', entry.keydownListener as EventListener, true);
  }
  entry.keydownListener = null;
}

/**
 * Whether the async clipboard read API is available in this runtime.
 *
 * Used by both Ctrl/Cmd+V interception and the `paste` event listener
 * fallback to decide *up front* whether they have any chance of
 * actually pasting. If this returns `false`, the listeners must NOT
 * cancel the event — letting the event propagate gives xterm (or any
 * other interested handler) a chance to act on it instead of leaving
 * the user with a silent no-op.
 */
function canReadClipboard(): boolean {
  return typeof navigator !== 'undefined' && typeof navigator.clipboard?.readText === 'function';
}

/**
 * Read text from the system clipboard via `navigator.clipboard.readText()`
 * and forward it to the terminal via `term.paste()`. Used by both the
 * Ctrl/Cmd+V keydown branch (where xterm's keydown handler would
 * otherwise eat the keystroke before any `paste` event fires) and the
 * `paste` event listener fallback (when `clipboardData` is empty, as
 * happens in some WebView2 right-click → Paste flows).
 *
 * Callers must gate cancellation of the original event on
 * [`canReadClipboard`] — this function silently no-ops if the API is
 * missing, and unconditionally cancelling the event in that case
 * would block xterm from handling the keystroke / paste itself.
 *
 * The async resolution of `readText()` is racy with session disposal:
 * the user could dispatch Ctrl+V and then close the tab before the
 * clipboard read resolves. We re-check the registry by `sessionId`
 * before calling `term.paste(text)` so we don't write into a disposed
 * (or replaced) terminal.
 *
 * Failures are logged but otherwise silent — there's no useful UI
 * recovery and we don't want to spam the user with toasts every time
 * they paste an empty clipboard.
 */
function pasteFromClipboard(sessionId: SessionId, entry: RegistryEntry): void {
  if (!canReadClipboard()) return;
  void navigator.clipboard
    .readText()
    .then((text) => {
      if (!text) return;
      // Guard against a concurrent disposeTerminal(): if the registry
      // entry for this session is gone or has been replaced, drop the
      // paste rather than writing to a stale Terminal instance.
      if (registry.get(sessionId) !== entry) return;
      entry.term.paste(text);
    })
    .catch((err: unknown) => {
      const message = err instanceof Error ? err.message : String(err);
      console.warn(`[use-terminal] clipboard.readText() failed: ${message}`);
    });
}

/**
 * Re-measure + repaint a single terminal. Safe to call on an unattached or
 * zero-size terminal (no-ops). Only emits `sessionResize` when cols/rows
 * have actually changed since the last successful fit. Always calls
 * `term.refresh()` when the renderer has a non-zero viewport — this is the
 * recovery path for stale canvas state after a visibility transition or a
 * pre-font-load initial measurement.
 */
function refitEntry(sessionId: SessionId, entry: RegistryEntry): void {
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
  void sessionResize({ sessionId, cols, rows }).catch((err: unknown) => {
    const message = err instanceof Error ? err.message : String(err);
    console.warn(`[use-terminal] session_resize(${sessionId}) failed: ${message}`);
  });
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
  teardownPasteListener(entry);
  teardownKeydownListener(entry);

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
  refitEntry(sessionId, entry);

  // Capture-phase keydown listener on the host. Two responsibilities:
  //
  // 1. Shift+Enter → ESC + CR (`\x1b\r`). xterm.js by default sends a
  //    plain `\r` for both Enter and Shift+Enter, which CLIs like Claude
  //    Code and GitHub Copilot CLI interpret as "submit". The de-facto
  //    convention (matching what `claude /terminal-setup` configures in
  //    iTerm2) is to send ESC-prefixed CR for "newline without submit".
  //
  // 2. Ctrl+V / Cmd+V → trigger paste via `navigator.clipboard.readText()`
  //    and write the result through `term.paste()`. If we don't intercept,
  //    xterm's own keydown handler eats the keypress (sending the literal
  //    `\x16` SYN byte to the PTY) and the browser never fires a `paste`
  //    event, so our capture-phase paste listener has nothing to handle.
  //    Right-click → Paste still goes through the paste listener below.
  //
  // We listen at the **host** in the **capture** phase so we run before
  // xterm's own keydown listener (registered on its hidden textarea, also
  // capture-phase). xterm's `attachCustomKeyEventHandler` is unreliable
  // here because by the time it runs the textarea may already have
  // committed default behaviour, and we cannot from inside it
  // `preventDefault()` the textarea's own newline insertion. Capturing on
  // the host fully owns the event before any descendant listener.
  const keydownListener = (event: KeyboardEvent): void => {
    // Skip IME composition: Enter/Shift+Enter during candidate selection
    // belongs to the IME, not to the terminal. `keyCode === 229` is the
    // legacy Chromium/WebView signal for "still composing".
    if (event.isComposing || event.keyCode === 229) return;
    if (
      event.key === 'Enter' &&
      event.shiftKey &&
      !event.ctrlKey &&
      !event.altKey &&
      !event.metaKey
    ) {
      event.preventDefault();
      event.stopPropagation();
      void sessionInput({ sessionId, data: '\x1b\r' }).catch((err: unknown) => {
        const message = err instanceof Error ? err.message : String(err);
        console.warn(`[use-terminal] session_input(${sessionId}) failed: ${message}`);
      });
      return;
    }
    // Paste shortcuts. The accepted matrix is asymmetric on purpose:
    //   - Ctrl+V         (Windows/Linux convention)
    //   - Ctrl+Shift+V   (Linux terminal convention — many emulators)
    //   - Cmd+V          (macOS convention; **without** Shift)
    // Cmd+Shift+V on macOS is "paste and match style" in apps that
    // implement formatted clipboard semantics; it has no useful meaning
    // in a terminal and we leave it to pass through unchanged. Alt is
    // never accepted (Ctrl+Alt+V / Cmd+Alt+V / Alt+V are not paste).
    //
    // We match on `event.code === 'KeyV'` rather than `event.key`. `key`
    // is the produced character — on a Russian keyboard layout the
    // physical V position prints `м`, so a `key === 'v'` test would miss
    // the user's normal paste shortcut. `code` reflects the **physical**
    // key location and is layout-independent, which is what every other
    // major terminal app keys off for shortcut matching.
    const v = event.code === 'KeyV';
    const isCtrlPaste = v && event.ctrlKey && !event.metaKey && !event.altKey;
    const isMetaPaste = v && event.metaKey && !event.ctrlKey && !event.altKey && !event.shiftKey;
    if (isCtrlPaste || isMetaPaste) {
      // Only suppress xterm's default handling when we have a paste path
      // that might actually succeed. If `navigator.clipboard.readText`
      // is unavailable, swallowing the event would just turn Ctrl+V
      // into a silent no-op; instead, let it propagate so xterm can do
      // whatever it would have done.
      if (canReadClipboard()) {
        event.preventDefault();
        event.stopPropagation();
        pasteFromClipboard(sessionId, entry);
      }
    }
  };
  host.addEventListener('keydown', keydownListener as EventListener, true);
  entry.keydownListener = keydownListener;

  // Paste support. xterm.js installs its own paste listeners on the
  // textarea AND on its element (`xterm/src/browser/Clipboard.ts`), and
  // its handler calls `event.stopPropagation()` — so a bubble-phase
  // listener at the host **never fires**. Worse, in the Tauri/WebView2
  // environment the `clipboardData` xterm receives via that path is
  // sometimes empty (Ctrl+V / right-click → Paste / X11 middle-click all
  // silently no-op). We capture at the host, which runs before any
  // descendant listener; we own the event end-to-end. If `clipboardData`
  // is populated we use it directly (works without permission, since the
  // user gesture supplies the data). Otherwise we fall back to the async
  // `navigator.clipboard.readText()` — slower, may prompt in some
  // environments, but recovers when the WebView won't fill clipboardData.
  //
  // Cancellation policy: only call `preventDefault`/`stopPropagation`
  // when we have something to paste (inline payload) or a viable async
  // fallback. If both are unavailable we let the event continue so
  // xterm or any other listener still has a shot at handling it.
  const pasteListener = (event: ClipboardEvent): void => {
    const inline = event.clipboardData?.getData('text/plain') ?? '';
    if (inline) {
      event.preventDefault();
      event.stopPropagation();
      entry.term.paste(inline);
      return;
    }
    if (!canReadClipboard()) return;
    event.preventDefault();
    event.stopPropagation();
    pasteFromClipboard(sessionId, entry);
  };
  host.addEventListener('paste', pasteListener as EventListener, true);
  entry.pasteListener = pasteListener;

  if (typeof ResizeObserver !== 'undefined') {
    const observer = new ResizeObserver(() => {
      if (entry.resizeTimer !== null) clearTimeout(entry.resizeTimer);
      entry.resizeTimer = setTimeout(() => refitEntry(sessionId, entry), RESIZE_DEBOUNCE_MS);
    });
    observer.observe(host);
    entry.observer = observer;
  }
}

function detachFromHost(entry: RegistryEntry): void {
  teardownObserver(entry);
  teardownPasteListener(entry);
  teardownKeydownListener(entry);
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

  const refit = useCallback(() => {
    const id = sessionIdRef.current;
    const entry = registry.get(id);
    if (!entry) return;
    refitEntry(id, entry);
  }, []);

  // Eagerly create the terminal so `session://output` events are buffered
  // by xterm even before `attach` runs.
  useEffect(() => {
    getOrCreate(sessionId);
  }, [sessionId]);

  return useMemo(() => ({ attach, detach, focus, refit }), [attach, detach, focus, refit]);
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
  fontsReadyAttached = false;
}

/** Test-only: peek at the registry. */
export function __getTerminalRegistryForTests(): ReadonlyMap<SessionId, RegistryEntry> {
  return registry;
}
