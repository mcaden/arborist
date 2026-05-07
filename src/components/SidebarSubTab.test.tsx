// Behavioural tests for `SidebarSubTab`.

import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SessionId, SubSession, SubSessionId } from '@/types/arborist';

import { SidebarSubTab } from './SidebarSubTab';

const PARENT: SessionId = '00000000-0000-0000-0000-000000000a01' as SessionId;
const PARENT_OTHER: SessionId = '00000000-0000-0000-0000-000000000b01' as SessionId;

// Override type permits `pid: undefined` explicitly even though
// `SubSession.pid?: number` rejects it under
// `exactOptionalPropertyTypes: true`. Tests need to construct
// already-exited rows where pid is gone, so we widen here and the
// `as SubSession` cast in the helper drops the `| undefined` away.
type SubOverrides = Partial<Omit<SubSession, 'id' | 'pid'>> &
  Pick<SubSession, 'id'> & {
    pid?: number | undefined;
  };

function makeSub(overrides: SubOverrides): SubSession {
  return {
    parentSessionId: PARENT,
    defId: 'shell',
    kind: 'terminal',
    label: 'Shell',
    status: 'running',
    composedCommand: 'sh -i',
    createdAt: 0,
    ...overrides,
  } as SubSession;
}

function id(suffix: string): SubSessionId {
  return ('11111111-1111-1111-1111-1111111111' + suffix) as SubSessionId;
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSubSessionStore.setState({
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    pendingClose: undefined,
    isHydrated: true,
  });
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    pendingClose: undefined,
    isHydrated: true,
    statusMessages: {},
    hasUnread: {},
    activity: {},
    metrics: {},
    lastTurnEndAt: {},
    lastTurnDurationMs: {},
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('SidebarSubTab', () => {
  it('renders label, status dot, and a close button', () => {
    const sub = makeSub({ id: id('01'), label: 'My Shell' });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    expect(screen.getByText('My Shell')).toBeInTheDocument();
    expect(screen.getByTestId('sub-status-running')).toBeInTheDocument();
    expect(screen.getByLabelText(/close sub-session/i)).toBeInTheDocument();
  });

  it('returns null for unknown sub-session id', () => {
    const { container } = render(<SidebarSubTab parentId={PARENT} subSessionId={id('99')} parentIsActive />);
    expect(container.firstChild).toBeNull();
  });

  it('click activates focus via subSessionFocus', () => {
    const sub = makeSub({ id: id('02') });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });

  it('clicking a sub-tab whose parent is inactive also focuses the parent', () => {
    const sub = makeSub({ id: id('03'), parentSessionId: PARENT_OTHER });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT_OTHER} subSessionId={sub.id} parentIsActive={false} />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));
    expect(bridgeMock.sessionFocus).toHaveBeenCalledWith({ sessionId: PARENT_OTHER });
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });

  it('close button on a terminal sub-tab invokes subSessionClose with default intent and stops propagation', () => {
    const sub = makeSub({ id: id('04') });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByLabelText(/close sub-session/i));
    // Terminal kind closes immediately (the tab IS the process); intent
    // is left undefined so the backend defaults to `tabOnly`.
    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
    // The parent tab's click handler must not run for the close button.
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
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByLabelText(/close sub-session/i));
    // Running app tab must NOT call the backend close — the dialog
    // mediates the user's choice.
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
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByLabelText(/close sub-session/i));
    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
    expect(useSubSessionStore.getState().pendingClose).toBeUndefined();
  });

  it('uses role=button (not role=tab) so it stays out of the sidebar tablist roving-tabindex model', () => {
    const sub = makeSub({ id: id('05a') });
    useSubSessionStore.setState({
      subSessions: [sub],
      activeByParent: { [PARENT]: sub.id },
    });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    // Sub-tabs live inside <ul role="group"> — using role=tab here would
    // violate the WAI-ARIA tabs pattern (tab must be a child of tablist)
    // and confuse the parent tablist's keyboard model.
    expect(screen.queryByRole('tab')).toBeNull();
    expect(screen.getByRole('button', { name: sub.label })).toBeInTheDocument();
  });

  it('shows aria-current only when terminal sub owns the viewport', () => {
    const sub = makeSub({ id: id('05') });
    useSubSessionStore.setState({
      subSessions: [sub],
      activeByParent: { [PARENT]: sub.id },
    });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    expect(screen.getByRole('button', { name: sub.label }).getAttribute('aria-current')).toBe('true');
  });

  it('application kind is never aria-current by viewport rule', () => {
    const sub = makeSub({ id: id('06'), kind: 'application' });
    useSubSessionStore.setState({
      subSessions: [sub],
      activeByParent: { [PARENT]: sub.id },
    });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    expect(screen.getByRole('button', { name: sub.label }).getAttribute('aria-current')).toBeNull();
  });

  it('clicking a greyed exited application sub-tab triggers relaunch (Phase 7)', () => {
    const sub = makeSub({ id: id('07'), kind: 'application', status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));
    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
    // Focus must NOT be called — the click is a relaunch gesture, not a
    // focus gesture.
    expect(bridgeMock.subSessionFocus).not.toHaveBeenCalled();
  });

  it('clicking a greyed errored application sub-tab triggers relaunch (Phase 7)', () => {
    const sub = makeSub({ id: id('08'), kind: 'application', status: 'error', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));
    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
  });

  it('clicking a running application sub-tab focuses, does NOT relaunch (Phase 7)', () => {
    const sub = makeSub({ id: id('09'), kind: 'application', status: 'running', pid: 42 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
    expect(bridgeMock.subSessionRelaunch).not.toHaveBeenCalled();
  });

  it('clicking a greyed exited terminal sub-tab focuses (does NOT relaunch — relaunch lives in the in-pane overlay)', () => {
    // When a terminal sub-session's PTY child exits outside the user's
    // control (process died, `exit` typed in shell, etc.) the row stays
    // put with a grey dot. Clicking it brings the (still-mounted) pane
    // back into view; SubTerminalView then renders the relaunch / close
    // overlay so the user can decide. Sidebar click does NOT spawn a
    // new process — the user explicitly opted out of automatic restart.
    const sub = makeSub({ id: id('0a'), kind: 'terminal', status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));
    expect(bridgeMock.subSessionRelaunch).not.toHaveBeenCalled();
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });

  it('clicking a greyed errored terminal sub-tab focuses (does NOT relaunch)', () => {
    const sub = makeSub({ id: id('0b'), kind: 'terminal', status: 'error', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByRole('button', { name: sub.label }));
    expect(bridgeMock.subSessionRelaunch).not.toHaveBeenCalled();
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });
});
