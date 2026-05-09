// Zustand-backed cache of live sub-session metadata. Mirrors the backend's
// sub-session list (terminal + application kinds) so the React tree can
// subscribe granularly to changes.
//
// Scope:
// * Holds `SubSession` records grouped by parent worktree tab id.
// * Exposes thin actions that wrap `subsession_*` commands and keep the
//   cache in sync optimistically.
// * Subscribes to `subsession://status` and `subsession://exited` (via
//   `lib/sub-session-events.ts`) to update per-sub-session status as the
//   backend reports it.
//
// **Explicitly out of scope**: `session://output` for terminal sub-tabs is
// *not* routed through this store (same rule as the parent-session store —
// PTY bytes go straight to the xterm hook to avoid re-rendering on every
// keystroke).
//
// Conventions (mirroring `session-store.ts`):
// * Components subscribe via the granular selectors / hooks below — never
//   `useSubSessionStore(s => s)`.
// * Actions don't mutate state; every `set` produces a fresh object/array.
// * `useSubSessionActions()` returns a stable action bag.

import { useMemo } from 'react';
import { create } from 'zustand';
import { useShallow } from 'zustand/react/shallow';

import { formatError, subSessionClose, subSessionCreate, subSessionFocus, subSessionList, subSessionRelaunch } from '@/lib/tauri-bridge';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type {
  SubSession,
  SubSessionCloseIntent,
  SubSessionCreateArgs,
  SubSessionExitedEvent,
  SubSessionId,
  SubSessionRestoredEvent,
  SubSessionStatusEvent,
  WorktreeTabId,
} from '@/types/arborist';

export interface SubSessionStoreState {
  /** Flat list of all known sub-sessions, in creation order per worktree tab. */
  subSessions: SubSession[];
  /**
   * Optional human-readable note attached to the most recent status
   * change for each sub-session (mirror of session-store pattern).
   * Today only set when the backend includes `message` in
   * `subsession://status`. Cleared on the next message-less status.
   */
  statusMessages: Record<SubSessionId, string>;
  /**
   * Sub-session id whose close-confirmation dialog is currently open,
   * if any. Set by [`requestClose`] and cleared by [`cancelClose`] or
   * any successful [`close`] call.
   *
   * Mirrors the parent-session store's `pendingClose` UI pattern so
   * `SubCloseConfirmDialog` can mount declaratively from a shared
   * layout component instead of being passed handlers prop-by-prop.
   */
  pendingClose: SubSessionId | undefined;
  isHydrated: boolean;
}

export interface SubSessionStoreActions {
  /** One-shot pull of the backend list. Idempotent. */
  hydrate: () => Promise<void>;
  /** Spawn a new sub-session under `args.parentWorktreeTabId`. */
  create: (args: SubSessionCreateArgs) => Promise<SubSession>;
  /**
   * Close a sub-session. `intent` controls what happens to the
   * underlying process (see [`SubSessionCloseIntent`]); when omitted
   * the backend defaults to `tabOnly`. Removes the row from the
   * cache regardless of intent — failure to terminate the external
   * window is logged but doesn't keep the tab visible.
   */
  close: (id: SubSessionId, intent?: SubSessionCloseIntent) => Promise<void>;
  /**
   * Open the close-confirmation dialog for `id`. Used by
   * `SidebarSubTab` for app-kind sub-tabs whose underlying window we
   * could politely close (`requestAppClose`); terminal-kind and
   * already-exited app-kind sub-tabs short-circuit straight to
   * [`close`] (`tabOnly`) instead.
   */
  requestClose: (id: SubSessionId) => void;
  /** Dismiss the close-confirmation dialog without closing. */
  cancelClose: () => void;
  /**
   * Focus a sub-session: for terminal kind, a pure UI swap via
   * worktree-tab `setActiveChild`; for application kind, calls the
   * backend focuser.
   */
  focus: (id: SubSessionId) => Promise<void>;
  /**
   * Drop all sub-sessions for a worktree tab (used when the worktree
   * tab is closed — the cascade itself happens in the backend, but
   * the frontend converges the cache locally so the UI is consistent
   * immediately).
   */
  dropForWorktreeTab: (tabId: WorktreeTabId) => void;
  /**
   * Re-spawn a sub-session under its existing id. Used by
   * `SidebarSubTab` when the user clicks a greyed-out application
   * sub-tab; also valid for terminal sub-tabs whose PTY died. Per-id
   * dedupe prevents a double-click from spawning twice.
   */
  relaunch: (id: SubSessionId) => Promise<void>;
  // --- event handlers (called from sub-session-events.ts) ---
  applyStatus: (event: SubSessionStatusEvent) => void;
  applyExited: (event: SubSessionExitedEvent) => void;
  /**
   * Insert a sub-session received via `subsession://restored`.
   * Idempotent on duplicate restores.
   */
  applyRestored: (event: SubSessionRestoredEvent) => void;
}

type Store = SubSessionStoreState & { actions: SubSessionStoreActions };

type SubStatus = SubSession['status'];

/**
 * Exhaustive check for "terminal" sub-session statuses (no further
 * transitions, PID is gone). Uses a `Record` so adding a new
 * `SubSessionStatus` variant fails the type-check until handled.
 */
function isTerminalStatus(status: SubStatus): boolean {
  const map: Record<SubStatus, boolean> = {
    starting: false,
    running: false,
    exited: true,
    error: true,
  };
  return map[status];
}

/**
 * Build a SubSession from `current` with new status and an optional pid.
 * `exactOptionalPropertyTypes: true` rejects `pid: undefined` literals
 * because `pid?: number` only allows number-or-omitted; this helper
 * destructures the existing pid out and only re-adds it when a real
 * number is supplied. Use everywhere a status transition needs to
 * conditionally clear the PID (e.g. relaunch flip, terminal-state
 * fallback in `applyExited`, snapshot rollback).
 */
function withStatusAndPid(current: SubSession, status: SubStatus, pid: number | undefined): SubSession {
  const { pid: _omit, ...rest } = current;
  void _omit;
  const next: SubSession = { ...rest, status };
  if (pid !== undefined) next.pid = pid;
  return next;
}

export const useSubSessionStore = create<Store>((set, get) => {
  // Per-id dedupe set for in-flight `relaunch` calls. Lives outside
  // the Zustand state object because it's purely operational and would
  // cause needless re-renders if it triggered subscribers. A second
  // click on the same sub-tab while the first relaunch is in flight is
  // a no-op.
  const relaunchPending = new Set<SubSessionId>();

  // Application-kind sub-sessions whose `create` call resolved while
  // status was still `starting` and that should receive focus once they
  // reach `running`. `subsession_focus_impl` rejects focus on app-kind
  // subs that aren't yet running, which caused the inconsistent
  // post-spawn focus reported in #50 — focus succeeded only when the
  // spawn happened to finish before the call landed. We stage the
  // intent here and drain it in `applyStatus` when the running
  // transition arrives. Lives outside Zustand state so it doesn't
  // trigger subscriber re-renders.
  //
  // Scope is intentionally narrow: only `create` populates this set.
  // `relaunch` deliberately does NOT — per the comment on that action,
  // clicking a greyed-out app sub-tab to revive it is a revive gesture
  // and shouldn't steal viewport focus from whatever the user is
  // currently working on.
  const pendingFocus = new Set<SubSessionId>();

  const actions: SubSessionStoreActions = {
    hydrate: async () => {
      const all = await subSessionList();
      set({
        subSessions: all,
        isHydrated: true,
      });
    },

    create: async (args) => {
      const sub = await subSessionCreate(args);
      set((s) => ({
        subSessions: [...s.subSessions, sub],
      }));
      // Auto-focus the freshly spawned sub-session (#50). Terminal-kind
      // focus is a pure UI swap and works at any status, so dispatch
      // immediately. Application-kind focus needs `running` on the
      // backend; if we're already there, fire now, otherwise stage and
      // let `applyStatus` drain when the status flips.
      if (sub.kind === 'terminal' || sub.status === 'running') {
        void actions.focus(sub.id).catch((err) => {
          console.warn(`[sub-session-store] auto-focus after create(${sub.id}) failed: ${formatError(err)}`);
        });
      } else {
        pendingFocus.add(sub.id);
      }
      return sub;
    },

    close: async (id, intent) => {
      const closingSub = get().subSessions.find((s) => s.id === id);
      pendingFocus.delete(id);
      try {
        await subSessionClose(id, intent);
      } finally {
        // Always converge local state — same rationale as session-store
        // close: leaving a stale row in the sidebar is worse than briefly
        // out-of-sync with the backend.
        const { subSessions, statusMessages, pendingClose } = get();
        const next = subSessions.filter((s) => s.id !== id);
        const nextMsgs: Record<SubSessionId, string> = { ...statusMessages };
        delete nextMsgs[id];
        set({
          subSessions: next,
          statusMessages: nextMsgs,
          // Auto-clear pendingClose if the dialog was open for the row
          // we just closed (e.g. SubCloseConfirmDialog confirmed).
          pendingClose: pendingClose === id ? undefined : pendingClose,
        });
        if (closingSub) {
          const wttState = useWorktreeTabStore.getState();
          const tab = wttState.tabs.find((t) => t.id === closingSub.parentWorktreeTabId);
          if (tab?.activeChildId?.kind === 'subSession' && tab.activeChildId.id === id) {
            void wttState.actions.setActiveChild(tab.id, null).catch((err) => {
              console.warn(`[sub-session-store] setActiveChild after close(${id}) failed: ${formatError(err)}`);
            });
          }
        }
      }
    },

    requestClose: (id) => {
      set({ pendingClose: id });
    },

    cancelClose: () => {
      set({ pendingClose: undefined });
    },

    focus: async (id) => {
      const sub = get().subSessions.find((s) => s.id === id);
      if (!sub) return;

      // Terminal sub-sessions must also claim the parent worktree tab's
      // active child so MainArea swaps to this PTY. Application
      // sub-sessions only raise their external window and do not own the
      // in-app viewport.
      if (sub.kind === 'terminal') {
        const wttState = useWorktreeTabStore.getState();
        const tab = wttState.tabs.find((t) => t.id === sub.parentWorktreeTabId);
        if (tab) {
          const wttActions = wttState.actions;
          void wttActions.focus(tab.id);
          void wttActions.setActiveChild(tab.id, { kind: 'subSession', id }).catch((err) => {
            console.warn(`[sub-session-store] setActiveChild after focus(${id}) failed: ${formatError(err)}`);
          });
        }
      }

      await subSessionFocus(id);
    },

    dropForWorktreeTab: (tabId) => {
      const droppedIds = new Set(
        get()
          .subSessions.filter((sub) => sub.parentWorktreeTabId === tabId)
          .map((sub) => sub.id),
      );
      if (droppedIds.size === 0) return;
      for (const id of droppedIds) pendingFocus.delete(id);
      set((s) => {
        const next = s.subSessions.filter((sub) => !droppedIds.has(sub.id));
        const nextMsgs: Record<SubSessionId, string> = {};
        for (const [k, v] of Object.entries(s.statusMessages)) {
          if (!droppedIds.has(k as SubSessionId)) nextMsgs[k as SubSessionId] = v;
        }
        // If a close-confirm dialog was open for one of the removed
        // rows, drop it — the row is gone so the dialog has no target.
        const nextPending = s.pendingClose !== undefined && droppedIds.has(s.pendingClose) ? undefined : s.pendingClose;
        return {
          subSessions: next,
          statusMessages: nextMsgs,
          pendingClose: nextPending,
        };
      });
    },

    relaunch: async (id) => {
      // Per-id dedupe — second click while the first call is in flight
      // is a no-op. Prevents accidental double-spawns from a quick
      // double-click on a greyed app tab.
      if (relaunchPending.has(id)) return;
      relaunchPending.add(id);

      // Optimistic UI: flip status to `starting` immediately so the
      // greyed row visually transitions before the real status event
      // arrives. We deliberately don't touch `activeByParent` — for
      // application kind that's a focus gesture and clicking a greyed
      // tab to relaunch shouldn't steal viewport focus.
      //
      // Snapshot the prior `{status, pid, message}` so we can roll back
      // if the bridge call rejects (capability denial, NotFound,
      // backend error). Without rollback the row would otherwise stay
      // stuck in `starting` indefinitely because no status event
      // arrives for a call that never reached the backend lifecycle.
      // Snapshot of the row pre-flip so we can roll back on backend
      // rejection. Wrapped in a single-prop object so TypeScript's
      // narrowing tracks the assignment across the inner closures —
      // a bare `let` would narrow to `null` after the initial assignment
      // because the inner `set()` callback doesn't update the outer
      // control-flow analysis.
      type PriorSnapshot = {
        status: SubStatus;
        pid: number | undefined;
        message: string | undefined;
      };
      const snapshotRef: { current: PriorSnapshot | null } = { current: null };
      set((s) => {
        const idx = s.subSessions.findIndex((sub) => sub.id === id);
        if (idx === -1) return {};
        const current = s.subSessions[idx]!;
        snapshotRef.current = {
          status: current.status,
          pid: current.pid,
          message: s.statusMessages[id],
        };
        const nextSubs = [...s.subSessions];
        nextSubs[idx] = withStatusAndPid(current, 'starting', undefined);
        const nextMsgs: Record<SubSessionId, string> = { ...s.statusMessages };
        delete nextMsgs[id];
        return { subSessions: nextSubs, statusMessages: nextMsgs };
      });

      try {
        await subSessionRelaunch(id);
        // Status flows back via subsession://status — no further local
        // mutation needed.
      } catch (err) {
        // Rollback: restore the row to whatever status it had before
        // the optimistic flip and surface the failure as a status
        // message so the user can see what happened. We re-throw so
        // call sites still see the rejection.
        const snapshot = snapshotRef.current;
        if (snapshot) {
          set((s) => {
            const idx = s.subSessions.findIndex((sub) => sub.id === id);
            if (idx === -1) return {};
            const current = s.subSessions[idx]!;
            const nextSubs = [...s.subSessions];
            nextSubs[idx] = withStatusAndPid(current, snapshot.status, snapshot.pid);
            const nextMsgs: Record<SubSessionId, string> = { ...s.statusMessages };
            const failMsg = formatError(err);
            if (failMsg) {
              nextMsgs[id] = failMsg;
            } else if (snapshot.message !== undefined) {
              nextMsgs[id] = snapshot.message;
            }
            return { subSessions: nextSubs, statusMessages: nextMsgs };
          });
        }
        throw err;
      } finally {
        relaunchPending.delete(id);
      }
    },

    applyStatus: (event) => {
      set((s) => {
        const idx = s.subSessions.findIndex((sub) => sub.id === event.id);
        if (idx === -1) return {};
        const current = s.subSessions[idx]!;
        // PID forced to omitted for terminal states (mirror of the
        // backend's `set_status` rule — keeps frontend in lockstep).
        // `withStatusAndPid` deletes `pid` instead of setting it to
        // `undefined` so `exactOptionalPropertyTypes: true` is happy.
        const nextPid = isTerminalStatus(event.status) ? undefined : (event.pid ?? current.pid);
        const updated: SubSession = withStatusAndPid(current, event.status, nextPid);
        const nextSubs = [...s.subSessions];
        nextSubs[idx] = updated;
        const nextMsgs: Record<SubSessionId, string> = { ...s.statusMessages };
        if (event.message) {
          nextMsgs[event.id] = event.message;
        } else if (event.id in nextMsgs) {
          delete nextMsgs[event.id];
        }
        return { subSessions: nextSubs, statusMessages: nextMsgs };
      });
      // Drain any deferred auto-focus once the backend reports the
      // process is actually running — see `pendingFocus` in `create`
      // (#50). Terminal states clear the entry without focusing so a
      // sub that died before reaching `running` doesn't haunt the set.
      if (pendingFocus.has(event.id)) {
        if (event.status === 'running') {
          pendingFocus.delete(event.id);
          void actions.focus(event.id).catch((err) => {
            console.warn(`[sub-session-store] deferred auto-focus for ${event.id} failed: ${formatError(err)}`);
          });
        } else if (isTerminalStatus(event.status)) {
          pendingFocus.delete(event.id);
        }
      }
    },

    applyExited: (event) => {
      // `subsession://exited` is a companion signal to the terminal-status
      // `subsession://status { status: 'exited' | 'error' }` event; the
      // canonical status update is the latter. This handler is a fallback
      // for the case where the status event is missed or out-of-order:
      // pick the status from `exitCode` (non-zero → error, otherwise
      // exited). Idempotent if status is already terminal.
      set((s) => {
        const idx = s.subSessions.findIndex((sub) => sub.id === event.id);
        if (idx === -1) return {};
        const current = s.subSessions[idx]!;
        if (isTerminalStatus(current.status)) return {};
        const synthetic: SubSession['status'] = event.exitCode !== undefined && event.exitCode !== 0 ? 'error' : 'exited';
        const nextSubs = [...s.subSessions];
        nextSubs[idx] = withStatusAndPid(current, synthetic, undefined);
        return { subSessions: nextSubs };
      });
      // Process never reached `running`; drop any deferred focus so it
      // doesn't fire if a new sub-session reuses the id later.
      pendingFocus.delete(event.id);
    },

    applyRestored: (event) => {
      // Insert a sub-session received from the restore-on-launch second
      // pass. Idempotent — if the row is already in the cache (e.g. a
      // duplicate restored event, or the user has already hydrated via
      // subsession_list) we leave it alone.
      set((s) => {
        const incoming = event.subSession;
        if (s.subSessions.some((sub) => sub.id === incoming.id)) return {};
        return { subSessions: [...s.subSessions, incoming] };
      });
    },
  };

  return {
    subSessions: [],
    statusMessages: {},
    pendingClose: undefined,
    isHydrated: false,
    actions,
  };
});

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export const selectAllSubSessions = (s: Store): SubSession[] => s.subSessions;
export const selectIsHydrated = (s: Store): boolean => s.isHydrated;
export const selectSubStatusMessage =
  (id: SubSessionId | undefined) =>
  (s: Store): string | undefined =>
    id ? s.statusMessages[id] : undefined;
export const selectPendingSubClose = (s: Store): SubSessionId | undefined => s.pendingClose;

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

export const useAllSubSessions = (): SubSession[] => useSubSessionStore(selectAllSubSessions);

export function useSubSessionsForWorktreeTab(tabId: WorktreeTabId | undefined): SubSession[] {
  return useSubSessionStore(useShallow((s) => (tabId ? s.subSessions.filter((sub) => sub.parentWorktreeTabId === tabId) : [])));
}

export function useSubSessionById(id: SubSessionId | undefined): SubSession | undefined {
  return useSubSessionStore(useShallow((s) => s.subSessions.find((sub) => sub.id === id)));
}

export const useIsSubHydrated = (): boolean => useSubSessionStore(selectIsHydrated);

export const useSubStatusMessage = (id: SubSessionId | undefined): string | undefined =>
  useSubSessionStore(useMemo(() => selectSubStatusMessage(id), [id]));

export const usePendingSubClose = (): SubSessionId | undefined => useSubSessionStore(selectPendingSubClose);

export function useSubSessionActions(): SubSessionStoreActions {
  return useSubSessionStore((s) => s.actions);
}
