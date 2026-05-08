// Behavioural tests for `SidebarSubTab`.

import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SubSession, SubSessionId } from '@/types/arborist';

import { SidebarSubTab } from './SidebarSubTab';

const TAB_PARENT = 'tab-parent';
const TAB_PARENT_OTHER = 'tab-parent-other';

type SubOverrides = Partial<Omit<SubSession, 'id' | 'pid'>> &
  Pick<SubSession, 'id'> & {
    pid?: number | undefined;
  };

function makeSub(overrides: SubOverrides): SubSession {
  const { pid, ...restOverrides } = overrides;
  const sub: SubSession = {
    parentWorktreeTabId: TAB_PARENT,
    defId: 'shell',
    kind: 'terminal',
    label: 'Shell',
    status: 'running',
    composedCommand: 'sh -i',
    createdAt: 0,
    ...restOverrides,
  };
  if (pid !== undefined) sub.pid = pid;
  return sub;
}

function id(suffix: string): SubSessionId {
  return ('11111111-1111-1111-1111-1111111111' + suffix) as SubSessionId;
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
    pendingClose: undefined,
    isHydrated: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('SidebarSubTab', () => {
  it('renders label, status dot, and a close button', () => {
    const sub = makeSub({ id: id('01'), label: 'My Shell' });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);

    expect(screen.getByText('My Shell')).toBeInTheDocument();
    expect(screen.getByTestId('sub-status-running')).toBeInTheDocument();
    expect(screen.getByLabelText(/close sub-session/i)).toBeInTheDocument();
  });

  it('returns null for unknown sub-session id', () => {
    const { container } = render(<SidebarSubTab subSessionId={id('99')} />);
    expect(container.firstChild).toBeNull();
  });

  it('click activates focus via subSessionFocus', () => {
    const sub = makeSub({ id: id('02') });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));

    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });

  it('clicking a sub-tab focuses that sub-session using its stored worktree owner', () => {
    const sub = makeSub({ id: id('03'), parentWorktreeTabId: TAB_PARENT_OTHER });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));

    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
    expect(bridgeMock.sessionFocus).not.toHaveBeenCalled();
  });

  it('close button on a terminal sub-tab invokes subSessionClose with default intent and stops propagation', () => {
    const sub = makeSub({ id: id('04') });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByLabelText(/close sub-session/i));

    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
    expect(bridgeMock.subSessionFocus).not.toHaveBeenCalled();
  });

  it('close button on a running application sub-tab opens the close-confirm dialog (no immediate close)', () => {
    const sub = makeSub({
      id: id('04b'),
      kind: 'application',
      status: 'running',
      pid: 42,
    });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByLabelText(/close sub-session/i));

    expect(bridgeMock.subSessionClose).not.toHaveBeenCalled();
    expect(useSubSessionStore.getState().pendingClose).toBe(sub.id);
  });

  it('close button on an already-exited application sub-tab closes immediately (no dialog)', () => {
    const sub = makeSub({
      id: id('04c'),
      kind: 'application',
      status: 'exited',
      pid: undefined,
    });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByLabelText(/close sub-session/i));

    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
    expect(useSubSessionStore.getState().pendingClose).toBeUndefined();
  });

  it('uses role=button (not role=tab) so it stays out of the sidebar tablist roving-tabindex model', () => {
    const sub = makeSub({ id: id('05a') });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);

    expect(screen.queryByRole('tab')).toBeNull();
    expect(screen.getByRole('button', { name: sub.label })).toBeInTheDocument();
  });

  it('clicking a greyed exited application sub-tab triggers relaunch (Phase 7)', () => {
    const sub = makeSub({ id: id('07'), kind: 'application', status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));

    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
    expect(bridgeMock.subSessionFocus).not.toHaveBeenCalled();
  });

  it('clicking a greyed errored application sub-tab triggers relaunch (Phase 7)', () => {
    const sub = makeSub({ id: id('08'), kind: 'application', status: 'error', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));

    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
  });

  it('clicking a running application sub-tab focuses, does NOT relaunch (Phase 7)', () => {
    const sub = makeSub({ id: id('09'), kind: 'application', status: 'running', pid: 42 });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));

    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
    expect(bridgeMock.subSessionRelaunch).not.toHaveBeenCalled();
  });

  it('clicking a greyed exited terminal sub-tab focuses (does NOT relaunch — relaunch lives in the in-pane overlay)', () => {
    const sub = makeSub({ id: id('0a'), kind: 'terminal', status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));

    expect(bridgeMock.subSessionRelaunch).not.toHaveBeenCalled();
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });

  it('clicking a greyed errored terminal sub-tab focuses (does NOT relaunch)', () => {
    const sub = makeSub({ id: id('0b'), kind: 'terminal', status: 'error', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SidebarSubTab subSessionId={sub.id} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));

    expect(bridgeMock.subSessionRelaunch).not.toHaveBeenCalled();
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });
});
