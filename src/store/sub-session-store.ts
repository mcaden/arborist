// Zustand-backed cache of live sub-session metadata. Mirrors the backend's
// sub-session list (terminal + application kinds) so the React tree can
// subscribe granularly to changes.
//
// Scope (Phase 4):
// * Holds `SubSession` records grouped by parent session id.
// * Tracks the active sub-tab per parent (`undefined` means the parent
//   session itself is active, not a sub-tab).
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
import type {
  SessionId,
  SubSession,
  SubSessionCloseIntent,
  SubSessionCreateArgs,
  SubSessionExitedEvent,
  SubSessionId,
  SubSessionRestoredEvent,
  SubSessionStatusEvent,
} from '@/types/arborist';

export interface SubSessionStoreState {
  /** Flat list of all known sub-sessions, in creation order per parent. */
  subSessions: SubSession[];
  /**
   * Active sub-tab per parent. `undefined` for a parent whose own
   * terminal is showing (no sub-tab selected). Keyed by parent
   * `SessionId`.
   */
  activeByParent: Record<SessionId, SubSessionId>;
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
  /** Spawn a new sub-session under `args.parentSessionId`. */
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
   * Focus a sub-session: marks it active in `activeByParent` and (for
   * application kind) calls the backend focuser. Terminal kind is a
   * pure UI swap.
   */
  focus: (id: SubSessionId) => Promise<void>;
  /**
   * Activate the parent session itself (clears `activeByParent` for
   * that parent so the parent's terminal viewport shows).
   */
  activateParent: (parentId: SessionId) => void;
  /**
   * Drop all sub-sessions for a parent (used when the parent session
   * is closed — the cascade itself happens in the backend in Phase 7,
   * but the frontend converges the cache locally so the UI is
   * consistent immediately).
   */
  dropForParent: (parentId: SessionId) => void;
  /**
   * Re-spawn a sub-session under its existing id (Phase 7). Used by
   * `SidebarSubTab` when the user clicks a greyed-out application
   * sub-tab; also valid for terminal sub-tabs whose PTY died. Per-id
   * dedupe prevents a double-click from spawning twice.
   */
  relaunch: (id: SubSessionId) => Promise<void>;
  // --- event handlers (called from sub-session-events.ts) ---
  applyStatus: (event: SubSessionStatusEvent) => void;
  applyExited: (event: SubSessionExitedEvent) => void;
  /**
   * Insert a sub-session received via `subsession://restored` (Phase 7
   * restore-on-launch). Idempotent on duplicate restores; never steals
   * `activeByParent` away from a sub-tab the parent already owns.
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

  const actions: SubSessionStoreActions = {
    hydrate: async () => {
      const all = await subSessionList();
      // Preserve UI-only `activeByParent`, but drop entries that no
      // longer reference a real sub-session under the same parent —
      // otherwise selectors point at nothing after a backend resync.
      const valid: Record<SessionId, SubSessionId> = {};
      const { activeByParent: prev } = get();
      for (const [parent, activeId] of Object.entries(prev)) {
        const match = all.find((sub) => sub.id === activeId && sub.parentSessionId === parent);
        if (match) valid[parent as SessionId] = activeId;
      }
      set({
        subSessions: all,
        isHydrated: true,
        activeByParent: valid,
      });
    },

    create: async (args) => {
      const sub = await subSessionCreate(args);
      set((s) => ({
        subSessions: [...s.subSessions, sub],
        activeByParent: { ...s.activeByParent, [sub.parentSessionId]: sub.id },
      }));
      return sub;
    },

    close: async (id, intent) => {
      // Capture parent before we mutate so we can re-pick a neighbour.
      const sub = get().subSessions.find((s) => s.id === id);
      try {
        await subSessionClose(id, intent);
      } finally {
        // Always converge local state — same rationale as session-store
        // close: leaving a stale row in the sidebar is worse than briefly
        // out-of-sync with the backend.
        const { subSessions, activeByParent, statusMessages, pendingClose } = get();
        const next = subSessions.filter((s) => s.id !== id);
        const nextActive: Record<SessionId, SubSessionId> = { ...activeByParent };
        if (sub && activeByParent[sub.parentSessionId] === id) {
          // Pick a neighbour under the same parent, or clear (back to
          // parent terminal) if none remain.
          const neighbour = pickNeighbour(subSessions, sub.parentSessionId, id);
          if (neighbour) {
            nextActive[sub.parentSessionId] = neighbour;
          } else {
            delete nextActive[sub.parentSessionId];
          }
        }
        const nextMsgs: Record<SubSessionId, string> = { ...statusMessages };
        delete nextMsgs[id];
        set({
          subSessions: next,
          activeByParent: nextActive,
          statusMessages: nextMsgs,
          // Auto-clear pendingClose if the dialog was open for the row
          // we just closed (e.g. SubCloseConfirmDialog confirmed).
          pendingClose: pendingClose === id ? undefined : pendingClose,
        });
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
      if (sub.kind === 'terminal') {
        // Optimistic UI: mark active immediately so the swap feels instant.
        set((s) => ({
          activeByParent: { ...s.activeByParent, [sub.parentSessionId]: id },
        }));
      }
      // Backend focus is only meaningful for application kind. Terminal
      // sub-sessions are a pure tab swap; the backend impl is a no-op.
      // Either way the call is cheap and centralises the dispatch.
      // For application kind we deliberately do NOT touch activeByParent
      // — clicking an app sub-tab is a focus gesture, not a viewport
      // swap (CONTEXT_MENU_PLAN.md, Frontend §9).
      await subSessionFocus(id);
    },

    activateParent: (parentId) => {
      set((s) => {
        if (!(parentId in s.activeByParent)) return {};
        const next = { ...s.activeByParent };
        delete next[parentId];
        return { activeByParent: next };
      });
    },

    dropForParent: (parentId) => {
      set((s) => {
        const next = s.subSessions.filter((sub) => sub.parentSessionId !== parentId);
        if (next.length === s.subSessions.length && !(parentId in s.activeByParent)) {
          return {};
        }
        const nextActive = { ...s.activeByParent };
        delete nextActive[parentId];
        // Drop status messages for the orphaned ids.
        const droppedIds = new Set(s.subSessions.filter((sub) => sub.parentSessionId === parentId).map((sub) => sub.id));
        const nextMsgs: Record<SubSessionId, string> = {};
        for (const [k, v] of Object.entries(s.statusMessages)) {
          if (!droppedIds.has(k as SubSessionId)) nextMsgs[k as SubSessionId] = v;
        }
        // If a close-confirm dialog was open for one of the removed
        // rows, drop it — the row is gone so the dialog has no target.
        const nextPending = s.pendingClose !== undefined && droppedIds.has(s.pendingClose) ? undefined : s.pendingClose;
        return {
          subSessions: next,
          activeByParent: nextActive,
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
    },

    applyRestored: (event) => {
      // Phase 7: insert a sub-session received from the restore-on-launch
      // second pass. Idempotent — if the row is already in the cache
      // (e.g. a duplicate restored event, or the user has already
      // hydrated via subsession_list) we leave it alone. We also never
      // steal `activeByParent` from a tab the parent already owns —
      // restore happens before the user has clicked anything but the
      // hydrate path may have repopulated `activeByParent` from the
      // session-store's persisted active sub-tab.
      set((s) => {
        const incoming = event.subSession;
        if (s.subSessions.some((sub) => sub.id === incoming.id)) return {};
        const nextSubs = [...s.subSessions, incoming];
        const nextActive: Record<SessionId, SubSessionId> = { ...s.activeByParent };
        if (!(incoming.parentSessionId in nextActive)) {
          // Parent has no active sub-tab yet — adopt this one so the
          // restored row is visible if the parent gets focused. For
          // application kind this is harmless (clicking an app sub-tab
          // doesn't swap the viewport anyway).
          //
          // We DON'T set this for application kind to preserve the
          // existing rule that app sub-tabs never claim the viewport.
          if (incoming.kind === 'terminal') {
            nextActive[incoming.parentSessionId] = incoming.id;
          }
        }
        return { subSessions: nextSubs, activeByParent: nextActive };
      });
    },
  };

  return {
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    pendingClose: undefined,
    isHydrated: false,
    actions,
  };
});

function pickNeighbour(list: SubSession[], parentId: SessionId, removingId: SubSessionId): SubSessionId | undefined {
  const siblings = list.filter((s) => s.parentSessionId === parentId);
  const idx = siblings.findIndex((s) => s.id === removingId);
  if (idx === -1) return siblings[0]?.id;
  // Prefer the next sibling, fall back to the previous.
  return (siblings[idx + 1] ?? siblings[idx - 1])?.id;
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export const selectAllSubSessions = (s: Store): SubSession[] => s.subSessions;
export const selectActiveByParent =
  (parentId: SessionId | undefined) =>
  (s: Store): SubSessionId | undefined =>
    parentId ? s.activeByParent[parentId] : undefined;
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

export function useSubSessionsForParent(parentId: SessionId | undefined): SubSession[] {
  return useSubSessionStore(useShallow((s) => (parentId ? s.subSessions.filter((sub) => sub.parentSessionId === parentId) : [])));
}

export function useActiveSubSessionId(parentId: SessionId | undefined): SubSessionId | undefined {
  return useSubSessionStore(useMemo(() => selectActiveByParent(parentId), [parentId]));
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
