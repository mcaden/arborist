// Unit tests for the typed Tauri bridge wrapper. We mock both
// `@tauri-apps/api/core` (for `invoke`) and `@tauri-apps/api/event` (for
// `listen`) so the tests run in plain jsdom without a Tauri runtime.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.fn();
const listenMock = vi.fn();

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: (event: string, cb: (payload: unknown) => void) => listenMock(event, cb),
}));

import * as bridge from './tauri-bridge';
import type { SessionOutputEvent, SessionStatusEvent } from '@/types/grove';

beforeEach(() => {
  invokeMock.mockReset();
  listenMock.mockReset();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('ping', () => {
  it("calls invoke('ping') and forwards the resolved value", async () => {
    invokeMock.mockResolvedValueOnce('pong');

    const result = await bridge.ping();

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith('ping', undefined);
    expect(result).toBe('pong');
  });
});

// ---------------------------------------------------------------------------
// Stub coverage. Each command listed in DESIGN §6 (other than `ping`) must
// reject with `'not implemented'` until the corresponding phase lands.
// Adding a real implementation will flip exactly one of these assertions —
// at which point that row should move into its own behavioural test and be
// removed from this list.
// ---------------------------------------------------------------------------

interface StubCase {
  readonly name: keyof typeof bridge;
  readonly invoke: () => Promise<unknown>;
}

const STUB_CASES: readonly StubCase[] = [
  {
    name: 'sessionCreate',
    invoke: () =>
      bridge.sessionCreate({
        tool: 'claude',
        worktreePath: '/tmp/wt',
        instructionSetId: 'claude-default',
      }),
  },
  { name: 'sessionList', invoke: () => bridge.sessionList() },
  {
    name: 'sessionClose',
    invoke: () => bridge.sessionClose({ sessionId: 'sid' }),
  },
  {
    name: 'sessionFocus',
    invoke: () => bridge.sessionFocus({ sessionId: 'sid' }),
  },
  {
    name: 'sessionResize',
    invoke: () => bridge.sessionResize({ sessionId: 'sid', cols: 80, rows: 24 }),
  },
  {
    name: 'sessionInput',
    invoke: () => bridge.sessionInput({ sessionId: 'sid', data: 'x' }),
  },
  {
    name: 'sessionRestart',
    invoke: () => bridge.sessionRestart({ sessionId: 'sid' }),
  },
  { name: 'configGet', invoke: () => bridge.configGet() },
  { name: 'configSet', invoke: () => bridge.configSet({}) },
  { name: 'instructionsList', invoke: () => bridge.instructionsList() },
];

describe('command stubs', () => {
  it.each(STUB_CASES)('$name rejects with "not implemented"', async ({ invoke }) => {
    await expect(invoke()).rejects.toThrow('not implemented');
    // Stubs must not touch the real Tauri invoke until they are
    // implemented for real.
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

// ---------------------------------------------------------------------------
// Event subscriber wrappers
// ---------------------------------------------------------------------------

describe('onSessionOutput', () => {
  it('subscribes to session://output, forwards the payload, and returns the unlisten fn', async () => {
    const unlisten = vi.fn();
    let captured: ((event: { payload: SessionOutputEvent }) => void) | null = null;
    listenMock.mockImplementation(
      (_event: string, cb: (event: { payload: SessionOutputEvent }) => void) => {
        captured = cb;
        return Promise.resolve(unlisten);
      },
    );

    const cb = vi.fn();
    const returned = await bridge.onSessionOutput(cb);

    expect(listenMock).toHaveBeenCalledWith('session://output', expect.any(Function));
    expect(returned).toBe(unlisten);

    const payload: SessionOutputEvent = { sessionId: 'sid', data: 'hi' };
    expect(captured).not.toBeNull();
    captured!({ payload });
    expect(cb).toHaveBeenCalledWith(payload);
  });
});

describe('onSessionStatus', () => {
  it('subscribes to session://status, forwards the payload, and returns the unlisten fn', async () => {
    const unlisten = vi.fn();
    let captured: ((event: { payload: SessionStatusEvent }) => void) | null = null;
    listenMock.mockImplementation(
      (_event: string, cb: (event: { payload: SessionStatusEvent }) => void) => {
        captured = cb;
        return Promise.resolve(unlisten);
      },
    );

    const cb = vi.fn();
    const returned = await bridge.onSessionStatus(cb);

    expect(listenMock).toHaveBeenCalledWith('session://status', expect.any(Function));
    expect(returned).toBe(unlisten);

    const payload: SessionStatusEvent = {
      sessionId: 'sid',
      status: 'running',
    };
    expect(captured).not.toBeNull();
    captured!({ payload });
    expect(cb).toHaveBeenCalledWith(payload);
  });
});
