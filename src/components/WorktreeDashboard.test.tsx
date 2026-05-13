import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { PluginRegistryProvider, createBuiltinsRegistry, createRegistry, type DashboardWidgetPlugin } from '@/plugins';
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
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('WorktreeDashboard', () => {
  it('renders the worktree name, path, and branch', () => {
    useWorktreeTabStore.setState({ tabs: [tab({ branch: 'feature-x' })] });

    renderDashboard();

    expect(screen.getByRole('heading', { name: 'feature-x' })).toBeInTheDocument();
    expect(screen.getByText('/repo/feature-x')).toBeInTheDocument();
    expect(screen.getByText(/on branch feature-x/i)).toBeInTheDocument();
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

    renderDashboard();

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

    renderDashboard();

    fireEvent.click(screen.getByTestId('worktree-dashboard-launch-copilot'));

    expect(bridgeMock.sessionCreate).toHaveBeenCalledWith(
      expect.objectContaining({
        tool: 'copilot',
        worktreePath: '/repo/feature-x',
      }),
    );
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

  it('renders nothing when the tab has been removed underneath us', () => {
    const { container } = renderDashboard();
    expect(container.firstChild).toBeNull();
  });
});
