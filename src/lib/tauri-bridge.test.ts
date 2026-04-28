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
import type {
  AppConfig,
  InstructionSet,
  PartialAppConfig,
  SessionOutputEvent,
  SessionStatusEvent,
} from '@/types/grove';

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
// Phase 4 commands — real implementations.
// ---------------------------------------------------------------------------

describe('configGet', () => {
  it("calls invoke('config_get') with no args and returns the parsed AppConfig", async () => {
    const cfg: AppConfig = {
      configVersion: 1,
      defaultInstructionSets: { claude: 'claude-default', copilot: 'copilot-default' },
      instructionSetsDir: '/cfg/instr',
      worktreeRoots: [],
      prelaunchCommands: [],
      worktreePrelaunchCommands: {},
      lastOpenSessions: [],
      tabOrder: [],
    };
    invokeMock.mockResolvedValueOnce(cfg);

    const result = await bridge.configGet();

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith('config_get', undefined);
    expect(result).toEqual(cfg);
  });
});

describe('configSet', () => {
  it("calls invoke('config_set') wrapping the partial under `partial`", async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    const patch: PartialAppConfig = { prelaunchCommands: ['nvm use'] };

    await bridge.configSet(patch);

    expect(invokeMock).toHaveBeenCalledWith('config_set', { partial: patch });
  });

  it('forwards rejections from the backend', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'InvalidPath', message: 'relative' });
    await expect(bridge.configSet({ instructionSetsDir: 'relative/x' })).rejects.toEqual({
      code: 'InvalidPath',
      message: 'relative',
    });
  });
});

describe('instructionsList', () => {
  it("calls invoke('instructions_list') and returns the list", async () => {
    const sets: InstructionSet[] = [
      {
        id: 'claude-default',
        name: 'Claude default',
        tool: 'claude',
        filePath: '/cfg/instr/claude-default.md',
        isDefault: true,
      },
    ];
    invokeMock.mockResolvedValueOnce(sets);

    const result = await bridge.instructionsList();

    expect(invokeMock).toHaveBeenCalledWith('instructions_list', undefined);
    expect(result).toEqual(sets);
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
