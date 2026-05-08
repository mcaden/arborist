import { act, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

const mockTerminals: Array<{
  open: ReturnType<typeof vi.fn>;
  write: ReturnType<typeof vi.fn>;
  onData: ReturnType<typeof vi.fn>;
  focus: ReturnType<typeof vi.fn>;
  dispose: ReturnType<typeof vi.fn>;
  loadAddon: ReturnType<typeof vi.fn>;
  paste: ReturnType<typeof vi.fn>;
  cols: number;
  rows: number;
}> = [];

vi.mock('@xterm/xterm', () => {
  const Terminal = vi.fn().mockImplementation(() => {
    const inst = {
      open: vi.fn(),
      write: vi.fn(),
      onData: vi.fn(),
      focus: vi.fn(),
      dispose: vi.fn(),
      loadAddon: vi.fn(),
      paste: vi.fn(),
      cols: 80,
      rows: 24,
    };
    mockTerminals.push(inst);
    return inst;
  });
  return { Terminal };
});

vi.mock('@xterm/addon-fit', () => ({
  FitAddon: vi.fn().mockImplementation(() => ({ fit: vi.fn(), dispose: vi.fn() })),
}));

import { MainArea } from './MainArea';
import { __resetTerminalRegistryForTests } from '@/hooks/use-terminal';
import { resetBridgeMocks } from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { ChildId, SessionView, SubSession, SubSessionId, WorktreeTab, WorktreeTabId } from '@/types/arborist';

function makeSession(id: string, label = id): SessionView {
  return {
    id,
    tool: 'claude',
    worktreePath: `/wt/${id}`,
    worktreeName: id,
    label,
    instructionSetId: 'default',
    status: 'running',
    createdAt: 0,
    tabIndex: 0,
  };
}

function tabFor(session: SessionView, activeChildId?: ChildId): WorktreeTab {
  const t: WorktreeTab = {
    id: `tab-${session.id}` as WorktreeTabId,
    path: session.worktreePath,
    name: session.worktreeName,
    label: session.worktreeName,
    tabIndex: 0,
    iconId: 1,
  };
  if (activeChildId) t.activeChildId = activeChildId;
  return t;
}

function seedWorktreeTabs(sessions: SessionView[], activeId: string | undefined): void {
  const tabs = sessions.map((s) => tabFor(s, s.id === activeId ? ({ kind: 'session', id: s.id } as ChildId) : undefined));
  const activeTab = tabs.find((t) => sessions.find((s) => s.id === activeId)?.worktreePath === t.path);
  useWorktreeTabStore.setState({
    tabs,
    activeId: activeTab ? activeTab.id : null,
    isHydrated: true,
  });
}

beforeEach(() => {
  resetBridgeMocks();
  mockTerminals.length = 0;
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    isHydrated: false,
  });
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
    isHydrated: false,
  });
  useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: false });
});

afterEach(() => {
  __resetTerminalRegistryForTests();
});

describe('MainArea', () => {
  it('renders empty state when no sessions exist', () => {
    render(<MainArea />);
    expect(screen.getByText(/no session selected/i)).toBeInTheDocument();
  });

  it('renders one TerminalView per session, only the active one visible', () => {
    const sessions = [makeSession('s1'), makeSession('s2')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    seedWorktreeTabs(sessions, 's1');

    render(<MainArea />);

    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels).toHaveLength(2);
    const wrappers = panels.map((p) => p.parentElement!);
    expect(wrappers[0]!.style.visibility).not.toBe('hidden');
    expect(wrappers[1]!.style.visibility).toBe('hidden');
    expect(wrappers[0]!.getAttribute('aria-hidden')).toBe('false');
    expect(wrappers[1]!.getAttribute('aria-hidden')).toBe('true');
  });

  it('switching active session does NOT unmount the previously-active TerminalView', () => {
    const sessions = [makeSession('s1'), makeSession('s2')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    seedWorktreeTabs(sessions, 's1');

    const { rerender } = render(<MainArea />);
    expect(mockTerminals).toHaveLength(2);
    const initialDisposeCalls = mockTerminals.map((t) => t.dispose.mock.calls.length);

    act(() => {
      useSessionStore.setState({ activeId: 's2' });
      seedWorktreeTabs(sessions, 's2');
    });
    rerender(<MainArea />);

    expect(mockTerminals).toHaveLength(2);
    expect(mockTerminals[0]!.dispose.mock.calls.length).toBe(initialDisposeCalls[0]);
    expect(mockTerminals[1]!.dispose.mock.calls.length).toBe(initialDisposeCalls[1]);
  });

  it('disposes the terminal when its session is removed from the store', () => {
    const sessions = [makeSession('s1'), makeSession('s2')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    seedWorktreeTabs(sessions, 's1');

    render(<MainArea />);
    expect(mockTerminals).toHaveLength(2);

    act(() => {
      const next = [makeSession('s1')];
      useSessionStore.setState({ sessions: next });
      seedWorktreeTabs(next, 's1');
    });

    expect(mockTerminals[1]!.dispose).toHaveBeenCalled();
    expect(mockTerminals[0]!.dispose).not.toHaveBeenCalled();
  });

  function makeSub(id: string, parentId: string, overrides: Partial<SubSession> = {}): SubSession {
    return {
      id: id as SubSessionId,
      parentWorktreeTabId: `tab-${parentId}` as WorktreeTabId,
      defId: 'shell',
      kind: 'terminal',
      label: id,
      status: 'running',
      composedCommand: 'sh -i',
      createdAt: 0,
      ...overrides,
    };
  }

  it('mounts every terminal sub-session even when not visible (T-03)', () => {
    const sessions = [makeSession('s1')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    seedWorktreeTabs(sessions, 's1');
    useSubSessionStore.setState({
      subSessions: [makeSub('sub-1', 's1'), makeSub('sub-2', 's1')],
      statusMessages: {},
      isHydrated: true,
    });

    render(<MainArea />);

    expect(mockTerminals).toHaveLength(3);
  });

  it('terminal sub-session swaps the visible viewport for its parent', () => {
    const sessions = [makeSession('s1')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    useWorktreeTabStore.setState({
      tabs: [tabFor(sessions[0]!, { kind: 'subSession', id: 'sub-1' as SubSessionId })],
      activeId: 'tab-s1' as WorktreeTabId,
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('sub-1', 's1')],
      statusMessages: {},
      isHydrated: true,
    });

    render(<MainArea />);

    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels).toHaveLength(2);
    const wrappers = panels.map((p) => p.parentElement!);
    expect(wrappers[0]!.style.visibility).toBe('hidden');
    expect(wrappers[1]!.style.visibility).not.toBe('hidden');
    expect(wrappers[1]!.getAttribute('aria-hidden')).toBe('false');
  });

  it('application sub-session does NOT swap the viewport', () => {
    const sessions = [makeSession('s1')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    useWorktreeTabStore.setState({
      tabs: [tabFor(sessions[0]!, { kind: 'session', id: 's1' })],
      activeId: 'tab-s1' as WorktreeTabId,
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('app-1', 's1', { kind: 'application' })],
      statusMessages: {},
      isHydrated: true,
    });

    render(<MainArea />);

    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels).toHaveLength(1);
    expect(panels[0]!.parentElement!.style.visibility).not.toBe('hidden');
  });

  it('shows the dashboard instead of a blank pane when activeChildId points at a missing session', () => {
    const sessions = [makeSession('s1')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    useWorktreeTabStore.setState({
      tabs: [tabFor(sessions[0]!, { kind: 'session', id: 'missing' })],
      activeId: 'tab-s1' as WorktreeTabId,
      isHydrated: true,
    });

    render(<MainArea />);

    expect(screen.getByTestId('worktree-dashboard')).toBeInTheDocument();
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels[0]!.parentElement!.style.visibility).toBe('hidden');
  });

  it('shows the dashboard instead of a blank pane when activeChildId points at an application sub-session', () => {
    const sessions = [makeSession('s1')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    useWorktreeTabStore.setState({
      tabs: [tabFor(sessions[0]!, { kind: 'subSession', id: 'app-1' as SubSessionId })],
      activeId: 'tab-s1' as WorktreeTabId,
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('app-1', 's1', { kind: 'application' })],
      statusMessages: {},
      isHydrated: true,
    });

    render(<MainArea />);

    expect(screen.getByTestId('worktree-dashboard')).toBeInTheDocument();
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels[0]!.parentElement!.style.visibility).toBe('hidden');
  });

  it('shows the first worktree dashboard instead of a blank pane when no active worktree tab is set', () => {
    const sessions = [makeSession('s1')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    useWorktreeTabStore.setState({
      tabs: [tabFor(sessions[0]!)],
      activeId: null,
      isHydrated: true,
    });

    render(<MainArea />);

    expect(screen.getByTestId('worktree-dashboard')).toBeInTheDocument();
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels[0]!.parentElement!.style.visibility).toBe('hidden');
  });

  it('shows the first worktree dashboard instead of a blank pane when the active worktree tab is stale', () => {
    const sessions = [makeSession('s1')];
    useSessionStore.setState({ sessions, activeId: 's1', isHydrated: true });
    useWorktreeTabStore.setState({
      tabs: [tabFor(sessions[0]!)],
      activeId: 'missing-tab' as WorktreeTabId,
      isHydrated: true,
    });

    render(<MainArea />);

    expect(screen.getByTestId('worktree-dashboard')).toBeInTheDocument();
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels[0]!.parentElement!.style.visibility).toBe('hidden');
  });

  it('shows the active session terminal when sessions exist but no worktree tabs are available', () => {
    const sessions = [makeSession('s1'), makeSession('s2')];
    useSessionStore.setState({ sessions, activeId: 's2', isHydrated: true });
    useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: true });

    render(<MainArea />);

    expect(screen.queryByTestId('worktree-dashboard')).not.toBeInTheDocument();
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels).toHaveLength(2);
    const wrappers = panels.map((p) => p.parentElement!);
    expect(wrappers[0]!.style.visibility).toBe('hidden');
    expect(wrappers[1]!.style.visibility).not.toBe('hidden');
  });

  it('inactive parent: its sub-sessions stay hidden even when active there', () => {
    const sessions = [makeSession('s1'), makeSession('s2')];
    useSessionStore.setState({ sessions, activeId: 's2', isHydrated: true });
    useWorktreeTabStore.setState({
      tabs: [tabFor(sessions[0]!, { kind: 'subSession', id: 'sub-1' as SubSessionId }), tabFor(sessions[1]!, { kind: 'session', id: 's2' })],
      activeId: 'tab-s2' as WorktreeTabId,
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('sub-1', 's1')],
      statusMessages: {},
      isHydrated: true,
    });

    render(<MainArea />);

    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels).toHaveLength(3);
    const wrappers = panels.map((p) => p.parentElement!);
    const visible = wrappers.filter((w) => w.style.visibility !== 'hidden');
    expect(visible).toHaveLength(1);
  });
});
