import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { WorktreeGitStatus, WorktreeTabId } from '@/types/arborist';

import { gitStatusWidget } from './index';

const TAB_ID = 'tab-feature-x' as WorktreeTabId;

function renderWidget(tabPath = '/repo/feature-x'): ReturnType<typeof render> {
  const Component = gitStatusWidget.Component;
  return render(<Component tabId={TAB_ID} tabPath={tabPath} />);
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('git-status dashboard widget', () => {
  it('renders git status counts and ahead/behind from the backend', async () => {
    bridgeMock.worktreeGitStatus.mockResolvedValueOnce({
      branch: 'feature-x',
      head: 'deadbeef',
      upstream: 'origin/feature-x',
      ahead: 2,
      behind: 1,
      staged: 1,
      unstaged: 2,
      untracked: 3,
      conflicted: 0,
      files: [
        { path: 'a.ts', kind: 'staged', status: 'M.' },
        { path: 'b.ts', kind: 'unstaged', status: '.M' },
      ],
      filesTruncated: false,
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-count-staged')).toHaveTextContent('1');
    });
    expect(screen.getByTestId('worktree-dashboard-count-unstaged')).toHaveTextContent('2');
    expect(screen.getByTestId('worktree-dashboard-count-untracked')).toHaveTextContent('3');
    expect(screen.getByTestId('worktree-dashboard-count-conflicted')).toHaveTextContent('0');
    expect(screen.getByTestId('worktree-dashboard-ahead-behind')).toHaveTextContent(/↑2.*↓1/);
    expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledWith('/repo/feature-x');
  });

  it('renders source branch divergence when provided by the backend', async () => {
    bridgeMock.worktreeGitStatus.mockResolvedValueOnce({
      branch: 'feature-x',
      head: 'deadbeef',
      upstream: 'origin/feature-x',
      ahead: 0,
      behind: 0,
      sourceBranch: 'main',
      sourceAhead: 12,
      sourceBehind: 3,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByText('main')).toBeInTheDocument();
    });
    expect(screen.getByTestId('worktree-dashboard-source-divergence')).toHaveTextContent(/↑12.*↓3/);
    // Upstream ahead/behind should show "In sync" when 0/0
    expect(screen.getByTestId('worktree-dashboard-ahead-behind')).toHaveTextContent(/In sync/);
  });

  it('shows "In sync" for upstream when ahead and behind are both zero', async () => {
    bridgeMock.worktreeGitStatus.mockResolvedValueOnce({
      branch: 'feature-x',
      head: 'deadbeef',
      upstream: 'origin/feature-x',
      ahead: 0,
      behind: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-ahead-behind')).toHaveTextContent(/In sync/);
    });
  });

  it('shows "In sync" for source branch divergence when sourceAhead and sourceBehind are both zero', async () => {
    bridgeMock.worktreeGitStatus.mockResolvedValueOnce({
      branch: 'feature-x',
      head: 'deadbeef',
      upstream: 'origin/feature-x',
      ahead: 0,
      behind: 0,
      sourceBranch: 'main',
      sourceAhead: 0,
      sourceBehind: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-source-divergence')).toHaveTextContent(/In sync/);
    });
  });

  it('does not dispatch overlapping requests when a poll tick or click lands before the previous call resolves', async () => {
    let resolveFirst: (v: WorktreeGitStatus) => void = () => {};
    bridgeMock.worktreeGitStatus.mockReturnValueOnce(
      new Promise((res) => {
        resolveFirst = res;
      }),
    );

    renderWidget();

    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(1);
    });

    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));
    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));
    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));

    await Promise.resolve();
    expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(1);

    resolveFirst({
      ahead: 0,
      behind: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
    });
    await waitFor(() => {
      expect(screen.getByText(/Working tree clean/)).toBeInTheDocument();
    });

    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));
    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(2);
    });
  });

  it('clicking Refresh re-invokes worktreeGitStatus', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderWidget();

      await waitFor(() => {
        expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(1);
      });

      fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));
      await waitFor(() => {
        expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(2);
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it('surfaces an inline error when git status throws', async () => {
    bridgeMock.worktreeGitStatus.mockRejectedValueOnce(new Error('git not found'));

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-git-error')).toHaveTextContent(/git not found/);
    });
  });

  it('surfaces an inline error when the backend reports a structured failure', async () => {
    bridgeMock.worktreeGitStatus.mockResolvedValueOnce({
      ahead: 0,
      behind: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
      error: 'not a git repository',
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-git-error')).toHaveTextContent(/not a git repository/);
    });
  });

  it('clears stale status when switching to a different worktree path', async () => {
    bridgeMock.worktreeGitStatus.mockResolvedValueOnce({
      ahead: 0,
      behind: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
      error: 'feature-x: not a git repository',
    });

    const Component = gitStatusWidget.Component;
    const { rerender } = render(<Component tabId={TAB_ID} tabPath="/repo/feature-x" />);
    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-git-error')).toHaveTextContent(/feature-x/);
    });

    let resolveSecond: (v: WorktreeGitStatus) => void = () => {};
    bridgeMock.worktreeGitStatus.mockReturnValueOnce(
      new Promise((res) => {
        resolveSecond = res;
      }),
    );

    rerender(<Component tabId={TAB_ID} tabPath="/repo/feature-y" />);

    await waitFor(() => {
      expect(screen.queryByTestId('worktree-dashboard-git-error')).toBeNull();
    });

    resolveSecond({
      ahead: 0,
      behind: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
    });
    await waitFor(() => {
      expect(screen.getByText(/Working tree clean/)).toBeInTheDocument();
    });
  });

  it("does not block the new path's initial fetch when the previous path still has an in-flight request", async () => {
    bridgeMock.worktreeGitStatus.mockReturnValueOnce(new Promise(() => {}));

    const Component = gitStatusWidget.Component;
    const { rerender } = render(<Component tabId={TAB_ID} tabPath="/repo/feature-x" />);
    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(1);
      expect(bridgeMock.worktreeGitStatus).toHaveBeenNthCalledWith(1, '/repo/feature-x');
    });

    bridgeMock.worktreeGitStatus.mockResolvedValueOnce({
      ahead: 0,
      behind: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      conflicted: 0,
      files: [],
      filesTruncated: false,
    });

    rerender(<Component tabId={TAB_ID} tabPath="/repo/feature-y" />);

    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(2);
      expect(bridgeMock.worktreeGitStatus).toHaveBeenNthCalledWith(2, '/repo/feature-y');
    });
    await waitFor(() => {
      expect(screen.getByText(/Working tree clean/)).toBeInTheDocument();
    });
  });
});
