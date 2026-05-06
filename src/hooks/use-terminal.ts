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
// * Wake/visibility/DPI refit: when the OS sleeps, WebView2 can suspend its
//   renderer; on resume the canvas/inline-size state can be stale even
//   though the host's CSS box never changed (so `ResizeObserver` doesn't
//   fire). We listen for `document.visibilitychange` (visible again),
//   `window.focus` (covers WebView2 cases where focus returns without a
//   visibility transition) and `matchMedia('(resolution: <DPR>dppx)')`
//   `change` (DPI change from docking/undocking), and refit only the
//   *active* terminal. Hidden terminals (kept mounted by `MainArea` with
//   `visibility: hidden`) are deliberately skipped — `TerminalView`'s
//   `isActive` effect already runs `refit()` on tab activation, so they
//   self-heal when the user switches to them. This keeps the wake pass
//   O(1) regardless of how many sessions are open. All triggers are
//   coalesced through a single `rAF` so a sleep→wake that fires multiple
//   events still only does one refit pass.
//
//   Workspace-switch safety (DESIGN.md §5.5c — `workspace_switch`):
//   `workspace_switch` parks every session in the outgoing workspace
//   (PTY killed, persisted record preserved) and inline-restores the new
//   workspace's sessions under a write barrier. The session-store
//   subscription disposes each terminal entry as its id leaves the
//   store, then `adoptWorkspace` atomically swaps in the new session
//   list + reconciled `activeId`. Wake-refit must remain safe across
//   that transition without any explicit teardown of the install-once
//   wake listeners. Three guards make this true:
//     1. `scheduleWakeRefit` reads `useSessionStore.getState().activeId`
//        *inside* its `rAF` callback (not at install time), so it sees
//        the post-`adoptWorkspace` value.
//     2. Before calling `refitEntry` it checks
//        `entry.wrapper.isConnected`; parked / disposed entries either
//        return `undefined` from `registry.get` (no entry) or fail the
//        `isConnected` check, and the callback no-ops.
//     3. `refitEntry` itself re-checks `entry.wrapper.isConnected` and
//        the surrounding call site is wrapped in `try/catch`.
//   A wake event firing in the orphan window between disposal and
//   adopt is therefore a benign no-op; the next wake event after
//   `adoptWorkspace` refits the new active session. Pinned by the
//   "survives a workspace-switch orphan window" test in
//   `use-terminal.test.tsx`.

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

// Wake-refit listener state. All module-scope state in this block —
// the install flag, the rAF/timer coalescing handles, the visibility/
// focus listener references, and the DPI media query plus its
// API-agnostic detach closure — is owned by `ensureWakeListeners()`
// (install) and `teardownWakeListeners()` (test-only cleanup); no other
// site reads or mutates it. The DPI media query is re-attached against
// the new DPR after every fire because `(resolution: Xdppx)` queries
// are pinned to a specific value — so we listen for the *current* DPR
// transitioning false, then re-arm.
let wakeListenersInstalled = false;
let wakeRefitPending = false;
let wakeRefitFrame: number | null = null;
let wakeRefitFallbackTimer: ReturnType<typeof setTimeout> | null = null;
let visibilityListener: (() => void) | null = null;
let focusListener: (() => void) | null = null;
let dpiMediaQuery: MediaQueryList | null = null;
// Detach closure captured at install time — invokes either
// `removeEventListener('change', ...)` (modern) or `removeListener(...)`
// (legacy WebView fallback) so teardown doesn't have to know which API
// the runtime supports.
let dpiMediaQueryDetach: (() => void) | null = null;

/**
 * Coalesce multiple wake triggers (visibility, focus, DPI) into a single
 * refit pass per animation frame. Only refits the *active* session;
 * hidden sessions (kept mounted by `MainArea` with `visibility: hidden`)
 * are skipped because `TerminalView`'s `isActive` effect already runs
 * `refit()` when the user switches to them — so they self-heal lazily.
 * This keeps wake work O(1) regardless of session count.
 */
function scheduleWakeRefit(): void {
  if (wakeRefitPending) return;
  wakeRefitPending = true;

  const run = (): void => {
    wakeRefitPending = false;
    wakeRefitFrame = null;
    if (wakeRefitFallbackTimer !== null) {
      clearTimeout(wakeRefitFallbackTimer);
      wakeRefitFallbackTimer = null;
    }
    const activeId = useSessionStore.getState().activeId;
    if (!activeId) return;
    const entry = registry.get(activeId);
    if (!entry || !entry.wrapper || !entry.wrapper.isConnected) return;
    try {
      refitEntry(activeId, entry);
    } catch {
      // refitEntry already swallows fit() throws; this is belt-and-suspenders.
    }
  };

  if (typeof requestAnimationFrame === 'function') {
    wakeRefitFrame = requestAnimationFrame(run);
  } else {
    wakeRefitFallbackTimer = setTimeout(run, 0);
  }
}

/**
 * Install/refresh the DPI media-query listener. We pin to the current DPR;
 * when it transitions (docking/undocking, monitor change during sleep) we
 * refit and re-arm against the new DPR. Idempotent w.r.t. an already-armed
 * query — the caller is expected to clear `dpiMediaQuery` before re-arming.
 *
 * Older WebViews only expose the legacy `addListener`/`removeListener` API
 * on `MediaQueryList` (no `addEventListener`); we mirror the fallback used
 * in `App.tsx` for the dark-mode media query.
 */
function installDpiListener(): void {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return;
  const dpr = window.devicePixelRatio || 1;
  let mq: MediaQueryList;
  try {
    mq = window.matchMedia(`(resolution: ${dpr}dppx)`);
  } catch {
    // Some older WebViews don't support `resolution` in matchMedia syntax.
    // DPI changes are best-effort; visibility/focus still cover sleep/wake.
    return;
  }
  let detach: (() => void) | null = null;
  const listener = (_event: MediaQueryListEvent): void => {
    scheduleWakeRefit();
    // Detach the (now-stale) query and re-arm against the new DPR.
    if (detach) {
      try {
        detach();
      } catch {
        // ignore
      }
    }
    if (dpiMediaQuery === mq) {
      dpiMediaQuery = null;
      dpiMediaQueryDetach = null;
    }
    installDpiListener();
  };
  // Modern: addEventListener. Legacy WebViews: addListener.
  if (typeof mq.addEventListener === 'function') {
    try {
      mq.addEventListener('change', listener);
    } catch {
      return;
    }
    detach = (): void => mq.removeEventListener('change', listener);
  } else if (typeof mq.addListener === 'function') {
    try {
      mq.addListener(listener);
    } catch {
      return;
    }
    detach = (): void => mq.removeListener(listener);
  } else {
    // Neither API available — DPI changes will go unhandled, but
    // visibility/focus still cover sleep/wake.
    return;
  }
  dpiMediaQuery = mq;
  dpiMediaQueryDetach = detach;
}

/**
 * Wire up wake/visibility/DPI listeners that recover the renderer after
 * the OS suspends WebView2 (system sleep, monitor unplug, etc). Idempotent;
 * a second call is a no-op. See file header for the design rationale.
 */
function ensureWakeListeners(): void {
  if (wakeListenersInstalled) return;
  if (typeof window === 'undefined' || typeof document === 'undefined') return;

  visibilityListener = (): void => {
    if (!document.hidden) scheduleWakeRefit();
  };
  document.addEventListener('visibilitychange', visibilityListener);

  focusListener = (): void => {
    scheduleWakeRefit();
  };
  window.addEventListener('focus', focusListener);

  installDpiListener();
  wakeListenersInstalled = true;
}

/**
 * Test-only: tear down the wake listeners installed by `ensureWakeListeners`.
 * Called from `__resetTerminalRegistryForTests` so each test starts clean.
 */
function teardownWakeListeners(): void {
  if (typeof document !== 'undefined' && visibilityListener) {
    document.removeEventListener('visibilitychange', visibilityListener);
  }
  visibilityListener = null;
  if (typeof window !== 'undefined' && focusListener) {
    window.removeEventListener('focus', focusListener);
  }
  focusListener = null;
  if (dpiMediaQueryDetach) {
    try {
      dpiMediaQueryDetach();
    } catch {
      // ignore
    }
  }
  dpiMediaQuery = null;
  dpiMediaQueryDetach = null;
  if (wakeRefitFrame !== null && typeof cancelAnimationFrame === 'function') {
    cancelAnimationFrame(wakeRefitFrame);
  }
  wakeRefitFrame = null;
  if (wakeRefitFallbackTimer !== null) {
    clearTimeout(wakeRefitFallbackTimer);
    wakeRefitFallbackTimer = null;
  }
  wakeRefitPending = false;
  wakeListenersInstalled = false;
}

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

  ensureWakeListeners();
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
 * Shape of the bits of xterm's `_core` we poke at via private API to fix
 * fit-time DOM-sizing quirks (see `refitEntry`). Any missing piece is
 * silently skipped — every call site uses optional chaining.
 */
interface XtermCorePeek {
  _core?: {
    _renderService?: {
      clear?: () => void;
      handleCharSizeChanged?: () => void;
    };
  };
}

/**
 * Re-measure + repaint a single terminal. Safe to call on an unattached or
 * zero-size terminal (no-ops). Only emits `sessionResize` when cols/rows
 * have actually changed since the last successful fit.
 *
 * Why this is more than just `fitAddon.fit()` plus `refresh()`:
 *
 * The renderer writes the `.xterm-screen` and row elements' size as
 * **inline styles** in pixels (DomRenderer._updateDimensions: width =
 * `cols × cell.width`, height = `rows × cell.height`). Those inline sizes
 * are only refreshed by four code paths inside xterm: `term.resize()`,
 * the `onCharSizeChange` event, the `onDevicePixelRatioChange` handler,
 * and an option change. When the renderer's inline sizes drift out of
 * sync with the host's actual CSS box (the easy way: any sequence where
 * the host's box is sized AFTER the renderer first wrote inline pixels —
 * visibility transition, late layout pass, parent flex resolving after
 * mount) the terminal looks "squished" or "doesn't fit" until something
 * triggers one of those four paths.
 *
 * `FitAddon.fit()` only triggers `term.resize()` when proposed cols/rows
 * differ from current. Window resizes naturally take that branch (the
 * host's CSS width genuinely changes); tab activation, fonts.ready, and
 * the manual force-refit hit the no-op branch and leave stale inline
 * sizes intact. We mirror what fit() *would* have done by:
 *
 *   1. Calling `_renderService.handleCharSizeChanged()` after fit() —
 *      forces `_updateDimensions` to re-apply `cols × cell.width` and
 *      `rows × cell.height` to every row + `.xterm-screen` element.
 *      Cheap when nothing actually changed; effective when the inline
 *      sizes were stale.
 *   2. Calling `_renderService.clear()` so the renderer drops any cached
 *      paint state from the previous (stale) layout — same call FitAddon
 *      itself uses internally on the resize branch.
 *
 * Both pokes use private xterm APIs that `FitAddon` itself uses
 * internally. They are guarded with optional chaining so a future xterm
 * major that renames or removes them simply degrades to today's
 * stale-state behavior rather than crashing.
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

  const renderService = (entry.term as unknown as XtermCorePeek)._core?._renderService;

  // (1) Force re-application of the renderer's inline sizes
  // (.xterm-screen + row elements) from current cols × cell.width. See
  // the doc comment above for why this is the missing piece that makes a
  // programmatic refit behave like a window resize.
  try {
    renderService?.handleCharSizeChanged?.();
  } catch {
    // Ignore — best-effort.
  }

  // (2) Drop any cached paint state so the next render frame starts
  // fresh. Same call FitAddon uses on its resize branch.
  try {
    renderService?.clear?.();
  } catch {
    // Ignore — best-effort.
  }

  // Belt-and-suspenders: refresh the visible viewport so any renderer
  // that ignored clear() (or that batches dirty rows) still repaints.
  // Bounded to viewport (not scrollback) so this is cheap.
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
  /**
   * Current xterm `cols`/`rows` for this session, or `null` if the
   * terminal has no entry yet (created lazily on first `attach`/render).
   * Used by callers that need to drive a backend respawn at the right
   * size (e.g. `session_restart` after a crash).
   */
  getDimensions: () => InitialPtyDims | null;
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

  const getDimensions = useCallback(() => getTerminalDimensions(sessionIdRef.current), []);

  // Eagerly create the terminal so `session://output` events are buffered
  // by xterm even before `attach` runs.
  useEffect(() => {
    getOrCreate(sessionId);
  }, [sessionId]);

  return useMemo(
    () => ({ attach, detach, focus, refit, getDimensions }),
    [attach, detach, focus, refit, getDimensions],
  );
}

/**
 * Initial PTY dimensions handed to `session_create` / `session_restart`
 * before the frontend has a chance to mount + fit the xterm Terminal.
 *
 * The pre-fix bug: the backend always opened the PTY at
 * `DEFAULT_PTY_SIZE` (80×24), so the CLI's first paint (the splash, the
 * first prompt) was rendered at 80 cols regardless of how wide the
 * actual host turned out to be. The eventual `session_resize` from
 * `fitAddon.fit()` came after the splash had already been drawn into
 * scrollback at 80-col layout. Frontend code now passes the real
 * intended dims at create/restart time so the child's first byte sees
 * the right width.
 *
 * This file is the right home because the per-session xterm registry
 * (where existing terminals can be sampled for accurate dims) is
 * module-private here.
 */
export interface InitialPtyDims {
  cols: number;
  rows: number;
}

/**
 * Conservative starting point used when no live terminal exists to
 * sample (very-first-session boot) and the DOM probe also fails. Wider
 * than the historical 80-col default so the splash isn't artificially
 * narrow on the rare bad-measure path; the post-mount `fitAddon.fit()`
 * will issue a `session_resize` to the true host width within a frame
 * either way.
 */
export const FALLBACK_PTY_DIMS: Readonly<InitialPtyDims> = Object.freeze({
  cols: 132,
  rows: 40,
});

const PROBE_SAMPLE_TEXT = 'M'.repeat(80);

/** Inner padding (px on each side) of `TerminalView`'s host wrapper. */
const TERMINAL_HOST_INNER_PADDING_PX = 8;

/**
 * Read the *measured* cols/rows of an existing terminal entry. Returns
 * `null` unless the entry is attached to a connected host AND has been
 * successfully fit at least once.
 *
 * Why the strict gate: a fresh `Terminal` defaults to 80×24 even before
 * `open()`/`fit()`, so a naive `term.cols/term.rows` read can leak the
 * old hardcoded default into `session_restart`. We use `entry.lastCols`
 * /`lastRows` as the proven-fit signal — those are written only by a
 * successful `refitEntry()` (which itself requires a connected wrapper
 * and a non-throwing `fit()`).
 */
export function getTerminalDimensions(sessionId: SessionId): InitialPtyDims | null {
  const entry = registry.get(sessionId);
  if (!entry) return null;
  if (!entry.wrapper?.isConnected) return null;
  const cols = entry.lastCols;
  const rows = entry.lastRows;
  if (cols <= 0 || rows <= 0) return null;
  return { cols, rows };
}

/**
 * Estimate the cols/rows a brand-new session's PTY should be opened at
 * so the CLI's first paint matches the eventual `fitAddon.fit()` size.
 *
 * Strategy, in order:
 *   1. Reuse any *proven-fit* terminal's measured cols/rows (delegates
 *      to [`getTerminalDimensions`], which gates on a successful fit
 *      so the xterm 80×24 default never leaks out). All sessions share
 *      the same `<main>` host, so cell metrics (and therefore cols/rows)
 *      are identical between proven-fit entries.
 *   2. Fall back to a one-off DOM probe: measure a hidden monospace
 *      `<span>` for cell-width × cell-height and divide the `<main>`
 *      element's rect (minus `TerminalView`'s 8-px padding) by it.
 *   3. If the DOM isn't available (jsdom without a `<main>`, or any
 *      probe failure), return [`FALLBACK_PTY_DIMS`].
 */
export function measureInitialPtyDimensions(): InitialPtyDims {
  // Reuse fast-path: any *proven-fit* terminal entry. We delegate to
  // `getTerminalDimensions`, which gates on `wrapper.isConnected` AND
  // `entry.lastCols/lastRows > 0`. A plain `entry.term.cols/rows`
  // check here would silently hand back the xterm 80×24 default for
  // an entry whose host is connected but currently zero-size (so its
  // first `fitAddon.fit()` threw and `lastCols/lastRows` stayed 0) —
  // exactly the splash-too-narrow regression we're guarding against.
  // Cell metrics are identical across entries (all share the same
  // `<main>` font), so any proven-fit entry is as good as another.
  for (const id of registry.keys()) {
    const dims = getTerminalDimensions(id);
    if (dims !== null) return dims;
  }

  if (typeof document === 'undefined') {
    return { ...FALLBACK_PTY_DIMS };
  }

  const main = document.querySelector('main');
  if (!main) return { ...FALLBACK_PTY_DIMS };

  const rect = main.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) {
    // `<main>` is in the tree but laid out at 0×0 (e.g. an ancestor is
    // `display: none` mid-transition). Clamping `0/cellW` to a 20-col
    // floor would silently produce a tiny PTY — the very thing the
    // splash-too-narrow regression test guards against. Bail to the
    // fallback so the post-mount `fitAddon.fit()` corrects it.
    return { ...FALLBACK_PTY_DIMS };
  }

  let cellWidth = 0;
  let cellHeight = 0;
  try {
    const probe = document.createElement('span');
    probe.textContent = PROBE_SAMPLE_TEXT;
    probe.style.cssText =
      'position:absolute;left:-9999px;top:-9999px;visibility:hidden;' +
      'white-space:pre;font-family:monospace;font-size:15px;line-height:normal;';
    document.body.appendChild(probe);
    cellWidth = probe.offsetWidth / PROBE_SAMPLE_TEXT.length;
    cellHeight = probe.offsetHeight;
    document.body.removeChild(probe);
  } catch {
    // jsdom or hostile environment — fall through to defaults.
  }

  if (cellWidth <= 0 || cellHeight <= 0) return { ...FALLBACK_PTY_DIMS };

  const usableW = Math.max(rect.width - TERMINAL_HOST_INNER_PADDING_PX * 2, 0);
  const usableH = Math.max(rect.height - TERMINAL_HOST_INNER_PADDING_PX * 2, 0);
  const cols = Math.max(20, Math.floor(usableW / cellWidth));
  const rows = Math.max(5, Math.floor(usableH / cellHeight));
  return { cols, rows };
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
  teardownWakeListeners();
}

/** Test-only: peek at the registry. */
export function __getTerminalRegistryForTests(): ReadonlyMap<SessionId, RegistryEntry> {
  return registry;
}
