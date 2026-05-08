import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { WorktreeTab, WorktreeTabId } from '@/types/arborist';

import { WorktreeTabContextMenu } from './WorktreeTabContextMenu';

const TAB_ID = 'tab-feature-x' as WorktreeTabId;

function tab(): WorktreeTab {
  return {
    id: TAB_ID,
    path: '/repo/feature-x',
    name: 'feature-x',
    label: 'feature-x',
    tabIndex: 0,
    iconId: 1,
  };
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useConfigStore.setState((s) => ({
    config: { ...s.config, customProcesses: [] },
    status: 'ready',
    error: null,
  }));
  useWorktreeTabStore.setState({ tabs: [tab()], activeId: TAB_ID, isHydrated: true });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('WorktreeTabContextMenu', () => {
  const noop = () => {};

  it('renders Launch Claude, Launch Copilot, and Close items', () => {
    render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    expect(screen.getByTestId('worktree-tab-context-menu-launch-claude')).toBeInTheDocument();
    expect(screen.getByTestId('worktree-tab-context-menu-launch-copilot')).toBeInTheDocument();
    expect(screen.getByTestId('worktree-tab-context-menu-close')).toBeInTheDocument();
  });

  it('Close calls worktreeTabClose for this tab and dismisses the menu', () => {
    const onClose = vi.fn();
    render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-close'));
    expect(bridgeMock.worktreeTabClose).toHaveBeenCalledWith({ id: TAB_ID });
    expect(onClose).toHaveBeenCalled();
  });

  it('clicking Launch Claude calls sessionCreate with this worktree path', () => {
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
    render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-launch-claude'));
    expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(expect.objectContaining({ tool: 'claude', worktreePath: '/repo/feature-x' }));
  });

  it('clicking Launch Copilot calls sessionCreate with copilot tool', () => {
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
    render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-launch-copilot'));
    expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(expect.objectContaining({ tool: 'copilot', worktreePath: '/repo/feature-x' }));
  });

  it('returns null when the tab has been removed from the store', () => {
    const onClose = vi.fn();
    useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: true });
    const { container } = render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    expect(container.firstChild).toBeNull();
    expect(onClose).toHaveBeenCalled();
  });

  it('closes when the tab is removed while the menu is mounted', async () => {
    const onClose = vi.fn();
    render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: true });
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it('Escape closes the menu', () => {
    const onClose = vi.fn();
    render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.keyDown(screen.getByTestId('worktree-tab-context-menu'), { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('estimates menu height from item count so long custom-process menus stay within the viewport', () => {
    const originalInnerHeight = window.innerHeight;
    const originalInnerWidth = window.innerWidth;
    Object.defineProperty(window, 'innerHeight', { configurable: true, value: 240 });
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 400 });
    useConfigStore.setState((s) => ({
      config: {
        ...s.config,
        customProcesses: Array.from({ length: 6 }, (_, idx) => ({
          id: `proc-${idx}`,
          name: `Process ${idx}`,
          kind: 'terminal',
          command: `echo ${idx}`,
          enabled: true,
        })),
      },
      status: 'ready',
      error: null,
    }));

    try {
      render(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 390, y: 230 }} onClose={noop} />);

      const menu = screen.getByTestId('worktree-tab-context-menu');
      expect(menu).toHaveStyle({ left: '176px', top: '4px' });
    } finally {
      Object.defineProperty(window, 'innerHeight', { configurable: true, value: originalInnerHeight });
      Object.defineProperty(window, 'innerWidth', { configurable: true, value: originalInnerWidth });
    }
  });
});
