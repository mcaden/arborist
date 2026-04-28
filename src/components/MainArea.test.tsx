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
import type { SessionView } from '@/types/grove';

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
    // Only the inactive one's wrapping container should have display: none.
    const wrappers = panels.map((p) => p.parentElement!);
    expect(wrappers[0]!.style.display).not.toBe('none');
    expect(wrappers[1]!.style.display).toBe('none');
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
});
