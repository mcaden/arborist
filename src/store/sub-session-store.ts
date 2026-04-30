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

import {
  subSessionClose,
  subSessionCreate,
  subSessionFocus,
  subSessionList,
} from '@/lib/tauri-bridge';
import type {
  SessionId,
  SubSession,
  SubSessionCreateArgs,
  SubSessionExitedEvent,
  SubSessionId,
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
  isHydrated: boolean;
}

export interface SubSessionStoreActions {
  /** One-shot pull of the backend list. Idempotent. */
  hydrate: () => Promise<void>;
  /** Spawn a new sub-session under `args.parentSessionId`. */
  create: (args: SubSessionCreateArgs) => Promise<SubSession>;
  /** Close a sub-session. Removes it from the cache. */
  close: (id: SubSessionId) => Promise<void>;
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
  // --- event handlers (called from sub-session-events.ts) ---
  applyStatus: (event: SubSessionStatusEvent) => void;
  applyExited: (event: SubSessionExitedEvent) => void;
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

export const useSubSessionStore = create<Store>((set, get) => {
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

    close: async (id) => {
      // Capture parent before we mutate so we can re-pick a neighbour.
      const sub = get().subSessions.find((s) => s.id === id);
      try {
        await subSessionClose(id);
      } finally {
        // Always converge local state — same rationale as session-store
        // close: leaving a stale row in the sidebar is worse than briefly
        // out-of-sync with the backend.
        const { subSessions, activeByParent, statusMessages } = get();
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
        set({ subSessions: next, activeByParent: nextActive, statusMessages: nextMsgs });
      }
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
        const droppedIds = new Set(
          s.subSessions.filter((sub) => sub.parentSessionId === parentId).map((sub) => sub.id),
        );
        const nextMsgs: Record<SubSessionId, string> = {};
        for (const [k, v] of Object.entries(s.statusMessages)) {
          if (!droppedIds.has(k as SubSessionId)) nextMsgs[k as SubSessionId] = v;
        }
        return { subSessions: next, activeByParent: nextActive, statusMessages: nextMsgs };
      });
    },

    applyStatus: (event) => {
      set((s) => {
        const idx = s.subSessions.findIndex((sub) => sub.id === event.id);
        if (idx === -1) return {};
        const current = s.subSessions[idx]!;
        const updated: SubSession = {
          ...current,
          status: event.status,
          // PID forced to `undefined` for terminal states (mirror of the
          // backend's `set_status` rule — keeps frontend in lockstep).
          pid: isTerminalStatus(event.status) ? undefined : (event.pid ?? current.pid),
        };
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
        const synthetic: SubSession['status'] =
          event.exitCode !== undefined && event.exitCode !== 0 ? 'error' : 'exited';
        const nextSubs = [...s.subSessions];
        nextSubs[idx] = { ...current, status: synthetic, pid: undefined };
        return { subSessions: nextSubs };
      });
    },
  };

  return {
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    isHydrated: false,
    actions,
  };
});

function pickNeighbour(
  list: SubSession[],
  parentId: SessionId,
  removingId: SubSessionId,
): SubSessionId | undefined {
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

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

export const useAllSubSessions = (): SubSession[] => useSubSessionStore(selectAllSubSessions);

export function useSubSessionsForParent(parentId: SessionId | undefined): SubSession[] {
  return useSubSessionStore(
    useShallow((s) =>
      parentId ? s.subSessions.filter((sub) => sub.parentSessionId === parentId) : [],
    ),
  );
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

export function useSubSessionActions(): SubSessionStoreActions {
  return useSubSessionStore((s) => s.actions);
}
