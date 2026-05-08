import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import {
  selectInFlightPreps,
  selectRecentCompletedPreps,
  useWorktreePrepStore,
  type PrepCompletedRecord,
  type PrepRunningRecord,
} from './worktree-prep-store';
import type { WorktreePrepEvent } from '@/types/arborist';

function startedEvent(prepId: string, overrides: Partial<Extract<WorktreePrepEvent, { kind: 'started' }>> = {}): WorktreePrepEvent {
  return {
    kind: 'started',
    prepId,
    worktreePath: '/repo/.worktrees/feature',
    logPath: '/data/worktree-prep-logs/' + prepId + '.log',
    command: 'npm install',
    startedAt: 1700000000,
    ...overrides,
  };
}

function exitedEvent(prepId: string, overrides: Partial<Extract<WorktreePrepEvent, { kind: 'exited' }>> = {}): WorktreePrepEvent {
  return {
    kind: 'exited',
    prepId,
    worktreePath: '/repo/.worktrees/feature',
    logPath: '/data/worktree-prep-logs/' + prepId + '.log',
    exitCode: 0,
    errorMessage: null,
    startedAt: 1700000000,
    finishedAt: 1700000005,
    ...overrides,
  };
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useWorktreePrepStore.getState()._resetForTest();
});

afterEach(() => {
  useWorktreePrepStore.getState()._resetForTest();
});

describe('worktree-prep-store', () => {
  it('subscribe attaches the bridge listener and returns the unlisten fn', async () => {
    const unlisten = vi.fn();
    bridgeMock.onWorktreePrep.mockResolvedValueOnce(unlisten);

    const fn = await useWorktreePrepStore.getState().subscribe();

    expect(bridgeMock.onWorktreePrep).toHaveBeenCalledTimes(1);
    expect(fn).toBe(unlisten);
  });

  it('records a started event in inFlight', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    dispatch!(startedEvent('p1'));

    const inFlight = selectInFlightPreps(useWorktreePrepStore.getState());
    expect(inFlight).toHaveLength(1);
    expect(inFlight[0]).toMatchObject<Partial<PrepRunningRecord>>({
      state: 'running',
      prepId: 'p1',
      command: 'npm install',
    });
  });

  it('moves a started prep to recent on exit (success)', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    dispatch!(startedEvent('p1'));
    dispatch!(exitedEvent('p1', { exitCode: 0 }));

    expect(selectInFlightPreps(useWorktreePrepStore.getState())).toHaveLength(0);
    const recent = selectRecentCompletedPreps(useWorktreePrepStore.getState());
    expect(recent).toHaveLength(1);
    expect(recent[0]).toMatchObject<Partial<PrepCompletedRecord>>({
      state: 'completed',
      prepId: 'p1',
      ok: true,
      exitCode: 0,
    });
  });

  it('handles exit-without-prior-started idempotently', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    dispatch!(exitedEvent('p1', { exitCode: 1 }));

    expect(selectInFlightPreps(useWorktreePrepStore.getState())).toHaveLength(0);
    const recent = selectRecentCompletedPreps(useWorktreePrepStore.getState());
    expect(recent).toHaveLength(1);
    expect(recent[0]?.ok).toBe(false);
    expect(recent[0]?.exitCode).toBe(1);
  });

  it('marks spawn failures as not-ok with errorMessage', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    dispatch!(exitedEvent('p1', { exitCode: null, errorMessage: 'spawn failed: ENOENT' }));

    const recent = selectRecentCompletedPreps(useWorktreePrepStore.getState());
    expect(recent[0]?.ok).toBe(false);
    expect(recent[0]?.errorMessage).toBe('spawn failed: ENOENT');
  });

  it('caps recent completions at 10', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    for (let i = 0; i < 15; i++) {
      dispatch!(exitedEvent('p' + i));
    }

    const recent = selectRecentCompletedPreps(useWorktreePrepStore.getState());
    expect(recent).toHaveLength(10);
    // Newest first.
    expect(recent[0]?.prepId).toBe('p14');
    expect(recent[9]?.prepId).toBe('p5');
  });

  it('dismissCompleted removes a recent record', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    dispatch!(exitedEvent('p1'));
    dispatch!(exitedEvent('p2'));

    useWorktreePrepStore.getState().dismissCompleted('p1');
    const recent = selectRecentCompletedPreps(useWorktreePrepStore.getState());
    expect(recent.map((r) => r.prepId)).toEqual(['p2']);
  });

  it('markOpenLogFailed replaces the failure reason for an existing completion', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    dispatch!(exitedEvent('p1', { exitCode: 2, errorMessage: null }));

    useWorktreePrepStore.getState().markOpenLogFailed('p1', 'open log: denied');

    const recent = selectRecentCompletedPreps(useWorktreePrepStore.getState());
    expect(recent[0]?.errorMessage).toBe('open log: denied');
    expect(recent[0]?.ok).toBe(false);
  });

  it('waitForCompletion resolves when the target prep exits', async () => {
    let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;
    bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
      dispatch = cb;
      return Promise.resolve(() => {});
    });

    await useWorktreePrepStore.getState().subscribe();
    const waited = useWorktreePrepStore.getState().waitForCompletion('p1');
    dispatch!(exitedEvent('p2'));
    dispatch!(exitedEvent('p1'));

    await expect(waited).resolves.toMatchObject({ prepId: 'p1', state: 'completed' });
  });
});
