// Hand-written TypeScript mirrors of the Rust types in
// `src-tauri/src/types.rs`. Each interface carries a `MIRROR:` marker
// pointing at the canonical Rust definition. **When you change a Rust struct,
// update the matching interface here in the same commit.**
//
// Drift is enforced at test time by `arborist.test.ts`: every fixture under
// `./fixtures/` must satisfy its declared TS type *and* must have exactly
// the same key set. A renamed Rust field will fail the fixture round-trip on
// the Rust side and the satisfies-check here on the TS side.

// MIRROR: src-tauri/src/types.rs::SessionId
export type SessionId = string;

// MIRROR: src-tauri/src/types.rs::SubSessionId
// Wire shape is identical to SessionId (a UUID string) but the Rust side
// uses a distinct newtype so the compiler enforces the boundary.
export type SubSessionId = string;

// MIRROR: src-tauri/src/types.rs::CustomProcessDefId
// User-facing slug for a `CustomProcessDef`. Matches `[a-zA-Z0-9_-]+` and
// is unique within `AppConfig.customProcesses`. Built-in IDs: `shell`,
// `open-folder`, `vscode`.
export type CustomProcessDefId = string;

// MIRROR: src-tauri/src/types.rs::InstructionSetId
export type InstructionSetId = string;

// MIRROR: src-tauri/src/types.rs::WorktreeTabId
// Stable identifier for a WorktreeTab. Backed by a UUID v4 on the Rust side;
// distinct from SessionId/SubSessionId at the type level.
export type WorktreeTabId = string;

// MIRROR: src-tauri/src/types.rs::Tool
export type Tool = 'claude' | 'copilot';

// MIRROR: src-tauri/src/types.rs::SessionStatus
export type SessionStatus = 'starting' | 'running' | 'exited' | 'error';

// MIRROR: src-tauri/src/types.rs::CustomProcessKind
// Sub-session flavour. `terminal` runs inside an in-app PTY; `application`
// spawns an external GUI program detached.
export type CustomProcessKind = 'terminal' | 'application';

// MIRROR: src-tauri/src/types.rs::SubSessionStatus
export type SubSessionStatus = 'starting' | 'running' | 'exited' | 'error';

// MIRROR: src-tauri/src/types.rs::TempFileSpec
export interface TempFileSpec {
  path: string;
  contents: string;
}

// MIRROR: src-tauri/src/types.rs::Session
// Backend-only record. Not sent to the frontend in normal flows; included
// here so persistence/debug tooling can type it correctly.
export interface Session {
  id: SessionId;
  tool: Tool;
  worktreePath: string;
  worktreeName: string;
  label: string;
  instructionSetId?: InstructionSetId;
  composedCommand: string;
  status: SessionStatus;
  pid?: number;
  createdAt: number;
  tabIndex: number;
  tempFiles: TempFileSpec[];
  /**
   * Most recently observed AI-side session id. When set, the backend
   * augments the spawn command with `--resume <id>` on app-restart
   * restore so the conversation continues. Backend-only — currently
   * not exposed on `SessionView`.
   */
  aiSessionId?: string;
}

// MIRROR: src-tauri/src/types.rs::SessionView
// Frontend-facing projection: omits `composedCommand` and `tempFiles`.
export interface SessionView {
  id: SessionId;
  tool: Tool;
  worktreePath: string;
  worktreeName: string;
  label: string;
  instructionSetId?: InstructionSetId;
  status: SessionStatus;
  pid?: number;
  createdAt: number;
  tabIndex: number;
}

// MIRROR: src-tauri/src/types.rs::ChildId
// Discriminated child identifier — either a Session or SubSession.
// Wire shape: `{ kind: 'session', id: SessionId }` or `{ kind: 'subSession', id: SubSessionId }`.
export type ChildId = { kind: 'session'; id: SessionId } | { kind: 'subSession'; id: SubSessionId };

// MIRROR: src-tauri/src/types.rs::WorktreeTab
// First-class worktree tab record. Parent in the sidebar hierarchy.
// Child sessions/sub-sessions are grouped by matching `worktreePath`.
export interface WorktreeTab {
  id: WorktreeTabId;
  path: string;
  name: string;
  branch?: string;
  label: string;
  tabIndex: number;
  activeChildId?: ChildId;
}

// MIRROR: src-tauri/src/types.rs::InstructionSet
export interface InstructionSet {
  id: InstructionSetId;
  name: string;
  tool: Tool;
  filePath: string;
  isDefault: boolean;
}

// MIRROR: src-tauri/src/types.rs::DefaultInstructionSets
export interface DefaultInstructionSets {
  claude: InstructionSetId;
  copilot: InstructionSetId;
}

// MIRROR: src-tauri/src/types.rs::AiLaunchCommands
// Per-agent CLI launch override. Each field is a verbatim shell snippet
// (e.g. `"npx claude --model sonnet"`) interpolated into the composed
// command in place of the bare program token. Empty string = use default
// (`claude` / `copilot`). Added in `configVersion = 4`.
export interface AiLaunchCommands {
  claude: string;
  copilot: string;
  /**
   * Cached `data:image/png;base64,…` for Claude's launcher executable,
   * resolved from `claude` (preferring the canonical CLI name even
   * when `claude` above is a custom wrapper). Backend-managed —
   * frontend patches don't carry it; the merge layer preserves it
   * across `aiLaunchCommands` patches that don't change the command,
   * and clears it when the command does change.
   */
  claudeIconDataUri?: string;
  copilotIconDataUri?: string;
}

// MIRROR: src-tauri/src/types.rs::AppConfig
export interface AppConfig {
  configVersion: number;
  defaultInstructionSets: DefaultInstructionSets;
  instructionSetsDir: string;
  /**
   * Active workspace root: the single git repository the app operates
   * within. `null` until the user picks one in the first-boot picker
   * (Roadmap §1.1). Added in `configVersion = 3`.
   */
  workspaceRoot: string | null;
  worktreeRoots: string[];
  prelaunchCommands: string[];
  worktreePrelaunchCommands: Record<string, string[]>;
  /** Per-agent CLI launch override. Empty string fields fall back to the
   * hardcoded defaults. Added in `configVersion = 4`. */
  aiLaunchCommands: AiLaunchCommands;
  lastOpenSessions: SessionId[];
  tabOrder: SessionId[];
  /** Persisted active-session selection. `null` when no session is active. */
  activeSessionId: SessionId | null;
  /**
   * User-defined custom-process launchers exposed in the tab context menu.
   * Built-ins (`shell`, `open-folder`, `vscode`) are seeded on
   * `configVersion = 4` migration and may be edited or deleted by the user.
   */
  customProcesses: CustomProcessDef[];
  /**
   * Lightweight restore records for sub-tabs that were open at last
   * shutdown. Restore re-creates terminal sub-sessions and brings
   * application sub-sessions back greyed (re-launch on click).
   */
  lastOpenSubSessions: SubSessionRecord[];
  /** First-class worktree tab records. Added in `configVersion = 5`. */
  worktreeTabs: WorktreeTab[];
  /** Top-level sidebar ordering over worktree tab IDs. Added in `configVersion = 5`. */
  worktreeTabOrder: WorktreeTabId[];
  /** Most recently focused worktree tab. Added in `configVersion = 5`. */
  activeWorktreeTabId: WorktreeTabId | null;
}

// MIRROR: src-tauri/src/types.rs::PartialDefaultInstructionSets
export interface PartialDefaultInstructionSets {
  claude?: InstructionSetId;
  copilot?: InstructionSetId;
}

// MIRROR: src-tauri/src/types.rs::PartialAiLaunchCommands
export interface PartialAiLaunchCommands {
  claude?: string;
  copilot?: string;
}

// MIRROR: src-tauri/src/types.rs::PartialAppConfig
// Every field optional so Phase 4's `config_set` can deep-merge updates.
// `activeSessionId` is tri-state: omit to leave alone, `null` to clear,
// string to set.
export interface PartialAppConfig {
  configVersion?: number;
  defaultInstructionSets?: PartialDefaultInstructionSets;
  instructionSetsDir?: string;
  /**
   * Tri-state: omit to leave alone; `null` to clear; string to set. The
   * backend canonicalizes the path and rejects relative values with
   * `InvalidPath`.
   */
  workspaceRoot?: string | null;
  worktreeRoots?: string[];
  prelaunchCommands?: string[];
  worktreePrelaunchCommands?: Record<string, string[]>;
  aiLaunchCommands?: PartialAiLaunchCommands;
  lastOpenSessions?: SessionId[];
  tabOrder?: SessionId[];
  activeSessionId?: SessionId | null;
  /** Replaces the entire `customProcesses` list when present. */
  customProcesses?: CustomProcessDef[];
  /** Replaces the entire `lastOpenSubSessions` list when present. */
  lastOpenSubSessions?: SubSessionRecord[];
  /** Replaces the worktree tabs list when present. */
  worktreeTabs?: WorktreeTab[];
  /** Replaces the worktree tab order when present. */
  worktreeTabOrder?: WorktreeTabId[];
  /** Tri-state: omit to leave alone; `null` to clear; string to set. */
  activeWorktreeTabId?: WorktreeTabId | null;
}

// MIRROR: src-tauri/src/types.rs::CustomProcessDef
// Persisted in `AppConfig.customProcesses`. `command` is passed verbatim to
// `$SHELL -c` (or `%COMSPEC% /c` on Windows); the parent worktree tab path is
// set as `cwd` and **never** interpolated into the command.
export interface CustomProcessDef {
  id: CustomProcessDefId;
  name: string;
  kind: CustomProcessKind;
  command: string;
  enabled: boolean;
  /** Optional UI hint (icon name / emoji / preset key); reserved for future use. */
  icon?: string;
  /**
   * Cached `data:image/png;base64,…` for the resolved app icon. The
   * backend's `backfill_icons` pass populates this from `command` at
   * config-save time. Frontend renders synchronously from this — no
   * per-render IPC. Frontend patches that omit this field do **not**
   * clobber the cache (see `merge_partial` in the Rust side).
   */
  iconDataUri?: string;
}

// MIRROR: src-tauri/src/types.rs::SubSession
// In-memory + on-the-wire representation of a sub-tab. Lives in a
// parallel `SubSessionStore` on the Rust side; the frontend mirrors them
// in a Zustand slice (Phase 4).
export interface SubSession {
  id: SubSessionId;
  parentWorktreeTabId: WorktreeTabId;
  defId: CustomProcessDefId;
  kind: CustomProcessKind;
  label: string;
  status: SubSessionStatus;
  pid?: number;
  composedCommand: string;
  createdAt: number;
}

// MIRROR: src-tauri/src/types.rs::SubSessionRecord
// Lightweight restore record persisted in
// `AppConfig.lastOpenSubSessions`. Carries only what the restore pass
// needs to attempt re-creation.
export interface SubSessionRecord {
  id: SubSessionId;
  parentWorktreeTabId: WorktreeTabId;
  defId: CustomProcessDefId;
  kind: CustomProcessKind;
  label: string;
  /**
   * Resolved command at the time the sub-session was created. Persisted
   * so a later edit to the underlying `CustomProcessDef.command` doesn't
   * change what restore would relaunch (mirror of `SubSession.composedCommand`).
   *
   * Optional in TypeScript because legacy v3 records (and tests) may
   * omit it; sanitize-on-load fills it from the def's command if missing.
   */
  composedCommand?: string;
}

// MIRROR: src-tauri/src/types.rs::AppError
// Wire shape of every error coming from a Tauri command. The frontend may
// branch on `code`; the strings come from `Error::code()` in Rust.
export interface AppError {
  code: string;
  message: string;
}

// MIRROR: src-tauri/src/types.rs::SessionOutputEvent
// Payload of the `session://output` Tauri event (DESIGN §6).
export interface SessionOutputEvent {
  sessionId: SessionId;
  data: string;
}

// MIRROR: src-tauri/src/types.rs::SessionStatusEvent
// Payload of the `session://status` Tauri event (DESIGN §6).
//
// `message` is an optional context string the backend includes for
// notable status transitions — used today by `restore_all_sessions`
// when a worktree directory has been deleted between launches
// (Roadmap §4.3).
export interface SessionStatusEvent {
  sessionId: SessionId;
  status: SessionStatus;
  message?: string;
}

// MIRROR: src-tauri/src/types.rs::SubSessionCreateArgs
export interface SubSessionCreateArgs {
  parentWorktreeTabId: WorktreeTabId;
  defId: CustomProcessDefId;
}

// MIRROR: src-tauri/src/types.rs::SubSessionIdArg
export interface SubSessionIdArg {
  id: SubSessionId;
}

// MIRROR: src-tauri/src/types.rs::SubSessionCloseIntent
//
// Discriminated tag describing what should happen to the underlying
// process when the user closes a sub-tab. Terminal kind ignores the
// variant; application kind branches on it.
export type SubSessionCloseIntent = 'tabOnly' | 'requestAppClose' | 'forceKill';

// MIRROR: src-tauri/src/types.rs::SubSessionCloseArgs
export interface SubSessionCloseArgs {
  id: SubSessionId;
  intent?: SubSessionCloseIntent;
}

// MIRROR: src-tauri/src/types.rs::SubSessionListArgs
export interface SubSessionListArgs {
  parentWorktreeTabId?: WorktreeTabId;
}

// MIRROR: src-tauri/src/types.rs::SubSessionInputArgs
export interface SubSessionInputArgs {
  id: SubSessionId;
  data: string;
}

// MIRROR: src-tauri/src/types.rs::SubSessionResizeArgs
export interface SubSessionResizeArgs {
  id: SubSessionId;
  cols: number;
  rows: number;
}

// MIRROR: src-tauri/src/types.rs::SubSessionStatusEvent
// Payload of the `subsession://status` Tauri event (Phase 2).
export interface SubSessionStatusEvent {
  id: SubSessionId;
  status: SubSessionStatus;
  pid?: number;
  message?: string;
}

// MIRROR: src-tauri/src/types.rs::SubSessionExitedEvent
// Payload of the `subsession://exited` Tauri event (Phase 3 application
// sub-tabs). Phase 2's terminal sub-tabs use `subsession://status` with
// `SubSessionStatus = 'exited'` instead.
export interface SubSessionExitedEvent {
  id: SubSessionId;
  exitCode?: number;
}

// MIRROR: src-tauri/src/types.rs::SubSessionRestoredEvent
// Payload of the `subsession://restored` Tauri event (Phase 7).
//
// Emitted once per sub-session by the restore-on-launch second pass
// (see `commands::subsession::restore_all_sub_sessions_impl`). Carries
// the full `SubSession` snapshot so the frontend store can insert the
// row in a single `applyRestored` call without an extra round-trip to
// `subsession_list`. Status events for the same id (Running, Exited,
// Error) follow the restore event in normal flow.
export interface SubSessionRestoredEvent {
  subSession: SubSession;
}

// MIRROR: src-tauri/src/types.rs::WorktreeTabOpenArgs
export interface WorktreeTabOpenArgs {
  path: string;
}

// MIRROR: src-tauri/src/types.rs::WorktreeTabCloseArgs
export interface WorktreeTabCloseArgs {
  id: WorktreeTabId;
}

// MIRROR: src-tauri/src/types.rs::WorktreeTabFocusArgs
export interface WorktreeTabFocusArgs {
  id: WorktreeTabId;
}

// MIRROR: src-tauri/src/types.rs::WorktreeTabReorderArgs
export interface WorktreeTabReorderArgs {
  ids: WorktreeTabId[];
}

// MIRROR: src-tauri/src/types.rs::WorktreeTabSetActiveChildArgs
export interface WorktreeTabSetActiveChildArgs {
  id: WorktreeTabId;
  childId?: ChildId;
}

// MIRROR: src-tauri/src/types.rs::WorktreeTabCloseResult
export interface WorktreeTabCloseResult {
  childErrors?: string[];
}

// MIRROR: src-tauri/src/activity.rs::ActivityEvent
// Tagged union; matches `#[serde(tag = "kind", rename_all = "camelCase")]`.
export type ActivityEvent =
  | { kind: 'title'; value: string }
  | { kind: 'attention' }
  | { kind: 'working' }
  | { kind: 'idle' }
  | { kind: 'promptStart' }
  | { kind: 'commandStart' }
  | { kind: 'commandEnd'; exit: number | null }
  | { kind: 'turnEnd'; durationMs: number | null }
  // Copilot events.jsonl tailer (Phase 2.5). New variants are emitted
  // alongside the legacy PTY-byte signals; the reducer treats them as
  // additive — they don't replace `working` / `idle`.
  | { kind: 'turnStart' }
  | { kind: 'toolStart'; toolName: string; toolCallId: string }
  | { kind: 'toolEnd'; toolCallId: string; success: boolean }
  | {
      kind: 'awaitingPermission';
      requestId: string;
      permissionKind: string;
      summary: string | null;
    }
  | { kind: 'permissionResolved'; requestId: string; approved: boolean };

// MIRROR: src-tauri/src/types.rs::SessionActivityEvent
// Payload of the `session://activity` Tauri event (DESIGN §6). The
// `ActivityEvent` fields are flattened into the payload alongside
// `sessionId`.
export type SessionActivityEvent = { sessionId: SessionId } & ActivityEvent;

// MIRROR: src-tauri/src/types.rs::SessionMetricsEvent
// Payload of the `session://metrics` Tauri event (Issue #3). All fields
// except `sessionId` and `observedAt` are optional — the watcher emits
// only what it can resolve. The same shape doubles as the in-memory
// snapshot the sidebar renders.
export interface SessionMetricsEvent {
  sessionId: SessionId;
  /** Model identifier as reported by the CLI (e.g. `claude-sonnet-4-6`). */
  model?: string;
  /** Percentage of the context window in use, 0..=100. */
  contextUsedPct?: number;
  /** Tokens currently counted against the context window. */
  contextTokensUsed?: number;
  /** Model context-window limit in tokens, when known. */
  contextTokensLimit?: number;
  /** Cumulative input tokens across observed turns. */
  inputTokens?: number;
  /** Cumulative output tokens across observed turns. */
  outputTokens?: number;
  /** Wall-clock unix-seconds at which this snapshot was produced. */
  observedAt: number;
}

/** In-memory alias — sidebar reads the same shape it received over the wire. */
export type SessionMetrics = SessionMetricsEvent;

// MIRROR: src-tauri/src/types.rs::WorktreeInfo
// Returned by the `worktrees_list` command (DESIGN §6, Phase 10).
// `branch` is omitted by the backend when the worktree has a detached HEAD,
// so we model it as an optional string.
export interface WorktreeInfo {
  path: string;
  branch?: string;
  isMain: boolean;
  isLocked: boolean;
}

// MIRROR: src-tauri/src/types.rs::WorkspaceValidateResult
// Returned by the `workspace_validate` command (Roadmap §1.1). `error` is
// only populated when `valid === false`.
//
// `alreadyOpenInAnotherInstance` is an **advisory** Phase 8 signal:
// `true` when a non-blocking probe of the per-(branch, workspace) `.lock`
// file revealed that another Arborist process **bound to the same
// `(branch, workspace)` pair** currently holds it, `false` if the
// probe acquired the lock cleanly (and immediately released it), and
// `undefined` if the probe was not performed (e.g. the path failed
// earlier validation, or the call site didn't have an `app_data_dir`
// to derive the lock path from). The lock is OS-advisory and
// auto-releases when the holding process exits (clean or crash) — so
// `true` here means "another live instance" and never a stale-lock
// remnant. Contention with a *different* branch (e.g. release vs dev
// build of the same workspace) is **not** detected here because each
// branch gets its own scoped lock path. Picker UIs should surface a
// warning when `true` but still allow the user to confirm — the
// authoritative lock acquire happens at switch/boot time and will
// fail with `WorkspaceLocked` if the contention is still present
// then.
export interface WorkspaceValidateResult {
  valid: boolean;
  error?: string;
  alreadyOpenInAnotherInstance?: boolean;
}

// MIRROR: src-tauri/src/types.rs::WorktreeCreateResult
// Returned by the `worktree_create` command (Roadmap §2.2). `path` is the
// canonical absolute path to the newly-created worktree directory.
export interface WorktreeCreateResult {
  path: string;
}

// MIRROR: src-tauri/src/types.rs::WorkspaceSwitchArgs
// Argument struct for the `workspace_switch` command. The Tauri invoke
// wrapper passes this as `{ args: { path } }` to match the Rust handler
// signature `workspace_switch(args: WorkspaceSwitchArgs)`.
export interface WorkspaceSwitchArgs {
  path: string;
}

// MIRROR: src-tauri/src/types.rs::WorkspaceSwitchResult
// Resolves on success of `workspace_switch`. `workspaceRoot` is the
// **canonical** path the backend bound to. `noOp` is `true` if the
// requested path matched the workspace already in use — in that case
// `config` and `sessions` mirror the *current* (unchanged) state so
// the wire payload is non-nullable but the frontend can short-circuit
// adoption.
//
// On a real swap, `config` and `sessions` reflect the **new**
// workspace's state *after* the inline restore loop has run —
// sessions are already in `Starting` status, so the frontend adopts
// everything in one render with no flicker. The
// `workspace://changed` event was deleted in PR5; this result is now
// the sole authoritative state-transfer channel for in-app switches.
export interface WorkspaceSwitchResult {
  workspaceRoot: string;
  noOp: boolean;
  config: AppConfig;
  sessions: SessionView[];
}
