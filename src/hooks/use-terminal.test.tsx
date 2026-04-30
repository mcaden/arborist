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
  paste: ReturnType<typeof vi.fn>;
  cols: number;
  rows: number;
  _dataCb?: (data: string) => void;
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
      paste: vi.fn(),
      cols: 80,
      rows: 24,
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

  it('Shift+Enter on host sends ESC+CR via sessionInput and prevents default', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const evt = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    const dispatched = host.dispatchEvent(evt);

    expect(dispatched).toBe(false); // preventDefault called
    expect(sessionInput).toHaveBeenCalledWith({ sessionId: 's1', data: '\x1b\r' });
  });

  it('plain Enter on host is left to xterm (no interception, no input sent)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const evt = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: false,
      bubbles: true,
      cancelable: true,
    });
    const dispatched = host.dispatchEvent(evt);

    expect(dispatched).toBe(true); // not preventDefault'd
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Shift+Enter keyup is ignored (only keydown is intercepted)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const evt = new KeyboardEvent('keyup', {
      key: 'Enter',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    host.dispatchEvent(evt);

    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Shift+Enter during IME composition is left to the IME', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const evt = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      isComposing: true,
      bubbles: true,
      cancelable: true,
    });
    const dispatched = host.dispatchEvent(evt);

    expect(dispatched).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('legacy IME signal (keyCode 229) is left untouched', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    // KeyboardEvent constructor doesn't accept keyCode; assign on the
    // dispatched event to mimic legacy WebView behaviour.
    const evt = new KeyboardEvent('keydown', {
      key: 'Process',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    Object.defineProperty(evt, 'keyCode', { value: 229 });
    const dispatched = host.dispatchEvent(evt);

    expect(dispatched).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Ctrl+Shift+Enter is not intercepted (other shortcuts unaffected)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const evt = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    const dispatched = host.dispatchEvent(evt);

    expect(dispatched).toBe(true);
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Ctrl+V on host triggers navigator.clipboard.readText and term.paste', async () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const readText = vi.fn().mockResolvedValue('clip-text');
    const originalNav = (globalThis as { navigator?: Navigator }).navigator;
    Object.defineProperty(globalThis, 'navigator', {
      value: { ...originalNav, clipboard: { readText } },
      configurable: true,
    });

    try {
      const evt = new KeyboardEvent('keydown', {
        key: 'v',
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      });
      const prevented = !host.dispatchEvent(evt);

      expect(prevented).toBe(true);
      expect(readText).toHaveBeenCalled();
      await Promise.resolve();
      await Promise.resolve();
      expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('clip-text');
    } finally {
      Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
    }
  });

  it('Cmd+V (metaKey) on host triggers paste via navigator.clipboard.readText', async () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const readText = vi.fn().mockResolvedValue('mac-clip');
    const originalNav = (globalThis as { navigator?: Navigator }).navigator;
    Object.defineProperty(globalThis, 'navigator', {
      value: { ...originalNav, clipboard: { readText } },
      configurable: true,
    });

    try {
      const evt = new KeyboardEvent('keydown', {
        key: 'v',
        metaKey: true,
        bubbles: true,
        cancelable: true,
      });
      const prevented = !host.dispatchEvent(evt);

      expect(prevented).toBe(true);
      expect(readText).toHaveBeenCalled();
      await Promise.resolve();
      await Promise.resolve();
      expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('mac-clip');
    } finally {
      Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
    }
  });

  it('Ctrl+Shift+V on host also triggers paste (Linux terminal convention)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const readText = vi.fn().mockResolvedValue('linux-clip');
    const originalNav = (globalThis as { navigator?: Navigator }).navigator;
    Object.defineProperty(globalThis, 'navigator', {
      value: { ...originalNav, clipboard: { readText } },
      configurable: true,
    });

    try {
      const evt = new KeyboardEvent('keydown', {
        key: 'v',
        ctrlKey: true,
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      });
      const prevented = !host.dispatchEvent(evt);

      expect(prevented).toBe(true);
      expect(readText).toHaveBeenCalled();
    } finally {
      Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
    }
  });

  it('Ctrl+Alt+V is not intercepted (Alt-modifier passthrough)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const readText = vi.fn();
    const originalNav = (globalThis as { navigator?: Navigator }).navigator;
    Object.defineProperty(globalThis, 'navigator', {
      value: { ...originalNav, clipboard: { readText } },
      configurable: true,
    });

    try {
      const evt = new KeyboardEvent('keydown', {
        key: 'v',
        ctrlKey: true,
        altKey: true,
        bubbles: true,
        cancelable: true,
      });
      const dispatched = host.dispatchEvent(evt);

      expect(dispatched).toBe(true);
      expect(readText).not.toHaveBeenCalled();
    } finally {
      Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
    }
  });

  it('plain "v" keystroke (no modifier) is not intercepted', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const readText = vi.fn();
    const originalNav = (globalThis as { navigator?: Navigator }).navigator;
    Object.defineProperty(globalThis, 'navigator', {
      value: { ...originalNav, clipboard: { readText } },
      configurable: true,
    });

    try {
      const evt = new KeyboardEvent('keydown', {
        key: 'v',
        bubbles: true,
        cancelable: true,
      });
      const dispatched = host.dispatchEvent(evt);

      expect(dispatched).toBe(true);
      expect(readText).not.toHaveBeenCalled();
    } finally {
      Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
    }
  });

  it('Shift+Enter listener is removed on detach', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    act(() => result.current.detach());

    const evt = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    host.dispatchEvent(evt);

    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('re-attach to a new host moves the keydown listener (no leak on old host)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host1 = makeHost();
    const host2 = makeHost();
    act(() => result.current.attach(host1));
    act(() => result.current.attach(host2));

    const evt1 = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    host1.dispatchEvent(evt1);
    expect(sessionInput).not.toHaveBeenCalled();

    const evt2 = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    host2.dispatchEvent(evt2);
    expect(sessionInput).toHaveBeenCalledWith({ sessionId: 's1', data: '\x1b\r' });
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

  it('paste DOM event on host forwards clipboard text via term.paste()', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt, 'clipboardData', {
      value: { getData: (type: string) => (type === 'text/plain' ? 'hello world' : '') },
    });
    const prevented = !host.dispatchEvent(evt);

    expect(prevented).toBe(true);
    expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('hello world');
  });

  it('paste falls back to navigator.clipboard.readText when clipboardData is empty', async () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const readText = vi.fn().mockResolvedValue('from-async-clipboard');
    const originalNav = (globalThis as { navigator?: Navigator }).navigator;
    Object.defineProperty(globalThis, 'navigator', {
      value: { ...originalNav, clipboard: { readText } },
      configurable: true,
    });

    try {
      const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
      Object.defineProperty(evt, 'clipboardData', { value: { getData: () => '' } });
      host.dispatchEvent(evt);

      expect(readText).toHaveBeenCalled();
      // Drain the microtask queue so the .then(...) on readText resolves.
      // Two awaits: one for the readText resolution, one for the chained .then.
      await Promise.resolve();
      await Promise.resolve();

      expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('from-async-clipboard');
    } finally {
      Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
    }
  });

  it('paste with empty clipboard and no async fallback does not call term.paste()', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const originalNav = (globalThis as { navigator?: Navigator }).navigator;
    Object.defineProperty(globalThis, 'navigator', {
      value: { ...originalNav, clipboard: undefined },
      configurable: true,
    });

    try {
      const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
      Object.defineProperty(evt, 'clipboardData', { value: { getData: () => '' } });
      host.dispatchEvent(evt);

      expect(mockTerminals[0]!.paste).not.toHaveBeenCalled();
    } finally {
      Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
    }
  });

  it('paste runs in capture phase, beating a descendants stopPropagation', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    // Mimic xterm's behaviour: a descendant listener that stops propagation.
    // Because our host listener is in the **capture** phase, it must run
    // BEFORE this bubble-phase descendant listener gets a chance to stop
    // the event.
    const descendant = document.createElement('div');
    host.appendChild(descendant);
    descendant.addEventListener('paste', (e) => {
      e.stopPropagation();
    });

    const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt, 'clipboardData', {
      value: { getData: () => 'capture-wins' },
    });
    descendant.dispatchEvent(evt);

    expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('capture-wins');
  });

  it('Shift+Enter runs in capture phase, beating a descendants stopPropagation', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    // Same scenario for keydown — xterm registers its keydown listener on
    // its hidden textarea (a descendant of host) with capture-phase too,
    // but because OUR listener is registered on a closer-to-root host
    // capture-phase fires first regardless of where focus actually is.
    const descendant = document.createElement('div');
    host.appendChild(descendant);
    descendant.addEventListener('keydown', (e) => {
      e.stopPropagation();
    });

    const evt = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    descendant.dispatchEvent(evt);

    expect(sessionInput).toHaveBeenCalledWith({ sessionId: 's1', data: '\x1b\r' });
  });

  it('detach removes the paste listener from the host', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    act(() => result.current.detach());

    const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt, 'clipboardData', {
      value: { getData: () => 'after detach' },
    });
    host.dispatchEvent(evt);

    expect(mockTerminals[0]!.paste).not.toHaveBeenCalled();
  });

  it('re-attach to a new host moves the paste listener (no leak on old host)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host1 = makeHost();
    const host2 = makeHost();
    act(() => result.current.attach(host1));
    act(() => result.current.attach(host2));

    const evt1 = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt1, 'clipboardData', { value: { getData: () => 'old' } });
    host1.dispatchEvent(evt1);
    expect(mockTerminals[0]!.paste).not.toHaveBeenCalled();

    const evt2 = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt2, 'clipboardData', { value: { getData: () => 'new' } });
    host2.dispatchEvent(evt2);
    expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('new');
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
