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
  formatError,
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
  /** Capture-phase listener installed on `host` to forward `paste` events. */
  pasteListener: ((event: ClipboardEvent) => void) | null;
  /** Capture-phase listener installed on `host` to intercept Shift+Enter. */
  keydownListener: ((event: KeyboardEvent) => void) | null;
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
      const message = formatError(err);
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
    pasteListener: null,
    keydownListener: null,
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
function pasteFromClipboard(id: string, entry: RegistryEntry): void {
  if (!canReadClipboard()) return;
  void navigator.clipboard
    .readText()
    .then((text) => {
      if (!text) return;
      // Guard against a concurrent disposeTerminal(): if the registry
      // entry for this id is gone or has been replaced, drop the
      // paste rather than writing to a stale Terminal instance.
      if (registry.get(id) !== entry) return;
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
  const sendResize =
    entry.ioKind === 'session'
      ? sessionResize({ sessionId: id, cols, rows })
      : subSessionResize({ id: id as SubSessionId, cols, rows });
  void sendResize.catch((err: unknown) => {
    const message = formatError(err);
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
  refitEntry(id, entry);

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
      // Dispatch through the same kind-aware switch the term.onData
      // handler uses (see createEntry) so Shift+Enter works for both
      // parent sessions and terminal sub-sessions.
      const inputPromise =
        entry.ioKind === 'session'
          ? sessionInput({ sessionId: id as SessionId, data: '\x1b\r' })
          : subSessionInput({ id: id as SubSessionId, data: '\x1b\r' });
      void inputPromise.catch((err: unknown) => {
        const message = err instanceof Error ? err.message : String(err);
        console.warn(`[use-terminal] ${entry.ioKind} input(${id}) failed: ${message}`);
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
        pasteFromClipboard(id, entry);
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
    pasteFromClipboard(id, entry);
  };
  host.addEventListener('paste', pasteListener as EventListener, true);
  entry.pasteListener = pasteListener;

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
   * Clear the visible viewport AND scrollback. Lighter than
   * `term.reset()` (which also resets parsing state and rebuilds the
   * renderer); we only want to wipe what the user sees so the next
   * session starts clean. No-op when there's no entry yet.
   *
   * Used by `SubTerminalView` on the exited→starting transition so a
   * relaunched terminal doesn't begin atop the previous run's final
   * frame.
   */
  clear: () => void;
  /**
   * Current xterm `cols`/`rows` for this session, or `null` if the
   * terminal has no entry yet (created lazily on first `attach`/render).
   * Used by callers that need to drive a backend respawn at the right
   * size (e.g. `session_restart` after a crash).
   */
  getDimensions: () => InitialPtyDims | null;
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

  const clear = useCallback(() => {
    const entry = registry.get(idRef.current);
    if (!entry) return;
    // xterm's `clear()` wipes viewport + scrollback but preserves
    // renderer/state — exactly what we want for a sub-session
    // relaunch (heavier `reset()` would re-init the renderer and
    // briefly flash).
    entry.term.clear();
  }, []);

  const getDimensions = useCallback(() => getTerminalDimensions(idRef.current), []);

  // Eagerly create the terminal so `session://output` events are buffered
  // by xterm even before `attach` runs.
  useEffect(() => {
    getOrCreate(id, ioKind);
  }, [id, ioKind]);

  return useMemo(
    () => ({ attach, detach, focus, refit, clear, getDimensions }),
    [attach, detach, focus, refit, clear, getDimensions],
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

// --------------------------------------------------------------------------
// Debug helpers (Sidebar "Fit" button)
// --------------------------------------------------------------------------
//
// `forceRefitAllTerminals` is the imperative escape hatch users can hit when
// the renderer looks wrong. It runs the same `refitEntry` path that the
// debounced ResizeObserver and the activation rAF would, on every entry —
// not just the active one — so a hidden tab also gets corrected before the
// user switches to it.
//
// `captureTerminalDebugSnapshot` produces a JSON-serializable snapshot of
// each registry entry plus environment context (DPR, fonts state,
// visibility, window size). The Sidebar button captures one snapshot
// BEFORE forcing a refit and another AFTER, so a paste-back into a fresh
// debug session shows both the suspected-bad state and what the manual
// refit changed.

export function forceRefitAllTerminals(): void {
  for (const [id, entry] of registry) {
    refitEntry(id, entry);
  }
}

export interface TerminalDebugRect {
  width: number;
  height: number;
  top: number;
  left: number;
}

export interface TerminalDebugAncestor {
  tag: string;
  classes: string;
  display: string;
  visibility: string;
  position: string;
  width: number;
  height: number;
}

export interface TerminalDebugEntry {
  sessionId: SessionId;
  isAttached: boolean;
  hostConnected: boolean;
  wrapperConnected: boolean;
  termCols: number;
  termRows: number;
  lastReportedCols: number;
  lastReportedRows: number;
  fontFamily: string | undefined;
  fontSize: number | undefined;
  hostRect: TerminalDebugRect | null;
  wrapperRect: TerminalDebugRect | null;
  screenRect: TerminalDebugRect | null;
  /** Approximate cell dims derived from `.xterm-screen` rect / cols/rows. */
  approxCellWidth: number | null;
  approxCellHeight: number | null;
  /** Computed style of the host element. */
  hostDisplay: string | null;
  hostVisibility: string | null;
  /** A few ancestors, oldest-first (root → host's parent). */
  ancestors: TerminalDebugAncestor[];
}

export interface TerminalDebugSnapshot {
  capturedAt: string;
  windowInnerWidth: number | null;
  windowInnerHeight: number | null;
  devicePixelRatio: number | null;
  documentVisibility: string | null;
  documentHasFocus: boolean | null;
  fontsStatus: string | null;
  darkMode: boolean | null;
  registrySize: number;
  entries: TerminalDebugEntry[];
}

function safeRect(el: Element | null): TerminalDebugRect | null {
  if (!el) return null;
  try {
    const r = el.getBoundingClientRect();
    return { width: r.width, height: r.height, top: r.top, left: r.left };
  } catch {
    return null;
  }
}

function safeComputed(el: Element | null): CSSStyleDeclaration | null {
  if (!el || typeof window === 'undefined') return null;
  try {
    return window.getComputedStyle(el);
  } catch {
    return null;
  }
}

function describeAncestors(host: HTMLElement | null, max = 6): TerminalDebugAncestor[] {
  // Walk parent chain bottom-up (closest-first, capped at `max`), then
  // reverse so the returned array is oldest-first (root → host's parent)
  // as documented on `TerminalDebugEntry.ancestors`. Reading the snapshot
  // top-down matches how DevTools shows the DOM tree.
  const collected: TerminalDebugAncestor[] = [];
  let cur: HTMLElement | null = host?.parentElement ?? null;
  let depth = 0;
  while (cur && depth < max) {
    const cs = safeComputed(cur);
    const r = safeRect(cur);
    collected.push({
      tag: cur.tagName.toLowerCase(),
      classes: cur.className || '',
      display: cs?.display ?? '',
      visibility: cs?.visibility ?? '',
      position: cs?.position ?? '',
      width: r?.width ?? 0,
      height: r?.height ?? 0,
    });
    cur = cur.parentElement;
    depth += 1;
  }
  return collected.reverse();
}

function describeEntry(sessionId: SessionId, entry: RegistryEntry): TerminalDebugEntry {
  const host = entry.host;
  const wrapper = entry.wrapper;
  const screen = wrapper?.querySelector<HTMLElement>('.xterm-screen') ?? null;
  const screenRect = safeRect(screen);
  const cols = entry.term.cols;
  const rows = entry.term.rows;
  const hostCs = safeComputed(host);

  // Best-effort font info — xterm's options are typed loosely; coerce.
  let fontFamily: string | undefined;
  let fontSize: number | undefined;
  try {
    const opts = entry.term.options as { fontFamily?: string; fontSize?: number } | undefined;
    fontFamily = opts?.fontFamily;
    fontSize = opts?.fontSize;
  } catch {
    // ignore
  }

  return {
    sessionId,
    isAttached: !!host,
    hostConnected: host?.isConnected ?? false,
    wrapperConnected: wrapper?.isConnected ?? false,
    termCols: cols,
    termRows: rows,
    lastReportedCols: entry.lastCols,
    lastReportedRows: entry.lastRows,
    fontFamily,
    fontSize,
    hostRect: safeRect(host),
    wrapperRect: safeRect(wrapper),
    screenRect,
    approxCellWidth: screenRect && cols > 0 ? screenRect.width / cols : null,
    approxCellHeight: screenRect && rows > 0 ? screenRect.height / rows : null,
    hostDisplay: hostCs?.display ?? null,
    hostVisibility: hostCs?.visibility ?? null,
    ancestors: describeAncestors(host),
  };
}

export function captureTerminalDebugSnapshot(): TerminalDebugSnapshot {
  const hasWindow = typeof window !== 'undefined';
  const hasDoc = typeof document !== 'undefined';
  const fonts = hasDoc ? (document as Document & { fonts?: { status?: string } }).fonts : undefined;
  return {
    capturedAt: new Date().toISOString(),
    windowInnerWidth: hasWindow ? window.innerWidth : null,
    windowInnerHeight: hasWindow ? window.innerHeight : null,
    devicePixelRatio: hasWindow ? window.devicePixelRatio : null,
    documentVisibility: hasDoc ? document.visibilityState : null,
    documentHasFocus:
      hasDoc && typeof document.hasFocus === 'function' ? document.hasFocus() : null,
    fontsStatus: fonts?.status ?? null,
    darkMode: hasDoc ? document.documentElement.classList.contains('dark') : null,
    registrySize: registry.size,
    entries: Array.from(registry, ([id, entry]) => describeEntry(id, entry)),
  };
}
