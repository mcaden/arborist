// Zustand-backed cache of live session metadata. Mirrors the backend's
// session list so the React tree can subscribe granularly to changes.
//
// Scope (Phase 8):
// * Holds `SessionView` records + UI-only fields (`activeId`, `pendingClose`).
// * Exposes thin actions that wrap the relevant Tauri commands and keep the
//   cache in sync optimistically.
// * Subscribes to `session://status` (via `lib/session-events.ts`) to update
//   per-session status as the backend reports it.
//
// **Explicitly out of scope**: `session://output` is *never* routed through
// this store. PTY bytes go straight from the bridge to the xterm `use-terminal`
// hook (Phase 11). Routing them through Zustand would re-render every
// subscriber on every keystroke.
//
// Conventions (mirroring `config-store.ts`):
// * Components subscribe via the granular selectors exported below — never
//   `useSessionStore(s => s)`.
// * Actions don't mutate state; every `set` produces a fresh object/array.
// * `useSessionActions()` returns a stable action bag so callers can pull it
//   once and not re-render when state changes.

import { useMemo } from 'react';
import { create } from 'zustand';
import { useShallow } from 'zustand/react/shallow';

import {
  configSet,
  sessionClose,
  sessionCreate,
  sessionFocus,
  sessionList,
  type SessionCreateArgs,
} from '@/lib/tauri-bridge';
import type { SessionId, SessionStatusEvent, SessionView } from '@/types/grove';

export interface SessionStoreState {
  sessions: SessionView[];
  /** Currently focused tab. `undefined` when no session exists. */
  activeId: SessionId | undefined;
  /** Id of the session whose close-confirm modal is open (Phase 9). */
  pendingClose: SessionId | undefined;
  isHydrated: boolean;
}

export interface SessionStoreActions {
  hydrate: () => Promise<void>;
  create: (args: SessionCreateArgs) => Promise<SessionView>;
  close: (id: SessionId) => Promise<void>;
  focus: (id: SessionId) => Promise<void>;
  reorder: (ids: SessionId[]) => Promise<void>;
  requestClose: (id: SessionId) => void;
  cancelClose: () => void;
  applyStatus: (evt: SessionStatusEvent) => void;
}

type Store = SessionStoreState & { actions: SessionStoreActions };

const INITIAL_STATE: SessionStoreState = {
  sessions: [],
  activeId: undefined,
  pendingClose: undefined,
  isHydrated: false,
};

/**
 * After removing `closedId` from `sessions`, pick the next session to focus:
 * the one previously to the right; if none, the one to the left; otherwise
 * `undefined`. `previousSessions` is the list *before* removal, ordered as
 * the user sees it (i.e. by `tabIndex`).
 */
function pickNeighbour(
  previousSessions: SessionView[],
  closedId: SessionId,
): SessionId | undefined {
  const idx = previousSessions.findIndex((s) => s.id === closedId);
  if (idx === -1) return undefined;
  const right = previousSessions[idx + 1];
  if (right) return right.id;
  const left = previousSessions[idx - 1];
  if (left) return left.id;
  return undefined;
}

export const useSessionStore = create<Store>((set, get) => {
  const actions: SessionStoreActions = {
    hydrate: async () => {
      const sessions = await sessionList();
      set({ sessions, isHydrated: true });
    },

    create: async (args) => {
      const view = await sessionCreate(args);
      set((s) => ({
        sessions: [...s.sessions, view],
        activeId: view.id,
      }));
      return view;
    },

    close: async (id) => {
      await sessionClose({ sessionId: id });
      const previous = get().sessions;
      const wasActive = get().activeId === id;
      const nextSessions = previous.filter((s) => s.id !== id);
      const patch: Partial<SessionStoreState> = { sessions: nextSessions };
      if (wasActive) {
        // Always assign explicitly so `activeId` is cleared when the last
        // session closes.
        patch.activeId = pickNeighbour(previous, id);
      }
      // `pendingClose` is closed automatically when the session it referenced
      // is gone.
      if (get().pendingClose === id) patch.pendingClose = undefined;
      set(patch);
    },

    focus: async (id) => {
      // Optimistic: switching tabs must feel instant.
      set({ activeId: id });
      try {
        await sessionFocus({ sessionId: id });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        // No rollback — focus is UI-driven and a backend reject just means
        // the persisted active marker is stale. The user's intent stands.
        console.warn(`[session-store] session_focus(${id}) rejected: ${message}`);
      }
    },

    reorder: async (ids) => {
      const byId = new Map(get().sessions.map((s) => [s.id, s] as const));
      const reordered: SessionView[] = [];
      for (const id of ids) {
        const view = byId.get(id);
        if (view) reordered.push(view);
      }
      // Preserve any sessions not mentioned in `ids` at the tail (defensive —
      // callers should pass a complete order, but we don't want to silently
      // drop tabs).
      for (const view of get().sessions) {
        if (!ids.includes(view.id)) reordered.push(view);
      }
      set({ sessions: reordered });
      // Diff-only: only the field that actually changed.
      await configSet({ tabOrder: ids });
    },

    requestClose: (id) => {
      set({ pendingClose: id });
    },

    cancelClose: () => {
      set({ pendingClose: undefined });
    },

    applyStatus: (evt) => {
      const sessions = get().sessions;
      const idx = sessions.findIndex((s) => s.id === evt.sessionId);
      if (idx === -1) {
        // Race: status arrived after the session was closed locally. Drop
        // silently — the backend will catch up.
        console.debug(`[session-store] dropping status for unknown session ${evt.sessionId}`);
        return;
      }
      const current = sessions[idx]!;
      // NOTE: the backend `SessionStatusEvent` currently carries only
      // `status` (see `src-tauri/src/types.rs::SessionStatusEvent`). If a
      // future phase widens that payload to include `pid`/exit code, mirror
      // it here.
      const next: SessionView = { ...current, status: evt.status };
      const nextSessions = sessions.slice();
      nextSessions[idx] = next;
      set({ sessions: nextSessions });
    },
  };

  return { ...INITIAL_STATE, actions };
});

// ---------------------------------------------------------------------------
// Granular selectors. Components should reach for these instead of pulling
// the whole store; doing so keeps re-renders tight.
// ---------------------------------------------------------------------------

export const selectSessions = (s: Store): SessionView[] => s.sessions;
export const selectActiveId = (s: Store): SessionId | undefined => s.activeId;
export const selectPendingClose = (s: Store): SessionId | undefined => s.pendingClose;
export const selectIsHydrated = (s: Store): boolean => s.isHydrated;

export const useSessions = (): SessionView[] => useSessionStore(selectSessions);
export const useActiveSessionId = (): SessionId | undefined => useSessionStore(selectActiveId);
export const usePendingClose = (): SessionId | undefined => useSessionStore(selectPendingClose);
export const useIsHydrated = (): boolean => useSessionStore(selectIsHydrated);

export function useActiveSession(): SessionView | undefined {
  const sessions = useSessions();
  const activeId = useActiveSessionId();
  return useMemo(
    () => (activeId ? sessions.find((s) => s.id === activeId) : undefined),
    [sessions, activeId],
  );
}

export function useSessionById(id: SessionId | undefined): SessionView | undefined {
  const sessions = useSessions();
  return useMemo(() => (id ? sessions.find((s) => s.id === id) : undefined), [sessions, id]);
}

const selectActions = (s: Store): SessionStoreActions => s.actions;

/**
 * Stable bag of every action. Subscribes via shallow equality on the
 * `actions` object, which is created exactly once in the store factory, so
 * this hook never causes a re-render itself.
 */
export function useSessionActions(): SessionStoreActions {
  return useSessionStore(useShallow(selectActions));
}
