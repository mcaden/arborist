// In-memory store of worktree-tab close lifecycle state (round-3 fix for
// PR #221).
//
// The close operation runs entirely synchronously inside `worktreeTabClose`
// (one IPC call, no backend events) but can take 10-60 seconds on Windows
// while we cascade-close sub-sessions and retry the git/fs cleanup against
// AV/file-watcher/grandchild-handle races. We deliberately do NOT introduce
// a new event channel — instead the calling `worktree-tab-store.close`
// action wraps the await with `markStarted` / `markCompleted` so the
// `WorktreeCloseBanner` can render live feedback. This mirrors the UX of
// `WorktreePrepBanner` without coupling the close path to a backend event
// the user wouldn't otherwise need.
//
// Conventions match the prep store: successful completions auto-dismiss
// from `recent` after the banner's timer; failures stay sticky until the
// user dismisses them so they don't fly by.

import { create } from 'zustand';

import type { WorktreeTabCloseResult, WorktreeTabId } from '@/types/arborist';

const RECENT_LIMIT = 10;

export interface CloseRunningRecord {
  state: 'running';
  tabId: WorktreeTabId;
  // Worktree path (full string); the banner trims to leaf for display.
  worktreePath: string;
  willDelete: boolean;
  startedAt: number;
}

export interface CloseCompletedRecord {
  state: 'completed';
  tabId: WorktreeTabId;
  worktreePath: string;
  willDelete: boolean;
  startedAt: number;
  finishedAt: number;
  // Coarse outcome — banner colour-codes on this. `attention` means the
  // backend returned but had partial issues the user should see (delete
  // refused for live apps, sub-session kill unconfirmed, etc.).
  status: 'success' | 'attention' | 'failure';
  // Human-readable summary. For success this is empty; for attention /
  // failure it's whatever the caller built from the result.
  message: string;
}

export type CloseRecord = CloseRunningRecord | CloseCompletedRecord;

export interface WorktreeCloseStoreState {
  inFlight: Record<WorktreeTabId, CloseRunningRecord>;
  recent: CloseCompletedRecord[];
  markStarted: (rec: { tabId: WorktreeTabId; worktreePath: string; willDelete: boolean }) => void;
  markCompleted: (rec: {
    tabId: WorktreeTabId;
    status: 'success' | 'attention' | 'failure';
    message?: string;
    result?: WorktreeTabCloseResult;
  }) => void;
  dismissCompleted: (tabId: WorktreeTabId) => void;
  _resetForTest: () => void;
}

export const useWorktreeCloseStore = create<WorktreeCloseStoreState>((set) => ({
  inFlight: {},
  recent: [],
  markStarted: ({ tabId, worktreePath, willDelete }) =>
    set((s) => ({
      inFlight: {
        ...s.inFlight,
        [tabId]: {
          state: 'running',
          tabId,
          worktreePath,
          willDelete,
          startedAt: Date.now(),
        },
      },
    })),
  markCompleted: ({ tabId, status, message }) =>
    set((s) => {
      const { [tabId]: started, ...restInFlight } = s.inFlight;
      const startedAt = started?.startedAt ?? Date.now();
      const worktreePath = started?.worktreePath ?? '';
      const willDelete = started?.willDelete ?? false;
      const completed: CloseCompletedRecord = {
        state: 'completed',
        tabId,
        worktreePath,
        willDelete,
        startedAt,
        finishedAt: Date.now(),
        status,
        message: message ?? '',
      };
      const filtered = s.recent.filter((r) => r.tabId !== tabId);
      let successCount = 0;
      const next = [completed, ...filtered].filter((r) => {
        if (r.status !== 'success') return true;
        successCount += 1;
        return successCount <= RECENT_LIMIT;
      });
      return { inFlight: restInFlight, recent: next };
    }),
  dismissCompleted: (tabId) => set((s) => ({ recent: s.recent.filter((r) => r.tabId !== tabId) })),
  _resetForTest: () => {
    set({ inFlight: {}, recent: [] });
  },
}));

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export const selectInFlightCloses = (s: WorktreeCloseStoreState): readonly CloseRunningRecord[] => Object.values(s.inFlight);

export const selectRecentCompletedCloses = (s: WorktreeCloseStoreState): readonly CloseCompletedRecord[] => s.recent;
