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
  getSelection: ReturnType<typeof vi.fn>;
  cols: number;
  rows: number;
  _dataCb?: (data: string) => void;
  _oscHandlers: Map<number, (data: string) => boolean | Promise<boolean>>;
  parser: { registerOscHandler: ReturnType<typeof vi.fn> };
  _core: {
    _renderService: {
      clear: ReturnType<typeof vi.fn>;
      handleCharSizeChanged: ReturnType<typeof vi.fn>;
    };
  };
}> = [];

vi.mock('@xterm/xterm', () => {
  const Terminal = vi.fn(function (this: (typeof mockTerminals)[number]) {
    Object.assign(this, {
      open: vi.fn(),
      write: vi.fn(),
      onData: vi.fn(),
      focus: vi.fn(),
      dispose: vi.fn(),
      loadAddon: vi.fn(),
      refresh: vi.fn(),
      paste: vi.fn(),
      getSelection: vi.fn(() => ''),
      cols: 80,
      rows: 24,
      _oscHandlers: new Map<number, (data: string) => boolean | Promise<boolean>>(),
      parser: {
        registerOscHandler: vi.fn(),
      },
      _core: {
        _renderService: {
          clear: vi.fn(),
          handleCharSizeChanged: vi.fn(),
        },
      },
    });
    this.onData.mockImplementation((cb: (data: string) => void) => {
      this._dataCb = cb;
    });
    this.parser.registerOscHandler.mockImplementation((ident: number, handler: (data: string) => boolean | Promise<boolean>) => {
      this._oscHandlers.set(ident, handler);
      return { dispose: vi.fn() };
    });
    mockTerminals.push(this);
  });
  return { Terminal };
});

const mockFitAddons: Array<{ fit: ReturnType<typeof vi.fn>; dispose: ReturnType<typeof vi.fn> }> = [];
vi.mock('@xterm/addon-fit', () => {
  const FitAddon = vi.fn(function (this: (typeof mockFitAddons)[number]) {
    Object.assign(this, { fit: vi.fn(), dispose: vi.fn() });
    mockFitAddons.push(this);
  });
  return { FitAddon };
});

import { renderHook } from '@testing-library/react';
import {
  __resetTerminalRegistryForTests,
  __getTerminalRegistryForTests,
  disposeTerminal,
  FALLBACK_PTY_DIMS,
  getTerminalDimensions,
  initTerminalRouter,
  measureInitialPtyDimensions,
  useSubTerminal,
  useTerminal,
} from './use-terminal';
import {
  clipboardReadText,
  clipboardWriteText,
  onSessionOutput,
  resetBridgeMocks,
  sessionInput,
  sessionResize,
  subSessionInput,
  subSessionResize,
} from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';

function makeHost(width = 600, height = 400): HTMLDivElement {
  const el = document.createElement('div');
  Object.defineProperty(el, 'clientWidth', { value: width, configurable: true });
  Object.defineProperty(el, 'clientHeight', { value: height, configurable: true });
  document.body.appendChild(el);
  return el;
}

// Build a bubbling, cancelable keydown event with the supplied init — the `bubbles`/`cancelable` flags are required for host dispatch in every case.
function keydown(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
}

// Swap globalThis.navigator for one reporting a macOS `platform`, run `fn`, then restore — used by the Backspace-on-macOS tests. The Backspace
// workaround only fires when `isMacPlatform()` is true; jsdom's default navigator is not macOS, so non-mac tests need no override.
function withMacPlatform(fn: () => void): void {
  const originalNav = (globalThis as { navigator?: Navigator }).navigator;
  Object.defineProperty(globalThis, 'navigator', { value: { ...originalNav, platform: 'MacIntel' }, configurable: true });
  try {
    fn();
  } finally {
    Object.defineProperty(globalThis, 'navigator', { value: originalNav, configurable: true });
  }
}

// Render a terminal and attach it to a fresh host — the preamble nearly every interaction test needs. Returns the host plus the just-created mock
// Terminal so tests can assert against `term.paste`, read its OSC handlers, etc., without repeating the renderHook → makeHost → attach boilerplate.
function attachTerminal(id = 's1'): { host: HTMLDivElement; term: (typeof mockTerminals)[number] } {
  const { result } = renderHook(() => useTerminal(id));
  const host = makeHost();
  act(() => result.current.attach(host));
  return { host, term: mockTerminals[mockTerminals.length - 1]! };
}

// Drive the "modifier+V pastes the clipboard" happy path: mock the system clipboard, dispatch the chord, drain the async read, and assert the text
// reached `term.paste`. Shared by the Ctrl+V / Cmd+V / non-Latin-layout cases, which differ only in the key chord and the pasted payload.
async function expectChordPastes(init: KeyboardEventInit, clip: string): Promise<void> {
  const { host, term } = attachTerminal();
  clipboardReadText.mockResolvedValue(clip);
  const prevented = !host.dispatchEvent(keydown(init));
  expect(prevented).toBe(true);
  expect(clipboardReadText).toHaveBeenCalled();
  await Promise.resolve();
  await Promise.resolve();
  expect(term.paste).toHaveBeenCalledWith(clip);
}

// Attach a terminal and return its registered OSC 52 handler (always present — `createEntry` registers it). The OSC 52 tests differ only in payload.
function attachOsc52Handler(): (data: string) => boolean | Promise<boolean> {
  const { term } = attachTerminal();
  const handler = term._oscHandlers.get(52);
  expect(handler).toBeDefined();
  return handler!;
}

let originalResizeObserver: typeof ResizeObserver | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  resetBridgeMocks();
  mockTerminals.length = 0;
  mockFitAddons.length = 0;
  originalResizeObserver = (globalThis as unknown as { ResizeObserver?: typeof ResizeObserver }).ResizeObserver;
});

afterEach(() => {
  __resetTerminalRegistryForTests();
  // Reset session store activeId so wake-refit tests don't leak state.
  useSessionStore.setState({ activeId: undefined });
  document.body.innerHTML = '';
  vi.useRealTimers();
  // Restore (or delete) ResizeObserver — several tests in this file
  // overwrite globalThis.ResizeObserver with a fake to capture the
  // callback. Without this, the fake leaks into later tests/files and
  // creates order-dependent behavior.
  if (originalResizeObserver === undefined) {
    delete (globalThis as unknown as { ResizeObserver?: typeof ResizeObserver }).ResizeObserver;
  } else {
    (globalThis as unknown as { ResizeObserver: typeof ResizeObserver }).ResizeObserver = originalResizeObserver;
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

  it('Backspace on macOS sends DEL (\\x7f) straight to the PTY and prevents default', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();

    withMacPlatform(() => {
      act(() => result.current.attach(host));
      const dispatched = host.dispatchEvent(keydown({ key: 'Backspace' }));
      expect(dispatched).toBe(false); // preventDefault called
    });
    expect(sessionInput).toHaveBeenCalledWith({ sessionId: 's1', data: '\x7f' });
  });

  it('Option(Alt)+Backspace on macOS sends ESC+DEL (\\x1b\\x7f) for word-delete', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();

    withMacPlatform(() => {
      act(() => result.current.attach(host));
      host.dispatchEvent(keydown({ key: 'Backspace', altKey: true }));
    });
    expect(sessionInput).toHaveBeenCalledWith({ sessionId: 's1', data: '\x1b\x7f' });
  });

  it('Ctrl/Cmd+Backspace on macOS is left to fall through (not intercepted)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();

    withMacPlatform(() => {
      act(() => result.current.attach(host));
      expect(host.dispatchEvent(keydown({ key: 'Backspace', ctrlKey: true }))).toBe(true);
      expect(host.dispatchEvent(keydown({ key: 'Backspace', metaKey: true }))).toBe(true);
    });
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Backspace off macOS is left to xterm (no interception, no input sent)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const dispatched = host.dispatchEvent(keydown({ key: 'Backspace' }));

    expect(dispatched).toBe(true); // not preventDefault'd
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('Ctrl+V on host reads the system clipboard and pastes via term.paste', async () => {
    await expectChordPastes({ key: 'v', code: 'KeyV', ctrlKey: true }, 'clip-text');
  });

  it('Cmd+V (metaKey) on host pastes via the clipboard plugin', async () => {
    await expectChordPastes({ key: 'v', code: 'KeyV', metaKey: true }, 'mac-clip');
  });

  it('Ctrl+Shift+V on host also triggers paste (Linux terminal convention)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const prevented = !host.dispatchEvent(keydown({ key: 'v', code: 'KeyV', ctrlKey: true, shiftKey: true }));

    expect(prevented).toBe(true);
    expect(clipboardReadText).toHaveBeenCalled();
  });

  it('Ctrl+Alt+V is not intercepted (Alt-modifier passthrough)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const dispatched = host.dispatchEvent(keydown({ key: 'v', code: 'KeyV', ctrlKey: true, altKey: true }));

    expect(dispatched).toBe(true);
    expect(clipboardReadText).not.toHaveBeenCalled();
  });

  it('Cmd+Shift+V is not intercepted (passthrough; "paste and match style" on macOS)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const dispatched = host.dispatchEvent(keydown({ key: 'v', code: 'KeyV', metaKey: true, shiftKey: true }));

    expect(dispatched).toBe(true);
    expect(clipboardReadText).not.toHaveBeenCalled();
  });

  it('Cmd+Alt+V is not intercepted (Alt-modifier passthrough on macOS)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const dispatched = host.dispatchEvent(keydown({ key: 'v', code: 'KeyV', metaKey: true, altKey: true }));

    expect(dispatched).toBe(true);
    expect(clipboardReadText).not.toHaveBeenCalled();
  });

  it('Ctrl+Cmd+V is not intercepted (both ctrl and meta together is undefined)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const dispatched = host.dispatchEvent(keydown({ key: 'v', code: 'KeyV', ctrlKey: true, metaKey: true }));

    expect(dispatched).toBe(true);
    expect(clipboardReadText).not.toHaveBeenCalled();
  });

  it('plain "v" keystroke (no modifier) is not intercepted', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const dispatched = host.dispatchEvent(keydown({ key: 'v', code: 'KeyV' }));

    expect(dispatched).toBe(true);
    expect(clipboardReadText).not.toHaveBeenCalled();
  });

  it('Ctrl+V on a non-Latin keyboard layout still triggers paste (matches by code, not key)', async () => {
    // On a Russian QWERTY layout the V position prints `м`, so `event.key` is `'м'` — not `'v'`. We deliberately match on `event.code === 'KeyV'`
    // (physical key) rather than `event.key` so the user's normal paste shortcut works regardless of active keyboard layout.
    await expectChordPastes({ key: 'м', code: 'KeyV', ctrlKey: true }, 'layout-clip');
  });

  it('Ctrl + non-V key with key:"v" is not intercepted (matches by code, not by produced character)', () => {
    // Inverse of the layout test: on a Russian layout the key that produces `'v'` is at a different physical position (code `KeyM`, since Cyrillic
    // `в` is on a different key entirely) — but for this test the important property is that `event.key === 'v'` does not imply the user pressed the V
    // shortcut. Anything that isn't `code === 'KeyV'` must pass through unchanged.
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const dispatched = host.dispatchEvent(keydown({ key: 'v', code: 'KeyM', ctrlKey: true }));

    expect(dispatched).toBe(true);
    expect(clipboardReadText).not.toHaveBeenCalled();
  });

  it('Ctrl+V resolved after disposeTerminal does not write to a stale terminal', async () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    const term = mockTerminals[0]!;

    // Hand-rolled deferred so we can dispatch the keydown, dispose the session, and only THEN resolve the clipboard read — exactly the race we're
    // guarding against.
    let resolveReadText!: (text: string) => void;
    clipboardReadText.mockImplementation(
      () =>
        new Promise<string>((resolve) => {
          resolveReadText = resolve;
        }),
    );
    host.dispatchEvent(keydown({ key: 'v', code: 'KeyV', ctrlKey: true }));
    expect(clipboardReadText).toHaveBeenCalled();

    // Dispose the session BEFORE the clipboard read resolves.
    act(() => disposeTerminal('s1'));

    // Now resolve the pending read. The guard inside pasteFromClipboard should drop the paste because the registry entry is gone.
    resolveReadText('stale-paste');
    await Promise.resolve();
    await Promise.resolve();

    expect(term.paste).not.toHaveBeenCalled();
  });

  it('Ctrl+C with a selection copies it to the clipboard and suppresses SIGINT', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    mockTerminals[0]!.getSelection.mockReturnValue('selected text');

    const prevented = !host.dispatchEvent(keydown({ key: 'c', code: 'KeyC', ctrlKey: true }));

    expect(prevented).toBe(true);
    expect(clipboardWriteText).toHaveBeenCalledWith('selected text');
  });

  it('Ctrl+C with no selection is not intercepted (falls through to SIGINT)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    mockTerminals[0]!.getSelection.mockReturnValue('');

    const dispatched = host.dispatchEvent(keydown({ key: 'c', code: 'KeyC', ctrlKey: true }));

    expect(dispatched).toBe(true);
    expect(clipboardWriteText).not.toHaveBeenCalled();
  });

  it('Ctrl+Shift+C copies the selection', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    mockTerminals[0]!.getSelection.mockReturnValue('shift-copy');

    const prevented = !host.dispatchEvent(keydown({ key: 'C', code: 'KeyC', ctrlKey: true, shiftKey: true }));

    expect(prevented).toBe(true);
    expect(clipboardWriteText).toHaveBeenCalledWith('shift-copy');
  });

  it('Cmd+C copies the selection on macOS', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    mockTerminals[0]!.getSelection.mockReturnValue('mac-copy');

    const prevented = !host.dispatchEvent(keydown({ key: 'c', code: 'KeyC', metaKey: true }));

    expect(prevented).toBe(true);
    expect(clipboardWriteText).toHaveBeenCalledWith('mac-copy');
  });

  it('OSC 52 clipboard write from the CLI is forwarded to the system clipboard', async () => {
    const handler = attachOsc52Handler();

    // OSC 52 payload: "<selection>;<base64>". "hi" → "aGk=".
    const handled = await handler('c;aGk=');

    expect(handled).toBe(true);
    expect(clipboardWriteText).toHaveBeenCalledWith('hi');
  });

  it('OSC 52 decodes multi-byte UTF-8 correctly', async () => {
    const handler = attachOsc52Handler();

    // "café — 日本" base64-encoded from its UTF-8 bytes.
    const b64 = Buffer.from('café — 日本', 'utf-8').toString('base64');
    const handled = await handler(`c;${b64}`);

    expect(handled).toBe(true);
    expect(clipboardWriteText).toHaveBeenCalledWith('café — 日本');
  });

  it('OSC 52 read request ("?") is declined and never leaks the clipboard', async () => {
    const handler = attachOsc52Handler();

    const handled = await handler('c;?');

    expect(handled).toBe(false);
    expect(clipboardWriteText).not.toHaveBeenCalled();
  });

  it('OSC 52 with malformed base64 is declined cleanly', async () => {
    const handler = attachOsc52Handler();

    const handled = await handler('c;@@not-base64@@');

    expect(handled).toBe(false);
    expect(clipboardWriteText).not.toHaveBeenCalled();
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

  it('paste falls back to the clipboard plugin when clipboardData is empty', async () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    clipboardReadText.mockResolvedValue('from-async-clipboard');
    const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt, 'clipboardData', { value: { getData: () => '' } });
    host.dispatchEvent(evt);

    expect(clipboardReadText).toHaveBeenCalled();
    // Drain the microtask queue so the .then(...) on the read resolves. Two awaits: one for the read resolution, one for the chained .then.
    await Promise.resolve();
    await Promise.resolve();

    expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('from-async-clipboard');
  });

  it('paste with empty clipboardData reads the plugin, finds nothing, and pastes nothing (but still preventDefaults)', async () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    clipboardReadText.mockResolvedValue('');
    const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt, 'clipboardData', { value: { getData: () => '' } });
    const prevented = !host.dispatchEvent(evt);

    // The plugin clipboard is always available in Tauri, so we own the paste end-to-end and always suppress the default to avoid a double-paste
    // through xterm's own handler.
    expect(prevented).toBe(true);
    expect(clipboardReadText).toHaveBeenCalled();
    await Promise.resolve();
    await Promise.resolve();
    expect(mockTerminals[0]!.paste).not.toHaveBeenCalled();
  });

  it('paste with inline clipboardData consumes it without reading the plugin', () => {
    // Inline payload is the happy path and must always be consumed directly — we own the paste end-to-end here and never round-trip to the plugin.
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));

    const evt = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent;
    Object.defineProperty(evt, 'clipboardData', {
      value: { getData: () => 'inline-text' },
    });
    const prevented = !host.dispatchEvent(evt);

    expect(prevented).toBe(true);
    expect(mockTerminals[0]!.paste).toHaveBeenCalledWith('inline-text');
    expect(clipboardReadText).not.toHaveBeenCalled();
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
    (globalThis as unknown as { ResizeObserver: typeof ResizeObserver }).ResizeObserver = FakeRO as unknown as typeof ResizeObserver;

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
    (globalThis as unknown as { ResizeObserver: typeof ResizeObserver }).ResizeObserver = FakeRO as unknown as typeof ResizeObserver;

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

describe('useSubTerminal', () => {
  it('forwards input to subSessionInput, not sessionInput', () => {
    const { result } = renderHook(() => useSubTerminal('sub-1' as never));
    const host = makeHost();
    act(() => result.current.attach(host));
    // First Terminal in the registry is the sub-session's.
    const term = mockTerminals[0]!;
    expect(term._dataCb).toBeDefined();
    act(() => {
      term._dataCb!('hello');
    });
    expect(subSessionInput).toHaveBeenCalledWith({ id: 'sub-1', data: 'hello' });
    expect(sessionInput).not.toHaveBeenCalled();
  });

  it('forwards resize to subSessionResize, not sessionResize', () => {
    const { result } = renderHook(() => useSubTerminal('sub-2' as never));
    const host = makeHost();
    act(() => result.current.attach(host));
    expect(subSessionResize).toHaveBeenCalledWith({
      id: 'sub-2',
      cols: 80,
      rows: 24,
    });
    expect(sessionResize).not.toHaveBeenCalled();
  });
});

describe('wake/visibility/DPI refit', () => {
  // These tests cover the sleep/wake recovery path: when WebView2 is
  // suspended (system sleep, monitor unplug), the host's CSS box doesn't
  // change so ResizeObserver never fires, but the renderer canvas can
  // still be stale. We listen for `visibilitychange`, `window.focus`, and
  // DPI media-query changes and refit only the *active* session — hidden
  // sessions self-heal via TerminalView's `isActive` activation refit.

  function dispatchVisibilityChange(hidden: boolean): void {
    Object.defineProperty(document, 'hidden', { value: hidden, configurable: true });
    Object.defineProperty(document, 'visibilityState', {
      value: hidden ? 'hidden' : 'visible',
      configurable: true,
    });
    document.dispatchEvent(new Event('visibilitychange'));
  }

  function setActive(id: string): void {
    act(() => {
      useSessionStore.setState({ activeId: id });
    });
  }

  it('refits the active terminal when the document becomes visible again', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    setActive('s1');
    // Baseline: attach already ran one synchronous fit.
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    // Sleep → wake: dispatch visibilitychange(visible). The refit is
    // coalesced through rAF (which vi's fake timers also drive), so we
    // tick a frame to let it run.
    act(() => {
      dispatchVisibilityChange(false);
      vi.advanceTimersByTime(20);
    });

    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
    expect(mockTerminals[0]!.refresh).toHaveBeenLastCalledWith(0, 23);
  });

  it('does NOT refit when the document is hidden (sleep/minimize)', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    setActive('s1');
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    act(() => {
      dispatchVisibilityChange(true);
      vi.advanceTimersByTime(20);
    });

    // Refit is gated on `!document.hidden`, so a hide event is a no-op.
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
  });

  it('refits the active terminal when window receives focus', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    setActive('s1');
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    act(() => {
      window.dispatchEvent(new Event('focus'));
      vi.advanceTimersByTime(20);
    });

    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
  });

  it('only refits the active session — hidden sessions are skipped (O(1) wake work)', () => {
    // MainArea keeps every TerminalView mounted with `visibility: hidden`
    // so all wrappers are `isConnected`. The wake path must NOT iterate
    // them all — TerminalView's isActive effect already refits when the
    // user activates a previously-hidden tab. This test pins that O(1)
    // contract: 1 active refit, 0 hidden refits.
    const { result: r1 } = renderHook(() => useTerminal('s1'));
    const { result: r2 } = renderHook(() => useTerminal('s2'));
    const h1 = makeHost();
    const h2 = makeHost();
    act(() => {
      r1.current.attach(h1);
      r2.current.attach(h2);
    });
    setActive('s1');
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
    expect(mockFitAddons[1]!.fit).toHaveBeenCalledTimes(1);

    act(() => {
      window.dispatchEvent(new Event('focus'));
      vi.advanceTimersByTime(20);
    });

    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2); // active refit
    expect(mockFitAddons[1]!.fit).toHaveBeenCalledTimes(1); // hidden untouched
  });

  it('coalesces multiple wake events fired in the same frame into a single refit', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    setActive('s1');
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    // Sleep/wake on Windows often fires visibility AND focus back-to-back.
    // All three triggers must collapse into one rAF tick → one extra fit.
    act(() => {
      dispatchVisibilityChange(false);
      window.dispatchEvent(new Event('focus'));
      window.dispatchEvent(new Event('focus'));
      vi.advanceTimersByTime(20);
    });

    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
  });

  it('skips refit when no session is active', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    // No setActive() — activeId stays undefined.
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    act(() => {
      window.dispatchEvent(new Event('focus'));
      vi.advanceTimersByTime(20);
    });

    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
  });

  it('skips refit when the active terminal wrapper is no longer connected', () => {
    const { result } = renderHook(() => useTerminal('s1'));
    const host = makeHost();
    act(() => result.current.attach(host));
    setActive('s1');
    act(() => result.current.detach());

    act(() => {
      window.dispatchEvent(new Event('focus'));
      vi.advanceTimersByTime(20);
    });

    // attach was the only successful fit; the wake refit saw the active
    // entry had no connected wrapper and skipped it.
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
  });

  it('does not double-install listeners across repeated initTerminalRouter calls', () => {
    // Direct assertion on registration, not downstream behavior: the
    // `wakeRefitPending` flag would collapse N independent handlers into
    // one extra fit() per frame anyway, so a fit-count canary cannot
    // distinguish "1 listener" from "N listeners". Spy on
    // addEventListener to count the actual registrations.
    const docSpy = vi.spyOn(document, 'addEventListener');
    const winSpy = vi.spyOn(window, 'addEventListener');
    try {
      initTerminalRouter();
      initTerminalRouter();
      initTerminalRouter();

      const visibilityRegistrations = docSpy.mock.calls.filter((call) => call[0] === 'visibilitychange');
      const focusRegistrations = winSpy.mock.calls.filter((call) => call[0] === 'focus');
      expect(visibilityRegistrations).toHaveLength(1);
      expect(focusRegistrations).toHaveLength(1);
    } finally {
      docSpy.mockRestore();
      winSpy.mockRestore();
    }
  });

  it('refits via DPI media-query change (docking/undocking)', () => {
    // Polyfill matchMedia: capture the most-recently-attached `change`
    // listener so the test can fire it explicitly.
    const mqls: Array<{
      media: string;
      listener: ((event: MediaQueryListEvent) => void) | null;
      removed: boolean;
    }> = [];
    const fakeMatchMedia = (query: string): MediaQueryList => {
      const entry: (typeof mqls)[number] = { media: query, listener: null, removed: false };
      mqls.push(entry);
      return {
        media: query,
        matches: true,
        onchange: null,
        addEventListener: vi.fn((_type: string, cb: (event: MediaQueryListEvent) => void) => {
          entry.listener = cb;
        }),
        removeEventListener: vi.fn(() => {
          entry.removed = true;
        }),
        addListener: vi.fn(),
        removeListener: vi.fn(),
        dispatchEvent: vi.fn(),
      } as unknown as MediaQueryList;
    };
    const originalMatchMedia = window.matchMedia;
    const originalDpr = window.devicePixelRatio;
    window.matchMedia = fakeMatchMedia as typeof window.matchMedia;
    // Start at fractional DPR (Windows 150% scaling).
    Object.defineProperty(window, 'devicePixelRatio', { value: 1.5, configurable: true });
    try {
      const { result } = renderHook(() => useTerminal('s1'));
      const host = makeHost();
      act(() => result.current.attach(host));
      setActive('s1');
      expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

      // Wake listeners installed one DPI query against the current DPR.
      expect(mqls).toHaveLength(1);
      const initial = mqls[0]!;
      expect(initial.media).toBe('(resolution: 1.5dppx)');
      expect(initial.listener).not.toBeNull();

      // DPR changes (laptop docked → external monitor with different DPI).
      // Update window.devicePixelRatio FIRST so the synchronous re-arm
      // inside the change handler reads the new value, then fire change.
      Object.defineProperty(window, 'devicePixelRatio', { value: 2, configurable: true });
      act(() => {
        initial.listener!({ matches: false } as MediaQueryListEvent);
        vi.advanceTimersByTime(20);
      });

      // Refit fired and the (now-stale) query was detached + a new one
      // armed against the NEW DPR (not the stale 1.5).
      expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
      expect(initial.removed).toBe(true);
      expect(mqls).toHaveLength(2);
      expect(mqls[1]!.media).toBe('(resolution: 2dppx)');
      expect(mqls[1]!.listener).not.toBeNull();
    } finally {
      window.matchMedia = originalMatchMedia;
      Object.defineProperty(window, 'devicePixelRatio', {
        value: originalDpr,
        configurable: true,
      });
    }
  });

  it('uses legacy addListener/removeListener when MediaQueryList lacks addEventListener', () => {
    // Older WebViews only expose the deprecated MediaQueryList API
    // (`addListener`/`removeListener`); App.tsx does the same fallback for
    // its prefers-color-scheme query (App.tsx:50-57). Without this
    // fallback the DPI listener would silently never arm on those engines.
    const addListener = vi.fn();
    const removeListener = vi.fn();
    let captured: ((event: MediaQueryListEvent) => void) | null = null;
    const fakeMatchMedia = (query: string): MediaQueryList =>
      ({
        media: query,
        matches: true,
        onchange: null,
        // addEventListener is intentionally `undefined` to force the
        // legacy branch — that's the runtime shape of older WebViews.
        addEventListener: undefined,
        removeEventListener: undefined,
        addListener: addListener.mockImplementation((cb: (event: MediaQueryListEvent) => void) => {
          captured = cb;
        }),
        removeListener,
        dispatchEvent: vi.fn(),
      }) as unknown as MediaQueryList;
    const originalMatchMedia = window.matchMedia;
    window.matchMedia = fakeMatchMedia as typeof window.matchMedia;
    try {
      const { result } = renderHook(() => useTerminal('s1'));
      const host = makeHost();
      act(() => result.current.attach(host));
      setActive('s1');
      expect(addListener).toHaveBeenCalledTimes(1);
      expect(captured).not.toBeNull();

      // Fire the legacy listener — should refit AND detach via removeListener.
      act(() => {
        captured!({ matches: false } as MediaQueryListEvent);
        vi.advanceTimersByTime(20);
      });

      expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(2);
      expect(removeListener).toHaveBeenCalledTimes(1);
      // Re-arm uses the legacy path again on the new MediaQueryList.
      expect(addListener).toHaveBeenCalledTimes(2);
    } finally {
      window.matchMedia = originalMatchMedia;
    }
  });

  it('survives a workspace-switch orphan window: parked active session, then new active', () => {
    // Pins the workspace-switch-safety contract for wake-refit.
    //
    // During `workspace_switch` (see docs/runtime-flows.md#workspace-switching) the backend parks every
    // session in the outgoing workspace (kills the PTY, preserves the
    // record), and the frontend's session-store subscription disposes
    // each terminal entry as its session id leaves the store.
    // `useSessionStore.activeId` is reconciled atomically by
    // `adoptWorkspace` once the new workspace's session list lands.
    //
    // There is a brief orphan window where:
    //   (a) the entry the wake listener might target has been disposed
    //       (registry.get(activeId) returns undefined), or
    //   (b) the entry is gone AND `activeId` still points at the old
    //       session id for one render before adopt runs.
    //
    // A wake event (visibility/focus/DPI) firing in that window must not
    // throw and must not invoke fit on a disposed addon. Once the new
    // workspace's active session is mounted, wake-refit must pick it up
    // on the *next* event without any teardown of the install-once
    // listeners (they live for app lifetime — see `wakeListenersInstalled`).

    const { result: r1 } = renderHook(() => useTerminal('s1'));
    const host1 = makeHost();
    act(() => r1.current.attach(host1));
    setActive('s1');
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    // Simulate park: the session-store subscription disposes the entry
    // when the id leaves the store. Crucially we do NOT clear activeId
    // here — `adoptWorkspace` reconciles it in the same render that
    // installs the new workspace's sessions, but a wake event between
    // the two reads can still see a stale activeId.
    act(() => {
      disposeTerminal('s1');
    });

    // Wake event in the orphan window: the active id resolves to no
    // entry. Must not throw, must not call any disposed addon's fit().
    // (The disposed FitAddon is `mockFitAddons[0]`; we check its call
    // count is unchanged from baseline.)
    expect(() => {
      act(() => {
        dispatchVisibilityChange(false);
        vi.advanceTimersByTime(20);
      });
    }).not.toThrow();
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);

    // New workspace's active session arrives: mount + activate.
    const { result: r2 } = renderHook(() => useTerminal('s2'));
    const host2 = makeHost();
    act(() => r2.current.attach(host2));
    setActive('s2');
    // attach() always runs one synchronous fit on the new entry.
    expect(mockFitAddons[1]!.fit).toHaveBeenCalledTimes(1);

    // Subsequent wake event refits the new active (proves the
    // install-once listeners survived the orphan window — no teardown
    // is required across workspace switch).
    act(() => {
      window.dispatchEvent(new Event('focus'));
      vi.advanceTimersByTime(20);
    });
    expect(mockFitAddons[1]!.fit).toHaveBeenCalledTimes(2);
    // The disposed entry stays untouched — no resurrection path.
    expect(mockFitAddons[0]!.fit).toHaveBeenCalledTimes(1);
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
