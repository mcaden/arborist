//! AI plugin trait + built-in dispatch helpers.
//!
//! Issue #96 keeps `Tool` as the persisted serde discriminator but routes
//! tool-specific behavior through this module so callsites outside
//! `plugins/ai/*` do not branch on `Tool::{Claude,Copilot}` directly.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::plugins::Plugin;
use crate::types::Tool;
use crate::types::{SessionId, TempFileSpec};

pub mod claude;
pub mod copilot;

/// AI plugin trait. See module-level docs for the full implementor contract. Sub-issue #96 will expand this with `compose(...)`, `env(...)`, and
/// `spawn_metrics_watcher(...)` methods as the existing per-tool code is migrated; the v1 shape keeps only the fields needed for the registry to
/// surface an AI plugin and resolve its default program / instruction set.
pub trait AiPlugin: Plugin {
    /// Bare program token used when composing the launch command. The user may override this via `AppConfig.ai_launch_commands` (per-plugin map);
    /// callers that want the **effective** program string must consult the config first.
    fn default_program(&self) -> &'static str;

    /// Filename of the built-in instruction-set markdown under the `instructions/` directory (e.g. `"claude-default.md"`). Used by the host to seed
    /// `AppConfig.default_instruction_sets` when the user has not selected anything.
    fn default_instruction_set_path(&self) -> &'static str;
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

/// Spawn-prep side effects run right before PTY spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnPrep {
    pub ensure_temp_dir: bool,
    pub stale_files: Vec<PathBuf>,
}

/// Built-in AI plugin descriptor used by dispatch sites that need both the
/// persisted [`Tool`] discriminator and its plugin metadata.
pub struct BuiltinAi {
    pub tool: Tool,
    pub plugin: &'static dyn AiPlugin,
}

/// Built-in AI plugins in stable registration order.
pub const BUILTIN_AI: [BuiltinAi; 2] = [
    BuiltinAi {
        tool: Tool::Claude,
        plugin: &claude::PLUGIN,
    },
    BuiltinAi {
        tool: Tool::Copilot,
        plugin: &copilot::PLUGIN,
    },
];

/// Resolve a built-in AI plugin for a persisted [`Tool`].
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
    match tool {
        Tool::Claude => crate::compose::build_claude(inputs, quoter),
        Tool::Copilot => crate::compose::build_copilot(inputs, quoter),
    }
}

/// Extra environment variables injected for the selected tool.
#[must_use]
pub fn env(tool: Tool, session_id: &SessionId) -> Vec<(String, std::ffi::OsString)> {
    match tool {
        Tool::Claude => Vec::new(),
        Tool::Copilot => {
            let path = crate::compose::copilot_otel_path(session_id);
            vec![
                ("COPILOT_OTEL_FILE_EXPORTER_PATH".to_owned(), path.into_os_string()),
                ("COPILOT_OTEL_ENABLED".to_owned(), "true".into()),
                ("OTEL_BSP_SCHEDULE_DELAY".to_owned(), "1000".into()),
            ]
        }
    }
}

/// Spawn prep behavior per tool.
#[must_use]
pub fn spawn_prep(tool: Tool, session_id: &SessionId) -> SpawnPrep {
    match tool {
        Tool::Claude => SpawnPrep::default(),
        Tool::Copilot => SpawnPrep {
            ensure_temp_dir: true,
            stale_files: vec![crate::compose::copilot_otel_path(session_id)],
        },
    }
}

/// Resolve which metrics watcher implementation to run for `tool`.
#[must_use]
pub fn metrics_watcher_kind(tool: Tool, session_id: SessionId, cwd: &Path) -> Option<MetricsWatcherKind> {
    match tool {
        Tool::Claude => crate::session_metrics::home_dir().map(|home| MetricsWatcherKind::Claude {
            home,
            cwd: cwd.to_path_buf(),
        }),
        Tool::Copilot => Some(MetricsWatcherKind::Copilot {
            otel_path: crate::compose::copilot_otel_path(&session_id),
        }),
    }
}

/// Whether the Copilot events tailer should be armed for this tool.
#[must_use]
pub const fn starts_activity_events_watcher(tool: Tool) -> bool {
    matches!(tool, Tool::Copilot)
}

/// Whether create-time spawn should preallocate an AI session id.
#[must_use]
pub fn create_ai_session_id(tool: Tool) -> Option<String> {
    matches!(tool, Tool::Copilot).then(|| Uuid::new_v4().to_string())
}

/// Restart-time AI-session-id policy for this tool.
#[must_use]
pub const fn restart_ai_session_policy(tool: Tool) -> RestartAiSessionPolicy {
    match tool {
        Tool::Claude => RestartAiSessionPolicy::Clear,
        Tool::Copilot => RestartAiSessionPolicy::RotateUuid,
    }
}

/// Resume preflight policy:
/// - Claude: require transcript/session state path to exist before `--resume`.
/// - Copilot: allow `--resume` unconditionally (CLI creates missing sessions).
#[must_use]
pub const fn resume_requires_preflight(tool: Tool) -> bool {
    matches!(tool, Tool::Claude)
}

/// Resolve the expected transcript/session-state path for `ai_session_id`.
#[must_use]
pub fn ai_session_transcript_path(tool: Tool, home: &Path, worktree_path: &Path, ai_session_id: &str) -> PathBuf {
    match tool {
        Tool::Claude => home
            .join(".claude")
            .join("projects")
            .join(crate::session_metrics::encode_cwd(worktree_path))
            .join(format!("{ai_session_id}.jsonl")),
        Tool::Copilot => home.join(".copilot").join("session-state").join(ai_session_id),
    }
}

/// Prefix used when discovering instruction-set files for a specific tool.
#[must_use]
pub const fn instruction_stem_prefix(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => claude::INSTRUCTION_STEM_PREFIX,
        Tool::Copilot => copilot::INSTRUCTION_STEM_PREFIX,
    }
}
