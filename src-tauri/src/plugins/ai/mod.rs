//! AI plugin trait + built-in dispatch helpers.
//!
//! Issue #96 keeps `Tool` as the persisted serde discriminator but routes
//! tool-specific behavior through this module so callsites outside
//! `plugins/ai/*` do not branch on `Tool::{Claude,Copilot}` directly.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::plugins::Plugin;
use crate::types::Tool;
use crate::types::{SessionId, TempFileSpec};

pub mod claude;
pub mod copilot;

/// AI plugin trait. See module-level docs for the full implementor contract.
///
/// Every per-tool behaviour surface used by session creation/spawn/restart/restore
/// lives here so call sites can dispatch through the registered plugin instead of
/// branching on [`Tool`].
pub trait AiPlugin: Plugin {
    /// Bare program token used when composing the launch command. The user may override this via `AppConfig.ai_launch_commands` (per-plugin map);
    /// callers that want the **effective** program string must consult the config first.
    fn default_program(&self) -> &'static str;

    /// Compose the launch command + any temp files for this tool.
    fn compose(&self, inputs: &crate::compose::ComposeInputs<'_>, quoter: crate::compose::Quoter) -> (String, Vec<TempFileSpec>);

    /// Extra environment variables injected into the spawned process.
    fn env(&self, session_id: &SessionId) -> Vec<(String, OsString)>;

    /// Spawn-prep side effects run right before PTY spawn.
    fn spawn_prep(&self, session_id: &SessionId) -> SpawnPrep;

    /// Resolve which metrics watcher implementation to run.
    fn metrics_watcher_kind(&self, session_id: SessionId, cwd: &Path) -> Option<MetricsWatcherKind>;

    /// Whether this tool should also arm the activity-events watcher (the events.jsonl tailer for Copilot, or the hook-events.jsonl tailer for
    /// Claude). When `true`, the host calls [`Self::activity_events_kind`] to discover which tailer flavour to spawn.
    fn starts_activity_events_watcher(&self) -> bool;

    /// Path the activity-events watcher should tail, expressed as the tailer flavour. Returns `None` when no tailer applies for this tool (or when
    /// the prerequisite inputs — `ai_session_id`, `home` — are missing). Used by [`crate::session_metrics::MetricsRegistry::start`] to spawn the
    /// appropriate per-tool watcher thread.
    fn activity_events_kind(&self, session_id: SessionId, home: Option<&Path>, ai_session_id: Option<&str>) -> Option<ActivityEventsKind> {
        // Default: no tailer. Implementations that want one override.
        let _ = (session_id, home, ai_session_id);
        None
    }

    /// Per-session settings file path written before spawn (when applicable). Returned path is materialised by the host through
    /// [`crate::types::TempFileSpec`] (the contents come from the plugin's [`Self::compose`]). For tools that do not need a per-session settings
    /// file (Copilot) this returns `None`.
    fn settings_file_path(&self, session_id: &SessionId) -> Option<PathBuf> {
        let _ = session_id;
        None
    }

    /// Whether create-time spawn should preallocate an AI session id.
    fn create_ai_session_id(&self) -> Option<String>;

    /// Restart-time AI-session-id policy for this tool.
    fn restart_ai_session_policy(&self) -> RestartAiSessionPolicy;

    /// Whether restore-time `--resume` should verify transcript/session-state first.
    fn resume_requires_preflight(&self) -> bool;

    /// Resolve the expected transcript/session-state path for `ai_session_id`.
    fn ai_session_transcript_path(&self, home: &Path, worktree_path: &Path, ai_session_id: &str) -> PathBuf;
}

/// Tool-specific restart behavior for persisted `Session.ai_session_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAiSessionPolicy {
    /// Keep the existing id untouched.
    Preserve,
    /// Clear persisted id before restart and rediscover later.
    Clear,
    /// Allocate a fresh UUID and persist it after successful restart.
    RotateUuid,
}

/// Which metrics watcher implementation to run for a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsWatcherKind {
    Claude { home: PathBuf, cwd: PathBuf },
    Copilot { otel_path: PathBuf },
}

/// Which activity-events tailer flavour to spawn for a tool, with its resolved on-disk path.
///
/// Both flavours implement the same shape (per-session JSONL append-only file, polled at the same cadence, emitting [`crate::types::ActivityEvent`]s
/// through the shared `session://activity` channel). The variant simply selects which parser the watcher thread runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityEventsKind {
    /// Copilot CLI's structured event stream at `~/.copilot/session-state/<ai_session_id>/events.jsonl`.
    CopilotEventsJsonl(PathBuf),
    /// Arborist's Claude hook-events file at `<session_temp_dir>/hook-events.jsonl`, populated by the `arborist-claude-hook` helper binary.
    ClaudeHookEventsJsonl(PathBuf),
}

/// Spawn-prep side effects run right before PTY spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnPrep {
    pub ensure_temp_dir: bool,
    pub reset_files: Vec<SpawnPrepFile>,
}

/// Typed session temp files that may be reset during spawn prep. Reset handlers are responsible for creating their own parent directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnPrepFile {
    CopilotOtel,
    /// Claude's per-session `hook-events.jsonl` — the JSONL stream the `arborist-claude-hook` helper appends to and the
    /// [`crate::claude_hook_events`] tailer reads. Reset removes any leftover file from a prior spawn so the tailer starts clean (matters for
    /// restart, which re-uses the same session id).
    ClaudeHookEvents,
}

/// Built-in AI plugin descriptor used by dispatch sites that need both the
/// persisted [`Tool`] discriminator and its plugin metadata.
#[derive(Clone, Copy)]
pub struct BuiltinAi {
    pub tool: Tool,
    pub plugin: &'static dyn AiPlugin,
    pub factory: fn() -> Arc<dyn AiPlugin>,
}

fn claude_factory() -> Arc<dyn AiPlugin> {
    Arc::new(claude::ClaudePlugin)
}

fn copilot_factory() -> Arc<dyn AiPlugin> {
    Arc::new(copilot::CopilotPlugin)
}

/// Built-in AI plugins in stable registration order.
pub const BUILTIN_AI: [BuiltinAi; 2] = [
    BuiltinAi {
        tool: Tool::Claude,
        plugin: &claude::PLUGIN,
        factory: claude_factory,
    },
    BuiltinAi {
        tool: Tool::Copilot,
        plugin: &copilot::PLUGIN,
        factory: copilot_factory,
    },
];

/// Resolve a built-in AI plugin for a persisted [`Tool`].
///
/// This is the intentional serde-glue seam: the exhaustive match keeps the compiler checking that every persisted `Tool` variant has a plugin.
/// Call sites outside `plugins/ai/*` should use the wrapper helpers below instead of branching on `Tool` directly.
#[must_use]
pub fn plugin_for_tool(tool: Tool) -> &'static dyn AiPlugin {
    match tool {
        Tool::Claude => &claude::PLUGIN,
        Tool::Copilot => &copilot::PLUGIN,
    }
}

/// Resolve a built-in AI plugin by registry id (`"claude"`, `"copilot"`).
#[must_use]
pub fn plugin_for_id(id: &str) -> Option<&'static dyn AiPlugin> {
    BUILTIN_AI.iter().find(|p| p.plugin.id() == id).map(|p| p.plugin)
}

/// Compose the launch command + temp files for the selected AI tool.
#[must_use]
pub fn compose(tool: Tool, inputs: &crate::compose::ComposeInputs<'_>, quoter: crate::compose::Quoter) -> (String, Vec<TempFileSpec>) {
    plugin_for_tool(tool).compose(inputs, quoter)
}

/// Extra environment variables injected for the selected tool.
#[must_use]
pub fn env(tool: Tool, session_id: &SessionId) -> Vec<(String, OsString)> {
    plugin_for_tool(tool).env(session_id)
}

/// Spawn prep behavior per tool.
#[must_use]
pub fn spawn_prep(tool: Tool, session_id: &SessionId) -> SpawnPrep {
    plugin_for_tool(tool).spawn_prep(session_id)
}

/// Resolve which metrics watcher implementation to run for `tool`.
#[must_use]
pub fn metrics_watcher_kind(tool: Tool, session_id: SessionId, cwd: &Path) -> Option<MetricsWatcherKind> {
    plugin_for_tool(tool).metrics_watcher_kind(session_id, cwd)
}

/// Whether the activity-events watcher should be armed for this tool.
#[must_use]
pub fn starts_activity_events_watcher(tool: Tool) -> bool {
    plugin_for_tool(tool).starts_activity_events_watcher()
}

/// Resolve the activity-events tailer flavour + path for `tool`. Returns `None` when no tailer applies or when prerequisites are missing.
#[must_use]
pub fn activity_events_kind(tool: Tool, session_id: SessionId, home: Option<&Path>, ai_session_id: Option<&str>) -> Option<ActivityEventsKind> {
    plugin_for_tool(tool).activity_events_kind(session_id, home, ai_session_id)
}

/// Per-session settings file path for `tool`, or `None` if the tool does not use one.
#[must_use]
pub fn settings_file_path(tool: Tool, session_id: &SessionId) -> Option<PathBuf> {
    plugin_for_tool(tool).settings_file_path(session_id)
}

/// Whether create-time spawn should preallocate an AI session id.
#[must_use]
pub fn create_ai_session_id(tool: Tool) -> Option<String> {
    plugin_for_tool(tool).create_ai_session_id()
}

/// Restart-time AI-session-id policy for this tool.
#[must_use]
pub fn restart_ai_session_policy(tool: Tool) -> RestartAiSessionPolicy {
    plugin_for_tool(tool).restart_ai_session_policy()
}

/// Resume preflight policy:
/// - Claude: require transcript/session state path to exist before `--resume`.
/// - Copilot: allow `--resume` unconditionally (CLI creates missing sessions).
#[must_use]
pub fn resume_requires_preflight(tool: Tool) -> bool {
    plugin_for_tool(tool).resume_requires_preflight()
}

/// Resolve the expected transcript/session-state path for `ai_session_id`.
#[must_use]
pub fn ai_session_transcript_path(tool: Tool, home: &Path, worktree_path: &Path, ai_session_id: &str) -> PathBuf {
    plugin_for_tool(tool).ai_session_transcript_path(home, worktree_path, ai_session_id)
}
