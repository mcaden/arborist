// Behavioural tests for `SubTerminalView`.
//
// Status (running / exited / error) is communicated by the sidebar
// indicator. `SubTerminalView` deliberately renders no overlay so the
// final scrollback (e.g. an `exit` echo or an error message) stays
// visible. These tests pin that contract — particularly the regression
// the user reported where typing `exit` in pwsh produced a modal
// dialog instead of leaving the pane intact.

import { render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

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

describe('SubTerminalView', () => {
  it('renders no overlay or status dialog while the sub-session is running', () => {
    const sub = makeSub({ id: id('01'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.queryByRole('button')).not.toBeInTheDocument();
  });

  it('renders no overlay when the sub-session has exited (sidebar dot is the indicator)', () => {
    const sub = makeSub({ id: id('02'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.queryByRole('button')).not.toBeInTheDocument();
  });

  it('renders no overlay when the sub-session is in error state', () => {
    const sub = makeSub({ id: id('03'), status: 'error', pid: undefined });
    useSubSessionStore.setState({
      subSessions: [sub],
      statusMessages: { [sub.id]: 'spawn failed: ENOENT' },
    });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.queryByRole('button')).not.toBeInTheDocument();
  });
});
