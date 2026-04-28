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

beforeEach(() => {
  vi.useFakeTimers();
  resetBridgeMocks();
  mockTerminals.length = 0;
  mockFitAddons.length = 0;
});

afterEach(() => {
  __resetTerminalRegistryForTests();
  document.body.innerHTML = '';
  vi.useRealTimers();
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

  it('debounced ResizeObserver triggers fit + sessionResize once', () => {
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

    // Fire several rapid resizes; only one debounced call should fire.
    act(() => {
      captured!([], {} as ResizeObserver);
      captured!([], {} as ResizeObserver);
      captured!([], {} as ResizeObserver);
    });
    act(() => {
      vi.advanceTimersByTime(60);
    });

    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
    expect(sessionResize).toHaveBeenCalledTimes(1);
    expect(sessionResize).toHaveBeenCalledWith({ sessionId: 's1', cols: 80, rows: 24 });
  });
});
