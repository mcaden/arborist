// Behavioural tests for `SubTerminalView` — focused on the exit overlay
// (the surface most likely to regress; xterm DOM lifecycle is exercised
// indirectly through `use-terminal.test.tsx`).

import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

// Stub the terminal hook — these tests don't care about xterm internals,
// only about the overlay behaviour for exited / errored sub-sessions.
vi.mock('@/hooks/use-terminal', () => ({
  useSubTerminal: () => ({
    attach: vi.fn(),
    detach: vi.fn(),
    focus: vi.fn(),
    refit: vi.fn(),
  }),
}));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SessionId, SubSession, SubSessionId } from '@/types/arborist';

import { SubTerminalView } from './SubTerminalView';

const PARENT: SessionId = '00000000-0000-0000-0000-000000000a01' as SessionId;

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
  return ('22222222-2222-2222-2222-2222222222' + suffix) as SubSessionId;
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSubSessionStore.setState({
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    isHydrated: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('SubTerminalView exit overlay', () => {
  it('hides the overlay while the sub-session is running', () => {
    const sub = makeSub({ id: id('01'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /relaunch/i })).not.toBeInTheDocument();
  });

  it('renders Relaunch and Close buttons when the sub-session has exited', () => {
    const sub = makeSub({ id: id('02'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByRole('alert')).toBeInTheDocument();
    expect(screen.getByText(/sub-session exited/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /relaunch/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^close$/i })).toBeInTheDocument();
  });

  it('renders the error overlay copy when status is error', () => {
    const sub = makeSub({ id: id('03'), status: 'error', pid: undefined });
    useSubSessionStore.setState({
      subSessions: [sub],
      statusMessages: { [sub.id]: 'spawn failed: ENOENT' },
    });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.getByText(/sub-session encountered an error/i)).toBeInTheDocument();
    expect(screen.getByTestId('sub-terminal-status-message')).toHaveTextContent(
      'spawn failed: ENOENT',
    );
    expect(screen.getByRole('button', { name: /relaunch/i })).toBeInTheDocument();
  });

  it('Relaunch button invokes subSessionRelaunch with the sub-session id', () => {
    const sub = makeSub({ id: id('04'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    fireEvent.click(screen.getByRole('button', { name: /relaunch/i }));
    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
    expect(bridgeMock.subSessionClose).not.toHaveBeenCalled();
  });

  it('Close button still invokes subSessionClose (kept as secondary action)', () => {
    const sub = makeSub({ id: id('05'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    fireEvent.click(screen.getByRole('button', { name: /^close$/i }));
    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id);
    expect(bridgeMock.subSessionRelaunch).not.toHaveBeenCalled();
  });
});
