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

describe('git-status widget — pull request section', () => {
  it('renders the PR number, state, passing checks, and opens the link externally', async () => {
    bridgeMock.worktreePrInfo.mockResolvedValueOnce({
      provider: 'github',
      cliAvailable: true,
      repoWebUrl: 'https://github.com/o/r',
      pr: { number: 42, url: 'https://github.com/o/r/pull/42', title: 'Add thing', state: 'open', checks: 'passing', isDraft: false },
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-pr-link')).toHaveTextContent('#42');
    });
    expect(screen.getByTestId('worktree-dashboard-pr-state')).toHaveTextContent('Open');
    expect(screen.getByTestId('worktree-dashboard-pr-checks')).toHaveTextContent(/passing/i);
    expect(screen.getByText('Add thing')).toBeInTheDocument();
    expect(bridgeMock.worktreePrInfo).toHaveBeenCalledWith('/repo/feature-x');

    fireEvent.click(screen.getByTestId('worktree-dashboard-pr-link'));
    expect(bridgeMock.openExternalUrl).toHaveBeenCalledWith('https://github.com/o/r/pull/42');
  });

  it('renders a draft state with a failing checks badge', async () => {
    bridgeMock.worktreePrInfo.mockResolvedValueOnce({
      provider: 'gitlab',
      cliAvailable: true,
      pr: { number: 7, url: 'u', state: 'draft', checks: 'failing', isDraft: true },
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-pr-state')).toHaveTextContent('Draft');
    });
    expect(screen.getByTestId('worktree-dashboard-pr-checks')).toHaveTextContent(/failing/i);
  });

  it('shows a note and a repository link when no PR exists for the branch', async () => {
    bridgeMock.worktreePrInfo.mockResolvedValueOnce({
      provider: 'github',
      cliAvailable: true,
      repoWebUrl: 'https://github.com/o/r',
      note: 'No pull request found for this branch.',
    });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-pr-note')).toHaveTextContent(/No pull request found/);
    });
    fireEvent.click(screen.getByTestId('worktree-dashboard-pr-repo-link'));
    expect(bridgeMock.openExternalUrl).toHaveBeenCalledWith('https://github.com/o/r');
  });

  it('hides the PR section entirely for an unrecognised host', async () => {
    bridgeMock.worktreePrInfo.mockResolvedValueOnce({ provider: 'unknown', cliAvailable: false, note: 'Unrecognised git host.' });

    renderWidget();

    await waitFor(() => {
      expect(bridgeMock.worktreePrInfo).toHaveBeenCalled();
    });
    expect(screen.queryByTestId('worktree-dashboard-pr')).toBeNull();
    expect(screen.queryByTestId('worktree-dashboard-pr-note')).toBeNull();
  });

  it('surfaces an inline error when the PR lookup rejects', async () => {
    bridgeMock.worktreePrInfo.mockRejectedValueOnce(new Error('bridge denied'));

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-pr-error')).toHaveTextContent(/bridge denied/);
    });
  });

  it('surfaces the structured backend error even when provider is unknown', async () => {
    // The always-Ok backend resolves with `error` set (and provider defaulting to `unknown`) for e.g. an invalid worktree path. This must not be
    // swallowed by the unrecognised-host short-circuit, which otherwise renders nothing.
    bridgeMock.worktreePrInfo.mockResolvedValueOnce({ provider: 'unknown', cliAvailable: false, error: 'invalid worktree path: nope' });

    renderWidget();

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-pr-error')).toHaveTextContent(/invalid worktree path/);
    });
    expect(screen.queryByTestId('worktree-dashboard-pr')).toBeNull();
  });

  it('clicking Refresh also re-invokes worktreePrInfo', async () => {
    renderWidget();

    await waitFor(() => {
      expect(bridgeMock.worktreePrInfo).toHaveBeenCalledTimes(1);
    });

    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));
    await waitFor(() => {
      expect(bridgeMock.worktreePrInfo).toHaveBeenCalledTimes(2);
    });
  });
});
