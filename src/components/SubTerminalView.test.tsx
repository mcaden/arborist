// Behavioural tests for `SubTerminalView`.
//
// Status (running / exited / error):
//   * running / starting → no in-pane overlay; the xterm host fills the
//     viewport.
//   * exited / error → an in-pane "Relaunch / Close" overlay is rendered
//     ON TOP of the still-mounted xterm host. The terminal scrollback is
//     cleared on the entry edge so the user doesn't see a stale prompt
//     looking suspiciously alive. Sidebar dot still goes grey.
//
// The `clear()` API is exercised on TWO transitions to defend against a
// PTY race where a stray byte from the just-killed child arrives after
// the new spawn begins (rubber-duck critique). Once on
// running→exited/error, once on exited/error→starting.

import { render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

const clearMock = vi.fn();

vi.mock('@/hooks/use-terminal', () => ({
  useSubTerminal: () => ({
    attach: vi.fn(),
    detach: vi.fn(),
    focus: vi.fn(),
    refit: vi.fn(),
    clear: clearMock,
  }),
}));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SessionId, SubSession, SubSessionId } from '@/types/arborist';

import { SubTerminalView } from './SubTerminalView';

const PARENT: SessionId = '00000000-0000-0000-0000-000000000a01' as SessionId;

// Override type permits `pid: undefined` explicitly even though
// `SubSession.pid?: number` rejects it under
// `exactOptionalPropertyTypes: true`. Tests need to construct
// already-exited rows where pid is gone.
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

// Drop `pid` from a SubSession and apply a new status. Used by transition
// tests to model an exit without leaving a stale PID. We can't write
// `{...sub, status: 'exited', pid: undefined}` literally because
// `exactOptionalPropertyTypes: true` rejects an explicit-undefined for
// `pid?: number`.
function withStatus(sub: SubSession, status: SubSession['status']): SubSession {
  const { pid: _drop, ...rest } = sub;
  return { ...rest, status } as SubSession;
}

function id(suffix: string): SubSessionId {
  return ('22222222-2222-2222-2222-2222222222' + suffix) as SubSessionId;
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  clearMock.mockReset();
  useSubSessionStore.setState({
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    pendingClose: undefined,
    isHydrated: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('SubTerminalView', () => {
  it('renders no overlay while the sub-session is running', () => {
    const sub = makeSub({ id: id('01'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /relaunch/i })).not.toBeInTheDocument();
  });

  it('shows the relaunch overlay when the sub-session has exited', () => {
    const sub = makeSub({ id: id('02'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByRole('dialog', { name: /sub-session ended/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /relaunch/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^close$/i })).toBeInTheDocument();
  });

  it('shows the relaunch overlay when the sub-session is in error state (with error-flavoured copy)', () => {
    const sub = makeSub({ id: id('03'), status: 'error', pid: undefined });
    useSubSessionStore.setState({
      subSessions: [sub],
      statusMessages: { [sub.id]: 'spawn failed: ENOENT' },
    });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByRole('dialog', { name: /sub-session ended/i })).toBeInTheDocument();
    expect(screen.getByText(/sub-session ended with an error/i)).toBeInTheDocument();
  });

  it('clears the terminal once on the running → exited transition', () => {
    const sub = makeSub({ id: id('04'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    const { rerender } = render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(clearMock).not.toHaveBeenCalled();
    useSubSessionStore.setState({
      subSessions: [withStatus(sub, 'exited')],
    });
    rerender(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(clearMock).toHaveBeenCalledTimes(1);
  });

  it('clears the terminal again on the exited → starting transition (defends against late stray bytes)', () => {
    const sub = makeSub({ id: id('05'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    const { rerender } = render(<SubTerminalView subSessionId={sub.id} isActive />);
    // Initial entry into exited counts as the "into-exited" edge — first
    // mount with exited status fires the clear.
    expect(clearMock).toHaveBeenCalledTimes(1);
    useSubSessionStore.setState({
      subSessions: [withStatus(sub, 'starting')],
    });
    rerender(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(clearMock).toHaveBeenCalledTimes(2);
  });

  it('clicking Relaunch in the overlay calls subSessionRelaunch with the sub id', () => {
    const sub = makeSub({ id: id('06'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    screen.getByRole('button', { name: /relaunch/i }).click();
    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
  });

  it('clicking Close in the overlay calls subSessionClose with default tabOnly intent', () => {
    const sub = makeSub({ id: id('07'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    screen.getByRole('button', { name: /^close$/i }).click();
    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
  });
});
