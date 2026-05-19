//! Copilot AI plugin metadata.
//!
//! Issue #96 migrates Copilot-specific behavior behind plugin dispatch. This
//! file defines the stable plugin identity and core metadata consumed across
//! composition and settings paths.

use crate::plugins::ai::AiPlugin;
use crate::plugins::Plugin;
use crate::types::SessionId;
use crate::types::Tool;

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

    fn metrics_watcher_kind(&self, session_id: SessionId, _cwd: &std::path::Path) -> Option<crate::plugins::ai::MetricsWatcherKind> {
        Some(crate::plugins::ai::MetricsWatcherKind::Copilot {
            otel_path: crate::compose::copilot_otel_path(&session_id),
        })
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
        crate::plugins::ai::RestartAiSessionPolicy::RotateUuid
    }

    fn resume_requires_preflight(&self) -> bool {
        false
    }

    fn resume_args(&self, ai_session_id: &str) -> Vec<String> {
        vec!["--resume".to_owned(), ai_session_id.to_owned()]
    }

    fn ai_session_transcript_path(&self, home: &std::path::Path, _worktree_path: &std::path::Path, ai_session_id: &str) -> std::path::PathBuf {
        home.join(".copilot").join("session-state").join(ai_session_id)
    }
}
