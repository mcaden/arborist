// Behavioural tests for `useSubSessionStore`. The Tauri bridge is mocked
// wholesale (see `tauri-bridge.mock.ts`) so no real `invoke()` runs.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { SessionId, SubSession, SubSessionId } from '@/types/arborist';

import { useSubSessionStore } from './sub-session-store';

const PARENT_A: SessionId = '00000000-0000-0000-0000-000000000a01' as SessionId;
const PARENT_B: SessionId = '00000000-0000-0000-0000-000000000b01' as SessionId;

// Override type permits `pid: undefined` explicitly even though
// `SubSession.pid?: number` rejects it under
// `exactOptionalPropertyTypes: true`.
type SubOverrides = Partial<Omit<SubSession, 'id' | 'pid'>> &
  Pick<SubSession, 'id'> & {
    pid?: number | undefined;
  };

function makeSub(overrides: SubOverrides): SubSession {
  return {
    parentSessionId: PARENT_A,
    defId: 'shell',
    kind: 'terminal',
    label: 'Shell',
    status: 'running',
    pid: 1234,
    composedCommand: 'sh -i',
    createdAt: 1_700_000_000,
    ...overrides,
  } as SubSession;
}

function id(suffix: string): SubSessionId {
  return ('11111111-1111-1111-1111-1111111111' + suffix) as SubSessionId;
}

function resetStore(): void {
  useSubSessionStore.setState({
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
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

describe('useSubSessionStore', () => {
  describe('hydrate', () => {
    it('pulls list from backend and marks hydrated', async () => {
      const a = makeSub({ id: id('01') });
      bridgeMock.subSessionList.mockResolvedValueOnce([a]);
      await useSubSessionStore.getState().actions.hydrate();
      const state = useSubSessionStore.getState();
      expect(state.isHydrated).toBe(true);
      expect(state.subSessions).toEqual([a]);
    });

    it('drops activeByParent entries that no longer match a real sub-session', async () => {
      const sub = makeSub({ id: id('20'), parentSessionId: PARENT_A });
      useSubSessionStore.setState({
        subSessions: [],
        activeByParent: { [PARENT_A]: id('99'), [PARENT_B]: sub.id },
      });
      bridgeMock.subSessionList.mockResolvedValueOnce([sub]);
      await useSubSessionStore.getState().actions.hydrate();
      const s = useSubSessionStore.getState();
      // PARENT_A pointed at a stale id → dropped.
      expect(PARENT_A in s.activeByParent).toBe(false);
      // PARENT_B pointed at sub.id, but sub belongs to PARENT_A → dropped.
      expect(PARENT_B in s.activeByParent).toBe(false);
    });

    it('does not clobber UI-only activeByParent on rehydrate', async () => {
      const sub = makeSub({ id: id('02'), parentSessionId: PARENT_A });
      useSubSessionStore.setState({
        subSessions: [sub],
        activeByParent: { [PARENT_A]: sub.id },
      });
      bridgeMock.subSessionList.mockResolvedValueOnce([sub]);
      await useSubSessionStore.getState().actions.hydrate();
      expect(useSubSessionStore.getState().activeByParent[PARENT_A]).toBe(sub.id);
    });
  });

  describe('create', () => {
    it('appends to list and activates under its parent', async () => {
      const sub = makeSub({ id: id('03') });
      bridgeMock.subSessionCreate.mockResolvedValueOnce(sub);
      const returned = await useSubSessionStore.getState().actions.create({
        parentSessionId: PARENT_A,
        defId: 'shell',
      });
      expect(returned).toEqual(sub);
      const s = useSubSessionStore.getState();
      expect(s.subSessions).toEqual([sub]);
      expect(s.activeByParent[PARENT_A]).toBe(sub.id);
    });
  });

  describe('close', () => {
    it('removes from list and picks neighbour under same parent', async () => {
      const a = makeSub({ id: id('04'), parentSessionId: PARENT_A });
      const b = makeSub({ id: id('05'), parentSessionId: PARENT_A });
      const c = makeSub({ id: id('06'), parentSessionId: PARENT_A });
      useSubSessionStore.setState({
        subSessions: [a, b, c],
        activeByParent: { [PARENT_A]: b.id },
      });
      await useSubSessionStore.getState().actions.close(b.id);
      const s = useSubSessionStore.getState();
      expect(s.subSessions.map((x) => x.id)).toEqual([a.id, c.id]);
      // Prefer the next sibling (c) over previous (a).
      expect(s.activeByParent[PARENT_A]).toBe(c.id);
    });

    it('clears activeByParent for parent when last sub-session closes', async () => {
      const only = makeSub({ id: id('07'), parentSessionId: PARENT_A });
      useSubSessionStore.setState({
        subSessions: [only],
        activeByParent: { [PARENT_A]: only.id },
      });
      await useSubSessionStore.getState().actions.close(only.id);
      const s = useSubSessionStore.getState();
      expect(s.subSessions).toEqual([]);
      expect(PARENT_A in s.activeByParent).toBe(false);
    });

    it('still converges local state when backend close throws', async () => {
      const sub = makeSub({ id: id('08'), parentSessionId: PARENT_A });
      useSubSessionStore.setState({
        subSessions: [sub],
        activeByParent: { [PARENT_A]: sub.id },
      });
      bridgeMock.subSessionClose.mockRejectedValueOnce(new Error('disk full'));
      await expect(useSubSessionStore.getState().actions.close(sub.id)).rejects.toThrow(
        'disk full',
      );
      const s = useSubSessionStore.getState();
      expect(s.subSessions).toEqual([]);
      expect(PARENT_A in s.activeByParent).toBe(false);
    });
  });

  describe('focus', () => {
    it('marks active and forwards to backend for terminal kind', async () => {
      const a = makeSub({ id: id('09'), parentSessionId: PARENT_A });
      const b = makeSub({ id: id('10'), parentSessionId: PARENT_A });
      useSubSessionStore.setState({
        subSessions: [a, b],
        activeByParent: { [PARENT_A]: a.id },
      });
      await useSubSessionStore.getState().actions.focus(b.id);
      expect(useSubSessionStore.getState().activeByParent[PARENT_A]).toBe(b.id);
      expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(b.id);
    });

    it('does NOT touch activeByParent for application kind', async () => {
      const term = makeSub({ id: id('0a'), parentSessionId: PARENT_A, kind: 'terminal' });
      const app = makeSub({
        id: id('0b'),
        parentSessionId: PARENT_A,
        kind: 'application',
        defId: 'vscode',
      });
      useSubSessionStore.setState({
        subSessions: [term, app],
        activeByParent: { [PARENT_A]: term.id },
      });
      await useSubSessionStore.getState().actions.focus(app.id);
      // Viewport sticks with the previously-visible terminal sub-session.
      expect(useSubSessionStore.getState().activeByParent[PARENT_A]).toBe(term.id);
      // Backend focuser still invoked so the OS window is raised.
      expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(app.id);
    });

    it('is a no-op for unknown id', async () => {
      await useSubSessionStore.getState().actions.focus(id('aa'));
      expect(bridgeMock.subSessionFocus).not.toHaveBeenCalled();
    });
  });

  describe('activateParent', () => {
    it('removes the parent entry so the parent terminal shows', () => {
      const sub = makeSub({ id: id('11'), parentSessionId: PARENT_A });
      useSubSessionStore.setState({
        subSessions: [sub],
        activeByParent: { [PARENT_A]: sub.id },
      });
      useSubSessionStore.getState().actions.activateParent(PARENT_A);
      expect(PARENT_A in useSubSessionStore.getState().activeByParent).toBe(false);
    });
  });

  describe('dropForParent', () => {
    it('removes all subs for the parent and clears active + status', () => {
      const a = makeSub({ id: id('12'), parentSessionId: PARENT_A });
      const b = makeSub({ id: id('13'), parentSessionId: PARENT_B });
      useSubSessionStore.setState({
        subSessions: [a, b],
        activeByParent: { [PARENT_A]: a.id, [PARENT_B]: b.id },
        statusMessages: { [a.id]: 'something' },
      });
      useSubSessionStore.getState().actions.dropForParent(PARENT_A);
      const s = useSubSessionStore.getState();
      expect(s.subSessions.map((x) => x.id)).toEqual([b.id]);
      expect(PARENT_A in s.activeByParent).toBe(false);
      expect(s.activeByParent[PARENT_B]).toBe(b.id);
      expect(a.id in s.statusMessages).toBe(false);
    });
  });

  describe('applyStatus', () => {
    it('updates status + pid in place', () => {
      const sub = makeSub({ id: id('14'), status: 'starting', pid: undefined });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore.getState().actions.applyStatus({
        id: sub.id,
        status: 'running',
        pid: 9999,
      });
      const updated = useSubSessionStore.getState().subSessions[0]!;
      expect(updated.status).toBe('running');
      expect(updated.pid).toBe(9999);
    });

    it('clears pid on terminal status', () => {
      const sub = makeSub({ id: id('15'), pid: 100 });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore.getState().actions.applyStatus({
        id: sub.id,
        status: 'exited',
      });
      expect(useSubSessionStore.getState().subSessions[0]!.pid).toBeUndefined();
    });

    it('records and clears status message', () => {
      const sub = makeSub({ id: id('16') });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore
        .getState()
        .actions.applyStatus({ id: sub.id, status: 'error', message: 'oops' });
      expect(useSubSessionStore.getState().statusMessages[sub.id]).toBe('oops');
      useSubSessionStore.getState().actions.applyStatus({ id: sub.id, status: 'running' });
      expect(sub.id in useSubSessionStore.getState().statusMessages).toBe(false);
    });

    it('ignores events for unknown ids', () => {
      useSubSessionStore.getState().actions.applyStatus({ id: id('17'), status: 'running' });
      expect(useSubSessionStore.getState().subSessions).toEqual([]);
    });
  });

  describe('applyExited', () => {
    it('forces status to exited if not already terminal', () => {
      const sub = makeSub({ id: id('18'), status: 'running', pid: 7 });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore.getState().actions.applyExited({ id: sub.id, exitCode: 0 });
      const updated = useSubSessionStore.getState().subSessions[0]!;
      expect(updated.status).toBe('exited');
      expect(updated.pid).toBeUndefined();
    });

    it('synthesizes status=error when exitCode is non-zero', () => {
      const sub = makeSub({ id: id('20a'), status: 'running', pid: 5 });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore.getState().actions.applyExited({ id: sub.id, exitCode: 137 });
      const updated = useSubSessionStore.getState().subSessions[0]!;
      expect(updated.status).toBe('error');
      expect(updated.pid).toBeUndefined();
    });

    it('is idempotent if already exited', () => {
      const sub = makeSub({ id: id('19'), status: 'exited', pid: undefined });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore.getState().actions.applyExited({ id: sub.id });
      expect(useSubSessionStore.getState().subSessions[0]!.status).toBe('exited');
    });
  });

  describe('applyRestored (Phase 7)', () => {
    it('inserts a sub-session received from restore-on-launch', () => {
      const sub = makeSub({ id: id('20'), kind: 'terminal' });
      useSubSessionStore.getState().actions.applyRestored({ subSession: sub });
      const state = useSubSessionStore.getState();
      expect(state.subSessions).toEqual([sub]);
      // Terminal restore claims activeByParent if no other sub-tab has it.
      expect(state.activeByParent[PARENT_A]).toBe(sub.id);
    });

    it('is idempotent on duplicate restore', () => {
      const sub = makeSub({ id: id('21'), kind: 'terminal' });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore.getState().actions.applyRestored({ subSession: sub });
      expect(useSubSessionStore.getState().subSessions).toHaveLength(1);
    });

    it('does not steal activeByParent if parent already owns one', () => {
      const existing = makeSub({ id: id('22'), kind: 'terminal' });
      useSubSessionStore.setState({
        subSessions: [existing],
        activeByParent: { [PARENT_A]: existing.id },
      });
      const restored = makeSub({ id: id('23'), kind: 'terminal' });
      useSubSessionStore.getState().actions.applyRestored({ subSession: restored });
      // activeByParent must still point at `existing`, not the new row.
      expect(useSubSessionStore.getState().activeByParent[PARENT_A]).toBe(existing.id);
    });

    it('does not claim activeByParent for application kind', () => {
      const sub = makeSub({ id: id('24'), kind: 'application', status: 'exited' });
      useSubSessionStore.getState().actions.applyRestored({ subSession: sub });
      const state = useSubSessionStore.getState();
      expect(state.subSessions).toEqual([sub]);
      // Application sub-tabs never claim viewport — activeByParent stays clear.
      expect(state.activeByParent[PARENT_A]).toBeUndefined();
    });
  });

  describe('relaunch (Phase 7)', () => {
    it('flips status to starting and calls subSessionRelaunch', async () => {
      const sub = makeSub({
        id: id('30'),
        kind: 'application',
        status: 'exited',
        pid: undefined,
      });
      useSubSessionStore.setState({ subSessions: [sub] });
      bridgeMock.subSessionRelaunch.mockResolvedValueOnce({ ...sub, status: 'running', pid: 99 });

      await useSubSessionStore.getState().actions.relaunch(sub.id);

      expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
      // After awaited completion, the optimistic `starting` flip is still
      // visible — the real status update arrives later via the
      // subsession://status channel which this test doesn't simulate.
      const updated = useSubSessionStore.getState().subSessions[0]!;
      expect(updated.status).toBe('starting');
      expect(updated.pid).toBeUndefined();
    });

    it('dedupes concurrent calls per id', async () => {
      const sub = makeSub({ id: id('31'), kind: 'application', status: 'error' });
      useSubSessionStore.setState({ subSessions: [sub] });

      let resolveFirst: (() => void) | null = null;
      bridgeMock.subSessionRelaunch.mockImplementationOnce(
        () =>
          new Promise<SubSession>((resolve) => {
            resolveFirst = () => resolve({ ...sub, status: 'running', pid: 1 });
          }),
      );

      const first = useSubSessionStore.getState().actions.relaunch(sub.id);
      // Second call while first is pending — must be a no-op.
      const second = useSubSessionStore.getState().actions.relaunch(sub.id);
      await second; // resolves immediately (no-op)

      expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledTimes(1);

      resolveFirst!();
      await first;
    });

    it('is a no-op for unknown id', async () => {
      bridgeMock.subSessionRelaunch.mockResolvedValueOnce(makeSub({ id: id('32') }));
      // Does NOT throw — the optimistic-status update is a setState that
      // bails out early when the row is absent.
      await useSubSessionStore.getState().actions.relaunch(id('99'));
      // The bridge call still goes out — the backend is the source of
      // truth for "id not found" and will reject. We just check we
      // didn't crash and the store stayed empty.
      expect(useSubSessionStore.getState().subSessions).toEqual([]);
    });

    it('clears the dedupe set even when the bridge rejects', async () => {
      const sub = makeSub({ id: id('33'), kind: 'application', status: 'exited' });
      useSubSessionStore.setState({ subSessions: [sub] });
      bridgeMock.subSessionRelaunch.mockRejectedValueOnce(new Error('boom'));

      await expect(useSubSessionStore.getState().actions.relaunch(sub.id)).rejects.toThrow('boom');

      // A subsequent retry must NOT be deduped.
      bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
      await useSubSessionStore.getState().actions.relaunch(sub.id);
      expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledTimes(2);
    });

    it('rolls back the optimistic status flip and surfaces the error message when the bridge rejects', async () => {
      const sub = makeSub({
        id: id('44'),
        kind: 'application',
        status: 'exited',
        pid: undefined,
      });
      useSubSessionStore.setState({
        subSessions: [sub],
        statusMessages: { [sub.id]: 'previous hint' },
      });
      // Reject with the wire shape Tauri actually produces (an `AppError`
      // payload — a plain `{ code, message }` object, *not* an `Error`
      // instance). A naive `String(err)` on this would yield
      // "[object Object]"; the rollback path must use `formatError` so the
      // user sees the real failure.
      bridgeMock.subSessionRelaunch.mockRejectedValueOnce({
        code: 'CapabilityDenied',
        message: 'capability denied',
      });

      await expect(useSubSessionStore.getState().actions.relaunch(sub.id)).rejects.toMatchObject({
        code: 'CapabilityDenied',
        message: 'capability denied',
      });

      // Status rolled back to the prior terminal state — not stuck in `starting`.
      const after = useSubSessionStore.getState();
      const restored = after.subSessions.find((s) => s.id === sub.id);
      expect(restored?.status).toBe('exited');
      expect(restored?.pid).toBeUndefined();
      // Failure message replaced the previous hint with a human-readable
      // string (NOT "[object Object]") so the user can see what went wrong.
      expect(after.statusMessages[sub.id]).toBe('CapabilityDenied: capability denied');
    });
  });
});
