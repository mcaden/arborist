// In-memory store of worktree-prep run state (issue #63).
//
// The Rust backend owns prep execution. It emits a `worktree://prep` event
// when a run starts and again when it exits (see `src-tauri/src/worktree_prep.rs`
// and the `tauri-bridge::onWorktreePrep` wrapper). This store keeps a tiny
// view of those events so the `WorktreePrepBanner` component can render
// live status without each banner mounting its own listener.
//
// Conventions:
// * The `exited` handler is **idempotent** and self-sufficient: it upserts
//   the completion record even when the matching `started` event was missed
//   (e.g. a sub-second prep that exited before the listener attached during
//   app boot). The Rust event payload carries every field the UI needs.
// * Successful completions auto-dismiss from `recent` after a short delay
//   (handled by the banner component, not here — the store just records
//   the most-recent N completions).
// * The store deliberately keeps NO long-term history: prep records vanish
//   on app restart. Auto-resuming previously-running prep across restarts
//   is intentionally out of scope (see plan.md "Notes / open questions").

import { create } from 'zustand';

import { onWorktreePrep } from '@/lib/tauri-bridge';
import type { UnlistenFn } from '@tauri-apps/api/event';
import type { WorktreePrepEvent, WorktreePrepId } from '@/types/arborist';

/** How many recent completions to retain for surface-after-the-fact UI. */
const RECENT_LIMIT = 10;

export interface PrepRunningRecord {
  state: 'running';
  prepId: WorktreePrepId;
  worktreePath: string;
  logPath: string;
  command: string;
  startedAt: number;
}

export interface PrepCompletedRecord {
  state: 'completed';
  prepId: WorktreePrepId;
  worktreePath: string;
  logPath: string;
  /** `null` when the child was signalled or failed to spawn. */
  exitCode: number | null;
  /** `null` unless `exitCode` is `null`, in which case the spawn/signal reason. */
  errorMessage: string | null;
  startedAt: number;
  finishedAt: number;
  /** Convenience: derived from `exitCode === 0` && `errorMessage === null`. */
  ok: boolean;
}

export type PrepRecord = PrepRunningRecord | PrepCompletedRecord;

export interface WorktreePrepStoreState {
  /** Currently-executing preps keyed by `prepId`. */
  inFlight: Record<WorktreePrepId, PrepRunningRecord>;
  /** Most-recent completions, newest first; capped at [`RECENT_LIMIT`]. */
  recent: PrepCompletedRecord[];
  /**
   * Attach the backend event listener. Returns the same `UnlistenFn` for
   * the caller (typically the boot effect in `App.tsx`) to invoke on
   * teardown. Safe to call multiple times — each call yields an
   * independent subscription, but the typical pattern is one subscribe
   * per app instance.
   */
  subscribe: () => Promise<UnlistenFn>;
  /** Drop a completion from `recent` (e.g. after auto-dismiss timer fires). */
  dismissCompleted: (prepId: WorktreePrepId) => void;
  /** Test helper: wipe state without touching subscriptions. */
  _resetForTest: () => void;
}

function applyStarted(state: WorktreePrepStoreState, ev: Extract<WorktreePrepEvent, { kind: 'started' }>): Partial<WorktreePrepStoreState> {
  return {
    inFlight: {
      ...state.inFlight,
      [ev.prepId]: {
        state: 'running',
        prepId: ev.prepId,
        worktreePath: ev.worktreePath,
        logPath: ev.logPath,
        command: ev.command,
        startedAt: ev.startedAt,
      },
    },
  };
}

function applyExited(state: WorktreePrepStoreState, ev: Extract<WorktreePrepEvent, { kind: 'exited' }>): Partial<WorktreePrepStoreState> {
  const { [ev.prepId]: _removed, ...rest } = state.inFlight;
  void _removed;
  const completed: PrepCompletedRecord = {
    state: 'completed',
    prepId: ev.prepId,
    worktreePath: ev.worktreePath,
    logPath: ev.logPath,
    exitCode: ev.exitCode,
    errorMessage: ev.errorMessage,
    startedAt: ev.startedAt,
    finishedAt: ev.finishedAt,
    ok: ev.exitCode === 0 && ev.errorMessage === null,
  };
  // Replace any prior record for this prepId, then prepend so newest is first.
  const filtered = state.recent.filter((r) => r.prepId !== ev.prepId);
  const next = [completed, ...filtered].slice(0, RECENT_LIMIT);
  return { inFlight: rest, recent: next };
}

export const useWorktreePrepStore = create<WorktreePrepStoreState>((set) => ({
  inFlight: {},
  recent: [],
  subscribe: () =>
    onWorktreePrep((ev) => {
      set((state) => (ev.kind === 'started' ? applyStarted(state, ev) : applyExited(state, ev)));
    }),
  dismissCompleted: (prepId) => {
    set((state) => ({ recent: state.recent.filter((r) => r.prepId !== prepId) }));
  },
  _resetForTest: () => {
    set({ inFlight: {}, recent: [] });
  },
}));

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export const selectInFlightPreps = (s: WorktreePrepStoreState): readonly PrepRunningRecord[] => Object.values(s.inFlight);

export const selectRecentCompletedPreps = (s: WorktreePrepStoreState): readonly PrepCompletedRecord[] => s.recent;
