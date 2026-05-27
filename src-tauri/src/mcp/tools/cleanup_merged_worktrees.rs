use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::compose::validate_ref_name;
use crate::git::{git_command_mcp_ro, WorktreeGitStatusSummary};
use crate::mcp::audit::AuditEntryInput;
use crate::mcp::confirm::{fingerprint_args, ConsumeError};
use crate::mcp::context::McpContext;
use crate::mcp::error::{error, McpInternalError};
use crate::mcp::ipc::McpSessionRegistry;
use crate::mcp::types::{McpAuditDecision, McpErrorCode, McpToolDescriptor, McpToolName};
use crate::types::{Session, SessionId};

const TOOL_ID: &str = "cleanup_merged_worktrees";
const DEFAULT_TARGET_BRANCH: &str = "main";
const MAX_CANDIDATES: usize = 100;
const OWN_WORKTREE_USER_ACTION: &str = "Close the session in that worktree first, or invoke from another session";

static IN_FLIGHT_WORKSPACES: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

#[must_use]
pub fn descriptor() -> McpToolDescriptor {
    McpToolDescriptor {
        name: TOOL_ID.to_owned(),
        description: "Identify merged worktrees relative to origin/main (or targetBranch). By default performs a dry run; executing removal requires confirmation.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "targetBranch": {
                    "type": "string",
                    "default": DEFAULT_TARGET_BRANCH,
                    "description": "Branch to compare against. Must be an unqualified ref name."
                },
                "dryRun": {
                    "type": "boolean",
                    "default": true
                },
                "confirmationToken": {
                    "type": "string",
                    "description": "Opaque single-use token returned after the user approves the pending cleanup request."
                },
                "allowStaleRemoteData": {
                    "type": "boolean",
                    "default": false,
                    "description": "Required for execution when the dry run had staleData=true because origin/<targetBranch> could not be fetched."
                },
                "excludePaths": {
                    "type": "array",
                    "maxItems": MAX_CANDIDATES,
                    "items": {
                        "type": "string"
                    },
                    "description": "Workspace-relative worktree paths to leave untouched."
                }
            },
            "additionalProperties": false
        }),
    }
}

pub async fn invoke(registry: &McpSessionRegistry, session_id: &str, args: Value) -> Result<Value, McpInternalError> {
    let context = registry.context();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let parsed = parse_args(args)?;
        let mut host = WiredRegistryHost::new(context)?;
        run_cleanup(&mut host, &session_id, parsed)
    })
    .await
    .map_err(|err| McpInternalError::Internal {
        message: format!("cleanup_merged_worktrees task failed: {err}"),
    })?
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CleanupArgs {
    #[serde(default)]
    target_branch: Option<String>,
    #[serde(default = "default_true")]
    dry_run: bool,
    #[serde(default)]
    confirmation_token: Option<String>,
    #[serde(default)]
    allow_stale_remote_data: bool,
    #[serde(default)]
    exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum MergedVia {
    Merge,
    #[serde(rename = "patch-equivalent")]
    PatchEquivalent,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CleanupCandidate {
    relative_path: String,
    branch: String,
    head_oid: String,
    merged_via: Option<MergedVia>,
    is_merged: bool,
    has_uncommitted_changes: bool,
    has_active_session: bool,
    is_locked: bool,
    lock_reason: Option<String>,
    skip_reason: Option<String>,
    skip_reason_human: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CleanupSkipped {
    relative_path: String,
    reason: String,
    reason_human: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CleanupErrorEntry {
    relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_action: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CleanupPartialEntry {
    relative_path: String,
    reason: String,
    error: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum CleanupRemovalOutcome {
    Removed { relative_path: String },
    Partial { relative_path: String, reason: String, error: String },
    Error { relative_path: String, error: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DryRunResult {
    dry_run: bool,
    target_branch: String,
    candidates: Vec<CleanupCandidate>,
    would_remove: Vec<String>,
    skipped: Vec<CleanupSkipped>,
    errors: Vec<CleanupErrorEntry>,
    stale_data: bool,
    truncated: bool,
    total_candidates_considered: usize,
    as_of: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecuteResult {
    dry_run: bool,
    target_branch: String,
    outcomes: Vec<CleanupRemovalOutcome>,
    removed: Vec<String>,
    partial: Vec<CleanupPartialEntry>,
    errors: Vec<CleanupErrorEntry>,
    skipped: Vec<CleanupSkipped>,
    stale_data: bool,
    as_of: String,
}

#[derive(Debug, Clone)]
struct CleanupWorktree {
    path: PathBuf,
    branch: Option<String>,
    head_oid: String,
    is_main: bool,
    is_locked: bool,
    lock_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct RemovalTarget {
    relative_path: String,
    worktree_path: PathBuf,
    branch: String,
    head_oid: String,
}

#[derive(Debug, Clone)]
struct ScanSnapshot {
    target_branch: String,
    candidates: Vec<CleanupCandidate>,
    skipped: Vec<CleanupSkipped>,
    errors: Vec<CleanupErrorEntry>,
    removal_targets: Vec<RemovalTarget>,
    stale_data: bool,
    truncated: bool,
    total_candidates_considered: usize,
    as_of: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmationBinding {
    target_branch: String,
    stale_data: bool,
    candidates: Vec<ConfirmationCandidateBinding>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmationCandidateBinding {
    relative_path: String,
    branch: String,
    head_oid: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum RemovalResultKind {
    Removed,
    Partial { reason: String, error: String },
}

#[derive(Debug, Clone)]
struct CleanupAuditEntry {
    decision: McpAuditDecision,
    summary: String,
    result: Value,
    duration_ms: u64,
}

trait CleanupHost {
    fn workspace_root(&self) -> Result<PathBuf, McpInternalError>;
    fn own_worktree_path(&self, session_id: &str) -> Result<PathBuf, McpInternalError>;
    fn fetch_origin_target(&mut self, workspace_root: &Path, target_branch: &str) -> Result<(), McpInternalError>;
    fn list_worktrees(&self, workspace_root: &Path) -> Result<Vec<CleanupWorktree>, McpInternalError>;
    fn branches_merged_into(&mut self, workspace_root: &Path, target_oid: &str) -> Result<HashSet<String>, McpInternalError>;
    fn cherry_empty(&mut self, workspace_root: &Path, upstream_ref: &str, branch: &str) -> Result<bool, McpInternalError>;
    fn rev_parse_verify(&mut self, workspace_root: &Path, ref_expr: &str) -> Result<String, McpInternalError>;
    fn git_status(&mut self, worktree_path: &Path) -> Result<WorktreeGitStatusSummary, McpInternalError>;
    fn has_live_session(&self, worktree_path: &Path) -> bool;
    fn has_persisted_session(&self, worktree_path: &Path) -> bool;

    fn has_live_subsession(&self, _worktree_path: &Path) -> bool {
        false
    }

    fn has_persisted_subsession(&self, _worktree_path: &Path) -> bool {
        false
    }

    fn create_pending_action(&mut self, session_id: &str, summary: String, fingerprint: [u8; 32], payload: Value) -> Result<(), McpInternalError>;

    fn consume_confirmation(&mut self, token: &str, expected_fingerprint: &[u8; 32]) -> Result<(), ConsumeError>;

    fn remove_worktree(&mut self, workspace_root: &Path, worktree_path: &Path) -> Result<RemovalResultKind, McpInternalError>;

    fn append_audit(&mut self, _session_id: &str, entry: CleanupAuditEntry) -> Result<(), McpInternalError> {
        let _ = (&entry.decision, &entry.summary, &entry.result, entry.duration_ms);
        Ok(())
    }

    fn now_rfc3339(&self) -> String;
}

struct WiredRegistryHost {
    context: Arc<McpContext>,
    sessions: Vec<Session>,
}

impl WiredRegistryHost {
    fn new(context: Arc<McpContext>) -> Result<Self, McpInternalError> {
        let store = {
            let workspace = match context.app.workspace.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            workspace.store.clone()
        };
        let sessions = store
            .as_ref()
            .map(|store| store.load_sessions().into_values().collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(Self { context, sessions })
    }
}

impl CleanupHost for WiredRegistryHost {
    fn workspace_root(&self) -> Result<PathBuf, McpInternalError> {
        let workspace = match self.context.app.workspace.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        workspace.workspace_root.clone().ok_or_else(|| McpInternalError::WorkspaceUnbound {
            message: "Open a workspace in Arborist before retrying cleanup_merged_worktrees".to_owned(),
        })
    }

    fn own_worktree_path(&self, session_id: &str) -> Result<PathBuf, McpInternalError> {
        // The caller's session id reaches the MCP sidecar via TLS handshake and is propagated to
        // every tool invocation. We look it up against the persisted sessions snapshot taken at
        // host construction; if it's missing we still need to fail closed because the own-worktree
        // refusal protects an in-flight session from removing itself out from under the user.
        let parsed = Uuid::parse_str(session_id).map(SessionId).map_err(|err| McpInternalError::Internal {
            message: format!("invalid internal session id '{session_id}': {err}"),
        })?;
        self.sessions
            .iter()
            .find(|session| session.id == parsed)
            .map(|session| session.worktree_path.clone())
            .ok_or_else(|| McpInternalError::Internal {
                message: format!("session '{session_id}' is not registered in the current workspace"),
            })
    }

    fn fetch_origin_target(&mut self, workspace_root: &Path, target_branch: &str) -> Result<(), McpInternalError> {
        // A failed fetch is not fatal — the engine surfaces `staleData=true` so the caller can opt
        // in explicitly via `allowStaleRemoteData`. We only escalate to an `Internal` error when
        // the git binary itself is unusable; offline / no-network failures bubble up as a
        // non-success exit and we let the scan continue with whatever local state we have.
        let mut cmd = git_command_mcp_ro(workspace_root);
        cmd.args(["fetch", "--no-tags", "--quiet", "origin", target_branch]);
        let output = run_git(cmd, "git fetch --no-tags origin <target>")?;
        if output.status.success() {
            Ok(())
        } else {
            Err(McpInternalError::Internal {
                message: format!("git fetch --no-tags origin {target_branch}: {}", output_message(&output)),
            })
        }
    }

    fn list_worktrees(&self, workspace_root: &Path) -> Result<Vec<CleanupWorktree>, McpInternalError> {
        let mut cmd = git_command_mcp_ro(workspace_root);
        cmd.args(["worktree", "list", "--porcelain"]);
        let output = run_git(cmd, "git worktree list --porcelain")?;
        if !output.status.success() {
            return Err(McpInternalError::Internal {
                message: format!("git worktree list: {}", output_message(&output)),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_cleanup_porcelain(&stdout))
    }

    fn branches_merged_into(&mut self, workspace_root: &Path, target_oid: &str) -> Result<HashSet<String>, McpInternalError> {
        let mut cmd = git_command_mcp_ro(workspace_root);
        // `--format=%(refname:short)` keeps the output to a clean list of branch names with no
        // leading whitespace markers that `git branch --list` adds in human mode.
        cmd.args(["branch", "--list", "--merged", target_oid, "--format=%(refname:short)"]);
        let output = run_git(cmd, "git branch --merged")?;
        if !output.status.success() {
            return Err(McpInternalError::Internal {
                message: format!("git branch --merged {target_oid}: {}", output_message(&output)),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect())
    }

    fn cherry_empty(&mut self, workspace_root: &Path, upstream_ref: &str, branch: &str) -> Result<bool, McpInternalError> {
        // `git cherry <upstream> <branch>` emits `+` for commits on `branch` that have no
        // patch-equivalent on `upstream`, and `-` for those that do. The branch is considered
        // patch-equivalent-merged when no `+` line appears.
        let mut cmd = git_command_mcp_ro(workspace_root);
        cmd.args(["cherry", upstream_ref, branch]);
        let output = run_git(cmd, "git cherry")?;
        if !output.status.success() {
            return Err(McpInternalError::Internal {
                message: format!("git cherry {upstream_ref} {branch}: {}", output_message(&output)),
            });
        }
        let any_unmerged = String::from_utf8_lossy(&output.stdout).lines().any(|line| line.starts_with('+'));
        Ok(!any_unmerged)
    }

    fn rev_parse_verify(&mut self, workspace_root: &Path, ref_expr: &str) -> Result<String, McpInternalError> {
        self.context
            .app
            .git_runner
            .rev_parse_verify(workspace_root, ref_expr)
            .map_err(McpInternalError::from)
    }

    fn git_status(&mut self, worktree_path: &Path) -> Result<WorktreeGitStatusSummary, McpInternalError> {
        self.context.app.git_runner.git_status_mcp(worktree_path).map_err(McpInternalError::from)
    }

    fn has_live_session(&self, worktree_path: &Path) -> bool {
        // "Live" means there's an OS process backing a persisted session whose worktree path
        // (canonicalised both sides to defeat symlink hops) matches. We use the persisted
        // sessions snapshot taken at construction and the live PtyPool to bridge persisted-state
        // to actually-running-state.
        let canon = dunce::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
        self.sessions.iter().any(|session| {
            let session_canon = dunce::canonicalize(&session.worktree_path).unwrap_or_else(|_| session.worktree_path.clone());
            session_canon == canon && self.context.app.pool.pid_of(&session.id).is_some()
        })
    }

    fn has_persisted_session(&self, worktree_path: &Path) -> bool {
        let canon = dunce::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
        self.sessions.iter().any(|session| {
            let session_canon = dunce::canonicalize(&session.worktree_path).unwrap_or_else(|_| session.worktree_path.clone());
            session_canon == canon
        })
    }

    fn create_pending_action(&mut self, session_id: &str, summary: String, fingerprint: [u8; 32], payload: Value) -> Result<(), McpInternalError> {
        self.context
            .confirm
            .create(session_id.to_owned(), McpToolName::CleanupMergedWorktrees, summary, fingerprint, payload)?;
        Ok(())
    }

    fn consume_confirmation(&mut self, token: &str, expected_fingerprint: &[u8; 32]) -> Result<(), ConsumeError> {
        self.context.confirm.try_consume(token, expected_fingerprint).map(|_| ())
    }

    fn remove_worktree(&mut self, workspace_root: &Path, worktree_path: &Path) -> Result<RemovalResultKind, McpInternalError> {
        // Mirror `merge_main_into_worktrees`' tolerance for transient failures: a removal that
        // fails partway through (e.g., the worktree directory was open in Explorer on Windows)
        // lands in `Partial { reason, error }` rather than an outright `Err`, so the engine can
        // report it alongside its successes in the same `executeResult`.
        match self.context.app.git_runner.remove_worktree(workspace_root, worktree_path) {
            Ok(()) => Ok(RemovalResultKind::Removed),
            Err(err) => Ok(RemovalResultKind::Partial {
                reason: "removal-failed".to_owned(),
                error: err.to_string(),
            }),
        }
    }

    fn append_audit(&mut self, session_id: &str, entry: CleanupAuditEntry) -> Result<(), McpInternalError> {
        let label = self
            .sessions
            .iter()
            .find(|session| {
                let id_str = session.id.0.as_hyphenated().to_string();
                id_str == session_id || session.id.0.as_simple().to_string() == session_id
            })
            .map(|session| session.label.clone())
            .unwrap_or_default();

        let input = AuditEntryInput {
            ts: self.now_rfc3339(),
            session_id: session_id.to_owned(),
            session_label: label,
            tool: TOOL_ID.to_owned(),
            decision: entry.decision,
            args_summary: entry.summary,
            result: entry.result,
            duration_ms: entry.duration_ms,
            // Tracing IDs the audit log expects in every record. We don't have a per-request id
            // bubbling down from the IPC layer here yet, so we mint fresh UUIDs per audit entry —
            // the chain still verifies because seq + prev_hash carry the integrity guarantee.
            request_id: Uuid::new_v4().as_simple().to_string(),
            confirmation_token_sha256: None,
            audit_id: Uuid::new_v4().as_simple().to_string(),
        };
        self.context
            .audit
            .append_destructive(input)
            .map(|_| ())
            .map_err(|err| McpInternalError::Internal {
                message: format!("audit append failed: {err}"),
            })
    }

    fn now_rfc3339(&self) -> String {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
    }
}

fn parse_cleanup_porcelain(input: &str) -> Vec<CleanupWorktree> {
    let mut out: Vec<CleanupWorktree> = Vec::new();
    let mut is_first = true;
    let mut current: Option<CleanupWorktree> = None;

    for raw in input.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            if let Some(wt) = current.take() {
                out.push(wt);
                is_first = false;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(wt) = current.take() {
                out.push(wt);
                is_first = false;
            }
            current = Some(CleanupWorktree {
                path: PathBuf::from(rest),
                branch: None,
                head_oid: String::new(),
                is_main: is_first,
                is_locked: false,
                lock_reason: None,
            });
        } else if let Some(wt) = current.as_mut() {
            if let Some(rest) = line.strip_prefix("HEAD ") {
                wt.head_oid = rest.to_owned();
            } else if let Some(rest) = line.strip_prefix("branch ") {
                wt.branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_owned());
            } else if line == "locked" {
                wt.is_locked = true;
            } else if let Some(reason) = line.strip_prefix("locked ") {
                wt.is_locked = true;
                wt.lock_reason = Some(reason.to_owned());
            } else if line == "detached" {
                wt.branch = None;
            }
        }
    }
    if let Some(wt) = current.take() {
        out.push(wt);
    }
    // Canonicalise paths so downstream string-equality against persisted session worktrees
    // collapses symlink hops (matches the `parse_porcelain` behaviour in `git.rs`).
    for wt in &mut out {
        if let Ok(canon) = dunce::canonicalize(&wt.path) {
            wt.path = canon;
        }
    }
    out
}

fn run_git(mut cmd: Command, ctx: &str) -> Result<Output, McpInternalError> {
    cmd.output().map_err(|err| McpInternalError::Internal {
        message: format!("{ctx}: {err}"),
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

// Retained as a documented marker until follow-up work removes the trait branch entirely.
#[allow(dead_code)]
fn _legacy_unwired() -> Result<(), McpInternalError> {
    runtime_access_unavailable()
}

impl ScanSnapshot {
    fn dry_run_result(&self) -> DryRunResult {
        DryRunResult {
            dry_run: true,
            target_branch: self.target_branch.clone(),
            candidates: self.candidates.clone(),
            would_remove: self.removal_targets.iter().map(|target| target.relative_path.clone()).collect(),
            skipped: self.skipped.clone(),
            errors: self.errors.clone(),
            stale_data: self.stale_data,
            truncated: self.truncated,
            total_candidates_considered: self.total_candidates_considered,
            as_of: self.as_of.clone(),
        }
    }
}

#[derive(Debug)]
struct WorkspaceCleanupGuard {
    workspace_root: PathBuf,
}

impl Drop for WorkspaceCleanupGuard {
    fn drop(&mut self) {
        let mut guard = match IN_FLIGHT_WORKSPACES.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.remove(&self.workspace_root);
    }
}

fn default_true() -> bool {
    true
}

fn runtime_access_unavailable<T>() -> Result<T, McpInternalError> {
    Err(McpInternalError::Internal {
        message: "cleanup_merged_worktrees is implemented in this module, but McpSessionRegistry does not yet expose the workspace/session context needed to invoke it safely".to_owned(),
    })
}

fn parse_args(args: Value) -> Result<CleanupArgs, McpInternalError> {
    let parsed: CleanupArgs =
        serde_json::from_value(args).map_err(|err| error(McpErrorCode::InvalidArg, format!("invalid cleanup_merged_worktrees arguments: {err}")))?;

    if parsed.exclude_paths.len() > MAX_CANDIDATES {
        return Err(error(
            McpErrorCode::InvalidArg,
            format!("excludePaths cannot contain more than {MAX_CANDIDATES} entries"),
        ));
    }

    if let Some(target_branch) = parsed.target_branch.as_deref() {
        validate_ref_name(target_branch).map_err(|reason| error(McpErrorCode::InvalidArg, format!("invalid targetBranch: {reason}")))?;
    }

    for entry in &parsed.exclude_paths {
        normalize_relative_input(entry)?;
    }

    Ok(parsed)
}

fn run_cleanup<H: CleanupHost>(host: &mut H, session_id: &str, args: CleanupArgs) -> Result<Value, McpInternalError> {
    let started = Instant::now();
    let workspace_root = host.workspace_root()?;
    let _guard = acquire_workspace_cleanup_guard(&workspace_root)?;
    let own_worktree = host.own_worktree_path(session_id)?;
    let target_branch = args.target_branch.unwrap_or_else(|| DEFAULT_TARGET_BRANCH.to_owned());

    validate_ref_name(&target_branch).map_err(|reason| error(McpErrorCode::InvalidArg, format!("invalid targetBranch: {reason}")))?;

    let scan = scan_workspace(host, &workspace_root, &own_worktree, &target_branch, &args.exclude_paths)?;

    if args.dry_run {
        let result = scan.dry_run_result();
        let value = to_json(&result)?;
        host.append_audit(
            session_id,
            CleanupAuditEntry {
                decision: McpAuditDecision::NotRequired,
                summary: build_confirmation_summary(&workspace_root, scan.removal_targets.len()),
                result: value.clone(),
                duration_ms: duration_ms(started),
            },
        )?;
        return Ok(value);
    }

    if scan.removal_targets.is_empty() {
        // If the caller is presenting a previously-issued confirmation token, the original scan
        // had at least one removal target; the candidate must have drifted (became dirty, gained
        // an active session, or its branch was unmerged) between approval and replay. We refuse
        // to silently downgrade an approved destructive call to a no-op — surface as stale so
        // the user can re-confirm against the new state.
        if args.confirmation_token.is_some() {
            return Err(McpInternalError::ConfirmationStale {
                message: "cleanup_merged_worktrees: candidate state changed since confirmation; refresh and re-approve".to_owned(),
            });
        }

        let result = ExecuteResult {
            dry_run: false,
            target_branch: scan.target_branch.clone(),
            outcomes: Vec::new(),
            removed: Vec::new(),
            partial: Vec::new(),
            errors: scan.errors.clone(),
            skipped: scan.skipped.clone(),
            stale_data: false,
            as_of: scan.as_of.clone(),
        };
        let value = to_json(&result)?;
        host.append_audit(
            session_id,
            CleanupAuditEntry {
                decision: McpAuditDecision::NotRequired,
                summary: build_confirmation_summary(&workspace_root, 0),
                result: value.clone(),
                duration_ms: duration_ms(started),
            },
        )?;
        return Ok(value);
    }

    let summary = build_confirmation_summary(&workspace_root, scan.removal_targets.len());
    let fingerprint = confirmation_fingerprint(&scan)?;

    if args.confirmation_token.is_none() {
        let payload = to_json(&scan.dry_run_result())?;
        host.create_pending_action(session_id, summary, fingerprint, payload)?;
        return Err(McpInternalError::ConfirmationRequired {
            message: "cleanup_merged_worktrees requires confirmation".to_owned(),
        });
    }

    if scan.stale_data && !args.allow_stale_remote_data {
        return Err(McpInternalError::StaleRemoteData {
            message: "cleanup_merged_worktrees requires allowStaleRemoteData=true because the remote fetch failed".to_owned(),
        });
    }

    let token = args.confirmation_token.unwrap_or_default();
    host.consume_confirmation(&token, &fingerprint).map_err(map_consume_error)?;
    revalidate_execution(host, &workspace_root, &own_worktree, &scan)?;

    let result = execute_removals(host, &workspace_root, &scan);
    let value = to_json(&result)?;
    host.append_audit(
        session_id,
        CleanupAuditEntry {
            decision: McpAuditDecision::Approved,
            summary: build_confirmation_summary(&workspace_root, scan.removal_targets.len()),
            result: value.clone(),
            duration_ms: duration_ms(started),
        },
    )?;
    Ok(value)
}

fn scan_workspace<H: CleanupHost>(
    host: &mut H,
    workspace_root: &Path,
    own_worktree: &Path,
    target_branch: &str,
    exclude_paths: &[String],
) -> Result<ScanSnapshot, McpInternalError> {
    let as_of = host.now_rfc3339();
    let exclude = normalize_excludes(exclude_paths)?;
    let stale_data = host.fetch_origin_target(workspace_root, target_branch).is_err();
    let remote_ref = format!("refs/remotes/origin/{target_branch}");
    let upstream_ref = format!("origin/{target_branch}");
    let target_oid = host.rev_parse_verify(workspace_root, &remote_ref)?;
    let merged_branches = host.branches_merged_into(workspace_root, &target_oid)?;
    let worktrees = host.list_worktrees(workspace_root)?;

    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    let mut errors = Vec::new();
    let mut removal_targets = Vec::new();
    let mut total_candidates_considered = 0usize;
    let mut truncated = false;

    for worktree in worktrees {
        if worktree.is_main || worktree.path == workspace_root {
            continue;
        }

        let relative_path = relative_path_string(workspace_root, &worktree.path)?;
        if exclude.contains(&relative_path) {
            continue;
        }

        let Some(branch) = worktree.branch.clone() else {
            skipped.push(CleanupSkipped {
                relative_path,
                reason: "detached-head".to_owned(),
                reason_human: "Skipped because the worktree is on a detached HEAD".to_owned(),
            });
            continue;
        };

        total_candidates_considered += 1;
        if total_candidates_considered > MAX_CANDIDATES {
            truncated = true;
            break;
        }

        let merged_via = if merged_branches.contains(&branch) {
            Some(MergedVia::Merge)
        } else if host.cherry_empty(workspace_root, &upstream_ref, &branch)? {
            Some(MergedVia::PatchEquivalent)
        } else {
            None
        };
        let is_merged = merged_via.is_some();
        let mut has_uncommitted_changes = false;
        let mut has_active_session = false;
        let mut skip_reason = None;
        let mut skip_reason_human = None;
        let own_candidate = worktree.path == own_worktree;

        if is_merged {
            has_uncommitted_changes = host.git_status(&worktree.path)?.dirty;
            has_active_session = own_candidate || has_any_session(host, &worktree.path);

            if has_uncommitted_changes {
                skip_reason = Some("uncommitted-changes".to_owned());
                skip_reason_human = Some("Skipped because there are uncommitted changes".to_owned());
            } else if has_active_session && !own_candidate {
                skip_reason = Some("active-session".to_owned());
                skip_reason_human = Some("Skipped because an AI session is still running in it".to_owned());
            } else if worktree.is_locked {
                skip_reason = Some("locked".to_owned());
                skip_reason_human = Some(lock_reason_human(worktree.lock_reason.as_deref()));
            }

            if own_candidate {
                errors.push(CleanupErrorEntry {
                    relative_path: relative_path.clone(),
                    code: Some("own-worktree-refused".to_owned()),
                    error: None,
                    user_action: Some(OWN_WORKTREE_USER_ACTION.to_owned()),
                });
            }
        }

        if let (Some(reason), Some(reason_human)) = (skip_reason.clone(), skip_reason_human.clone()) {
            skipped.push(CleanupSkipped {
                relative_path: relative_path.clone(),
                reason,
                reason_human,
            });
        }

        if is_merged && skip_reason.is_none() && !own_candidate {
            removal_targets.push(RemovalTarget {
                relative_path: relative_path.clone(),
                worktree_path: worktree.path.clone(),
                branch: branch.clone(),
                head_oid: worktree.head_oid.clone(),
            });
        }

        candidates.push(CleanupCandidate {
            relative_path,
            branch,
            head_oid: worktree.head_oid,
            merged_via,
            is_merged,
            has_uncommitted_changes,
            has_active_session,
            is_locked: worktree.is_locked,
            lock_reason: worktree.lock_reason,
            skip_reason,
            skip_reason_human,
        });
    }

    Ok(ScanSnapshot {
        target_branch: target_branch.to_owned(),
        candidates,
        skipped,
        errors,
        removal_targets,
        stale_data,
        truncated,
        total_candidates_considered,
        as_of,
    })
}

fn revalidate_execution<H: CleanupHost>(
    host: &mut H,
    workspace_root: &Path,
    own_worktree: &Path,
    scan: &ScanSnapshot,
) -> Result<(), McpInternalError> {
    let remote_ref = format!("refs/remotes/origin/{}", scan.target_branch);
    let upstream_ref = format!("origin/{}", scan.target_branch);
    let target_oid = host
        .rev_parse_verify(workspace_root, &remote_ref)
        .map_err(|_| confirmation_stale("the target branch could not be resolved during revalidation"))?;
    let merged_branches = host
        .branches_merged_into(workspace_root, &target_oid)
        .map_err(|_| confirmation_stale("the merged-branch snapshot changed during revalidation"))?;
    let current_worktrees = host
        .list_worktrees(workspace_root)
        .map_err(|_| confirmation_stale("the worktree listing changed during revalidation"))?;
    let current_by_path: HashMap<_, _> = current_worktrees.into_iter().map(|worktree| (worktree.path.clone(), worktree)).collect();

    for target in &scan.removal_targets {
        let Some(current) = current_by_path.get(&target.worktree_path) else {
            return Err(confirmation_stale_for(&target.relative_path, "the worktree disappeared before execution"));
        };

        if current.is_main {
            return Err(confirmation_stale_for(&target.relative_path, "the worktree is now the primary checkout"));
        }
        if current.is_locked {
            return Err(confirmation_stale_for(&target.relative_path, "the worktree is now locked"));
        }
        if current.path == own_worktree {
            return Err(confirmation_stale_for(
                &target.relative_path,
                "the calling session is now bound to that worktree",
            ));
        }
        if current.branch.as_deref() != Some(target.branch.as_str()) {
            return Err(confirmation_stale_for(&target.relative_path, "the checked-out branch changed"));
        }
        if current.head_oid != target.head_oid {
            return Err(confirmation_stale_for(&target.relative_path, "the branch tip changed"));
        }

        let still_merged = if merged_branches.contains(&target.branch) {
            true
        } else {
            host.cherry_empty(workspace_root, &upstream_ref, &target.branch)
                .map_err(|_| confirmation_stale_for(&target.relative_path, "the patch-equivalence check changed"))?
        };
        if !still_merged {
            return Err(confirmation_stale_for(&target.relative_path, "the branch is no longer merged"));
        }

        let status = host
            .git_status(&target.worktree_path)
            .map_err(|_| confirmation_stale_for(&target.relative_path, "the worktree status changed"))?;
        if status.dirty {
            return Err(confirmation_stale_for(&target.relative_path, "the worktree became dirty"));
        }
        if has_any_session(host, &target.worktree_path) {
            return Err(confirmation_stale_for(&target.relative_path, "a session became active in the worktree"));
        }
    }

    Ok(())
}

fn execute_removals<H: CleanupHost>(host: &mut H, workspace_root: &Path, scan: &ScanSnapshot) -> ExecuteResult {
    let mut outcomes = Vec::new();
    let mut removed = Vec::new();
    let mut partial = Vec::new();
    let mut errors = scan.errors.clone();

    for target in &scan.removal_targets {
        match host.remove_worktree(workspace_root, &target.worktree_path) {
            Ok(RemovalResultKind::Removed) => {
                removed.push(target.relative_path.clone());
                outcomes.push(CleanupRemovalOutcome::Removed {
                    relative_path: target.relative_path.clone(),
                });
            }
            Ok(RemovalResultKind::Partial { reason, error }) => {
                partial.push(CleanupPartialEntry {
                    relative_path: target.relative_path.clone(),
                    reason: reason.clone(),
                    error: error.clone(),
                });
                outcomes.push(CleanupRemovalOutcome::Partial {
                    relative_path: target.relative_path.clone(),
                    reason,
                    error,
                });
            }
            Err(err) => {
                let message = err.to_string();
                errors.push(CleanupErrorEntry {
                    relative_path: target.relative_path.clone(),
                    code: None,
                    error: Some(message.clone()),
                    user_action: None,
                });
                outcomes.push(CleanupRemovalOutcome::Error {
                    relative_path: target.relative_path.clone(),
                    error: message,
                });
            }
        }
    }

    ExecuteResult {
        dry_run: false,
        target_branch: scan.target_branch.clone(),
        outcomes,
        removed,
        partial,
        errors,
        skipped: scan.skipped.clone(),
        stale_data: false,
        as_of: scan.as_of.clone(),
    }
}

fn acquire_workspace_cleanup_guard(workspace_root: &Path) -> Result<WorkspaceCleanupGuard, McpInternalError> {
    let mut guard = match IN_FLIGHT_WORKSPACES.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if !guard.insert(workspace_root.to_path_buf()) {
        return Err(error(McpErrorCode::Busy, "Another cleanup is already in progress in this workspace"));
    }

    Ok(WorkspaceCleanupGuard {
        workspace_root: workspace_root.to_path_buf(),
    })
}

fn normalize_excludes(exclude_paths: &[String]) -> Result<HashSet<String>, McpInternalError> {
    exclude_paths.iter().map(|entry| normalize_relative_input(entry)).collect()
}

fn normalize_relative_input(input: &str) -> Result<String, McpInternalError> {
    if input.trim().is_empty() {
        return Err(error(McpErrorCode::InvalidArg, "excludePaths entries cannot be empty"));
    }

    let mut parts = Vec::new();
    for component in Path::new(input).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(error(
                    McpErrorCode::InvalidArg,
                    format!("excludePaths entry '{input}' must be a workspace-relative path"),
                ));
            }
        }
    }

    if parts.is_empty() {
        return Err(error(
            McpErrorCode::InvalidArg,
            format!("excludePaths entry '{input}' must contain at least one path component"),
        ));
    }

    Ok(parts.join("/"))
}

fn relative_path_string(workspace_root: &Path, worktree_path: &Path) -> Result<String, McpInternalError> {
    let relative = worktree_path.strip_prefix(workspace_root).map_err(|_| McpInternalError::Internal {
        message: format!(
            "worktree '{}' is not inside workspace root '{}'",
            worktree_path.display(),
            workspace_root.display()
        ),
    })?;

    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(McpInternalError::Internal {
                    message: format!(
                        "worktree '{}' escaped the workspace root '{}'",
                        worktree_path.display(),
                        workspace_root.display()
                    ),
                });
            }
        }
    }

    Ok(if parts.is_empty() { ".".to_owned() } else { parts.join("/") })
}

fn has_any_session<H: CleanupHost>(host: &H, worktree_path: &Path) -> bool {
    host.has_live_session(worktree_path)
        || host.has_persisted_session(worktree_path)
        || host.has_live_subsession(worktree_path)
        || host.has_persisted_subsession(worktree_path)
}

fn confirmation_fingerprint(scan: &ScanSnapshot) -> Result<[u8; 32], McpInternalError> {
    let mut candidates: Vec<_> = scan
        .removal_targets
        .iter()
        .map(|target| ConfirmationCandidateBinding {
            relative_path: target.relative_path.clone(),
            branch: target.branch.clone(),
            head_oid: target.head_oid.clone(),
        })
        .collect();
    candidates.sort_by(|left, right| left.relative_path.cmp(&right.relative_path).then(left.branch.cmp(&right.branch)));

    let binding = ConfirmationBinding {
        target_branch: scan.target_branch.clone(),
        stale_data: scan.stale_data,
        candidates,
    };
    let canonical = serde_json::to_string(&binding).map_err(|err| McpInternalError::Internal {
        message: format!("serialize cleanup confirmation fingerprint: {err}"),
    })?;
    Ok(fingerprint_args(&canonical))
}

fn build_confirmation_summary(workspace_root: &Path, count: usize) -> String {
    let workspace = workspace_root.file_name().and_then(|name| name.to_str()).unwrap_or("workspace");
    format!("Cleanup {count} merged worktrees in {workspace}")
}

fn lock_reason_human(lock_reason: Option<&str>) -> String {
    match lock_reason {
        Some(reason) if !reason.trim().is_empty() => format!("Skipped because the worktree is locked: {reason}"),
        _ => "Skipped because the worktree is locked".to_owned(),
    }
}

fn map_consume_error(err: ConsumeError) -> McpInternalError {
    match err {
        ConsumeError::Unknown => McpInternalError::InvalidConfirmation {
            message: "cleanup confirmation token is unknown".to_owned(),
        },
        ConsumeError::Expired => McpInternalError::ConfirmationExpired {
            message: "cleanup confirmation token expired".to_owned(),
        },
        ConsumeError::FingerprintMismatch => confirmation_stale("the approved candidate set changed"),
    }
}

fn confirmation_stale(reason: impl Into<String>) -> McpInternalError {
    McpInternalError::ConfirmationStale {
        message: format!("cleanup confirmation is stale: {}", reason.into()),
    }
}

fn confirmation_stale_for(relative_path: &str, reason: &str) -> McpInternalError {
    confirmation_stale(format!("{relative_path}: {reason}"))
}

fn duration_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn to_json<T: Serialize>(value: &T) -> Result<Value, McpInternalError> {
    serde_json::to_value(value).map_err(|err| McpInternalError::Internal {
        message: format!("serialize cleanup_merged_worktrees value: {err}"),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use tempfile::TempDir;

    use super::*;
    use crate::mcp::confirm::PendingMcpActionRegistry;
    use crate::mcp::types::McpToolName;

    struct TestWorkspace {
        _tempdir: TempDir,
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let tempdir = TempDir::new().expect("tempdir");
            let root = tempdir.path().join("workspace");
            std::fs::create_dir_all(&root).expect("workspace dir");
            Self { _tempdir: tempdir, root }
        }
    }

    #[derive(Debug, Clone)]
    enum PlannedRemoval {
        Removed,
        Partial { reason: String, error: String },
        Error { message: String },
    }

    struct TestHost {
        workspace_root: Option<PathBuf>,
        own_worktree: Option<PathBuf>,
        worktrees: Vec<CleanupWorktree>,
        merged_branches: HashSet<String>,
        patch_equivalent_branches: HashSet<String>,
        live_sessions: HashSet<PathBuf>,
        persisted_sessions: HashSet<PathBuf>,
        live_subsessions: HashSet<PathBuf>,
        persisted_subsessions: HashSet<PathBuf>,
        dirty_paths: HashSet<PathBuf>,
        remote_oids: HashMap<String, String>,
        fetch_fails: bool,
        pending_registry: PendingMcpActionRegistry,
        last_pending_action_id: Option<String>,
        removal_plans: HashMap<PathBuf, PlannedRemoval>,
        audit_entries: Vec<CleanupAuditEntry>,
        branches_merged_calls: usize,
        now: String,
        fetch_barriers: Option<(Arc<Barrier>, Arc<Barrier>)>,
    }

    impl TestHost {
        fn new(workspace_root: PathBuf) -> Self {
            let main_head = "main-head".to_owned();
            Self {
                workspace_root: Some(workspace_root.clone()),
                own_worktree: Some(workspace_root.clone()),
                worktrees: vec![CleanupWorktree {
                    path: workspace_root,
                    branch: Some(DEFAULT_TARGET_BRANCH.to_owned()),
                    head_oid: main_head.clone(),
                    is_main: true,
                    is_locked: false,
                    lock_reason: None,
                }],
                merged_branches: HashSet::new(),
                patch_equivalent_branches: HashSet::new(),
                live_sessions: HashSet::new(),
                persisted_sessions: HashSet::new(),
                live_subsessions: HashSet::new(),
                persisted_subsessions: HashSet::new(),
                dirty_paths: HashSet::new(),
                remote_oids: HashMap::from([(format!("refs/remotes/origin/{DEFAULT_TARGET_BRANCH}"), main_head)]),
                fetch_fails: false,
                pending_registry: PendingMcpActionRegistry::new(),
                last_pending_action_id: None,
                removal_plans: HashMap::new(),
                audit_entries: Vec::new(),
                branches_merged_calls: 0,
                now: "2026-01-01T00:00:00Z".to_owned(),
                fetch_barriers: None,
            }
        }

        fn path_for(&self, relative: &str) -> PathBuf {
            self.workspace_root.as_ref().expect("workspace root").join(relative)
        }

        fn add_worktree(&mut self, relative: &str, branch: &str, head_oid: &str) {
            let path = self.path_for(relative);
            std::fs::create_dir_all(&path).expect("worktree dir");
            self.worktrees.push(CleanupWorktree {
                path,
                branch: Some(branch.to_owned()),
                head_oid: head_oid.to_owned(),
                is_main: false,
                is_locked: false,
                lock_reason: None,
            });
        }

        fn add_detached_worktree(&mut self, relative: &str, head_oid: &str) {
            let path = self.path_for(relative);
            std::fs::create_dir_all(&path).expect("detached worktree dir");
            self.worktrees.push(CleanupWorktree {
                path,
                branch: None,
                head_oid: head_oid.to_owned(),
                is_main: false,
                is_locked: false,
                lock_reason: None,
            });
        }

        fn mark_merged(&mut self, branch: &str) {
            self.merged_branches.insert(branch.to_owned());
        }

        fn mark_patch_equivalent(&mut self, branch: &str) {
            self.patch_equivalent_branches.insert(branch.to_owned());
        }

        fn mark_live_session(&mut self, relative: &str) {
            self.live_sessions.insert(self.path_for(relative));
        }

        fn mark_persisted_session(&mut self, relative: &str) {
            self.persisted_sessions.insert(self.path_for(relative));
        }

        fn mark_dirty(&mut self, relative: &str) {
            self.dirty_paths.insert(self.path_for(relative));
        }

        fn clear_dirty(&mut self, relative: &str) {
            self.dirty_paths.remove(&self.path_for(relative));
        }

        fn set_locked(&mut self, relative: &str, reason: Option<&str>) {
            let path = self.path_for(relative);
            if let Some(worktree) = self.worktrees.iter_mut().find(|worktree| worktree.path == path) {
                worktree.is_locked = true;
                worktree.lock_reason = reason.map(str::to_owned);
            }
        }

        fn set_head_oid(&mut self, relative: &str, head_oid: &str) {
            let path = self.path_for(relative);
            if let Some(worktree) = self.worktrees.iter_mut().find(|worktree| worktree.path == path) {
                worktree.head_oid = head_oid.to_owned();
            }
        }

        fn set_own_worktree(&mut self, relative: &str) {
            self.own_worktree = Some(self.path_for(relative));
        }

        fn plan_removal(&mut self, relative: &str, plan: PlannedRemoval) {
            self.removal_plans.insert(self.path_for(relative), plan);
        }

        fn approve_last_pending(&mut self) -> String {
            let action_id = self.last_pending_action_id.clone().expect("pending action id");
            self.pending_registry.approve(&action_id).expect("approve pending action").token
        }
    }

    impl CleanupHost for TestHost {
        fn workspace_root(&self) -> Result<PathBuf, McpInternalError> {
            self.workspace_root.clone().ok_or_else(|| McpInternalError::WorkspaceUnbound {
                message: "cleanup_merged_worktrees requires an open workspace".to_owned(),
            })
        }

        fn own_worktree_path(&self, _session_id: &str) -> Result<PathBuf, McpInternalError> {
            self.own_worktree.clone().ok_or_else(|| McpInternalError::Internal {
                message: "own worktree not configured for test host".to_owned(),
            })
        }

        fn fetch_origin_target(&mut self, _workspace_root: &Path, _target_branch: &str) -> Result<(), McpInternalError> {
            if let Some((entered, release)) = &self.fetch_barriers {
                entered.wait();
                release.wait();
            }
            if self.fetch_fails {
                Err(McpInternalError::Internal {
                    message: "fetch failed".to_owned(),
                })
            } else {
                Ok(())
            }
        }

        fn list_worktrees(&self, _workspace_root: &Path) -> Result<Vec<CleanupWorktree>, McpInternalError> {
            Ok(self.worktrees.clone())
        }

        fn branches_merged_into(&mut self, _workspace_root: &Path, _target_oid: &str) -> Result<HashSet<String>, McpInternalError> {
            self.branches_merged_calls += 1;
            Ok(self.merged_branches.clone())
        }

        fn cherry_empty(&mut self, _workspace_root: &Path, _upstream_ref: &str, branch: &str) -> Result<bool, McpInternalError> {
            Ok(self.patch_equivalent_branches.contains(branch))
        }

        fn rev_parse_verify(&mut self, _workspace_root: &Path, ref_expr: &str) -> Result<String, McpInternalError> {
            self.remote_oids.get(ref_expr).cloned().ok_or_else(|| McpInternalError::InvalidArg {
                message: format!("ref not found: {ref_expr}"),
            })
        }

        fn git_status(&mut self, worktree_path: &Path) -> Result<WorktreeGitStatusSummary, McpInternalError> {
            let dirty = self.dirty_paths.contains(worktree_path);
            Ok(WorktreeGitStatusSummary {
                dirty,
                ahead_of_upstream: None,
                behind_upstream: None,
                file_count: if dirty { 1 } else { 0 },
                has_upstream: true,
                error: None,
            })
        }

        fn has_live_session(&self, worktree_path: &Path) -> bool {
            self.live_sessions.contains(worktree_path)
        }

        fn has_persisted_session(&self, worktree_path: &Path) -> bool {
            self.persisted_sessions.contains(worktree_path)
        }

        fn has_live_subsession(&self, worktree_path: &Path) -> bool {
            self.live_subsessions.contains(worktree_path)
        }

        fn has_persisted_subsession(&self, worktree_path: &Path) -> bool {
            self.persisted_subsessions.contains(worktree_path)
        }

        fn create_pending_action(
            &mut self,
            session_id: &str,
            summary: String,
            fingerprint: [u8; 32],
            payload: Value,
        ) -> Result<(), McpInternalError> {
            let pending = self
                .pending_registry
                .create(session_id.to_owned(), McpToolName::CleanupMergedWorktrees, summary, fingerprint, payload)?;
            self.last_pending_action_id = Some(pending.id);
            Ok(())
        }

        fn consume_confirmation(&mut self, token: &str, expected_fingerprint: &[u8; 32]) -> Result<(), ConsumeError> {
            self.pending_registry.try_consume(token, expected_fingerprint).map(|_| ())
        }

        fn remove_worktree(&mut self, _workspace_root: &Path, worktree_path: &Path) -> Result<RemovalResultKind, McpInternalError> {
            let planned = self.removal_plans.remove(worktree_path).unwrap_or(PlannedRemoval::Removed);
            match planned {
                PlannedRemoval::Removed => {
                    self.worktrees.retain(|worktree| worktree.path != worktree_path);
                    Ok(RemovalResultKind::Removed)
                }
                PlannedRemoval::Partial { reason, error } => {
                    self.worktrees.retain(|worktree| worktree.path != worktree_path);
                    Ok(RemovalResultKind::Partial { reason, error })
                }
                PlannedRemoval::Error { message } => Err(McpInternalError::Internal { message }),
            }
        }

        fn append_audit(&mut self, _session_id: &str, entry: CleanupAuditEntry) -> Result<(), McpInternalError> {
            self.audit_entries.push(entry);
            Ok(())
        }

        fn now_rfc3339(&self) -> String {
            self.now.clone()
        }
    }

    fn invoke_with_host(host: &mut TestHost, args: Value) -> Result<Value, McpInternalError> {
        let parsed = parse_args(args)?;
        run_cleanup(host, "session-1", parsed)
    }

    fn array_len(value: &Value, key: &str) -> usize {
        value[key].as_array().map_or(0, Vec::len)
    }

    #[test]
    fn invalid_target_branch_is_invalid_arg() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());

        let err = invoke_with_host(&mut host, json!({ "targetBranch": "-d-main" })).expect_err("invalid target branch should fail");

        assert_eq!(err.code(), McpErrorCode::InvalidArg);
    }

    #[test]
    fn execute_without_token_requires_confirmation() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");

        let err = invoke_with_host(&mut host, json!({ "dryRun": false })).expect_err("missing token should require confirmation");

        assert_eq!(err.code(), McpErrorCode::ConfirmationRequired);
        assert!(host.last_pending_action_id.is_some());
    }

    #[test]
    fn exclude_paths_over_limit_is_invalid_arg() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        let exclude_paths: Vec<_> = (0..=MAX_CANDIDATES).map(|idx| format!("feature-{idx}")).collect();

        let err = invoke_with_host(&mut host, json!({ "excludePaths": exclude_paths })).expect_err("too many excludes should fail");

        assert_eq!(err.code(), McpErrorCode::InvalidArg);
    }

    #[test]
    fn dry_run_happy_path_lists_candidates_would_remove_and_skipped() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        for (relative, branch, head) in [
            ("feature-a", "feature/a", "head-a"),
            ("feature-b", "feature/b", "head-b"),
            ("feature-c", "feature/c", "head-c"),
            ("feature-d", "feature/d", "head-d"),
        ] {
            host.add_worktree(relative, branch, head);
            host.mark_merged(branch);
        }
        host.mark_dirty("feature-d");

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(value["dryRun"], json!(true));
        assert_eq!(array_len(&value, "candidates"), 4);
        assert_eq!(array_len(&value, "wouldRemove"), 3);
        assert_eq!(array_len(&value, "skipped"), 1);
        assert_eq!(value["skipped"][0]["reasonHuman"], json!("Skipped because there are uncommitted changes"));
        assert_eq!(host.branches_merged_calls, 1, "merged branches should be queried once per invocation");
    }

    #[test]
    fn merged_with_active_session_is_skipped() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");
        host.mark_live_session("feature-a");

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(value["skipped"][0]["reason"], json!("active-session"));
        assert_eq!(array_len(&value, "wouldRemove"), 0);
    }

    #[test]
    fn merged_with_persisted_session_is_skipped() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");
        host.mark_persisted_session("feature-a");

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(value["skipped"][0]["reason"], json!("active-session"));
        assert_eq!(array_len(&value, "wouldRemove"), 0);
    }

    #[test]
    fn merged_with_lock_reason_is_skipped() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");
        host.set_locked("feature-a", Some("manual hold"));

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(value["skipped"][0]["reason"], json!("locked"));
        assert_eq!(
            value["skipped"][0]["reasonHuman"],
            json!("Skipped because the worktree is locked: manual hold")
        );
    }

    #[test]
    fn own_worktree_surfaces_own_worktree_refused() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");
        host.set_own_worktree("feature-a");

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(array_len(&value, "wouldRemove"), 0);
        assert_eq!(value["errors"][0]["code"], json!("own-worktree-refused"));
        assert_eq!(value["errors"][0]["relativePath"], json!("feature-a"));
    }

    #[test]
    fn patch_equivalent_branch_sets_merged_via() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_patch_equivalent("feature/a");

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(value["candidates"][0]["mergedVia"], json!("patch-equivalent"));
        assert_eq!(array_len(&value, "wouldRemove"), 1);
    }

    #[test]
    fn fetch_failure_sets_stale_data_and_execute_requires_acknowledgement() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.fetch_fails = true;
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");

        let dry_run = invoke_with_host(&mut host, json!({})).expect("dry run should succeed on stale data");
        assert_eq!(dry_run["staleData"], json!(true));

        let confirm_err = invoke_with_host(&mut host, json!({ "dryRun": false })).expect_err("execution should require confirmation");
        assert_eq!(confirm_err.code(), McpErrorCode::ConfirmationRequired);

        let token = host.approve_last_pending();
        let stale_err = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token.clone() }))
            .expect_err("execution without stale-data acknowledgement should fail");
        assert_eq!(stale_err.code(), McpErrorCode::StaleRemoteData);

        let execute = invoke_with_host(
            &mut host,
            json!({
                "dryRun": false,
                "confirmationToken": token,
                "allowStaleRemoteData": true
            }),
        )
        .expect("execution should succeed once stale data is acknowledged");
        assert_eq!(array_len(&execute, "removed"), 1);
    }

    #[test]
    fn execute_with_valid_token_removes_candidates() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        for (relative, branch, head) in [("feature-a", "feature/a", "head-a"), ("feature-b", "feature/b", "head-b")] {
            host.add_worktree(relative, branch, head);
            host.mark_merged(branch);
        }

        let confirm_err = invoke_with_host(&mut host, json!({ "dryRun": false })).expect_err("execution should require confirmation");
        assert_eq!(confirm_err.code(), McpErrorCode::ConfirmationRequired);

        let token = host.approve_last_pending();
        let execute = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token })).expect("execution should succeed");

        assert_eq!(execute["removed"], json!(["feature-a", "feature-b"]));
        assert_eq!(array_len(&execute, "outcomes"), 2);
        let audit = host.audit_entries.last().expect("audit entry");
        assert_eq!(audit.decision, McpAuditDecision::Approved);
        assert!(audit.summary.contains("Cleanup 2 merged worktrees"));
        assert_eq!(audit.result["removed"], json!(["feature-a", "feature-b"]));
        assert!(audit.duration_ms < 60_000);
    }

    #[test]
    fn partial_removal_is_reported() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");
        host.plan_removal(
            "feature-a",
            PlannedRemoval::Partial {
                reason: "missingButRegistered".to_owned(),
                error: "git still listed the worktree after disk cleanup".to_owned(),
            },
        );

        let confirm_err = invoke_with_host(&mut host, json!({ "dryRun": false })).expect_err("execution should require confirmation");
        assert_eq!(confirm_err.code(), McpErrorCode::ConfirmationRequired);

        let token = host.approve_last_pending();
        let execute = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token }))
            .expect("execution should surface the partial outcome");

        assert_eq!(array_len(&execute, "partial"), 1);
        assert_eq!(execute["partial"][0]["reason"], json!("missingButRegistered"));
        assert_eq!(execute["outcomes"][0]["kind"], json!("partial"));
    }

    #[test]
    fn head_oid_drift_returns_confirmation_stale_and_token_remains_valid() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");

        let confirm_err = invoke_with_host(&mut host, json!({ "dryRun": false })).expect_err("execution should require confirmation");
        assert_eq!(confirm_err.code(), McpErrorCode::ConfirmationRequired);

        let token = host.approve_last_pending();
        host.set_head_oid("feature-a", "head-b");
        let err = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token.clone() }))
            .expect_err("drift should invalidate the confirmation");
        assert_eq!(err.code(), McpErrorCode::ConfirmationStale);

        host.set_head_oid("feature-a", "head-a");
        let execute = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token }))
            .expect("fingerprint mismatch should not consume the token");
        assert_eq!(array_len(&execute, "removed"), 1);
    }

    #[test]
    fn dirty_drift_returns_confirmation_stale() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");

        let confirm_err = invoke_with_host(&mut host, json!({ "dryRun": false })).expect_err("execution should require confirmation");
        assert_eq!(confirm_err.code(), McpErrorCode::ConfirmationRequired);

        let token = host.approve_last_pending();
        host.mark_dirty("feature-a");
        let err = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token.clone() }))
            .expect_err("dirty drift should invalidate the confirmation");
        assert_eq!(err.code(), McpErrorCode::ConfirmationStale);

        host.clear_dirty("feature-a");
        let execute = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token }))
            .expect("fingerprint mismatch should not consume the token");
        assert_eq!(array_len(&execute, "removed"), 1);
    }

    #[test]
    fn mixed_success_reports_removed_errors_and_skipped() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        for idx in 0..8 {
            let relative = format!("feature-{idx}");
            let branch = format!("feature/{idx}");
            let head = format!("head-{idx}");
            host.add_worktree(&relative, &branch, &head);
            host.mark_merged(&branch);
        }
        host.mark_dirty("feature-6");
        host.mark_live_session("feature-7");
        host.plan_removal(
            "feature-5",
            PlannedRemoval::Error {
                message: "simulated remove failure".to_owned(),
            },
        );

        let confirm_err = invoke_with_host(&mut host, json!({ "dryRun": false })).expect_err("execution should require confirmation");
        assert_eq!(confirm_err.code(), McpErrorCode::ConfirmationRequired);

        let token = host.approve_last_pending();
        let execute = invoke_with_host(&mut host, json!({ "dryRun": false, "confirmationToken": token }))
            .expect("execution should complete with mixed outcomes");

        assert_eq!(array_len(&execute, "removed"), 5);
        assert_eq!(array_len(&execute, "errors"), 1);
        assert_eq!(array_len(&execute, "skipped"), 2);
        assert_eq!(execute["errors"][0]["error"], json!("simulated remove failure"));
    }

    #[test]
    fn truncated_limits_candidates_to_first_hundred() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        for idx in 0..=MAX_CANDIDATES {
            let relative = format!("feature-{idx:03}");
            let branch = format!("feature/{idx:03}");
            let head = format!("head-{idx:03}");
            host.add_worktree(&relative, &branch, &head);
            host.mark_merged(&branch);
        }

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(array_len(&value, "candidates"), MAX_CANDIDATES);
        assert_eq!(value["truncated"], json!(true));
        assert_eq!(value["totalCandidatesConsidered"], json!(MAX_CANDIDATES + 1));
    }

    #[test]
    fn busy_second_call_returns_busy() {
        let workspace = TestWorkspace::new();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        let mut host_one = TestHost::new(workspace.root.clone());
        host_one.add_worktree("feature-a", "feature/a", "head-a");
        host_one.mark_merged("feature/a");
        host_one.fetch_barriers = Some((Arc::clone(&entered), Arc::clone(&release)));

        let handle = std::thread::spawn(move || invoke_with_host(&mut host_one, json!({})));
        entered.wait();

        let mut host_two = TestHost::new(workspace.root.clone());
        host_two.add_worktree("feature-a", "feature/a", "head-a");
        host_two.mark_merged("feature/a");
        let err = invoke_with_host(&mut host_two, json!({})).expect_err("second concurrent cleanup should be rejected");
        assert_eq!(err.code(), McpErrorCode::Busy);

        release.wait();
        let first = handle.join().expect("thread join");
        assert!(first.is_ok(), "first invocation should complete once released");
    }

    #[test]
    fn workspace_unbound_returns_workspace_unbound() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.workspace_root = None;

        let err = invoke_with_host(&mut host, json!({})).expect_err("workspace-unbound should fail");

        assert_eq!(err.code(), McpErrorCode::WorkspaceUnbound);
    }

    #[test]
    fn detached_worktrees_are_skipped_from_candidates() {
        let workspace = TestWorkspace::new();
        let mut host = TestHost::new(workspace.root.clone());
        host.add_detached_worktree("detached", "detached-head");
        host.add_worktree("feature-a", "feature/a", "head-a");
        host.mark_merged("feature/a");

        let value = invoke_with_host(&mut host, json!({})).expect("dry run should succeed");

        assert_eq!(array_len(&value, "candidates"), 1);
        assert_eq!(value["skipped"][0]["reason"], json!("detached-head"));
    }
}
