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
import type { ChildId, WorktreeTab, WorktreeTabId } from '@/types/arborist';

// ---------------------------------------------------------------------------
// State shape
// ---------------------------------------------------------------------------

export interface WorktreeTabStoreState {
  tabs: WorktreeTab[];
  activeId: WorktreeTabId | null;
  isHydrated: boolean;
}

export interface WorktreeTabStoreActions {
  hydrate: () => Promise<void>;
  open: (path: string) => Promise<WorktreeTab>;
  close: (id: WorktreeTabId) => Promise<void>;
  focus: (id: WorktreeTabId) => Promise<void>;
  reorder: (ids: WorktreeTabId[]) => Promise<void>;
  setActiveChild: (id: WorktreeTabId, childId: ChildId | null) => Promise<void>;
  /** Sync a single tab's activeChildId locally (e.g. after session focus). */
  patchActiveChild: (id: WorktreeTabId, childId: ChildId | null) => void;
}

type Store = WorktreeTabStoreState & { actions: WorktreeTabStoreActions };

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

export const useWorktreeTabStore = create<Store>((set, get) => {
  const actions: WorktreeTabStoreActions = {
    async hydrate() {
      try {
        const [tabs, cfg] = await Promise.all([worktreeTabList(), configGet()]);
        // Reconcile activeId against the freshly loaded tabs: prefer the persisted id only when it still exists, else fall back to the first
        // tab, else null. Mirrors `session-store.adoptWorkspace`'s reconciliation rule — without this, a stale `activeWorktreeTabId` (e.g.
        // left over from a deleted tab) would leave `useActiveWorktreeTab()` returning `undefined` even though tabs are present.
        const persistedId = cfg.activeWorktreeTabId ?? null;
        const activeId = persistedId !== null && tabs.some((t) => t.id === persistedId) ? persistedId : (tabs[0]?.id ?? null);
        set({ tabs, isHydrated: true, activeId });
      } catch (err) {
        console.error('[worktree-tab-store] hydrate failed:', formatError(err));
      }
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

    async close(id: WorktreeTabId) {
      const result = await worktreeTabClose({ id });
      if (result.childErrors && result.childErrors.length > 0) {
        console.warn('[worktree-tab-store] close had child errors:', result.childErrors);
      }
      set((s) => {
        const newTabs = s.tabs.filter((t) => t.id !== id);
        const newActiveId = s.activeId === id ? (newTabs[0]?.id ?? null) : s.activeId;
        return { tabs: newTabs, activeId: newActiveId };
      });
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
      const args = childId !== null ? { id, childId } : { id };
      await worktreeTabSetActiveChild(args);
      get().actions.patchActiveChild(id, childId);
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
  };

  return {
    tabs: [],
    activeId: null,
    isHydrated: false,
    actions,
  };
});

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export const useWorktreeTabs = (): WorktreeTab[] => useWorktreeTabStore((s) => s.tabs);
export const useActiveWorktreeTabId = (): WorktreeTabId | null => useWorktreeTabStore((s) => s.activeId);

/**
 * Stable bag of every action. Backed by a single `actions` object set once at store creation, so the selector returns a referentially-stable
 * reference and consumers do NOT re-render on every state change. Matches the `useSubSessionActions` convention.
 */
export const useWorktreeTabActions = (): WorktreeTabStoreActions => useWorktreeTabStore((s) => s.actions);

export const useActiveWorktreeTab = (): WorktreeTab | undefined => useWorktreeTabStore((s) => s.tabs.find((t) => t.id === s.activeId));
