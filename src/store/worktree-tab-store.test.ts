// Behavioural tests for `useWorktreeTabStore`. The Tauri bridge is mocked
// wholesale (see `tauri-bridge.mock.ts`) so no real `invoke()` runs.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { act, renderHook } from '@testing-library/react';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import type { AppConfig, ChildId, SessionId, SubSessionId, WorktreeTab, WorktreeTabId } from '@/types/arborist';

import { useWorktreeTabActions, useWorktreeTabStore } from './worktree-tab-store';

const TAB_A: WorktreeTabId = '00000000-0000-0000-0000-00000000000a' as WorktreeTabId;
const TAB_B: WorktreeTabId = '00000000-0000-0000-0000-00000000000b' as WorktreeTabId;
const TAB_C: WorktreeTabId = '00000000-0000-0000-0000-00000000000c' as WorktreeTabId;
const SESSION_X: SessionId = '11111111-1111-1111-1111-111111111111' as SessionId;
const SUB_Y: SubSessionId = '22222222-2222-2222-2222-222222222222' as SubSessionId;

function makeTab(id: WorktreeTabId, overrides: Partial<WorktreeTab> = {}): WorktreeTab {
  return {
    id,
    path: `/repo/${id}`,
    name: id,
    label: id,
    tabIndex: 0,
    iconId: 1,
    ...overrides,
  };
}

function configWith(activeWorktreeTabId: WorktreeTabId | null = null): AppConfig {
  return {
    configVersion: 11,
    workspaceRoot: null,
    worktreeRoots: [],
    worktreePrepCommands: [],
    aiLaunchCommands: { commands: {}, iconDataUris: {} },
    pluginSettings: { ai: {}, customProcess: {}, dashboardWidget: {} },
    repoCommandTrust: { records: {} },
    lastOpenSessions: [],
    tabOrder: [],
    activeSessionId: null,
    customProcesses: [],
    lastOpenSubSessions: [],
    worktreeTabs: [],
    worktreeTabOrder: [],
    activeWorktreeTabId,
    theme: 'system',
  };
}

function resetStore(): void {
  useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: false });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  resetStore();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('useWorktreeTabStore', () => {
  describe('hydrate', () => {
    it('loads tabs and adopts persisted activeId when it matches', async () => {
      const a = makeTab(TAB_A, { tabIndex: 0 });
      const b = makeTab(TAB_B, { tabIndex: 1 });
      bridgeMock.worktreeTabList.mockResolvedValueOnce([a, b]);
      bridgeMock.configGet.mockResolvedValueOnce(configWith(TAB_B));

      await useWorktreeTabStore.getState().actions.hydrate();

      const s = useWorktreeTabStore.getState();
      expect(s.isHydrated).toBe(true);
      expect(s.tabs).toEqual([a, b]);
      expect(s.activeId).toBe(TAB_B);
    });

    it('falls back to first tab when persisted activeId is stale', async () => {
      const a = makeTab(TAB_A, { tabIndex: 0 });
      const b = makeTab(TAB_B, { tabIndex: 1 });
      bridgeMock.worktreeTabList.mockResolvedValueOnce([a, b]);
      // configGet returns a stale id that does not appear in the freshly loaded tabs.
      bridgeMock.configGet.mockResolvedValueOnce(configWith(TAB_C));

      await useWorktreeTabStore.getState().actions.hydrate();

      const s = useWorktreeTabStore.getState();
      expect(s.tabs).toEqual([a, b]);
      // Stale id dropped → fall back to first tab so MainArea isn't blank.
      expect(s.activeId).toBe(TAB_A);
    });

    it('falls back to first tab when persisted activeId is null', async () => {
      const a = makeTab(TAB_A);
      bridgeMock.worktreeTabList.mockResolvedValueOnce([a]);
      bridgeMock.configGet.mockResolvedValueOnce(configWith(null));

      await useWorktreeTabStore.getState().actions.hydrate();

      expect(useWorktreeTabStore.getState().activeId).toBe(TAB_A);
    });

    it('settles to null activeId when there are no tabs', async () => {
      bridgeMock.worktreeTabList.mockResolvedValueOnce([]);
      bridgeMock.configGet.mockResolvedValueOnce(configWith(TAB_A));

      await useWorktreeTabStore.getState().actions.hydrate();

      const s = useWorktreeTabStore.getState();
      expect(s.tabs).toEqual([]);
      expect(s.activeId).toBeNull();
      expect(s.isHydrated).toBe(true);
    });

    it('propagates bridge errors so App boot can surface them', async () => {
      bridgeMock.worktreeTabList.mockRejectedValueOnce(new Error('disk read failed'));
      bridgeMock.configGet.mockResolvedValueOnce(configWith(null));

      await expect(useWorktreeTabStore.getState().actions.hydrate()).rejects.toThrow('disk read failed');

      expect(useWorktreeTabStore.getState().isHydrated).toBe(false);
    });

    it('self-heals an orphan session by opening a tab for its worktreePath', async () => {
      // Persisted tabs cover only TAB_A's worktree; the running session lives in /repo/orphan with no matching tab.
      const a = makeTab(TAB_A, { path: '/repo/a', tabIndex: 0 });
      const healed = makeTab(TAB_C, { path: '/repo/orphan', tabIndex: 1 });
      bridgeMock.worktreeTabList.mockResolvedValueOnce([a]);
      bridgeMock.configGet.mockResolvedValueOnce(configWith(TAB_A));
      bridgeMock.worktreeTabOpen.mockResolvedValueOnce(healed);

      await useWorktreeTabStore.getState().actions.hydrate(['/repo/a', '/repo/orphan']);

      const s = useWorktreeTabStore.getState();
      expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledTimes(1);
      expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path: '/repo/orphan' });
      expect(s.tabs.map((t) => t.id)).toEqual([TAB_A, TAB_C]);
      // Persisted activeId still points at TAB_A; not overridden by the healed tab.
      expect(s.activeId).toBe(TAB_A);
    });

    it('does not re-open tabs that already exist (idempotent dedupe by path)', async () => {
      const a = makeTab(TAB_A, { path: '/repo/a' });
      bridgeMock.worktreeTabList.mockResolvedValueOnce([a]);
      bridgeMock.configGet.mockResolvedValueOnce(configWith(TAB_A));

      await useWorktreeTabStore.getState().actions.hydrate(['/repo/a', '/repo/a']);

      // Path /repo/a is already covered; duplicate in knownPaths must not trigger a redundant open.
      expect(bridgeMock.worktreeTabOpen).not.toHaveBeenCalled();
    });

    it('continues self-heal when a single open call rejects, but logs', async () => {
      const a = makeTab(TAB_A, { path: '/repo/a' });
      const ok = makeTab(TAB_B, { path: '/repo/healed' });
      bridgeMock.worktreeTabList.mockResolvedValueOnce([a]);
      bridgeMock.configGet.mockResolvedValueOnce(configWith(TAB_A));
      // First missing path rejects; second succeeds. Hydrate must not throw — boot-time best-effort.
      bridgeMock.worktreeTabOpen.mockRejectedValueOnce(new Error('cwd missing')).mockResolvedValueOnce(ok);
      const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

      await useWorktreeTabStore.getState().actions.hydrate(['/repo/a', '/repo/missing', '/repo/healed']);

      expect(useWorktreeTabStore.getState().tabs.map((t) => t.id)).toEqual([TAB_A, TAB_B]);
      expect(warn).toHaveBeenCalled();
      warn.mockRestore();
    });
  });

  describe('open', () => {
    it('appends a new tab and activates it', async () => {
      const tab = makeTab(TAB_A);
      bridgeMock.worktreeTabOpen.mockResolvedValueOnce(tab);

      const returned = await useWorktreeTabStore.getState().actions.open('/repo/a');

      expect(returned).toEqual(tab);
      const s = useWorktreeTabStore.getState();
      expect(s.tabs).toEqual([tab]);
      expect(s.activeId).toBe(TAB_A);
      expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path: '/repo/a' });
    });

    it('replaces the matching tab in place when the backend returns an existing id (focus-existing)', async () => {
      const original = makeTab(TAB_A, { label: 'original', tabIndex: 0 });
      const other = makeTab(TAB_B, { tabIndex: 1 });
      useWorktreeTabStore.setState({ tabs: [original, other], activeId: TAB_B, isHydrated: true });
      // Backend returned the same tab id (label may have been refreshed).
      const focused = makeTab(TAB_A, { label: 'refreshed', tabIndex: 0 });
      bridgeMock.worktreeTabOpen.mockResolvedValueOnce(focused);

      await useWorktreeTabStore.getState().actions.open('/repo/a');

      const s = useWorktreeTabStore.getState();
      expect(s.tabs.map((t) => t.id)).toEqual([TAB_A, TAB_B]);
      expect(s.tabs[0]).toEqual(focused);
      // open() doubles as focus — active id moves to the opened tab.
      expect(s.activeId).toBe(TAB_A);
    });
  });

  describe('close', () => {
    it('forwards default appClosePolicy=detach to the backend when omitted', async () => {
      const a = makeTab(TAB_A);
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.close(TAB_A);

      expect(bridgeMock.worktreeTabClose).toHaveBeenCalledWith({
        id: TAB_A,
        deleteWorktree: false,
        appClosePolicy: 'detach',
      });
    });

    it('forwards explicit appClosePolicy to the backend', async () => {
      const a = makeTab(TAB_A);
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.close(TAB_A, true, 'terminate');

      expect(bridgeMock.worktreeTabClose).toHaveBeenCalledWith({
        id: TAB_A,
        deleteWorktree: true,
        appClosePolicy: 'terminate',
      });
    });

    it('removes the tab and picks the first remaining as active when the closed one was active', async () => {
      const a = makeTab(TAB_A, { tabIndex: 0 });
      const b = makeTab(TAB_B, { tabIndex: 1 });
      useWorktreeTabStore.setState({ tabs: [a, b], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.close(TAB_A);

      const s = useWorktreeTabStore.getState();
      expect(s.tabs.map((t) => t.id)).toEqual([TAB_B]);
      expect(s.activeId).toBe(TAB_B);
    });

    it('preserves the active id when a non-active tab is closed', async () => {
      const a = makeTab(TAB_A);
      const b = makeTab(TAB_B);
      useWorktreeTabStore.setState({ tabs: [a, b], activeId: TAB_B });

      await useWorktreeTabStore.getState().actions.close(TAB_A);

      expect(useWorktreeTabStore.getState().activeId).toBe(TAB_B);
    });

    it('clears active id to null when the last tab is closed', async () => {
      const a = makeTab(TAB_A);
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.close(TAB_A);

      const s = useWorktreeTabStore.getState();
      expect(s.tabs).toEqual([]);
      expect(s.activeId).toBeNull();
    });

    it('does not throw when backend reports child errors and still removes the tab', async () => {
      const a = makeTab(TAB_A);
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });
      bridgeMock.worktreeTabClose.mockResolvedValueOnce({ childErrors: ['session 123: failed'] });
      const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});

      await useWorktreeTabStore.getState().actions.close(TAB_A);

      expect(useWorktreeTabStore.getState().tabs).toEqual([]);
      expect(warnSpy).toHaveBeenCalled();
      warnSpy.mockRestore();
    });

    it('purges sessions whose worktreePath matches the closed tab via removeLocalForPath (issue #44 cascade)', async () => {
      const a = makeTab(TAB_A, { path: '/repo/a' });
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });
      const { useSessionStore } = await import('./session-store');
      const removeSpy = vi.spyOn(useSessionStore.getState().actions, 'removeLocalForPath').mockReturnValue([]);

      await useWorktreeTabStore.getState().actions.close(TAB_A);

      expect(removeSpy).toHaveBeenCalledWith('/repo/a');
      removeSpy.mockRestore();
    });
  });

  describe('focus', () => {
    it('forwards to backend and updates activeId', async () => {
      const a = makeTab(TAB_A);
      const b = makeTab(TAB_B);
      useWorktreeTabStore.setState({ tabs: [a, b], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.focus(TAB_B);

      expect(useWorktreeTabStore.getState().activeId).toBe(TAB_B);
      expect(bridgeMock.worktreeTabFocus).toHaveBeenCalledWith({ id: TAB_B });
    });

    it('updates activeId synchronously before the backend call resolves (optimistic UI)', async () => {
      // PR #65 review: the focus action used to `await worktreeTabFocus` before flipping `activeId`, making tab switches feel laggy under
      // backend contention. The optimistic update must take effect before the promise settles, matching the convention in `session-store`
      // and `sub-session-store`.
      const a = makeTab(TAB_A);
      const b = makeTab(TAB_B);
      useWorktreeTabStore.setState({ tabs: [a, b], activeId: TAB_A });
      // A pending promise that never resolves — proves the store doesn't wait on the backend before switching.
      bridgeMock.worktreeTabFocus.mockImplementationOnce(() => new Promise<void>(() => {}));

      const pending = useWorktreeTabStore.getState().actions.focus(TAB_B);

      expect(useWorktreeTabStore.getState().activeId).toBe(TAB_B);
      expect(bridgeMock.worktreeTabFocus).toHaveBeenCalledWith({ id: TAB_B });
      // Don't await `pending` — the test asserts the synchronous behaviour. The dangling promise is intentional and harmless.
      void pending;
    });

    it('does not roll back activeId when the backend rejects, only logs a warning', async () => {
      // PR #65 review: a backend rejection (e.g. tab raced a close) used to bubble out of `focus`, leaving callers to handle the error and
      // potentially leaving the UI in an inconsistent state. The user's intent stands; the rejection is downgraded to a warn log.
      const a = makeTab(TAB_A);
      const b = makeTab(TAB_B);
      useWorktreeTabStore.setState({ tabs: [a, b], activeId: TAB_A });
      bridgeMock.worktreeTabFocus.mockRejectedValueOnce(new Error('NotFound: tab gone'));
      const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});

      await expect(useWorktreeTabStore.getState().actions.focus(TAB_B)).resolves.toBeUndefined();

      expect(useWorktreeTabStore.getState().activeId).toBe(TAB_B);
      expect(warnSpy).toHaveBeenCalledWith(expect.stringContaining('worktree_tab_focus'));
      warnSpy.mockRestore();
    });
  });

  describe('reorder', () => {
    it('replaces tab list in the new order with refreshed tabIndex values', async () => {
      const a = makeTab(TAB_A, { tabIndex: 0 });
      const b = makeTab(TAB_B, { tabIndex: 1 });
      const c = makeTab(TAB_C, { tabIndex: 2 });
      useWorktreeTabStore.setState({ tabs: [a, b, c], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.reorder([TAB_C, TAB_A, TAB_B]);

      const s = useWorktreeTabStore.getState();
      expect(s.tabs.map((t) => t.id)).toEqual([TAB_C, TAB_A, TAB_B]);
      expect(s.tabs.map((t) => t.tabIndex)).toEqual([0, 1, 2]);
      expect(bridgeMock.worktreeTabReorder).toHaveBeenCalledWith({ ids: [TAB_C, TAB_A, TAB_B] });
    });

    it('appends straggler tabs missing from `ids` rather than dropping them (defense-in-depth)', async () => {
      // Backend rejects mismatched ids before this code runs, so on the happy path no straggler exists. This guards a UI-sequencing bug
      // path: if `ids` ever drifted from the local cache, losing a tab from state would be worse than showing it in a slightly wrong slot.
      const a = makeTab(TAB_A, { tabIndex: 0 });
      const b = makeTab(TAB_B, { tabIndex: 1 });
      const c = makeTab(TAB_C, { tabIndex: 2 });
      useWorktreeTabStore.setState({ tabs: [a, b, c], activeId: TAB_A });

      // Pretend the caller forgot TAB_B in `ids`. Backend mock still resolves so the reducer runs.
      await useWorktreeTabStore.getState().actions.reorder([TAB_C, TAB_A]);

      const s = useWorktreeTabStore.getState();
      expect(s.tabs.map((t) => t.id)).toEqual([TAB_C, TAB_A, TAB_B]);
      expect(s.tabs.map((t) => t.tabIndex)).toEqual([0, 1, 2]);
    });

    it('deduplicates ids that appear twice in the request', async () => {
      const a = makeTab(TAB_A, { tabIndex: 0 });
      const b = makeTab(TAB_B, { tabIndex: 1 });
      useWorktreeTabStore.setState({ tabs: [a, b], activeId: TAB_A });

      // Duplicates would never reach this reducer in practice (backend rejects) but the local code still needs to be defensive — a
      // duplicate must not displace the straggler-append slot.
      await useWorktreeTabStore.getState().actions.reorder([TAB_A, TAB_A]);

      const s = useWorktreeTabStore.getState();
      expect(s.tabs.map((t) => t.id)).toEqual([TAB_A, TAB_B]);
      expect(s.tabs.map((t) => t.tabIndex)).toEqual([0, 1]);
    });
  });

  describe('setActiveChild', () => {
    const sessionChild: ChildId = { kind: 'session', id: SESSION_X };
    const subChild: ChildId = { kind: 'subSession', id: SUB_Y };

    it('omits childId from the args when clearing (matches exactOptionalPropertyTypes contract)', async () => {
      const a = makeTab(TAB_A, { activeChildId: sessionChild });
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.setActiveChild(TAB_A, null);

      // Args must NOT carry an explicit `childId: undefined` — the backend payload type forbids it.
      expect(bridgeMock.worktreeTabSetActiveChild).toHaveBeenCalledWith({ id: TAB_A });
      const tab = useWorktreeTabStore.getState().tabs[0]!;
      expect('activeChildId' in tab).toBe(false);
    });

    it('includes childId for session children', async () => {
      const a = makeTab(TAB_A);
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.setActiveChild(TAB_A, sessionChild);

      expect(bridgeMock.worktreeTabSetActiveChild).toHaveBeenCalledWith({ id: TAB_A, childId: sessionChild });
      expect(useWorktreeTabStore.getState().tabs[0]!.activeChildId).toEqual(sessionChild);
    });

    it('includes childId for sub-session children', async () => {
      const a = makeTab(TAB_A);
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });

      await useWorktreeTabStore.getState().actions.setActiveChild(TAB_A, subChild);

      expect(bridgeMock.worktreeTabSetActiveChild).toHaveBeenCalledWith({ id: TAB_A, childId: subChild });
      expect(useWorktreeTabStore.getState().tabs[0]!.activeChildId).toEqual(subChild);
    });
  });

  describe('patchActiveChild', () => {
    it('only mutates the targeted tab', () => {
      const a = makeTab(TAB_A);
      const b = makeTab(TAB_B);
      useWorktreeTabStore.setState({ tabs: [a, b], activeId: TAB_A });
      const child: ChildId = { kind: 'session', id: SESSION_X };

      useWorktreeTabStore.getState().actions.patchActiveChild(TAB_B, child);

      const s = useWorktreeTabStore.getState();
      expect(s.tabs[0]).toEqual(a);
      expect(s.tabs[1]!.activeChildId).toEqual(child);
    });

    it('clears activeChildId by removing the property (no `undefined` retained)', () => {
      const child: ChildId = { kind: 'session', id: SESSION_X };
      const a = makeTab(TAB_A, { activeChildId: child });
      useWorktreeTabStore.setState({ tabs: [a], activeId: TAB_A });

      useWorktreeTabStore.getState().actions.patchActiveChild(TAB_A, null);

      const tab = useWorktreeTabStore.getState().tabs[0]!;
      expect('activeChildId' in tab).toBe(false);
    });
  });

  describe('useWorktreeTabActions', () => {
    it('returns a referentially-stable bag across unrelated state changes', () => {
      // Subscribers to `useWorktreeTabActions` must not re-render every time some unrelated slice of state mutates. Selecting `s.actions`
      // (a single object set once at store creation) is what keeps the reference identity stable. If anyone refactors the store to assemble
      // actions on the fly inside the selector, this test will fail and force them to think about it.
      const { result } = renderHook(() => useWorktreeTabActions());
      const first = result.current;

      act(() => {
        useWorktreeTabStore.setState({ tabs: [makeTab(TAB_A)], activeId: TAB_A, isHydrated: true });
      });

      expect(result.current).toBe(first);
    });
  });

  describe('selectWorktreeTabRollupStatus', () => {
    it('returns idle when no children match the path', async () => {
      const { selectWorktreeTabRollupStatus } = await import('./worktree-tab-store');
      const status = selectWorktreeTabRollupStatus('/repo/empty')({
        sessions: [],
        openPermissions: {},
        openTools: {},
        activity: {},
        inTurn: {},
        lastTurnEndAt: {},
      });
      expect(status).toBe('idle');
    });

    it('rolls up to the worst child status (error > awaitingPermission > attention > … > idle)', async () => {
      const { selectWorktreeTabRollupStatus } = await import('./worktree-tab-store');
      // Three children under /repo/a: one running idle, one awaiting permission, one error. Error wins.
      const status = selectWorktreeTabRollupStatus(
        '/repo/a',
        1_700_000_000,
      )({
        sessions: [
          { id: 's1', worktreePath: '/repo/a', status: 'running', createdAt: 1_700_000_000 },
          { id: 's2', worktreePath: '/repo/a', status: 'running', createdAt: 1_700_000_000 },
          { id: 's3', worktreePath: '/repo/a', status: 'error', createdAt: 1_700_000_000 },
          { id: 's4', worktreePath: '/repo/b', status: 'starting', createdAt: 1_700_000_000 }, // different path, ignored
        ],
        openPermissions: { s2: { p1: undefined } as never },
        openTools: {},
        activity: {},
        inTurn: {},
        lastTurnEndAt: {},
      });
      expect(status).toBe('error');
    });

    it('rolls up to awaitingPermission when one child has an open permission and others are idle/working', async () => {
      const { selectWorktreeTabRollupStatus } = await import('./worktree-tab-store');
      const status = selectWorktreeTabRollupStatus(
        '/repo/a',
        1_700_000_000,
      )({
        sessions: [
          { id: 's1', worktreePath: '/repo/a', status: 'running', createdAt: 1_700_000_000 },
          { id: 's2', worktreePath: '/repo/a', status: 'running', createdAt: 1_700_000_000 },
        ],
        openPermissions: { s2: { p1: undefined } as never },
        openTools: {},
        activity: { s1: 'working' },
        inTurn: {},
        lastTurnEndAt: {},
      });
      expect(status).toBe('awaitingPermission');
    });
  });
});
