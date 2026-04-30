// Behavioural tests for `useSubSessionStore`. The Tauri bridge is mocked
// wholesale (see `tauri-bridge.mock.ts`) so no real `invoke()` runs.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { SessionId, SubSession, SubSessionId } from '@/types/arborist';

import { useSubSessionStore } from './sub-session-store';

const PARENT_A: SessionId = '00000000-0000-0000-0000-000000000a01' as SessionId;
const PARENT_B: SessionId = '00000000-0000-0000-0000-000000000b01' as SessionId;

function makeSub(overrides: Partial<SubSession> & Pick<SubSession, 'id'>): SubSession {
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
    it('marks active and forwards to backend', async () => {
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
});
