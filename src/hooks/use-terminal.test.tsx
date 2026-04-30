import { act } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

// Hoisted mocks for xterm. Each Terminal instance records its own
// onData callback so tests can fire fake keystrokes.
const mockTerminals: Array<{
  open: ReturnType<typeof vi.fn>;
  write: ReturnType<typeof vi.fn>;
  onData: ReturnType<typeof vi.fn>;
  focus: ReturnType<typeof vi.fn>;
  dispose: ReturnType<typeof vi.fn>;
  loadAddon: ReturnType<typeof vi.fn>;
  refresh: ReturnType<typeof vi.fn>;
  cols: number;
  rows: number;
  _dataCb?: (data: string) => void;
  _core: {
    _renderService: {
      clear: ReturnType<typeof vi.fn>;
      handleCharSizeChanged: ReturnType<typeof vi.fn>;
    };
  };
}> = [];

vi.mock('@xterm/xterm', () => {
  const Terminal = vi.fn().mockImplementation(() => {
    const inst: (typeof mockTerminals)[number] = {
      open: vi.fn(),
      write: vi.fn(),
      onData: vi.fn(),
      focus: vi.fn(),
      dispose: vi.fn(),
      loadAddon: vi.fn(),
      refresh: vi.fn(),
      cols: 80,
      rows: 24,
      _core: {
        _renderService: {
          clear: vi.fn(),
          handleCharSizeChanged: vi.fn(),
        },
      },
    };
    inst.onData.mockImplementation((cb: (data: string) => void) => {
      inst._dataCb = cb;
    });
    mockTerminals.push(inst);
    return inst;
  });
  return { Terminal };
});

const mockFitAddons: Array<{ fit: ReturnType<typeof vi.fn>; dispose: ReturnType<typeof vi.fn> }> =
  [];
vi.mock('@xterm/addon-fit', () => {
  const FitAddon = vi.fn().mockImplementation(() => {
    const inst = { fit: vi.fn(), dispose: vi.fn() };
    mockFitAddons.push(inst);
    return inst;
  });
  return { FitAddon };
});

import { renderHook } from '@testing-library/react';
import {
  __resetTerminalRegistryForTests,
  __getTerminalRegistryForTests,
  captureTerminalDebugSnapshot,
  disposeTerminal,
  FALLBACK_PTY_DIMS,
  getTerminalDimensions,
  initTerminalRouter,
  measureInitialPtyDimensions,
  useTerminal,
} from './use-terminal';
import {
  onSessionOutput,
  resetBridgeMocks,
  sessionInput,
  sessionResize,
} from '@/lib/tauri-bridge.mock';

function makeHost(width = 600, height = 400): HTMLDivElement {
  const el = document.createElement('div');
  Object.defineProperty(el, 'clientWidth', { value: width, configurable: true });
  Object.defineProperty(el, 'clientHeight', { value: height, configurable: true });
  document.body.appendChild(el);
  return el;
}

let originalResizeObserver: typeof ResizeObserver | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  resetBridgeMocks();
  mockTerminals.length = 0;
  mockFitAddons.length = 0;
  originalResizeObserver = (globalThis as unknown as { ResizeObserver?: typeof ResizeObserver })
    .ResizeObserver;
});

afterEach(() => {
  __resetTerminalRegistryForTests();
  document.body.innerHTML = '';
  vi.useRealTimers();
  // Restore (or delete) ResizeObserver — several tests in this file
  // overwrite globalThis.ResizeObserver with a fake to capture the
  // callback. Without this, the fake leaks into later tests/files and
  // creates order-dependent behavior.
  if (originalResizeObserver === undefined) {
    delete (globalThis as unknown as { ResizeObserver?: typeof ResizeObserver }).ResizeObserver;
  } else {
    (globalThis as unknown as { ResizeObserver: typeof ResizeObserver }).ResizeObserver =
      originalResizeObserver;
  }
});

describe('useTerminal', () => {
  it('creates one Terminal per sessionId and reuses it on subsequent calls', () => {
    const { result, rerender } = renderHook(({ id }) => useTerminal(id), {
      initialProps: { id: 's1' },
    });
    expect(mockTerminals).toHaveLength(1);
    rerender({ id: 's1' });
    expect(mockTerminals).toHaveLength(1);
    rerender({ id: 's2' });
    expect(mockTerminals).toHaveLength(2);
    expect(result.current).toBeTruthy();
  });

  it('attach calls term.open once; second attach to same host is a no-op', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    expect(mockTerminals[0]!.open).toHaveBeenCalledTimes(1);
    act(() => result.current.attach(host));
    expect(mockTerminals[0]!.open).toHaveBeenCalledTimes(1);
  });

  it('forwards term.onData through to sessionInput', async () => {
    const { result } = renderHook(() => useTerminal('s1'));
    act(() => result.current.attach(makeHost()));
    const cb = mockTerminals[0]!._dataCb!;
    act(() => cb('hello'));
    expect(sessionInput).toHaveBeenCalledWith({ sessionId: 's1', data: 'hello' });
  });

  it('routes session://output events to the matching Terminal', async () => {
    renderHook(() => useTerminal('s1'));
    renderHook(() => useTerminal('s2'));
    // Drain any microtasks so the lazy onSessionOutput subscription runs.
    await Promise.resolve();
    const cb = onSessionOutput.mock.calls[0]![0];
    act(() => cb({ sessionId: 's2', data: 'abc' }));
    expect(mockTerminals[0]!.write).not.toHaveBeenCalled();
    expect(mockTerminals[1]!.write).toHaveBeenCalledWith('abc');
  });

  it('drops output for unknown session ids', async () => {
    renderHook(() => useTerminal('s1'));
    await Promise.resolve();
    const cb = onSessionOutput.mock.calls[0]![0];
    const debugSpy = vi.spyOn(console, 'debug').mockImplementation(() => {});
    act(() => cb({ sessionId: 'ghost', data: 'x' }));
    expect(mockTerminals[0]!.write).not.toHaveBeenCalled();
    expect(debugSpy).toHaveBeenCalled();
    debugSpy.mockRestore();
  });

  it('detach removes wrapper from host but does NOT dispose the terminal', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    expect(host.children.length).toBe(1);
    act(() => result.current.detach());
    expect(host.children.length).toBe(0);
    expect(mockTerminals[0]!.dispose).not.toHaveBeenCalled();
  });

  it('disposeTerminal removes from registry and disposes term + addon', () => {
    renderHook(() => useTerminal('s1'));
    expect(__getTerminalRegistryForTests().has('s1')).toBe(true);
    disposeTerminal('s1');
    expect(__getTerminalRegistryForTests().has('s1')).toBe(false);
    expect(mockTerminals[0]!.dispose).toHaveBeenCalled();
    expect(mockFitAddons[0]!.dispose).toHaveBeenCalled();
  });

  it('fits synchronously on attach and once more after debounced ResizeObserver', () => {
    // Polyfill ResizeObserver capturing the callback.
    let captured: ResizeObserverCallback | null = null;
    const observe = vi.fn();
    const disconnect = vi.fn();
    class FakeRO {
      constructor(cb: ResizeObserverCallback) {
        captured = cb;
      }
      observe = observe;
      disconnect = disconnect;
      unobserve = vi.fn();
    }
    (globalThis as unknown as { ResizeObserver: typeof ResizeObserver }).ResizeObserver =
      FakeRO as unknown as typeof ResizeObserver;

    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    // Initial synchronous fit happens during attach so the renderer is in
    // a known state immediately, without waiting for the observer's first
    // tick (which races with font loading).
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
    // First sessionResize fires because lastCols/lastRows defaulted to 0.
    expect(sessionResize).toHaveBeenCalledTimes(1);
    expect(sessionResize).toHaveBeenCalledWith({ sessionId: 's1', cols: 80, rows: 24 });

    // Fire several rapid observer ticks; only one debounced fit follows.
    act(() => {
      captured!([], {} as ResizeObserver);
      captured!([], {} as ResizeObserver);
      captured!([], {} as ResizeObserver);
    });
    act(() => {
      vi.advanceTimersByTime(60);
    });

    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
    // Dimensions unchanged → no second sessionResize emission.
    expect(sessionResize).toHaveBeenCalledTimes(1);
  });

  it('initTerminalRouter is idempotent: calling twice does not double-subscribe', () => {
    initTerminalRouter();
    initTerminalRouter();
    initTerminalRouter();
    expect(onSessionOutput).toHaveBeenCalledTimes(1);
  });

  it('refit() runs fit + refresh and emits sessionResize only when dims change', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    // Attach already fit once and emitted one resize (0,0 → 80,24), and
    // refreshed the renderer once (rows=24, so refresh(0, 23)).
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
    expect(sessionResize).toHaveBeenCalledTimes(1);
    expect(mockTerminals[0]!.refresh).toHaveBeenCalledTimes(1);
    expect(mockTerminals[0]!.refresh).toHaveBeenLastCalledWith(0, 23);

    // Same dims → fit + refresh both run (forces internal recompute and
    // viewport repaint) but no extra sessionResize.
    act(() => result.current.refit());
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
    expect(mockTerminals[0]!.refresh).toHaveBeenCalledTimes(2);
    expect(sessionResize).toHaveBeenCalledTimes(1);

    // Change reported dims → refit emits a new sessionResize and repaints
    // against the new row count.
    mockTerminals[0]!.cols = 100;
    mockTerminals[0]!.rows = 30;
    act(() => result.current.refit());
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(3);
    expect(mockTerminals[0]!.refresh).toHaveBeenCalledTimes(3);
    expect(mockTerminals[0]!.refresh).toHaveBeenLastCalledWith(0, 29);
    expect(sessionResize).toHaveBeenCalledTimes(2);
    expect(sessionResize).toHaveBeenLastCalledWith({ sessionId: 's1', cols: 100, rows: 30 });
  });

  it('refit() is a no-op when the terminal is not attached', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    expect(() => act(() => result.current.refit())).not.toThrow();
    expect(mockFitAddons[0]!.fit).not.toHaveBeenCalled();
    expect(mockTerminals[0]!.refresh).not.toHaveBeenCalled();
    expect(sessionResize).not.toHaveBeenCalled();
  });

  it('refit() forces a render-service repaint each call (recovers from fit() short-circuit when dims are unchanged)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const handleCharSizeChanged = mockTerminals[0]!._core._renderService.handleCharSizeChanged;
    const clear = mockTerminals[0]!._core._renderService.clear;

    // Attach already ran one synchronous fit cycle, so each spy fired
    // exactly once during attachment.
    expect(handleCharSizeChanged).toHaveBeenCalledTimes(1);
    expect(clear).toHaveBeenCalledTimes(1);

    // A subsequent imperative refit fires both again — even though the
    // mocked dims haven't changed (FitAddon.fit() would short-circuit
    // its internal renderService.clear + term.resize). This is the exact
    // case where a manual window resize "fixes it" but the old refit
    // path did nothing visible: the renderer's inline sizes on
    // .xterm-screen / row elements were never re-applied.
    act(() => result.current.refit());
    expect(handleCharSizeChanged).toHaveBeenCalledTimes(2);
    expect(clear).toHaveBeenCalledTimes(2);
  });

  it('refit() tolerates xterm builds without _core / _renderService', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    // Simulate a future xterm major that drops or renames the private
    // fields we poke at — refit should degrade to the old fit+refresh
    // behaviour rather than throwing.
    (mockTerminals[0] as unknown as { _core?: unknown })._core = undefined;
    expect(() => act(() => result.current.attach(host))).not.toThrow();
    expect(() => act(() => result.current.refit())).not.toThrow();
    expect(mockFitAddons[0]!.fit).toHaveBeenCalled();
    expect(mockTerminals[0]!.refresh).toHaveBeenCalled();
  });

  it('refit() does not cancel a pending debounced fit when fit() throws', () => {
    let captured: ResizeObserverCallback | null = null;
    class FakeRO {
      constructor(cb: ResizeObserverCallback) {
        captured = cb;
      }
      observe = vi.fn();
      disconnect = vi.fn();
      unobserve = vi.fn();
    }
    (globalThis as unknown as { ResizeObserver: typeof ResizeObserver }).ResizeObserver =
      FakeRO as unknown as typeof ResizeObserver;

    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    // Initial sync fit consumed (1 call). Now arrange for the next fit
    // (debounced via the observer) to be in flight.
    mockFitAddons[0]!.fit.mockClear();

    act(() => {
      captured!([], {} as ResizeObserver);
    });
    // Timer is pending; fit not yet called.
    expect(mockFitAddons[0]!.fit).not.toHaveBeenCalled();

    // Make the next fit() throw — simulates an ancestor going display:none
    // mid-debounce so the host is now zero-size.
    mockFitAddons[0]!.fit.mockImplementationOnce(() => {
      throw new Error('zero size');
    });
    act(() => result.current.refit());
    // refit threw; the pending debounce should NOT have been cancelled.
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    // Advance past the debounce window — the original pending fit fires.
    act(() => vi.advanceTimersByTime(60));
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
  });
});

describe('initial PTY dimension helpers', () => {
  it('getTerminalDimensions returns null for unknown sessions', () => {
    expect(getTerminalDimensions('nope')).toBeNull();
  });

  it('getTerminalDimensions returns null for an entry that has not been attached/fitted yet', () => {
    // Regression for the "80x24 leaks into session_restart" bug: a
    // brand-new xterm Terminal defaults to cols=80/rows=24 even before
    // open()/fit(). Returning those would feed the OS-default size
    // straight back into the backend on a fast Restart click.
    renderHook(() => useTerminal('s1'));
    expect(__getTerminalRegistryForTests().has('s1')).toBe(true);
    expect(getTerminalDimensions('s1')).toBeNull();
  });

  it('getTerminalDimensions returns the measured dims after a successful attach/fit', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    // attach() ran one synchronous refit; lastCols/lastRows now mirror
    // the mock terminal's seeded 80×24.
    expect(getTerminalDimensions('s1')).toEqual({ cols: 80, rows: 24 });
  });

  it('getTerminalDimensions returns null after detach (wrapper no longer connected)', () => {
    // Defensive: detach() leaves the registry entry in place (so the
    // next attach can re-parent without re-running term.open) but
    // disconnects the wrapper. We treat that as "no proven-fit dims"
    // rather than handing back the last-known size, which may not match
    // the new host's layout.
    const { result } = renderHook(() => useTerminal('s1'));
    act(() => result.current.attach(makeHost()));
    expect(getTerminalDimensions('s1')).toEqual({ cols: 80, rows: 24 });
    act(() => result.current.detach());
    expect(getTerminalDimensions('s1')).toBeNull();
  });

  it('measureInitialPtyDimensions reuses an attached, proven-fit terminal when present', () => {
    renderHook(() => useTerminal('s1'));
    // Force-mark the entry as both attached AND proven-fit (lastCols/Rows
    // > 0 — the same gate getTerminalDimensions uses). Without
    // lastCols/Rows the helper would correctly fall through to the DOM
    // probe even though host is connected, since the terminal was never
    // actually fit.
    const entry = __getTerminalRegistryForTests().get('s1')!;
    const host = document.createElement('div');
    document.body.appendChild(host);
    type Mut = {
      host: HTMLElement | null;
      wrapper: HTMLElement | null;
      lastCols: number;
      lastRows: number;
    };
    const mut = entry as unknown as Mut;
    mut.host = host;
    // measureInitialPtyDimensions delegates to getTerminalDimensions,
    // which checks `wrapper?.isConnected` not `host.isConnected`.
    const wrapper = document.createElement('div');
    host.appendChild(wrapper);
    mut.wrapper = wrapper;
    mut.lastCols = 173;
    mut.lastRows = 47;
    expect(measureInitialPtyDimensions()).toEqual({ cols: 173, rows: 47 });
  });

  it('measureInitialPtyDimensions does NOT reuse a connected-but-unfitted entry (lastCols/Rows still 0)', () => {
    // Regression for the "splash too narrow" risk that survived the
    // first round of review fixes: an entry whose host is connected
    // but whose first fitAddon.fit() threw (e.g. host was zero-size
    // mid-transition) leaves lastCols/lastRows at 0 even though
    // term.cols/rows still default to 80/24. Reusing those would feed
    // 80x24 straight back into session_create — exactly the bug we're
    // closing. The helper must skip the entry and fall through.
    renderHook(() => useTerminal('s1'));
    const entry = __getTerminalRegistryForTests().get('s1')!;
    const host = document.createElement('div');
    document.body.appendChild(host);
    const wrapper = document.createElement('div');
    host.appendChild(wrapper);
    type Mut = {
      host: HTMLElement | null;
      wrapper: HTMLElement | null;
      lastCols: number;
      lastRows: number;
    };
    const mut = entry as unknown as Mut;
    mut.host = host;
    mut.wrapper = wrapper;
    // term.cols/rows still default to 80/24 from the xterm mock (the
    // exact OS-default we want to avoid leaking). lastCols/lastRows
    // remain 0 — the proven-fit signal. No <main> in the DOM ⇒ helper
    // must reach the FALLBACK branch instead of returning {80,24}.
    expect(mockTerminals[0]!.cols).toBe(80);
    expect(mockTerminals[0]!.rows).toBe(24);
    expect(mut.lastCols).toBe(0);
    expect(mut.lastRows).toBe(0);
    expect(document.querySelector('main')).toBeNull();
    expect(measureInitialPtyDimensions()).toEqual(FALLBACK_PTY_DIMS);
  });

  it('measureInitialPtyDimensions ignores stale registry entries whose host is not connected', () => {
    // Reproduces the "session is mid-dispose" race: the registry entry
    // still exists, its `host` ref is non-null, but the host has been
    // removed from the DOM. We must NOT reuse those cols/rows — they
    // reflect a previous layout and could mislead the new session's
    // PTY size.
    renderHook(() => useTerminal('s1'));
    const entry = __getTerminalRegistryForTests().get('s1')!;
    const detachedHost = document.createElement('div'); // never appended
    (entry as unknown as { host: HTMLElement | null }).host = detachedHost;
    mockTerminals[0]!.cols = 999;
    mockTerminals[0]!.rows = 999;
    // No <main>, so we expect the fallback rather than the stale dims.
    expect(document.querySelector('main')).toBeNull();
    expect(measureInitialPtyDimensions()).toEqual(FALLBACK_PTY_DIMS);
  });

  it('measureInitialPtyDimensions falls back to defaults when no <main> exists in the DOM', () => {
    // No active terminals (registry empty) and no <main> element.
    expect(document.querySelector('main')).toBeNull();
    expect(measureInitialPtyDimensions()).toEqual(FALLBACK_PTY_DIMS);
  });

  it('measureInitialPtyDimensions falls back to defaults when <main> is laid out at 0×0', () => {
    // jsdom doesn't compute layout, so getBoundingClientRect returns
    // {width: 0, height: 0}. Without the explicit zero-rect bail, the
    // helper would clamp 0/cellW up to the 20-col floor and silently
    // hand back a tiny PTY — exactly the splash-too-narrow regression.
    const main = document.createElement('main');
    document.body.appendChild(main);
    const rect = main.getBoundingClientRect();
    expect(rect.width).toBe(0);
    expect(rect.height).toBe(0);
    expect(measureInitialPtyDimensions()).toEqual(FALLBACK_PTY_DIMS);
  });
});

describe('captureTerminalDebugSnapshot', () => {
  it('returns ancestors oldest-first (root → host parent), matching the documented order', () => {
    // Build a small DOM tree:  body > article > section > main > host
    // Snapshot's `ancestors` must come back as
    // [body, article, section, main] (oldest → youngest).
    const article = document.createElement('article');
    const section = document.createElement('section');
    const main = document.createElement('main');
    const host = document.createElement('div');
    section.appendChild(main);
    article.appendChild(section);
    document.body.appendChild(article);
    main.appendChild(host);

    const { result } = renderHook(() => useTerminal('s1'));
    act(() => result.current.attach(host));

    const snapshot = captureTerminalDebugSnapshot();
    const entry = snapshot.entries.find((e) => e.sessionId === 's1');
    expect(entry).toBeDefined();
    const tags = entry!.ancestors.map((a) => a.tag);
    // describeAncestors walks `host.parentElement` upward (host = the div
    // attached above), so closest-first that's [main, section, article,
    // body, html]. Reversed for oldest-first → [html, body, ...main].
    expect(tags[0]).toBe('html');
    expect(tags[tags.length - 1]).toBe('main');
    expect(tags).toEqual(['html', 'body', 'article', 'section', 'main']);
  });
});
