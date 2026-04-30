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
  attachCustomKeyEventHandler: ReturnType<typeof vi.fn>;
  cols: number;
  rows: number;
  _dataCb?: (data: string) => void;
  _keyHandler?: (event: KeyboardEvent) => boolean;
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
      attachCustomKeyEventHandler: vi.fn(),
      cols: 80,
      rows: 24,
    };
    inst.onData.mockImplementation((cb: (data: string) => void) => {
      inst._dataCb = cb;
    });
    inst.attachCustomKeyEventHandler.mockImplementation((cb: (event: KeyboardEvent) => boolean) => {
      inst._keyHandler = cb;
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
  disposeTerminal,
  initTerminalRouter,
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

  it('Shift+Enter sends ESC+CR via sessionInput and suppresses xterm default', () => {
    renderHook(() => useTerminal('s1'));
    const handler = mockTerminals[0]!._keyHandler!;
    expect(handler).toBeTypeOf('function');
    const evt = {
      type: 'keydown',
      key: 'Enter',
      shiftKey: true,
      ctrlKey: false,
      altKey: false,
      metaKey: false,
    } as unknown as KeyboardEvent;
    const result = handler(evt);
    expect(result).toBe(false);
    expect(sessionInput).toHaveBeenCalledWith({ sessionId: 's1', data: '\x1b\r' });
  });

  it('plain Enter is left to xterm default (handler returns true)', () => {
    renderHook(() => useTerminal('s1'));
    const handler = mockTerminals[0]!._keyHandler!;
    const evt = {
      type: 'keydown',
      key: 'Enter',
      shiftKey: false,
      ctrlKey: false,
      altKey: false,
      metaKey: false,
    } as unknown as KeyboardEvent;
    expect(handler(evt)).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Shift+Enter keyup is ignored (handler returns true, no input sent)', () => {
    renderHook(() => useTerminal('s1'));
    const handler = mockTerminals[0]!._keyHandler!;
    const evt = {
      type: 'keyup',
      key: 'Enter',
      shiftKey: true,
      ctrlKey: false,
      altKey: false,
      metaKey: false,
    } as unknown as KeyboardEvent;
    expect(handler(evt)).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Shift+Enter during IME composition is left to the IME', () => {
    renderHook(() => useTerminal('s1'));
    const handler = mockTerminals[0]!._keyHandler!;
    const evt = {
      type: 'keydown',
      key: 'Enter',
      shiftKey: true,
      ctrlKey: false,
      altKey: false,
      metaKey: false,
      isComposing: true,
      keyCode: 13,
    } as unknown as KeyboardEvent;
    expect(handler(evt)).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('legacy IME signal (keyCode 229) is left untouched', () => {
    renderHook(() => useTerminal('s1'));
    const handler = mockTerminals[0]!._keyHandler!;
    const evt = {
      type: 'keydown',
      key: 'Process',
      shiftKey: true,
      ctrlKey: false,
      altKey: false,
      metaKey: false,
      isComposing: false,
      keyCode: 229,
    } as unknown as KeyboardEvent;
    expect(handler(evt)).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Ctrl+Shift+Enter is not intercepted (other shortcuts unaffected)', () => {
    renderHook(() => useTerminal('s1'));
    const handler = mockTerminals[0]!._keyHandler!;
    const evt = {
      type: 'keydown',
      key: 'Enter',
      shiftKey: true,
      ctrlKey: true,
      altKey: false,
      metaKey: false,
    } as unknown as KeyboardEvent;
    expect(handler(evt)).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
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
