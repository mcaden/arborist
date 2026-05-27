//! `merge_main_into_worktrees` MCP tool.
//!
//! Implements the Phase 3 merge flow with a dry-run preview, confirmation-token replay
//! protection, per-workspace single-flight, and per-worktree conflict cleanup.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use crate::compose::validate_ref_name;
use crate::git::{git_command_mcp_mut, git_command_mcp_ro, GitRunner, WorktreeGitStatusSummary};
use crate::mcp::audit::AuditEntryInput;
use crate::mcp::confirm::{fingerprint_args, ConsumeError, PendingMcpActionRegistry};
use crate::mcp::context::McpContext;
use crate::mcp::error::McpInternalError;
use crate::mcp::ipc::McpSessionRegistry;
use crate::mcp::types::{McpAuditDecision, McpToolDescriptor, McpToolName};
use crate::types::{Session, SessionId, SessionStatus, WorktreeInfo};

const DEFAULT_SOURCE_BRANCH: &str = "main";
const MAX_EXCLUDE_PATHS: usize = 100;
const MAX_CANDIDATES: usize = 100;
const SUMMARY_LIMIT: usize = 200;

static WORKSPACE_IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MergeMainArgs {
    #[serde(default = "default_source_branch")]
    source_branch: String,
    #[serde(default = "default_true")]
    dry_run: bool,
    #[serde(default)]
    confirmation_token: Option<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    #[serde(default)]
    strategy: MergeStrategy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
enum MergeStrategy {
    #[default]
    FfOnly,
    Merge,
    Rebase,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CandidateEntry {
    relative_path: String,
    branch: String,
    head_oid: String,
    ahead: u32,
    behind: u32,
    would_fast_forward: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SkippedEntry {
    relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ahead: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    behind: Option<u32>,
    skip_reason: String,
    reason_human: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct MergedEntry {
    relative_path: String,
    branch: String,
    strategy: MergeStrategy,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ConflictEntry {
    relative_path: String,
    branch: String,
    strategy: MergeStrategy,
    files: Vec<String>,
    file_count: usize,
    aborted: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ErrorEntry {
    relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmationFingerprint {
    source_branch: String,
    source_oid: String,
    strategy: MergeStrategy,
    candidates: Vec<CandidateEntry>,
    truncated: bool,
}

#[derive(Debug, Clone)]
struct InspectionState {
    source_branch: String,
    source_ref: String,
    source_oid: String,
    strategy: MergeStrategy,
    candidates: Vec<CandidateEntry>,
    would_fast_forward: Vec<CandidateEntry>,
    would_merge: Vec<CandidateEntry>,
    skipped: Vec<SkippedEntry>,
    stale_data: bool,
    truncated: bool,
    as_of: String,
}

impl InspectionState {
    fn fingerprint(&self) -> Result<[u8; 32], McpInternalError> {
        let payload = ConfirmationFingerprint {
            source_branch: self.source_branch.clone(),
            source_oid: self.source_oid.clone(),
            strategy: self.strategy,
            candidates: self.candidates.clone(),
            truncated: self.truncated,
        };
        Ok(fingerprint_args(&canonical_json_string(&payload)?))
    }

    fn dry_run_response(&self) -> Value {
        json!({
            "dryRun": true,
            "sourceBranch": self.source_branch,
            "strategy": self.strategy,
            "candidates": self.candidates,
            "wouldFastForward": self.would_fast_forward,
            "wouldMerge": self.would_merge,
            "skipped": self.skipped,
            "staleData": self.stale_data,
            "asOf": self.as_of,
            "truncated": self.truncated,
            "summary": {
                "candidates": self.candidates.len(),
                "wouldFastForward": self.would_fast_forward.len(),
                "wouldMerge": self.would_merge.len(),
                "skipped": self.skipped.len(),
            }
        })
    }

    fn pending_payload(&self, args: &MergeMainArgs) -> Value {
        json!({
            "args": {
                "sourceBranch": args.source_branch,
                "dryRun": false,
                "excludePaths": args.exclude_paths,
                "strategy": args.strategy,
            },
            "preview": self.dry_run_response(),
        })
    }

    fn confirmation_summary(&self) -> String {
        let action = match self.strategy {
            MergeStrategy::FfOnly => "fast-forward merge",
            MergeStrategy::Merge => "merge",
            MergeStrategy::Rebase => "rebase",
        };
        truncate_summary(format!(
            "Apply {action} of origin/{} into {} worktree(s); {} skipped.",
            self.source_branch,
            self.execution_candidates().len(),
            self.skipped.len()
        ))
    }

    fn execution_candidates(&self) -> &[CandidateEntry] {
        match self.strategy {
            MergeStrategy::FfOnly => &self.would_fast_forward,
            MergeStrategy::Merge | MergeStrategy::Rebase => &self.candidates,
        }
    }

    fn audit_summary(&self) -> String {
        truncate_summary(format!(
            "merge_main_into_worktrees source={} strategy={} candidates={} ff={} merge={} skipped={}",
            self.source_branch,
            strategy_name(self.strategy),
            self.candidates.len(),
            self.would_fast_forward.len(),
            self.would_merge.len(),
            self.skipped.len()
        ))
    }

    fn audit_result_preview(&self) -> Value {
        json!({
            "dryRun": true,
            "sourceBranch": self.source_branch,
            "strategy": self.strategy,
            "candidates": self.candidates.len(),
            "wouldFastForward": self.would_fast_forward.len(),
            "wouldMerge": self.would_merge.len(),
            "skipped": self.skipped.len(),
            "staleData": self.stale_data,
            "truncated": self.truncated,
        })
    }
}

struct ToolRuntime {
    workspace_root: Option<PathBuf>,
    sessions: Arc<Mutex<Vec<Session>>>,
    git: Arc<dyn MergeMainGit>,
    executor: Arc<dyn MergeMainExecutor>,
    confirm: Arc<PendingMcpActionRegistry>,
    audit: Arc<crate::mcp::audit::AuditLog>,
}

impl ToolRuntime {
    fn sessions_snapshot(&self) -> Vec<Session> {
        match self.sessions.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

trait MergeMainGit: Send + Sync {
    fn fetch_source_branch(&self, workspace_root: &Path, source_branch: &str) -> Result<(), McpInternalError>;
    fn list_worktrees(&self, workspace_root: &Path) -> Result<Vec<WorktreeInfo>, McpInternalError>;
    fn resolve_ref(&self, repo_root: &Path, ref_expr: &str) -> Result<String, McpInternalError>;
    fn git_status_summary(&self, worktree_path: &Path) -> Result<WorktreeGitStatusSummary, McpInternalError>;
    fn is_ancestor(&self, repo_root: &Path, ancestor: &str, descendant: &str) -> Result<bool, McpInternalError>;
    fn ahead_behind(&self, worktree_path: &Path, source_ref: &str) -> Result<(u32, u32), McpInternalError>;
}

trait MergeMainExecutor: Send + Sync {
    fn ff_only(&self, worktree_path: &Path, source_ref: &str) -> Result<CommandOutcome, McpInternalError>;
    fn merge_no_ff(&self, worktree_path: &Path, source_ref: &str) -> Result<CommandOutcome, McpInternalError>;
    fn rebase(&self, worktree_path: &Path, source_ref: &str) -> Result<CommandOutcome, McpInternalError>;
    fn abort_merge(&self, worktree_path: &Path) -> Result<(), McpInternalError>;
    fn abort_rebase(&self, worktree_path: &Path) -> Result<(), McpInternalError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandOutcome {
    Success,
    Conflict { files: Vec<String> },
    Error { message: String },
}

struct RealMergeMainGit {
    runner: Arc<dyn GitRunner>,
}

impl MergeMainGit for RealMergeMainGit {
    fn fetch_source_branch(&self, workspace_root: &Path, source_branch: &str) -> Result<(), McpInternalError> {
        let mut cmd = git_command_mcp_ro(workspace_root);
        cmd.args(["fetch", "--no-tags", "origin", source_branch]);
        let output = run_git_output(cmd, "git fetch --no-tags origin <source>")?;
        if output.status.success() {
            Ok(())
        } else {
            Err(McpInternalError::Internal {
                message: format!("git fetch --no-tags origin {source_branch}: {}", output_message(&output)),
            })
        }
    }

    fn list_worktrees(&self, workspace_root: &Path) -> Result<Vec<WorktreeInfo>, McpInternalError> {
        self.runner.list_worktrees(workspace_root).map_err(McpInternalError::from)
    }

    fn resolve_ref(&self, repo_root: &Path, ref_expr: &str) -> Result<String, McpInternalError> {
        self.runner.rev_parse_verify(repo_root, ref_expr).map_err(McpInternalError::from)
    }

    fn git_status_summary(&self, worktree_path: &Path) -> Result<WorktreeGitStatusSummary, McpInternalError> {
        self.runner.git_status_mcp(worktree_path).map_err(McpInternalError::from)
    }

    fn is_ancestor(&self, repo_root: &Path, ancestor: &str, descendant: &str) -> Result<bool, McpInternalError> {
        let mut cmd = git_command_mcp_ro(repo_root);
        cmd.args(["merge-base", "--is-ancestor", ancestor, descendant]);
        let output = run_git_output(cmd, "git merge-base --is-ancestor")?;
        if output.status.success() {
            return Ok(true);
        }
        if output.status.code() == Some(1) && output_message(&output).is_empty() {
            return Ok(false);
        }
        Err(McpInternalError::Internal {
            message: format!("git merge-base --is-ancestor: {}", output_message(&output)),
        })
    }

    fn ahead_behind(&self, worktree_path: &Path, source_ref: &str) -> Result<(u32, u32), McpInternalError> {
        let range = format!("{source_ref}...HEAD");
        let mut cmd = git_command_mcp_ro(worktree_path);
        cmd.args(["rev-list", "--left-right", "--count", &range]);
        let output = run_git_output(cmd, "git rev-list --left-right --count")?;
        if !output.status.success() {
            return Err(McpInternalError::Internal {
                message: format!("git rev-list --left-right --count: {}", output_message(&output)),
            });
        }
        parse_ahead_behind(&output).ok_or_else(|| McpInternalError::Internal {
            message: "git rev-list --left-right --count returned an unreadable count".to_owned(),
        })
    }
}

#[derive(Default)]
struct RealMergeMainExecutor;

impl MergeMainExecutor for RealMergeMainExecutor {
    fn ff_only(&self, worktree_path: &Path, source_ref: &str) -> Result<CommandOutcome, McpInternalError> {
        let mut cmd = git_command_mcp_mut(worktree_path);
        cmd.args(["merge", "--ff-only", source_ref]);
        let output = run_git_output(cmd, "git merge --ff-only")?;
        if output.status.success() {
            Ok(CommandOutcome::Success)
        } else {
            Ok(CommandOutcome::Error {
                message: output_message(&output),
            })
        }
    }

    fn merge_no_ff(&self, worktree_path: &Path, source_ref: &str) -> Result<CommandOutcome, McpInternalError> {
        let mut cmd = git_command_mcp_mut(worktree_path);
        cmd.args(["merge", "--no-ff", source_ref]);
        let output = run_git_output(cmd, "git merge --no-ff")?;
        if output.status.success() {
            return Ok(CommandOutcome::Success);
        }
        let files = conflicted_files(worktree_path)?;
        if files.is_empty() {
            Ok(CommandOutcome::Error {
                message: output_message(&output),
            })
        } else {
            Ok(CommandOutcome::Conflict { files })
        }
    }

    fn rebase(&self, worktree_path: &Path, source_ref: &str) -> Result<CommandOutcome, McpInternalError> {
        let mut cmd = git_command_mcp_mut(worktree_path);
        cmd.args(["rebase", source_ref]);
        let output = run_git_output(cmd, "git rebase")?;
        if output.status.success() {
            return Ok(CommandOutcome::Success);
        }
        let files = conflicted_files(worktree_path)?;
        if files.is_empty() {
            Ok(CommandOutcome::Error {
                message: output_message(&output),
            })
        } else {
            Ok(CommandOutcome::Conflict { files })
        }
    }

    fn abort_merge(&self, worktree_path: &Path) -> Result<(), McpInternalError> {
        run_abort(worktree_path, &["merge", "--abort"], &["There is no merge to abort"])
    }

    fn abort_rebase(&self, worktree_path: &Path) -> Result<(), McpInternalError> {
        run_abort(
            worktree_path,
            &["rebase", "--abort"],
            &["no rebase in progress", "No rebase in progress", "No rebase in progress?"],
        )
    }
}

struct WorkspaceGuard {
    key: String,
}

impl WorkspaceGuard {
    fn acquire(workspace_root: &Path) -> Result<Self, McpInternalError> {
        let key = normalize_path_key(workspace_root);
        let mut guard = match WORKSPACE_IN_FLIGHT.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !guard.insert(key.clone()) {
            return Err(McpInternalError::Busy {
                message: "merge_main_into_worktrees is already running for this workspace".to_owned(),
            });
        }
        Ok(Self { key })
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = WORKSPACE_IN_FLIGHT.lock() {
            guard.remove(&self.key);
        }
    }
}

#[must_use]
pub fn descriptor() -> McpToolDescriptor {
    McpToolDescriptor {
        name: "merge_main_into_worktrees".to_owned(),
        description: "Preview or apply merges from origin/<sourceBranch> into eligible worktrees; defaults to dry-run fast-forward only.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "sourceBranch": {
                    "type": "string",
                    "description": "Unqualified branch name to merge from. Defaults to 'main'."
                },
                "dryRun": {
                    "type": "boolean",
                    "default": true
                },
                "confirmationToken": {
                    "type": "string",
                    "description": "Required when dryRun is false."
                },
                "excludePaths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": MAX_EXCLUDE_PATHS,
                    "default": []
                },
                "strategy": {
                    "type": "string",
                    "enum": ["ff-only", "merge", "rebase"],
                    "default": "ff-only"
                }
            },
            "additionalProperties": false
        }),
    }
}

pub async fn invoke(registry: &McpSessionRegistry, session_id: &str, args: Value) -> Result<Value, McpInternalError> {
    let context = registry_context(registry);
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let runtime = build_runtime(&context);
        invoke_with_runtime(&runtime, &session_id, args)
    })
    .await
    .map_err(|err| McpInternalError::Internal {
        message: format!("merge_main_into_worktrees task failed: {err}"),
    })?
}

fn invoke_with_runtime(runtime: &ToolRuntime, session_id: &str, args: Value) -> Result<Value, McpInternalError> {
    let started_at = Instant::now();
    let args = parse_args(args)?;
    let source_branch = validate_ref_name(&args.source_branch).map_err(|message| McpInternalError::InvalidArg {
        message: format!("invalid sourceBranch: {message}"),
    })?;

    let Some(workspace_root) = runtime.workspace_root.clone() else {
        return Err(McpInternalError::WorkspaceUnbound {
            message: "Open a workspace in Arborist before retrying merge_main_into_worktrees".to_owned(),
        });
    };

    let _busy_guard = WorkspaceGuard::acquire(&workspace_root)?;
    let sessions = runtime.sessions_snapshot();
    let current_session = current_session(&sessions, session_id)?;
    let inspection = inspect_worktrees(runtime, &workspace_root, &sessions, current_session, &args, &source_branch)?;
    let audit_summary = inspection.audit_summary();

    if args.dry_run {
        let response = inspection.dry_run_response();
        append_audit_best_effort(
            runtime,
            current_session,
            McpAuditDecision::NotRequired,
            &audit_summary,
            inspection.audit_result_preview(),
            None,
            started_at,
        );
        return Ok(response);
    }

    if inspection.stale_data {
        append_audit_best_effort(
            runtime,
            current_session,
            McpAuditDecision::Stale,
            &audit_summary,
            inspection.audit_result_preview(),
            None,
            started_at,
        );
        return Err(McpInternalError::StaleRemoteData {
            message: format!(
                "failed to refresh origin/{} before execution; retry after fetching succeeds",
                inspection.source_branch
            ),
        });
    }

    let fingerprint = inspection.fingerprint()?;
    let confirmation_summary = inspection.confirmation_summary();

    let Some(token) = args.confirmation_token.as_deref() else {
        if let Err(err) = runtime.confirm.create(
            session_id.to_owned(),
            McpToolName::MergeMainIntoWorktrees,
            confirmation_summary.clone(),
            fingerprint,
            inspection.pending_payload(&args),
        ) {
            append_audit_best_effort(
                runtime,
                current_session,
                McpAuditDecision::Pending,
                &audit_summary,
                inspection.audit_result_preview(),
                None,
                started_at,
            );
            return Err(err);
        }
        append_audit_best_effort(
            runtime,
            current_session,
            McpAuditDecision::Pending,
            &audit_summary,
            inspection.audit_result_preview(),
            None,
            started_at,
        );
        return Err(McpInternalError::ConfirmationRequired {
            message: format!(
                "merge_main_into_worktrees requires confirmation before applying strategy '{}'",
                strategy_name(inspection.strategy)
            ),
        });
    };

    match runtime.confirm.try_consume(token, &fingerprint) {
        Ok(_) => {}
        Err(ConsumeError::Unknown) => {
            return Err(McpInternalError::InvalidConfirmation {
                message: "confirmation token is unknown or already consumed".to_owned(),
            })
        }
        Err(ConsumeError::Expired) => {
            append_audit_best_effort(
                runtime,
                current_session,
                McpAuditDecision::Expired,
                &audit_summary,
                inspection.audit_result_preview(),
                Some(token),
                started_at,
            );
            return Err(McpInternalError::ConfirmationExpired {
                message: "confirmation token expired; re-run the preview and request approval again".to_owned(),
            });
        }
        Err(ConsumeError::FingerprintMismatch) => {
            append_audit_best_effort(
                runtime,
                current_session,
                McpAuditDecision::Stale,
                &audit_summary,
                inspection.audit_result_preview(),
                Some(token),
                started_at,
            );
            return Err(McpInternalError::ConfirmationStale {
                message: "workspace state changed since confirmation was requested; re-run the preview".to_owned(),
            });
        }
    }

    let response = execute_plan(runtime, &inspection);
    append_audit_best_effort(
        runtime,
        current_session,
        McpAuditDecision::Approved,
        &audit_summary,
        audit_execution_result(&inspection, &response),
        Some(token),
        started_at,
    );
    Ok(response)
}

fn parse_args(args: Value) -> Result<MergeMainArgs, McpInternalError> {
    let parsed: MergeMainArgs = serde_json::from_value(args).map_err(|err| McpInternalError::InvalidArg {
        message: format!("arguments must match the merge_main_into_worktrees schema: {err}"),
    })?;
    if parsed.exclude_paths.len() > MAX_EXCLUDE_PATHS {
        return Err(McpInternalError::InvalidArg {
            message: format!("excludePaths must contain at most {MAX_EXCLUDE_PATHS} entries"),
        });
    }
    Ok(parsed)
}

fn inspect_worktrees(
    runtime: &ToolRuntime,
    workspace_root: &Path,
    sessions: &[Session],
    current_session: &Session,
    args: &MergeMainArgs,
    source_branch: &str,
) -> Result<InspectionState, McpInternalError> {
    let mut skipped = Vec::new();
    let mut stale_data = false;
    if runtime.git.fetch_source_branch(workspace_root, source_branch).is_err() {
        stale_data = true;
    }

    let source_ref = format!("origin/{source_branch}");
    let source_oid = runtime.git.resolve_ref(workspace_root, &format!("refs/remotes/{source_ref}"))?;
    let worktrees = runtime.git.list_worktrees(workspace_root)?;
    let exclude_paths: HashSet<String> = args.exclude_paths.iter().map(|path| normalize_text_key(path)).collect();
    let current_path_key = normalize_path_key(&current_session.worktree_path);
    let current_session_id = current_session.id;
    let mut candidates = Vec::new();

    for worktree in worktrees {
        let relative_path = worktree_relative_path(workspace_root, &worktree.path);
        let branch = worktree.branch.clone();
        let path_key = normalize_path_key(&worktree.path);

        if worktree.is_main {
            skipped.push(make_skipped(
                &relative_path,
                branch,
                None,
                None,
                None,
                "primary-clone",
                "Skipped because this is the primary clone",
            ));
            continue;
        }

        if branch.is_none() {
            skipped.push(make_skipped(
                &relative_path,
                None,
                None,
                None,
                None,
                "detached",
                "Skipped because the worktree is detached",
            ));
            continue;
        }

        if exclude_paths.contains(&normalize_text_key(&relative_path)) || exclude_paths.contains(&path_key) {
            skipped.push(make_skipped(
                &relative_path,
                branch,
                None,
                None,
                None,
                "excluded",
                "Skipped because excludePaths matched this worktree",
            ));
            continue;
        }

        if path_key == current_path_key {
            skipped.push(make_skipped(
                &relative_path,
                branch,
                None,
                None,
                None,
                "own-worktree-refused",
                "Skipped because this is the calling session's worktree",
            ));
            continue;
        }

        if branch.as_deref() == Some(source_branch) {
            skipped.push(make_skipped(
                &relative_path,
                branch,
                None,
                None,
                None,
                "source-branch",
                "Skipped because this worktree is checked out on the source branch",
            ));
            continue;
        }

        if worktree.is_locked {
            skipped.push(make_skipped(
                &relative_path,
                branch,
                None,
                None,
                None,
                "locked",
                "Skipped because the worktree is locked",
            ));
            continue;
        }

        if has_other_active_session(sessions, current_session_id, &worktree.path) {
            skipped.push(make_skipped(
                &relative_path,
                branch,
                None,
                None,
                None,
                "active-session",
                "Skipped because another AI session is active in this worktree",
            ));
            continue;
        }

        let status = match runtime.git.git_status_summary(&worktree.path) {
            Ok(status) => status,
            Err(err) => {
                skipped.push(make_skipped(
                    &relative_path,
                    branch,
                    None,
                    None,
                    None,
                    "status-unavailable",
                    &format!("Skipped because git status could not be read: {err}"),
                ));
                continue;
            }
        };
        if status.dirty {
            skipped.push(make_skipped(
                &relative_path,
                branch,
                None,
                None,
                None,
                "uncommitted-changes",
                "Skipped because there are uncommitted changes",
            ));
            continue;
        }

        let head_oid = match runtime.git.resolve_ref(&worktree.path, "HEAD") {
            Ok(head_oid) => head_oid,
            Err(err) => {
                skipped.push(make_skipped(
                    &relative_path,
                    branch,
                    None,
                    None,
                    None,
                    "metadata-unavailable",
                    &format!("Skipped because HEAD could not be resolved: {err}"),
                ));
                continue;
            }
        };

        let (ahead, behind) = match runtime.git.ahead_behind(&worktree.path, &source_ref) {
            Ok(counts) => counts,
            Err(err) => {
                skipped.push(make_skipped(
                    &relative_path,
                    branch,
                    Some(head_oid),
                    None,
                    None,
                    "metadata-unavailable",
                    &format!("Skipped because ahead/behind counts could not be computed: {err}"),
                ));
                continue;
            }
        };

        let would_fast_forward = match runtime.git.is_ancestor(&worktree.path, "HEAD", &source_ref) {
            Ok(value) => value,
            Err(err) => {
                skipped.push(make_skipped(
                    &relative_path,
                    branch,
                    Some(head_oid),
                    Some(ahead),
                    Some(behind),
                    "metadata-unavailable",
                    &format!("Skipped because fast-forward status could not be computed: {err}"),
                ));
                continue;
            }
        };

        candidates.push(CandidateEntry {
            relative_path,
            branch: worktree.branch.unwrap_or_default(),
            head_oid,
            ahead,
            behind,
            would_fast_forward,
        });
    }

    let truncated = candidates.len() > MAX_CANDIDATES;
    candidates.truncate(MAX_CANDIDATES);

    let mut would_fast_forward = Vec::new();
    let mut would_merge = Vec::new();
    for candidate in &candidates {
        if candidate.would_fast_forward {
            would_fast_forward.push(candidate.clone());
        } else if args.strategy == MergeStrategy::FfOnly {
            skipped.push(make_skipped(
                &candidate.relative_path,
                Some(candidate.branch.clone()),
                Some(candidate.head_oid.clone()),
                Some(candidate.ahead),
                Some(candidate.behind),
                "non-fast-forward",
                "Skipped because this worktree cannot be updated with a fast-forward merge",
            ));
        } else {
            would_merge.push(candidate.clone());
        }
    }

    Ok(InspectionState {
        source_branch: source_branch.to_owned(),
        source_ref,
        source_oid,
        strategy: args.strategy,
        candidates,
        would_fast_forward,
        would_merge,
        skipped,
        stale_data,
        truncated,
        as_of: now_rfc3339(),
    })
}

fn execute_plan(runtime: &ToolRuntime, inspection: &InspectionState) -> Value {
    let mut merged = Vec::new();
    let mut conflicts = Vec::new();
    let mut errors = Vec::new();

    for candidate in inspection.execution_candidates() {
        let worktree_path = resolve_worktree_path(runtime.workspace_root.as_deref(), &candidate.relative_path);
        let outcome = match inspection.strategy {
            MergeStrategy::FfOnly => runtime.executor.ff_only(&worktree_path, &inspection.source_ref),
            MergeStrategy::Merge => runtime.executor.merge_no_ff(&worktree_path, &inspection.source_ref),
            MergeStrategy::Rebase => runtime.executor.rebase(&worktree_path, &inspection.source_ref),
        };

        match outcome {
            Ok(CommandOutcome::Success) => merged.push(MergedEntry {
                relative_path: candidate.relative_path.clone(),
                branch: candidate.branch.clone(),
                strategy: inspection.strategy,
            }),
            Ok(CommandOutcome::Conflict { files }) => match abort_after_conflict(runtime, inspection.strategy, &worktree_path) {
                Ok(()) => conflicts.push(ConflictEntry {
                    relative_path: candidate.relative_path.clone(),
                    branch: candidate.branch.clone(),
                    strategy: inspection.strategy,
                    file_count: files.len(),
                    files,
                    aborted: true,
                }),
                Err(err) => errors.push(ErrorEntry {
                    relative_path: candidate.relative_path.clone(),
                    branch: Some(candidate.branch.clone()),
                    message: format!("conflict detected but cleanup failed: {err}"),
                }),
            },
            Ok(CommandOutcome::Error { message }) => errors.push(ErrorEntry {
                relative_path: candidate.relative_path.clone(),
                branch: Some(candidate.branch.clone()),
                message,
            }),
            Err(err) => errors.push(ErrorEntry {
                relative_path: candidate.relative_path.clone(),
                branch: Some(candidate.branch.clone()),
                message: err.to_string(),
            }),
        }
    }

    json!({
        "dryRun": false,
        "sourceBranch": inspection.source_branch,
        "strategy": inspection.strategy,
        "merged": merged,
        "conflicts": conflicts,
        "errors": errors,
        "skipped": inspection.skipped,
        "staleData": false,
        "asOf": now_rfc3339(),
        "truncated": inspection.truncated,
        "summary": {
            "merged": merged.len(),
            "conflicts": conflicts.len(),
            "errors": errors.len(),
            "skipped": inspection.skipped.len(),
        }
    })
}

fn abort_after_conflict(runtime: &ToolRuntime, strategy: MergeStrategy, worktree_path: &Path) -> Result<(), McpInternalError> {
    match strategy {
        MergeStrategy::FfOnly => Ok(()),
        MergeStrategy::Merge => runtime.executor.abort_merge(worktree_path),
        MergeStrategy::Rebase => runtime.executor.abort_rebase(worktree_path),
    }
}

fn audit_execution_result(inspection: &InspectionState, response: &Value) -> Value {
    json!({
        "dryRun": false,
        "sourceBranch": inspection.source_branch,
        "strategy": inspection.strategy,
        "merged": response.get("merged").and_then(Value::as_array).map_or(0, Vec::len),
        "conflicts": response.get("conflicts").and_then(Value::as_array).map_or(0, Vec::len),
        "errors": response.get("errors").and_then(Value::as_array).map_or(0, Vec::len),
        "skipped": inspection.skipped.len(),
        "truncated": inspection.truncated,
    })
}

fn append_audit_best_effort(
    runtime: &ToolRuntime,
    session: &Session,
    decision: McpAuditDecision,
    args_summary: &str,
    result: Value,
    confirmation_token: Option<&str>,
    started_at: Instant,
) {
    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let entry = AuditEntryInput {
        ts: now_rfc3339(),
        session_id: session.id.to_string(),
        session_label: session.label.clone(),
        tool: McpToolName::MergeMainIntoWorktrees.as_id().to_owned(),
        decision,
        args_summary: truncate_summary(args_summary.to_owned()),
        result,
        duration_ms,
        request_id: Uuid::new_v4().as_simple().to_string(),
        confirmation_token_sha256: confirmation_token.map(confirmation_token_digest),
        audit_id: Uuid::new_v4().as_simple().to_string(),
    };
    if let Err(err) = runtime.audit.append_destructive(entry) {
        warn!(%err, "failed to append merge_main_into_worktrees audit row");
    }
}

fn build_runtime(context: &Arc<McpContext>) -> ToolRuntime {
    let (workspace_root, store) = {
        let workspace = match context.app.workspace.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        (workspace.workspace_root.clone(), workspace.store.clone())
    };

    let sessions = store
        .as_ref()
        .map(|store| store.load_sessions().into_values().collect::<Vec<_>>())
        .unwrap_or_default();

    ToolRuntime {
        workspace_root,
        sessions: Arc::new(Mutex::new(sessions)),
        git: Arc::new(RealMergeMainGit {
            runner: Arc::clone(&context.app.git_runner),
        }),
        executor: Arc::new(RealMergeMainExecutor),
        confirm: Arc::clone(&context.confirm),
        audit: Arc::clone(&context.audit),
    }
}

fn current_session<'a>(sessions: &'a [Session], session_id: &str) -> Result<&'a Session, McpInternalError> {
    let session_id = parse_session_id(session_id)?;
    sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| McpInternalError::Internal {
            message: format!("session '{session_id}' is not registered in the current workspace"),
        })
}

fn parse_session_id(session_id: &str) -> Result<SessionId, McpInternalError> {
    uuid::Uuid::parse_str(session_id)
        .map(SessionId)
        .map_err(|err| McpInternalError::Internal {
            message: format!("invalid internal session id '{session_id}': {err}"),
        })
}

fn has_other_active_session(sessions: &[Session], current_session_id: SessionId, worktree_path: &Path) -> bool {
    let worktree_key = normalize_path_key(worktree_path);
    sessions.iter().any(|session| {
        session.id != current_session_id
            && matches!(session.status, SessionStatus::Starting | SessionStatus::Running)
            && normalize_path_key(&session.worktree_path) == worktree_key
    })
}

fn make_skipped(
    relative_path: &str,
    branch: Option<String>,
    head_oid: Option<String>,
    ahead: Option<u32>,
    behind: Option<u32>,
    skip_reason: &str,
    reason_human: &str,
) -> SkippedEntry {
    SkippedEntry {
        relative_path: relative_path.to_owned(),
        branch,
        head_oid,
        ahead,
        behind,
        skip_reason: skip_reason.to_owned(),
        reason_human: reason_human.to_owned(),
    }
}

fn worktree_relative_path(workspace_root: &Path, worktree_path: &Path) -> String {
    match worktree_path.strip_prefix(workspace_root) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().to_string(),
        _ => worktree_path.to_string_lossy().to_string(),
    }
}

fn resolve_worktree_path(workspace_root: Option<&Path>, relative_path: &str) -> PathBuf {
    let path = PathBuf::from(relative_path);
    if path.is_absolute() {
        return path;
    }
    workspace_root.map_or(path.clone(), |root| root.join(path))
}

fn normalize_path_key(path: &Path) -> String {
    normalize_text_key(&path.to_string_lossy())
}

fn normalize_text_key(text: &str) -> String {
    #[cfg(windows)]
    {
        text.replace('/', "\\").to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        text.replace('\\', "/")
    }
}

fn truncate_summary(text: String) -> String {
    let mut truncated = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= SUMMARY_LIMIT {
            truncated.push('…');
            return truncated;
        }
        truncated.push(ch);
    }
    truncated
}

fn confirmation_token_digest(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)[..16].to_owned()
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn default_source_branch() -> String {
    DEFAULT_SOURCE_BRANCH.to_owned()
}

fn default_true() -> bool {
    true
}

fn strategy_name(strategy: MergeStrategy) -> &'static str {
    match strategy {
        MergeStrategy::FfOnly => "ff-only",
        MergeStrategy::Merge => "merge",
        MergeStrategy::Rebase => "rebase",
    }
}

fn run_git_output(mut cmd: Command, context: &str) -> Result<Output, McpInternalError> {
    cmd.output().map_err(|err| McpInternalError::Internal {
        message: format!("{context}: {err}"),
    })
}

fn output_message(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stderr}\n{stdout}"),
    }
}

fn parse_ahead_behind(output: &Output) -> Option<(u32, u32)> {
    let counts = String::from_utf8_lossy(&output.stdout);
    let mut parts = counts.split_whitespace();
    let behind = parts.next()?.parse().ok()?;
    let ahead = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

fn conflicted_files(worktree_path: &Path) -> Result<Vec<String>, McpInternalError> {
    let mut cmd = git_command_mcp_ro(worktree_path);
    cmd.args(["diff", "--name-only", "--diff-filter=U"]);
    let output = run_git_output(cmd, "git diff --name-only --diff-filter=U")?;
    if !output.status.success() {
        return Err(McpInternalError::Internal {
            message: format!("git diff --name-only --diff-filter=U: {}", output_message(&output)),
        });
    }
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_owned()) {
            files.push(trimmed.to_owned());
        }
    }
    Ok(files)
}

fn run_abort(worktree_path: &Path, args: &[&str], tolerated_messages: &[&str]) -> Result<(), McpInternalError> {
    let mut cmd = git_command_mcp_mut(worktree_path);
    cmd.args(args);
    let output = run_git_output(cmd, args.join(" ").as_str())?;
    if output.status.success() {
        return Ok(());
    }
    let message = output_message(&output);
    if tolerated_messages.iter().any(|needle| message.contains(needle)) {
        return Ok(());
    }
    Err(McpInternalError::Internal {
        message: format!("{}: {message}", args.join(" ")),
    })
}

fn canonical_json_string<T: Serialize>(value: &T) -> Result<String, McpInternalError> {
    let value = serde_json::to_value(value).map_err(|err| McpInternalError::Internal {
        message: format!("failed to serialize confirmation fingerprint: {err}"),
    })?;
    let mut out = String::new();
    write_canonical_json(&mut out, &value)?;
    Ok(out)
}

fn write_canonical_json(out: &mut String, value: &Value) -> Result<(), McpInternalError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => out.push_str(&value.to_string()),
        Value::String(value) => {
            let escaped = serde_json::to_string(value).map_err(|err| McpInternalError::Internal {
                message: format!("failed to encode JSON string: {err}"),
            })?;
            out.push_str(&escaped);
        }
        Value::Array(values) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(out, value)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut entries: BTreeMap<&str, &Value> = BTreeMap::new();
            for (key, value) in map {
                entries.insert(key.as_str(), value);
            }
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                let escaped = serde_json::to_string(key).map_err(|err| McpInternalError::Internal {
                    message: format!("failed to encode JSON object key: {err}"),
                })?;
                out.push_str(&escaped);
                out.push(':');
                write_canonical_json(out, value)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn registry_context(registry: &McpSessionRegistry) -> Arc<McpContext> {
    // Safe accessor on `McpSessionRegistry` introduced after this tool's first draft. Previously
    // this helper did an unsafe pointer cast under the assumption that `context` was the first
    // field; field reordering would have been undefined behaviour. Always go through `context()`.
    registry.context()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    use tempfile::TempDir;

    use super::*;
    use crate::commands::AppContext;
    use crate::config_store::ConfigStore;
    use crate::git::{DefaultBranchInfo, DefaultBranchSource, GitError, MergeFromBranchOutcome, MergeTreeOutcome};
    use crate::pty_pool::{PortablePtySpawner, PtyPool, PtySink};
    use crate::types::{SessionMetricsEvent, Tool};

    #[derive(Clone)]
    struct FakeMetadata {
        head_oid: String,
        dirty: bool,
        ahead: u32,
        behind: u32,
        would_fast_forward: bool,
        status_error: Option<String>,
        resolve_error: Option<String>,
        ahead_behind_error: Option<String>,
        ancestor_error: Option<String>,
    }

    impl FakeMetadata {
        fn new(head_oid: &str, ahead: u32, behind: u32, would_fast_forward: bool) -> Self {
            Self {
                head_oid: head_oid.to_owned(),
                dirty: false,
                ahead,
                behind,
                would_fast_forward,
                status_error: None,
                resolve_error: None,
                ahead_behind_error: None,
                ancestor_error: None,
            }
        }
    }

    #[derive(Default)]
    struct FakeGitState {
        worktrees: Vec<WorktreeInfo>,
        metadata: HashMap<PathBuf, FakeMetadata>,
        source_oids: HashMap<String, String>,
        fetch_error: Option<String>,
    }

    #[derive(Default)]
    struct FakeGit {
        state: Mutex<FakeGitState>,
    }

    impl FakeGit {
        fn set_worktrees(&self, worktrees: Vec<WorktreeInfo>) {
            self.state.lock().unwrap().worktrees = worktrees;
        }

        fn set_source_oid(&self, branch: &str, oid: &str) {
            self.state
                .lock()
                .unwrap()
                .source_oids
                .insert(format!("refs/remotes/origin/{branch}"), oid.to_owned());
        }

        fn set_metadata(&self, path: &Path, metadata: FakeMetadata) {
            self.state.lock().unwrap().metadata.insert(path.to_path_buf(), metadata);
        }

        fn update_head_oid(&self, path: &Path, head_oid: &str) {
            if let Some(metadata) = self.state.lock().unwrap().metadata.get_mut(path) {
                metadata.head_oid = head_oid.to_owned();
            }
        }

        fn set_fetch_error(&self, message: &str) {
            self.state.lock().unwrap().fetch_error = Some(message.to_owned());
        }
    }

    impl MergeMainGit for FakeGit {
        fn fetch_source_branch(&self, _: &Path, _: &str) -> Result<(), McpInternalError> {
            match self.state.lock().unwrap().fetch_error.clone() {
                Some(message) => Err(McpInternalError::Internal { message }),
                None => Ok(()),
            }
        }

        fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, McpInternalError> {
            Ok(self.state.lock().unwrap().worktrees.clone())
        }

        fn resolve_ref(&self, repo_root: &Path, ref_expr: &str) -> Result<String, McpInternalError> {
            let state = self.state.lock().unwrap();
            if ref_expr == "HEAD" {
                let metadata = state.metadata.get(repo_root).ok_or_else(|| McpInternalError::Internal {
                    message: format!("missing fake metadata for {}", repo_root.display()),
                })?;
                if let Some(message) = metadata.resolve_error.clone() {
                    return Err(McpInternalError::Internal { message });
                }
                return Ok(metadata.head_oid.clone());
            }
            state.source_oids.get(ref_expr).cloned().ok_or_else(|| McpInternalError::InvalidArg {
                message: format!("ref not found: {ref_expr}"),
            })
        }

        fn git_status_summary(&self, worktree_path: &Path) -> Result<WorktreeGitStatusSummary, McpInternalError> {
            let state = self.state.lock().unwrap();
            let metadata = state.metadata.get(worktree_path).ok_or_else(|| McpInternalError::Internal {
                message: format!("missing fake metadata for {}", worktree_path.display()),
            })?;
            if let Some(message) = metadata.status_error.clone() {
                return Err(McpInternalError::Internal { message });
            }
            Ok(WorktreeGitStatusSummary {
                dirty: metadata.dirty,
                ..WorktreeGitStatusSummary::default()
            })
        }

        fn is_ancestor(&self, repo_root: &Path, _: &str, _: &str) -> Result<bool, McpInternalError> {
            let state = self.state.lock().unwrap();
            let metadata = state.metadata.get(repo_root).ok_or_else(|| McpInternalError::Internal {
                message: format!("missing fake metadata for {}", repo_root.display()),
            })?;
            if let Some(message) = metadata.ancestor_error.clone() {
                return Err(McpInternalError::Internal { message });
            }
            Ok(metadata.would_fast_forward)
        }

        fn ahead_behind(&self, worktree_path: &Path, _: &str) -> Result<(u32, u32), McpInternalError> {
            let state = self.state.lock().unwrap();
            let metadata = state.metadata.get(worktree_path).ok_or_else(|| McpInternalError::Internal {
                message: format!("missing fake metadata for {}", worktree_path.display()),
            })?;
            if let Some(message) = metadata.ahead_behind_error.clone() {
                return Err(McpInternalError::Internal { message });
            }
            Ok((metadata.ahead, metadata.behind))
        }
    }

    #[derive(Default)]
    struct FakeExecutorState {
        ff_only: HashMap<PathBuf, CommandOutcome>,
        merge: HashMap<PathBuf, CommandOutcome>,
        rebase: HashMap<PathBuf, CommandOutcome>,
        merge_abort_calls: Vec<PathBuf>,
        rebase_abort_calls: Vec<PathBuf>,
    }

    #[derive(Default)]
    struct FakeExecutor {
        state: Mutex<FakeExecutorState>,
    }

    impl FakeExecutor {
        fn set_merge_result(&self, path: &Path, outcome: CommandOutcome) {
            self.state.lock().unwrap().merge.insert(path.to_path_buf(), outcome);
        }

        fn set_rebase_result(&self, path: &Path, outcome: CommandOutcome) {
            self.state.lock().unwrap().rebase.insert(path.to_path_buf(), outcome);
        }

        fn merge_abort_calls(&self) -> Vec<PathBuf> {
            self.state.lock().unwrap().merge_abort_calls.clone()
        }

        fn rebase_abort_calls(&self) -> Vec<PathBuf> {
            self.state.lock().unwrap().rebase_abort_calls.clone()
        }
    }

    impl MergeMainExecutor for FakeExecutor {
        fn ff_only(&self, worktree_path: &Path, _: &str) -> Result<CommandOutcome, McpInternalError> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .ff_only
                .get(worktree_path)
                .cloned()
                .unwrap_or(CommandOutcome::Success))
        }

        fn merge_no_ff(&self, worktree_path: &Path, _: &str) -> Result<CommandOutcome, McpInternalError> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .merge
                .get(worktree_path)
                .cloned()
                .unwrap_or(CommandOutcome::Success))
        }

        fn rebase(&self, worktree_path: &Path, _: &str) -> Result<CommandOutcome, McpInternalError> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .rebase
                .get(worktree_path)
                .cloned()
                .unwrap_or(CommandOutcome::Success))
        }

        fn abort_merge(&self, worktree_path: &Path) -> Result<(), McpInternalError> {
            self.state.lock().unwrap().merge_abort_calls.push(worktree_path.to_path_buf());
            Ok(())
        }

        fn abort_rebase(&self, worktree_path: &Path) -> Result<(), McpInternalError> {
            self.state.lock().unwrap().rebase_abort_calls.push(worktree_path.to_path_buf());
            Ok(())
        }
    }

    struct BlockingGit {
        entered: Mutex<Option<mpsc::Sender<()>>>,
        release: Arc<(Mutex<bool>, Condvar)>,
        worktrees: Vec<WorktreeInfo>,
        source_oid: String,
        metadata: HashMap<PathBuf, FakeMetadata>,
    }

    impl MergeMainGit for BlockingGit {
        fn fetch_source_branch(&self, _: &Path, _: &str) -> Result<(), McpInternalError> {
            if let Some(sender) = self.entered.lock().unwrap().take() {
                let _ = sender.send(());
            }
            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }
            Ok(())
        }

        fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, McpInternalError> {
            Ok(self.worktrees.clone())
        }

        fn resolve_ref(&self, repo_root: &Path, ref_expr: &str) -> Result<String, McpInternalError> {
            if ref_expr == "HEAD" {
                return Ok(self.metadata.get(repo_root).expect("metadata").head_oid.clone());
            }
            Ok(self.source_oid.clone())
        }

        fn git_status_summary(&self, worktree_path: &Path) -> Result<WorktreeGitStatusSummary, McpInternalError> {
            Ok(WorktreeGitStatusSummary {
                dirty: self.metadata.get(worktree_path).expect("metadata").dirty,
                ..WorktreeGitStatusSummary::default()
            })
        }

        fn is_ancestor(&self, repo_root: &Path, _: &str, _: &str) -> Result<bool, McpInternalError> {
            Ok(self.metadata.get(repo_root).expect("metadata").would_fast_forward)
        }

        fn ahead_behind(&self, worktree_path: &Path, _: &str) -> Result<(u32, u32), McpInternalError> {
            let metadata = self.metadata.get(worktree_path).expect("metadata");
            Ok((metadata.ahead, metadata.behind))
        }
    }

    struct TestHarness {
        runtime: ToolRuntime,
        git: Arc<FakeGit>,
        executor: Arc<FakeExecutor>,
        _audit_dir: TempDir,
        _workspace: TempDir,
    }

    impl TestHarness {
        fn new(workspace: TempDir, sessions: Vec<Session>, git: Arc<FakeGit>, executor: Arc<FakeExecutor>) -> Self {
            let audit_dir = TempDir::new().unwrap();
            let runtime = ToolRuntime {
                workspace_root: Some(workspace.path().to_path_buf()),
                sessions: Arc::new(Mutex::new(sessions)),
                git: git.clone(),
                executor: executor.clone(),
                confirm: Arc::new(PendingMcpActionRegistry::new()),
                audit: Arc::new(crate::mcp::audit::AuditLog::new(audit_dir.path().to_path_buf()).unwrap()),
            };
            Self {
                runtime,
                git,
                executor,
                _audit_dir: audit_dir,
                _workspace: workspace,
            }
        }
    }

    fn make_workspace() -> TempDir {
        let workspace = TempDir::new().unwrap();
        std::fs::create_dir_all(workspace.path().join(".git")).unwrap();
        workspace
    }

    fn make_worktree(workspace: &TempDir, name: &str) -> PathBuf {
        let path = workspace.path().join(".arborist").join(".worktrees").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn session(id: SessionId, worktree_path: &Path, label: &str, status: SessionStatus) -> Session {
        Session {
            id,
            tool: Tool::Claude,
            worktree_path: worktree_path.to_path_buf(),
            worktree_name: worktree_path.file_name().unwrap_or_default().to_string_lossy().to_string(),
            label: label.to_owned(),
            composed_command: "claude".to_owned(),
            structured_command: None,
            command_provenance: Vec::new(),
            status,
            pid: None,
            created_at: 0,
            tab_index: 0,
            temp_files: Vec::new(),
            ai_session_id: None,
            last_metrics: None::<SessionMetricsEvent>,
        }
    }

    fn approve_token(runtime: &ToolRuntime, session_id: &str) -> String {
        let pending = runtime.confirm.list_for_session(session_id);
        let token = runtime.confirm.approve(&pending[0].id).expect("token should be approved");
        token.token
    }

    fn build_ff_only_harness() -> (TestHarness, String, PathBuf, PathBuf, PathBuf) {
        let workspace = make_workspace();
        let own = make_worktree(&workspace, "own");
        let ff_one = make_worktree(&workspace, "ff-one");
        let ff_two = make_worktree(&workspace, "ff-two");
        let non_ff = make_worktree(&workspace, "non-ff");
        let session_id = SessionId::new();
        let current = session(session_id, &own, "current", SessionStatus::Running);
        let git = Arc::new(FakeGit::default());
        git.set_source_oid("main", "source-oid");
        git.set_worktrees(vec![
            WorktreeInfo {
                path: workspace.path().to_path_buf(),
                branch: Some("main".to_owned()),
                is_main: true,
                is_locked: false,
            },
            WorktreeInfo {
                path: ff_one.clone(),
                branch: Some("feature-one".to_owned()),
                is_main: false,
                is_locked: false,
            },
            WorktreeInfo {
                path: ff_two.clone(),
                branch: Some("feature-two".to_owned()),
                is_main: false,
                is_locked: false,
            },
            WorktreeInfo {
                path: non_ff.clone(),
                branch: Some("feature-three".to_owned()),
                is_main: false,
                is_locked: false,
            },
        ]);
        git.set_metadata(&ff_one, FakeMetadata::new("ff-one-head", 0, 2, true));
        git.set_metadata(&ff_two, FakeMetadata::new("ff-two-head", 1, 3, true));
        git.set_metadata(&non_ff, FakeMetadata::new("non-ff-head", 4, 1, false));
        git.set_metadata(&own, FakeMetadata::new("own-head", 0, 0, false));
        let harness = TestHarness::new(workspace, vec![current], git, Arc::new(FakeExecutor::default()));
        (harness, session_id.to_string(), ff_one, ff_two, non_ff)
    }

    #[test]
    fn rejects_invalid_source_branch() {
        let (harness, session_id, ..) = build_ff_only_harness();
        let err = invoke_with_runtime(&harness.runtime, &session_id, json!({ "sourceBranch": "../evil" })).expect_err("must fail");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::InvalidArg);
    }

    #[test]
    fn rejects_dry_run_false_without_confirmation_token() {
        let (harness, session_id, ..) = build_ff_only_harness();
        let err = invoke_with_runtime(&harness.runtime, &session_id, json!({ "dryRun": false })).expect_err("must require confirmation");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::ConfirmationRequired);
        assert_eq!(harness.runtime.confirm.list_for_session(&session_id).len(), 1);
    }

    #[test]
    fn rejects_invalid_strategy_variant() {
        let (harness, session_id, ..) = build_ff_only_harness();
        let err = invoke_with_runtime(&harness.runtime, &session_id, json!({ "strategy": "octopus" })).expect_err("must fail");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::InvalidArg);
    }

    #[test]
    fn rejects_too_many_exclude_paths() {
        let (harness, session_id, ..) = build_ff_only_harness();
        let err =
            invoke_with_runtime(&harness.runtime, &session_id, json!({ "excludePaths": vec!["x"; MAX_EXCLUDE_PATHS + 1] })).expect_err("must fail");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::InvalidArg);
    }

    #[test]
    fn dry_run_ff_only_splits_fast_forward_and_non_fast_forward() {
        let (harness, session_id, ..) = build_ff_only_harness();
        let value = invoke_with_runtime(&harness.runtime, &session_id, json!({})).expect("dry-run should succeed");
        assert_eq!(value["dryRun"], json!(true));
        assert_eq!(value["wouldFastForward"].as_array().unwrap().len(), 2);
        let skipped = value["skipped"].as_array().unwrap();
        assert!(skipped.iter().any(|entry| entry["skipReason"] == json!("non-fast-forward")));
    }

    #[test]
    fn execute_ff_only_merges_all_fast_forward_candidates() {
        let (harness, session_id, ..) = build_ff_only_harness();
        let err = invoke_with_runtime(&harness.runtime, &session_id, json!({ "dryRun": false })).expect_err("must require confirmation");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::ConfirmationRequired);
        let token = approve_token(&harness.runtime, &session_id);
        let value = invoke_with_runtime(&harness.runtime, &session_id, json!({ "dryRun": false, "confirmationToken": token }))
            .expect("execute should succeed");
        assert_eq!(value["merged"].as_array().unwrap().len(), 2);
        assert!(value["conflicts"].as_array().unwrap().is_empty());
        assert!(value["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn merge_strategy_dry_run_and_execute_report_conflicts_and_abort() {
        let (harness, session_id, _, _, non_ff) = build_ff_only_harness();
        harness.executor.set_merge_result(
            &non_ff,
            CommandOutcome::Conflict {
                files: vec!["conflicted.txt".to_owned()],
            },
        );
        let dry_run = invoke_with_runtime(&harness.runtime, &session_id, json!({ "strategy": "merge" })).expect("dry-run should succeed");
        assert_eq!(dry_run["wouldMerge"].as_array().unwrap().len(), 1);

        let err = invoke_with_runtime(&harness.runtime, &session_id, json!({ "dryRun": false, "strategy": "merge" }))
            .expect_err("must require confirmation");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::ConfirmationRequired);
        let token = approve_token(&harness.runtime, &session_id);
        let executed = invoke_with_runtime(
            &harness.runtime,
            &session_id,
            json!({ "dryRun": false, "strategy": "merge", "confirmationToken": token }),
        )
        .expect("execute should succeed");
        assert_eq!(executed["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(harness.executor.merge_abort_calls(), vec![non_ff]);
    }

    #[test]
    fn rebase_strategy_dry_run_and_execute_report_conflicts_and_abort() {
        let (harness, session_id, _, _, non_ff) = build_ff_only_harness();
        harness.executor.set_rebase_result(
            &non_ff,
            CommandOutcome::Conflict {
                files: vec!["rebase-conflict.txt".to_owned()],
            },
        );
        let dry_run = invoke_with_runtime(&harness.runtime, &session_id, json!({ "strategy": "rebase" })).expect("dry-run should succeed");
        assert_eq!(dry_run["wouldMerge"].as_array().unwrap().len(), 1);

        let err = invoke_with_runtime(&harness.runtime, &session_id, json!({ "dryRun": false, "strategy": "rebase" }))
            .expect_err("must require confirmation");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::ConfirmationRequired);
        let token = approve_token(&harness.runtime, &session_id);
        let executed = invoke_with_runtime(
            &harness.runtime,
            &session_id,
            json!({ "dryRun": false, "strategy": "rebase", "confirmationToken": token }),
        )
        .expect("execute should succeed");
        assert_eq!(executed["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(harness.executor.rebase_abort_calls(), vec![non_ff]);
    }

    #[test]
    fn skips_dirty_active_own_and_locked_worktrees() {
        let workspace = make_workspace();
        let own = make_worktree(&workspace, "own");
        let dirty = make_worktree(&workspace, "dirty");
        let active = make_worktree(&workspace, "active");
        let locked = make_worktree(&workspace, "locked");
        let current_id = SessionId::new();
        let current = session(current_id, &own, "current", SessionStatus::Running);
        let other_active = session(SessionId::new(), &active, "other", SessionStatus::Running);
        let git = Arc::new(FakeGit::default());
        git.set_source_oid("main", "source-oid");
        git.set_worktrees(vec![
            WorktreeInfo {
                path: own.clone(),
                branch: Some("feature-own".to_owned()),
                is_main: false,
                is_locked: false,
            },
            WorktreeInfo {
                path: dirty.clone(),
                branch: Some("feature-dirty".to_owned()),
                is_main: false,
                is_locked: false,
            },
            WorktreeInfo {
                path: active.clone(),
                branch: Some("feature-active".to_owned()),
                is_main: false,
                is_locked: false,
            },
            WorktreeInfo {
                path: locked.clone(),
                branch: Some("feature-locked".to_owned()),
                is_main: false,
                is_locked: true,
            },
        ]);
        let mut dirty_meta = FakeMetadata::new("dirty-head", 0, 1, true);
        dirty_meta.dirty = true;
        git.set_metadata(&own, FakeMetadata::new("own-head", 0, 0, false));
        git.set_metadata(&dirty, dirty_meta);
        git.set_metadata(&active, FakeMetadata::new("active-head", 0, 2, true));
        git.set_metadata(&locked, FakeMetadata::new("locked-head", 0, 2, true));
        let harness = TestHarness::new(workspace, vec![current.clone(), other_active], git, Arc::new(FakeExecutor::default()));

        let value = invoke_with_runtime(&harness.runtime, &current.id.to_string(), json!({})).expect("dry-run should succeed");
        let reasons: Vec<_> = value["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["skipReason"].as_str().unwrap().to_owned())
            .collect();
        assert!(reasons.contains(&"uncommitted-changes".to_owned()));
        assert!(reasons.contains(&"active-session".to_owned()));
        assert!(reasons.contains(&"own-worktree-refused".to_owned()));
        assert!(reasons.contains(&"locked".to_owned()));
    }

    #[test]
    fn marks_fetch_failures_as_stale_data() {
        let (harness, session_id, ..) = build_ff_only_harness();
        harness.git.set_fetch_error("network down");
        let value = invoke_with_runtime(&harness.runtime, &session_id, json!({})).expect("dry-run should still succeed");
        assert_eq!(value["staleData"], json!(true));
    }

    #[test]
    fn returns_confirmation_stale_without_consuming_token_when_world_changes() {
        let (harness, session_id, ff_one, ..) = build_ff_only_harness();
        let _preview = invoke_with_runtime(&harness.runtime, &session_id, json!({})).expect("dry-run should succeed");
        let err = invoke_with_runtime(&harness.runtime, &session_id, json!({ "dryRun": false })).expect_err("must require confirmation");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::ConfirmationRequired);
        let token = approve_token(&harness.runtime, &session_id);
        harness.git.update_head_oid(&ff_one, "new-head");

        let stale = invoke_with_runtime(
            &harness.runtime,
            &session_id,
            json!({ "dryRun": false, "confirmationToken": token.clone() }),
        )
        .expect_err("must stale");
        assert_eq!(stale.code(), crate::mcp::types::McpErrorCode::ConfirmationStale);

        let stale_again = invoke_with_runtime(&harness.runtime, &session_id, json!({ "dryRun": false, "confirmationToken": token }))
            .expect_err("token should remain unconsumed on stale");
        assert_eq!(stale_again.code(), crate::mcp::types::McpErrorCode::ConfirmationStale);
    }

    #[test]
    fn second_parallel_call_reports_busy() {
        let workspace = make_workspace();
        let own = make_worktree(&workspace, "own");
        let target = make_worktree(&workspace, "target");
        let session_id = SessionId::new();
        let current = session(session_id, &own, "current", SessionStatus::Running);
        let metadata = HashMap::from([
            (target.clone(), FakeMetadata::new("target-head", 0, 1, true)),
            (own.clone(), FakeMetadata::new("own-head", 0, 0, false)),
        ]);
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let git: Arc<dyn MergeMainGit> = Arc::new(BlockingGit {
            entered: Mutex::new(Some(entered_tx)),
            release: Arc::clone(&release),
            worktrees: vec![WorktreeInfo {
                path: target.clone(),
                branch: Some("feature".to_owned()),
                is_main: false,
                is_locked: false,
            }],
            source_oid: "source-oid".to_owned(),
            metadata,
        });
        let audit_dir = TempDir::new().unwrap();
        let runtime = ToolRuntime {
            workspace_root: Some(workspace.path().to_path_buf()),
            sessions: Arc::new(Mutex::new(vec![current.clone()])),
            git,
            executor: Arc::new(FakeExecutor::default()),
            confirm: Arc::new(PendingMcpActionRegistry::new()),
            audit: Arc::new(crate::mcp::audit::AuditLog::new(audit_dir.path().to_path_buf()).unwrap()),
        };

        let session_id_string = session_id.to_string();
        let runtime = Arc::new(runtime);
        let first_runtime = Arc::clone(&runtime);
        let first_session = session_id_string.clone();
        let handle = thread::spawn(move || invoke_with_runtime(&first_runtime, &first_session, json!({})).unwrap());
        entered_rx.recv().unwrap();

        let busy = invoke_with_runtime(&runtime, &session_id_string, json!({})).expect_err("second call should be busy");
        assert_eq!(busy.code(), crate::mcp::types::McpErrorCode::Busy);

        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
        handle.join().unwrap();
    }

    #[test]
    fn reports_workspace_unbound() {
        let audit_dir = TempDir::new().unwrap();
        let runtime = ToolRuntime {
            workspace_root: None,
            sessions: Arc::new(Mutex::new(Vec::new())),
            git: Arc::new(FakeGit::default()),
            executor: Arc::new(FakeExecutor::default()),
            confirm: Arc::new(PendingMcpActionRegistry::new()),
            audit: Arc::new(crate::mcp::audit::AuditLog::new(audit_dir.path().to_path_buf()).unwrap()),
        };
        let err = invoke_with_runtime(&runtime, &SessionId::new().to_string(), json!({})).expect_err("must fail");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::WorkspaceUnbound);
    }

    #[test]
    fn truncates_candidate_list_at_one_hundred_entries() {
        let workspace = make_workspace();
        let own = make_worktree(&workspace, "own");
        let session_id = SessionId::new();
        let current = session(session_id, &own, "current", SessionStatus::Running);
        let git = Arc::new(FakeGit::default());
        git.set_source_oid("main", "source-oid");
        git.set_metadata(&own, FakeMetadata::new("own-head", 0, 0, false));
        let mut worktrees = Vec::new();
        for index in 0..101 {
            let path = make_worktree(&workspace, &format!("feature-{index}"));
            worktrees.push(WorktreeInfo {
                path: path.clone(),
                branch: Some(format!("feature-{index}")),
                is_main: false,
                is_locked: false,
            });
            git.set_metadata(&path, FakeMetadata::new(&format!("head-{index}"), 0, 1, true));
        }
        git.set_worktrees(worktrees);
        let harness = TestHarness::new(workspace, vec![current], git, Arc::new(FakeExecutor::default()));

        let value = invoke_with_runtime(&harness.runtime, &session_id.to_string(), json!({})).expect("dry-run should succeed");
        assert_eq!(value["candidates"].as_array().unwrap().len(), 100);
        assert_eq!(value["truncated"], json!(true));
    }

    impl GitRunner for FakeGit {
        fn list_worktrees(&self, repo_root: &Path) -> Result<Vec<WorktreeInfo>, crate::types::Error> {
            MergeMainGit::list_worktrees(self, repo_root).map_err(|err| crate::types::Error::Internal(err.to_string()))
        }

        fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, crate::types::Error> {
            Ok(Some(path.to_path_buf()))
        }

        fn create_worktree(&self, repo_root: &Path, relative_path: &Path, _: &str) -> Result<PathBuf, crate::types::Error> {
            Ok(repo_root.join(relative_path))
        }

        fn remove_worktree(&self, _: &Path, _: &Path) -> Result<(), crate::types::Error> {
            Ok(())
        }

        fn git_status(&self, _: &Path) -> Result<crate::types::WorktreeGitStatus, crate::types::Error> {
            Ok(crate::types::WorktreeGitStatus::default())
        }

        fn fetch_origin(&self, _: &Path, _: std::time::Duration) -> Result<(), GitError> {
            Ok(())
        }

        fn branches_merged_into(&self, _: &Path, _: &str) -> Result<HashSet<String>, GitError> {
            Ok(HashSet::new())
        }

        fn cherry_empty(&self, _: &Path, _: &str, _: &str) -> Result<bool, GitError> {
            Ok(true)
        }

        fn merge_from_branch(&self, _: &Path, _: &str, _: bool, _: std::time::Duration) -> Result<MergeFromBranchOutcome, GitError> {
            Ok(MergeFromBranchOutcome::AlreadyUpToDate)
        }

        fn default_branch(&self, _: &Path) -> Result<DefaultBranchInfo, GitError> {
            Ok(DefaultBranchInfo {
                branch: "main".to_owned(),
                source: DefaultBranchSource::Main,
            })
        }

        fn rev_parse_verify(&self, root: &Path, ref_expr: &str) -> Result<String, GitError> {
            MergeMainGit::resolve_ref(self, root, ref_expr).map_err(|_| GitError::RefNotFound {
                ref_expr: ref_expr.to_owned(),
            })
        }

        fn git_status_mcp(&self, worktree: &Path) -> Result<WorktreeGitStatusSummary, GitError> {
            MergeMainGit::git_status_summary(self, worktree).map_err(|err| GitError::CommandFailed {
                context: "git status --porcelain=v2",
                message: err.to_string(),
            })
        }

        fn merge_tree_dry_run(&self, _: &Path, _: &str, _: &str) -> Result<MergeTreeOutcome, GitError> {
            Ok(MergeTreeOutcome::Unsupported)
        }

        fn merge_abort(&self, _: &Path) -> Result<(), GitError> {
            Ok(())
        }

        fn has_merge_head(&self, _: &Path) -> Result<bool, GitError> {
            Ok(false)
        }
    }

    fn null_sink() -> PtySink {
        PtySink::new(
            Arc::new(|_: &SessionId, _: String| {}),
            Arc::new(|_: &SessionId, _: SessionStatus, _: Option<u32>, _: Option<String>| {}),
            Arc::new(|_, _| {}),
        )
    }

    #[test]
    fn public_invoke_reports_workspace_unbound() {
        let store_dir = TempDir::new().unwrap();
        let store = ConfigStore::open(store_dir.path()).unwrap();
        let pool = Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)));
        let git: Arc<dyn GitRunner> = Arc::new(FakeGit::default());
        let app = Arc::new(AppContext::new(
            pool,
            store,
            null_sink(),
            git,
            Arc::new(|_| {}),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| {}),
        ));
        let context = Arc::new(McpContext::new(app, crate::mcp::types::McpContextConfig::default(), store_dir.path().to_path_buf()).unwrap());
        let registry = Arc::new(McpSessionRegistry::new(context));

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err = rt
            .block_on(invoke(&registry, &SessionId::new().to_string(), json!({})))
            .expect_err("must fail");
        assert_eq!(err.code(), crate::mcp::types::McpErrorCode::WorkspaceUnbound);
    }
}
