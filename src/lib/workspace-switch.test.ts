// PR5: `workspaceSwitch` is now atomic — the backend runs restore for
// the new workspace under its own write guard and returns the
// post-restore `{ config, sessions }` inline. `changeWorkspace` adopts
// the result into both stores in a single render. These tests pin
// that behaviour and the lock-contention error translation.
//
// PR6: `changeWorkspace` also flips a `workspaceSwitchUiStore`
// `isSwitching` flag synchronously before the invoke and clears it in
// `finally` — even on throw — so `App.tsx` can show an overlay and
// gate input while the backend's transactional switch runs.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { changeWorkspace } from './workspace-switch';
import { resetBridgeMocks, workspaceSwitch } from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';
import { useWorkspaceSwitchUiStore } from '@/store/workspace-switch-ui-store';
import type { AppConfig, SessionView, WorkspaceSwitchResult } from '@/types/arborist';

vi.mock('@/lib/tauri-bridge', () => import('@/lib/tauri-bridge.mock'));

let configAdopt: ReturnType<typeof vi.fn<(sessions: SessionView[], activeSessionId: string | null) => void>>;
let sessionAdopt: ReturnType<typeof vi.fn<(sessions: SessionView[], activeSessionId: string | null) => void>>;

function makeConfig(overrides: Partial<AppConfig> = {}): AppConfig {
  return {
    configVersion: 10,
    defaultInstructionSets: { claude: '', copilot: '' },
    instructionSetsDir: '',
    workspaceRoot: '/new',
    worktreeRoots: [],
    worktreePrepCommands: [],
    aiLaunchCommands: { commands: {}, iconDataUris: {} },
    pluginSettings: { ai: {}, customProcess: {}, dashboardWidget: {} },
    repoCommandTrust: { records: {} },
    lastOpenSessions: [],
    tabOrder: [],
    activeSessionId: null,
    customProcesses: [],
    lastOpenSubSessions: [],
    worktreeTabs: [],
    worktreeTabOrder: [],
    activeWorktreeTabId: null,
    ...overrides,
  };
}

function makeResult(overrides: Partial<WorkspaceSwitchResult> = {}): WorkspaceSwitchResult {
  return {
    workspaceRoot: '/new',
    noOp: false,
    config: makeConfig(),
    sessions: [],
    ...overrides,
  };
}

beforeEach(() => {
  resetBridgeMocks();
  configAdopt = vi.fn<(sessions: SessionView[], activeSessionId: string | null) => void>();
  sessionAdopt = vi.fn<(sessions: SessionView[], activeSessionId: string | null) => void>();
  useConfigStore.setState({ adoptWorkspace: configAdopt } as never);
  useSessionStore.setState((s) => ({
    actions: { ...s.actions, adoptWorkspace: sessionAdopt },
  }));
  useWorkspaceSwitchUiStore.setState({ isSwitching: false });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('changeWorkspace', () => {
  it('atomically adopts the backend-returned config and sessions on a successful switch', async () => {
    const sessions: SessionView[] = [];
    const config = makeConfig({ activeSessionId: null });
    workspaceSwitch.mockResolvedValue(makeResult({ config, sessions }));

    await changeWorkspace('/new');

    expect(workspaceSwitch).toHaveBeenCalledWith('/new');
    expect(configAdopt).toHaveBeenCalledTimes(1);
    expect(configAdopt).toHaveBeenCalledWith(config);
    expect(sessionAdopt).toHaveBeenCalledTimes(1);
    expect(sessionAdopt).toHaveBeenCalledWith(sessions, null);
    // Order matters — config-store must adopt first so workspaceRoot
    // selectors observe the new value before the session list shifts.
    const configOrder = configAdopt.mock.invocationCallOrder[0];
    const sessionOrder = sessionAdopt.mock.invocationCallOrder[0];
    expect(configOrder).toBeDefined();
    expect(sessionOrder).toBeDefined();
    expect(configOrder!).toBeLessThan(sessionOrder!);
  });

  it('forwards the activeSessionId from the result so the session-store can reconcile activeId', async () => {
    const config = makeConfig({ activeSessionId: 'sess-1' });
    workspaceSwitch.mockResolvedValue(makeResult({ config }));

    await changeWorkspace('/new');

    expect(sessionAdopt).toHaveBeenCalledWith(expect.any(Array), 'sess-1');
  });

  it('skips both adoptions on a no-op switch (already on the requested workspace)', async () => {
    workspaceSwitch.mockResolvedValue(makeResult({ workspaceRoot: '/cur', noOp: true }));

    await changeWorkspace('/cur');

    expect(configAdopt).not.toHaveBeenCalled();
    expect(sessionAdopt).not.toHaveBeenCalled();
  });

  it('translates WorkspaceLocked into a user-facing error and skips adoption', async () => {
    workspaceSwitch.mockRejectedValue({
      code: 'WorkspaceLocked',
      message: 'busy',
    });

    await expect(changeWorkspace('/locked')).rejects.toThrow(/already open in another/i);
    expect(configAdopt).not.toHaveBeenCalled();
    expect(sessionAdopt).not.toHaveBeenCalled();
  });

  it('propagates unrelated errors verbatim and skips adoption', async () => {
    const err = new Error('random failure');
    workspaceSwitch.mockRejectedValue(err);

    await expect(changeWorkspace('/x')).rejects.toBe(err);
    expect(configAdopt).not.toHaveBeenCalled();
    expect(sessionAdopt).not.toHaveBeenCalled();
  });
});

describe('changeWorkspace isSwitching flag', () => {
  it('flips isSwitching true before invoke and clears it in finally on success', async () => {
    let isSwitchingDuringInvoke: boolean | undefined;
    workspaceSwitch.mockImplementation(async () => {
      isSwitchingDuringInvoke = useWorkspaceSwitchUiStore.getState().isSwitching;
      return makeResult();
    });

    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(false);
    await changeWorkspace('/new');

    expect(isSwitchingDuringInvoke).toBe(true);
    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(false);
  });

  it('clears isSwitching in finally even when the bridge throws', async () => {
    workspaceSwitch.mockRejectedValue(new Error('boom'));

    await expect(changeWorkspace('/new')).rejects.toThrow(/boom/);

    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(false);
  });

  it('clears isSwitching in finally even when WorkspaceLocked is thrown', async () => {
    workspaceSwitch.mockRejectedValue({ code: 'WorkspaceLocked', message: 'busy' });

    await expect(changeWorkspace('/locked')).rejects.toThrow(/already open in another/i);

    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(false);
  });

  it('clears isSwitching in finally on a no-op switch', async () => {
    workspaceSwitch.mockResolvedValue(makeResult({ noOp: true }));

    await changeWorkspace('/cur');

    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(false);
  });

  it('drops a concurrent second call while the first is still pending and clears the flag when the first resolves', async () => {
    // Hold the first invoke in-flight via a deferred promise so we can
    // observe what happens when a second `changeWorkspace` call lands
    // during the wait. This exercises the actual race the reentrancy
    // guard was added to defend against — two real concurrent calls,
    // not a simulated `setState({ isSwitching: true })` precondition.
    let resolveFirst!: (v: WorkspaceSwitchResult) => void;
    workspaceSwitch.mockImplementationOnce(
      () =>
        new Promise<WorkspaceSwitchResult>((resolve) => {
          resolveFirst = resolve;
        }),
    );

    // Kick off the first call. Yield once so its synchronous prelude
    // (`setSwitching(true)` + `await workspaceSwitch(path)`) runs and
    // the promise is parked on its await.
    const first = changeWorkspace('/a');
    await Promise.resolve();
    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(true);
    expect(workspaceSwitch).toHaveBeenCalledTimes(1);
    expect(workspaceSwitch).toHaveBeenLastCalledWith('/a');

    // Second concurrent call: must observe the flag and short-circuit
    // without invoking the bridge or touching the stores. The
    // returned promise resolves immediately (silent-drop contract —
    // see JSDoc on `changeWorkspace`).
    await changeWorkspace('/b');
    expect(workspaceSwitch).toHaveBeenCalledTimes(1);
    expect(configAdopt).not.toHaveBeenCalled();
    expect(sessionAdopt).not.toHaveBeenCalled();
    // Flag is still owned by the first (in-flight) call.
    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(true);

    // Resolve the first call and let it complete; flag must clear and
    // adoption must fire exactly once (for the first call's result).
    resolveFirst(makeResult({ workspaceRoot: '/a' }));
    await first;
    expect(useWorkspaceSwitchUiStore.getState().isSwitching).toBe(false);
    expect(configAdopt).toHaveBeenCalledTimes(1);
    expect(sessionAdopt).toHaveBeenCalledTimes(1);
  });
});
