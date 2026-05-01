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
} from '@/types/arborist';

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
// Phase 7 commands — real implementations.
// ---------------------------------------------------------------------------

describe('sessionCreate', () => {
  it("calls invoke('session_create') wrapping the args under `args` and forwards the SessionView", async () => {
    const view = {
      id: 'sid-1',
      tool: 'claude' as const,
      worktreePath: '/repo/feat',
      worktreeName: 'feat',
      label: 'feat',
      instructionSetId: 'claude-default',
      status: 'running' as const,
      pid: 1234,
      createdAt: 1700000000,
      tabIndex: 0,
    };
    invokeMock.mockResolvedValueOnce(view);
    const args = {
      tool: 'claude' as const,
      worktreePath: '/repo/feat',
      instructionSetId: 'claude-default',
      cols: 100,
      rows: 30,
    };

    const result = await bridge.sessionCreate(args);

    expect(invokeMock).toHaveBeenCalledWith('session_create', { args });
    expect(result).toEqual(view);
  });
});

describe('sessionList', () => {
  it("calls invoke('session_list') with no args and forwards the array", async () => {
    invokeMock.mockResolvedValueOnce([]);
    const result = await bridge.sessionList();
    expect(invokeMock).toHaveBeenCalledWith('session_list', undefined);
    expect(result).toEqual([]);
  });
});

describe('session id-only commands', () => {
  it.each([
    ['sessionClose', 'session_close'],
    ['sessionFocus', 'session_focus'],
  ] as const)('%s wraps args under `args`', async (fn, command) => {
    invokeMock.mockResolvedValueOnce(undefined);
    const args = { sessionId: 'sid-1' };
    await (bridge[fn] as (a: { sessionId: string }) => Promise<void>)(args);
    expect(invokeMock).toHaveBeenCalledWith(command, { args });
  });

  it('sessionRestart wraps {sessionId, cols, rows} under `args`', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    const args = { sessionId: 'sid-1', cols: 120, rows: 40 };
    await bridge.sessionRestart(args);
    expect(invokeMock).toHaveBeenCalledWith('session_restart', { args });
  });
});

describe('sessionResize', () => {
  it("calls invoke('session_resize') wrapping the args", async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    const args = { sessionId: 'sid-1', cols: 120, rows: 40 };
    await bridge.sessionResize(args);
    expect(invokeMock).toHaveBeenCalledWith('session_resize', { args });
  });
});

describe('sessionInput', () => {
  it("calls invoke('session_input') wrapping the args", async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    const args = { sessionId: 'sid-1', data: 'hi\r' };
    await bridge.sessionInput(args);
    expect(invokeMock).toHaveBeenCalledWith('session_input', { args });
  });

  it('forwards backend rejections', async () => {
    invokeMock.mockRejectedValueOnce({
      code: 'NotFound',
      message: 'session sid-1 not in pty pool',
    });
    await expect(bridge.sessionInput({ sessionId: 'sid-1', data: 'x' })).rejects.toEqual({
      code: 'NotFound',
      message: 'session sid-1 not in pty pool',
    });
  });
});

describe('frontendReady', () => {
  it("calls invoke('frontend_ready') with no args", async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await bridge.frontendReady();
    expect(invokeMock).toHaveBeenCalledWith('frontend_ready', undefined);
  });
});

// ---------------------------------------------------------------------------
// Phase 4 commands — real implementations.
// ---------------------------------------------------------------------------

describe('configGet', () => {
  it("calls invoke('config_get') with no args and returns the parsed AppConfig", async () => {
    const cfg: AppConfig = {
      configVersion: 3,
      defaultInstructionSets: { claude: 'claude-default', copilot: 'copilot-default' },
      instructionSetsDir: '/cfg/instr',
      workspaceRoot: null,
      worktreeRoots: [],
      prelaunchCommands: [],
      worktreePrelaunchCommands: {},
      aiLaunchCommands: { claude: '', copilot: '' },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
    };
    invokeMock.mockResolvedValueOnce(cfg);

    const result = await bridge.configGet();

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith('config_get', undefined);
    expect(result).toEqual(cfg);
  });
});

describe('configSet', () => {
  it("calls invoke('config_set') wrapping the partial under `partial` and returns the merged AppConfig", async () => {
    const merged: AppConfig = {
      configVersion: 4,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: '',
      workspaceRoot: null,
      worktreeRoots: [],
      prelaunchCommands: ['nvm use'],
      worktreePrelaunchCommands: {},
      aiLaunchCommands: { claude: '', copilot: '' },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
    };
    invokeMock.mockResolvedValueOnce(merged);
    const patch: PartialAppConfig = { prelaunchCommands: ['nvm use'] };

    const result = await bridge.configSet(patch);

    expect(invokeMock).toHaveBeenCalledWith('config_set', { partial: patch });
    expect(result).toEqual(merged);
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

describe('workspaceValidate', () => {
  it("calls invoke('workspace_validate', { args: { path } }) and forwards the result", async () => {
    invokeMock.mockResolvedValueOnce({ valid: true });
    const out = await bridge.workspaceValidate('/some/path');
    expect(invokeMock).toHaveBeenCalledWith('workspace_validate', {
      args: { path: '/some/path' },
    });
    expect(out).toEqual({ valid: true });
  });

  it('forwards the inline error when the path is rejected', async () => {
    invokeMock.mockResolvedValueOnce({ valid: false, error: 'not a git repository' });
    const out = await bridge.workspaceValidate('/no');
    expect(out).toEqual({ valid: false, error: 'not a git repository' });
  });
});

describe('worktreeCreate', () => {
  it("calls invoke('worktree_create', { args: { name } }) and forwards the result", async () => {
    invokeMock.mockResolvedValueOnce({ path: '/ws/.worktrees/feat-x' });
    const out = await bridge.worktreeCreate('feat-x');
    expect(invokeMock).toHaveBeenCalledWith('worktree_create', {
      args: { name: 'feat-x' },
    });
    expect(out).toEqual({ path: '/ws/.worktrees/feat-x' });
  });

  it('propagates a rejected invoke', async () => {
    invokeMock.mockRejectedValueOnce(new Error('boom'));
    await expect(bridge.worktreeCreate('x')).rejects.toThrow('boom');
  });
});
