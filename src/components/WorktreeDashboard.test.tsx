import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { PluginRegistryProvider, createBuiltinsRegistry, createRegistry, type DashboardWidgetPlugin } from '@/plugins';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { WorktreeTab, WorktreeTabId } from '@/types/arborist';

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

function widget(id: string, order: number): DashboardWidgetPlugin {
  return {
    id,
    displayName: id,
    order,
    Component: () => <div data-testid={`widget-${id}`}>{id}</div>,
  };
}

function renderDashboard(registry = createBuiltinsRegistry()): ReturnType<typeof render> {
  return render(
    <PluginRegistryProvider registry={registry}>
      <WorktreeDashboard tabId={TAB_ID} />
    </PluginRegistryProvider>,
  );
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: false });
  useSessionStore.setState({ sessions: [], activeId: undefined, isHydrated: false });
  useConfigStore.setState((s) => ({
    config: { ...s.config, pluginSettings: { ai: {}, customProcess: {}, dashboardWidget: {} } },
    status: 'ready',
    error: null,
  }));
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('WorktreeDashboard', () => {
  it('renders the worktree name, path, and branch', async () => {
    useWorktreeTabStore.setState({ tabs: [tab({ branch: 'feature-x' })] });

    renderDashboard();
    await act(async () => {});

    expect(screen.getByRole('heading', { name: 'feature-x' })).toBeInTheDocument();
    expect(screen.getByText('/repo/feature-x')).toBeInTheDocument();
    expect(screen.getByText(/on branch feature-x/i)).toBeInTheDocument();
  });

  it('clicking Launch Claude calls sessionCreate with this worktree', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
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

    renderDashboard();
    await act(async () => {});

    fireEvent.click(screen.getByTestId('worktree-dashboard-launch-claude'));

    await waitFor(() =>
      expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(
        expect.objectContaining({
          tool: 'claude',
          worktreePath: '/repo/feature-x',
        }),
      ),
    );
    await act(async () => {});
  });

  it('clicking Launch Copilot calls sessionCreate with copilot tool', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
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

    renderDashboard();
    await act(async () => {});

    fireEvent.click(screen.getByTestId('worktree-dashboard-launch-copilot'));

    await waitFor(() =>
      expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(
        expect.objectContaining({
          tool: 'copilot',
          worktreePath: '/repo/feature-x',
        }),
      ),
    );
    await act(async () => {});
  });

  it('shows a launch error when sessionCreate rejects (e.g., codex missing)', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    bridgeMock.sessionCreate.mockRejectedValueOnce(new Error('spawn_command failed: program not found'));

    renderDashboard();
    await act(async () => {});

    fireEvent.click(screen.getByTestId('worktree-dashboard-launch-codex'));

    expect(await screen.findByTestId('worktree-dashboard-launch-error')).toHaveTextContent(/launch codex failed/i);
  });

  it('mounts widgets in registry order', () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    const registry = createRegistry();
    registry.registerWidget(widget('third', 30));
    registry.registerWidget(widget('first', 10));
    registry.registerWidget(widget('second', 20));

    renderDashboard(registry);

    expect(screen.getAllByTestId(/^widget-/).map((n) => n.textContent)).toEqual(['first', 'second', 'third']);
  });

  it('hides disabled AI plugins and dashboard widgets', async () => {
    useWorktreeTabStore.setState({ tabs: [tab()] });
    useConfigStore.setState((s) => ({
      config: {
        ...s.config,
        pluginSettings: {
          ai: { copilot: { enabled: false, settings: {} } },
          customProcess: {},
          dashboardWidget: { 'ai-usage': { enabled: false, settings: {} } },
        },
      },
    }));

    renderDashboard();
    await act(async () => {});

    expect(screen.getByTestId('worktree-dashboard-launch-claude')).toBeInTheDocument();
    expect(screen.queryByTestId('worktree-dashboard-launch-copilot')).toBeNull();
    expect(screen.queryByTestId('worktree-dashboard-ai-usage')).toBeNull();
  });

  it('renders nothing when the tab has been removed underneath us', () => {
    const { container } = renderDashboard();
    expect(container.firstChild).toBeNull();
  });
});
