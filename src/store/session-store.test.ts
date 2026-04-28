// Behavioural tests for `useSessionStore`. The Tauri bridge is mocked
// wholesale (see `tauri-bridge.mock.ts`) so no real `invoke()` runs.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { SessionStatusEvent, SessionView } from '@/types/arborist';

import { useSessionStore } from './session-store';

function makeView(overrides: Partial<SessionView> & Pick<SessionView, 'id'>): SessionView {
  return {
    tool: 'claude',
    worktreePath: `/repo/${overrides.id}`,
    worktreeName: overrides.id,
    label: overrides.id,
    instructionSetId: 'default-claude',
    status: 'running',
    createdAt: 1_700_000_000_000,
    tabIndex: 0,
    ...overrides,
  };
}

function resetStore(): void {
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    pendingClose: undefined,
    isHydrated: false,
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  resetStore();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('hydrate', () => {
  it('calls session_list, populates sessions, and flips isHydrated', async () => {
    const views = [makeView({ id: 'a', tabIndex: 0 }), makeView({ id: 'b', tabIndex: 1 })];
    bridgeMock.sessionList.mockResolvedValueOnce(views);

    await useSessionStore.getState().actions.hydrate();

    expect(bridgeMock.sessionList).toHaveBeenCalledTimes(1);
    expect(useSessionStore.getState().sessions).toEqual(views);
    expect(useSessionStore.getState().isHydrated).toBe(true);
  });

  it('is idempotent — a second hydrate replaces the cache', async () => {
    bridgeMock.sessionList.mockResolvedValueOnce([makeView({ id: 'a' })]);
    await useSessionStore.getState().actions.hydrate();
    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['a']);

    bridgeMock.sessionList.mockResolvedValueOnce([makeView({ id: 'b' }), makeView({ id: 'c' })]);
    await useSessionStore.getState().actions.hydrate();

    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['b', 'c']);
    expect(useSessionStore.getState().isHydrated).toBe(true);
  });
});

describe('create', () => {
  it('forwards args to session_create, appends, and sets activeId', async () => {
    const created = makeView({ id: 'new', tabIndex: 2 });
    bridgeMock.sessionCreate.mockResolvedValueOnce(created);
    useSessionStore.setState({
      sessions: [makeView({ id: 'old', tabIndex: 0 })],
      activeId: 'old',
    });

    const view = await useSessionStore
      .getState()
      .actions.create({ tool: 'claude', worktreePath: '/repo/new', instructionSetId: 'd' });

    expect(view).toEqual(created);
    expect(bridgeMock.sessionCreate).toHaveBeenCalledWith({
      tool: 'claude',
      worktreePath: '/repo/new',
      instructionSetId: 'd',
    });
    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['old', 'new']);
    expect(useSessionStore.getState().activeId).toBe('new');
  });
});

describe('close', () => {
  it('calls session_close and removes the session', async () => {
    const a = makeView({ id: 'a' });
    const b = makeView({ id: 'b' });
    useSessionStore.setState({ sessions: [a, b], activeId: 'a' });

    await useSessionStore.getState().actions.close('a');

    expect(bridgeMock.sessionClose).toHaveBeenCalledWith({ sessionId: 'a' });
    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['b']);
  });

  it('picks the right-neighbour as activeId when closing the active tab', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' }), makeView({ id: 'c' })],
      activeId: 'b',
    });

    await useSessionStore.getState().actions.close('b');

    expect(useSessionStore.getState().activeId).toBe('c');
  });

  it('falls back to the left-neighbour when the active tab is rightmost', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'b',
    });

    await useSessionStore.getState().actions.close('b');

    expect(useSessionStore.getState().activeId).toBe('a');
  });

  it('clears activeId when closing the only session', async () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'lone' })], activeId: 'lone' });

    await useSessionStore.getState().actions.close('lone');

    expect(useSessionStore.getState().sessions).toEqual([]);
    expect(useSessionStore.getState().activeId).toBeUndefined();
  });

  it('leaves activeId alone when closing a non-active tab', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
    });

    await useSessionStore.getState().actions.close('b');

    expect(useSessionStore.getState().activeId).toBe('a');
  });

  it('clears pendingClose when the targeted session is closed', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      pendingClose: 'a',
    });

    await useSessionStore.getState().actions.close('a');

    expect(useSessionStore.getState().pendingClose).toBeUndefined();
  });
});

describe('focus', () => {
  it('sets activeId synchronously and calls session_focus', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
    });

    const promise = useSessionStore.getState().actions.focus('b');
    expect(useSessionStore.getState().activeId).toBe('b');
    await promise;

    expect(bridgeMock.sessionFocus).toHaveBeenCalledWith({ sessionId: 'b' });
  });

  it('keeps activeId set and warns when session_focus rejects', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
    });
    bridgeMock.sessionFocus.mockRejectedValueOnce(new Error('not found'));

    await useSessionStore.getState().actions.focus('b');

    expect(useSessionStore.getState().activeId).toBe('b');
    expect(warn).toHaveBeenCalledTimes(1);
    warn.mockRestore();
  });
});

describe('reorder', () => {
  it('reorders local sessions and calls config_set with only tabOrder', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' }), makeView({ id: 'c' })],
    });

    await useSessionStore.getState().actions.reorder(['c', 'a', 'b']);

    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['c', 'a', 'b']);
    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    const [arg] = bridgeMock.configSet.mock.calls[0]!;
    expect(arg).toEqual({ tabOrder: ['c', 'a', 'b'] });
    expect(Object.keys(arg)).toEqual(['tabOrder']);
  });
});

describe('requestClose / cancelClose', () => {
  it('toggles pendingClose without any bridge call', () => {
    useSessionStore.getState().actions.requestClose('x');
    expect(useSessionStore.getState().pendingClose).toBe('x');

    useSessionStore.getState().actions.cancelClose();
    expect(useSessionStore.getState().pendingClose).toBeUndefined();

    expect(bridgeMock.sessionClose).not.toHaveBeenCalled();
    expect(bridgeMock.configSet).not.toHaveBeenCalled();
  });
});

describe('applyStatus', () => {
  it('updates the matching session status', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a', status: 'starting' })],
    });

    const evt: SessionStatusEvent = { sessionId: 'a', status: 'running' };
    useSessionStore.getState().actions.applyStatus(evt);

    expect(useSessionStore.getState().sessions[0]!.status).toBe('running');
  });

  it('drops events for unknown sessions without throwing or mutating', () => {
    const debug = vi.spyOn(console, 'debug').mockImplementation(() => {});
    const before = [makeView({ id: 'a' })];
    useSessionStore.setState({ sessions: before });

    expect(() =>
      useSessionStore.getState().actions.applyStatus({
        sessionId: 'ghost',
        status: 'exited',
      }),
    ).not.toThrow();

    expect(useSessionStore.getState().sessions).toBe(before);
    expect(debug).toHaveBeenCalledTimes(1);
    debug.mockRestore();
  });

  it('records statusMessages when the event carries a message', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a', status: 'starting' })],
      statusMessages: {},
    });

    useSessionStore.getState().actions.applyStatus({
      sessionId: 'a',
      status: 'error',
      message: 'Worktree path no longer exists: /tmp/gone',
    });

    expect(useSessionStore.getState().statusMessages['a']).toBe(
      'Worktree path no longer exists: /tmp/gone',
    );
  });

  it('clears prior statusMessages when a later event omits message', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a', status: 'error' })],
      statusMessages: { a: 'old error' },
    });

    useSessionStore.getState().actions.applyStatus({
      sessionId: 'a',
      status: 'running',
    });

    expect(useSessionStore.getState().statusMessages['a']).toBeUndefined();
  });
});
