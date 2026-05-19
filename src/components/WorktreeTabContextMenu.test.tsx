import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { PluginRegistryProvider } from '@/plugins';
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

function renderWithPlugins(ui: JSX.Element): ReturnType<typeof render> {
  return render(<PluginRegistryProvider>{ui}</PluginRegistryProvider>);
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useConfigStore.setState((s) => ({
    config: { ...s.config, customProcesses: [], pluginSettings: { ai: {}, customProcess: {}, dashboardWidget: {} } },
    status: 'ready',
    error: null,
  }));
  useWorktreeTabStore.setState({ tabs: [tab()], activeId: TAB_ID, pendingClose: undefined, isHydrated: true });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('WorktreeTabContextMenu', () => {
  const noop = () => {};

  it('renders Launch Claude, Launch Copilot, and Close items', () => {
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    expect(screen.getByTestId('worktree-tab-context-menu-launch-claude')).toBeInTheDocument();
    expect(screen.getByTestId('worktree-tab-context-menu-launch-copilot')).toBeInTheDocument();
    expect(screen.getByTestId('worktree-tab-context-menu-close')).toBeInTheDocument();
  });

  it('omits disabled AI launch items', () => {
    useConfigStore.setState((s) => ({
      config: {
        ...s.config,
        pluginSettings: {
          ai: { copilot: { enabled: false, settings: {} } },
          customProcess: {},
          dashboardWidget: {},
        },
      },
    }));

    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);

    expect(screen.getByTestId('worktree-tab-context-menu-launch-claude')).toBeInTheDocument();
    expect(screen.queryByTestId('worktree-tab-context-menu-launch-copilot')).toBeNull();
  });

  it('Close requests close (sets pendingClose) and dismisses the menu', () => {
    const onClose = vi.fn();
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-close'));
    expect(useWorktreeTabStore.getState().pendingClose).toBe(TAB_ID);
    expect(bridgeMock.worktreeTabClose).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });

  it('clicking Launch Claude calls sessionCreate with this worktree path', async () => {
    bridgeMock.sessionCreate.mockResolvedValueOnce({
      id: 'new-id',
      tool: 'claude',
      worktreePath: '/repo/feature-x',
      worktreeName: 'feature-x',
      label: 'feature-x',
      status: 'starting',
      createdAt: 0,
      tabIndex: 0,
    });
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-launch-claude'));
    await waitFor(() =>
      expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(expect.objectContaining({ tool: 'claude', worktreePath: '/repo/feature-x' })),
    );
    await act(async () => {});
  });

  it('clicking Launch Copilot calls sessionCreate with copilot tool', async () => {
    bridgeMock.sessionCreate.mockResolvedValueOnce({
      id: 'new-id',
      tool: 'copilot',
      worktreePath: '/repo/feature-x',
      worktreeName: 'feature-x',
      label: 'feature-x',
      status: 'starting',
      createdAt: 0,
      tabIndex: 0,
    });
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-launch-copilot'));
    await waitFor(() =>
      expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(expect.objectContaining({ tool: 'copilot', worktreePath: '/repo/feature-x' })),
    );
    await act(async () => {});
  });

  it('keeps menu open and surfaces an error when Launch Codex fails', async () => {
    bridgeMock.sessionCreate.mockRejectedValueOnce(new Error('spawn_command failed: program not found'));
    const onClose = vi.fn();
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);

    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-launch-codex'));

    expect(await screen.findByTestId('worktree-tab-context-menu-error')).toHaveTextContent(/launch codex failed/i);
    expect(screen.getByTestId('worktree-tab-context-menu')).not.toContainElement(screen.getByTestId('worktree-tab-context-menu-error'));
    expect(onClose).not.toHaveBeenCalled();
  });

  it('does not update state after unmount when Launch Codex rejects', async () => {
    let rejectLaunch: ((reason?: unknown) => void) | undefined;
    bridgeMock.sessionCreate.mockImplementationOnce(
      () =>
        new Promise<never>((_, reject) => {
          rejectLaunch = reject;
        }),
    );
    const consoleErrorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
    const { unmount } = renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);

    fireEvent.click(screen.getByTestId('worktree-tab-context-menu-launch-codex'));
    unmount();

    await act(async () => {
      rejectLaunch?.(new Error('spawn_command failed: program not found'));
      await Promise.resolve();
      await Promise.resolve();
    });

    const hasUnmountedWarning = consoleErrorSpy.mock.calls.some((call) =>
      call.some((arg) => typeof arg === 'string' && arg.includes("Can't perform a React state update on an unmounted component")),
    );
    expect(hasUnmountedWarning).toBe(false);
    consoleErrorSpy.mockRestore();
  });

  it('returns null when the tab has been removed from the store', () => {
    const onClose = vi.fn();
    useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: true });
    const { container } = renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    expect(container.firstChild).toBeNull();
    expect(onClose).toHaveBeenCalled();
  });

  it('closes when the tab is removed while the menu is mounted', async () => {
    const onClose = vi.fn();
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    act(() => {
      useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: true });
    });
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it('Escape closes the menu', () => {
    const onClose = vi.fn();
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.keyDown(screen.getByTestId('worktree-tab-context-menu'), { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('Enter activates the focused menu item rather than the last hovered item', async () => {
    bridgeMock.sessionCreate.mockResolvedValueOnce({
      id: 'new-id',
      tool: 'claude',
      worktreePath: '/repo/feature-x',
      worktreeName: 'feature-x',
      label: 'feature-x',
      status: 'starting',
      createdAt: 0,
      tabIndex: 0,
    });
    renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    const launchClaude = screen.getByTestId('worktree-tab-context-menu-launch-claude');
    launchClaude.focus();
    fireEvent.mouseEnter(screen.getByTestId('worktree-tab-context-menu-close'));
    fireEvent.keyDown(launchClaude, { key: 'Enter' });

    await waitFor(() => expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(expect.objectContaining({ tool: 'claude' })));
    expect(bridgeMock.worktreeTabClose).not.toHaveBeenCalled();
    await act(async () => {});
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
      renderWithPlugins(<WorktreeTabContextMenu tabId={TAB_ID} anchor={{ x: 390, y: 230 }} onClose={noop} />);

      const menu = screen.getByTestId('worktree-tab-context-menu');
      expect(menu.parentElement).toHaveStyle({ left: '176px', top: '4px' });
    } finally {
      Object.defineProperty(window, 'innerHeight', { configurable: true, value: originalInnerHeight });
      Object.defineProperty(window, 'innerWidth', { configurable: true, value: originalInnerWidth });
    }
  });
});
