import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { WorktreePrepBanner } from './WorktreePrepBanner';
import { useWorktreePrepStore } from '@/store/worktree-prep-store';
import type { WorktreePrepEvent } from '@/types/arborist';

let dispatch: ((ev: WorktreePrepEvent) => void) | undefined;

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  bridgeMock.onWorktreePrep.mockImplementationOnce((cb) => {
    dispatch = cb;
    return Promise.resolve(() => {});
  });
  useWorktreePrepStore.getState()._resetForTest();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  useWorktreePrepStore.getState()._resetForTest();
  dispatch = undefined;
});

async function subscribe(): Promise<void> {
  await useWorktreePrepStore.getState().subscribe();
}

describe('WorktreePrepBanner', () => {
  it('renders nothing when there is no prep state', async () => {
    await subscribe();
    const { container } = render(<WorktreePrepBanner />);
    expect(container.firstChild).toBeNull();
  });

  it('shows the running banner while a prep is in flight', async () => {
    await subscribe();
    render(<WorktreePrepBanner />);
    act(() => {
      dispatch!({
        kind: 'started',
        prepId: 'p1',
        worktreePath: '/repo/.worktrees/feature-x',
        logPath: '/data/p1.log',
        command: 'npm install',
        startedAt: 1700000000,
      });
    });
    expect(screen.getByTestId('worktree-prep-banner-running')).toHaveTextContent(/feature-x/);
  });

  it('shows a success banner after a successful exit and auto-dismisses it', async () => {
    await subscribe();
    render(<WorktreePrepBanner />);
    act(() => {
      dispatch!({
        kind: 'exited',
        prepId: 'p1',
        worktreePath: '/repo/.worktrees/feature-x',
        logPath: '/data/p1.log',
        exitCode: 0,
        errorMessage: null,
        startedAt: 1700000000,
        finishedAt: 1700000005,
      });
    });
    expect(screen.getByTestId('worktree-prep-banner-success')).toHaveTextContent(/feature-x/);
    act(() => {
      vi.advanceTimersByTime(5_000);
    });
    expect(screen.queryByTestId('worktree-prep-banner-success')).toBeNull();
  });

  it('shows a sticky failure banner with View log + Dismiss', async () => {
    await subscribe();
    render(<WorktreePrepBanner />);
    act(() => {
      dispatch!({
        kind: 'exited',
        prepId: 'p1',
        worktreePath: '/repo/.worktrees/feature-x',
        logPath: '/data/p1.log',
        exitCode: 2,
        errorMessage: null,
        startedAt: 1700000000,
        finishedAt: 1700000005,
      });
    });
    const banner = screen.getByTestId('worktree-prep-banner-failure');
    expect(banner).toHaveTextContent(/feature-x/);
    expect(banner).toHaveTextContent(/exit code 2/);

    fireEvent.click(screen.getByRole('button', { name: /view log/i }));
    expect(bridgeMock.worktreePrepOpenLog).toHaveBeenCalledWith({ logPath: '/data/p1.log' });

    // Failure banner does NOT auto-dismiss.
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(screen.getByTestId('worktree-prep-banner-failure')).toBeInTheDocument();

    // Dismiss button removes it.
    fireEvent.click(screen.getByRole('button', { name: /dismiss prep failure/i }));
    expect(screen.queryByTestId('worktree-prep-banner-failure')).toBeNull();
  });

  it('shows reason "process was signalled" when exitCode is null without an errorMessage', async () => {
    await subscribe();
    render(<WorktreePrepBanner />);
    act(() => {
      dispatch!({
        kind: 'exited',
        prepId: 'p1',
        worktreePath: '/repo/.worktrees/x',
        logPath: '/data/p1.log',
        exitCode: null,
        errorMessage: null,
        startedAt: 1,
        finishedAt: 2,
      });
    });
    expect(screen.getByTestId('worktree-prep-banner-failure')).toHaveTextContent(/process was signalled/);
  });

  it('shows the spawn errorMessage as the reason when present', async () => {
    await subscribe();
    render(<WorktreePrepBanner />);
    act(() => {
      dispatch!({
        kind: 'exited',
        prepId: 'p1',
        worktreePath: '/repo/.worktrees/x',
        logPath: '/data/p1.log',
        exitCode: null,
        errorMessage: 'spawn failed: ENOENT',
        startedAt: 1,
        finishedAt: 2,
      });
    });
    expect(screen.getByTestId('worktree-prep-banner-failure')).toHaveTextContent(/spawn failed: ENOENT/);
  });

  it('aggregates the running label when multiple preps are in flight', async () => {
    await subscribe();
    render(<WorktreePrepBanner />);
    act(() => {
      dispatch!({
        kind: 'started',
        prepId: 'p1',
        worktreePath: '/r/a',
        logPath: '/data/p1.log',
        command: 'a',
        startedAt: 1,
      });
      dispatch!({
        kind: 'started',
        prepId: 'p2',
        worktreePath: '/r/b',
        logPath: '/data/p2.log',
        command: 'b',
        startedAt: 1,
      });
    });
    expect(screen.getByTestId('worktree-prep-banner-running')).toHaveTextContent(/Worktree prep running… \(2\)/);
  });
});
