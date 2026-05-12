import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { PluginRegistryProvider } from '@/plugins';
import { useSessionStore } from '@/store/session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { SessionView, WorktreeGitStatus, WorktreeTab, WorktreeTabId } from '@/types/arborist';

import { WorktreeDashboard } from './WorktreeDashboard';

const TAB_ID = 'tab-feature-x' as WorktreeTabId;

function tab(overrides: Partial<WorktreeTab> = {}): WorktreeTab {
  return {
    id: TAB_ID,
    path: '/repo/feature-x',
    name: 'feature-x',
    label: 'feature-x',
    tabIndex: 0,
    iconId: 1,
    ...overrides,
  };
}

function session(id: string, worktreePath: string): SessionView {
  return {
    id,
    tool: 'claude',
    worktreePath,
    worktreeName: 'feature-x',
    label: id,
    instructionSetId: 'default-claude',
    status: 'running',
    createdAt: 0,
    tabIndex: 0,
  };
}

function renderWithPlugins(ui: ReactNode) {
  const rendered = render(<PluginRegistryProvider>{ui}</PluginRegistryProvider>);
  return {
    ...rendered,
    rerender: (nextUi: ReactNode) => rendered.rerender(<PluginRegistryProvider>{nextUi}</PluginRegistryProvider>),
  };
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: false });
  useSessionStore.setState({ sessions: [], activeId: undefined, isHydrated: false });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('WorktreeDashboard', () => {
  it('renders the worktree name, path, branch, and child count', () => {
    useWorktreeTabStore.setState({ tabs: [tab({ branch: 'feature-x' })] });
    useSessionStore.setState({
      sessions: [session('s1', '/repo/feature-x'), session('s2', '/repo/feature-x'), session('s3', '/other')],
      isHydrated: true,
    });

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    expect(screen.getByRole('heading', { name: 'feature-x' })).toBeInTheDocument();
    expect(screen.getByText('/repo/feature-x')).toBeInTheDocument();
    expect(screen.getByText(/on branch feature-x/i)).toBeInTheDocument();
    // 2 sessions match this worktree path; the third does not.
    expect(screen.getByText(/2 agents in this worktree/i)).toBeInTheDocument();
  });

  it('shows the empty-state hint when no children exist', () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    expect(screen.getByText(/no agents yet/i)).toBeInTheDocument();
  });

  it('clicking Launch Claude calls sessionCreate with this worktree', () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    bridgeMock.sessionCreate.mockResolvedValueOnce({
      id: 'new-id',
      tool: 'claude',
      worktreePath: '/repo/feature-x',
      worktreeName: 'feature-x',
      label: 'feature-x',
      instructionSetId: 'default-claude',
      status: 'starting',
      createdAt: 0,
      tabIndex: 0,
    });

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    fireEvent.click(screen.getByTestId('worktree-dashboard-launch-claude'));

    expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(
      expect.objectContaining({
        tool: 'claude',
        worktreePath: '/repo/feature-x',
      }),
    );
  });

  it('clicking Launch Copilot calls sessionCreate with copilot tool', () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    bridgeMock.sessionCreate.mockResolvedValueOnce({
      id: 'new-id',
      tool: 'copilot',
      worktreePath: '/repo/feature-x',
      worktreeName: 'feature-x',
      label: 'feature-x',
      instructionSetId: 'default-copilot',
      status: 'starting',
      createdAt: 0,
      tabIndex: 0,
    });

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    fireEvent.click(screen.getByTestId('worktree-dashboard-launch-copilot'));

    expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(
      expect.objectContaining({
        tool: 'copilot',
        worktreePath: '/repo/feature-x',
      }),
    );
  });

  it('renders nothing when the tab has been removed underneath us', () => {
    // No tab in the store with this id.
    const { container } = renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);
    expect(container.firstChild).toBeNull();
  });

  it('renders git status counts and ahead/behind from the backend', async () => {
    useWorktreeTabStore.setState({ tabs: [tab({ branch: 'feature-x' })] });
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

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-count-staged')).toHaveTextContent('1');
    });
    expect(screen.getByTestId('worktree-dashboard-count-unstaged')).toHaveTextContent('2');
    expect(screen.getByTestId('worktree-dashboard-count-untracked')).toHaveTextContent('3');
    expect(screen.getByTestId('worktree-dashboard-count-conflicted')).toHaveTextContent('0');
    expect(screen.getByTestId('worktree-dashboard-ahead-behind')).toHaveTextContent(/↑2.*↓1/);
    expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledWith('/repo/feature-x');
  });

  it('does not dispatch overlapping requests when a poll tick fires before the previous call resolves', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    // Make the bridge call hang on the first invocation so the in-flight guard
    // would have to suppress the next poll/click attempt.
    let resolveFirst: (v: WorktreeGitStatus) => void = () => {};
    bridgeMock.worktreeGitStatus.mockReturnValueOnce(
      new Promise((res) => {
        resolveFirst = res;
      }),
    );

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    // Initial mount fired the (still pending) first call.
    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(1);
    });

    // Click Refresh several times while the first call is still pending —
    // each click should be suppressed by the in-flight guard.
    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));
    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));
    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));

    // Give microtasks a tick to settle.
    await Promise.resolve();
    expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(1);

    // Resolve the first call and confirm a subsequent click *is* allowed to fire.
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
    useWorktreeTabStore.setState({ tabs: [tab()] });
    // Fake the 15s polling interval so the assertion below is deterministic on
    // slow CI — we only want to count: the initial mount call + the click.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

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

  it('surfaces an inline error when git status fails', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    bridgeMock.worktreeGitStatus.mockRejectedValueOnce(new Error('git not found'));

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-git-error')).toHaveTextContent(/git not found/);
    });
  });

  it('surfaces an inline error when the backend reports a structured failure', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
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

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-git-error')).toHaveTextContent(/not a git repository/);
    });
  });

  it('clears stale status when switching to a different worktree tab', async () => {
    const TAB_OTHER = 'tab-feature-y' as WorktreeTabId;
    useWorktreeTabStore.setState({
      tabs: [tab(), { id: TAB_OTHER, path: '/repo/feature-y', name: 'feature-y', label: 'feature-y', tabIndex: 1, iconId: 2 }],
    });

    // First tab returns a structured error.
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

    const { rerender } = renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);
    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-git-error')).toHaveTextContent(/feature-x/);
    });

    // Switch to TAB_OTHER. Hold the new resolution open so we can observe the
    // intermediate state — the prior tab's error must NOT be visible while the
    // new tab's call is in flight.
    let resolveSecond: (v: WorktreeGitStatus) => void = () => {};
    bridgeMock.worktreeGitStatus.mockReturnValueOnce(
      new Promise((res) => {
        resolveSecond = res;
      }),
    );

    rerender(<WorktreeDashboard tabId={TAB_OTHER} />);

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

    // Flush the state update from the resolved promise so React doesn't emit an
    // `act(...)` warning after the test exits — wait until the clean-tree
    // indicator that depends on the resolved value is on screen.
    await waitFor(() => {
      expect(screen.getByText(/Working tree clean/)).toBeInTheDocument();
    });
  });

  it("does not block the new tab's initial fetch when the previous tab still has an in-flight request", async () => {
    const TAB_OTHER = 'tab-feature-y' as WorktreeTabId;
    useWorktreeTabStore.setState({
      tabs: [tab(), { id: TAB_OTHER, path: '/repo/feature-y', name: 'feature-y', label: 'feature-y', tabIndex: 1, iconId: 2 }],
    });

    // First tab's call hangs forever — simulates a slow `git status` on a
    // huge repo. Without the tab-switch reset of `inFlightRef` /
    // `statusLoading`, the new tab's first refresh would be short-circuited
    // by the in-flight guard and its Refresh button would stay disabled.
    bridgeMock.worktreeGitStatus.mockReturnValueOnce(new Promise(() => {}));

    const { rerender } = renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);
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

    rerender(<WorktreeDashboard tabId={TAB_OTHER} />);

    // The new tab must immediately dispatch its own fetch — not wait for the
    // prior tab's still-pending request to resolve.
    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(2);
      expect(bridgeMock.worktreeGitStatus).toHaveBeenNthCalledWith(2, '/repo/feature-y');
    });

    // Wait for the new tab's resolved state to land so React doesn't emit an
    // `act(...)` warning when the test exits.
    await waitFor(() => {
      expect(screen.getByText(/Working tree clean/)).toBeInTheDocument();
    });
  });

  it('aggregates input/output tokens across sessions for this worktree only', () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    useSessionStore.setState({
      sessions: [session('s1', '/repo/feature-x'), session('s2', '/repo/feature-x'), session('s3', '/other')],
      metrics: {
        s1: { sessionId: 's1', inputTokens: 100, outputTokens: 50, model: 'claude-sonnet-4-6', observedAt: 1 },
        s2: { sessionId: 's2', inputTokens: 200, outputTokens: 75, model: 'claude-sonnet-4-6', observedAt: 2 },
        // s3 is in a different worktree — must not contribute.
        s3: { sessionId: 's3', inputTokens: 999, outputTokens: 999, observedAt: 3 },
      },
      isHydrated: true,
    });

    renderWithPlugins(<WorktreeDashboard tabId={TAB_ID} />);

    expect(screen.getByTestId('worktree-dashboard-input-tokens')).toHaveTextContent('300');
    expect(screen.getByTestId('worktree-dashboard-output-tokens')).toHaveTextContent('125');
  });
});
