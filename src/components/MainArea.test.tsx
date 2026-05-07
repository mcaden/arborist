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
import type { SessionView, SubSession, SubSessionId } from '@/types/arborist';

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

beforeEach(() => {
  resetBridgeMocks();
  mockTerminals.length = 0;
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    pendingClose: undefined,
    isHydrated: false,
  });
  useSubSessionStore.setState({
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    isHydrated: false,
  });
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
    useSessionStore.setState({
      sessions: [makeSession('s1'), makeSession('s2')],
      activeId: 's1',
      isHydrated: true,
    });
    render(<MainArea />);
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    expect(panels).toHaveLength(2);
    // Inactive panels are hidden via `visibility: hidden` (not `display:
    // none`) so xterm.js's char-size measurement and fitAddon stay sane
    // across tab switches.
    const wrappers = panels.map((p) => p.parentElement!);
    expect(wrappers[0]!.style.visibility).not.toBe('hidden');
    expect(wrappers[1]!.style.visibility).toBe('hidden');
    expect(wrappers[0]!.getAttribute('aria-hidden')).toBe('false');
    expect(wrappers[1]!.getAttribute('aria-hidden')).toBe('true');
  });

  it('switching active session does NOT unmount the previously-active TerminalView', () => {
    useSessionStore.setState({
      sessions: [makeSession('s1'), makeSession('s2')],
      activeId: 's1',
      isHydrated: true,
    });
    const { rerender } = render(<MainArea />);
    expect(mockTerminals).toHaveLength(2);
    const initialDisposeCalls = mockTerminals.map((t) => t.dispose.mock.calls.length);

    act(() => {
      useSessionStore.setState({ activeId: 's2' });
    });
    rerender(<MainArea />);

    // Both terminals still exist; neither was disposed by the switch.
    expect(mockTerminals).toHaveLength(2);
    expect(mockTerminals[0]!.dispose.mock.calls.length).toBe(initialDisposeCalls[0]);
    expect(mockTerminals[1]!.dispose.mock.calls.length).toBe(initialDisposeCalls[1]);
  });

  it('disposes the terminal when its session is removed from the store', () => {
    useSessionStore.setState({
      sessions: [makeSession('s1'), makeSession('s2')],
      activeId: 's1',
      isHydrated: true,
    });
    render(<MainArea />);
    expect(mockTerminals).toHaveLength(2);
    act(() => {
      useSessionStore.setState({ sessions: [makeSession('s1')] });
    });
    expect(mockTerminals[1]!.dispose).toHaveBeenCalled();
    expect(mockTerminals[0]!.dispose).not.toHaveBeenCalled();
  });

  // ------- sub-session swap behaviour (Phase 5) ----------------------

  function makeSub(id: string, parentId: string, overrides: Partial<SubSession> = {}): SubSession {
    return {
      id: id as SubSessionId,
      parentSessionId: parentId as SessionView['id'],
      defId: 'shell',
      kind: 'terminal',
      label: id,
      status: 'running',
      composedCommand: 'sh -i',
      createdAt: 0,
      ...overrides,
    } as SubSession;
  }

  it('mounts every terminal sub-session even when not visible (T-03)', () => {
    useSessionStore.setState({
      sessions: [makeSession('s1')],
      activeId: 's1',
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('sub-1', 's1'), makeSub('sub-2', 's1')],
      activeByParent: {},
      statusMessages: {},
      isHydrated: true,
    });
    render(<MainArea />);
    // 1 parent + 2 sub-session terminals.
    expect(mockTerminals).toHaveLength(3);
  });

  it('terminal sub-session swaps the visible viewport for its parent', () => {
    useSessionStore.setState({
      sessions: [makeSession('s1')],
      activeId: 's1',
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('sub-1', 's1')],
      activeByParent: { s1: 'sub-1' as SubSessionId },
      statusMessages: {},
      isHydrated: true,
    });
    render(<MainArea />);
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    // Parent + one terminal sub.
    expect(panels).toHaveLength(2);
    const wrappers = panels.map((p) => p.parentElement!);
    // Parent is hidden, sub is visible.
    expect(wrappers[0]!.style.visibility).toBe('hidden');
    expect(wrappers[1]!.style.visibility).not.toBe('hidden');
    expect(wrappers[1]!.getAttribute('aria-hidden')).toBe('false');
  });

  it('application sub-session does NOT swap the viewport', () => {
    useSessionStore.setState({
      sessions: [makeSession('s1')],
      activeId: 's1',
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('app-1', 's1', { kind: 'application' })],
      activeByParent: { s1: 'app-1' as SubSessionId },
      statusMessages: {},
      isHydrated: true,
    });
    render(<MainArea />);
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    // Only the parent panel — application subs aren't mounted in MainArea.
    expect(panels).toHaveLength(1);
    expect(panels[0]!.parentElement!.style.visibility).not.toBe('hidden');
  });

  it('inactive parent: its sub-sessions stay hidden even when active there', () => {
    useSessionStore.setState({
      sessions: [makeSession('s1'), makeSession('s2')],
      activeId: 's2',
      isHydrated: true,
    });
    useSubSessionStore.setState({
      subSessions: [makeSub('sub-1', 's1')],
      // s1 has a sub active, but s1 is NOT the active session.
      activeByParent: { s1: 'sub-1' as SubSessionId },
      statusMessages: {},
      isHydrated: true,
    });
    render(<MainArea />);
    const panels = screen.getAllByRole('tabpanel', { hidden: true });
    // 2 parents + 1 terminal sub.
    expect(panels).toHaveLength(3);
    const wrappers = panels.map((p) => p.parentElement!);
    // Only s2 (the active parent) is visible.
    const visible = wrappers.filter((w) => w.style.visibility !== 'hidden');
    expect(visible).toHaveLength(1);
  });
});
