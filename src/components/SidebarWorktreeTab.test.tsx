import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { SessionView, WorktreeTab, WorktreeTabId } from '@/types/arborist';

import { SidebarWorktreeTab } from './SidebarWorktreeTab';

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

function session(id: string, status: SessionView['status'] = 'running'): SessionView {
  return {
    id,
    tool: 'claude',
    worktreePath: '/repo/feature-x',
    worktreeName: 'feature-x',
    label: id,
    instructionSetId: 'default-claude',
    status,
    createdAt: 0,
    tabIndex: 0,
  };
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useWorktreeTabStore.setState({ tabs: [tab({ branch: 'feat/x' })], activeId: null, pendingClose: undefined, isHydrated: false });
  useSessionStore.setState({ sessions: [], activeId: undefined, isHydrated: false });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('SidebarWorktreeTab', () => {
  const noop = () => {};

  it('renders the worktree name and branch', () => {
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={noop} />);

    expect(screen.getByText('feature-x')).toBeInTheDocument();
    expect(screen.getByText('feat/x')).toBeInTheDocument();
  });

  it('marks the active worktree header for assistive tech', () => {
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={true} onOpenContextMenu={noop} />);

    expect(screen.getByTestId(`worktree-tab-${TAB_ID}`)).toHaveAttribute('aria-current', 'page');
  });

  it('does not mark inactive worktree headers as current', () => {
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={noop} />);

    expect(screen.getByTestId(`worktree-tab-${TAB_ID}`)).not.toHaveAttribute('aria-current');
  });

  it('clicking the header focuses the worktree tab and clears activeChildId', () => {
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={noop} />);

    fireEvent.click(screen.getByTestId(`worktree-tab-${TAB_ID}`));

    expect(bridgeMock.worktreeTabFocus).toHaveBeenCalledWith({ id: TAB_ID });
    // Clearing activeChildId omits the field from the args (matches the bridge contract).
    expect(bridgeMock.worktreeTabSetActiveChild).toHaveBeenCalledWith({ id: TAB_ID });
  });

  it('clicking close requests close (sets pendingClose) without immediately invoking the bridge', () => {
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={noop} />);

    fireEvent.click(screen.getByTestId(`worktree-tab-close-${TAB_ID}`));

    expect(useWorktreeTabStore.getState().pendingClose).toBe(TAB_ID);
    expect(bridgeMock.worktreeTabClose).not.toHaveBeenCalled();
  });

  it('clicking the ⋮ button invokes the onOpenContextMenu callback with viewport coordinates', () => {
    const onOpen = vi.fn();
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={onOpen} />);

    fireEvent.click(screen.getByTestId(`worktree-tab-menu-${TAB_ID}`));

    expect(onOpen).toHaveBeenCalledWith(
      TAB_ID,
      expect.objectContaining({ x: expect.any(Number) as number, y: expect.any(Number) as number }),
      expect.any(HTMLElement),
    );
  });

  it('right-click does NOT open the context menu (moved to ⋮ button, issue #49)', () => {
    const onOpen = vi.fn();
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={onOpen} />);

    fireEvent.contextMenu(screen.getByTestId(`worktree-tab-${TAB_ID}`), { clientX: 12, clientY: 34 });

    expect(onOpen).not.toHaveBeenCalled();
  });

  it('does not render a rolled-up status icon even when a child reports an error', () => {
    useSessionStore.setState({
      sessions: [session('s1', 'error')],
      isHydrated: true,
    });

    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={noop} />);

    expect(screen.queryByRole('img', { name: /error/i })).toBeNull();
  });

  it('does not render a status icon when no child status is present', () => {
    render(<SidebarWorktreeTab tabId={TAB_ID} isActive={false} onOpenContextMenu={noop} />);

    expect(screen.queryByRole('img', { name: /error|attention|awaiting|running|working|thinking|starting/i })).toBeNull();
  });
});
