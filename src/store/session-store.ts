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

import { useEffect, useMemo, useState } from 'react';
import { create } from 'zustand';
import { useShallow } from 'zustand/react/shallow';

import {
  configSet,
  sessionClose,
  sessionCreate,
  sessionFocus,
  sessionList,
  type SessionCloseResult,
  type SessionCreateArgs,
} from '@/lib/tauri-bridge';
import type {
  SessionActivityEvent,
  SessionId,
  SessionMetrics,
  SessionMetricsEvent,
  SessionStatusEvent,
  SessionView,
} from '@/types/arborist';

export interface SessionStoreState {
  sessions: SessionView[];
  /** Currently focused tab. `undefined` when no session exists. */
  activeId: SessionId | undefined;
  /** Id of the session whose close-confirm modal is open (Phase 9). */
  pendingClose: SessionId | undefined;
  isHydrated: boolean;
  /**
   * Optional human-readable note attached to the most recent status
   * change for each session. Populated by `applyStatus` when the
   * backend includes `message` (e.g. stale-worktree restore failures
   * — Roadmap §4.3). Cleared whenever the next status arrives without
   * a message. Frontend-only — not persisted, not part of the Rust
   * `SessionView` mirror.
   */
  statusMessages: Record<SessionId, string>;
  /**
   * Per-session "has unread output since last activation" flag. Set when
   * `session://output` arrives for a non-active session; cleared on
   * activation. Boolean (not byte count) so we update the store at most
   * once per false→true transition — keeps PTY output off the
   * re-render hot path (see DESIGN §5.2).
   */
  hasUnread: Record<SessionId, true>;
  /**
   * Per-session activity inferred from the `session://activity` event
   * stream. `working` while the agent is producing output; `idle` when
   * quiescent; `attention` when the CLI rang the bell or sent a
   * notification OSC. Cleared (back to `idle`) on focus when the prior
   * value was `attention`. Frontend-only — not persisted.
   */
  activity: Record<SessionId, SessionActivity>;
  /**
   * Latest token-usage / context-window snapshot per session, as emitted
   * on `session://metrics` (Issue #3). Cleared on session close and on
   * status transitions back to `starting` (restart) so stale numbers
   * don't linger in the sidebar.
   */
  metrics: Record<SessionId, SessionMetrics>;
  /**
   * Per-session unix-seconds timestamp of the most recent agent-turn
   * completion. Set when a `session://activity` event with
   * `kind: "turnEnd"` arrives. Drives the `awaiting` display state via
   * [`selectDisplayStatus`]. Frontend-only — not persisted; cleared on
   * close and on `starting` (restart) so the indicator goes back to
   * `idle` until the new run finishes its first turn.
   */
  lastTurnEndAt: Record<SessionId, number>;
  /**
   * Most recently observed agent-turn duration in milliseconds, as
   * reported by the `kind: "turnEnd"` event when the source includes it
   * (Copilot OTel `invoke_agent` span — Claude omits). Surfaced only in
   * tooltips; not part of any layout-affecting state.
   */
  lastTurnDurationMs: Record<SessionId, number>;
  /**
   * Per-session set of currently-open tool invocations, keyed by
   * `toolCallId`. Populated by the Copilot events.jsonl tailer
   * (`toolStart` / `toolEnd`). Drives the `runningTool` display state and
   * tooltip text. Frontend-only — not persisted.
   */
  openTools: Record<SessionId, Record<string, OpenTool>>;
  /**
   * Per-session set of pending permission prompts (Copilot only), keyed
   * by `requestId`. While non-empty, the session is **blocked on the
   * user** — promoted to the highest-priority `awaitingPermission`
   * display state so the sidebar makes the cue impossible to miss.
   * Frontend-only — not persisted.
   */
  openPermissions: Record<SessionId, Record<string, OpenPermission>>;
  /**
   * Per-session "agent is currently inside an assistant turn" flag, set
   * on `turnStart` and cleared on `turnEnd`. Drives the `thinking`
   * display state for the in-turn-with-no-open-tools case.
   * Frontend-only — not persisted.
   */
  inTurn: Record<SessionId, true>;
}

/** A currently-running tool invocation as seen by the events.jsonl tailer. */
export interface OpenTool {
  toolName: string;
  toolCallId: string;
}

/** A currently-pending permission prompt blocking the agent. */
export interface OpenPermission {
  requestId: string;
  permissionKind: string;
  summary: string | null;
}

export type SessionActivity = 'working' | 'idle' | 'attention';

/**
 * Single derived state the sidebar renders as one icon. Combines
 * lifecycle (`session.status`), PTY-derived activity, the most recent
 * agent-turn-end timestamp, and how long the session has been alive. See
 * [`selectDisplayStatus`] for the priority order.
 */
export type DisplayStatus =
  | 'starting'
  | 'error'
  | 'exited'
  | 'awaitingPermission'
  | 'attention'
  | 'runningTool'
  | 'thinking'
  | 'working'
  | 'awaiting'
  | 'idle';

/**
 * Time (in seconds) a freshly-spawned, never-active session must live
 * before the sidebar promotes it from `idle` to `awaiting` ("the agent
 * has booted and is waiting for input"). Without this grace period the
 * tab would flicker to `awaiting` during the first frame of a normal
 * spawn.
 */
export const AWAITING_GRACE_SECONDS = 5;

export interface SessionStoreActions {
  hydrate: () => Promise<void>;
  create: (args: SessionCreateArgs) => Promise<SessionView>;
  close: (
    id: SessionId,
    deleteWorktree?: boolean,
    opts?: { pruneOnError?: boolean },
  ) => Promise<SessionCloseResult>;
  focus: (id: SessionId) => Promise<void>;
  reorder: (ids: SessionId[]) => Promise<void>;
  requestClose: (id: SessionId) => void;
  cancelClose: () => void;
  applyStatus: (evt: SessionStatusEvent) => void;
  /**
   * Mark a session as having received unread output. No-op if the
   * session is currently active or already flagged. Idempotent — safe to
   * call from the high-frequency `session://output` handler.
   */
  noteUnread: (id: SessionId) => void;
  /**
   * Apply a `session://activity` event from the backend. Idempotent: only
   * mutates the store on a true state transition. Safe to call from the
   * high-frequency event handler.
   */
  applyActivity: (evt: SessionActivityEvent) => void;
  /**
   * Apply a `session://metrics` snapshot from the backend. Idempotent
   * for unchanged payloads (the backend also debounces) and defensive
   * against races with `close`.
   */
  applyMetrics: (evt: SessionMetricsEvent) => void;
}

type Store = SessionStoreState & { actions: SessionStoreActions };

const INITIAL_STATE: SessionStoreState = {
  sessions: [],
  activeId: undefined,
  pendingClose: undefined,
  isHydrated: false,
  statusMessages: {},
  hasUnread: {},
  activity: {},
  metrics: {},
  lastTurnEndAt: {},
  lastTurnDurationMs: {},
  openTools: {},
  openPermissions: {},
  inTurn: {},
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
      // Clear any orphan status messages — keys may belong to sessions
      // that no longer exist after the backend reload.
      set({
        sessions,
        isHydrated: true,
        statusMessages: {},
        hasUnread: {},
        activity: {},
        metrics: {},
        lastTurnEndAt: {},
        lastTurnDurationMs: {},
        openTools: {},
        openPermissions: {},
        inTurn: {},
      });
    },

    create: async (args) => {
      const view = await sessionCreate(args);
      set((s) => ({
        sessions: [...s.sessions, view],
        activeId: view.id,
      }));
      return view;
    },

    close: async (id, deleteWorktree, opts) => {
      // By default, converge local state to "session gone" even if the
      // backend call rejects. The PTY may have been killed and the
      // persisted record removed before the failure (e.g. a transient
      // disk error in a later step), and leaving a stale row in the
      // sidebar is a worse UX than briefly out-of-sync with the backend.
      // Worktree-deletion failures arrive as a non-throwing
      // `worktreeDeleteError` field; hard backend failures still
      // propagate to the caller AFTER local pruning.
      //
      // Callers that need failed sessions to remain visible/retryable
      // (e.g. the workspace-switch flow, which wants the user to resolve
      // problems on the original workspace before changing it) can pass
      // `{ pruneOnError: false }` to suppress the pruning side-effect on
      // backend failure. Successful closes always prune.
      const pruneOnError = opts?.pruneOnError ?? true;
      const pruneLocal = (): void => {
        // Read state *inside* the prune so we don't clobber any
        // concurrent updates (e.g. a session created while sessionClose
        // was in flight).
        const {
          sessions,
          activeId,
          pendingClose,
          statusMessages,
          hasUnread,
          activity,
          metrics,
          lastTurnEndAt,
          lastTurnDurationMs,
          openTools,
          openPermissions,
          inTurn,
        } = get();
        if (!sessions.some((s) => s.id === id)) {
          // Already pruned by some other path — nothing to do.
          return;
        }
        const wasActive = activeId === id;
        const nextSessions = sessions.filter((s) => s.id !== id);
        const patch: Partial<SessionStoreState> = { sessions: nextSessions };
        if (wasActive) {
          // Always assign explicitly so `activeId` is cleared when the last
          // session closes.
          patch.activeId = pickNeighbour(sessions, id);
        }
        // `pendingClose` is closed automatically when the session it
        // referenced is gone.
        if (pendingClose === id) patch.pendingClose = undefined;
        // Drop any orphan status-message keyed under this session id.
        if (id in statusMessages) {
          const next = { ...statusMessages };
          delete next[id];
          patch.statusMessages = next;
        }
        if (id in hasUnread) {
          const nextUnread = { ...hasUnread };
          delete nextUnread[id];
          patch.hasUnread = nextUnread;
        }
        if (id in activity) {
          const nextActivity = { ...activity };
          delete nextActivity[id];
          patch.activity = nextActivity;
        }
        if (id in metrics) {
          const nextMetrics = { ...metrics };
          delete nextMetrics[id];
          patch.metrics = nextMetrics;
        }
        if (id in lastTurnEndAt) {
          const next = { ...lastTurnEndAt };
          delete next[id];
          patch.lastTurnEndAt = next;
        }
        if (id in lastTurnDurationMs) {
          const next = { ...lastTurnDurationMs };
          delete next[id];
          patch.lastTurnDurationMs = next;
        }
        if (id in openTools) {
          const next = { ...openTools };
          delete next[id];
          patch.openTools = next;
        }
        if (id in openPermissions) {
          const next = { ...openPermissions };
          delete next[id];
          patch.openPermissions = next;
        }
        if (id in inTurn) {
          const next = { ...inTurn };
          delete next[id];
          patch.inTurn = next;
        }
        set(patch);
      };

      let succeeded = false;
      try {
        const result = await sessionClose({
          sessionId: id,
          deleteWorktree: deleteWorktree ?? false,
        });
        succeeded = true;
        return result;
      } finally {
        if (succeeded || pruneOnError) {
          pruneLocal();
        }
      }
    },

    focus: async (id) => {
      // Optimistic: switching tabs must feel instant.
      const { hasUnread, activity } = get();
      const patch: Partial<SessionStoreState> = { activeId: id };
      if (id in hasUnread) {
        const next = { ...hasUnread };
        delete next[id];
        patch.hasUnread = next;
      }
      // Auto-clear `attention` on focus — the user is now looking at the
      // tab, so the cue has served its purpose. Other states (working /
      // idle) are intrinsic to the session and persist.
      if (activity[id] === 'attention') {
        const nextActivity = { ...activity };
        delete nextActivity[id];
        patch.activity = nextActivity;
      }
      set(patch);
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
      const next: SessionView = { ...current, status: evt.status };
      const nextSessions = sessions.slice();
      nextSessions[idx] = next;
      // Track the optional status message keyed by session id. We
      // overwrite (not merge) so a status transition without a message
      // clears any stale annotation from a prior transition.
      const { statusMessages, metrics } = get();
      const nextMessages = { ...statusMessages };
      if (evt.message !== undefined && evt.message.length > 0) {
        nextMessages[evt.sessionId] = evt.message;
      } else {
        delete nextMessages[evt.sessionId];
      }
      const patch: Partial<SessionStoreState> = {
        sessions: nextSessions,
        statusMessages: nextMessages,
      };
      // On restart (back to `starting`), drop any stale metrics so the
      // sidebar doesn't show numbers from the previous run. Same logic
      // applies to the turn-end markers — a new run hasn't completed any
      // turns yet, so the sidebar should fall back to `idle`.
      if (evt.status === 'starting') {
        if (evt.sessionId in metrics) {
          const nextMetrics = { ...metrics };
          delete nextMetrics[evt.sessionId];
          patch.metrics = nextMetrics;
        }
        const { lastTurnEndAt, lastTurnDurationMs } = get();
        if (evt.sessionId in lastTurnEndAt) {
          const next = { ...lastTurnEndAt };
          delete next[evt.sessionId];
          patch.lastTurnEndAt = next;
        }
        if (evt.sessionId in lastTurnDurationMs) {
          const next = { ...lastTurnDurationMs };
          delete next[evt.sessionId];
          patch.lastTurnDurationMs = next;
        }
      }
      // Phase 2.5: drop any stale events.jsonl-derived state on every
      // status transition that ends or restarts the run. `starting`
      // covers the restart path (backend pre-allocates a fresh
      // ai_session_id, so prior open tools/permissions/in-turn flags are
      // by definition stale). `exited` and `error` cover terminal
      // transitions — `selectDisplayStatus` already short-circuits on
      // those, but leaving the maps populated is a hygiene gap for
      // tooltips and any future consumer that reads them directly.
      if (evt.status === 'starting' || evt.status === 'exited' || evt.status === 'error') {
        const { openTools, openPermissions, inTurn } = get();
        if (evt.sessionId in openTools) {
          const next = { ...openTools };
          delete next[evt.sessionId];
          patch.openTools = next;
        }
        if (evt.sessionId in openPermissions) {
          const next = { ...openPermissions };
          delete next[evt.sessionId];
          patch.openPermissions = next;
        }
        if (evt.sessionId in inTurn) {
          const next = { ...inTurn };
          delete next[evt.sessionId];
          patch.inTurn = next;
        }
      }
      set(patch);
    },

    noteUnread: (id) => {
      const { activeId, hasUnread, sessions } = get();
      if (id === activeId) return;
      if (hasUnread[id]) return;
      // Defensive: ignore unknown session ids (race with close).
      if (!sessions.some((s) => s.id === id)) return;
      set({ hasUnread: { ...hasUnread, [id]: true } });
    },

    applyActivity: (evt) => {
      const {
        sessions,
        activity,
        activeId,
        lastTurnEndAt,
        lastTurnDurationMs,
        openTools,
        openPermissions,
        inTurn,
      } = get();
      // Defensive: drop events for unknown sessions (race with close).
      if (!sessions.some((s) => s.id === evt.sessionId)) return;

      // ---- Copilot events.jsonl variants (Phase 2.5) -----------------
      // These maintain auxiliary state that drives the new
      // `awaitingPermission` / `runningTool` / `thinking` display states
      // without competing with the legacy PTY-byte working/idle/attention
      // axis. They are idempotent for matching ids and defensive against
      // unmatched ends (e.g. tailer started mid-file).
      if (evt.kind === 'turnStart') {
        if (inTurn[evt.sessionId]) return;
        set({ inTurn: { ...inTurn, [evt.sessionId]: true } });
        return;
      }
      if (evt.kind === 'toolStart') {
        const prev = openTools[evt.sessionId] ?? {};
        if (prev[evt.toolCallId]?.toolName === evt.toolName) return;
        set({
          openTools: {
            ...openTools,
            [evt.sessionId]: {
              ...prev,
              [evt.toolCallId]: { toolName: evt.toolName, toolCallId: evt.toolCallId },
            },
          },
        });
        return;
      }
      if (evt.kind === 'toolEnd') {
        const prev = openTools[evt.sessionId];
        if (!prev || !(evt.toolCallId in prev)) return;
        const nextForSession = { ...prev };
        delete nextForSession[evt.toolCallId];
        const nextOpenTools = { ...openTools };
        if (Object.keys(nextForSession).length === 0) {
          delete nextOpenTools[evt.sessionId];
        } else {
          nextOpenTools[evt.sessionId] = nextForSession;
        }
        set({ openTools: nextOpenTools });
        return;
      }
      if (evt.kind === 'awaitingPermission') {
        const prev = openPermissions[evt.sessionId] ?? {};
        const existing = prev[evt.requestId];
        if (
          existing &&
          existing.permissionKind === evt.permissionKind &&
          existing.summary === evt.summary
        ) {
          return;
        }
        set({
          openPermissions: {
            ...openPermissions,
            [evt.sessionId]: {
              ...prev,
              [evt.requestId]: {
                requestId: evt.requestId,
                permissionKind: evt.permissionKind,
                summary: evt.summary,
              },
            },
          },
        });
        return;
      }
      if (evt.kind === 'permissionResolved') {
        const prev = openPermissions[evt.sessionId];
        if (!prev || !(evt.requestId in prev)) return;
        const nextForSession = { ...prev };
        delete nextForSession[evt.requestId];
        const nextOpenPermissions = { ...openPermissions };
        if (Object.keys(nextForSession).length === 0) {
          delete nextOpenPermissions[evt.sessionId];
        } else {
          nextOpenPermissions[evt.sessionId] = nextForSession;
        }
        set({ openPermissions: nextOpenPermissions });
        return;
      }

      // Turn-end is a fire-and-forget marker — it doesn't compete with the
      // PTY-driven working/idle/attention state machine, so handle it
      // first and return. We record both the wall-clock arrival time
      // (drives the `awaiting` display state) and the source-reported
      // duration (tooltip only).
      if (evt.kind === 'turnEnd') {
        const nowSec = Math.floor(Date.now() / 1000);
        const patch: Partial<SessionStoreState> = {
          lastTurnEndAt: { ...lastTurnEndAt, [evt.sessionId]: nowSec },
        };
        if (typeof evt.durationMs === 'number') {
          patch.lastTurnDurationMs = {
            ...lastTurnDurationMs,
            [evt.sessionId]: evt.durationMs,
          };
        } else if (lastTurnDurationMs[evt.sessionId] !== undefined) {
          // Source did not report a duration this time. Drop any prior
          // value so the tooltip doesn't show a stale number from an
          // earlier turn (or from the other tool, after an agent swap).
          const next = { ...lastTurnDurationMs };
          delete next[evt.sessionId];
          patch.lastTurnDurationMs = next;
        }
        // A completed turn implies the agent is no longer producing
        // output — drop a stale `working` flag so the icon flips to
        // `awaiting` immediately rather than waiting for the PTY-stream
        // idle tick. Don't clobber `attention` (still user-relevant).
        if (activity[evt.sessionId] === 'working') {
          const nextActivity = { ...activity };
          delete nextActivity[evt.sessionId];
          patch.activity = nextActivity;
        }
        // Also clear the in-turn flag so `thinking` releases promptly
        // (events.jsonl `turn_end` and OTel `invoke_agent` close races
        // — whichever lands first wins).
        if (inTurn[evt.sessionId]) {
          const nextInTurn = { ...inTurn };
          delete nextInTurn[evt.sessionId];
          patch.inTurn = nextInTurn;
        }
        set(patch);
        return;
      }

      const current = activity[evt.sessionId];
      let next: SessionActivity | undefined;
      switch (evt.kind) {
        case 'working':
          next = 'working';
          break;
        case 'idle':
          // Don't downgrade an `attention` cue to `idle` automatically;
          // attention persists until the user focuses the tab.
          next = current === 'attention' ? 'attention' : 'idle';
          break;
        case 'attention':
          // If the tab is already focused there's no value in showing the
          // cue — drop it.
          if (evt.sessionId === activeId) return;
          next = 'attention';
          break;
        default:
          // title / promptStart / commandStart / commandEnd: not surfaced
          // on the sidebar today.
          return;
      }

      if (current === next) return;
      set({ activity: { ...activity, [evt.sessionId]: next } });
    },

    applyMetrics: (evt) => {
      const { sessions, metrics } = get();
      // Defensive: drop events for unknown sessions (race with close).
      if (!sessions.some((s) => s.id === evt.sessionId)) return;
      const prev = metrics[evt.sessionId];
      // Cheap structural equality: every key/value is a primitive or
      // undefined, so a JSON compare is both correct and fast enough at
      // the rate the backend emits (≤ once per ~2s per session).
      if (prev && JSON.stringify(prev) === JSON.stringify(evt)) return;
      set({ metrics: { ...metrics, [evt.sessionId]: evt } });
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
export const selectStatusMessage =
  (id: SessionId | undefined) =>
  (s: Store): string | undefined =>
    id === undefined ? undefined : s.statusMessages[id];
export const selectHasUnread =
  (id: SessionId | undefined) =>
  (s: Store): boolean =>
    id === undefined ? false : s.hasUnread[id] === true;
export const selectActivity =
  (id: SessionId | undefined) =>
  (s: Store): SessionActivity | undefined =>
    id === undefined ? undefined : s.activity[id];
export const selectMetrics =
  (id: SessionId | undefined) =>
  (s: Store): SessionMetrics | undefined =>
    id === undefined ? undefined : s.metrics[id];
export const selectLastTurnEndAt =
  (id: SessionId | undefined) =>
  (s: Store): number | undefined =>
    id === undefined ? undefined : s.lastTurnEndAt[id];
export const selectLastTurnDurationMs =
  (id: SessionId | undefined) =>
  (s: Store): number | undefined =>
    id === undefined ? undefined : s.lastTurnDurationMs[id];
export const selectOpenTools =
  (id: SessionId | undefined) =>
  (s: Store): Record<string, OpenTool> | undefined =>
    id === undefined ? undefined : s.openTools[id];
export const selectOpenPermissions =
  (id: SessionId | undefined) =>
  (s: Store): Record<string, OpenPermission> | undefined =>
    id === undefined ? undefined : s.openPermissions[id];

/**
 * Derive the single icon-state the sidebar should render for `id`. The
 * priority order is intentional and reflects what a user most needs to
 * notice first:
 *
 *  1. `error` — the session can't do anything until the user reacts.
 *  2. `starting` — the spinner; nothing else is meaningful yet.
 *  3. `exited` — the session has terminated and won't make progress
 *     until the user restarts or recreates it.
 *  4. `awaitingPermission` — the agent is **blocked on the user**
 *     (Copilot permission prompt). Highest signal-to-noise of all the
 *     post-boot states; shown ahead of `attention` because a missed
 *     permission prompt is worse than a missed bell.
 *  5. `attention` — the agent (or the OS) explicitly asked the user to
 *     look here (BEL / OSC 9 / OSC 777).
 *  6. `runningTool` — Copilot is currently executing a tool (events.jsonl
 *     `tool.execution_start` without matching complete). Distinct from
 *     `thinking` because the user can sometimes anticipate what the tool
 *     will do (e.g. "running shell").
 *  7. `thinking` — Copilot is inside an assistant turn (events.jsonl
 *     `turn_start` without matching `turn_end`) but no tool is open.
 *     Distinguishes "model is generating tokens" from the legacy
 *     PTY-byte-fuzzy `working` state.
 *  8. `working` — the PTY is streaming output, i.e. the agent is
 *     producing tokens (legacy fallback for sessions without an
 *     events.jsonl signal — Claude, or Copilot before bootstrap).
 *  9. `awaiting` — the agent finished a turn and is parked at its
 *     prompt, OR the session has booted but never produced a turn and
 *     the [`AWAITING_GRACE_SECONDS`] window has elapsed (a CLI typically
 *     drops to its REPL prompt by then).
 * 10. `idle` — fallback for the brief boot window before
 *     [`AWAITING_GRACE_SECONDS`] expires.
 *
 * `nowSec` is injected so tests can pin time deterministically.
 */
export function selectDisplayStatus(
  id: SessionId | undefined,
  nowSec: number = Math.floor(Date.now() / 1000),
): (s: Store) => DisplayStatus {
  return (s: Store): DisplayStatus => {
    if (id === undefined) return 'idle';
    const session = s.sessions.find((x) => x.id === id);
    if (!session) return 'idle';
    if (session.status === 'error') return 'error';
    if (session.status === 'starting') return 'starting';
    if (session.status === 'exited') return 'exited';
    if (s.openPermissions[id] && Object.keys(s.openPermissions[id]!).length > 0) {
      return 'awaitingPermission';
    }
    const activity = s.activity[id];
    if (activity === 'attention') return 'attention';
    if (s.openTools[id] && Object.keys(s.openTools[id]!).length > 0) {
      return 'runningTool';
    }
    if (s.inTurn[id]) return 'thinking';
    if (activity === 'working') return 'working';
    if (s.lastTurnEndAt[id] !== undefined) return 'awaiting';
    if (nowSec - session.createdAt >= AWAITING_GRACE_SECONDS) return 'awaiting';
    return 'idle';
  };
}

export const useSessions = (): SessionView[] => useSessionStore(selectSessions);
export const useActiveSessionId = (): SessionId | undefined => useSessionStore(selectActiveId);
export const usePendingClose = (): SessionId | undefined => useSessionStore(selectPendingClose);
export const useIsHydrated = (): boolean => useSessionStore(selectIsHydrated);
export const useStatusMessage = (id: SessionId | undefined): string | undefined =>
  useSessionStore(selectStatusMessage(id));
export const useHasUnread = (id: SessionId | undefined): boolean =>
  useSessionStore(selectHasUnread(id));
export const useActivity = (id: SessionId | undefined): SessionActivity | undefined =>
  useSessionStore(selectActivity(id));
export const useMetrics = (id: SessionId | undefined): SessionMetrics | undefined =>
  useSessionStore(selectMetrics(id));
export const useLastTurnEndAt = (id: SessionId | undefined): number | undefined =>
  useSessionStore(selectLastTurnEndAt(id));
export const useLastTurnDurationMs = (id: SessionId | undefined): number | undefined =>
  useSessionStore(selectLastTurnDurationMs(id));
export const useOpenTools = (id: SessionId | undefined): Record<string, OpenTool> | undefined =>
  useSessionStore(selectOpenTools(id));
export const useOpenPermissions = (
  id: SessionId | undefined,
): Record<string, OpenPermission> | undefined => useSessionStore(selectOpenPermissions(id));

/**
 * Subscribe to the derived `DisplayStatus` for `id`. Recomputes (and
 * re-renders the caller) once a second via a tick so the time-based
 * `idle → awaiting` promotion fires within ~1s of the
 * [`AWAITING_GRACE_SECONDS`] boundary even if no other event touches
 * the session. The 1s cadence is cheap (one shared timer for the whole
 * app) and keeps the displayed status close to its documented timing.
 */
export function useDisplayStatus(id: SessionId | undefined): DisplayStatus {
  const tickedNow = useNowTickSeconds();
  return useSessionStore(selectDisplayStatus(id, tickedNow));
}

/**
 * Returns `Math.floor(Date.now() / 1000)` and re-renders the caller
 * roughly once a second. Used by [`useDisplayStatus`] to drive the
 * boot-grace transition.
 *
 * Implemented as a single shared interval with reference-counted
 * subscribers — N tabs ⇒ N components ⇒ one timer, not N. The interval
 * is created lazily on first subscribe and torn down when the last
 * subscriber unmounts. The cadence is intentionally well below
 * [`AWAITING_GRACE_SECONDS`] (5s) so the documented transition lands
 * close to its boundary rather than potentially trailing it. Internal —
 * not exported as part of the public store API.
 */
let nowTickSubscribers = 0;
let nowTickHandle: number | undefined;
const nowTickListeners = new Set<(value: number) => void>();
const NOW_TICK_INTERVAL_MS = 1_000;

function ensureNowTickRunning(): void {
  if (nowTickHandle !== undefined) return;
  nowTickHandle = window.setInterval(() => {
    const value = Math.floor(Date.now() / 1000);
    for (const listener of nowTickListeners) listener(value);
  }, NOW_TICK_INTERVAL_MS);
}

function teardownNowTickIfUnused(): void {
  if (nowTickSubscribers > 0 || nowTickHandle === undefined) return;
  window.clearInterval(nowTickHandle);
  nowTickHandle = undefined;
}

// Vite HMR: when this module is hot-replaced in dev, the old module's
// setInterval keeps firing while the new module instance creates a
// fresh one — left unchecked, every save accumulates another ticker.
// Clear the handle and drop subscribers on dispose so the new module
// starts from a clean slate.
if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    if (nowTickHandle !== undefined) {
      window.clearInterval(nowTickHandle);
      nowTickHandle = undefined;
    }
    nowTickListeners.clear();
    nowTickSubscribers = 0;
  });
}

function useNowTickSeconds(): number {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    nowTickSubscribers += 1;
    nowTickListeners.add(setNow);
    ensureNowTickRunning();
    return () => {
      nowTickListeners.delete(setNow);
      nowTickSubscribers -= 1;
      teardownNowTickIfUnused();
    };
  }, []);
  return now;
}

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
