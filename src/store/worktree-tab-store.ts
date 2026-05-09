// Zustand store for worktree tabs (Issue #44).
//
// Holds the list of WorktreeTab records and the active worktree tab ID.
// Actions wrap the relevant Tauri commands and keep the cache in sync.
//
// Conventions:
// * Components subscribe via granular selectors — never `useWorktreeTabStore(s => s)`.
// * Actions don't mutate state; every `set` produces a fresh object/array.
// * Actions live under a single `actions: {}` field so the bag is referentially stable for the lifetime of the store. `useWorktreeTabActions`
//   selects `s.actions` directly — no `useShallow` needed and consumers never re-render just because some unrelated state changed. Matches the
//   sub-session-store convention.

import { create } from 'zustand';

import {
  worktreeTabOpen,
  worktreeTabClose,
  worktreeTabFocus,
  worktreeTabList,
  worktreeTabReorder,
  worktreeTabSetActiveChild,
  configGet,
  formatError,
} from '@/lib/tauri-bridge';
import type { ChildId, WorktreeTab, WorktreeTabCloseResult, WorktreeTabId } from '@/types/arborist';

// ---------------------------------------------------------------------------
// State shape
// ---------------------------------------------------------------------------

export interface WorktreeTabStoreState {
  tabs: WorktreeTab[];
  activeId: WorktreeTabId | null;
  isHydrated: boolean;
  /**
   * Worktree tab whose close-confirm dialog is currently open, if any. Set by
   * [`requestClose`] and cleared by [`cancelClose`] or by [`close`] resolving
   * for the same id. Mirrors the (now-removed) session-store `pendingClose`
   * pattern so `WorktreeCloseConfirmDialog` can mount declaratively from the
   * sidebar without per-tab state.
   */
  pendingClose: WorktreeTabId | undefined;
}

export interface WorktreeTabStoreActions {
  /**
   * Load persisted worktree tabs and reconcile against the live session list. Any session whose `worktreePath` has no matching tab triggers an
   * idempotent `worktreeTabOpen` to self-heal — covers the orphan case where a previous run crashed between `session_create` and the frontend's
   * follow-up `worktreeTabOpen`. Pass `knownPaths` from the freshly hydrated session-store so this store stays free of cross-store imports.
   * Throws on bridge failure so `App.boot`'s error overlay can surface the underlying problem instead of silently rendering an empty sidebar.
   */
  hydrate: (knownPaths?: ReadonlyArray<string>) => Promise<void>;
  open: (path: string) => Promise<WorktreeTab>;
  /**
   * Cascade-close a worktree tab and all its child sessions/sub-sessions. When `deleteWorktree` is true, the backend additionally runs
   * `git worktree remove --force` on the tab's worktree path; the failure of that step is surfaced as `worktreeDeleteError` on the result instead
   * of as a thrown error so the UI can always converge on a "tab gone" state.
   */
  close: (id: WorktreeTabId, deleteWorktree?: boolean) => Promise<WorktreeTabCloseResult>;
  focus: (id: WorktreeTabId) => Promise<void>;
  reorder: (ids: WorktreeTabId[]) => Promise<void>;
  setActiveChild: (id: WorktreeTabId, childId: ChildId | null) => Promise<void>;
  /** Sync a single tab's activeChildId locally (e.g. after session focus). */
  patchActiveChild: (id: WorktreeTabId, childId: ChildId | null) => void;
  /** Open the close-confirm dialog for `id`. UI-only; no bridge call. */
  requestClose: (id: WorktreeTabId) => void;
  /** Dismiss the close-confirm dialog without closing anything. */
  cancelClose: () => void;
}

type Store = WorktreeTabStoreState & { actions: WorktreeTabStoreActions };

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

export const useWorktreeTabStore = create<Store>((set, get) => {
  const actions: WorktreeTabStoreActions = {
    async hydrate(knownPaths?: ReadonlyArray<string>) {
      // Bridge failures here are fatal at boot: they signal a corrupt config or a backend that's refusing the command. Letting them propagate
      // routes them to App.boot's error overlay, which surfaces the actual error to the user instead of silently rendering an empty sidebar
      // (the previous behaviour silently swallowed every failure to console.error).
      const [tabs, cfg] = await Promise.all([worktreeTabList(), configGet()]);

      // Self-heal orphan sessions: any session whose `worktreePath` is not represented by a tab gets one created via the idempotent backend
      // command. This recovers from the rare case where a previous boot crashed between `session_create` and the frontend's follow-up
      // `worktreeTabOpen` — without this, the orphan session would never render in the new sidebar (top-level iterates worktree tabs only).
      // Order is preserved so deterministic iteration matches `tabIndex` ordering for the original tabs and append-order for healed ones.
      const reconciled: WorktreeTab[] = [...tabs];
      if (knownPaths && knownPaths.length > 0) {
        const havePaths = new Set(tabs.map((t) => t.path));
        const seenMissing = new Set<string>();
        for (const path of knownPaths) {
          if (havePaths.has(path) || seenMissing.has(path)) continue;
          seenMissing.add(path);
          try {
            const newTab = await worktreeTabOpen({ path });
            // Backend may return an existing tab if a concurrent caller raced us; dedupe defensively before appending.
            if (!reconciled.some((t) => t.id === newTab.id)) {
              reconciled.push(newTab);
            }
          } catch (err) {
            // A single self-heal failure must not block boot — log and continue. Worst case, the orphan session still won't render under any
            // tab, which is the same outcome as before this self-heal existed.
            console.warn(`[worktree-tab-store] self-heal worktreeTabOpen(${path}) failed: ${formatError(err)}`);
          }
        }
      }

      // Reconcile activeId against the (possibly extended) tab list: prefer the persisted id only when it still exists, else fall back to the
      // first tab, else null. Mirrors `session-store.adoptWorkspace`'s rule — a stale `activeWorktreeTabId` left over from a deleted tab would
      // otherwise leave `useActiveWorktreeTab()` returning `undefined` even though tabs are present.
      const persistedId = cfg.activeWorktreeTabId ?? null;
      const activeId = persistedId !== null && reconciled.some((t) => t.id === persistedId) ? persistedId : (reconciled[0]?.id ?? null);
      set({ tabs: reconciled, isHydrated: true, activeId, pendingClose: undefined });
    },

    async open(path: string) {
      const tab = await worktreeTabOpen({ path });
      set((s) => {
        const exists = s.tabs.some((t) => t.id === tab.id);
        return {
          tabs: exists ? s.tabs.map((t) => (t.id === tab.id ? tab : t)) : [...s.tabs, tab],
          activeId: tab.id,
        };
      });
      return tab;
    },

    async close(id: WorktreeTabId, deleteWorktree?: boolean) {
      // Capture the path BEFORE the backend call so we can converge frontend caches even if the result payload were missing it. Backend
      // cascade closes child sessions/sub-sessions but we don't get per-child UI events for that — the session-store would otherwise leave
      // zombie rows. Lazy-import session-store to avoid a circular import (session-store already imports this module).
      const closingTab = get().tabs.find((t) => t.id === id);
      const result = await worktreeTabClose({ id, deleteWorktree: deleteWorktree ?? false });
      if (result.childErrors && result.childErrors.length > 0) {
        console.warn('[worktree-tab-store] close had child errors:', result.childErrors);
      }
      set((s) => {
        const newTabs = s.tabs.filter((t) => t.id !== id);
        const newActiveId = s.activeId === id ? (newTabs[0]?.id ?? null) : s.activeId;
        return {
          tabs: newTabs,
          activeId: newActiveId,
          // Auto-clear pendingClose if the dialog was open for the row we just closed.
          pendingClose: s.pendingClose === id ? undefined : s.pendingClose,
        };
      });
      if (closingTab) {
        try {
          // Lazy require to break the import cycle session-store -> worktree-tab-store at module-load time.
          const { useSessionStore } = await import('@/store/session-store');
          useSessionStore.getState().actions.removeLocalForPath(closingTab.path);
        } catch (err) {
          console.warn(`[worktree-tab-store] removeLocalForPath(${closingTab.path}) failed: ${formatError(err)}`);
        }
        try {
          // Sub-sessions are now owned by worktree tabs, not by agent sessions. Drop their local cache entries so the sidebar is consistent.
          const { useSubSessionStore } = await import('@/store/sub-session-store');
          useSubSessionStore.getState().actions.dropForWorktreeTab(id);
        } catch (err) {
          console.warn(`[worktree-tab-store] dropForWorktreeTab(${id}) failed: ${formatError(err)}`);
        }
      }
      return result;
    },

    async focus(id: WorktreeTabId) {
      // Optimistic: switching the active worktree tab must feel instant. Backend rejections (NotFound, WorkspaceSwitchInProgress, etc.) just
      // mean the persisted active marker is stale — the user's intent stands. Mirrors `session-store.focus` and the terminal-kind branch in
      // `sub-session-store.focus`. We deliberately do NOT roll back `activeId` on rejection: that would yank the UI back to the previously
      // focused tab while the user is trying to interact with the new one, and would surface backend bookkeeping errors as visible flicker.
      set({ activeId: id });
      try {
        await worktreeTabFocus({ id });
      } catch (err) {
        console.warn(`[worktree-tab-store] worktree_tab_focus(${id}) rejected: ${formatError(err)}`);
      }
    },

    async reorder(ids: WorktreeTabId[]) {
      await worktreeTabReorder({ ids });
      set((s) => {
        const byId = new Map(s.tabs.map((t) => [t.id, t]));
        const seen = new Set<WorktreeTabId>();
        const reordered: WorktreeTab[] = [];
        let idx = 0;
        for (const id of ids) {
          const tab = byId.get(id);
          if (tab && !seen.has(id)) {
            reordered.push({ ...tab, tabIndex: idx });
            seen.add(id);
            idx += 1;
          }
        }
        // Defense-in-depth: backend validates `ids` is exactly the existing set, so on the happy path no straggler exists. If a UI
        // sequencing bug ever caused drift between the local cache and the dispatched `ids`, append the missing tabs rather than silently
        // dropping them — losing a tab from the cache is worse than showing it in a slightly wrong slot, and a follow-up `worktreeTabList()`
        // (or any subsequent open/close) will reconcile.
        for (const t of s.tabs) {
          if (!seen.has(t.id)) {
            reordered.push({ ...t, tabIndex: idx });
            idx += 1;
          }
        }
        return { tabs: reordered };
      });
    },

    async setActiveChild(id: WorktreeTabId, childId: ChildId | null) {
      // Patch local state SYNCHRONOUSLY so the UI reacts immediately and so a slow backend can't reapply a stale value if a newer click has
      // already arrived. Race-protection: callers may fire-and-forget several `setActiveChild` calls in quick succession; whichever ran most
      // recently determines the local state. We deliberately do NOT re-patch after the await — that would resurrect stale intent.
      get().actions.patchActiveChild(id, childId);
      const args = childId !== null ? { id, childId } : { id };
      try {
        await worktreeTabSetActiveChild(args);
      } catch (err) {
        // Persistence failure is not user-visible — the local cache is already in sync with the user's intent. Log so issues surface in dev.
        console.warn(`[worktree-tab-store] worktree_tab_set_active_child(${id}) rejected: ${formatError(err)}`);
      }
    },

    patchActiveChild(id: WorktreeTabId, childId: ChildId | null) {
      set((s) => ({
        tabs: s.tabs.map((t) => {
          if (t.id !== id) return t;
          if (childId === null) {
            const { activeChildId: _, ...rest } = t;
            return rest as WorktreeTab;
          }
          return { ...t, activeChildId: childId };
        }),
      }));
    },

    requestClose: (id) => {
      set({ pendingClose: id });
    },

    cancelClose: () => {
      set({ pendingClose: undefined });
    },
  };

  return {
    tabs: [],
    activeId: null,
    isHydrated: false,
    pendingClose: undefined,
    actions,
  };
});

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export const useWorktreeTabs = (): WorktreeTab[] => useWorktreeTabStore((s) => s.tabs);
export const useActiveWorktreeTabId = (): WorktreeTabId | null => useWorktreeTabStore((s) => s.activeId);
export const usePendingWorktreeTabClose = (): WorktreeTabId | undefined => useWorktreeTabStore((s) => s.pendingClose);

/**
 * Stable bag of every action. Backed by a single `actions` object set once at store creation, so the selector returns a referentially-stable
 * reference and consumers do NOT re-render on every state change. Matches the `useSubSessionActions` convention.
 */
export const useWorktreeTabActions = (): WorktreeTabStoreActions => useWorktreeTabStore((s) => s.actions);

export const useActiveWorktreeTab = (): WorktreeTab | undefined => useWorktreeTabStore((s) => s.tabs.find((t) => t.id === s.activeId));

/**
 * Priority order for the parent worktree tab's rolled-up status icon. The worst-priority status across the tab's child sessions is what the
 * sidebar renders on the parent row. Mirrors the priority in `selectDisplayStatus` but only for the states a user cares about *at the parent
 * level* — `error` > `awaitingPermission` > `attention` > `running/working` > `awaiting` > `idle/exited`. Unknown states fall through to 0.
 *
 * Exported separately from the rollup selector so consumers (e.g. tests, parent-tab tooltip) can compare priorities directly without
 * recomputing the rollup.
 */
import type { DisplayStatus } from '@/store/session-store';

const STATUS_PRIORITY: Record<DisplayStatus, number> = {
  error: 9,
  awaitingPermission: 8,
  attention: 7,
  runningTool: 6,
  thinking: 5,
  working: 4,
  starting: 3,
  awaiting: 2,
  idle: 1,
  exited: 0,
};

export function compareDisplayStatus(a: DisplayStatus, b: DisplayStatus): number {
  return (STATUS_PRIORITY[b] ?? 0) - (STATUS_PRIORITY[a] ?? 0);
}

/**
 * Compute the worst (highest-priority) `DisplayStatus` across the children of `tabPath`. Returns `'idle'` when the tab has no children — a
 * brand-new worktree tab with no sessions is conceptually idle, not exited. The selector takes the canonical `tabPath` (not the tab id) so
 * callers can pass a stable string from a Zustand selector closure without re-subscribing the whole tabs array.
 *
 * Caller is expected to pass `nowSec` from `useNowTickSeconds` if they want time-based `idle → awaiting` promotion to track per-second; in
 * tests, omit it (defaults to wall-clock seconds).
 */
export function selectWorktreeTabRollupStatus(tabPath: string, nowSec?: number): (s: SessionStoreLike) => DisplayStatus {
  return (s) => {
    const children = s.sessions.filter((sess) => sess.worktreePath === tabPath);
    if (children.length === 0) return 'idle';
    let worst: DisplayStatus = 'idle';
    for (const child of children) {
      const status = computeChildStatus(child.id, s, nowSec);
      if (compareDisplayStatus(status, worst) < 0) worst = status;
    }
    return worst;
  };
}

// Local minimal shape so this selector doesn't import the full session-store state (which would create a circular import); the function only
// needs the same fields `selectDisplayStatus` reads.
interface SessionStoreLike {
  sessions: ReadonlyArray<{ id: string; worktreePath: string; status: string; createdAt: number }>;
  openPermissions: Record<string, Record<string, unknown> | undefined>;
  openTools: Record<string, Record<string, unknown> | undefined>;
  activity: Record<string, 'working' | 'idle' | 'attention' | undefined>;
  inTurn: Record<string, true | undefined>;
  lastTurnEndAt: Record<string, number | undefined>;
}

function computeChildStatus(id: string, s: SessionStoreLike, nowSec?: number): DisplayStatus {
  const session = s.sessions.find((x) => x.id === id);
  if (!session) return 'idle';
  if (session.status === 'error') return 'error';
  if (session.status === 'starting') return 'starting';
  if (session.status === 'exited') return 'exited';
  if (s.openPermissions[id] && Object.keys(s.openPermissions[id]!).length > 0) return 'awaitingPermission';
  const activity = s.activity[id];
  if (activity === 'attention') return 'attention';
  if (s.openTools[id] && Object.keys(s.openTools[id]!).length > 0) return 'runningTool';
  if (s.inTurn[id]) return 'thinking';
  if (activity === 'working') return 'working';
  if (s.lastTurnEndAt[id] !== undefined) return 'awaiting';
  const now = nowSec ?? Math.floor(Date.now() / 1000);
  if (now - session.createdAt >= 5) return 'awaiting';
  return 'idle';
}
