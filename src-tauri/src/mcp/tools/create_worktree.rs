//! `create_worktree` MCP tool.
//!
//! Per `dev/ai/02-create-worktree.md`. This is the highest-risk MCP mutation because it can
//! create on-disk worktrees, run prep commands, and launch sibling AI sessions.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::commands::{self, session};
use crate::compose::{self, ComposeInputs};
use crate::git::{self, GitRunner};
use crate::mcp::audit::{AuditEntryInput, AuditLog};
use crate::mcp::confirm::{fingerprint_args, ConsumeError, PendingMcpActionRegistry};
use crate::mcp::error::McpInternalError;
use crate::mcp::ipc::McpSessionRegistry;
use crate::mcp::trust::TrustedRequestStore;
use crate::mcp::types::{AppConfigMcp, McpAuditDecision, McpConfirmationMode, McpToolDescriptor, McpToolName};
use crate::repo_settings::{RepoSettings, WORKTREES_REL};
use crate::shell_trust::{self, RepoCommandCandidate};
use crate::types::{
    AppConfig, AppError, SessionCreateArgs, SessionId, SessionInputArgs, ShellCommandKind, ShellCommandPreview, Tool, WorktreePrepId,
};

const MAX_NAME_CHARS: usize = 64;
const MAX_PROMPT_BYTES: usize = 4 * 1024;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const MAX_CHILDREN_PER_PARENT: usize = 8;
const MAX_LINEAGE_DEPTH: usize = 2;
const CONFIRMATION_TTL_SECS: u64 = 60;

static MCP_SPAWN_PARENTS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateWorktreeArgs {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    optional_prompt: Option<String>,
    #[serde(default)]
    prep_from_config: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spawn_sibling: Option<SpawnSiblingArgs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirmation_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpawnSiblingArgs {
    tool: Tool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initial_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateWorktreeSuccess {
    worktree: WorktreePayload,
    prep: Option<PrepPayload>,
    spawned_session: Option<SpawnedSessionPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreePayload {
    absolute_path: PathBuf,
    relative_path: PathBuf,
    branch: String,
    head: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrepPayload {
    id: String,
    log_relative_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpawnedSessionPayload {
    session_id: String,
    tool: Tool,
}

impl CreateWorktreeSuccess {
    fn to_json(&self) -> Value {
        json!({
            "worktree": {
                "absolutePath": path_string(&self.worktree.absolute_path),
                "relativePath": path_string(&self.worktree.relative_path),
                "branch": self.worktree.branch.clone(),
                "head": self.worktree.head.clone(),
            },
            "prep": self.prep.as_ref().map(|prep| {
                json!({
                    "id": prep.id.clone(),
                    "logRelativePath": prep.log_relative_path.clone(),
                })
            }),
            "spawnedSession": self.spawned_session.as_ref().map(|spawned| {
                json!({
                    "sessionId": spawned.session_id.clone(),
                    "tool": spawned.tool.as_id(),
                })
            }),
        })
    }
}

#[derive(Debug, Clone)]
enum CoreError {
    Public(McpInternalError),
    NameInUse { message: String, suggested_alternatives: Vec<String> },
    ConfirmationRequired { message: String, pending_id: String, summary: String },
    RepoCommandTrustRequired { message: String, preview: ShellCommandPreview },
    PrepNotConfigured { message: String },
    PrepFailed { message: String, log_relative_path: String },
}

impl CoreError {
    fn public_error(self) -> McpInternalError {
        match self {
            Self::Public(err) => err,
            Self::NameInUse {
                message,
                suggested_alternatives,
            } => {
                let mut full = message;
                if !suggested_alternatives.is_empty() {
                    full.push_str(". Suggested alternatives: ");
                    full.push_str(&suggested_alternatives.join(", "));
                }
                McpInternalError::NameInUse { message: full }
            }
            Self::ConfirmationRequired {
                message,
                pending_id,
                summary,
            } => McpInternalError::ConfirmationRequired {
                message: format!("{message} (pendingId={pending_id}, expiresInSecs={CONFIRMATION_TTL_SECS}, summary={summary})"),
            },
            Self::RepoCommandTrustRequired { message, preview } => {
                let _ = preview;
                McpInternalError::RepoCommandTrustRequired { message }
            }
            Self::PrepNotConfigured { message } => McpInternalError::InvalidArg { message },
            Self::PrepFailed { message, log_relative_path } => {
                let _ = log_relative_path;
                McpInternalError::InvalidArg { message }
            }
        }
    }

    #[cfg(test)]
    fn code(&self) -> &'static str {
        match self {
            Self::Public(McpInternalError::WorkspaceUnbound { .. }) => "workspace-unbound",
            Self::Public(McpInternalError::InvalidName { .. }) => "invalid-name",
            Self::Public(McpInternalError::InvalidArg { .. }) => "invalid-arg",
            Self::Public(McpInternalError::InvalidConfirmation { .. }) => "invalid-confirmation",
            Self::Public(McpInternalError::ConfirmationExpired { .. }) => "confirmation-expired",
            Self::Public(McpInternalError::ConfirmationStale { .. }) => "confirmation-stale",
            Self::Public(McpInternalError::SpawnLineageLimitExceeded { .. }) => "spawn-lineage-limit-exceeded",
            Self::Public(McpInternalError::DefaultBranchUnknown { .. }) => "default-branch-unknown",
            Self::Public(McpInternalError::WorktreeVanished { .. }) => "worktree-vanished",
            Self::Public(McpInternalError::WorktreeMissing { .. }) => "worktree-missing",
            Self::Public(McpInternalError::Busy { .. }) => "busy",
            Self::Public(McpInternalError::Internal { .. }) => "internal",
            Self::Public(McpInternalError::ToolDisabled { .. }) => "tool-disabled",
            Self::Public(McpInternalError::ToolNotImplemented { .. }) => "tool-not-implemented",
            Self::Public(McpInternalError::RateLimited { .. }) => "rate-limited",
            Self::Public(McpInternalError::HostUnavailable { .. }) => "host-unavailable",
            Self::Public(McpInternalError::InvalidPath { .. }) => "invalid-path",
            Self::Public(McpInternalError::NameInUse { .. }) => "name-in-use",
            Self::Public(McpInternalError::ConfirmationRequired { .. }) => "confirmation-required",
            Self::Public(McpInternalError::RepoCommandTrustRequired { .. }) => "repo-command-trust-required",
            Self::Public(McpInternalError::OwnWorktreeRefused { .. }) => "own-worktree-refused",
            Self::Public(McpInternalError::StaleRemoteData { .. }) => "stale-remote-data",
            Self::Public(McpInternalError::DryRunUnsupported { .. }) => "dry-run-unsupported",
            Self::Public(McpInternalError::TooManyPendingActions { .. }) => "too-many-pending-actions",
            Self::Public(McpInternalError::Unauthenticated { .. }) => "unauthenticated",
            Self::Public(McpInternalError::SessionRevoked { .. }) => "session-revoked",
            Self::NameInUse { .. } => "name-in-use",
            Self::ConfirmationRequired { .. } => "confirmation-required",
            Self::RepoCommandTrustRequired { .. } => "repo-command-trust-required",
            Self::PrepNotConfigured { .. } => "prep-not-configured",
            Self::PrepFailed { .. } => "prep-failed",
        }
    }
}

trait ConfirmationGate {
    fn create(&self, session_id: &str, summary: &str, fingerprint: [u8; 32], payload: Value) -> Result<String, McpInternalError>;
    fn try_consume(&self, token: &str, expected_fingerprint: &[u8; 32]) -> Result<(), ConsumeError>;
}

struct RegistryConfirmationGate<'a> {
    registry: &'a PendingMcpActionRegistry,
}

impl ConfirmationGate for RegistryConfirmationGate<'_> {
    fn create(&self, session_id: &str, summary: &str, fingerprint: [u8; 32], payload: Value) -> Result<String, McpInternalError> {
        self.registry
            .create(session_id, McpToolName::CreateWorktree, summary.to_owned(), fingerprint, payload)
            .map(|pending| pending.id)
    }

    fn try_consume(&self, token: &str, expected_fingerprint: &[u8; 32]) -> Result<(), ConsumeError> {
        self.registry.try_consume(token, expected_fingerprint).map(|_| ())
    }
}

trait LineageTracker {
    fn depth(&self, session_id: &str) -> usize;
    fn child_count(&self, parent_session_id: &str) -> usize;
    fn record_spawn(&self, child_session_id: &str, parent_session_id: &str);
}

struct StaticLineageTracker;

impl LineageTracker for StaticLineageTracker {
    fn depth(&self, session_id: &str) -> usize {
        let guard = match MCP_SPAWN_PARENTS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut depth = 0_usize;
        let mut current = session_id;
        while let Some(parent) = guard.get(current) {
            depth += 1;
            if depth > 64 {
                break;
            }
            current = parent;
        }
        depth
    }

    fn child_count(&self, parent_session_id: &str) -> usize {
        let guard = match MCP_SPAWN_PARENTS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.values().filter(|parent| parent.as_str() == parent_session_id).count()
    }

    fn record_spawn(&self, child_session_id: &str, parent_session_id: &str) {
        let mut guard = match MCP_SPAWN_PARENTS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.insert(child_session_id.to_owned(), parent_session_id.to_owned());
    }
}

trait WorktreeCreator {
    fn create(&self, workspace_root: &Path, name: &str, from_oid: &str) -> Result<PathBuf, CoreError>;
}

struct ProdWorktreeCreator;

impl WorktreeCreator for ProdWorktreeCreator {
    fn create(&self, workspace_root: &Path, name: &str, from_oid: &str) -> Result<PathBuf, CoreError> {
        let workspace_root = dunce::canonicalize(workspace_root).map_err(|err| {
            CoreError::Public(McpInternalError::WorktreeMissing {
                message: format!("workspace root '{}': {err}", workspace_root.display()),
            })
        })?;
        if !workspace_root.is_dir() {
            return Err(CoreError::Public(McpInternalError::WorktreeMissing {
                message: format!("workspace root '{}' is not a directory", workspace_root.display()),
            }));
        }

        crate::repo_settings::ensure_arborist_dir(&workspace_root).map_err(|err| {
            CoreError::Public(McpInternalError::InvalidArg {
                message: format!(
                    "could not prepare {}: {err}",
                    workspace_root.join(crate::repo_settings::ARBORIST_DIR).display()
                ),
            })
        })?;

        let relative = PathBuf::from(WORKTREES_REL).join(name);
        let absolute = workspace_root.join(&relative);
        if fs::symlink_metadata(&absolute).is_ok() {
            return Err(CoreError::NameInUse {
                message: format!("{} already exists", absolute.display()),
                suggested_alternatives: suggested_alternatives(&workspace_root, name),
            });
        }

        let worktrees_dir = workspace_root.join(WORKTREES_REL);
        match fs::symlink_metadata(&worktrees_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CoreError::Public(McpInternalError::InvalidArg {
                    message: format!(
                        "{} is a symlink; refusing to create a worktree outside the workspace",
                        worktrees_dir.display()
                    ),
                }));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&worktrees_dir).map_err(|create_err| {
                    CoreError::Public(McpInternalError::Internal {
                        message: format!("could not create {}: {create_err}", worktrees_dir.display()),
                    })
                })?;
            }
            Err(err) => {
                return Err(CoreError::Public(McpInternalError::Internal {
                    message: format!("could not inspect {}: {err}", worktrees_dir.display()),
                }));
            }
        }

        let canon_worktrees = dunce::canonicalize(&worktrees_dir).map_err(|err| {
            CoreError::Public(McpInternalError::Internal {
                message: format!("could not canonicalize {}: {err}", worktrees_dir.display()),
            })
        })?;
        if !canon_worktrees.starts_with(&workspace_root) {
            return Err(CoreError::Public(McpInternalError::InvalidArg {
                message: format!("{} resolves outside the workspace", worktrees_dir.display()),
            }));
        }

        let output = git::git_command_mcp(&workspace_root)
            .args(["worktree", "add"])
            .arg(&relative)
            .args(["-b", name])
            .arg(from_oid)
            .output()
            .map_err(|err| {
                CoreError::Public(McpInternalError::Internal {
                    message: format!("git worktree add: {err}"),
                })
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if stderr.contains("already exists") || stderr.contains("already checked out") || stderr.contains("a branch named") {
                return Err(CoreError::NameInUse {
                    message: if stderr.is_empty() {
                        format!("{} already exists", absolute.display())
                    } else {
                        stderr
                    },
                    suggested_alternatives: suggested_alternatives(&workspace_root, name),
                });
            }
            return Err(CoreError::Public(McpInternalError::Internal {
                message: format!(
                    "git worktree add failed: {}",
                    if stderr.is_empty() { "<no stderr>".to_owned() } else { stderr }
                ),
            }));
        }

        let new_path = dunce::canonicalize(&absolute).map_err(|err| {
            CoreError::Public(McpInternalError::Internal {
                message: format!("worktree created but canonicalization failed: {}: {err}", absolute.display()),
            })
        })?;
        if !new_path.starts_with(&workspace_root) {
            return Err(CoreError::Public(McpInternalError::InvalidArg {
                message: format!(
                    "created worktree {} resolved outside workspace {}",
                    new_path.display(),
                    workspace_root.display()
                ),
            }));
        }
        Ok(new_path)
    }
}

trait PrepRunner {
    fn run(&self, worktree_path: &Path) -> Result<PrepPayload, CoreError>;
}

struct ProdPrepRunner {
    cfg: AppConfig,
    workspace_state_dir: PathBuf,
}

impl PrepRunner for ProdPrepRunner {
    fn run(&self, worktree_path: &Path) -> Result<PrepPayload, CoreError> {
        let cleaned = crate::worktree_prep::clean_commands(&self.cfg.worktree_prep_commands);
        if cleaned.is_empty() {
            return Err(CoreError::PrepNotConfigured {
                message: "prepFromConfig was requested, but no worktree prep commands are configured".to_owned(),
            });
        }

        let prep_id = WorktreePrepId::new_v4();
        let log_relative_path = PathBuf::from(crate::worktree_prep::LOG_SUBDIR).join(format!("{prep_id}.log"));
        let log_path = self.workspace_state_dir.join(&log_relative_path);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent).map_err(|err| CoreError::PrepFailed {
                message: format!("prep failed: could not create log directory '{}': {err}", parent.display()),
                log_relative_path: path_string(&log_relative_path),
            })?;
        }

        let script = cleaned.join(" && ");
        let mut stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|err| CoreError::PrepFailed {
                message: format!("prep failed: could not open log '{}': {err}", log_path.display()),
                log_relative_path: path_string(&log_relative_path),
            })?;
        writeln!(stdout_file, "[arborist] worktree prep for {}", worktree_path.display()).map_err(|err| CoreError::PrepFailed {
            message: format!("prep failed: could not write log header '{}': {err}", log_path.display()),
            log_relative_path: path_string(&log_relative_path),
        })?;
        writeln!(stdout_file, "[arborist] command: {script}").map_err(|err| CoreError::PrepFailed {
            message: format!("prep failed: could not write log header '{}': {err}", log_path.display()),
            log_relative_path: path_string(&log_relative_path),
        })?;
        let stderr_file = stdout_file.try_clone().map_err(|err| CoreError::PrepFailed {
            message: format!("prep failed: could not duplicate log handle '{}': {err}", log_path.display()),
            log_relative_path: path_string(&log_relative_path),
        })?;

        let shell = compose::platform_shell();
        let mut command = Command::new(&shell.program);
        command
            .arg(shell.flag)
            .arg(&script)
            .current_dir(worktree_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let status = command.status().map_err(|err| CoreError::PrepFailed {
            message: format!("prep failed: could not spawn prep command: {err}"),
            log_relative_path: path_string(&log_relative_path),
        })?;
        if !status.success() {
            return Err(CoreError::PrepFailed {
                message: format!(
                    "prep failed: command exited with {}. Inspect '{}'",
                    status.code().map_or_else(|| "no exit code".to_owned(), |code| code.to_string()),
                    log_relative_path.display()
                ),
                log_relative_path: path_string(&log_relative_path),
            });
        }

        Ok(PrepPayload {
            id: prep_id.to_string(),
            log_relative_path: path_string(&log_relative_path),
        })
    }
}

trait SpawnHook {
    fn spawn(
        &self,
        tool: Tool,
        worktree_path: &Path,
        parent_session_id: &str,
        initial_prompt: Option<&str>,
    ) -> Result<SpawnedSessionPayload, CoreError>;
}

struct ProdSpawnHook<'a> {
    ctx: &'a commands::AppContext,
    lineage: &'a dyn LineageTracker,
}

impl SpawnHook for ProdSpawnHook<'_> {
    fn spawn(
        &self,
        tool: Tool,
        worktree_path: &Path,
        parent_session_id: &str,
        initial_prompt: Option<&str>,
    ) -> Result<SpawnedSessionPayload, CoreError> {
        let created = session::session_create_impl(
            self.ctx,
            SessionCreateArgs {
                tool,
                worktree_path: worktree_path.to_path_buf(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
            },
        )
        .map_err(map_app_error)?;
        let child_session_id = created.id.to_string();
        self.lineage.record_spawn(&child_session_id, parent_session_id);

        if let Some(initial_prompt) = initial_prompt {
            let mut input = initial_prompt.to_owned();
            if !input.ends_with('\n') {
                input.push('\n');
            }
            session::session_input_impl(
                self.ctx,
                SessionInputArgs {
                    session_id: created.id,
                    data: input,
                },
            )
            .map_err(map_app_error)?;
        }

        Ok(SpawnedSessionPayload {
            session_id: child_session_id,
            tool,
        })
    }
}

struct CoreEnv<'a> {
    session_id: &'a str,
    session_label: &'a str,
    workspace_root: &'a Path,
    confirmation_mode: McpConfirmationMode,
    git: &'a dyn GitRunner,
    confirmation: &'a dyn ConfirmationGate,
    trust: &'a TrustedRequestStore,
    audit: &'a AuditLog,
    lineage: &'a dyn LineageTracker,
    worktree_creator: &'a dyn WorktreeCreator,
    prep_runner: &'a dyn PrepRunner,
    spawn_hook: &'a dyn SpawnHook,
    worktree_preview: Option<ShellCommandPreview>,
    spawn_preview: Option<ShellCommandPreview>,
}

#[must_use]
pub fn descriptor() -> McpToolDescriptor {
    McpToolDescriptor {
        name: "create_worktree".to_owned(),
        description: "Create a linked worktree under <workspaceRoot>/.arborist/.worktrees/<name>. Optionally run configured prep and spawn a sibling AI session."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "Worktree and branch name. Runtime validation uses the same rules as the in-app create dialog."
                },
                "fromBranch": {
                    "type": "string",
                    "description": "Unqualified branch name to fork from. Defaults to the workspace default branch."
                },
                "optionalPrompt": {
                    "type": "string",
                    "maxLength": 4096,
                    "description": "Optional visible prompt text. Rejected if it contains NUL, ANSI control escapes, or disallowed control bytes."
                },
                "prepFromConfig": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run the user's configured worktree prep commands after creation."
                },
                "spawnSibling": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["tool"],
                    "properties": {
                        "tool": {
                            "type": "string",
                            "enum": ["claude", "copilot", "codex"]
                        },
                        "initialPrompt": {
                            "type": "string",
                            "maxLength": 4096,
                            "description": "Optional first user-visible PTY input for the spawned session."
                        }
                    }
                },
                "confirmationToken": {
                    "type": "string",
                    "description": "Opaque single-use confirmation token returned by Arborist approval UI."
                }
            }
        }),
    }
}

pub async fn invoke(registry: &McpSessionRegistry, session_id: &str, args: Value) -> Result<Value, McpInternalError> {
    let parsed = parse_and_sanitize_args(args).map_err(CoreError::public_error)?;
    let mcp_context = registry_context_clone(registry);
    let app = &mcp_context.app;
    let _switch = session::acquire_switch_read(app).map_err(map_app_error_to_mcp)?;
    let workspace_root = bound_workspace_root(app)?;
    let session_label = session_label_for(app, session_id);
    let user_cfg = app.store().load_config();
    let (effective_cfg, _) = effective_config_with_repo_overlay(&workspace_root, &user_cfg);
    let target_worktree_path = target_worktree_path(&workspace_root, &parsed.name);
    let confirmation_mode = create_worktree_confirmation_mode(&effective_cfg.mcp);

    if parsed.prep_from_config && crate::worktree_prep::clean_commands(&effective_cfg.worktree_prep_commands).is_empty() {
        return Err(CoreError::PrepNotConfigured {
            message: "prepFromConfig was requested, but no worktree prep commands are configured".to_owned(),
        }
        .public_error());
    }
    if let Some(spawn) = parsed.spawn_sibling.as_ref() {
        if !effective_cfg.ai_plugin_enabled_for_tool(spawn.tool) {
            return Err(McpInternalError::InvalidArg {
                message: format!("AI tool '{}' is disabled in Arborist Settings", spawn.tool.as_id()),
            });
        }
    }

    let worktree_preview = if parsed.prep_from_config {
        Some(build_worktree_prep_preview(&workspace_root, &user_cfg, target_worktree_path.clone()))
    } else {
        None
    };
    let spawn_preview = if let Some(spawn) = parsed.spawn_sibling.as_ref() {
        Some(build_spawn_preview(app, &workspace_root, &user_cfg, spawn.tool, target_worktree_path)?)
    } else {
        None
    };

    let confirmation = RegistryConfirmationGate {
        registry: &mcp_context.confirm,
    };
    let lineage = StaticLineageTracker;
    let worktree_creator = ProdWorktreeCreator;
    let prep_runner = ProdPrepRunner {
        cfg: effective_cfg,
        workspace_state_dir: mcp_context.workspace_state_dir.clone(),
    };
    let spawn_hook = ProdSpawnHook { ctx: app, lineage: &lineage };
    let env = CoreEnv {
        session_id,
        session_label: &session_label,
        workspace_root: &workspace_root,
        confirmation_mode,
        git: &*app.git_runner,
        confirmation: &confirmation,
        trust: &mcp_context.trust,
        audit: &mcp_context.audit,
        lineage: &lineage,
        worktree_creator: &worktree_creator,
        prep_runner: &prep_runner,
        spawn_hook: &spawn_hook,
        worktree_preview,
        spawn_preview,
    };

    core_create(parsed, &env)
        .map(|success| success.to_json())
        .map_err(CoreError::public_error)
}

#[allow(clippy::too_many_arguments)]
fn core_create(args: CreateWorktreeArgs, env: &CoreEnv<'_>) -> Result<CreateWorktreeSuccess, CoreError> {
    let start = Instant::now();
    let has_confirmation_token = args.confirmation_token.is_some();
    if !has_confirmation_token {
        ensure_target_name_available(env.workspace_root, &args.name)?;
    }

    let lineage_depth = env.lineage.depth(env.session_id);
    if args.spawn_sibling.is_some() {
        let child_count = env.lineage.child_count(env.session_id);
        if child_count >= MAX_CHILDREN_PER_PARENT {
            return Err(CoreError::Public(McpInternalError::SpawnLineageLimitExceeded {
                message: format!("session '{}' already has {MAX_CHILDREN_PER_PARENT} MCP-spawned children", env.session_id),
            }));
        }
        if lineage_depth + 1 > MAX_LINEAGE_DEPTH {
            return Err(CoreError::Public(McpInternalError::SpawnLineageLimitExceeded {
                message: format!("creating another spawned child would exceed the lineage depth limit of {MAX_LINEAGE_DEPTH}"),
            }));
        }
    }

    if let Some(preview) = env.worktree_preview.as_ref() {
        if preview.trust_required {
            return Err(CoreError::RepoCommandTrustRequired {
                message: repo_command_trust_message(preview),
                preview: preview.clone(),
            });
        }
    }
    if let Some(preview) = env.spawn_preview.as_ref() {
        if preview.trust_required {
            return Err(CoreError::RepoCommandTrustRequired {
                message: repo_command_trust_message(preview),
                preview: preview.clone(),
            });
        }
    }

    let (from_branch, from_oid) = resolve_from_branch(&args, env.git, env.workspace_root)?;
    let summary = confirmation_summary(&args, &from_branch);
    let fingerprint = build_fingerprint(
        &args,
        &from_branch,
        &from_oid,
        lineage_depth,
        env.confirmation_mode,
        env.worktree_preview.as_ref(),
        env.spawn_preview.as_ref(),
    )?;
    let payload = serde_json::to_value(&args).map_err(|err| {
        CoreError::Public(McpInternalError::Internal {
            message: format!("serialize confirmation payload: {err}"),
        })
    })?;

    let requires_confirmation = requires_confirmation(env.confirmation_mode, &args, lineage_depth);
    let decision = if let Some(token) = args.confirmation_token.as_deref() {
        match env.confirmation.try_consume(token, &fingerprint) {
            Ok(()) => McpAuditDecision::Approved,
            Err(ConsumeError::Unknown) => {
                return Err(CoreError::Public(McpInternalError::InvalidConfirmation {
                    message: "confirmation token is unknown, stale, or has already been consumed".to_owned(),
                }));
            }
            Err(ConsumeError::Expired) => {
                return Err(CoreError::Public(McpInternalError::ConfirmationExpired {
                    message: "confirmation token expired; request a fresh approval".to_owned(),
                }));
            }
            Err(ConsumeError::FingerprintMismatch) => {
                return Err(CoreError::Public(McpInternalError::ConfirmationStale {
                    message: "confirmation token does not match the current request arguments".to_owned(),
                }));
            }
        }
    } else if !requires_confirmation {
        McpAuditDecision::AutoApproved
    } else if lineage_depth == 0 {
        if env.trust.check(env.session_id, McpToolName::CreateWorktree, &fingerprint).is_some() {
            McpAuditDecision::AutoApproved
        } else {
            let pending_id = env
                .confirmation
                .create(env.session_id, &summary, fingerprint, payload)
                .map_err(CoreError::Public)?;
            return Err(CoreError::ConfirmationRequired {
                message: "user confirmation is required before Arborist will create this worktree".to_owned(),
                pending_id,
                summary,
            });
        }
    } else {
        let pending_id = env
            .confirmation
            .create(env.session_id, &summary, fingerprint, payload)
            .map_err(CoreError::Public)?;
        return Err(CoreError::ConfirmationRequired {
            message: "this session was itself MCP-spawned, so creating another worktree requires fresh confirmation".to_owned(),
            pending_id,
            summary,
        });
    };

    if has_confirmation_token {
        ensure_target_name_available(env.workspace_root, &args.name)?;
    }

    let absolute_path = env.worktree_creator.create(env.workspace_root, &args.name, &from_oid)?;
    let relative_path = absolute_path
        .strip_prefix(env.workspace_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| absolute_path.clone());

    let prep = if args.prep_from_config {
        Some(env.prep_runner.run(&absolute_path)?)
    } else {
        None
    };
    let spawned_session = if let Some(spawn) = args.spawn_sibling.as_ref() {
        Some(
            env.spawn_hook
                .spawn(spawn.tool, &absolute_path, env.session_id, spawn.initial_prompt.as_deref())?,
        )
    } else {
        None
    };

    let success = CreateWorktreeSuccess {
        worktree: WorktreePayload {
            absolute_path,
            relative_path,
            branch: args.name.clone(),
            head: from_oid,
        },
        prep,
        spawned_session,
    };
    append_audit(env, &args, &summary, decision, start.elapsed(), &success)?;
    Ok(success)
}

fn ensure_target_name_available(workspace_root: &Path, name: &str) -> Result<(), CoreError> {
    let target_path = target_worktree_path(workspace_root, name);
    if fs::symlink_metadata(&target_path).is_ok() {
        return Err(CoreError::NameInUse {
            message: format!("{} already exists", target_path.display()),
            suggested_alternatives: suggested_alternatives(workspace_root, name),
        });
    }
    Ok(())
}

fn parse_and_sanitize_args(args: Value) -> Result<CreateWorktreeArgs, CoreError> {
    let mut parsed: CreateWorktreeArgs = serde_json::from_value(args).map_err(|err| {
        CoreError::Public(McpInternalError::InvalidArg {
            message: format!("create_worktree args must match the declared schema: {err}"),
        })
    })?;
    if parsed.name.chars().count() > MAX_NAME_CHARS {
        return Err(CoreError::Public(McpInternalError::InvalidName {
            message: format!("name cannot exceed {MAX_NAME_CHARS} characters"),
        }));
    }
    compose::validate_worktree_name(&parsed.name).map_err(|message| CoreError::Public(McpInternalError::InvalidName { message }))?;
    if let Some(from_branch) = parsed.from_branch.as_deref() {
        compose::validate_ref_name(from_branch).map_err(|message| {
            CoreError::Public(McpInternalError::InvalidArg {
                message: format!("invalid fromBranch: {message}"),
            })
        })?;
    }
    parsed.optional_prompt = sanitize_prompt("optionalPrompt", parsed.optional_prompt)?;
    if let Some(spawn) = parsed.spawn_sibling.as_mut() {
        spawn.initial_prompt = sanitize_prompt("spawnSibling.initialPrompt", spawn.initial_prompt.clone())?;
    }
    Ok(parsed)
}

fn sanitize_prompt(field: &str, value: Option<String>) -> Result<Option<String>, CoreError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let bytes = value.as_bytes();
    if bytes.len() > MAX_PROMPT_BYTES {
        return Err(CoreError::Public(McpInternalError::InvalidArg {
            message: format!("{field} exceeds {MAX_PROMPT_BYTES} bytes"),
        }));
    }

    let mut index = 0_usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            0x00 => {
                return Err(CoreError::Public(McpInternalError::InvalidArg {
                    message: format!("{field} contains NUL byte 0x00 at byte offset {index}"),
                }));
            }
            b'\n' | b'\t' => {
                index += 1;
            }
            b'\r' => {
                return Err(CoreError::Public(McpInternalError::InvalidArg {
                    message: format!("{field} contains disallowed control byte 0x0D at byte offset {index}"),
                }));
            }
            0x1B => {
                let next = bytes.get(index + 1).copied();
                let sequence = match next {
                    Some(b']') => "OSC escape sequence 0x1B 0x5D",
                    Some(b'[') => "CSI escape sequence 0x1B 0x5B",
                    Some(other) => {
                        return Err(CoreError::Public(McpInternalError::InvalidArg {
                            message: format!("{field} contains escape byte 0x1B followed by 0x{other:02X} at byte offset {index}"),
                        }));
                    }
                    None => "escape byte 0x1B",
                };
                return Err(CoreError::Public(McpInternalError::InvalidArg {
                    message: format!("{field} contains disallowed {sequence} at byte offset {index}"),
                }));
            }
            0x01..=0x1F | 0x7F => {
                return Err(CoreError::Public(McpInternalError::InvalidArg {
                    message: format!("{field} contains disallowed control byte 0x{byte:02X} at byte offset {index}"),
                }));
            }
            _ => {
                index += 1;
            }
        }
    }

    Ok(Some(value))
}

fn resolve_from_branch(args: &CreateWorktreeArgs, git: &dyn GitRunner, workspace_root: &Path) -> Result<(String, String), CoreError> {
    if let Some(from_branch) = args.from_branch.as_deref() {
        let oid = resolve_branch_oid(git, workspace_root, from_branch, false)?;
        return Ok((from_branch.to_owned(), oid));
    }

    let default_branch = git.default_branch(workspace_root).map_err(|err| CoreError::Public(err.into()))?;
    let oid = resolve_branch_oid(git, workspace_root, &default_branch.branch, true)?;
    Ok((default_branch.branch, oid))
}

fn resolve_branch_oid(git: &dyn GitRunner, workspace_root: &Path, branch: &str, default_branch: bool) -> Result<String, CoreError> {
    let local_ref = format!("refs/heads/{branch}");
    match git.rev_parse_verify(workspace_root, &local_ref) {
        Ok(oid) => return Ok(oid),
        Err(crate::git::GitError::RefNotFound { .. }) => {}
        Err(err) => return Err(CoreError::Public(err.into())),
    }

    let remote_ref = format!("refs/remotes/origin/{branch}");
    match git.rev_parse_verify(workspace_root, &remote_ref) {
        Ok(oid) => Ok(oid),
        Err(crate::git::GitError::RefNotFound { .. }) if default_branch => Err(CoreError::Public(McpInternalError::DefaultBranchUnknown {
            message: format!("default branch '{branch}' could not be resolved to a commit"),
        })),
        Err(crate::git::GitError::RefNotFound { .. }) => Err(CoreError::Public(McpInternalError::InvalidArg {
            message: format!("fromBranch '{branch}' was not found as a local branch or origin branch"),
        })),
        Err(err) => Err(CoreError::Public(err.into())),
    }
}

fn build_fingerprint(
    args: &CreateWorktreeArgs,
    from_branch: &str,
    from_oid: &str,
    lineage_depth: usize,
    confirmation_mode: McpConfirmationMode,
    worktree_preview: Option<&ShellCommandPreview>,
    spawn_preview: Option<&ShellCommandPreview>,
) -> Result<[u8; 32], CoreError> {
    let mut repo_trust_fingerprints = Vec::new();
    if let Some(preview) = worktree_preview {
        repo_trust_fingerprints.extend(preview.trust_records.iter().map(|record| record.fingerprint.clone()));
    }
    if let Some(preview) = spawn_preview {
        repo_trust_fingerprints.extend(preview.trust_records.iter().map(|record| record.fingerprint.clone()));
    }
    repo_trust_fingerprints.sort();
    repo_trust_fingerprints.dedup();

    let canonical = serde_json::to_string(&json!({
        "name": args.name,
        "fromBranch": from_branch,
        "fromBranchOid": from_oid,
        "optionalPromptSha256": args.optional_prompt.as_deref().map(hash_sha256_hex),
        "prepFromConfig": args.prep_from_config,
        "spawnSibling": args.spawn_sibling.as_ref().map(|spawn| {
            json!({
                "tool": spawn.tool.as_id(),
                "initialPromptSha256": spawn.initial_prompt.as_deref().map(hash_sha256_hex),
            })
        }),
        "confirmationMode": confirmation_mode_id(confirmation_mode),
        "repoTrustFingerprints": repo_trust_fingerprints,
        "lineageDepth": lineage_depth,
    }))
    .map_err(|err| {
        CoreError::Public(McpInternalError::Internal {
            message: format!("serialize args fingerprint: {err}"),
        })
    })?;
    Ok(fingerprint_args(&canonical))
}

fn requires_confirmation(mode: McpConfirmationMode, args: &CreateWorktreeArgs, lineage_depth: usize) -> bool {
    if lineage_depth >= 1 {
        return true;
    }
    if args.prep_from_config || args.spawn_sibling.is_some() {
        return true;
    }
    mode != McpConfirmationMode::Never
}

fn confirmation_summary(args: &CreateWorktreeArgs, from_branch: &str) -> String {
    let mut summary = format!("Create worktree '{}' from '{}'", args.name, from_branch);
    if args.prep_from_config {
        summary.push_str(", run configured prep");
    }
    if let Some(spawn) = args.spawn_sibling.as_ref() {
        summary.push_str(&format!(", and spawn a {} sibling session", spawn.tool.as_id()));
    }
    summary
}

fn append_audit(
    env: &CoreEnv<'_>,
    args: &CreateWorktreeArgs,
    summary: &str,
    decision: McpAuditDecision,
    duration: std::time::Duration,
    success: &CreateWorktreeSuccess,
) -> Result<(), CoreError> {
    let ts = OffsetDateTime::now_utc().format(&Rfc3339).map_err(|err| {
        CoreError::Public(McpInternalError::Internal {
            message: format!("format audit timestamp: {err}"),
        })
    })?;
    let confirmation_token_sha256 = args.confirmation_token.as_deref().map(hash_sha256_hex);
    env.audit
        .append_destructive(AuditEntryInput {
            ts,
            session_id: env.session_id.to_owned(),
            session_label: env.session_label.to_owned(),
            tool: McpToolName::CreateWorktree.as_id().to_owned(),
            decision,
            args_summary: summary.to_owned(),
            result: json!({
                "scopeHints": ["workspace"],
                "optionalPromptSha256": args.optional_prompt.as_deref().map(hash_sha256_hex),
                "spawnInitialPromptSha256": args
                    .spawn_sibling
                    .as_ref()
                    .and_then(|spawn| spawn.initial_prompt.as_deref())
                    .map(hash_sha256_hex),
                "response": success.to_json(),
            }),
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            request_id: Uuid::new_v4().as_simple().to_string(),
            confirmation_token_sha256,
            audit_id: Uuid::new_v4().as_simple().to_string(),
        })
        .map(|_| ())
        .map_err(|err| {
            CoreError::Public(McpInternalError::Internal {
                message: format!("append MCP audit log: {err}"),
            })
        })
}

fn build_worktree_prep_preview(workspace_root: &Path, user_cfg: &AppConfig, target_worktree_path: PathBuf) -> ShellCommandPreview {
    let (effective_cfg, repo_settings) = effective_config_with_repo_overlay(workspace_root, user_cfg);
    let cleaned = crate::worktree_prep::clean_commands(repo_settings.worktree_prep_commands_if_user_unset(user_cfg).unwrap_or(&[]));
    let candidates = if cleaned.is_empty() {
        Vec::new()
    } else {
        vec![RepoCommandCandidate {
            kind: ShellCommandKind::WorktreePrep,
            command: cleaned.join(" && "),
            scope: None,
            workspace_root: workspace_root.to_path_buf(),
            source_path: RepoSettings::settings_path(workspace_root),
            target_worktree_path: target_worktree_path.clone(),
        }]
    };
    shell_trust::preview(target_worktree_path, &effective_cfg, candidates)
}

fn build_spawn_preview(
    ctx: &commands::AppContext,
    workspace_root: &Path,
    user_cfg: &AppConfig,
    tool: Tool,
    target_worktree_path: PathBuf,
) -> Result<ShellCommandPreview, McpInternalError> {
    let (effective_cfg, repo_settings) = effective_config_with_repo_overlay(workspace_root, user_cfg);
    let label = target_worktree_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_owned());
    let preview_session_id = SessionId(uuid::Uuid::nil());
    let composed = compose::compose_command(&ComposeInputs {
        session_id: preview_session_id,
        tool,
        worktree_path: &target_worktree_path,
        worktree_label: &label,
        cli_launch_command: Some(effective_cfg.ai_launch_command_for_tool(tool)),
        helper_exe_path: ctx.claude_hook_helper.as_deref(),
        user_home: ctx.user_home.as_deref(),
    })
    .map_err(|err| McpInternalError::Internal { message: err.to_string() })?;
    let command = shell_trust::normalize_command_for_session(preview_session_id, &composed.composed_command);
    let candidates = repo_settings
        .ai_launch_command_for_id_if_user_unset(user_cfg, tool.as_id())
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(|_| RepoCommandCandidate {
            kind: ShellCommandKind::AiLaunch,
            command,
            scope: Some(tool.as_id().to_owned()),
            workspace_root: workspace_root.to_path_buf(),
            source_path: RepoSettings::settings_path(workspace_root),
            target_worktree_path: target_worktree_path.clone(),
        })
        .into_iter()
        .collect();
    Ok(shell_trust::preview(target_worktree_path, &effective_cfg, candidates))
}

fn effective_config_with_repo_overlay(workspace_root: &Path, user_cfg: &AppConfig) -> (AppConfig, RepoSettings) {
    let repo_settings = RepoSettings::load(workspace_root);
    let mut effective_cfg = user_cfg.clone();
    repo_settings.apply_to(&mut effective_cfg);
    (effective_cfg, repo_settings)
}

fn create_worktree_confirmation_mode(config: &AppConfigMcp) -> McpConfirmationMode {
    let default_config = AppConfigMcp::default();
    config
        .tools
        .get(McpToolName::CreateWorktree.as_id())
        .map(|tool| tool.requires_confirmation)
        .or_else(|| {
            default_config
                .tools
                .get(McpToolName::CreateWorktree.as_id())
                .map(|tool| tool.requires_confirmation)
        })
        .unwrap_or(McpConfirmationMode::FirstUse)
}

fn confirmation_mode_id(mode: McpConfirmationMode) -> &'static str {
    match mode {
        McpConfirmationMode::Always => "always",
        McpConfirmationMode::FirstUse => "firstUse",
        McpConfirmationMode::Never => "never",
    }
}

fn repo_command_trust_message(preview: &ShellCommandPreview) -> String {
    preview
        .commands
        .iter()
        .find(|command| !command.trusted)
        .map(|command| {
            let source = command
                .source_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "repo settings".to_owned());
            let kind = match command.kind {
                ShellCommandKind::AiLaunch => "AI launch",
                ShellCommandKind::WorktreePrep => "worktree prep",
            };
            format!("repo-provided {kind} command from {source} must be trusted before it can run")
        })
        .unwrap_or_else(|| "repo-provided command must be trusted before it can run".to_owned())
}

fn target_worktree_path(workspace_root: &Path, name: &str) -> PathBuf {
    workspace_root.join(WORKTREES_REL).join(name)
}

fn suggested_alternatives(workspace_root: &Path, base: &str) -> Vec<String> {
    let mut suggestions = Vec::new();
    for suffix in 2..=4 {
        let candidate = format!("{base}-{suffix}");
        if compose::validate_worktree_name(&candidate).is_err() {
            continue;
        }
        if fs::symlink_metadata(target_worktree_path(workspace_root, &candidate)).is_ok() {
            continue;
        }
        suggestions.push(candidate);
    }
    suggestions
}

fn hash_sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

fn bound_workspace_root(ctx: &commands::AppContext) -> Result<PathBuf, McpInternalError> {
    let guard = ctx.workspace.read().map_err(|_| McpInternalError::Internal {
        message: "workspace lock poisoned".to_owned(),
    })?;
    let Some(workspace_root) = guard.workspace_root.clone() else {
        return Err(McpInternalError::WorkspaceUnbound {
            message: "this MCP session is not bound to an open workspace".to_owned(),
        });
    };
    drop(guard);
    dunce::canonicalize(&workspace_root).map_err(|err| McpInternalError::WorktreeMissing {
        message: format!("workspace root '{}': {err}", workspace_root.display()),
    })
}

fn session_label_for(ctx: &commands::AppContext, session_id: &str) -> String {
    let Ok(uuid) = Uuid::parse_str(session_id) else {
        return session_id.to_owned();
    };
    ctx.store()
        .load_sessions()
        .get(&SessionId(uuid))
        .map(|session| session.label.clone())
        .unwrap_or_else(|| session_id.to_owned())
}

fn map_app_error(err: AppError) -> CoreError {
    CoreError::Public(map_app_error_to_mcp(err))
}

fn map_app_error_to_mcp(err: AppError) -> McpInternalError {
    match err.code.as_str() {
        "InvalidPath" => McpInternalError::InvalidArg { message: err.message },
        "WorktreeMissing" => McpInternalError::WorktreeMissing { message: err.message },
        "WorkspaceSwitchInProgress" => McpInternalError::Busy { message: err.message },
        "PluginDisabled" | "NotFound" | "PermissionDenied" => McpInternalError::InvalidArg { message: err.message },
        _ => McpInternalError::Internal { message: err.message },
    }
}

fn registry_context_clone(registry: &McpSessionRegistry) -> std::sync::Arc<crate::mcp::McpContext> {
    // Safe accessor on `McpSessionRegistry` introduced after this tool's first draft. Previously
    // this helper did an unsafe pointer cast under the assumption that `context` was the first
    // field; field reordering would have been undefined behaviour. Always go through `context()`.
    registry.context()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use arborist_types::git::{DefaultBranchInfo, DefaultBranchSource, MergeFromBranchOutcome, MergeTreeOutcome, WorktreeGitStatusSummary};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::config_store::ConfigStore;
    use crate::git::GitError;
    use crate::mcp::context::McpContext;
    use crate::mcp::types::McpContextConfig;
    use crate::pty_pool::{PortablePtySpawner, PtyPool, PtySink};
    use crate::types::{Error, SessionMetricsEvent, WorktreeGitStatus, WorktreeInfo};

    #[derive(Default)]
    struct StubGitRunner {
        default_branch: Option<DefaultBranchInfo>,
        rev_parses: HashMap<String, String>,
    }

    impl StubGitRunner {
        fn with_default_branch(mut self, branch: &str, oid: &str) -> Self {
            self.default_branch = Some(DefaultBranchInfo {
                branch: branch.to_owned(),
                source: DefaultBranchSource::OriginHead,
            });
            self.rev_parses.insert(format!("refs/heads/{branch}"), oid.to_owned());
            self
        }
    }

    impl GitRunner for StubGitRunner {
        fn list_worktrees(&self, _repo_root: &Path) -> Result<Vec<WorktreeInfo>, Error> {
            panic!("unused in create_worktree tests")
        }

        fn git_toplevel(&self, _path: &Path) -> Result<Option<PathBuf>, Error> {
            panic!("unused in create_worktree tests")
        }

        fn create_worktree(&self, _repo_root: &Path, _relative_path: &Path, _branch: &str) -> Result<PathBuf, Error> {
            panic!("unused in create_worktree tests")
        }

        fn remove_worktree(&self, _repo_root: &Path, _worktree_path: &Path) -> Result<(), Error> {
            panic!("unused in create_worktree tests")
        }

        fn git_status(&self, _worktree_path: &Path) -> Result<WorktreeGitStatus, Error> {
            panic!("unused in create_worktree tests")
        }

        fn fetch_origin(&self, _root: &Path, _timeout: Duration) -> Result<(), GitError> {
            panic!("unused in create_worktree tests")
        }

        fn branches_merged_into(&self, _root: &Path, _target_oid: &str) -> Result<std::collections::HashSet<String>, GitError> {
            panic!("unused in create_worktree tests")
        }

        fn cherry_empty(&self, _root: &Path, _upstream_oid: &str, _branch: &str) -> Result<bool, GitError> {
            panic!("unused in create_worktree tests")
        }

        fn merge_from_branch(
            &self,
            _worktree: &Path,
            _source_oid: &str,
            _leave_conflicts: bool,
            _timeout: Duration,
        ) -> Result<MergeFromBranchOutcome, GitError> {
            panic!("unused in create_worktree tests")
        }

        fn default_branch(&self, _root: &Path) -> Result<DefaultBranchInfo, GitError> {
            self.default_branch.clone().ok_or(GitError::DefaultBranchUnknown)
        }

        fn rev_parse_verify(&self, _root: &Path, ref_expr: &str) -> Result<String, GitError> {
            self.rev_parses.get(ref_expr).cloned().ok_or_else(|| GitError::RefNotFound {
                ref_expr: ref_expr.to_owned(),
            })
        }

        fn git_status_mcp(&self, _worktree: &Path) -> Result<WorktreeGitStatusSummary, GitError> {
            panic!("unused in create_worktree tests")
        }

        fn merge_tree_dry_run(&self, _root: &Path, _base_oid: &str, _source_oid: &str) -> Result<MergeTreeOutcome, GitError> {
            panic!("unused in create_worktree tests")
        }

        fn merge_abort(&self, _worktree: &Path) -> Result<(), GitError> {
            panic!("unused in create_worktree tests")
        }

        fn has_merge_head(&self, _worktree: &Path) -> Result<bool, GitError> {
            panic!("unused in create_worktree tests")
        }
    }

    #[derive(Default)]
    struct TestLineageTracker {
        depth: usize,
        child_count: usize,
        recorded: Mutex<Vec<(String, String)>>,
    }

    impl LineageTracker for TestLineageTracker {
        fn depth(&self, _session_id: &str) -> usize {
            self.depth
        }

        fn child_count(&self, _parent_session_id: &str) -> usize {
            self.child_count
        }

        fn record_spawn(&self, child_session_id: &str, parent_session_id: &str) {
            self.recorded
                .lock()
                .expect("recorded lock")
                .push((child_session_id.to_owned(), parent_session_id.to_owned()));
        }
    }

    #[derive(Default)]
    struct TestWorktreeCreator {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl WorktreeCreator for TestWorktreeCreator {
        fn create(&self, workspace_root: &Path, name: &str, _from_oid: &str) -> Result<PathBuf, CoreError> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((workspace_root.display().to_string(), name.to_owned()));
            let path = workspace_root.join(WORKTREES_REL).join(name);
            fs::create_dir_all(&path).expect("create fake worktree path");
            Ok(path)
        }
    }

    #[derive(Clone)]
    enum TestPrepResult {
        Success(PrepPayload),
        Error(CoreError),
    }

    struct TestPrepRunner {
        calls: Mutex<Vec<PathBuf>>,
        result: TestPrepResult,
    }

    impl PrepRunner for TestPrepRunner {
        fn run(&self, worktree_path: &Path) -> Result<PrepPayload, CoreError> {
            self.calls.lock().expect("prep calls lock").push(worktree_path.to_path_buf());
            match &self.result {
                TestPrepResult::Success(result) => Ok(result.clone()),
                TestPrepResult::Error(err) => Err(err.clone()),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SpawnCall {
        tool: Tool,
        worktree_path: PathBuf,
        parent_session_id: String,
        initial_prompt: Option<String>,
    }

    struct TestSpawnHook {
        calls: Mutex<Vec<SpawnCall>>,
        result: SpawnedSessionPayload,
    }

    impl SpawnHook for TestSpawnHook {
        fn spawn(
            &self,
            tool: Tool,
            worktree_path: &Path,
            parent_session_id: &str,
            initial_prompt: Option<&str>,
        ) -> Result<SpawnedSessionPayload, CoreError> {
            self.calls.lock().expect("spawn calls lock").push(SpawnCall {
                tool,
                worktree_path: worktree_path.to_path_buf(),
                parent_session_id: parent_session_id.to_owned(),
                initial_prompt: initial_prompt.map(str::to_owned),
            });
            Ok(self.result.clone())
        }
    }

    struct ExpiredConfirmationGate {
        pending_id: String,
    }

    impl ConfirmationGate for ExpiredConfirmationGate {
        fn create(&self, _session_id: &str, _summary: &str, _fingerprint: [u8; 32], _payload: Value) -> Result<String, McpInternalError> {
            Ok(self.pending_id.clone())
        }

        fn try_consume(&self, _token: &str, _expected_fingerprint: &[u8; 32]) -> Result<(), ConsumeError> {
            Err(ConsumeError::Expired)
        }
    }

    fn default_spawn_hook() -> TestSpawnHook {
        TestSpawnHook {
            calls: Mutex::new(Vec::new()),
            result: SpawnedSessionPayload {
                session_id: "child-session".to_owned(),
                tool: Tool::Claude,
            },
        }
    }

    fn default_prep_runner() -> TestPrepRunner {
        TestPrepRunner {
            calls: Mutex::new(Vec::new()),
            result: TestPrepResult::Success(PrepPayload {
                id: "prep-1".to_owned(),
                log_relative_path: "worktree-prep-logs\\prep-1.log".to_owned(),
            }),
        }
    }

    fn registry_confirmation_gate<'a>(registry: &'a PendingMcpActionRegistry) -> RegistryConfirmationGate<'a> {
        RegistryConfirmationGate { registry }
    }

    fn new_audit_log() -> (TempDir, AuditLog) {
        let dir = TempDir::new().expect("tempdir");
        let audit = AuditLog::new(dir.path().to_path_buf()).expect("audit log");
        (dir, audit)
    }

    fn new_trust_store() -> TrustedRequestStore {
        TrustedRequestStore::new(Duration::from_secs(24 * 60 * 60))
    }

    #[allow(clippy::too_many_arguments)]
    fn base_env<'a>(
        session_id: &'a str,
        session_label: &'a str,
        workspace_root: &'a Path,
        git: &'a dyn GitRunner,
        confirmation: &'a dyn ConfirmationGate,
        trust: &'a TrustedRequestStore,
        audit: &'a AuditLog,
        lineage: &'a dyn LineageTracker,
        worktree_creator: &'a dyn WorktreeCreator,
        prep_runner: &'a dyn PrepRunner,
        spawn_hook: &'a dyn SpawnHook,
    ) -> CoreEnv<'a> {
        CoreEnv {
            session_id,
            session_label,
            workspace_root,
            confirmation_mode: McpConfirmationMode::Never,
            git,
            confirmation,
            trust,
            audit,
            lineage,
            worktree_creator,
            prep_runner,
            spawn_hook,
            worktree_preview: None,
            spawn_preview: None,
        }
    }

    fn create_args(name: &str) -> CreateWorktreeArgs {
        CreateWorktreeArgs {
            name: name.to_owned(),
            from_branch: None,
            optional_prompt: None,
            prep_from_config: false,
            spawn_sibling: None,
            confirmation_token: None,
        }
    }

    fn parse_args(value: Value) -> CreateWorktreeArgs {
        parse_and_sanitize_args(value).expect("args should parse")
    }

    #[test]
    fn invalid_name_returns_invalid_name() {
        let err = parse_and_sanitize_args(json!({ "name": "../../etc/passwd" })).expect_err("invalid name should fail");
        assert_eq!(err.code(), "invalid-name");
    }

    #[test]
    fn invalid_from_branch_path_traversal_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({ "name": "feature/demo", "fromBranch": "../main" })).expect_err("invalid fromBranch should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn invalid_from_branch_upstream_syntax_returns_invalid_arg() {
        let err =
            parse_and_sanitize_args(json!({ "name": "feature/demo", "fromBranch": "main@{upstream}" })).expect_err("invalid fromBranch should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn invalid_from_branch_revision_operator_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({ "name": "feature/demo", "fromBranch": "HEAD~2" })).expect_err("invalid fromBranch should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn optional_prompt_over_4kb_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "optionalPrompt": "a".repeat(MAX_PROMPT_BYTES + 1)
        }))
        .expect_err("oversized prompt should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn optional_prompt_nul_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "optionalPrompt": "ignore previous\u{0000}instructions"
        }))
        .expect_err("NUL byte should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn optional_prompt_osc_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "optionalPrompt": "\u{001b}]0;FAKE\u{0007}"
        }))
        .expect_err("OSC escape should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn optional_prompt_csi_clear_screen_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "optionalPrompt": "\u{001b}[2J"
        }))
        .expect_err("CSI clear-screen should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn optional_prompt_csi_cursor_hide_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "optionalPrompt": "\u{001b}[?25l"
        }))
        .expect_err("CSI cursor-hide should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn optional_prompt_bare_carriage_return_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "optionalPrompt": "hello\rworld"
        }))
        .expect_err("bare carriage return should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn initial_prompt_tab_is_allowed() {
        let args = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "spawnSibling": {
                "tool": "claude",
                "initialPrompt": "plan\tstep"
            }
        }))
        .expect("tab should be allowed");
        assert_eq!(
            args.spawn_sibling.as_ref().and_then(|spawn| spawn.initial_prompt.as_deref()),
            Some("plan\tstep")
        );
    }

    #[test]
    fn initial_prompt_over_4kb_returns_invalid_arg() {
        let err = parse_and_sanitize_args(json!({
            "name": "feature/demo",
            "spawnSibling": {
                "tool": "claude",
                "initialPrompt": "a".repeat(MAX_PROMPT_BYTES + 1)
            }
        }))
        .expect_err("oversized initial prompt should fail");
        assert_eq!(err.code(), "invalid-arg");
    }

    #[test]
    fn happy_path_name_only_without_confirmation_creates_worktree() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "1111111111111111111111111111111111111111");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );

        let success = core_create(create_args("feature/demo"), &env).expect("happy path should succeed");

        assert_eq!(success.worktree.branch, "feature/demo");
        assert_eq!(success.worktree.head, "1111111111111111111111111111111111111111");
        assert!(success.worktree.absolute_path.exists());
        assert_eq!(success.prep, None);
        assert_eq!(success.spawned_session, None);
    }

    #[test]
    fn name_only_first_use_returns_confirmation_required_then_trust_allows_retry() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "2222222222222222222222222222222222222222");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let mut env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );
        env.confirmation_mode = McpConfirmationMode::FirstUse;

        let err = core_create(create_args("feature/demo"), &env).expect_err("first-use should require confirmation");
        assert_eq!(err.code(), "confirmation-required");
        let pending_actions = pending.list_for_session("session-1");
        assert_eq!(pending_actions.len(), 1);
        let pending_action = &pending_actions[0];
        trust.record(
            "session-1",
            McpToolName::CreateWorktree,
            pending_action.args_fingerprint,
            pending_action.summary.clone(),
            None,
        );
        assert!(pending.deny(&pending_action.id));

        let success = core_create(create_args("feature/demo"), &env).expect("trusted retry should auto-approve");
        assert_eq!(success.worktree.branch, "feature/demo");
        assert!(pending.list_for_session("session-1").is_empty());
    }

    #[test]
    fn prep_from_config_pending_action_preserves_full_payload_and_approved_retry_runs_prep() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "3333333333333333333333333333333333333333");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );
        let args = parse_args(json!({
            "name": "feature/demo",
            "prepFromConfig": true,
            "optionalPrompt": "hello\nworld"
        }));

        let err = core_create(args.clone(), &env).expect_err("prep requests should require confirmation");
        assert_eq!(err.code(), "confirmation-required");
        let pending_actions = pending.list_for_session("session-1");
        assert_eq!(pending_actions.len(), 1);
        assert_eq!(pending_actions[0].payload, serde_json::to_value(&args).expect("serialize args payload"));

        let token = pending.approve(&pending_actions[0].id).expect("approve should mint a token");
        let mut approved_args = args;
        approved_args.confirmation_token = Some(token.token);
        let success = core_create(approved_args, &env).expect("approved prep call should succeed");

        assert_eq!(prep.calls.lock().expect("prep calls").len(), 1);
        assert_eq!(success.prep.as_ref().map(|prep| prep.id.as_str()), Some("prep-1"));
    }

    #[test]
    fn spawn_sibling_confirmation_flow_records_parent_session() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "4444444444444444444444444444444444444444");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );
        let args = parse_args(json!({
            "name": "feature/demo",
            "spawnSibling": {
                "tool": "claude",
                "initialPrompt": "Investigate issue 183"
            }
        }));

        let err = core_create(args.clone(), &env).expect_err("spawn requests should require confirmation");
        assert_eq!(err.code(), "confirmation-required");
        let pending_action = pending.list_for_session("session-1").pop().expect("pending action");
        let token = pending.approve(&pending_action.id).expect("approve should mint a token");

        let mut approved_args = args;
        approved_args.confirmation_token = Some(token.token);
        let success = core_create(approved_args, &env).expect("approved spawn should succeed");

        assert_eq!(
            success.spawned_session.as_ref().map(|spawned| spawned.session_id.as_str()),
            Some("child-session")
        );
        let calls = spawn.calls.lock().expect("spawn calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].parent_session_id, "session-1");
        assert_eq!(calls[0].initial_prompt.as_deref(), Some("Investigate issue 183"));
    }

    #[test]
    fn prep_failed_stops_before_spawn() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "5555555555555555555555555555555555555555");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = TestPrepRunner {
            calls: Mutex::new(Vec::new()),
            result: TestPrepResult::Error(CoreError::PrepFailed {
                message: "prep failed: command exited with 1. Inspect 'worktree-prep-logs\\prep-fail.log'".to_owned(),
                log_relative_path: "worktree-prep-logs\\prep-fail.log".to_owned(),
            }),
        };
        let spawn = default_spawn_hook();
        let env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );
        let args = parse_args(json!({
            "name": "feature/demo",
            "prepFromConfig": true,
            "spawnSibling": { "tool": "claude" }
        }));

        let pending_action = match core_create(args.clone(), &env).expect_err("prep+spawn should require confirmation") {
            CoreError::ConfirmationRequired { pending_id, .. } => pending_id,
            other => panic!("expected confirmation required, got {other:?}"),
        };
        let token = pending.approve(&pending_action).expect("approve should mint a token");
        let mut approved_args = args;
        approved_args.confirmation_token = Some(token.token);
        let err = core_create(approved_args, &env).expect_err("prep failure should abort before spawn");

        assert_eq!(err.code(), "prep-failed");
        assert!(spawn.calls.lock().expect("spawn calls").is_empty());
    }

    #[test]
    fn name_in_use_returns_suggestions() {
        let workspace = TempDir::new().expect("workspace");
        fs::create_dir_all(target_worktree_path(workspace.path(), "feature/demo")).expect("existing worktree path");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "6666666666666666666666666666666666666666");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );

        let err = core_create(create_args("feature/demo"), &env).expect_err("existing path should fail");
        match err {
            CoreError::NameInUse { suggested_alternatives, .. } => {
                assert_eq!(suggested_alternatives, vec!["feature/demo-2", "feature/demo-3", "feature/demo-4"])
            }
            other => panic!("expected name-in-use, got {other:?}"),
        }
    }

    #[test]
    fn lineage_depth_forces_confirmation_even_when_mode_is_never() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "7777777777777777777777777777777777777777");
        let lineage = TestLineageTracker {
            depth: 1,
            child_count: 0,
            recorded: Mutex::new(Vec::new()),
        };
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let env = base_env(
            "session-1",
            "feature-child",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );

        let err = core_create(create_args("feature/demo"), &env).expect_err("lineage depth should force confirmation");
        assert_eq!(err.code(), "confirmation-required");
    }

    #[test]
    fn confirmation_token_replay_returns_invalid_confirmation_on_second_use() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "8888888888888888888888888888888888888888");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let mut env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );
        env.confirmation_mode = McpConfirmationMode::Always;

        let pending_id = match core_create(create_args("feature/demo"), &env).expect_err("always should require confirmation") {
            CoreError::ConfirmationRequired { pending_id, .. } => pending_id,
            other => panic!("expected confirmation required, got {other:?}"),
        };
        let token = pending.approve(&pending_id).expect("approve should mint a token");

        let mut approved_args = create_args("feature/demo");
        approved_args.confirmation_token = Some(token.token.clone());
        core_create(approved_args.clone(), &env).expect("first token use should succeed");
        let replay_err = core_create(approved_args, &env).expect_err("second token use should fail");
        assert_eq!(replay_err.code(), "invalid-confirmation");
    }

    #[test]
    fn expired_confirmation_token_returns_confirmation_expired() {
        let workspace = TempDir::new().expect("workspace");
        let confirmation = ExpiredConfirmationGate {
            pending_id: "pending-expired".to_owned(),
        };
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "9999999999999999999999999999999999999999");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let mut env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );
        env.confirmation_mode = McpConfirmationMode::Always;

        let mut args = create_args("feature/demo");
        assert_eq!(
            core_create(args.clone(), &env)
                .expect_err("first call should require confirmation")
                .code(),
            "confirmation-required"
        );
        args.confirmation_token = Some("expired-token".to_owned());
        let err = core_create(args, &env).expect_err("expired token should fail");
        assert_eq!(err.code(), "confirmation-expired");
    }

    #[test]
    fn drifted_args_return_confirmation_stale_without_consuming_token() {
        let workspace = TempDir::new().expect("workspace");
        let pending = PendingMcpActionRegistry::new();
        let confirmation = registry_confirmation_gate(&pending);
        let trust = new_trust_store();
        let (_audit_dir, audit) = new_audit_log();
        let git = StubGitRunner::default().with_default_branch("main", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let lineage = TestLineageTracker::default();
        let creator = TestWorktreeCreator::default();
        let prep = default_prep_runner();
        let spawn = default_spawn_hook();
        let mut env = base_env(
            "session-1",
            "feature-parent",
            workspace.path(),
            &git,
            &confirmation,
            &trust,
            &audit,
            &lineage,
            &creator,
            &prep,
            &spawn,
        );
        env.confirmation_mode = McpConfirmationMode::Always;

        let original_args = parse_args(json!({
            "name": "feature/demo",
            "spawnSibling": { "tool": "claude", "initialPrompt": "first" }
        }));
        let pending_id = match core_create(original_args.clone(), &env).expect_err("first call should require confirmation") {
            CoreError::ConfirmationRequired { pending_id, .. } => pending_id,
            other => panic!("expected confirmation required, got {other:?}"),
        };
        let token = pending.approve(&pending_id).expect("approve should mint a token");

        let mut drifted_args = parse_args(json!({
            "name": "feature/demo",
            "spawnSibling": { "tool": "claude", "initialPrompt": "second" }
        }));
        drifted_args.confirmation_token = Some(token.token.clone());
        let drift_err = core_create(drifted_args, &env).expect_err("drifted args should fail");
        assert_eq!(drift_err.code(), "confirmation-stale");

        let mut original_retry = original_args;
        original_retry.confirmation_token = Some(token.token);
        core_create(original_retry, &env).expect("matching args should still succeed after stale attempt");
    }

    #[test]
    fn invoke_returns_workspace_unbound_when_context_is_unbound() {
        let store_root = TempDir::new().expect("store root");
        let store = ConfigStore::open(store_root.path()).expect("config store");
        let sink = PtySink::new(Arc::new(|_, _| {}), Arc::new(|_, _, _, _| {}), Arc::new(|_, _| {}));
        let pool = Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)));
        let ctx = Arc::new(commands::AppContext::new(
            pool,
            store,
            sink,
            Arc::new(crate::git::RealGitRunner),
            Arc::new(|_: SessionMetricsEvent| {}),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| {}),
        ));
        let mcp = Arc::new(McpContext::new(ctx, McpContextConfig::default(), store_root.path().join("mcp")).expect("mcp context"));
        let registry = McpSessionRegistry::new(mcp);

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let err = rt
            .block_on(invoke(&registry, "session-1", json!({ "name": "feature/demo" })))
            .expect_err("unbound invoke");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::WorkspaceUnbound);
    }
}
