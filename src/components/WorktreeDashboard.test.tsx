import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { SessionView, WorktreeTab, WorktreeTabId } from '@/types/arborist';

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

    render(<WorktreeDashboard tabId={TAB_ID} />);

    expect(screen.getByRole('heading', { name: 'feature-x' })).toBeInTheDocument();
    expect(screen.getByText('/repo/feature-x')).toBeInTheDocument();
    expect(screen.getByText(/on branch feature-x/i)).toBeInTheDocument();
    // 2 sessions match this worktree path; the third does not.
    expect(screen.getByText(/2 agents in this worktree/i)).toBeInTheDocument();
  });

  it('shows the empty-state hint when no children exist', () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });

    render(<WorktreeDashboard tabId={TAB_ID} />);

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

    render(<WorktreeDashboard tabId={TAB_ID} />);

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

    render(<WorktreeDashboard tabId={TAB_ID} />);

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
    const { container } = render(<WorktreeDashboard tabId={TAB_ID} />);
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

    render(<WorktreeDashboard tabId={TAB_ID} />);

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-count-staged')).toHaveTextContent('1');
    });
    expect(screen.getByTestId('worktree-dashboard-count-unstaged')).toHaveTextContent('2');
    expect(screen.getByTestId('worktree-dashboard-count-untracked')).toHaveTextContent('3');
    expect(screen.getByTestId('worktree-dashboard-count-conflicted')).toHaveTextContent('0');
    expect(screen.getByTestId('worktree-dashboard-ahead-behind')).toHaveTextContent(/↑2.*↓1/);
    expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledWith('/repo/feature-x');
  });

  it('clicking Refresh re-invokes worktreeGitStatus', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });

    render(<WorktreeDashboard tabId={TAB_ID} />);

    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(1);
    });

    fireEvent.click(screen.getByTestId('worktree-dashboard-git-refresh'));

    await waitFor(() => {
      expect(bridgeMock.worktreeGitStatus).toHaveBeenCalledTimes(2);
    });
  });

  it('surfaces an inline error when git status fails', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    bridgeMock.worktreeGitStatus.mockRejectedValueOnce(new Error('git not found'));

    render(<WorktreeDashboard tabId={TAB_ID} />);

    await waitFor(() => {
      expect(screen.getByTestId('worktree-dashboard-git-error')).toHaveTextContent(/git not found/);
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

    render(<WorktreeDashboard tabId={TAB_ID} />);

    expect(screen.getByTestId('worktree-dashboard-input-tokens')).toHaveTextContent('300');
    expect(screen.getByTestId('worktree-dashboard-output-tokens')).toHaveTextContent('125');
  });
});
