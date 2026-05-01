//! Shared, serializable data model for Arborist.
//!
//! Every type in this module is a load-bearing wire contract between the Rust
//! backend and the React/TypeScript frontend. **The TypeScript mirror lives in
//! `src/types/arborist.ts`**: when you change anything here, update the matching
//! TS interface in the same commit (look for the `MIRROR:` markers).
//!
//! Conventions:
//! * `#[serde(rename_all = "camelCase")]` on every struct so the on-the-wire
//!   shape matches idiomatic TypeScript naming.
//! * Enums use `#[serde(rename_all = "lowercase")]` to produce simple string
//!   discriminants (`"claude"`, `"running"`, …).
//! * ID newtypes use `#[serde(transparent)]` so they appear as plain strings on
//!   the wire while remaining strongly typed in Rust.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

/// Stable identifier for a [`Session`]. Backed by a UUID v4 in practice, but
/// the wire shape is just the canonical hyphenated string form.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Generate a fresh random session ID.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for an [`InstructionSet`]. Currently a string slug
/// derived from the instruction file name (e.g. `"claude-default"`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct InstructionSetId(pub String);

impl InstructionSetId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InstructionSetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Which AI CLI a session is bound to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Copilot,
}

/// Lifecycle state of a session's underlying PTY child.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Starting,
    Running,
    Exited,
    Error,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A temp file the backend must materialise on disk before (re)spawning a
/// session. Currently used by Claude for its `--system-prompt` file.
///
/// Persisted as part of [`Session`] so a Phase 7 `respawn_existing` can
/// rematerialise the file after a crash/restart without re-running
/// composition.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TempFileSpec {
    pub path: PathBuf,
    pub contents: String,
}

/// Full, persisted session record. Lives in the Rust `sessions.json` store
/// (Phase 4) and is **never** sent to the frontend as-is — use
/// [`SessionView`] for that.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: SessionId,
    pub tool: Tool,
    pub worktree_path: PathBuf,
    pub worktree_name: String,
    pub label: String,
    /// Optional user-curated instruction set overlay. When `None`:
    /// * Claude is launched with no `--system-prompt`; the agent relies
    ///   on its auto-discovered `CLAUDE.md` from `cwd`.
    /// * Copilot ignores this field — it always auto-discovers
    ///   `.github/copilot-instructions.md` from `cwd` regardless.
    ///
    /// Both tools always receive the worktree as their `cwd`, so
    /// repository-level instructions are picked up either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_set_id: Option<InstructionSetId>,
    /// Full shell command string. Backend-only; reused verbatim by
    /// `respawn_existing` so we never recompose at restart time.
    pub composed_command: String,
    pub status: SessionStatus,
    /// OS PID of the live PTY child; cleared on exit.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<u32>,
    pub created_at: i64,
    pub tab_index: usize,
    /// Temp files this session owns on disk. Backend-only; omitted from
    /// [`SessionView`].
    #[serde(default)]
    pub temp_files: Vec<TempFileSpec>,
    /// Most recently observed AI-side session id (Claude transcript file
    /// stem; Copilot OTel `gen_ai.conversation.id` / session-state dir
    /// name). When set, `restore_all_sessions` augments the spawn command
    /// with `--resume <id>` so the conversation continues across an app
    /// restart. Backend-only — omitted from [`SessionView`]; not surfaced
    /// to the frontend today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_session_id: Option<String>,
}

/// Frontend-facing projection of [`Session`]. Intentionally drops
/// `composed_command` (backend-only restart material) and `temp_files`
/// (backend-only filesystem material).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: SessionId,
    pub tool: Tool,
    pub worktree_path: PathBuf,
    pub worktree_name: String,
    pub label: String,
    /// See [`Session::instruction_set_id`] for semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_set_id: Option<InstructionSetId>,
    pub status: SessionStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<u32>,
    pub created_at: i64,
    pub tab_index: usize,
}

impl From<&Session> for SessionView {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id,
            tool: s.tool,
            worktree_path: s.worktree_path.clone(),
            worktree_name: s.worktree_name.clone(),
            label: s.label.clone(),
            instruction_set_id: s.instruction_set_id.clone(),
            status: s.status,
            pid: s.pid,
            created_at: s.created_at,
            tab_index: s.tab_index,
        }
    }
}

// ---------------------------------------------------------------------------
// Worktree discovery
// ---------------------------------------------------------------------------

/// One entry in the result of `worktrees_list` (DESIGN §6). Mirrored on the
/// frontend by `WorktreeInfo` in `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// `None` for detached HEAD.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    /// `true` for the primary worktree of the repository.
    pub is_main: bool,
    pub is_locked: bool,
}

// ---------------------------------------------------------------------------
// InstructionSet
// ---------------------------------------------------------------------------

/// A discovered instruction set on disk. Discovery happens in Phase 4.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstructionSet {
    pub id: InstructionSetId,
    pub name: String,
    pub tool: Tool,
    pub file_path: PathBuf,
    pub is_default: bool,
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// Per-tool default instruction set selection.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DefaultInstructionSets {
    pub claude: InstructionSetId,
    pub copilot: InstructionSetId,
}

/// Current on-disk schema version for [`AppConfig`]. Incremented whenever
/// the persisted shape changes in a non-backwards-compatible way so the
/// loader can migrate (or quarantine) old files.
///
/// Version history:
/// * `1` — initial release.
/// * `2` — added `active_session_id` (Phase 7).
/// * `3` — added `workspace_root` (single-workspace model, Roadmap §1).
/// * `4` — added `ai_launch_commands` (per-agent CLI launch override).
pub const CONFIG_VERSION_CURRENT: u32 = 4;

/// Per-agent CLI launch command override. Each field is a verbatim shell
/// snippet (e.g. `"npx claude --model sonnet"`) interpolated into the
/// composed command in place of the bare program token. Empty string means
/// "use the default" (`claude` / `copilot`). Added in `configVersion = 4`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AiLaunchCommands {
    #[serde(default)]
    pub claude: String,
    #[serde(default)]
    pub copilot: String,
}

/// Persisted application configuration. Lives in `config.json` (Phase 4).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    /// Schema version of this on-disk config. Bumped when the layout
    /// changes; the loader quarantines files with versions it does not
    /// understand.
    pub config_version: u32,
    pub default_instruction_sets: DefaultInstructionSets,
    pub instruction_sets_dir: PathBuf,
    /// Active workspace root: the single git repository the app operates
    /// within. `None` until the user picks one in the first-boot picker
    /// (Roadmap §1.1). When set, takes precedence over `worktree_roots` for
    /// session-creation worktree discovery. Added in `configVersion = 3`.
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,
    pub worktree_roots: Vec<PathBuf>,
    pub prelaunch_commands: Vec<String>,
    /// Per-worktree overrides. Key = canonicalized worktree path as a string.
    pub worktree_prelaunch_commands: BTreeMap<String, Vec<String>>,
    /// Per-agent CLI launch override. Empty fields fall back to the
    /// hardcoded defaults (`claude` / `copilot`). Added in
    /// `configVersion = 4`.
    #[serde(default)]
    pub ai_launch_commands: AiLaunchCommands,
    pub last_open_sessions: Vec<SessionId>,
    pub tab_order: Vec<SessionId>,
    /// ID of the most recently focused session. Persisted by `session_focus`
    /// and consulted by Phase 8+ on launch to decide which tab to show
    /// active. Cleared when the active session is closed. Added in
    /// `configVersion = 2`.
    #[serde(default)]
    pub active_session_id: Option<SessionId>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION_CURRENT,
            default_instruction_sets: DefaultInstructionSets::default(),
            instruction_sets_dir: PathBuf::new(),
            workspace_root: None,
            worktree_roots: Vec::new(),
            prelaunch_commands: Vec::new(),
            worktree_prelaunch_commands: BTreeMap::new(),
            ai_launch_commands: AiLaunchCommands::default(),
            last_open_sessions: Vec::new(),
            tab_order: Vec::new(),
            active_session_id: None,
        }
    }
}

/// Partial form of [`DefaultInstructionSets`] used by [`PartialAppConfig`]
/// for deep-merging in Phase 4's `config_set` command.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialDefaultInstructionSets {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub claude: Option<InstructionSetId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub copilot: Option<InstructionSetId>,
}

/// Partial form of [`AiLaunchCommands`]. Each field is `Some` to overwrite
/// that agent's launch command (set empty string to clear / revert to
/// default), or `None` to leave it alone.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialAiLaunchCommands {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub claude: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub copilot: Option<String>,
}

/// Patch over [`AppConfig`]: every field optional so callers can update one
/// key at a time. Phase 4 deep-merges this into the persisted config.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialAppConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub config_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub default_instruction_sets: Option<PartialDefaultInstructionSets>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub instruction_sets_dir: Option<PathBuf>,
    /// Tri-state: absent → leave alone; `null` → clear; `"<path>"` → set.
    /// Mirrors the encoding used for `active_session_id`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub workspace_root: Option<Option<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worktree_roots: Option<Vec<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prelaunch_commands: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worktree_prelaunch_commands: Option<BTreeMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ai_launch_commands: Option<PartialAiLaunchCommands>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_open_sessions: Option<Vec<SessionId>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tab_order: Option<Vec<SessionId>>,
    /// Tri-state: absent → leave alone; `null` → clear; `"<uuid>"` → set.
    /// Encoded with the `double_option` helper so JSON `null` is preserved
    /// as `Some(None)` rather than collapsing to "field absent".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub active_session_id: Option<Option<SessionId>>,
}

/// serde adapter for `Option<Option<T>>`: distinguishes "absent" from
/// "present-but-null". JSON has no native `Some(None)`, so we serialise
/// `Some(None)` as `null` and rely on `skip_serializing_if = Option::is_none`
/// to elide the absent case.
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match v {
            // Outer None is elided by `skip_serializing_if`; this branch
            // would only fire if the field weren't tagged with that.
            None => s.serialize_none(),
            Some(inner) => inner.serialize(s),
        }
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        // If the field is present, parse it as `Option<T>` (null → None,
        // value → Some). Wrap in the outer `Some` to mark "present".
        Option::<T>::deserialize(d).map(Some)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Payload of the `session://output` event (DESIGN §6).
///
/// Mirrored on the frontend by `SessionOutputEvent` in
/// `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionOutputEvent {
    pub session_id: SessionId,
    pub data: String,
}

/// Payload of the `session://activity` event (DESIGN §6).
///
/// `event` is a tagged enum (see [`crate::activity::ActivityEvent`]):
/// `{ kind: "title", value: "..." }`, `{ kind: "attention" }`,
/// `{ kind: "working" }`, `{ kind: "idle" }`, etc.
///
/// Mirrored on the frontend by `SessionActivityEvent` in
/// `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivityEvent {
    pub session_id: SessionId,
    #[serde(flatten)]
    pub event: crate::activity::ActivityEvent,
}

/// Snapshot of the latest token / context-window observation for a session,
/// used both as the payload for the `session://metrics` event and as the
/// in-memory state the frontend renders. All fields except `session_id` and
/// `observed_at` are optional: a snapshot may carry only a token count if
/// the model's context limit cannot be resolved.
///
/// Mirrored on the frontend by `SessionMetrics` in `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetricsEvent {
    pub session_id: SessionId,
    /// Model identifier as reported by the CLI (e.g. `"claude-sonnet-4-6"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Percentage of the context window in use, 0..=100. Omitted when the
    /// model's context limit cannot be resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used_pct: Option<u8>,
    /// Tokens currently counted against the context window
    /// (= `input + cache_creation + cache_read + output` for the latest
    /// observed assistant turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens_used: Option<u64>,
    /// Model context-window limit in tokens (e.g. 200_000), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens_limit: Option<u64>,
    /// Cumulative input tokens across observed turns of this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Cumulative output tokens across observed turns of this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Wall-clock unix-seconds at which this snapshot was produced.
    pub observed_at: u64,
}

impl SessionMetricsEvent {
    /// True when two snapshots carry the same data — every field except
    /// `observed_at`. Used by the per-tool watchers to suppress redundant
    /// `session://metrics` emissions when nothing has changed since the
    /// previous poll. Comparing `Self` directly via derived `PartialEq`
    /// would always differ because `observed_at` advances every poll.
    ///
    /// **Future-proofing:** the destructuring patterns below intentionally
    /// list every field by name (no `..`) so that adding a new field to
    /// `SessionMetricsEvent` is a compile error here. That forces an
    /// explicit decision: include the new field in the dedup comparison,
    /// or document why it's excluded (like `observed_at`).
    #[must_use]
    pub fn same_payload_as(&self, other: &Self) -> bool {
        let Self {
            session_id: a_session_id,
            model: a_model,
            context_used_pct: a_pct,
            context_tokens_used: a_used,
            context_tokens_limit: a_limit,
            input_tokens: a_in,
            output_tokens: a_out,
            observed_at: _, // intentionally excluded — see fn doc
        } = self;
        let Self {
            session_id: b_session_id,
            model: b_model,
            context_used_pct: b_pct,
            context_tokens_used: b_used,
            context_tokens_limit: b_limit,
            input_tokens: b_in,
            output_tokens: b_out,
            observed_at: _, // intentionally excluded — see fn doc
        } = other;
        a_session_id == b_session_id
            && a_model == b_model
            && a_pct == b_pct
            && a_used == b_used
            && a_limit == b_limit
            && a_in == b_in
            && a_out == b_out
    }
}

/// Payload of the `session://status` event (DESIGN §6).
///
/// `message` is an optional human-readable note that accompanies the
/// status change — used today for stale-worktree restore failures
/// (Roadmap §4.3) so the terminal overlay can explain *why* the session
/// is in `error` state instead of just showing a generic banner.
///
/// Mirrored on the frontend by `SessionStatusEvent` in
/// `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusEvent {
    pub session_id: SessionId,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Command argument shapes (DESIGN §6)
// ---------------------------------------------------------------------------

/// Arguments for the `session_create` command.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionCreateArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateArgs {
    pub tool: Tool,
    pub worktree_path: PathBuf,
    /// Optional. When omitted, the session is launched with no
    /// `--system-prompt` for Claude (Copilot never used this field).
    /// See [`Session::instruction_set_id`].
    #[serde(default)]
    pub instruction_set_id: Option<InstructionSetId>,
    /// Initial PTY width (columns) the child process will see at startup.
    /// The frontend measures the terminal host before calling `session_create`
    /// so the CLI's first paint (e.g., a Copilot/Claude splash screen)
    /// renders at the right width — without this, the child reads 80 cols
    /// from the OS, draws its splash narrow, and never re-paints when the
    /// later `session_resize` arrives.
    pub cols: u16,
    /// Initial PTY height (rows). See [`Self::cols`].
    pub rows: u16,
}

/// Arguments for any command keyed only by session id
/// (`session_focus`, `session_restart`). `session_close` uses the richer
/// [`SessionCloseArgs`] so the user can opt into worktree deletion.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionIdArg`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdArg {
    pub session_id: SessionId,
}

/// Arguments for `session_close`. Extends [`SessionIdArg`] with an opt-in
/// flag that removes the session's git worktree from disk after the PTY is
/// torn down. The backend gates removal behind safety checks (never the
/// main worktree, never a path outside the configured workspace root); see
/// `commands::session::session_close_impl`.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionCloseArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseArgs {
    pub session_id: SessionId,
    /// When `true`, run `git worktree remove --force <worktree_path>` after
    /// terminating the PTY. Defaults to `false` so legacy callers (and any
    /// future code that forgets to set the flag) preserve existing
    /// behaviour.
    #[serde(default)]
    pub delete_worktree: bool,
}

/// Result of `session_close`. The session record and PTY are always torn
/// down on success; if the user opted into worktree deletion and the
/// `git worktree remove` step failed, the failure is reported here as a
/// warning string rather than as a hard error so callers can converge UI
/// state regardless.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionCloseResult`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseResult {
    /// Human-readable error message from `git worktree remove`. `None`
    /// when worktree deletion was not requested or succeeded.
    pub worktree_delete_error: Option<String>,
}

/// Arguments for `session_resize`.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionResizeArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionResizeArgs {
    pub session_id: SessionId,
    pub cols: u16,
    pub rows: u16,
}

/// Arguments for `session_restart`. Carries the current PTY dimensions so
/// the freshly-spawned child process sees the right size from its very
/// first `ioctl(TIOCGWINSZ)` / ConPTY query, instead of starting at the
/// OS-default 80×24 and rendering its initial output (splash screen,
/// shell prompt, …) at the wrong width.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionRestartArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionRestartArgs {
    pub session_id: SessionId,
    pub cols: u16,
    pub rows: u16,
}

/// Arguments for `session_input`.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionInputArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInputArgs {
    pub session_id: SessionId,
    pub data: String,
}

/// Arguments for `workspace_validate` (Roadmap §1.1).
///
/// MIRROR: `src/lib/tauri-bridge.ts::WorkspaceValidateArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceValidateArgs {
    pub path: String,
}

/// Result of `workspace_validate`. `valid: true` iff the candidate path is
/// an absolute, existing directory containing a git repository. On failure,
/// `error` carries a short human-readable reason for inline picker feedback.
///
/// `alreadyOpenInAnotherInstance` is an **advisory** flag set when a
/// non-blocking probe of the per-(branch, workspace) `.lock` file
/// could not acquire the OS lock — i.e. another Arborist process
/// (any branch) currently holds it, *or* a stale lock with no owner
/// is still pinning the file. The picker UI surfaces a warning but
/// still allows the user to confirm; the actual lock is acquired
/// transactionally by `workspace_switch` (or boot), which will fail
/// with `WorkspaceLocked` if the contention is still present.
/// Absent for the legacy / canonical layout when no `.lock` file
/// exists yet — that case reads as "no contention" (false).
///
/// MIRROR: `src/types/arborist.ts::WorkspaceValidateResult`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceValidateResult {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// `Some(true)` if a non-blocking lock probe revealed contention;
    /// `Some(false)` if the probe succeeded; `None` if no probe was
    /// performed (e.g. the path failed earlier validation).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub already_open_in_another_instance: Option<bool>,
}

/// Arguments for `worktree_create` (Roadmap §2.2).
///
/// MIRROR: `src/lib/tauri-bridge.ts::WorktreeCreateArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCreateArgs {
    pub name: String,
}

/// Result of `worktree_create`. `path` is the canonical absolute path to
/// the newly-created worktree directory.
///
/// MIRROR: `src/types/arborist.ts::WorktreeCreateResult`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCreateResult {
    pub path: PathBuf,
}

/// Arguments for `workspace_switch` (Phase 7 — in-app workspace switch).
///
/// MIRROR: `src/lib/tauri-bridge.ts::WorkspaceSwitchArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSwitchArgs {
    pub path: String,
}

/// Result of `workspace_switch`. `workspaceRoot` is the **canonical** path
/// the backend bound to (which may differ in casing / separators from the
/// path the frontend submitted). `noOp` is `true` if the requested path
/// resolved to the workspace already in use, in which case no teardown
/// happened and no `workspace://changed` event was emitted.
///
/// MIRROR: `src/types/arborist.ts::WorkspaceSwitchResult`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSwitchResult {
    pub workspace_root: PathBuf,
    pub no_op: bool,
}

/// Payload for the `workspace://changed` event, fired after a successful
/// in-app workspace switch (or on the initial bind if a future phase adds
/// "open another window" semantics). The frontend reacts by reloading
/// `config.get`, `session.list`, and re-issuing `frontend_ready` so the
/// backend's restore-on-launch can fire for the new workspace.
///
/// MIRROR: `src/types/arborist.ts::WorkspaceChangedEvent`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceChangedEvent {
    pub workspace_root: PathBuf,
}

/// Crate-wide error type. Internal Rust code consumes this via `?`; at the
/// Tauri command boundary it is converted to [`AppError`] so the frontend
/// gets a stable, serde-friendly shape it can branch on.
#[derive(Error, Debug)]
pub enum Error {
    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("worktree missing: {0}")]
    WorktreeMissing(std::path::PathBuf),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("config quarantined: {0}")]
    ConfigQuarantined(String),

    #[error("pty spawn failed: {0}")]
    PtySpawnFailed(String),

    #[error("pty write failed: {0}")]
    PtyWriteFailed(String),

    #[error("pty resize failed: {0}")]
    PtyResizeFailed(String),

    #[error("pty kill failed: {0}")]
    PtyKillFailed(String),

    /// An instruction file exceeds the 1 MiB cap from DESIGN §8.2. The
    /// payload is the offending file's path for diagnostics.
    #[error("instruction file too large: {0}")]
    InstructionFileTooLarge(std::path::PathBuf),

    /// A session's persisted instruction temp file is missing on disk and
    /// could not be re-materialised. Surfaces during restore (Phase 7)
    /// when both the on-disk file and the persisted contents are gone.
    #[error("instruction file missing: {0}")]
    InstructionFileMissing(std::path::PathBuf),

    /// The selected instruction set's `tool` does not match the requested
    /// session tool (e.g. asking to spawn a Claude session with a
    /// `copilot-default` instruction set).
    #[error("tool/instruction-set mismatch: {0}")]
    ToolMismatch(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Stable string discriminant exposed to the frontend via [`AppError`].
    /// **Never rename these without updating the TypeScript callers** — the
    /// UI may branch on them.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath(_) => "InvalidPath",
            Self::WorktreeMissing(_) => "WorktreeMissing",
            Self::NotFound(_) => "NotFound",
            Self::ConfigQuarantined(_) => "ConfigQuarantined",
            Self::PtySpawnFailed(_) => "PtySpawnFailed",
            Self::PtyWriteFailed(_) => "PtyWriteFailed",
            Self::PtyResizeFailed(_) => "PtyResizeFailed",
            Self::PtyKillFailed(_) => "PtyKillFailed",
            Self::InstructionFileTooLarge(_) => "InstructionFileTooLarge",
            Self::InstructionFileMissing(_) => "InstructionFileMissing",
            Self::ToolMismatch(_) => "ToolMismatch",
            Self::Io(_) => "Io",
            Self::Serde(_) => "Serde",
            Self::Internal(_) => "Internal",
        }
    }
}

/// Wire shape of an error sent from Rust to the frontend. Always
/// `{ "code": "<variant>", "message": "<human-readable>" }`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: String,
    pub message: String,
}

impl AppError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<Error> for AppError {
    fn from(err: Error) -> Self {
        Self {
            code: err.code().to_owned(),
            message: err.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::{json, Value};

    /// Round-trip a value through JSON and assert the resulting [`Value`]
    /// equals the supplied fixture, *and* that deserialising the fixture
    /// reproduces the original value. This is the canonical drift detector.
    fn assert_roundtrip<T>(value: &T, fixture: Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let serialized: Value = serde_json::to_value(value).expect("serialize");
        assert_eq!(serialized, fixture, "serialized form drifted from fixture");

        let deserialized: T = serde_json::from_value(fixture).expect("deserialize");
        assert_eq!(&deserialized, value, "deserialized value drifted");
    }

    fn sample_session() -> Session {
        Session {
            id: SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid")),
            tool: Tool::Claude,
            worktree_path: PathBuf::from("/repo/feature-x"),
            worktree_name: "feature-x".to_owned(),
            label: "feature-x".to_owned(),
            instruction_set_id: Some(InstructionSetId::new("claude-default")),
            composed_command: "claude --system-prompt /tmp/arborist/abc/sp.md".to_owned(),
            status: SessionStatus::Running,
            pid: Some(12345),
            created_at: 1_700_000_000,
            tab_index: 0,
            temp_files: vec![TempFileSpec {
                path: PathBuf::from("/tmp/arborist/abc/sp.md"),
                contents: "context".to_owned(),
            }],
            ai_session_id: None,
        }
    }

    fn session_fixture() -> Value {
        json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "tool": "claude",
            "worktreePath": "/repo/feature-x",
            "worktreeName": "feature-x",
            "label": "feature-x",
            "instructionSetId": "claude-default",
            "composedCommand": "claude --system-prompt /tmp/arborist/abc/sp.md",
            "status": "running",
            "pid": 12345,
            "createdAt": 1_700_000_000,
            "tabIndex": 0,
            "tempFiles": [
                { "path": "/tmp/arborist/abc/sp.md", "contents": "context" }
            ]
        })
    }

    fn session_view_fixture() -> Value {
        json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "tool": "claude",
            "worktreePath": "/repo/feature-x",
            "worktreeName": "feature-x",
            "label": "feature-x",
            "instructionSetId": "claude-default",
            "status": "running",
            "pid": 12345,
            "createdAt": 1_700_000_000,
            "tabIndex": 0
        })
    }

    fn instruction_set_fixture() -> (InstructionSet, Value) {
        let value = InstructionSet {
            id: InstructionSetId::new("claude-default"),
            name: "Claude default".to_owned(),
            tool: Tool::Claude,
            file_path: PathBuf::from("/cfg/instructions/claude-default.md"),
            is_default: true,
        };
        let fixture = json!({
            "id": "claude-default",
            "name": "Claude default",
            "tool": "claude",
            "filePath": "/cfg/instructions/claude-default.md",
            "isDefault": true
        });
        (value, fixture)
    }

    fn app_config_fixture() -> (AppConfig, Value) {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "/repo/feature-x".to_owned(),
            vec!["nvm use".to_owned(), "asdf reshim".to_owned()],
        );
        let value = AppConfig {
            config_version: 4,
            default_instruction_sets: DefaultInstructionSets {
                claude: InstructionSetId::new("claude-default"),
                copilot: InstructionSetId::new("copilot-default"),
            },
            instruction_sets_dir: PathBuf::from("/cfg/instructions"),
            workspace_root: Some(PathBuf::from("/repo")),
            worktree_roots: vec![PathBuf::from("/repo")],
            prelaunch_commands: vec!["source ~/.zshenv".to_owned()],
            worktree_prelaunch_commands: overrides,
            ai_launch_commands: AiLaunchCommands {
                claude: "npx claude".to_owned(),
                copilot: String::new(),
            },
            last_open_sessions: vec![SessionId(
                Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"),
            )],
            tab_order: vec![SessionId(
                Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"),
            )],
            active_session_id: Some(SessionId(
                Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"),
            )),
        };
        let fixture = json!({
            "configVersion": 4,
            "defaultInstructionSets": {
                "claude": "claude-default",
                "copilot": "copilot-default"
            },
            "instructionSetsDir": "/cfg/instructions",
            "workspaceRoot": "/repo",
            "worktreeRoots": ["/repo"],
            "prelaunchCommands": ["source ~/.zshenv"],
            "worktreePrelaunchCommands": {
                "/repo/feature-x": ["nvm use", "asdf reshim"]
            },
            "aiLaunchCommands": {
                "claude": "npx claude",
                "copilot": ""
            },
            "lastOpenSessions": ["550e8400-e29b-41d4-a716-446655440000"],
            "tabOrder": ["550e8400-e29b-41d4-a716-446655440000"],
            "activeSessionId": "550e8400-e29b-41d4-a716-446655440000"
        });
        (value, fixture)
    }

    fn partial_app_config_fixture() -> (PartialAppConfig, Value) {
        let value = PartialAppConfig {
            config_version: None,
            default_instruction_sets: Some(PartialDefaultInstructionSets {
                claude: Some(InstructionSetId::new("claude-default")),
                copilot: None,
            }),
            instruction_sets_dir: None,
            workspace_root: None,
            worktree_roots: Some(vec![PathBuf::from("/repo")]),
            prelaunch_commands: None,
            worktree_prelaunch_commands: None,
            ai_launch_commands: None,
            last_open_sessions: None,
            tab_order: None,
            active_session_id: None,
        };
        let fixture = json!({
            "defaultInstructionSets": { "claude": "claude-default" },
            "worktreeRoots": ["/repo"]
        });
        (value, fixture)
    }

    #[test]
    fn session_roundtrip() {
        assert_roundtrip(&sample_session(), session_fixture());
    }

    #[test]
    fn session_view_roundtrip() {
        let view = SessionView::from(&sample_session());
        assert_roundtrip(&view, session_view_fixture());
    }

    #[test]
    fn session_view_drops_backend_only_fields() {
        let view = SessionView::from(&sample_session());
        let serialized: Value = serde_json::to_value(&view).expect("serialize");
        let obj = serialized.as_object().expect("object");
        assert!(
            !obj.contains_key("composedCommand"),
            "SessionView must not expose composedCommand"
        );
        assert!(
            !obj.contains_key("tempFiles"),
            "SessionView must not expose tempFiles"
        );
    }

    #[test]
    fn instruction_set_roundtrip() {
        let (value, fixture) = instruction_set_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn app_config_roundtrip() {
        let (value, fixture) = app_config_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn partial_app_config_roundtrip() {
        let (value, fixture) = partial_app_config_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn partial_app_config_omits_none_fields() {
        let (value, _) = partial_app_config_fixture();
        let serialized: Value = serde_json::to_value(&value).expect("serialize");
        let obj = serialized.as_object().expect("object");
        // None fields must be elided so deep-merge sees a true patch.
        assert!(!obj.contains_key("configVersion"));
        assert!(!obj.contains_key("instructionSetsDir"));
        assert!(!obj.contains_key("workspaceRoot"));
        assert!(!obj.contains_key("prelaunchCommands"));
        assert!(!obj.contains_key("worktreePrelaunchCommands"));
        assert!(!obj.contains_key("lastOpenSessions"));
        assert!(!obj.contains_key("tabOrder"));
        assert!(!obj.contains_key("activeSessionId"));
    }

    #[test]
    fn partial_app_config_workspace_root_tri_state() {
        let absent: PartialAppConfig = serde_json::from_value(json!({})).expect("absent");
        assert_eq!(absent.workspace_root, None);

        let cleared: PartialAppConfig =
            serde_json::from_value(json!({ "workspaceRoot": null })).expect("clear");
        assert_eq!(cleared.workspace_root, Some(None));

        let set: PartialAppConfig =
            serde_json::from_value(json!({ "workspaceRoot": "/repo" })).expect("set");
        assert_eq!(set.workspace_root, Some(Some(PathBuf::from("/repo"))));

        let serialised = serde_json::to_value(&cleared).expect("ser");
        assert_eq!(serialised, json!({ "workspaceRoot": null }));
    }

    #[test]
    fn partial_app_config_active_session_id_tri_state() {
        // Absent: deserialised as `None` → "leave alone".
        let absent: PartialAppConfig = serde_json::from_value(json!({})).expect("absent");
        assert_eq!(absent.active_session_id, None);

        // null: deserialised as `Some(None)` → "clear".
        let cleared: PartialAppConfig =
            serde_json::from_value(json!({ "activeSessionId": null })).expect("clear");
        assert_eq!(cleared.active_session_id, Some(None));

        // string: deserialised as `Some(Some(uuid))` → "set".
        let set: PartialAppConfig = serde_json::from_value(json!({
            "activeSessionId": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .expect("set");
        assert_eq!(
            set.active_session_id,
            Some(Some(SessionId(
                Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid")
            )))
        );

        // Round-trip: Some(None) serialises to null.
        let serialised = serde_json::to_value(&cleared).expect("ser");
        assert_eq!(serialised, json!({ "activeSessionId": null }));
        // Outer None serialises to {} (field elided).
        let serialised_absent = serde_json::to_value(&absent).expect("ser");
        assert_eq!(serialised_absent, json!({}));
    }

    #[test]
    fn tool_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(Tool::Claude).expect("v"),
            json!("claude")
        );
        assert_eq!(
            serde_json::to_value(Tool::Copilot).expect("v"),
            json!("copilot")
        );
    }

    #[test]
    fn session_status_serializes_lowercase() {
        for (variant, wire) in [
            (SessionStatus::Starting, "starting"),
            (SessionStatus::Running, "running"),
            (SessionStatus::Exited, "exited"),
            (SessionStatus::Error, "error"),
        ] {
            assert_eq!(serde_json::to_value(variant).expect("v"), json!(wire));
        }
    }

    #[test]
    fn session_id_is_transparent_string() {
        let id = SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"));
        assert_eq!(
            serde_json::to_value(id).expect("v"),
            json!("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn instruction_set_id_is_transparent_string() {
        let id = InstructionSetId::new("claude-default");
        assert_eq!(
            serde_json::to_value(&id).expect("v"),
            json!("claude-default")
        );
    }

    #[test]
    fn app_error_wire_shape() {
        let err = AppError::new("InvalidPath", "boom");
        assert_eq!(
            serde_json::to_value(&err).expect("v"),
            json!({ "code": "InvalidPath", "message": "boom" })
        );
    }

    #[test]
    fn error_codes_are_stable() {
        // Frontend may branch on these strings — keep them stable across phases.
        assert_eq!(Error::InvalidPath("p".into()).code(), "InvalidPath");
        assert_eq!(
            Error::WorktreeMissing(std::path::PathBuf::from("/x")).code(),
            "WorktreeMissing"
        );
        assert_eq!(Error::NotFound("p".into()).code(), "NotFound");
        assert_eq!(Error::Io(std::io::Error::other("e")).code(), "Io");
        assert_eq!(Error::Internal("e".into()).code(), "Internal");
        assert_eq!(Error::PtySpawnFailed("e".into()).code(), "PtySpawnFailed");
        assert_eq!(Error::PtyWriteFailed("e".into()).code(), "PtyWriteFailed");
        assert_eq!(Error::PtyResizeFailed("e".into()).code(), "PtyResizeFailed");
        assert_eq!(Error::PtyKillFailed("e".into()).code(), "PtyKillFailed");
        assert_eq!(
            Error::InstructionFileTooLarge(std::path::PathBuf::from("/x")).code(),
            "InstructionFileTooLarge"
        );
        assert_eq!(
            Error::InstructionFileMissing(std::path::PathBuf::from("/x")).code(),
            "InstructionFileMissing"
        );
        assert_eq!(Error::ToolMismatch("x".into()).code(), "ToolMismatch");
    }

    #[test]
    fn error_converts_to_app_error_with_message() {
        let app: AppError = Error::InvalidPath("/no/such/dir".into()).into();
        assert_eq!(app.code, "InvalidPath");
        assert!(app.message.contains("/no/such/dir"));
    }

    #[test]
    fn session_output_event_roundtrip() {
        let value = SessionOutputEvent {
            session_id: SessionId(
                Uuid::parse_str("8a3e1c5e-2b41-4b31-9dc7-1d77a3a51f00").expect("uuid"),
            ),
            data: "hello from PTY".to_owned(),
        };
        let fixture = json!({
            "sessionId": "8a3e1c5e-2b41-4b31-9dc7-1d77a3a51f00",
            "data": "hello from PTY"
        });
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn session_status_event_roundtrip() {
        let value = SessionStatusEvent {
            session_id: SessionId(
                Uuid::parse_str("8a3e1c5e-2b41-4b31-9dc7-1d77a3a51f00").expect("uuid"),
            ),
            status: SessionStatus::Running,
            message: None,
        };
        let fixture = json!({
            "sessionId": "8a3e1c5e-2b41-4b31-9dc7-1d77a3a51f00",
            "status": "running"
        });
        assert_roundtrip(&value, fixture);
    }
}
