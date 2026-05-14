// Behavioural tests for `TabContextMenu`. Tauri bridge is mocked
// wholesale (per project convention) so no real `invoke()` runs.

import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import type { SessionId, SessionView } from '@/types/arborist';

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

function seed(): void {
  useSessionStore.setState({
    sessions: [makeView()],
    activeId: PARENT,
    isHydrated: true,
    statusMessages: {},
    hasUnread: {},
    activity: {},
    metrics: {},
    lastTurnEndAt: {},
    lastTurnDurationMs: {},
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('TabContextMenu', () => {
  it('renders Restart and Close items only', () => {
    seed();
    const onClose = vi.fn();

    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);

    const menu = screen.getByTestId('tab-context-menu');
    expect(menu).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /restart/i })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /close/i })).toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: /launch/i })).toBeNull();
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
    act(() => {});
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

  it('Restart invokes session_restart with the parent id and closes', async () => {
    seed();
    const onClose = vi.fn();

    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /restart/i }));

    await waitFor(() => expect(bridgeMock.sessionRestart).toHaveBeenCalledWith(expect.objectContaining({ sessionId: PARENT })));
    const call = bridgeMock.sessionRestart.mock.calls[0]?.[0];
    expect(call?.cols).toBeGreaterThan(0);
    expect(call?.rows).toBeGreaterThan(0);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Close immediately invokes session close (no confirmation dialog)', () => {
    seed();
    const onClose = vi.fn();

    render(<TabContextMenu parentSessionId={PARENT} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /close/i }));

    expect(bridgeMock.sessionClose).toHaveBeenCalledWith({ sessionId: PARENT, deleteWorktree: false });
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
