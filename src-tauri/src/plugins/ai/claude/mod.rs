//! Claude AI plugin metadata.
//!
//! Issue #96 migrates Claude-specific behavior behind plugin dispatch. This file
//! defines the stable plugin identity and core metadata consumed across
//! composition, settings, and instruction discovery paths.

use crate::plugins::ai::AiPlugin;
use crate::plugins::Plugin;
use crate::types::SessionId;
use crate::types::Tool;

pub mod hooks;

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

    fn spawn_prep(&self, session_id: &SessionId) -> crate::plugins::ai::SpawnPrep {
        // We materialise per-session temp files (`system-prompt.md`, `claude-settings.json`) under `<session_temp_dir>/`, so the directory must
        // exist. Also wipe any stale `hook-events.jsonl` from a prior run of the same session id so the tailer starts from a clean state — same
        // pattern Copilot uses for its OTel JSONL.
        crate::plugins::ai::SpawnPrep {
            ensure_temp_dir: true,
            stale_files: vec![crate::claude_hook_events::hook_events_path(session_id)],
        }
    }

    fn metrics_watcher_kind(&self, _session_id: SessionId, cwd: &std::path::Path) -> Option<crate::plugins::ai::MetricsWatcherKind> {
        crate::session_metrics::home_dir().map(|home| crate::plugins::ai::MetricsWatcherKind::Claude {
            home,
            cwd: cwd.to_path_buf(),
        })
    }

    fn starts_activity_events_watcher(&self) -> bool {
        true
    }

    fn activity_events_kind(
        &self,
        session_id: SessionId,
        _home: Option<&std::path::Path>,
        _ai_session_id: Option<&str>,
    ) -> Option<crate::plugins::ai::ActivityEventsKind> {
        // Claude's hook tailer reads a per-session file under the session temp dir — no dependency on the home dir or the AI session id, so we
        // produce a kind unconditionally. The hook helper materialises the file on the first fire even if the user never types a message.
        Some(crate::plugins::ai::ActivityEventsKind::ClaudeHookEventsJsonl(
            crate::claude_hook_events::hook_events_path(&session_id),
        ))
    }

    fn settings_file_path(&self, session_id: &SessionId) -> Option<std::path::PathBuf> {
        Some(crate::compose::session_temp_dir(session_id).join("claude-settings.json"))
    }

    fn create_ai_session_id(&self) -> Option<String> {
        // Pre-allocate at create time so `--session-id <uuid>` can splice on first spawn and `--resume <uuid>` can splice on every subsequent spawn.
        // The deterministic id also makes the transcript path predictable from spawn-instant 0 (matches Copilot's model).
        Some(uuid::Uuid::new_v4().to_string())
    }

    fn restart_ai_session_policy(&self) -> crate::plugins::ai::RestartAiSessionPolicy {
        // Preserve the existing id so restart re-attaches to the same Claude transcript via `--resume <uuid>`. Was `Clear` before pre-allocation
        // landed — when the id was discovered after the user's first message rather than created upfront, clearing was the only safe option.
        crate::plugins::ai::RestartAiSessionPolicy::Preserve
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
