//! Codex AI plugin metadata.
//!
//! OpenAI Codex CLI (`codex`) is an interactive coding agent that runs locally.
//! It starts in TUI mode by default and auto-discovers instructions from `cwd`.
//! Resume syntax differs from Claude/Copilot: `codex resume <session_id>` (a
//! subcommand, not a `--resume` flag).
//!
//! The metrics watcher implementation lives in
//! [`crate::session_metrics`] (`run_codex_watcher`); that module documents the
//! rollout-file layout and the event types it consumes.

use crate::plugins::ai::AiPlugin;
use crate::plugins::Plugin;
use crate::types::SessionId;
use crate::types::Tool;

/// Stable singleton instance for dispatch sites that need a `'static`
/// [`AiPlugin`] reference without allocating.
pub static PLUGIN: CodexPlugin = CodexPlugin;

pub struct CodexPlugin;

impl Plugin for CodexPlugin {
    fn id(&self) -> &'static str {
        Tool::Codex.as_id()
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }
}

impl AiPlugin for CodexPlugin {
    fn default_program(&self) -> &'static str {
        "codex"
    }

    fn compose(&self, inputs: &crate::compose::ComposeInputs<'_>, quoter: crate::compose::Quoter) -> (String, Vec<crate::types::TempFileSpec>) {
        crate::compose::build_codex(inputs, quoter)
    }

    fn env(&self, _session_id: &SessionId) -> Vec<(String, std::ffi::OsString)> {
        Vec::new()
    }

    fn spawn_prep(&self, _session_id: &SessionId) -> crate::plugins::ai::SpawnPrep {
        crate::plugins::ai::SpawnPrep::default()
    }

    fn metrics_watcher_kind(&self, _session_id: SessionId, cwd: &std::path::Path) -> Option<crate::plugins::ai::MetricsWatcherKind> {
        crate::session_metrics::home_dir().map(|home| crate::plugins::ai::MetricsWatcherKind::Codex {
            home,
            cwd: cwd.to_path_buf(),
        })
    }

    fn starts_activity_events_watcher(&self) -> bool {
        false
    }

    fn create_ai_session_id(&self) -> Option<String> {
        // Codex manages its own thread IDs internally. The watcher discovers
        // the thread id from the rollout file's SessionMeta line.
        None
    }

    fn restart_ai_session_policy(&self) -> crate::plugins::ai::RestartAiSessionPolicy {
        crate::plugins::ai::RestartAiSessionPolicy::Clear
    }

    fn resume_requires_preflight(&self) -> bool {
        // Codex's `resume <thread_id>` handles missing threads gracefully (prints its own error).
        // Rollout files use `rollout-<timestamp>-<uuid>.jsonl` naming in date-nested dirs, with
        // thread_id living in the first-line `session_meta` payload, so a cheap path probe by id
        // is not reliable. Let the CLI validate.
        false
    }

    fn ai_session_transcript_path(&self, home: &std::path::Path, _worktree_path: &std::path::Path, ai_session_id: &str) -> std::path::PathBuf {
        // `resume_requires_preflight()` is false for Codex, so this is currently not used to gate
        // restore. Keep a stable placeholder path to satisfy the trait contract; if Codex preflight
        // is ever enabled, this needs a real rollout-file lookup strategy.
        home.join(".codex").join("sessions").join(ai_session_id)
    }
}
