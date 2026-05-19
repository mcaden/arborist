//! Codex AI plugin metadata.
//!
//! OpenAI Codex CLI (`codex`) is an interactive coding agent that runs locally.
//! It starts in TUI mode by default and auto-discovers instructions from `cwd`.
//! Resume syntax differs from Claude/Copilot: `codex resume <session_id>` (a
//! subcommand, not a `--resume` flag).

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

    fn metrics_watcher_kind(&self, _session_id: SessionId, _cwd: &std::path::Path) -> Option<crate::plugins::ai::MetricsWatcherKind> {
        None
    }

    fn starts_activity_events_watcher(&self) -> bool {
        false
    }

    fn create_ai_session_id(&self) -> Option<String> {
        None
    }

    fn restart_ai_session_policy(&self) -> crate::plugins::ai::RestartAiSessionPolicy {
        crate::plugins::ai::RestartAiSessionPolicy::Clear
    }

    fn resume_requires_preflight(&self) -> bool {
        true
    }

    fn ai_session_transcript_path(&self, home: &std::path::Path, _worktree_path: &std::path::Path, ai_session_id: &str) -> std::path::PathBuf {
        home.join(".codex").join("sessions").join(ai_session_id)
    }
}
