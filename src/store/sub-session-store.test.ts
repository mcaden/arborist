// Behavioural tests for `useSubSessionStore`. The Tauri bridge is mocked
// wholesale (see `tauri-bridge.mock.ts`) so no real `invoke()` runs.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { SubSession, SubSessionId, WorktreeTabId } from '@/types/arborist';

import { useSubSessionStore } from './sub-session-store';

const TAB_A = 'tab-a' as WorktreeTabId;
const TAB_B = 'tab-b' as WorktreeTabId;

type SubOverrides = Partial<Omit<SubSession, 'id' | 'pid'>> &
  Pick<SubSession, 'id'> & {
    pid?: number | undefined;
  };

function makeSub(overrides: SubOverrides): SubSession {
  const { pid, ...restOverrides } = overrides;
  const sub: SubSession = {
    parentWorktreeTabId: TAB_A,
    defId: 'shell',
    kind: 'terminal',
    label: 'Shell',
    status: 'running',
    composedCommand: 'sh -i',
    createdAt: 1_700_000_000,
    ...restOverrides,
  };
  if ('pid' in overrides) {
    if (pid !== undefined) sub.pid = pid;
  } else {
    sub.pid = 1234;
  }
  return sub;
}

function id(suffix: string): SubSessionId {
  return ('11111111-1111-1111-1111-1111111111' + suffix) as SubSessionId;
}

function resetStore(): void {
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
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
  });

  describe('create', () => {
    it('appends to list', async () => {
      const sub = makeSub({ id: id('03') });
      bridgeMock.subSessionCreate.mockResolvedValueOnce(sub);

      const returned = await useSubSessionStore.getState().actions.create({
        parentWorktreeTabId: TAB_A,
        defId: 'shell',
      });

      expect(returned).toEqual(sub);
      expect(useSubSessionStore.getState().subSessions).toEqual([sub]);
    });
  });

  describe('close', () => {
    it('removes from list', async () => {
      const a = makeSub({ id: id('04') });
      const b = makeSub({ id: id('05') });
      useSubSessionStore.setState({
        subSessions: [a, b],
      });

      await useSubSessionStore.getState().actions.close(b.id);

      const s = useSubSessionStore.getState();
      expect(s.subSessions.map((x) => x.id)).toEqual([a.id]);
    });

    it('clears pendingClose and status message for the closed row', async () => {
      const sub = makeSub({ id: id('07') });
      useSubSessionStore.setState({
        subSessions: [sub],
        statusMessages: { [sub.id]: 'oops' },
        pendingClose: sub.id,
      });

      await useSubSessionStore.getState().actions.close(sub.id);

      const s = useSubSessionStore.getState();
      expect(s.subSessions).toEqual([]);
      expect(s.pendingClose).toBeUndefined();
      expect(s.statusMessages[sub.id]).toBeUndefined();
    });

    it('still converges local state when backend close throws', async () => {
      const sub = makeSub({ id: id('08') });
      useSubSessionStore.setState({
        subSessions: [sub],
      });
      bridgeMock.subSessionClose.mockRejectedValueOnce(new Error('disk full'));

      await expect(useSubSessionStore.getState().actions.close(sub.id)).rejects.toThrow('disk full');

      expect(useSubSessionStore.getState().subSessions).toEqual([]);
    });
  });

  describe('requestClose / cancelClose', () => {
    it('toggles pendingClose locally', () => {
      const sub = makeSub({ id: id('08a') });
      useSubSessionStore.getState().actions.requestClose(sub.id);
      expect(useSubSessionStore.getState().pendingClose).toBe(sub.id);
      useSubSessionStore.getState().actions.cancelClose();
      expect(useSubSessionStore.getState().pendingClose).toBeUndefined();
    });
  });

  describe('focus', () => {
    it('forwards to backend for known ids', async () => {
      const sub = makeSub({ id: id('09') });
      useSubSessionStore.setState({ subSessions: [sub] });

      await useSubSessionStore.getState().actions.focus(sub.id);

      expect(bridgeMock.subSessionFocus).toHaveBeenCalledWith(sub.id);
    });

    it('is a no-op for unknown id', async () => {
      await useSubSessionStore.getState().actions.focus(id('aa'));
      expect(bridgeMock.subSessionFocus).not.toHaveBeenCalled();
    });
  });

  describe('dropForWorktreeTab', () => {
    it('removes all subs for the tab and clears status + pendingClose', () => {
      const a = makeSub({ id: id('12'), parentWorktreeTabId: TAB_A });
      const b = makeSub({ id: id('13'), parentWorktreeTabId: TAB_B });
      useSubSessionStore.setState({
        subSessions: [a, b],
        statusMessages: { [a.id]: 'something', [b.id]: 'keep' },
        pendingClose: a.id,
      });

      useSubSessionStore.getState().actions.dropForWorktreeTab(TAB_A);

      const s = useSubSessionStore.getState();
      expect(s.subSessions.map((x) => x.id)).toEqual([b.id]);
      expect(a.id in s.statusMessages).toBe(false);
      expect(s.statusMessages[b.id]).toBe('keep');
      expect(s.pendingClose).toBeUndefined();
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
      useSubSessionStore.getState().actions.applyStatus({ id: sub.id, status: 'error', message: 'oops' });
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
    });

    it('is idempotent on duplicate restore', () => {
      const sub = makeSub({ id: id('21'), kind: 'terminal' });
      useSubSessionStore.setState({ subSessions: [sub] });
      useSubSessionStore.getState().actions.applyRestored({ subSession: sub });
      expect(useSubSessionStore.getState().subSessions).toHaveLength(1);
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
      const second = useSubSessionStore.getState().actions.relaunch(sub.id);
      await second;

      expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledTimes(1);

      resolveFirst!();
      await first;
    });

    it('is a no-op for unknown id', async () => {
      bridgeMock.subSessionRelaunch.mockResolvedValueOnce(makeSub({ id: id('32') }));
      await useSubSessionStore.getState().actions.relaunch(id('99'));
      expect(useSubSessionStore.getState().subSessions).toEqual([]);
    });
  });
});
