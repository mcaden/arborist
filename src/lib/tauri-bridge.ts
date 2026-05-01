// Typed wrappers around Tauri's `invoke()` and `listen()` APIs. All
// frontend code MUST go through this module — never import from
// `@tauri-apps/api` directly. This is the single source of truth for the
// frontend half of the command/event contract documented in
// `dev/docs/DESIGN.md` §6.
//
// Phase 3 status: only `ping` is implemented. Every other command listed in
// DESIGN §6 is stubbed with `Promise.reject(new Error('not implemented'))`
// so callers can be written and typed against the final shape; later phases
// will flip stubs to real implementations one at a time.
//
// The mock counterpart in `./tauri-bridge.mock.ts` is structurally
// substitutable — see the `satisfies typeof import(...)` check in that
// file.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { open as openDialog } from '@tauri-apps/plugin-dialog';

export { formatError, isAppErrorLike } from '@/lib/tauri-error';
export type { AppErrorLike } from '@/lib/tauri-error';

import type {
  AppConfig,
  InstructionSet,
  InstructionSetId,
  PartialAppConfig,
  SessionId,
  SessionOutputEvent,
  SessionStatusEvent,
  SessionActivityEvent,
  SessionMetricsEvent,
  SessionView,
  SubSession,
  SubSessionCreateArgs,
  SubSessionExitedEvent,
  SubSessionId,
  SubSessionInputArgs,
  SubSessionListArgs,
  SubSessionResizeArgs,
  SubSessionRestoredEvent,
  SubSessionStatusEvent,
  Tool,
  WorktreeInfo,
  WorkspaceValidateResult,
  WorktreeCreateResult,
} from '@/types/arborist';

// ---------------------------------------------------------------------------
// Command argument shapes
//
// MIRROR: the keys here mirror the payload column of DESIGN §6. When a real
// implementation lands in a later phase, the matching Rust `#[serde(...)]`
// payload struct in `src-tauri/src/types.rs` (or a sibling) becomes the
// canonical definition and these aliases must be re-checked.
// ---------------------------------------------------------------------------

export interface SessionCreateArgs {
  tool: Tool;
  worktreePath: string;
  instructionSetId?: InstructionSetId;
}

export interface SessionIdArg {
  sessionId: SessionId;
}

/**
 * Arguments for `session_close`. The optional `deleteWorktree` flag asks
 * the backend to run `git worktree remove --force` on the session's
 * worktree after the PTY is torn down. The backend refuses to delete the
 * configured workspace root (main worktree).
 *
 * MIRROR: `src-tauri/src/types.rs::SessionCloseArgs`.
 */
export interface SessionCloseArgs {
  sessionId: SessionId;
  deleteWorktree?: boolean;
}

/**
 * Result of `session_close`. The session record + PTY are always torn
 * down on success; if the user opted into worktree deletion and the
 * `git worktree remove` step failed, that failure is reported here as a
 * warning string instead of as a hard error so the UI can converge on a
 * "tab gone" state regardless.
 *
 * MIRROR: `src-tauri/src/types.rs::SessionCloseResult`.
 */
export interface SessionCloseResult {
  worktreeDeleteError: string | null;
}

export interface SessionResizeArgs {
  sessionId: SessionId;
  cols: number;
  rows: number;
}

export interface SessionInputArgs {
  sessionId: SessionId;
  data: string;
}

// ---------------------------------------------------------------------------
// Smoke-test command
// ---------------------------------------------------------------------------

/**
 * Round-trips through the Tauri command boundary, returning `'pong'`.
 * Used in development and tests to verify the RPC scaffold is wired.
 */
export function ping(): Promise<string> {
  return invoke<string>('ping');
}

// ---------------------------------------------------------------------------
// Session lifecycle commands (Phase 7).
// ---------------------------------------------------------------------------

/**
 * Create a new session backed by a freshly-spawned PTY child. The returned
 * [`SessionView`] reflects the post-spawn state (status `running`, populated
 * `pid`).
 */
export function sessionCreate(args: SessionCreateArgs): Promise<SessionView> {
  return invoke<SessionView>('session_create', { args });
}

/**
 * Returns every persisted session as a [`SessionView`], ordered by
 * `tabIndex`.
 */
export function sessionList(): Promise<SessionView[]> {
  return invoke<SessionView[]>('session_list');
}

/**
 * Kill the PTY child, drop the persisted session record, and trim the
 * session out of `lastOpenSessions`/`tabOrder`/`activeSessionId`. Idempotent
 * for already-exited sessions. When `deleteWorktree` is `true`, the backend
 * additionally runs `git worktree remove --force` on the session's
 * worktree; failures of that step surface in
 * [`SessionCloseResult.worktreeDeleteError`] rather than as a thrown
 * error, so callers can always treat a fulfilled promise as "session
 * gone".
 */
export function sessionClose(args: SessionCloseArgs): Promise<SessionCloseResult> {
  return invoke<SessionCloseResult>('session_close', { args });
}

/**
 * Mark `sessionId` as the persisted active session. Errors with `NotFound`
 * if the session is not in the store.
 */
export function sessionFocus(args: SessionIdArg): Promise<void> {
  return invoke<void>('session_focus', { args });
}

/** Resize the PTY of the given session. */
export function sessionResize(args: SessionResizeArgs): Promise<void> {
  return invoke<void>('session_resize', { args });
}

/** Write `data` to the PTY master of the given session. */
export function sessionInput(args: SessionInputArgs): Promise<void> {
  return invoke<void>('session_input', { args });
}

/**
 * Re-spawn `sessionId` from its persisted `composedCommand`. The command
 * is reused verbatim — never recomposed (DESIGN §5.4).
 */
export function sessionRestart(args: SessionIdArg): Promise<void> {
  return invoke<void>('session_restart', { args });
}

/**
 * Signals the backend that the frontend is now subscribed to
 * `session://output` and `session://status`. The first call triggers
 * restore-on-launch; subsequent calls are no-ops on the backend side
 * (DESIGN §5.5).
 */
export function frontendReady(): Promise<void> {
  return invoke<void>('frontend_ready');
}

/**
 * Returns the persisted [`AppConfig`]. Path fields are canonicalized by the
 * backend; missing instruction-set IDs are silently rewritten to the
 * discovered default for the relevant tool.
 */
export function configGet(): Promise<AppConfig> {
  return invoke<AppConfig>('config_get');
}

/**
 * Deep-merges `partial` into the persisted [`AppConfig`]. Only the fields
 * present on `partial` (i.e. not `undefined`) are touched; the rest survive
 * unchanged. Backend may reject relative paths with an `InvalidPath` error.
 */
export function configSet(partial: PartialAppConfig): Promise<void> {
  return invoke<void>('config_set', { partial });
}

/**
 * Discovers and returns the list of [`InstructionSet`]s under the configured
 * `instructionSetsDir`. Files exceeding 1 MiB or escaping the directory via
 * symlink are skipped.
 */
export function instructionsList(): Promise<InstructionSet[]> {
  return invoke<InstructionSet[]>('instructions_list');
}

/**
 * Enumerate git worktrees rooted at `repoRoot`. Always resolves with a
 * (possibly empty) array — the backend swallows discovery failures so the
 * UI can fall back to the manual "Browse…" picker (DESIGN §6, Phase 10).
 */
export function worktreesList(repoRoot: string): Promise<WorktreeInfo[]> {
  return invoke<WorktreeInfo[]>('worktrees_list', { repoRoot });
}

/**
 * Validate a candidate workspace root. Resolves with `{ valid: true }` when
 * the path is an absolute, existing directory containing a git repository,
 * or `{ valid: false, error }` otherwise. Never rejects for "invalid path"
 * — the picker shows inline feedback (Roadmap §1.1).
 */
export function workspaceValidate(path: string): Promise<WorkspaceValidateResult> {
  return invoke<WorkspaceValidateResult>('workspace_validate', {
    args: { path },
  });
}

/**
 * Create a new linked git worktree at `<workspaceRoot>/.worktrees/<name>`
 * on a fresh branch named `<name>`. Rejects with `AppError` on validation
 * or git failure (Roadmap §2.2).
 */
export function worktreeCreate(name: string): Promise<WorktreeCreateResult> {
  return invoke<WorktreeCreateResult>('worktree_create', {
    args: { name },
  });
}

/**
 * Open the OS native directory picker. Resolves to the absolute path the
 * user chose, or `null` if they cancelled. Backed by the
 * `tauri-plugin-dialog` plugin (Phase 10).
 *
 * Components MUST go through this wrapper rather than importing the plugin
 * directly so the bridge mock can stub it in tests.
 */
export async function pickDirectory(): Promise<string | null> {
  const picked = await openDialog({ directory: true, multiple: false });
  // The plugin returns `string | string[] | null` depending on `multiple`;
  // we asked for a single selection so any non-string is treated as cancel.
  return typeof picked === 'string' ? picked : null;
}

// ---------------------------------------------------------------------------
// Event subscribers
//
// The returned `UnlistenFn` MUST be called by the consumer on cleanup
// (typically inside a `useEffect` cleanup function). Forgetting to do so
// leaks the listener for the lifetime of the WebView.
// ---------------------------------------------------------------------------

export function onSessionOutput(cb: (payload: SessionOutputEvent) => void): Promise<UnlistenFn> {
  return listen<SessionOutputEvent>('session://output', (event) => cb(event.payload));
}

export function onSessionStatus(cb: (payload: SessionStatusEvent) => void): Promise<UnlistenFn> {
  return listen<SessionStatusEvent>('session://status', (event) => cb(event.payload));
}

export function onSessionActivity(
  cb: (payload: SessionActivityEvent) => void,
): Promise<UnlistenFn> {
  return listen<SessionActivityEvent>('session://activity', (event) => cb(event.payload));
}

export function onSessionMetrics(cb: (payload: SessionMetricsEvent) => void): Promise<UnlistenFn> {
  return listen<SessionMetricsEvent>('session://metrics', (event) => cb(event.payload));
}

// ---------------------------------------------------------------------------
// Phase 2: sub-session commands & events.
//
// Sub-sessions are children of a session and represent the "+ button"
// items chosen from the tab context menu. Phase 2 ships *terminal* kind
// (a second PTY in the same worktree); Phase 3 adds *application* kind
// (detached external windows). Output for terminal sub-sessions reuses
// the existing `session://output` channel because the UUID id space is
// global; status changes get their own `subsession://status` channel.
// ---------------------------------------------------------------------------

export function subSessionCreate(args: SubSessionCreateArgs): Promise<SubSession> {
  return invoke<SubSession>('subsession_create', { args });
}

export function subSessionClose(id: SubSessionId): Promise<void> {
  return invoke<void>('subsession_close', { args: { id } });
}

export function subSessionFocus(id: SubSessionId): Promise<void> {
  return invoke<void>('subsession_focus', { args: { id } });
}

export function subSessionList(parentSessionId?: SessionId): Promise<SubSession[]> {
  const args: SubSessionListArgs = parentSessionId === undefined ? {} : { parentSessionId };
  return invoke<SubSession[]>('subsession_list', { args });
}

export function subSessionInput(args: SubSessionInputArgs): Promise<void> {
  return invoke<void>('subsession_input', { args });
}

export function subSessionResize(args: SubSessionResizeArgs): Promise<void> {
  return invoke<void>('subsession_resize', { args });
}

/**
 * Re-spawn a sub-session under its existing id (Phase 7). Used when the
 * user clicks a greyed-out application sub-tab whose external process has
 * already exited; also valid for terminal sub-tabs whose PTY has died.
 *
 * The persisted record (id, parent, defId, kind) is unchanged. The
 * `composedCommand` is re-derived from the current
 * `AppConfig.customProcesses` entry, so user edits to the def take effect
 * on relaunch. Status flows back via the existing `subsession://status`
 * channel.
 */
export function subSessionRelaunch(id: SubSessionId): Promise<SubSession> {
  return invoke<SubSession>('subsession_relaunch', { args: { id } });
}

/**
 * Best-effort fetch of the OS application icon for an
 * `application`-kind sub-session, returned as a `data:image/png;base64,…`
 * URI. Returns `null` when extraction is not possible (terminal
 * sub-session, PID exited, platform unsupported, miss, …) — the
 * caller falls back to the generic emoji.
 *
 * Backed by `IconCache` (see `src-tauri/src/process_icon.rs`); the
 * same exe path is only extracted once per process lifetime.
 */
export function subSessionIcon(id: SubSessionId): Promise<string | null> {
  return invoke<string | null>('subsession_icon', { args: { id } });
}

export function onSubSessionStatus(
  cb: (payload: SubSessionStatusEvent) => void,
): Promise<UnlistenFn> {
  return listen<SubSessionStatusEvent>('subsession://status', (event) => cb(event.payload));
}

export function onSubSessionExited(
  cb: (payload: SubSessionExitedEvent) => void,
): Promise<UnlistenFn> {
  return listen<SubSessionExitedEvent>('subsession://exited', (event) => cb(event.payload));
}

/**
 * Subscribe to `subsession://restored` events emitted once per sub-session
 * by the restore-on-launch second pass (Phase 7). Carries the full
 * `SubSession` payload so the frontend store can hydrate the row in a
 * single update without an extra `subsession_list` round-trip.
 */
export function onSubSessionRestored(
  cb: (payload: SubSessionRestoredEvent) => void,
): Promise<UnlistenFn> {
  return listen<SubSessionRestoredEvent>('subsession://restored', (event) => cb(event.payload));
}
