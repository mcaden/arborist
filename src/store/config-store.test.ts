// Behavioural tests for `useConfigStore`. We mock the Tauri bridge wholesale
// so no real `invoke()` is exercised — see `tauri-bridge.mock.ts`.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { AppConfig, PartialAppConfig } from '@/types/arborist';

import { useConfigStore } from './config-store';

const SAMPLE: AppConfig = {
  configVersion: 3,
  defaultInstructionSets: { claude: 'claude-default', copilot: 'copilot-default' },
  instructionSetsDir: '/cfg/instr',
  workspaceRoot: null,
  worktreeRoots: ['/repo'],
  prelaunchCommands: ['nvm use'],
  worktreePrelaunchCommands: { '/repo/feat-x': ['asdf reshim'] },
  aiLaunchCommands: { claude: '', copilot: '' },
  lastOpenSessions: [],
  tabOrder: [],
  activeSessionId: null,
};

function resetStore(): void {
  useConfigStore.setState({
    config: {
      configVersion: 3,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: '',
      workspaceRoot: null,
      worktreeRoots: [],
      prelaunchCommands: [],
      worktreePrelaunchCommands: {},
      aiLaunchCommands: { claude: '', copilot: '' },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
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
    const diff: PartialAppConfig = {
      prelaunchCommands: ['nvm use'],
      // explicit undefined must be stripped
      instructionSetsDir: undefined,
    };

    await useConfigStore.getState().set(diff);

    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    const [arg] = bridgeMock.configSet.mock.calls[0]!;
    expect(arg).toEqual({ prelaunchCommands: ['nvm use'] });
    expect(arg).not.toHaveProperty('instructionSetsDir');
  });

  it('mirrors the diff into the local cache after a successful write', async () => {
    useConfigStore.setState({ config: { ...SAMPLE } });
    await useConfigStore.getState().set({ prelaunchCommands: ['echo hi'] });

    expect(useConfigStore.getState().config.prelaunchCommands).toEqual(['echo hi']);
    // Untouched fields survive.
    expect(useConfigStore.getState().config.instructionSetsDir).toBe('/cfg/instr');
  });

  it('deep-merges defaultInstructionSets so a partial patch keeps the other tool', async () => {
    useConfigStore.setState({ config: { ...SAMPLE } });
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

    await expect(
      useConfigStore.getState().set({ instructionSetsDir: 'rel/path' }),
    ).rejects.toMatchObject({ code: 'InvalidPath' });

    // Cache untouched.
    expect(useConfigStore.getState().config.instructionSetsDir).toBe('/cfg/instr');
  });
});
