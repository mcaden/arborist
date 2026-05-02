// Behavioural tests for `SubTerminalView`.
//
// Status (running / exited / error):
//   * running / starting → no exited bar; the xterm host fills the
//     viewport.
//   * exited / error → a slim non-modal status bar renders BELOW the
//     still-mounted xterm host with Relaunch / Close inline buttons.
//     Deliberately not a dialog (no modal backdrop, no role="dialog")
//     so it reads as part of the panel chrome rather than an
//     interruption. The terminal scrollback stays visible — the user
//     keeps the shell's final output (exit echo, error message, …).
//
// The `clear()` API fires only on the exited/error → starting edge so
// a relaunch starts fresh, defending against a PTY race where a stray
// byte from the just-killed child arrives after the new spawn begins
// (rubber-duck critique). It does NOT fire on the entering-exited edge
// because that would erase the very output the user wants to read.

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
  it('renders no exited bar while the sub-session is running', () => {
    const sub = makeSub({ id: id('01'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.queryByRole('status', { name: /sub-session ended/i })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /relaunch/i })).not.toBeInTheDocument();
  });

  it('shows the exited bar (non-dialog) when the sub-session has exited', () => {
    const sub = makeSub({ id: id('02'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByRole('status', { name: /sub-session ended/i })).toBeInTheDocument();
    // Deliberately not a dialog — must NOT render with role="dialog"
    // so it doesn't read as a modal interruption.
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /relaunch/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^close$/i })).toBeInTheDocument();
  });

  it('shows error-flavoured copy when the sub-session is in error state', () => {
    const sub = makeSub({ id: id('03'), label: 'Shell', status: 'error', pid: undefined });
    useSubSessionStore.setState({
      subSessions: [sub],
      statusMessages: { [sub.id]: 'spawn failed: ENOENT' },
    });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByRole('status', { name: /sub-session ended/i })).toBeInTheDocument();
    expect(screen.getByText(/ended with an error/i)).toBeInTheDocument();
  });

  it('does NOT clear the terminal on the running → exited transition (preserves final scrollback)', () => {
    const sub = makeSub({ id: id('04'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    const { rerender } = render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(clearMock).not.toHaveBeenCalled();
    useSubSessionStore.setState({
      subSessions: [withStatus(sub, 'exited')],
    });
    rerender(<SubTerminalView subSessionId={sub.id} isActive />);
    // The whole point of dropping the modal overlay is to keep the
    // shell's final output visible. clear() must NOT fire here.
    expect(clearMock).not.toHaveBeenCalled();
  });

  it('clears the terminal on the exited → starting transition (defends against late stray bytes)', () => {
    const sub = makeSub({ id: id('05'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    const { rerender } = render(<SubTerminalView subSessionId={sub.id} isActive />);
    // First mount with already-exited status must NOT fire a spurious
    // clear (prev was undefined, not exited → starting).
    expect(clearMock).not.toHaveBeenCalled();
    useSubSessionStore.setState({
      subSessions: [withStatus(sub, 'starting')],
    });
    rerender(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(clearMock).toHaveBeenCalledTimes(1);
  });

  it('clicking Relaunch in the bar calls subSessionRelaunch with the sub id', () => {
    const sub = makeSub({ id: id('06'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    screen.getByRole('button', { name: /relaunch/i }).click();
    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
  });

  it('clicking Close in the bar calls subSessionClose with default tabOnly intent', () => {
    const sub = makeSub({ id: id('07'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    screen.getByRole('button', { name: /^close$/i }).click();
    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
  });

  // --- pane content dimming on exit -------------------------------------
  // The xterm host (terminal pane content) fades to opacity-50 when the
  // sub has exited or errored. The scrollback stays readable but the
  // visual treatment makes "this shell is dead" instantly apparent —
  // paired with the slim status bar at the bottom.

  it('dims the terminal pane content (opacity-50) when the sub has exited', () => {
    const sub = makeSub({ id: id('08'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByTestId('sub-terminal-host').className).toContain('opacity-50');
  });

  it('dims the terminal pane content (opacity-50) when the sub is in error state', () => {
    const sub = makeSub({ id: id('09'), status: 'error', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByTestId('sub-terminal-host').className).toContain('opacity-50');
  });

  it('does NOT dim the terminal pane content while the sub is running', () => {
    const sub = makeSub({ id: id('0a'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByTestId('sub-terminal-host').className).not.toContain('opacity-50');
  });

  it('does NOT dim the terminal pane content while the sub is starting', () => {
    const sub = makeSub({ id: id('0b'), status: 'starting' });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByTestId('sub-terminal-host').className).not.toContain('opacity-50');
  });
});
