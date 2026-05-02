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

// MIRROR: src-tauri/src/types.rs::InstructionSetId
export type InstructionSetId = string;

// MIRROR: src-tauri/src/types.rs::Tool
export type Tool = 'claude' | 'copilot';

// MIRROR: src-tauri/src/types.rs::SessionStatus
export type SessionStatus = 'starting' | 'running' | 'exited' | 'error';

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
// file revealed that another Arborist process currently holds it,
// `false` if the probe acquired the lock cleanly (and immediately
// released it), and `undefined` if the probe was not performed (e.g.
// the path failed earlier validation, or the call site didn't have an
// `app_data_dir` to derive the lock path from). Picker UIs should
// surface a warning when `true` but still allow the user to confirm —
// the authoritative lock acquire happens at switch/boot time and will
// fail with `WorkspaceLocked` if the contention is still present then.
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
// requested path matched the workspace already in use — no teardown
// happened and no `workspace://changed` event was emitted.
export interface WorkspaceSwitchResult {
  workspaceRoot: string;
  noOp: boolean;
}

// MIRROR: src-tauri/src/types.rs::WorkspaceChangedEvent
// Payload for the `workspace://changed` event. Subscribers should drop
// any in-memory state derived from the old workspace and re-fetch from
// the backend.
export interface WorkspaceChangedEvent {
  workspaceRoot: string;
}
