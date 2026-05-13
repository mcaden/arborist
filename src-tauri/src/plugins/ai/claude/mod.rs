//! Claude AI plugin metadata.
//!
//! Issue #96 migrates Claude-specific behavior behind plugin dispatch. This file
//! defines the stable plugin identity and core metadata consumed across
//! composition, settings, and instruction discovery paths.

use crate::plugins::ai::AiPlugin;
use crate::plugins::Plugin;
use crate::types::SessionId;
use crate::types::Tool;

/// Stable singleton instance for dispatch sites that need a `'static`
/// [`AiPlugin`] reference without allocating.
pub static PLUGIN: ClaudePlugin = ClaudePlugin;

pub struct ClaudePlugin;

impl Plugin for ClaudePlugin {
    fn id(&self) -> &'static str {
        Tool::Claude.as_id()
    }

    fn display_name(&self) -> &'static str {
        "Claude"
    }
}

impl AiPlugin for ClaudePlugin {
    fn default_program(&self) -> &'static str {
        "claude"
    }

    fn default_instruction_set_path(&self) -> &'static str {
        "claude-default.md"
    }

    fn compose(&self, inputs: &crate::compose::ComposeInputs<'_>, quoter: crate::compose::Quoter) -> (String, Vec<crate::types::TempFileSpec>) {
        crate::compose::build_claude(inputs, quoter)
    }

    fn env(&self, _session_id: &SessionId) -> Vec<(String, std::ffi::OsString)> {
        Vec::new()
    }

    fn spawn_prep(&self, _session_id: &SessionId) -> crate::plugins::ai::SpawnPrep {
        crate::plugins::ai::SpawnPrep::default()
    }

    fn metrics_watcher_kind(&self, _session_id: SessionId, cwd: &std::path::Path) -> Option<crate::plugins::ai::MetricsWatcherKind> {
        crate::session_metrics::home_dir().map(|home| crate::plugins::ai::MetricsWatcherKind::Claude {
            home,
            cwd: cwd.to_path_buf(),
        })
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

    fn ai_session_transcript_path(&self, home: &std::path::Path, worktree_path: &std::path::Path, ai_session_id: &str) -> std::path::PathBuf {
        home.join(".claude")
            .join("projects")
            .join(crate::session_metrics::encode_cwd(worktree_path))
            .join(format!("{ai_session_id}.jsonl"))
    }

    fn instruction_stem_prefix(&self) -> &'static str {
        INSTRUCTION_STEM_PREFIX
    }
}

/// Filename-stem prefix for Claude instruction sets (`claude-*.md`).
pub const INSTRUCTION_STEM_PREFIX: &str = "claude-";
