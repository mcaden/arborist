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

function makeSub(overrides: Partial<SubSession> & Pick<SubSession, 'id'>): SubSession {
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
    const { container } = render(
      <SidebarSubTab parentId={PARENT} subSessionId={id('99')} parentIsActive />,
    );
    expect(container.firstChild).toBeNull();
  });

  it('click activates focus via subSessionFocus', () => {
    const sub = makeSub({ id: id('02') });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByRole('tab'));
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });

  it('clicking a sub-tab whose parent is inactive also focuses the parent', () => {
    const sub = makeSub({ id: id('03'), parentSessionId: PARENT_OTHER });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT_OTHER} subSessionId={sub.id} parentIsActive={false} />);
    fireEvent.click(screen.getByRole('tab'));
    expect(bridgeMock.sessionFocus).toHaveBeenCalledWith({ sessionId: PARENT_OTHER });
    expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
  });

  it('close button invokes subSessionClose and stops propagation', () => {
    const sub = makeSub({ id: id('04') });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    fireEvent.click(screen.getByLabelText(/close sub-session/i));
    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id);
    // The parent tab's click handler must not run for the close button.
    expect(bridgeMock.subSessionFocus).not.toHaveBeenCalled();
  });

  it('shows aria-selected only when terminal sub owns the viewport', () => {
    const sub = makeSub({ id: id('05') });
    useSubSessionStore.setState({
      subSessions: [sub],
      activeByParent: { [PARENT]: sub.id },
    });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    expect(screen.getByRole('tab').getAttribute('aria-selected')).toBe('true');
  });

  it('application kind is never aria-selected by viewport rule', () => {
    const sub = makeSub({ id: id('06'), kind: 'application' });
    useSubSessionStore.setState({
      subSessions: [sub],
      activeByParent: { [PARENT]: sub.id },
    });
    render(<SidebarSubTab parentId={PARENT} subSessionId={sub.id} parentIsActive />);
    expect(screen.getByRole('tab').getAttribute('aria-selected')).toBe('false');
  });
});
