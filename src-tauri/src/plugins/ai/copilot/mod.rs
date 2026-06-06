//! Copilot AI plugin metadata.
//!
//! Issue #96 migrates Copilot-specific behavior behind plugin dispatch. This
//! file defines the stable plugin identity and core metadata consumed across
//! composition and settings paths.

use crate::plugins::ai::AiPlugin;
use crate::plugins::Plugin;
use crate::types::SessionId;
use crate::types::Tool;

pub mod metrics;

/// Stable singleton instance for dispatch sites that need a `'static`
/// [`AiPlugin`] reference without allocating.
pub static PLUGIN: CopilotPlugin = CopilotPlugin;

pub struct CopilotPlugin;

impl Plugin for CopilotPlugin {
    fn id(&self) -> &'static str {
        Tool::Copilot.as_id()
    }

    fn display_name(&self) -> &'static str {
        "GitHub Copilot"
    }
}

impl AiPlugin for CopilotPlugin {
    fn default_program(&self) -> &'static str {
        "copilot"
    }

    fn compose(&self, inputs: &crate::compose::ComposeInputs<'_>, quoter: crate::compose::Quoter) -> (String, Vec<crate::types::TempFileSpec>) {
        crate::compose::build_copilot(inputs, quoter)
    }

    fn env(&self, session_id: &SessionId) -> Vec<(String, std::ffi::OsString)> {
        let path = crate::compose::copilot_otel_path(session_id);
        vec![
            ("COPILOT_OTEL_FILE_EXPORTER_PATH".to_owned(), path.into_os_string()),
            ("COPILOT_OTEL_ENABLED".to_owned(), "true".into()),
            ("OTEL_BSP_SCHEDULE_DELAY".to_owned(), "1000".into()),
        ]
    }

    fn spawn_prep(&self, _session_id: &SessionId) -> crate::plugins::ai::SpawnPrep {
        crate::plugins::ai::SpawnPrep {
            ensure_temp_dir: false,
            reset_files: vec![crate::plugins::ai::SpawnPrepFile::CopilotOtel],
        }
    }

    fn metrics_parser(
        &self,
        session_id: SessionId,
        _cwd: &std::path::Path,
        _spawn_instant: std::time::SystemTime,
    ) -> Option<Box<dyn crate::session_metrics::MetricsParser>> {
        Some(Box::new(metrics::CopilotMetricsParser::new(crate::compose::copilot_otel_path(
            &session_id,
        ))))
    }

    fn starts_activity_events_watcher(&self) -> bool {
        true
    }

    fn activity_events_kind(
        &self,
        _session_id: SessionId,
        home: Option<&std::path::Path>,
        ai_session_id: Option<&str>,
    ) -> Option<crate::plugins::ai::ActivityEventsKind> {
        let home = home?;
        let aid = ai_session_id?;
        Some(crate::plugins::ai::ActivityEventsKind::CopilotEventsJsonl(
            crate::copilot_events::events_path(home, aid),
        ))
    }

    fn create_ai_session_id(&self) -> Option<String> {
        Some(uuid::Uuid::new_v4().to_string())
    }

    fn restart_ai_session_policy(&self) -> crate::plugins::ai::RestartAiSessionPolicy {
        // Preserve the pre-allocated uuid across restart and let Copilot re-bind to it via `--session-id <uuid>` (Copilot's `--session-id` is
        // documented to "resume known sessions … and starts new sessions with a specific UUID"). Was `RotateUuid` while we relied on the legacy
        // `--resume <unknown-uuid>` create-if-absent behaviour; copilot-cli >= 1.0.51 removed that quirk and added the dedicated `--session-id`
        // flag, which makes preserve-then-rebind identical to the prior rotate-and-rebind without churning the persisted id on every restart.
        // Net user-visible change: a Copilot restart now *continues* the same conversation (matching Claude's restart semantics) instead of
        // starting fresh — far more useful in practice for "the CLI got stuck, bounce it". Users who want a clean conversation use the CLI's own
        // `/clear` (or kill+create a new session).
        crate::plugins::ai::RestartAiSessionPolicy::Preserve
    }

    fn resume_requires_preflight(&self) -> bool {
        false
    }

    fn resume_args(&self, ai_session_id: &str) -> Vec<String> {
        // copilot-cli >= 1.0.51 split `--resume` (strictly resume *known* sessions; errors on unknown UUIDs) from `--session-id` (resume known or
        // create-at-UUID — the create-if-absent behaviour Arborist needs at first launch, restart, and restore-on-launch). All of Arborist's
        // Copilot spawn sites route through this method, so emitting `--session-id` here covers create, restart, and restore. Upstream change:
        // https://github.com/github/copilot-cli/issues/3377.
        vec!["--session-id".to_owned(), ai_session_id.to_owned()]
    }

    fn ai_session_transcript_path(&self, home: &std::path::Path, _worktree_path: &std::path::Path, ai_session_id: &str) -> std::path::PathBuf {
        home.join(".copilot").join("session-state").join(ai_session_id)
    }
}
