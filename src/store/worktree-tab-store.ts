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
  /**
   * Load persisted worktree tabs and reconcile against the live session list. Any session whose `worktreePath` has no matching tab triggers an
   * idempotent `worktreeTabOpen` to self-heal — covers the orphan case where a previous run crashed between `session_create` and the frontend's
   * follow-up `worktreeTabOpen`. Pass `knownPaths` from the freshly hydrated session-store so this store stays free of cross-store imports.
   * Throws on bridge failure so `App.boot`'s error overlay can surface the underlying problem instead of silently rendering an empty sidebar.
   */
  hydrate: (knownPaths?: ReadonlyArray<string>) => Promise<void>;
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
      set({ tabs: reconciled, isHydrated: true, activeId });
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
