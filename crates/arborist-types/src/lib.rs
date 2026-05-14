//! Shared, serializable data model for Arborist.
//!
//! Every type in this module is a load-bearing wire contract between the Rust backend and the React/TypeScript frontend. **The TypeScript mirror
//! lives in `src/types/arborist.ts`**: when you change anything here, update the matching TS interface in the same commit (look for the `MIRROR:`
//! markers).
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

// --------------------------------------------------------------------------- IDs
// ---------------------------------------------------------------------------

/// Stable identifier for a [`Session`]. Backed by a UUID v4 in practice, but the wire shape is just the canonical hyphenated string form.
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

/// Stable identifier for a [`SubSession`]. Backed by a UUID v4. Distinct from [`SessionId`] at the type level so the compiler enforces the boundary,
/// even though the wire shape is identical (a UUID string).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct SubSessionId(pub Uuid);

impl SubSessionId {
    /// Generate a fresh random sub-session ID.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SubSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SubSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for a [`CustomProcessDef`]. A short user-facing slug (e.g. `"shell"`, `"vscode"`, `"my-dev-server"`). Used both as the AppConfig
/// dictionary key and to bind sub-session restore records back to their definition. Built-in defs use reserved IDs (`shell`, `open-folder`, `vscode`)
/// but are otherwise indistinguishable from user-defined ones.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct CustomProcessDefId(pub String);

impl CustomProcessDefId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CustomProcessDefId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for a [`WorktreeTab`]. Backed by a UUID v4. Distinct from [`SessionId`] / [`SubSessionId`] at the type level to prevent
/// accidental cross-use.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct WorktreeTabId(pub Uuid);

impl WorktreeTabId {
    /// Generate a fresh random worktree-tab ID.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for WorktreeTabId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorktreeTabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// --------------------------------------------------------------------------- Enums
// ---------------------------------------------------------------------------

/// User-chosen colour-scheme preference (Issue #151). `System` follows the OS `prefers-color-scheme` media query; `Light`/`Dark` force the
/// corresponding theme regardless of OS setting. Serialises to `"system"` / `"light"` / `"dark"` for JSON and the TS mirror.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

/// Which AI CLI a session is bound to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Copilot,
}

impl Tool {
    /// Every persisted `Tool` variant in stable iteration order.
    pub const ALL: [Self; 2] = [Self::Claude, Self::Copilot];

    /// Stable serde discriminator used on disk and as the AI-plugin registry id.
    #[must_use]
    pub const fn as_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Copilot => "copilot",
        }
    }
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

/// Which flavour of [`CustomProcessDef`] is being launched.
///
/// * `Terminal` — runs the command inside an in-app PTY (xterm.js viewport,
///   bytes flow over `session://output`-style events). Backed by the same
///   `PtyPool` as full sessions.
/// * `Application` — spawns an external GUI program detached. The sub-tab
///   tracks only the OS PID; clicking the sub-tab focuses the program's window
///   via the platform-specific [`crate::app_launcher`] focuser.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum CustomProcessKind {
    Terminal,
    Application,
}

/// Lifecycle state of a [`SubSession`]. Mirrors [`SessionStatus`] for the terminal kind; for the application kind only `Running`, `Exited`, and
/// `Error` are observable (no separate "starting" — the spawn is synchronous, and an unfocusable / dead PID is reported as `Exited`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum SubSessionStatus {
    Starting,
    Running,
    Exited,
    Error,
}

// --------------------------------------------------------------------------- Session
// ---------------------------------------------------------------------------

/// A legacy temp file the backend must materialise on disk before (re)spawning a session.
///
/// New sessions no longer create prompt temp files, but this remains persisted so older sessions that already have `tempFiles` can restore without
/// quarantining or losing their original `composedCommand`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TempFileSpec {
    pub path: PathBuf,
    pub contents: String,
}

/// Full, persisted session record. Lives in the Rust `sessions.json` store (Phase 4) and is **never** sent to the frontend as-is — use
/// [`SessionView`] for that.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: SessionId,
    pub tool: Tool,
    pub worktree_path: PathBuf,
    pub worktree_name: String,
    pub label: String,
    /// Full shell command string. Backend-only; reused verbatim by `respawn_existing` so we never recompose at restart time.
    pub composed_command: String,
    pub status: SessionStatus,
    /// OS PID of the live PTY child; cleared on exit.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<u32>,
    pub created_at: i64,
    pub tab_index: usize,
    /// Legacy temp files this session owns on disk. Backend-only; omitted from [`SessionView`].
    #[serde(default)]
    pub temp_files: Vec<TempFileSpec>,
    /// Most recently observed AI-side session id (Claude transcript file stem; Copilot OTel `gen_ai.conversation.id` / session-state dir name). When
    /// set, `restore_all_sessions` augments the spawn command with `--resume <id>` so the conversation continues across an app restart. Backend-only
    /// — omitted from [`SessionView`]; not surfaced to the frontend today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_session_id: Option<String>,
}

/// Frontend-facing projection of [`Session`]. Intentionally drops `composed_command` (backend-only restart material) and `temp_files` (backend-only
/// filesystem material).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: SessionId,
    pub tool: Tool,
    pub worktree_path: PathBuf,
    pub worktree_name: String,
    pub label: String,
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
            status: s.status,
            pid: s.pid,
            created_at: s.created_at,
            tab_index: s.tab_index,
        }
    }
}

// --------------------------------------------------------------------------- Worktree tab (parent of sessions + sub-sessions in the sidebar)

/// A discriminated child identifier that can reference either a full AI-agent [`Session`] or a custom-process [`SubSession`] — the two kinds of
/// children that live under a [`WorktreeTab`].
///
/// MIRROR: `src/types/arborist.ts::ChildId`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(tag = "kind", content = "id", rename_all = "camelCase")]
pub enum ChildId {
    Session(SessionId),
    SubSession(SubSessionId),
}

impl std::fmt::Display for ChildId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(id) => write!(f, "session:{id}"),
            Self::SubSession(id) => write!(f, "subsession:{id}"),
        }
    }
}

/// First-class worktree tab record. In the sidebar hierarchy a worktree tab is the **parent**; AI-agent sessions and custom-process sub-sessions
/// are its children (grouped by matching `worktree_path`).
///
/// Persisted in [`AppConfig::worktree_tabs`] (introduced in `configVersion = 5`). The worktree tab–session link is derived at runtime by matching
/// `WorktreeTab.path == Session.worktree_path`; there is no stored foreign key on `Session`.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTab`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTab {
    pub id: WorktreeTabId,
    /// Canonical worktree path on disk.
    pub path: PathBuf,
    /// Display name — typically the directory basename.
    pub name: String,
    /// Git branch checked out in this worktree, resolved best-effort at creation time. May be `None` for detached HEAD or resolution failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Sidebar label, deduplicated across worktree tabs (for the rare case of two worktrees with the same basename in different repos).
    pub label: String,
    /// Top-level sidebar position. Authoritative order lives in `AppConfig.worktree_tab_order`; this field is a convenience for serialization
    /// round-trips and sorting.
    pub tab_index: usize,
    /// Last-focused child (agent session or custom-process sub-session). When `None`, focusing the tab shows the worktree dashboard placeholder.
    /// Set by `worktree_tab_set_active_child`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_child_id: Option<ChildId>,
    /// Tree-icon assignment in `1..=`[`crate::worktree_icon::WORKTREE_ICON_COUNT`] (Issue #45). Resolved to a bundled PNG asset by the frontend.
    /// `0` is the serde default for legacy v6 records without this field; the v6→v7 migration backfills any zero value with
    /// [`crate::worktree_icon::pick_least_used_icon`] so production code never sees `0` after `load_config`.
    #[serde(default)]
    pub icon_id: u32,
}

// --------------------------------------------------------------------------- Worktree discovery
// ---------------------------------------------------------------------------

/// One entry in the result of `worktrees_list`. Mirrored on the frontend by `WorktreeInfo` in `src/types/arborist.ts`.
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

/// Args for the `worktree_git_status` command (Issue #55).
///
/// MIRROR: `src/types/arborist.ts::WorktreeGitStatusArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeGitStatusArgs {
    pub path: PathBuf,
}

/// Categorical state of a single file in a worktree's working tree, as parsed from `git status --porcelain=v2 -z` (Issue #55).
///
/// `staged` corresponds to a non-`.` X column (changes already in the index), `unstaged` to a non-`.` Y column on a tracked file, and `untracked`
/// /`conflicted` map to the `?` and `u` porcelain-v2 prefixes respectively. A file with both X and Y dirty (e.g. modified-then-modified-again)
/// surfaces as both `staged` and `unstaged` — see [`WorktreeGitStatus::files`] for the full list and [`WorktreeGitStatus::staged`] /
/// [`WorktreeGitStatus::unstaged`] for counts.
///
/// MIRROR: `src/types/arborist.ts::GitStatusFileKind`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum GitStatusFileKind {
    Staged,
    Unstaged,
    Untracked,
    Conflicted,
}

/// One file entry in [`WorktreeGitStatus::files`] (Issue #55). The dashboard surfaces these as a digestible list rather than reconstructing
/// `git status --short` on the frontend.
///
/// MIRROR: `src/types/arborist.ts::GitStatusFile`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusFile {
    /// Worktree-relative path. Forward slashes on every platform — porcelain-v2 emits `/` even on Windows.
    pub path: String,
    pub kind: GitStatusFileKind,
    /// The two-character porcelain-v2 XY state code (e.g. `"M."`, `".M"`, `"MM"`, `"??"`, `"UU"`). Preserved verbatim so the UI can render a glyph
    /// without re-deriving the categorical kind. `"??"` for untracked, `"UU"` (etc.) for conflicted.
    pub status: String,
}

/// Snapshot of `git status` for a single worktree (Issue #55). Returned by the `worktree_git_status` command and used by the worktree dashboard.
/// All "count" fields are `0` and `files` is empty when the working tree is clean. On discovery failure (path missing, not a git repo,
/// `git` binary missing, non-zero status exit) the implementation returns a default-valued struct with [`Self::error`] populated to a
/// human-readable message — the dashboard distinguishes "clean tree" from "unreadable" by inspecting `error` (a successful snapshot leaves it
/// `None`). Output parsing is best-effort: unrecognised porcelain records are silently skipped, so parse anomalies surface as missing entries
/// rather than a signalled failure.
///
/// MIRROR: `src/types/arborist.ts::WorktreeGitStatus`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeGitStatus {
    /// Current branch, or `None` on detached HEAD.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    /// Short HEAD sha (first 12 chars), or `None` if HEAD could not be resolved (newborn repo with no commits).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub head: Option<String>,
    /// Configured upstream branch name (e.g. `origin/main`), or `None` when the branch tracks nothing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub upstream: Option<String>,
    /// Commits the local branch is ahead of its upstream. `0` when no upstream is configured.
    pub ahead: u32,
    /// Commits the local branch is behind its upstream. `0` when no upstream is configured.
    pub behind: u32,
    /// Files with staged changes (non-`.` X column).
    pub staged: u32,
    /// Files with unstaged working-tree changes (non-`.` Y column on a tracked file).
    pub unstaged: u32,
    /// Files Git is not tracking (`?` lines).
    pub untracked: u32,
    /// Files in a merge conflict (`u` lines).
    pub conflicted: u32,
    /// Per-file detail. Empty for a clean tree. Capped at [`MAX_GIT_STATUS_FILES`] to keep the wire payload bounded; the counts above are always
    /// authoritative even when the list is truncated. Order is the order `git status --porcelain=v2` emitted (i.e. git's discovery order).
    pub files: Vec<GitStatusFile>,
    /// `true` when [`Self::files`] was truncated to fit [`MAX_GIT_STATUS_FILES`].
    pub files_truncated: bool,
    /// `Some(message)` when the snapshot could not be produced (e.g. path missing,
    /// not a git repository, `git` binary unavailable, non-zero status exit). Counts
    /// and `files` will be empty/zero in that case. `None` indicates a successful
    /// snapshot — callers should distinguish "clean tree" from "failed to read"
    /// using this field rather than inferring from zero counts.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// Cap on the per-file list returned in [`WorktreeGitStatus::files`]. Counts are unaffected; only the detail list is bounded so a worktree with
/// thousands of dirty files cannot bloat the IPC payload.
pub const MAX_GIT_STATUS_FILES: usize = 200;

// --------------------------------------------------------------------------- AppConfig
// ---------------------------------------------------------------------------

/// Current on-disk schema version for [`AppConfig`]. Incremented whenever the persisted shape changes in a non-backwards-compatible way so the loader
/// can migrate (or quarantine) old files.
///
/// Version history:
/// * `1` — initial release.
/// * `2` — added `active_session_id` (Phase 7).
/// * `3` — added `workspace_root` (single-workspace model, Roadmap §1).
/// * `4` — added `ai_launch_commands` (per-agent CLI launch override),
///   `custom_processes`, and `last_open_sub_sessions` (context-menu / sub-tab feature). Migration seeds the built-in custom-process defs (`shell`,
///   `open-folder`, `vscode`) additively — only IDs not already present are inserted, never overwriting a user-edited def.
/// * `5` — replaced `prelaunch_commands` + `worktree_prelaunch_commands` with `worktree_prep_commands` (issue #63). Migration drops both legacy
///   keys silently because the old per-session semantics are incompatible with one-shot worktree prep.
/// * `6` — added `worktree_tabs`, `worktree_tab_order`, and `active_worktree_tab_id` (Issue #44: worktree-as-parent-tab). Migration synthesises
///   one [`WorktreeTab`] per unique canonical `Session.worktree_path` in `sessions.json`, preserving tab order.
/// * `7` — reparented sub-sessions from agent sessions to worktree tabs. `SubSession.parent_session_id` → `parent_worktree_tab_id`. Migration
///   resolves the old parent session's worktree path to a [`WorktreeTab`]; orphan records whose parent session or matching tab is missing are
///   dropped with a warning.
/// * `8` — added [`WorktreeTab::icon_id`] (Issue #45: per-worktree icon). Migration backfills any tab with `icon_id == 0` (the serde default for
///   pre-v8 records) by walking [`AppConfig::worktree_tab_order`] and applying [`crate::worktree_icon::pick_least_used_icon`] incrementally.
/// * `9` — generalized `ai_launch_commands` to plugin-keyed maps:
///   `{ commands: { "<plugin-id>": "<command>" }, iconDataUris: { "<plugin-id>": "<data-uri>" } }`.
///   Legacy `claude` / `copilot` fixed fields are migrated in-place on load.
/// * `10` — added `plugin_settings`, the plugin-keyed home for enable flags and user-editable plugin settings. AI launch command overrides moved to
///   `pluginSettings.ai[pluginId].settings.launchCommand`; `ai_launch_commands.commands` is retained only as legacy input compatibility.
/// * `11` — removed Arborist-managed instruction-set configuration. Claude and Copilot now rely on repository instruction discovery from `cwd`;
///   legacy session `tempFiles` are still loaded so existing sessions can restore.
pub const CONFIG_VERSION_CURRENT: u32 = 11;

/// Setting key used by AI plugins for the user-editable CLI launch override.
pub const AI_LAUNCH_COMMAND_SETTING: &str = "launchCommand";

/// Per-AI-plugin CLI launch command override.
///
/// `commands` maps plugin id → verbatim shell snippet (e.g. `"npx claude --model sonnet"`). Missing key and empty-string key both mean "use plugin
/// default program".
///
/// `icon_data_uris` caches resolved launcher icons by plugin id (`data:image/png;base64,…`). Backend-managed; frontend patches only touch `commands`.
#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AiLaunchCommands {
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    #[serde(default)]
    pub icon_data_uris: BTreeMap<String, Option<String>>,
}

impl AiLaunchCommands {
    /// Effective command override for a plugin id. Empty string = use plugin default.
    #[must_use]
    pub fn command_for_id(&self, plugin_id: &str) -> &str {
        self.commands.get(plugin_id).map_or("", String::as_str)
    }

    /// Effective command override for a persisted [`Tool`].
    #[must_use]
    pub fn command_for_tool(&self, tool: Tool) -> &str {
        self.command_for_id(tool.as_id())
    }

    /// Cached icon data URI for a plugin id, if present.
    #[must_use]
    pub fn icon_data_uri_for_id(&self, plugin_id: &str) -> Option<&str> {
        self.icon_data_uris.get(plugin_id).and_then(Option::as_deref)
    }

    /// Returns true when an icon cache entry exists for `plugin_id`, including explicit cached misses (`null` / `None`).
    #[must_use]
    pub fn has_icon_cache_entry_for_id(&self, plugin_id: &str) -> bool {
        self.icon_data_uris.contains_key(plugin_id)
    }

    /// Cached icon data URI for a persisted [`Tool`], if present.
    #[must_use]
    pub fn icon_data_uri_for_tool(&self, tool: Tool) -> Option<&str> {
        self.icon_data_uri_for_id(tool.as_id())
    }
}

/// Persisted value for a plugin-defined setting.
///
/// Kept deliberately small for v1: the renderer currently needs text values for AI launch commands, plus bool/list headroom for near-term plugin
/// settings without string-encoding structured state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum PluginSettingValue {
    String(String),
    Bool(bool),
    StringList(Vec<String>),
}

impl PluginSettingValue {
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Bool(_) | Self::StringList(_) => None,
        }
    }
}

/// Persisted settings for one plugin registration.
///
/// `enabled = None` means "use the plugin's default-enabled policy"; built-ins default to enabled unless their descriptor says otherwise.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginSettingState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub settings: BTreeMap<String, PluginSettingValue>,
}

impl PluginSettingState {
    #[must_use]
    pub fn is_enabled(&self, default_enabled: bool) -> bool {
        self.enabled.unwrap_or(default_enabled)
    }
}

/// User-controlled enable flags and settings grouped by plugin kind.
///
/// The kind buckets intentionally avoid global-id collisions: an AI plugin and a custom-process integration may both use the same stable id without
/// sharing settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginSettings {
    #[serde(default)]
    pub ai: BTreeMap<String, PluginSettingState>,
    #[serde(default)]
    pub custom_process: BTreeMap<String, PluginSettingState>,
    #[serde(default)]
    pub dashboard_widget: BTreeMap<String, PluginSettingState>,
}

impl PluginSettings {
    #[must_use]
    pub fn ai_enabled(&self, plugin_id: &str, default_enabled: bool) -> bool {
        self.ai.get(plugin_id).map_or(default_enabled, |state| state.is_enabled(default_enabled))
    }

    #[must_use]
    pub fn custom_process_enabled(&self, plugin_id: &str, default_enabled: bool) -> bool {
        self.custom_process
            .get(plugin_id)
            .map_or(default_enabled, |state| state.is_enabled(default_enabled))
    }

    #[must_use]
    pub fn dashboard_widget_enabled(&self, plugin_id: &str, default_enabled: bool) -> bool {
        self.dashboard_widget
            .get(plugin_id)
            .map_or(default_enabled, |state| state.is_enabled(default_enabled))
    }

    #[must_use]
    pub fn ai_launch_command_for_id(&self, plugin_id: &str) -> Option<&str> {
        self.ai
            .get(plugin_id)
            .and_then(|state| state.settings.get(AI_LAUNCH_COMMAND_SETTING))
            .and_then(PluginSettingValue::as_str)
    }
}

impl<'de> Deserialize<'de> for AiLaunchCommands {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            #[serde(default)]
            commands: BTreeMap<String, String>,
            #[serde(default)]
            icon_data_uris: BTreeMap<String, Option<String>>,
            // Legacy (config <= v8) fixed fields.
            #[serde(default)]
            claude: Option<String>,
            #[serde(default)]
            copilot: Option<String>,
            #[serde(default)]
            claude_icon_data_uri: Option<String>,
            #[serde(default)]
            copilot_icon_data_uri: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let mut commands = wire.commands;
        if let Some(v) = wire.claude {
            commands.entry(Tool::Claude.as_id().to_owned()).or_insert(v);
        }
        if let Some(v) = wire.copilot {
            commands.entry(Tool::Copilot.as_id().to_owned()).or_insert(v);
        }

        let mut icon_data_uris = wire.icon_data_uris;
        if let Some(v) = wire.claude_icon_data_uri {
            icon_data_uris.entry(Tool::Claude.as_id().to_owned()).or_insert(Some(v));
        }
        if let Some(v) = wire.copilot_icon_data_uri {
            icon_data_uris.entry(Tool::Copilot.as_id().to_owned()).or_insert(Some(v));
        }

        Ok(Self { commands, icon_data_uris })
    }
}

/// Persisted application configuration. Lives in `config.json` (Phase 4).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    /// Schema version of this on-disk config. Bumped when the layout changes; the loader quarantines files with versions it does not understand.
    pub config_version: u32,
    /// Active workspace root: the single git repository the app operates within. `None` until the user picks one in the first-boot picker (Roadmap
    /// §1.1). When set, takes precedence over `worktree_roots` for session-creation worktree discovery. Added in `configVersion = 3`.
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,
    pub worktree_roots: Vec<PathBuf>,
    /// Shell commands run **once** in the new worktree's directory after
    /// `worktree_create` succeeds (issue #63). Joined with ` && ` and passed to
    /// the platform shell. Output is captured to a per-prep log file under
    /// `<app_data_dir>/worktree-prep-logs/<prep-id>.log`. Blank/whitespace-only
    /// entries are filtered out before joining.
    ///
    /// `#[serde(default)]` so v4 configs that lack the new field still
    /// deserialize cleanly — the migration logic in `config_store.rs` then
    /// bumps the version stamp and rewrites the file.
    #[serde(default)]
    pub worktree_prep_commands: Vec<String>,
    /// Legacy AI launch command input plus backend-managed icon cache. The command source of truth moved to `plugin_settings` in configVersion 10.
    #[serde(default)]
    pub ai_launch_commands: AiLaunchCommands,
    /// Per-plugin enable flags and user-editable settings. Added in `configVersion = 10`; AI launch command overrides live under
    /// `pluginSettings.ai[pluginId].settings.launchCommand`.
    #[serde(default)]
    pub plugin_settings: PluginSettings,
    pub last_open_sessions: Vec<SessionId>,
    pub tab_order: Vec<SessionId>,
    /// ID of the most recently focused session. Persisted by `session_focus` and consulted by Phase 8+ on launch to decide which tab to show active.
    /// Cleared when the active session is closed. Added in `configVersion = 2`.
    #[serde(default)]
    pub active_session_id: Option<SessionId>,
    /// User-defined custom-process launchers exposed in the tab context menu. Built-in defs (`shell`, `open-folder`, `vscode`) are seeded on
    /// migration to `configVersion = 4` if absent; the user is free to edit, disable, or delete them. Order is preserved as the on-the- wire (Vec)
    /// order so the Settings tab stays stable across restarts.
    #[serde(default)]
    pub custom_processes: Vec<CustomProcessDef>,
    /// Lightweight restore records for sub-tabs (`SubSession`s) that were open at last shutdown. On launch the restore pass re-creates each terminal
    /// sub-session by re-spawning the matching `CustomProcessDef`; application sub-sessions come back in `Exited` (greyed) state and re-launch on
    /// user click. Records whose `defId` no longer exists in `custom_processes` are silently dropped at restore time.
    #[serde(default)]
    pub last_open_sub_sessions: Vec<SubSessionRecord>,
    /// First-class worktree tab records. Each represents a top-level
    /// sidebar tab; AI sessions and custom-process sub-sessions group
    /// underneath by matching `WorktreeTab.path == Session.worktree_path`.
    /// Added in `configVersion = 5` (Issue #44).
    #[serde(default)]
    pub worktree_tabs: Vec<WorktreeTab>,
    /// Top-level sidebar ordering over worktree tab IDs. Authoritative
    /// order for the sidebar; individual `WorktreeTab.tab_index` fields
    /// are secondary / derived. Added in `configVersion = 5`.
    #[serde(default)]
    pub worktree_tab_order: Vec<WorktreeTabId>,
    /// The most recently focused worktree tab. Drives which parent tab
    /// is highlighted on launch. Added in `configVersion = 5`.
    #[serde(default)]
    pub active_worktree_tab_id: Option<WorktreeTabId>,
    /// User-chosen width of the left sidebar in CSS pixels (Issue #94). `None` means "use the frontend default" (224 px today).
    /// Clamped to `[SIDEBAR_WIDTH_MIN_PX, SIDEBAR_WIDTH_MAX_PX]` on write so a hand-edited config can't shove the column off-screen.
    /// Backwards-compatible add: pre-#94 configs lack the field and serde fills it with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_width_px: Option<u32>,
    /// User-chosen colour-scheme preference (Issue #151). Defaults to `System` (follow OS). Backwards-compatible: pre-#151 configs lack the field
    /// and serde fills it with `ThemeMode::System`.
    #[serde(default)]
    pub theme: ThemeMode,
}

/// Lower bound for the resizable sidebar width (CSS px). Narrow enough to still show ~12 chars of label.
pub const SIDEBAR_WIDTH_MIN_PX: u32 = 180;
/// Upper bound for the resizable sidebar width (CSS px). Wide enough for full branch names without consuming half the window.
pub const SIDEBAR_WIDTH_MAX_PX: u32 = 480;

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION_CURRENT,
            workspace_root: None,
            worktree_roots: Vec::new(),
            worktree_prep_commands: Vec::new(),
            ai_launch_commands: AiLaunchCommands::default(),
            plugin_settings: PluginSettings::default(),
            last_open_sessions: Vec::new(),
            tab_order: Vec::new(),
            active_session_id: None,
            custom_processes: Vec::new(),
            last_open_sub_sessions: Vec::new(),
            worktree_tabs: Vec::new(),
            worktree_tab_order: Vec::new(),
            active_worktree_tab_id: None,
            sidebar_width_px: None,
            theme: ThemeMode::default(),
        }
    }
}

impl AppConfig {
    #[must_use]
    pub fn ai_plugin_enabled_for_tool(&self, tool: Tool) -> bool {
        self.plugin_settings.ai_enabled(tool.as_id(), true)
    }

    #[must_use]
    pub fn ai_launch_command_for_id(&self, plugin_id: &str) -> &str {
        self.plugin_settings
            .ai_launch_command_for_id(plugin_id)
            .unwrap_or_else(|| self.ai_launch_commands.command_for_id(plugin_id))
    }

    #[must_use]
    pub fn ai_launch_command_for_tool(&self, tool: Tool) -> &str {
        self.ai_launch_command_for_id(tool.as_id())
    }

    /// Set the source-of-truth AI launch command setting and invalidate the cached launcher icon if the effective command changed.
    pub fn set_ai_launch_command(&mut self, plugin_id: String, command: String) {
        let changed = self.ai_launch_command_for_id(&plugin_id) != command.as_str();
        self.plugin_settings
            .ai
            .entry(plugin_id.clone())
            .or_default()
            .settings
            .insert(AI_LAUNCH_COMMAND_SETTING.to_owned(), PluginSettingValue::String(command));
        self.ai_launch_commands.commands.remove(&plugin_id);
        if changed {
            self.ai_launch_commands.icon_data_uris.remove(&plugin_id);
        }
    }

    pub fn migrate_legacy_ai_launch_commands_to_plugin_settings(&mut self) {
        let commands = std::mem::take(&mut self.ai_launch_commands.commands);
        for (plugin_id, command) in commands {
            let state = self.plugin_settings.ai.entry(plugin_id).or_default();
            state
                .settings
                .entry(AI_LAUNCH_COMMAND_SETTING.to_owned())
                .or_insert(PluginSettingValue::String(command));
        }
    }
}

/// Partial form of [`AiLaunchCommands`]. Keys present in `commands` overwrite
/// only those plugin command entries; omitted keys are untouched.
#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialAiLaunchCommands {
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub commands: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for PartialAiLaunchCommands {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            #[serde(default)]
            commands: BTreeMap<String, String>,
            // Legacy fixed fields kept for backwards compatibility.
            #[serde(default)]
            claude: Option<String>,
            #[serde(default)]
            copilot: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let mut commands = wire.commands;
        if let Some(v) = wire.claude {
            commands.entry(Tool::Claude.as_id().to_owned()).or_insert(v);
        }
        if let Some(v) = wire.copilot {
            commands.entry(Tool::Copilot.as_id().to_owned()).or_insert(v);
        }
        Ok(Self { commands })
    }
}

/// Patch for one plugin's persisted settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialPluginSettingState {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub settings: BTreeMap<String, PluginSettingValue>,
}

/// Patch for plugin settings grouped by plugin kind. Missing kind buckets and missing plugin ids are left untouched.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialPluginSettings {
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub ai: BTreeMap<String, PartialPluginSettingState>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub custom_process: BTreeMap<String, PartialPluginSettingState>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub dashboard_widget: BTreeMap<String, PartialPluginSettingState>,
}

/// Patch over [`AppConfig`]: every field optional so callers can update one key at a time. Phase 4 deep-merges this into the persisted config.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartialAppConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub config_version: Option<u32>,
    /// Tri-state: absent → leave alone; `null` → clear; `"<path>"` → set. Mirrors the encoding used for `active_session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub workspace_root: Option<Option<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worktree_roots: Option<Vec<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worktree_prep_commands: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ai_launch_commands: Option<PartialAiLaunchCommands>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plugin_settings: Option<PartialPluginSettings>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_open_sessions: Option<Vec<SessionId>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tab_order: Option<Vec<SessionId>>,
    /// Tri-state: absent → leave alone; `null` → clear; `"<uuid>"` → set. Encoded with the `double_option` helper so JSON `null` is preserved as
    /// `Some(None)` rather than collapsing to "field absent".
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub active_session_id: Option<Option<SessionId>>,
    /// Replace the entire `customProcesses` list. Absence leaves it untouched. The Settings dialog (Phase 6) sends the full edited list rather than
    /// per-row patches so ordering is unambiguous.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub custom_processes: Option<Vec<CustomProcessDef>>,
    /// Replace the entire `lastOpenSubSessions` list. Absence leaves it untouched.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_open_sub_sessions: Option<Vec<SubSessionRecord>>,
    /// Replace the worktree tabs list. Absence leaves it untouched.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worktree_tabs: Option<Vec<WorktreeTab>>,
    /// Replace the worktree tab order. Absence leaves it untouched.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worktree_tab_order: Option<Vec<WorktreeTabId>>,
    /// Tri-state: absent → leave alone; `null` → clear; `"<uuid>"` → set.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub active_worktree_tab_id: Option<Option<WorktreeTabId>>,
    /// Sidebar width (CSS px). `None` → leave alone; `Some(n)` → set (clamped to [`SIDEBAR_WIDTH_MIN_PX`, `SIDEBAR_WIDTH_MAX_PX`]).
    /// We don't expose a tri-state "clear" since the frontend always sends a concrete width; reverting to the default just sends `224`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_width_px: Option<u32>,
    /// Colour-scheme preference (Issue #151). `None` → leave alone; `Some(mode)` → set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeMode>,
}

/// serde adapter for `Option<Option<T>>`: distinguishes "absent" from "present-but-null". JSON has no native `Some(None)`, so we serialise
/// `Some(None)` as `null` and rely on `skip_serializing_if = Option::is_none` to elide the absent case.
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match v {
            // Outer None is elided by `skip_serializing_if`; this branch would only fire if the field weren't tagged with that.
            None => s.serialize_none(),
            Some(inner) => inner.serialize(s),
        }
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        // If the field is present, parse it as `Option<T>` (null → None, value → Some). Wrap in the outer `Some` to mark "present".
        Option::<T>::deserialize(d).map(Some)
    }
}

// --------------------------------------------------------------------------- Errors
// ---------------------------------------------------------------------------

/// Payload of the `session://output` event.
///
/// Mirrored on the frontend by `SessionOutputEvent` in `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionOutputEvent {
    pub session_id: SessionId,
    pub data: String,
}

/// Streaming activity report emitted by the per-session activity scanner.
/// Forwarded to the frontend as the inner payload of [`SessionActivityEvent`].
//
// `rename_all` controls only variant names. `rename_all_fields` controls the named fields *inside* each variant — without it, a field like
// `tool_call_id` would serialize as `tool_call_id` on the wire while the TS mirror in `src/types/arborist.ts` expects `toolCallId`. The frontend
// reducer (`session-store.ts::applyActivity`) reads camelCase keys, so missing this rename silently zeroes every multi-word field. Pinned by the
// `activity_event_serde_uses_camelcase_field_keys` regression test in `activity.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ActivityEvent {
    /// Window title set via `OSC 0;<title>` or `OSC 2;<title>`.
    Title { value: String },
    /// Generic "the user should look at this tab" cue: explicit notification escapes only — legacy `OSC 9;<text>` (any payload that does not
    /// match the `<n>;<...>` numeric-subcommand shape, including digit-only messages like `OSC 9;42`), the explicit `OSC 9;2;<text>`
    /// notification subcommand, and `OSC 777;notify;...`. **Standalone BEL is intentionally ignored** — both `claude` and `copilot` ring the
    /// bell as part of normal readline-style behavior (autocomplete misses, backspace at column 0, scrollback edge), which produced an
    /// unacceptable rate of false-positive "attention required" cues while the agent was simply thinking. Numeric `OSC 9` subcommands other
    /// than `2` — notably `9;4;<state>;<value>` taskbar progress, which `claude` emits continuously while thinking — are also ignored. If a
    /// CLI wants to demand attention, it must do so via a real notification OSC.
    Attention,
    /// Output is flowing. Emitted on the first byte after an idle window (or the very first byte of the session). Idempotent — only fires on the
    /// idle→working transition.
    Working,
    /// No output for the idle threshold. Emitted once per working→idle transition.
    Idle,
    /// `OSC 133;A` — start of prompt. Future-proofed; not currently emitted by `claude` or `copilot`.
    PromptStart,
    /// `OSC 133;C` — start of command (user submitted prompt). Future-proofed.
    CommandStart,
    /// `OSC 133;D[;<exit>]` — command ended with optional exit code. Future-proofed.
    CommandEnd { exit: Option<i32> },
    /// An agent turn just completed. Emitted by the per-tool metrics watcher (Copilot OTel `invoke_agent` span close; Claude transcript
    /// `assistant`-line arrival), not by the PTY-stream scanner. Carries the wall-clock duration of the turn when the source provides it.
    TurnEnd { duration_ms: Option<u64> },

    /// Agent invoked a tool; user is not yet blocked on input. Emitted by the Copilot events.jsonl tailer on `tool.execution_start`. Tracked by
    /// frontend in a per-session open-tool map; the icon flips to `runningTool` while the count > 0 and no permission is pending.
    ToolStart { tool_call_id: String, tool_name: String },
    /// Tool finished. Pairs with [`Self::ToolStart`] by `tool_call_id`. Emitted on `tool.execution_complete`.
    ToolEnd { tool_call_id: String, success: bool },
    /// Agent requested a permission (most commonly: shell-command approval); user is **blocked**. Emitted on `permission.requested` from the Copilot
    /// events.jsonl tailer. The frontend promotes this to the highest non-error display priority — this is the single most actionable cue we can give
    /// the user about a sidebar tab.
    AwaitingPermission {
        request_id: String,
        /// Short human-readable identifier for what's being approved (e.g. tool name, or `"shell"`). Surfaced in tooltips. Field is `permission_kind`
        /// (not `kind`) to avoid colliding with the serde tag on the parent enum.
        #[serde(rename = "permissionKind")]
        permission_kind: String,
        /// Optional one-line summary (e.g. the shell command). Best- effort — may be empty if the source didn't include enough detail to render
        /// meaningfully.
        summary: Option<String>,
    },
    /// Permission resolved (approved or denied). Pairs with
    /// [`Self::AwaitingPermission`] by `request_id`. Emitted on
    /// `permission.completed`.
    PermissionResolved { request_id: String, approved: bool },
    /// An assistant turn began. Emitted on `assistant.turn_start` from the Copilot events.jsonl tailer. The frontend uses this together with the
    /// open-tool/open-permission counts to derive the `thinking` display state (in-turn AND nothing else open).
    TurnStart,
}

/// Payload of the `session://activity` event.
///
/// `event` is a tagged enum (see [`ActivityEvent`]): `{ kind: "title", value: "..." }`, `{ kind: "attention" }`, `{ kind: "working"
/// }`, `{ kind: "idle" }`, etc.
///
/// Mirrored on the frontend by `SessionActivityEvent` in `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivityEvent {
    pub session_id: SessionId,
    #[serde(flatten)]
    pub event: ActivityEvent,
}

/// Snapshot of the latest token / context-window observation for a session, used both as the payload for the `session://metrics` event and as the
/// in-memory state the frontend renders. All fields except `session_id` and `observed_at` are optional: a snapshot may carry only a token count if
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
    /// Percentage of the context window in use, 0..=100. Omitted when the model's context limit cannot be resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used_pct: Option<u8>,
    /// Tokens currently counted against the context window (= `input + cache_creation + cache_read + output` for the latest observed assistant turn).
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
    /// True when two snapshots carry the same data — every field except `observed_at`. Used by the per-tool watchers to suppress redundant
    /// `session://metrics` emissions when nothing has changed since the previous poll. Comparing `Self` directly via derived `PartialEq` would always
    /// differ because `observed_at` advances every poll.
    ///
    /// **Future-proofing:** the destructuring patterns below intentionally list every field by name (no `..`) so that adding a new field to
    /// `SessionMetricsEvent` is a compile error here. That forces an explicit decision: include the new field in the dedup comparison, or document
    /// why it's excluded (like `observed_at`).
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

/// Payload of the `session://status` event.
///
/// `message` is an optional human-readable note that accompanies the status change — used today for stale-worktree restore failures (Roadmap §4.3) so
/// the terminal overlay can explain *why* the session is in `error` state instead of just showing a generic banner.
///
/// Mirrored on the frontend by `SessionStatusEvent` in `src/types/arborist.ts`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusEvent {
    pub session_id: SessionId,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// --------------------------------------------------------------------------- Command argument shapes (see docs/architecture.md#command-and-event-contract)
// ---------------------------------------------------------------------------

/// Arguments for the `session_create` command.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionCreateArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateArgs {
    pub tool: Tool,
    pub worktree_path: PathBuf,
    /// Initial PTY width (columns) the child process will see at startup. The frontend measures the terminal host before calling `session_create` so
    /// the CLI's first paint (e.g., a Copilot/Claude splash screen) renders at the right width — without this, the child reads 80 cols from the OS,
    /// draws its splash narrow, and never re-paints when the later `session_resize` arrives.
    pub cols: u16,
    /// Initial PTY height (rows). See [`Self::cols`].
    pub rows: u16,
}

/// Arguments for any command keyed only by session id (`session_focus`, `session_restart`). `session_close` uses the richer
/// [`SessionCloseArgs`] so the user can opt into worktree deletion.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionIdArg`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdArg {
    pub session_id: SessionId,
}

/// Arguments for `session_close`. Extends [`SessionIdArg`] with an opt-in flag that removes the session's git worktree from disk after the PTY is
/// torn down. The backend gates removal behind safety checks (never the main worktree, never a path outside the configured workspace root); see
/// `commands::session::session_close_impl`.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionCloseArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseArgs {
    pub session_id: SessionId,
    /// When `true`, run `git worktree remove --force <worktree_path>` after terminating the PTY. Defaults to `false` so legacy callers (and any
    /// future code that forgets to set the flag) preserve existing behaviour.
    #[serde(default)]
    pub delete_worktree: bool,
}

/// Result of `session_close`. The session record is removed on success; if the PTY kill was issued but reaping could not be confirmed, the warning is
/// reported here and worktree deletion is refused. If the user opted into worktree deletion and the `git worktree remove` step failed, the failure is
/// also reported here as a warning string rather than as a hard error so callers can converge UI state regardless.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SessionCloseResult`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseResult {
    /// Human-readable warning from PTY teardown, currently populated when the child kill was issued but process reaping could not be confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown_error: Option<String>,
    /// Human-readable error message from `git worktree remove`. `None` when worktree deletion was not requested or succeeded.
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

/// Arguments for `session_restart`. Carries the current PTY dimensions so the freshly-spawned child process sees the right size from its very first
/// `ioctl(TIOCGWINSZ)` / ConPTY query, instead of starting at the OS-default 80×24 and rendering its initial output (splash screen, shell prompt, …)
/// at the wrong width.
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

// --------------------------------------------------------------------------- Sub-session command/event payloads (Phase 2 backend; frontend wraps in
// Phase 4). Mirrored on the frontend in `src/lib/tauri-bridge.ts`. ---------------------------------------------------------------------------

/// Arguments for `subsession_create`. The chosen [`CustomProcessDef`] is looked up in `AppConfig.customProcesses`; rejected if disabled or missing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionCreateArgs {
    pub parent_worktree_tab_id: WorktreeTabId,
    pub def_id: CustomProcessDefId,
}

/// Arguments for `subsession_close` / `subsession_focus`. A bare-id envelope keeps the wire shape uniform with [`SessionIdArg`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionIdArg {
    pub id: SubSessionId,
}

/// What the user wants to happen to the underlying app when their sub-tab is closed. Terminal sub-tabs ignore the variant — there's no GUI window to
/// address — and always behave as `TabOnly` (the PTY child gets killed because the tab IS the process).
///
/// The variants exist so app sub-tabs (VS Code, etc.) can offer the user the choice between detaching the tab while leaving the editor open, asking
/// the editor to close itself, or force-killing the underlying process (escape hatch when the editor refuses).
// `rename_all_fields` is inert today (all variants are unit-only) but guards against a future struct variant — without it, named fields in a future
// variant would serialise snake_case and silently desync from the TS mirror. Same defensive pattern as `activity::ActivityEvent`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SubSessionCloseIntent {
    /// Detach the sub-tab from Arborist; leave any external app window running. Default — preserves prior behaviour.
    #[default]
    TabOnly,
    /// Detach AND ask the OS to politely close the matched app window (Windows: `WM_CLOSE` to the resolver-matched HWND). Best-effort: the app may
    /// show a save-changes prompt and stay open; Arborist's tab is removed regardless.
    RequestAppClose,
    /// Detach AND force-kill the underlying process (`TerminateProcess` on Windows; `kill -9` on Unix). Use only when `RequestAppClose` has been
    /// refused or isn't available.
    ForceKill,
}

/// Arguments for `subsession_close`. `intent` defaults to
/// [`SubSessionCloseIntent::TabOnly`] when the field is absent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionCloseArgs {
    pub id: SubSessionId,
    #[serde(default)]
    pub intent: SubSessionCloseIntent,
}

/// Arguments for `subsession_list`. When `parent_worktree_tab_id` is `None` the result is the full set across every worktree tab; when `Some(id)`
/// the result is filtered to that tab and ordered as the sub-sessions were created.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionListArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_worktree_tab_id: Option<WorktreeTabId>,
}

/// Arguments for `subsession_input` (terminal sub-tabs only).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionInputArgs {
    pub id: SubSessionId,
    pub data: String,
}

/// Arguments for `subsession_resize` (terminal sub-tabs only).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionResizeArgs {
    pub id: SubSessionId,
    pub cols: u16,
    pub rows: u16,
}

/// Payload of `subsession://status`. Parallels [`SessionStatusEvent`].
///
/// MIRROR: `src/lib/tauri-bridge.ts::SubSessionStatusEvent`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionStatusEvent {
    pub id: SubSessionId,
    pub status: SubSessionStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message: Option<String>,
}

/// Payload of `subsession://exited`. Emitted when an Application sub-tab's detached process is observed to have exited. Phase 3 wires this from the
/// application-launcher polling thread; Phase 2's terminal sub-tabs rely on `subsession://status` + `SubSessionStatus::Exited` instead.
///
/// MIRROR: `src/lib/tauri-bridge.ts::SubSessionExitedEvent`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionExitedEvent {
    pub id: SubSessionId,
    /// Exit code if available; absent on signal/error termination.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit_code: Option<i32>,
}

/// Payload of `subsession://restored`. Emitted by the Phase 7 restore second pass for every sub-session re-materialised from
/// `AppConfig.lastOpenSubSessions`. The frontend store's `applyRestored` reducer inserts the entry idempotently so subsequent `subsession:// status`
/// events for the same id land on a real row.
///
/// Carrying the full [`SubSession`] (rather than just the id) means the frontend doesn't have to issue a follow-up `subsession_list` after restore —
/// it has the data it needs immediately.
///
/// MIRROR: `src/types/arborist.ts::SubSessionRestoredEvent`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionRestoredEvent {
    pub sub_session: SubSession,
}

// ---------------------------------------------------------------------------
// Worktree tab command argument shapes (Issue #44)
// ---------------------------------------------------------------------------

/// Arguments for `worktree_tab_open`. The backend canonicalises `path`,
/// checks it exists on disk, and returns an existing tab if one matches
/// the canonical path (idempotent) or creates a new one.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTabOpenArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTabOpenArgs {
    pub path: String,
}

/// How `worktree_tab_close` should handle application-kind sub-sessions when
/// cascading under a closing worktree tab.
///
/// Terminal sub-sessions and AI sessions are unaffected by this setting; they
/// are always terminated by their existing close paths.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTabAppClosePolicy`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum WorktreeTabAppClosePolicy {
    /// Remove Arborist tracking only; keep external app processes running.
    #[default]
    Detach,
    /// Attempt to terminate app sub-sessions (graceful first, then safe fallback where allowed).
    Terminate,
}

/// Arguments for `worktree_tab_close`. Cascades close to all child
/// sessions and sub-sessions under the tab. The optional `delete_worktree`
/// flag asks the backend to run `git worktree remove --force` on the
/// tab's worktree directory after every child has been torn down. The
/// backend refuses to delete the configured workspace root (main worktree)
/// or any path outside the workspace root.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTabCloseArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTabCloseArgs {
    pub id: WorktreeTabId,
    #[serde(default)]
    pub delete_worktree: bool,
    #[serde(default)]
    pub app_close_policy: WorktreeTabAppClosePolicy,
}

/// Arguments for `worktree_tab_focus`.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTabFocusArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTabFocusArgs {
    pub id: WorktreeTabId,
}

/// Arguments for `worktree_tab_reorder`. The full ordered list replaces
/// the persisted `worktree_tab_order` and updates each tab's `tab_index`.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTabReorderArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTabReorderArgs {
    pub ids: Vec<WorktreeTabId>,
}

/// Arguments for `worktree_tab_set_active_child`. `child_id` of `None`
/// clears the active child, causing the worktree dashboard to show.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTabSetActiveChildArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTabSetActiveChildArgs {
    pub id: WorktreeTabId,
    #[serde(default)]
    pub child_id: Option<ChildId>,
}

/// Result of `worktree_tab_close`. Reports any errors encountered while
/// cascading close to child sessions/sub-sessions without failing the
/// whole operation. The optional `worktree_delete_error` reports a
/// failure of the post-cascade `git worktree remove` step (only set when
/// the caller passed `delete_worktree=true`); the worktree tab itself is
/// always removed from config regardless of deletion outcome so the UI
/// can converge on a "tab gone" state.
///
/// MIRROR: `src/types/arborist.ts::WorktreeTabCloseResult`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTabCloseResult {
    /// Per-child errors that occurred during cascade close.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_errors: Vec<String>,
    /// Error message from the post-cascade `git worktree remove --force` step,
    /// only populated when the caller asked to delete the worktree directory
    /// and the deletion failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_delete_error: Option<String>,
}

/// Arguments for `workspace_validate` (Roadmap §1.1).
///
/// MIRROR: `src/lib/tauri-bridge.ts::WorkspaceValidateArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceValidateArgs {
    pub path: String,
}

/// Result of `workspace_validate`. `valid: true` iff the candidate path is an absolute, existing directory containing a git repository. On failure,
/// `error` carries a short human-readable reason for inline picker feedback.
///
/// `alreadyOpenInAnotherInstance` is an **advisory** flag set when a non-blocking probe of the per-(branch, workspace) `.lock` file could not acquire
/// the OS lock — i.e. another Arborist process **bound to the same `(branch, workspace)` pair** currently holds it. The lock is OS-advisory: if a
/// previous owner exited (cleanly or by crash) the OS releases the file handle and the probe will succeed, so this flag does **not** indicate a stale
/// lock — `WorkspaceLockGuard` does not require any explicit cleanup (see `workspace_lock.rs` "Crash semantics"). Contention with a different branch
/// (e.g. release vs dev build of the same workspace) is **not** detected here because each branch gets its own scoped lock path under
/// `<app_data_dir>/[branches/<branch>/]workspaces/<key>/.lock`. The picker UI surfaces a warning but still allows the user to confirm; the actual
/// lock is acquired transactionally by `workspace_switch` (or boot), which will fail with `WorkspaceLocked` if the contention is still present. The
/// probe treats a missing `.lock` file as "no contention" (it short-circuits and returns `Ok(true)` without creating the file), so the missing file
/// alone never produces an absent value — it serialises as `Some(false)` ("probed, no contention"). The field is `None` only when the path failed
/// earlier validation (no probe attempted), the caller passed `app_data_dir = None`, or the probe itself hit an I/O error.
///
/// MIRROR: `src/types/arborist.ts::WorkspaceValidateResult`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceValidateResult {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// `Some(true)` if a non-blocking lock probe revealed contention; `Some(false)` if the probe succeeded; `None` if no probe was performed (e.g.
    /// the path failed earlier validation).
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

/// Result of `worktree_create`. `path` is the canonical absolute path to the newly-created worktree directory. `prep` is `Some(...)` iff the user has
/// configured at least one non-blank `worktree_prep_commands` entry; the prep child runs in the background and reports completion via the
/// `worktree://prep` event channel.
///
/// MIRROR: `src/types/arborist.ts::WorktreeCreateResult`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCreateResult {
    pub path: PathBuf,
    /// Always present on the wire (serialised as `null` when no prep was kicked off) so the TS contract is unambiguous.
    pub prep: Option<WorktreePrepInfo>,
}

/// Newtype wrapper for prep-run identifiers. UUID v4. Distinct from `SessionId` so the two spaces cannot accidentally cross over.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorktreePrepId(pub Uuid);

impl WorktreePrepId {
    #[must_use]
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for WorktreePrepId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Info describing a kicked-off prep run, returned in [`WorktreeCreateResult::prep`] and echoed in [`WorktreePrepEvent`] payloads.
///
/// MIRROR: `src/types/arborist.ts::WorktreePrepInfo`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreePrepInfo {
    pub prep_id: WorktreePrepId,
    /// The new worktree's canonical absolute path.
    pub worktree_path: PathBuf,
    /// Absolute path to the per-prep log file under `<app_data_dir>/worktree-prep-logs/`.
    pub log_path: PathBuf,
}

/// Lifecycle event for a worktree-prep run. Emitted on the Tauri channel `worktree://prep`.
///
/// Payloads are intentionally self-contained (each variant carries `prep_id`, `worktree_path`, `log_path`) so the frontend store can render a
/// completed-prep banner even if it missed the corresponding `started` event (e.g. a very fast prep that exits before the listener is attached).
///
/// MIRROR: `src/types/arborist.ts::WorktreePrepEvent`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum WorktreePrepEvent {
    /// Prep was kicked off (the shell child has been spawned, or spawn failed and we're emitting `started` followed immediately by `exited` for
    /// presentation symmetry — see `worktree_prep::maybe_spawn`).
    Started {
        prep_id: WorktreePrepId,
        worktree_path: PathBuf,
        log_path: PathBuf,
        /// Joined-with-` && ` script that was passed to the shell. Surfaced so the banner can show what's running without the user having to open
        /// the log file.
        command: String,
        /// Unix timestamp (seconds since epoch) when the child was spawned (or when the spawn-failure was recorded).
        started_at: i64,
    },
    /// Prep finished. `exit_code` is `None` for signal exits, spawn failures, or any non-clean termination; `error_message` carries a human-readable
    /// reason in those cases. Both nullable fields are always present on the wire (serialised as `null`) so the TS contract is unambiguous.
    Exited {
        prep_id: WorktreePrepId,
        worktree_path: PathBuf,
        log_path: PathBuf,
        exit_code: Option<i32>,
        error_message: Option<String>,
        started_at: i64,
        finished_at: i64,
    },
}

/// Arguments for `worktree_prep_open_log`.
///
/// MIRROR: `src/types/arborist.ts::WorktreePrepOpenLogArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreePrepOpenLogArgs {
    pub log_path: PathBuf,
}

// --------------------------------------------------------------------------- Custom processes / sub-sessions (Phase 1: types only; backend lands in
// Phases 2–3, frontend in 4–6, restore in 7). ---------------------------------------------------------------------------

/// A user- or built-in-defined "custom process" launcher. Persisted in
/// [`AppConfig::custom_processes`]. Disabled defs are visible in the
/// Settings tab (with a toggle) but hidden from the tab context menu.
///
/// `command` is a single shell command string composed exactly like a session's `composedCommand`: passed to `$SHELL -c` (or `%COMSPEC% /c` on
/// Windows) with `cwd` set to the parent session's worktree path. **The worktree path is never interpolated** into the command (see SECURITY.md
/// for injection-prevention rationale).
///
/// MIRROR: `src/types/arborist.ts::CustomProcessDef`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomProcessDef {
    pub id: CustomProcessDefId,
    pub name: String,
    pub kind: CustomProcessKind,
    pub command: String,
    /// When `false`, hidden from the context menu's "Launch…" submenu. Existing sub-sessions backed by a disabled def keep running until the user
    /// closes them (Phase 5).
    pub enabled: bool,
    /// Optional UI hint (icon name / emoji / preset key). Reserved for future use; the v1 sidebar renders a generic icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Cached `data:image/png;base64,…` URI for the app icon, resolved from `command` at def-save / first-load time. `None` until resolution succeeds
    /// (or permanently if no executable can be found, e.g. for shell built-ins like `cd`). The frontend treats `Some` as overriding the emoji `icon`
    /// glyph.
    ///
    /// Filled in by the backend's `backfill_icons` pass — frontend patches that omit this field do **not** clobber the cache, see
    /// `config_store::merge_partial`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_data_uri: Option<String>,
}

/// In-memory + on-the-wire representation of a sub-tab. Sub-sessions live in a parallel `SubSessionStore` (Phase 2); only the lightweight
/// [`SubSessionRecord`] is persisted across restarts. Identifies its parent worktree tab via `parent_worktree_tab_id` and the def that launched it
/// via `def_id` (so the Sidebar can re-resolve the user-facing name/icon if the def is renamed).
///
/// MIRROR: `src/types/arborist.ts::SubSession`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSession {
    pub id: SubSessionId,
    pub parent_worktree_tab_id: WorktreeTabId,
    pub def_id: CustomProcessDefId,
    pub kind: CustomProcessKind,
    pub label: String,
    pub status: SubSessionStatus,
    /// OS PID of the underlying child (PTY child or detached GUI process). Cleared on exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Composed launch command captured once at sub-session creation and reused verbatim if the sub-session is re-spawned (Phase 2 will use it for
    /// terminal sub-tab restart). Mirrors [`Session::composed_command`]: later edits to the source [`CustomProcessDef`] do not retroactively rewrite
    /// already-running sub-sessions.
    pub composed_command: String,
    pub created_at: i64,
}

/// Lightweight restore record persisted in
/// [`AppConfig::last_open_sub_sessions`]. Carries only what the restore pass needs to attempt re-creation: the def the sub-tab was launched from,
/// the parent worktree tab it belongs to, the user-facing label (so the sidebar can render the tab even before restore resolves), and the kind (so
/// an Application sub-tab can come back greyed without re-launching the GUI).
///
/// ## Migration note (v5 → v6)
///
/// v5 records carry `parentSessionId` (a [`SessionId`]). v6 records carry `parentWorktreeTabId` (a [`WorktreeTabId`]). Both fields are
/// `Option`+`serde(default)` so v5 JSON deserialises into the current struct; the migration step in `config_store::migrate_v5_to_v6` rewrites
/// the legacy field to the canonical one, and `sanitize_loaded_sub_session_records` drops any record that still lacks `parentWorktreeTabId`
/// after migration.
///
/// MIRROR: `src/types/arborist.ts::SubSessionRecord`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubSessionRecord {
    pub id: SubSessionId,
    /// Legacy v5 field — present in persisted JSON from older versions. Cleared by `migrate_v5_to_v6`; never set by new code paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    /// Canonical v6 field — the worktree tab this sub-session belongs to. Required at runtime; `sanitize_loaded_sub_session_records` drops records
    /// where this is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_worktree_tab_id: Option<WorktreeTabId>,
    pub def_id: CustomProcessDefId,
    pub kind: CustomProcessKind,
    pub label: String,
    /// Resolved command at sub-session creation time. Persisted so a later edit to the underlying [`CustomProcessDef`] doesn't change what the
    /// restored sub-session would relaunch — matches the "compose once, store-and-reuse" invariant for top-level sessions.
    #[serde(default)]
    pub composed_command: String,
}

// --------------------------------------------------------------------------- Workspace switch (in-app pivot to a different workspace root)
// ---------------------------------------------------------------------------

/// Arguments for `workspace_switch` (Phase 7 — in-app workspace switch).
///
/// MIRROR: `src/types/arborist.ts::WorkspaceSwitchArgs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSwitchArgs {
    pub path: String,
}

/// Result of `workspace_switch`. `workspaceRoot` is the **canonical** path the backend bound to (which may differ in casing / separators from the
/// path the frontend submitted). `noOp` is `true` if the requested path resolved to the workspace already in use; in that case `config` and
/// `sessions` mirror the *current* (unchanged) workspace's state so the wire payload is non-nullable but the frontend can short-circuit adoption on
/// the flag.
///
/// On a real swap, `config` and `sessions` reflect the **new** workspace's state *after* the inline restore loop has run — sessions are already in
/// `Starting` (or `Error` if the restore preflight failed), so the frontend can adopt everything in one render with no flicker. The
/// `workspace://changed` event was deleted in PR5; this result is now the sole authoritative state-transfer channel for in-app switches.
///
/// MIRROR: `src/types/arborist.ts::WorkspaceSwitchResult`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSwitchResult {
    pub workspace_root: PathBuf,
    pub no_op: bool,
    pub config: AppConfig,
    pub sessions: Vec<SessionView>,
}

// --------------------------------------------------------------------------- Errors
// ---------------------------------------------------------------------------

/// Crate-wide error type. Internal Rust code consumes this via `?`; at the Tauri command boundary it is converted to [`AppError`] so the frontend
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

    /// A custom-process def submitted to `config_set` failed validation (empty `id`/`name`/`command`, malformed `id`, or duplicate `id`).
    #[error("invalid custom process def: {0}")]
    InvalidCustomProcessDef(String),

    /// A plugin settings patch failed validation (for example, a text-only setting received a boolean value).
    #[error("invalid plugin settings: {0}")]
    InvalidPluginSettings(String),

    /// A required external tool (e.g. `wmctrl` for Linux window focus, `code` for the VS Code launcher) is not on `PATH`. The payload is the missing
    /// tool's name so the frontend can surface a hint.
    #[error("tool missing: {0}")]
    ToolMissing(String),

    /// The requested operation does not apply to this resource (e.g. sending PTY input to an application-kind sub-session). Distinct from
    /// `NotImplemented` — the operation is by design unavailable.
    #[error("not applicable: {0}")]
    NotApplicable(String),

    /// An OS-level permission was denied (e.g. macOS Accessibility for `osascript` window activation). Surfaced as a distinct code so the frontend
    /// can prompt the user to grant the permission.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The platform does not support the requested feature (e.g. window focus on Wayland without a compositor extension). Distinct from `ToolMissing`
    /// — installing a tool will not help.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Spawning an application-kind process failed. Carries the underlying error message for diagnostics.
    #[error("app spawn failed: {0}")]
    AppSpawnFailed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Stable string discriminant exposed to the frontend via [`AppError`]. **Never rename these without updating the TypeScript callers** — the UI
    /// may branch on them.
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
            Self::InvalidCustomProcessDef(_) => "InvalidCustomProcessDef",
            Self::InvalidPluginSettings(_) => "InvalidPluginSettings",
            Self::ToolMissing(_) => "ToolMissing",
            Self::NotApplicable(_) => "NotApplicable",
            Self::PermissionDenied(_) => "PermissionDenied",
            Self::Unsupported(_) => "Unsupported",
            Self::AppSpawnFailed(_) => "AppSpawnFailed",
            Self::Io(_) => "Io",
            Self::Serde(_) => "Serde",
            Self::Internal(_) => "Internal",
        }
    }
}

/// Wire shape of an error sent from Rust to the frontend. Always `{ "code": "<variant>", "message": "<human-readable>" }`.
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

// --------------------------------------------------------------------------- Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::{json, Value};

    /// Round-trip a value through JSON and assert the resulting [`Value`] equals the supplied fixture, *and* that deserialising the fixture
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
            "status": "running",
            "pid": 12345,
            "createdAt": 1_700_000_000,
            "tabIndex": 0
        })
    }

    fn app_config_fixture() -> (AppConfig, Value) {
        let value = AppConfig {
            config_version: 11,
            workspace_root: Some(PathBuf::from("/repo")),
            worktree_roots: vec![PathBuf::from("/repo")],
            worktree_prep_commands: vec!["npm install".to_owned()],
            ai_launch_commands: AiLaunchCommands {
                commands: BTreeMap::new(),
                icon_data_uris: BTreeMap::new(),
            },
            plugin_settings: PluginSettings {
                ai: BTreeMap::from([(
                    "claude".to_owned(),
                    PluginSettingState {
                        enabled: Some(true),
                        settings: BTreeMap::from([(AI_LAUNCH_COMMAND_SETTING.to_owned(), PluginSettingValue::String("npx claude".to_owned()))]),
                    },
                )]),
                custom_process: BTreeMap::new(),
                dashboard_widget: BTreeMap::new(),
            },
            last_open_sessions: vec![SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"))],
            tab_order: vec![SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"))],
            active_session_id: Some(SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"))),
            custom_processes: vec![CustomProcessDef {
                id: CustomProcessDefId::new("shell"),
                name: "Shell".to_owned(),
                kind: CustomProcessKind::Terminal,
                command: "sh -i".to_owned(),
                enabled: true,
                icon: None,
                icon_data_uri: None,
            }],
            last_open_sub_sessions: vec![SubSessionRecord {
                id: SubSessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid")),
                parent_session_id: None,
                parent_worktree_tab_id: Some(WorktreeTabId(Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("uuid"))),
                def_id: CustomProcessDefId::new("shell"),
                kind: CustomProcessKind::Terminal,
                label: "Shell".to_owned(),
                composed_command: "sh -i".to_owned(),
            }],
            worktree_tabs: vec![],
            worktree_tab_order: vec![],
            active_worktree_tab_id: None,
            sidebar_width_px: None,
            theme: ThemeMode::System,
        };
        let fixture = json!({
            "configVersion": 11,
            "workspaceRoot": "/repo",
            "worktreeRoots": ["/repo"],
            "worktreePrepCommands": ["npm install"],
            "aiLaunchCommands": {
                "commands": {},
                "iconDataUris": {}
            },
            "pluginSettings": {
                "ai": {
                    "claude": {
                        "enabled": true,
                        "settings": {
                            "launchCommand": "npx claude"
                        }
                    }
                },
                "customProcess": {},
                "dashboardWidget": {}
            },
            "lastOpenSessions": ["550e8400-e29b-41d4-a716-446655440000"],
            "tabOrder": ["550e8400-e29b-41d4-a716-446655440000"],
            "activeSessionId": "550e8400-e29b-41d4-a716-446655440000",
            "customProcesses": [
                {
                    "id": "shell",
                    "name": "Shell",
                    "kind": "terminal",
                    "command": "sh -i",
                    "enabled": true
                }
            ],
            "lastOpenSubSessions": [
                {
                    "id": "11111111-1111-1111-1111-111111111111",
                    "parentWorktreeTabId": "22222222-2222-2222-2222-222222222222",
                    "defId": "shell",
                    "kind": "terminal",
                    "label": "Shell",
                    "composedCommand": "sh -i"
                }
            ],
            "worktreeTabs": [],
            "worktreeTabOrder": [],
            "activeWorktreeTabId": null,
            "theme": "system"
        });
        (value, fixture)
    }

    fn custom_process_def_fixture() -> (CustomProcessDef, Value) {
        let value = CustomProcessDef {
            id: CustomProcessDefId::new("vscode"),
            name: "VS Code".to_owned(),
            kind: CustomProcessKind::Application,
            command: "code .".to_owned(),
            enabled: true,
            icon: Some("vscode".to_owned()),
            icon_data_uri: None,
        };
        let fixture = json!({
            "id": "vscode",
            "name": "VS Code",
            "kind": "application",
            "command": "code .",
            "enabled": true,
            "icon": "vscode"
        });
        (value, fixture)
    }

    fn sub_session_fixture() -> (SubSession, Value) {
        let value = SubSession {
            id: SubSessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid")),
            parent_worktree_tab_id: WorktreeTabId(Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("uuid")),
            def_id: CustomProcessDefId::new("shell"),
            kind: CustomProcessKind::Terminal,
            label: "Shell".to_owned(),
            status: SubSessionStatus::Running,
            pid: Some(42),
            composed_command: "cmd && cmd".to_owned(),
            created_at: 1_700_000_000,
        };
        let fixture = json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "parentWorktreeTabId": "22222222-2222-2222-2222-222222222222",
            "defId": "shell",
            "kind": "terminal",
            "label": "Shell",
            "status": "running",
            "pid": 42,
            "composedCommand": "cmd && cmd",
            "createdAt": 1_700_000_000
        });
        (value, fixture)
    }

    fn sub_session_record_fixture() -> (SubSessionRecord, Value) {
        let value = SubSessionRecord {
            id: SubSessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid")),
            parent_session_id: None,
            parent_worktree_tab_id: Some(WorktreeTabId(Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("uuid"))),
            def_id: CustomProcessDefId::new("shell"),
            kind: CustomProcessKind::Terminal,
            label: "Shell".to_owned(),
            composed_command: "cmd /c shell".to_owned(),
        };
        let fixture = json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "parentWorktreeTabId": "22222222-2222-2222-2222-222222222222",
            "defId": "shell",
            "kind": "terminal",
            "label": "Shell",
            "composedCommand": "cmd /c shell"
        });
        (value, fixture)
    }

    fn partial_app_config_fixture() -> (PartialAppConfig, Value) {
        let value = PartialAppConfig {
            config_version: None,
            workspace_root: None,
            worktree_roots: Some(vec![PathBuf::from("/repo")]),
            worktree_prep_commands: None,
            ai_launch_commands: None,
            plugin_settings: Some(PartialPluginSettings {
                ai: BTreeMap::from([(
                    "claude".to_owned(),
                    PartialPluginSettingState {
                        enabled: Some(false),
                        settings: BTreeMap::from([(AI_LAUNCH_COMMAND_SETTING.to_owned(), PluginSettingValue::String("npx claude".to_owned()))]),
                    },
                )]),
                custom_process: BTreeMap::new(),
                dashboard_widget: BTreeMap::new(),
            }),
            last_open_sessions: None,
            tab_order: None,
            active_session_id: None,
            custom_processes: None,
            last_open_sub_sessions: None,
            worktree_tabs: None,
            worktree_tab_order: None,
            active_worktree_tab_id: None,
            sidebar_width_px: None,
            theme: None,
        };
        let fixture = json!({
            "worktreeRoots": ["/repo"],
            "pluginSettings": {
                "ai": {
                    "claude": {
                        "enabled": false,
                        "settings": {
                            "launchCommand": "npx claude"
                        }
                    }
                }
            }
        });
        (value, fixture)
    }

    #[test]
    fn session_roundtrip() {
        assert_roundtrip(&sample_session(), session_fixture());
    }

    #[test]
    fn session_ignores_legacy_instruction_set_id() {
        let mut fixture = session_fixture();
        fixture
            .as_object_mut()
            .expect("object")
            .insert("instructionSetId".to_owned(), json!("claude-default"));

        let deserialized: Session = serde_json::from_value(fixture).expect("legacy field ignored");
        assert_eq!(deserialized, sample_session());
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
        assert!(!obj.contains_key("composedCommand"), "SessionView must not expose composedCommand");
        assert!(!obj.contains_key("tempFiles"), "SessionView must not expose tempFiles");
    }

    #[test]
    fn app_config_roundtrip() {
        let (value, fixture) = app_config_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn app_config_ignores_legacy_instruction_set_fields() {
        let (mut expected, mut fixture) = app_config_fixture();
        expected.config_version = 10;
        let obj = fixture.as_object_mut().expect("object");
        obj.insert("configVersion".to_owned(), json!(10));
        obj.insert(
            "defaultInstructionSets".to_owned(),
            json!({ "claude": "claude-default", "copilot": "copilot-default" }),
        );
        obj.insert("instructionSetsDir".to_owned(), json!("/cfg/instructions"));

        let deserialized: AppConfig = serde_json::from_value(fixture).expect("legacy fields ignored");
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn ai_launch_commands_distinguishes_absent_from_explicit_null_icon_cache_entry() {
        let cmds = AiLaunchCommands {
            commands: BTreeMap::new(),
            icon_data_uris: BTreeMap::from([
                ("claude".to_owned(), None),
                ("copilot".to_owned(), Some("data:image/png;base64,AAAA".to_owned())),
            ]),
        };

        assert!(cmds.has_icon_cache_entry_for_id("claude"));
        assert!(cmds.has_icon_cache_entry_for_id("copilot"));
        assert!(!cmds.has_icon_cache_entry_for_id("cursor"));
        assert_eq!(cmds.icon_data_uri_for_id("claude"), None);
        assert_eq!(cmds.icon_data_uri_for_id("copilot"), Some("data:image/png;base64,AAAA"));
    }

    #[test]
    fn custom_process_def_roundtrip() {
        let (value, fixture) = custom_process_def_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn custom_process_def_omits_icon_when_none() {
        let value = CustomProcessDef {
            id: CustomProcessDefId::new("shell"),
            name: "Shell".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: "sh -i".to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        };
        let serialized: Value = serde_json::to_value(&value).expect("serialize");
        let obj = serialized.as_object().expect("object");
        assert!(!obj.contains_key("icon"), "icon must be elided when None");
    }

    #[test]
    fn sub_session_roundtrip() {
        let (value, fixture) = sub_session_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn sub_session_record_roundtrip() {
        let (value, fixture) = sub_session_record_fixture();
        assert_roundtrip(&value, fixture);
    }

    #[test]
    fn custom_process_kind_serializes_lowercase() {
        assert_eq!(serde_json::to_value(CustomProcessKind::Terminal).expect("v"), json!("terminal"));
        assert_eq!(serde_json::to_value(CustomProcessKind::Application).expect("v"), json!("application"));
    }

    #[test]
    fn worktree_tab_app_close_policy_serializes_lowercase() {
        assert_eq!(serde_json::to_value(WorktreeTabAppClosePolicy::Detach).expect("v"), json!("detach"));
        assert_eq!(serde_json::to_value(WorktreeTabAppClosePolicy::Terminate).expect("v"), json!("terminate"));
    }

    #[test]
    fn worktree_tab_close_args_default_app_policy_is_detach() {
        let parsed: WorktreeTabCloseArgs = serde_json::from_value(json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "deleteWorktree": true
        }))
        .expect("deserialize");
        assert_eq!(parsed.app_close_policy, WorktreeTabAppClosePolicy::Detach);
        assert!(parsed.delete_worktree);

        let explicit: WorktreeTabCloseArgs = serde_json::from_value(json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "appClosePolicy": "terminate"
        }))
        .expect("deserialize explicit terminate");
        assert_eq!(explicit.app_close_policy, WorktreeTabAppClosePolicy::Terminate);
    }

    #[test]
    fn sub_session_status_serializes_lowercase() {
        for (variant, wire) in [
            (SubSessionStatus::Starting, "starting"),
            (SubSessionStatus::Running, "running"),
            (SubSessionStatus::Exited, "exited"),
            (SubSessionStatus::Error, "error"),
        ] {
            assert_eq!(serde_json::to_value(variant).expect("v"), json!(wire));
        }
    }

    #[test]
    fn sub_session_id_is_transparent_string() {
        let id = SubSessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid"));
        assert_eq!(serde_json::to_value(id).expect("v"), json!("11111111-1111-1111-1111-111111111111"));
    }

    #[test]
    fn custom_process_def_id_is_transparent_string() {
        let id = CustomProcessDefId::new("vscode");
        assert_eq!(serde_json::to_value(&id).expect("v"), json!("vscode"));
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
        assert!(!obj.contains_key("workspaceRoot"));
        assert!(!obj.contains_key("worktreePrepCommands"));
        assert!(!obj.contains_key("lastOpenSessions"));
        assert!(!obj.contains_key("tabOrder"));
        assert!(!obj.contains_key("activeSessionId"));
        assert!(!obj.contains_key("customProcesses"));
        assert!(!obj.contains_key("lastOpenSubSessions"));
    }

    #[test]
    fn partial_app_config_workspace_root_tri_state() {
        let absent: PartialAppConfig = serde_json::from_value(json!({})).expect("absent");
        assert_eq!(absent.workspace_root, None);

        let cleared: PartialAppConfig = serde_json::from_value(json!({ "workspaceRoot": null })).expect("clear");
        assert_eq!(cleared.workspace_root, Some(None));

        let set: PartialAppConfig = serde_json::from_value(json!({ "workspaceRoot": "/repo" })).expect("set");
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
        let cleared: PartialAppConfig = serde_json::from_value(json!({ "activeSessionId": null })).expect("clear");
        assert_eq!(cleared.active_session_id, Some(None));

        // string: deserialised as `Some(Some(uuid))` → "set".
        let set: PartialAppConfig = serde_json::from_value(json!({
            "activeSessionId": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .expect("set");
        assert_eq!(
            set.active_session_id,
            Some(Some(SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"))))
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
        assert_eq!(serde_json::to_value(Tool::Claude).expect("v"), json!("claude"));
        assert_eq!(serde_json::to_value(Tool::Copilot).expect("v"), json!("copilot"));
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
        assert_eq!(serde_json::to_value(id).expect("v"), json!("550e8400-e29b-41d4-a716-446655440000"));
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
        assert_eq!(Error::WorktreeMissing(std::path::PathBuf::from("/x")).code(), "WorktreeMissing");
        assert_eq!(Error::NotFound("p".into()).code(), "NotFound");
        assert_eq!(Error::Io(std::io::Error::other("e")).code(), "Io");
        assert_eq!(Error::Internal("e".into()).code(), "Internal");
        assert_eq!(Error::PtySpawnFailed("e".into()).code(), "PtySpawnFailed");
        assert_eq!(Error::PtyWriteFailed("e".into()).code(), "PtyWriteFailed");
        assert_eq!(Error::PtyResizeFailed("e".into()).code(), "PtyResizeFailed");
        assert_eq!(Error::PtyKillFailed("e".into()).code(), "PtyKillFailed");
        assert_eq!(Error::InvalidCustomProcessDef("x".into()).code(), "InvalidCustomProcessDef");
        assert_eq!(Error::InvalidPluginSettings("x".into()).code(), "InvalidPluginSettings");
        assert_eq!(Error::ToolMissing("wmctrl".into()).code(), "ToolMissing");
        assert_eq!(Error::NotApplicable("no PTY".into()).code(), "NotApplicable");
        assert_eq!(Error::PermissionDenied("Accessibility".into()).code(), "PermissionDenied");
        assert_eq!(Error::Unsupported("Wayland".into()).code(), "Unsupported");
        assert_eq!(Error::AppSpawnFailed("e".into()).code(), "AppSpawnFailed");
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
            session_id: SessionId(Uuid::parse_str("8a3e1c5e-2b41-4b31-9dc7-1d77a3a51f00").expect("uuid")),
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
            session_id: SessionId(Uuid::parse_str("8a3e1c5e-2b41-4b31-9dc7-1d77a3a51f00").expect("uuid")),
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
