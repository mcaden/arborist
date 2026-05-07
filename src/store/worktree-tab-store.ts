// Zustand store for worktree tabs (Issue #44).
//
// Holds the list of WorktreeTab records and the active worktree tab ID.
// Actions wrap the relevant Tauri commands and keep the cache in sync.
//
// Conventions:
// * Components subscribe via granular selectors — never `useWorktreeTabStore(s => s)`.
// * Actions don't mutate state; every `set` produces a fresh object/array.

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

  // Actions
  hydrate: () => Promise<void>;
  open: (path: string) => Promise<WorktreeTab>;
  close: (id: WorktreeTabId) => Promise<void>;
  focus: (id: WorktreeTabId) => Promise<void>;
  reorder: (ids: WorktreeTabId[]) => Promise<void>;
  setActiveChild: (id: WorktreeTabId, childId: ChildId | null) => Promise<void>;
  /** Sync a single tab's activeChildId locally (e.g. after session focus). */
  patchActiveChild: (id: WorktreeTabId, childId: ChildId | null) => void;
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

export const useWorktreeTabStore = create<WorktreeTabStoreState>()((set, get) => ({
  tabs: [],
  activeId: null,
  isHydrated: false,

  async hydrate() {
    try {
      const [tabs, cfg] = await Promise.all([worktreeTabList(), configGet()]);
      const activeId = cfg.activeWorktreeTabId ?? tabs[0]?.id ?? null;
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
    await worktreeTabFocus({ id });
    set({ activeId: id });
  },

  async reorder(ids: WorktreeTabId[]) {
    await worktreeTabReorder({ ids });
    set((s) => {
      const byId = new Map(s.tabs.map((t) => [t.id, t]));
      const reordered = ids
        .map((id, idx) => {
          const tab = byId.get(id);
          return tab ? { ...tab, tabIndex: idx } : undefined;
        })
        .filter((t): t is WorktreeTab => t !== undefined);
      return { tabs: reordered };
    });
  },

  async setActiveChild(id: WorktreeTabId, childId: ChildId | null) {
    const args = childId !== null ? { id, childId } : { id };
    await worktreeTabSetActiveChild(args);
    get().patchActiveChild(id, childId);
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
}));

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export const useWorktreeTabs = (): WorktreeTab[] => useWorktreeTabStore((s) => s.tabs);
export const useActiveWorktreeTabId = (): WorktreeTabId | null => useWorktreeTabStore((s) => s.activeId);
// Stable action references — extracted once at store creation, never change.
const actionsSelector = (s: WorktreeTabStoreState) => ({
  hydrate: s.hydrate,
  open: s.open,
  close: s.close,
  focus: s.focus,
  reorder: s.reorder,
  setActiveChild: s.setActiveChild,
  patchActiveChild: s.patchActiveChild,
});

export const useWorktreeTabActions = (): Pick<
  WorktreeTabStoreState,
  'hydrate' | 'open' | 'close' | 'focus' | 'reorder' | 'setActiveChild' | 'patchActiveChild'
> => useWorktreeTabStore(actionsSelector);

export const useActiveWorktreeTab = (): WorktreeTab | undefined => useWorktreeTabStore((s) => s.tabs.find((t) => t.id === s.activeId));
