// Behavioural tests for `useConfigStore`. We mock the Tauri bridge wholesale
// so no real `invoke()` is exercised — see `tauri-bridge.mock.ts`.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { AppConfig, PartialAppConfig } from '@/types/arborist';

import { useConfigStore } from './config-store';

const SAMPLE: AppConfig = {
  configVersion: 4,
  defaultInstructionSets: { claude: 'claude-default', copilot: 'copilot-default' },
  instructionSetsDir: '/cfg/instr',
  workspaceRoot: null,
  worktreeRoots: ['/repo'],
  worktreePrepCommands: ['nvm use'],
  aiLaunchCommands: { commands: {}, iconDataUris: {} },
  lastOpenSessions: [],
  tabOrder: [],
  activeSessionId: null,
  customProcesses: [],
  lastOpenSubSessions: [],
  worktreeTabs: [],
  worktreeTabOrder: [],
  activeWorktreeTabId: null,
};

function resetStore(): void {
  useConfigStore.setState({
    config: {
      configVersion: 4,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: '',
      workspaceRoot: null,
      worktreeRoots: [],
      worktreePrepCommands: [],
      aiLaunchCommands: { commands: {}, iconDataUris: {} },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: [],
      lastOpenSubSessions: [],
      worktreeTabs: [],
      worktreeTabOrder: [],
      activeWorktreeTabId: null,
    },
    status: 'idle',
    error: null,
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  resetStore();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('useConfigStore.hydrate', () => {
  it('calls config_get exactly once and stores the returned snapshot', async () => {
    bridgeMock.configGet.mockResolvedValueOnce(SAMPLE);

    await useConfigStore.getState().hydrate();

    expect(bridgeMock.configGet).toHaveBeenCalledTimes(1);
    expect(useConfigStore.getState().config).toEqual(SAMPLE);
    expect(useConfigStore.getState().status).toBe('ready');
    expect(useConfigStore.getState().error).toBeNull();
  });

  it('exposes errors via status and re-throws', async () => {
    bridgeMock.configGet.mockRejectedValueOnce(new Error('boom'));

    await expect(useConfigStore.getState().hydrate()).rejects.toThrow('boom');

    expect(useConfigStore.getState().status).toBe('error');
    expect(useConfigStore.getState().error).toBe('boom');
  });
});

describe('useConfigStore.set', () => {
  it('forwards only the diff (no undefined fields) to config_set', async () => {
    // Cast away `exactOptionalPropertyTypes` so we can hand `config_set` an
    // explicit `undefined` and verify the store strips it. Production
    // callers cannot construct this shape (the type forbids it), but the
    // store contract still needs to defend against an accidental
    // `undefined` slipping through e.g. `Object.fromEntries`.
    const diff = {
      worktreePrepCommands: ['nvm use'],
      // explicit undefined must be stripped
      instructionSetsDir: undefined,
    } as unknown as PartialAppConfig;

    await useConfigStore.getState().set(diff);

    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    const [arg] = bridgeMock.configSet.mock.calls[0]!;
    expect(arg).toEqual({ worktreePrepCommands: ['nvm use'] });
    expect(arg).not.toHaveProperty('instructionSetsDir');
  });

  it('mirrors the merged config returned by the backend after a successful write', async () => {
    useConfigStore.setState({ config: { ...SAMPLE } });
    // Backend returns the merged config — the frontend trusts that
    // snapshot wholesale (load-bearing for backend-derived fields like
    // `iconDataUri`, which the frontend never sends).
    bridgeMock.configSet.mockResolvedValueOnce({
      ...SAMPLE,
      worktreePrepCommands: ['echo hi'],
    });

    await useConfigStore.getState().set({ worktreePrepCommands: ['echo hi'] });

    expect(useConfigStore.getState().config.worktreePrepCommands).toEqual(['echo hi']);
    // Untouched fields survive (mirrored from the returned snapshot).
    expect(useConfigStore.getState().config.instructionSetsDir).toBe('/cfg/instr');
  });

  it('deep-merges defaultInstructionSets so a partial patch keeps the other tool', async () => {
    useConfigStore.setState({ config: { ...SAMPLE } });
    bridgeMock.configSet.mockResolvedValueOnce({
      ...SAMPLE,
      defaultInstructionSets: { claude: 'claude-other', copilot: 'copilot-default' },
    });
    await useConfigStore.getState().set({
      defaultInstructionSets: { claude: 'claude-other' },
    });
    expect(useConfigStore.getState().config.defaultInstructionSets).toEqual({
      claude: 'claude-other',
      copilot: 'copilot-default',
    });
  });

  it('does not mutate the cache when the backend rejects', async () => {
    useConfigStore.setState({ config: { ...SAMPLE } });
    bridgeMock.configSet.mockRejectedValueOnce({
      code: 'InvalidPath',
      message: 'relative',
    });

    await expect(useConfigStore.getState().set({ instructionSetsDir: 'rel/path' })).rejects.toMatchObject({ code: 'InvalidPath' });

    // Cache untouched.
    expect(useConfigStore.getState().config.instructionSetsDir).toBe('/cfg/instr');
  });

  it('mirrors customProcesses + lastOpenSubSessions from the backend snapshot', async () => {
    useConfigStore.setState({ config: { ...SAMPLE } });
    const def = {
      id: 'shell',
      name: 'Shell',
      kind: 'terminal' as const,
      command: 'bash -i',
      enabled: true,
    };
    const rec = {
      id: 'sub-1',
      parentWorktreeTabId: 'tab-sess-1',
      defId: 'shell',
      kind: 'terminal' as const,
      label: 'Shell',
      composedCommand: 'bash -i',
    };
    // Backend returns the merged config; this also mimics the icon
    // backfill populating `iconDataUri` server-side.
    const defWithIcon = { ...def, iconDataUri: 'data:image/png;base64,XYZ' };
    bridgeMock.configSet.mockResolvedValueOnce({
      ...SAMPLE,
      customProcesses: [defWithIcon],
      lastOpenSubSessions: [rec],
    });
    await useConfigStore.getState().set({
      customProcesses: [def],
      lastOpenSubSessions: [rec],
    });
    expect(useConfigStore.getState().config.customProcesses).toEqual([defWithIcon]);
    expect(useConfigStore.getState().config.lastOpenSubSessions).toEqual([rec]);
  });
});
