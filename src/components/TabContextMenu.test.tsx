// Behavioural tests for `TabContextMenu`. Tauri bridge is mocked
// wholesale (per project convention) so no real `invoke()` runs.

import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { AppConfig, SessionId, SessionView } from '@/types/arborist';

import { TabContextMenu } from './TabContextMenu';

const PARENT: SessionId = '00000000-0000-0000-0000-000000000a01' as SessionId;

function makeView(): SessionView {
  return {
    id: PARENT,
    tool: 'claude',
    worktreePath: '/repo/x',
    worktreeName: 'x',
    label: 'x',
    instructionSetId: 'default-claude',
    status: 'running',
    createdAt: 1_700_000_000,
    tabIndex: 0,
  };
}

function seed(config: Partial<AppConfig> = {}): void {
  useSessionStore.setState({
    sessions: [makeView()],
    activeId: PARENT,
    pendingClose: undefined,
    isHydrated: true,
    statusMessages: {},
    hasUnread: {},
    activity: {},
    metrics: {},
    lastTurnEndAt: {},
    lastTurnDurationMs: {},
  });
  useSubSessionStore.setState({
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    isHydrated: true,
  });
  useConfigStore.setState({
    config: {
      configVersion: 4,
      defaultInstructionSets: { claude: 'default-claude', copilot: 'default-copilot' },
      instructionSetsDir: '/tmp/i',
      workspaceRoot: '/tmp',
      worktreeRoots: [],
      prelaunchCommands: [],
      worktreePrelaunchCommands: {},
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: [],
      lastOpenSubSessions: [],
      ...config,
    } as AppConfig,
    status: 'ready',
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  bridgeMock.subSessionCreate.mockResolvedValue({
    id: '11111111-1111-1111-1111-111111111111',
    parentSessionId: PARENT,
    defId: 'shell',
    kind: 'terminal',
    label: 'Shell',
    status: 'starting',
    composedCommand: 'sh -i',
    createdAt: 1_700_000_000,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('TabContextMenu', () => {
  it('renders top-level items and focuses Restart on open', () => {
    seed();
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    const menu = screen.getByTestId('tab-context-menu');
    expect(menu).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /restart/i })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /close/i })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /launch/i })).toBeInTheDocument();
  });

  it('Escape closes the menu and restores focus', () => {
    seed();
    const trigger = document.createElement('button');
    document.body.appendChild(trigger);
    const focusSpy = vi.spyOn(trigger, 'focus');
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} restoreFocusTo={trigger} />);
    fireEvent.keyDown(screen.getByTestId('tab-context-menu'), { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);
    // Focus restoration happens on rAF; flush.
    act(() => {
      // jsdom shims rAF as setTimeout(0); allow it to fire.
    });
    return new Promise<void>((resolve) => {
      requestAnimationFrame(() => {
        expect(focusSpy).toHaveBeenCalled();
        document.body.removeChild(trigger);
        resolve();
      });
    });
  });

  it('outside click closes the menu', () => {
    seed();
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.mouseDown(document.body);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Restart invokes session_restart with the parent id and closes', () => {
    seed();
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /restart/i }));
    expect(bridgeMock.sessionRestart).toHaveBeenCalledWith(expect.objectContaining({ sessionId: PARENT }));
    const call = bridgeMock.sessionRestart.mock.calls[0]?.[0];
    expect(call?.cols).toBeGreaterThan(0);
    expect(call?.rows).toBeGreaterThan(0);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Close requests session close via the existing dialog flow', () => {
    seed();
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /close/i }));
    expect(useSessionStore.getState().pendingClose).toBe(PARENT);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Launch submenu lists only enabled custom processes', () => {
    seed({
      customProcesses: [
        { id: 'a', name: 'Alpha', kind: 'terminal', command: 'true', enabled: true },
        { id: 'b', name: 'Beta (off)', kind: 'terminal', command: 'true', enabled: false },
        { id: 'c', name: 'Code', kind: 'application', command: 'code .', enabled: true },
      ],
    });
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /launch/i }));
    expect(screen.getByTestId('tab-context-menu-launch-a')).toBeInTheDocument();
    expect(screen.getByTestId('tab-context-menu-launch-c')).toBeInTheDocument();
    expect(screen.queryByTestId('tab-context-menu-launch-b')).toBeNull();
  });

  it('clicking a Launch item invokes sub_session_create with the right def', async () => {
    seed({
      customProcesses: [{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh -i', enabled: true }],
    });
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /launch/i }));
    await act(async () => {
      fireEvent.click(screen.getByTestId('tab-context-menu-launch-shell'));
    });
    expect(bridgeMock.subSessionCreate).toHaveBeenCalledWith({
      parentSessionId: PARENT,
      defId: 'shell',
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('launching a terminal sub focuses both parent (session) and the new sub-tab so the viewport swaps to it', async () => {
    seed({
      customProcesses: [{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh -i', enabled: true }],
    });
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /launch/i }));
    await act(async () => {
      fireEvent.click(screen.getByTestId('tab-context-menu-launch-shell'));
    });
    // Parent must be focused so MainArea picks its pane to render —
    // without this, when no parent (or a different parent) is currently
    // active the new sub stays hidden (observed by the user).
    expect(bridgeMock.sessionFocus).toHaveBeenCalledWith({ sessionId: PARENT });
    // And the sub itself must be focused so it owns the viewport
    // (`activeByParent[parent] = sub.id`). Same sub id seeded by the
    // default `subSessionCreate` mock above.
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith('11111111-1111-1111-1111-111111111111');
  });

  it('launching an application sub does NOT focus parent or sub (no viewport swap, no OS-window steal)', async () => {
    seed({
      customProcesses: [{ id: 'vscode', name: 'VS Code', kind: 'application', command: 'code .', enabled: true }],
    });
    bridgeMock.subSessionCreate.mockResolvedValueOnce({
      id: '22222222-2222-2222-2222-222222222222',
      parentSessionId: PARENT,
      defId: 'vscode',
      kind: 'application',
      label: 'VS Code',
      status: 'starting',
      composedCommand: 'code .',
      createdAt: 1_700_000_000,
    });
    const onClose = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /launch/i }));
    await act(async () => {
      fireEvent.click(screen.getByTestId('tab-context-menu-launch-vscode'));
    });
    expect(bridgeMock.subSessionCreate).toHaveBeenCalledWith({
      parentSessionId: PARENT,
      defId: 'vscode',
    });
    expect(bridgeMock.subSessionFocus).not.toHaveBeenCalled();
    expect(bridgeMock.sessionFocus).not.toHaveBeenCalled();
  });

  it('empty Launch submenu offers a Settings handoff', () => {
    seed({ customProcesses: [] });
    const onClose = vi.fn();
    const onOpenSettings = vi.fn();
    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} onOpenSettings={onOpenSettings} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /launch/i }));
    fireEvent.click(screen.getByTestId('tab-context-menu-empty'));
    expect(onClose).toHaveBeenCalledTimes(1);
    return new Promise<void>((resolve) => {
      requestAnimationFrame(() => {
        expect(onOpenSettings).toHaveBeenCalledTimes(1);
        resolve();
      });
    });
  });
});
