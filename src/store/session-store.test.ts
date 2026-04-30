// Behavioural tests for `useSessionStore`. The Tauri bridge is mocked
// wholesale (see `tauri-bridge.mock.ts`) so no real `invoke()` runs.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { SessionStatusEvent, SessionView } from '@/types/arborist';

import { useSessionStore, selectDisplayStatus, type DisplayStatus } from './session-store';

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

describe('noteUnread', () => {
  it('flags a non-active session', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
    });

    useSessionStore.getState().actions.noteUnread('b');
    expect(useSessionStore.getState().hasUnread['b']).toBe(true);
  });

  it('is a no-op for the active session', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      activeId: 'a',
    });

    useSessionStore.getState().actions.noteUnread('a');
    expect(useSessionStore.getState().hasUnread).toEqual({});
  });

  it('is idempotent — second call returns the same hasUnread object', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
    });

    useSessionStore.getState().actions.noteUnread('b');
    const first = useSessionStore.getState().hasUnread;
    useSessionStore.getState().actions.noteUnread('b');
    const second = useSessionStore.getState().hasUnread;
    // Same reference — no re-render churn on repeated output bursts.
    expect(second).toBe(first);
  });

  it('ignores unknown session ids (race with close)', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      activeId: 'a',
    });

    useSessionStore.getState().actions.noteUnread('ghost');
    expect(useSessionStore.getState().hasUnread).toEqual({});
  });

  it('focus on a flagged session clears the flag', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
      hasUnread: { b: true },
    });

    await useSessionStore.getState().actions.focus('b');
    expect(useSessionStore.getState().hasUnread).toEqual({});
  });

  it('close clears the flag for the closed session', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
      hasUnread: { b: true },
    });

    await useSessionStore.getState().actions.close('b');
    expect(useSessionStore.getState().hasUnread).toEqual({});
  });
});

describe('applyActivity', () => {
  it('working transitions set state and are idempotent', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      activeId: 'a',
    });
    const { applyActivity } = useSessionStore.getState().actions;

    applyActivity({ sessionId: 'a', kind: 'working' });
    expect(useSessionStore.getState().activity['a']).toBe('working');

    const before = useSessionStore.getState().activity;
    applyActivity({ sessionId: 'a', kind: 'working' });
    // Same reference — no-op transitions don't churn the store.
    expect(useSessionStore.getState().activity).toBe(before);
  });

  it('idle does not overwrite attention', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
      activity: { b: 'attention' },
    });
    useSessionStore.getState().actions.applyActivity({ sessionId: 'b', kind: 'idle' });
    expect(useSessionStore.getState().activity['b']).toBe('attention');
  });

  it('attention is dropped if the session is already focused', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      activeId: 'a',
    });
    useSessionStore.getState().actions.applyActivity({ sessionId: 'a', kind: 'attention' });
    expect(useSessionStore.getState().activity).toEqual({});
  });

  it('attention is set for an unfocused session', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
    });
    useSessionStore.getState().actions.applyActivity({ sessionId: 'b', kind: 'attention' });
    expect(useSessionStore.getState().activity['b']).toBe('attention');
  });

  it('focus auto-clears attention', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
      activity: { b: 'attention' },
    });
    await useSessionStore.getState().actions.focus('b');
    expect(useSessionStore.getState().activity).toEqual({});
  });

  it('focus does not clear working state', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
      activity: { b: 'working' },
    });
    await useSessionStore.getState().actions.focus('b');
    expect(useSessionStore.getState().activity['b']).toBe('working');
  });

  it('close clears any activity for the closed session', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
      activity: { b: 'working' },
    });
    await useSessionStore.getState().actions.close('b');
    expect(useSessionStore.getState().activity).toEqual({});
  });

  it('events for unknown sessions are dropped', () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'a' })], activeId: 'a' });
    useSessionStore.getState().actions.applyActivity({ sessionId: 'ghost', kind: 'working' });
    expect(useSessionStore.getState().activity).toEqual({});
  });

  it('non-surfaced kinds (title, prompt) are ignored', () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'a' })], activeId: 'a' });
    useSessionStore
      .getState()
      .actions.applyActivity({ sessionId: 'a', kind: 'title', value: 'claude' });
    useSessionStore.getState().actions.applyActivity({ sessionId: 'a', kind: 'promptStart' });
    expect(useSessionStore.getState().activity).toEqual({});
  });
});

describe('applyMetrics', () => {
  it('stores the snapshot keyed by sessionId', () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'a' })] });
    useSessionStore.getState().actions.applyMetrics({
      sessionId: 'a',
      contextUsedPct: 25,
      contextTokensUsed: 50_000,
      contextTokensLimit: 200_000,
      observedAt: 1700000000,
    });
    const m = useSessionStore.getState().metrics['a'];
    expect(m).toBeDefined();
    expect(m!.contextUsedPct).toBe(25);
    expect(m!.contextTokensLimit).toBe(200_000);
  });

  it('drops metrics for unknown sessions (race with close)', () => {
    useSessionStore.getState().actions.applyMetrics({
      sessionId: 'ghost',
      contextUsedPct: 10,
      observedAt: 0,
    });
    expect(useSessionStore.getState().metrics).toEqual({});
  });

  it('does not re-set state for an unchanged snapshot (debounce)', () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'a' })] });
    const evt = {
      sessionId: 'a' as const,
      contextUsedPct: 10,
      contextTokensUsed: 1000,
      observedAt: 1700000000,
    };
    useSessionStore.getState().actions.applyMetrics(evt);
    const before = useSessionStore.getState().metrics['a'];
    useSessionStore.getState().actions.applyMetrics({ ...evt });
    const after = useSessionStore.getState().metrics['a'];
    // Same reference => no new object created => no extra render.
    expect(after).toBe(before);
  });
});

describe('applyStatus + applyMetrics interaction', () => {
  it('clears stale metrics when a session transitions back to starting (restart)', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      metrics: {
        a: { sessionId: 'a', contextUsedPct: 50, observedAt: 1 },
      },
    });
    const evt: SessionStatusEvent = { sessionId: 'a', status: 'starting' };
    useSessionStore.getState().actions.applyStatus(evt);
    expect(useSessionStore.getState().metrics['a']).toBeUndefined();
  });

  it('preserves metrics across non-starting status transitions', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      metrics: {
        a: { sessionId: 'a', contextUsedPct: 50, observedAt: 1 },
      },
    });
    const evt: SessionStatusEvent = { sessionId: 'a', status: 'running' };
    useSessionStore.getState().actions.applyStatus(evt);
    expect(useSessionStore.getState().metrics['a']).toBeDefined();
  });
});

describe('close + metrics', () => {
  it('drops metrics for the closed session', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      metrics: {
        a: { sessionId: 'a', contextUsedPct: 50, observedAt: 1 },
      },
    });
    bridgeMock.sessionClose.mockImplementation(() => Promise.resolve());
    await useSessionStore.getState().actions.close('a');
    expect(useSessionStore.getState().metrics['a']).toBeUndefined();
  });
});

describe('applyActivity (turnEnd)', () => {
  it('records the wall-clock arrival time and (when present) the source duration', () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'a' })], activeId: 'a' });
    const beforeSec = Math.floor(Date.now() / 1000);
    useSessionStore
      .getState()
      .actions.applyActivity({ sessionId: 'a', kind: 'turnEnd', durationMs: 4321 });
    const ts = useSessionStore.getState().lastTurnEndAt['a'];
    expect(ts).toBeDefined();
    expect(ts!).toBeGreaterThanOrEqual(beforeSec);
    expect(useSessionStore.getState().lastTurnDurationMs['a']).toBe(4321);
  });

  it('omits duration when the source did not provide one (Claude transcript)', () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'a' })], activeId: 'a' });
    useSessionStore
      .getState()
      .actions.applyActivity({ sessionId: 'a', kind: 'turnEnd', durationMs: null });
    expect(useSessionStore.getState().lastTurnEndAt['a']).toBeDefined();
    expect(useSessionStore.getState().lastTurnDurationMs['a']).toBeUndefined();
  });

  it('clears a stale `working` activity flag so the icon flips to awaiting', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      activeId: 'a',
      activity: { a: 'working' },
    });
    useSessionStore
      .getState()
      .actions.applyActivity({ sessionId: 'a', kind: 'turnEnd', durationMs: 100 });
    expect(useSessionStore.getState().activity['a']).toBeUndefined();
  });

  it('does not clear an `attention` flag', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' }), makeView({ id: 'b' })],
      activeId: 'a',
      activity: { b: 'attention' },
    });
    useSessionStore
      .getState()
      .actions.applyActivity({ sessionId: 'b', kind: 'turnEnd', durationMs: 100 });
    expect(useSessionStore.getState().activity['b']).toBe('attention');
  });

  it('drops events for unknown sessions', () => {
    useSessionStore.setState({ sessions: [makeView({ id: 'a' })], activeId: 'a' });
    useSessionStore
      .getState()
      .actions.applyActivity({ sessionId: 'ghost', kind: 'turnEnd', durationMs: 100 });
    expect(useSessionStore.getState().lastTurnEndAt).toEqual({});
  });

  it('a status restart clears lastTurnEndAt + lastTurnDurationMs', () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      lastTurnEndAt: { a: 1700000000 },
      lastTurnDurationMs: { a: 500 },
    });
    useSessionStore.getState().actions.applyStatus({ sessionId: 'a', status: 'starting' });
    expect(useSessionStore.getState().lastTurnEndAt['a']).toBeUndefined();
    expect(useSessionStore.getState().lastTurnDurationMs['a']).toBeUndefined();
  });

  it('close drops both lastTurnEndAt and lastTurnDurationMs', async () => {
    useSessionStore.setState({
      sessions: [makeView({ id: 'a' })],
      lastTurnEndAt: { a: 1700000000 },
      lastTurnDurationMs: { a: 500 },
    });
    bridgeMock.sessionClose.mockImplementation(() => Promise.resolve());
    await useSessionStore.getState().actions.close('a');
    expect(useSessionStore.getState().lastTurnEndAt).toEqual({});
    expect(useSessionStore.getState().lastTurnDurationMs).toEqual({});
  });
});

describe('selectDisplayStatus', () => {
  // Per-state derivation table. `nowSec` is pinned so the boot grace
  // window is deterministic.
  const NOW = 2_000_000_000;

  function setup(opts: {
    status?: SessionView['status'];
    activity?: 'working' | 'idle' | 'attention';
    lastTurnEndAt?: number;
    createdAt?: number;
  }): void {
    useSessionStore.setState({
      sessions: [
        makeView({
          id: 'a',
          status: opts.status ?? 'running',
          createdAt: opts.createdAt ?? NOW - 60,
        }),
      ],
      activity: opts.activity ? { a: opts.activity } : {},
      lastTurnEndAt: opts.lastTurnEndAt !== undefined ? { a: opts.lastTurnEndAt } : {},
    });
  }

  function status(): DisplayStatus {
    return selectDisplayStatus('a', NOW)(useSessionStore.getState());
  }

  it('error overrides everything', () => {
    setup({ status: 'error', activity: 'working', lastTurnEndAt: NOW - 1 });
    expect(status()).toBe('error');
  });

  it('starting before activity', () => {
    setup({ status: 'starting' });
    expect(status()).toBe('starting');
  });

  it('exited before activity', () => {
    setup({ status: 'exited', activity: 'working' });
    expect(status()).toBe('exited');
  });

  it('attention beats working and awaiting', () => {
    setup({ activity: 'attention', lastTurnEndAt: NOW - 1 });
    expect(status()).toBe('attention');
  });

  it('working beats awaiting', () => {
    setup({ activity: 'working', lastTurnEndAt: NOW - 1 });
    expect(status()).toBe('working');
  });

  it('awaiting after a confirmed turnEnd', () => {
    setup({ lastTurnEndAt: NOW - 10 });
    expect(status()).toBe('awaiting');
  });

  it('awaiting for a fresh-but-quiescent session past the grace window', () => {
    // createdAt is well outside AWAITING_GRACE_SECONDS (5s).
    setup({ createdAt: NOW - 30 });
    expect(status()).toBe('awaiting');
  });

  it('idle inside the grace window', () => {
    setup({ createdAt: NOW - 1 });
    expect(status()).toBe('idle');
  });

  it('idle when no session matches the id', () => {
    useSessionStore.setState({ sessions: [] });
    expect(status()).toBe('idle');
  });
});
