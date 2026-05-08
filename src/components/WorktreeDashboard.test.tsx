import { fireEvent, render, screen } from '@testing-library/react';
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
});
