use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tracing::warn;
use uuid::Uuid;

use crate::compose;
use crate::config_store::ConfigStore;
use crate::git::git_command_mcp_ro;
use crate::mcp::context::McpContext;
use crate::mcp::error::McpInternalError;
use crate::mcp::ipc::McpSessionRegistry;
use crate::mcp::types::McpToolDescriptor;
use crate::pty_pool::PtyPool;
use crate::types::{Session, SessionId, SessionStatus, Tool};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const STATUS_FETCH_CAP: usize = 100;
const LOCK_REASON_MAX_CHARS: usize = 200;

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawListWorktreesArgs {
    #[serde(default)]
    filter: RawListWorktreesFilter,
    #[serde(default)]
    include_status: bool,
    #[serde(default)]
    include_lock_reason: bool,
    #[serde(default)]
    include_non_running_sessions: bool,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    offset: Option<u64>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawListWorktreesFilter {
    #[serde(default)]
    dirty: Option<bool>,
    #[serde(default)]
    has_active_session: Option<bool>,
    #[serde(default)]
    tool: Option<Tool>,
    #[serde(default)]
    older_than_days: Option<u64>,
}

#[derive(Debug, Clone)]
struct ListWorktreesArgs {
    filter: ListWorktreesFilter,
    include_status: bool,
    include_lock_reason: bool,
    include_non_running_sessions: bool,
    limit: usize,
    offset: usize,
}

#[derive(Debug, Clone, Default)]
struct ListWorktreesFilter {
    dirty: Option<bool>,
    has_active_session: Option<bool>,
    tool: Option<Tool>,
    older_than_days: Option<u64>,
}

#[derive(Clone)]
struct BoundSessionContext {
    workspace_root: PathBuf,
    store: ConfigStore,
    pool: Arc<PtyPool>,
}

#[derive(Debug, Clone)]
struct WorktreeSnapshot {
    path: PathBuf,
    branch: Option<String>,
    head: Option<String>,
    is_main: bool,
    is_locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
    last_modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
struct DetailedGitStatus {
    dirty: bool,
    staged: u32,
    unstaged: u32,
    untracked: u32,
    ahead: Option<u32>,
    behind: Option<u32>,
}

#[derive(Debug, Clone)]
struct WorktreeCandidate {
    path: PathBuf,
    relative_path: String,
    branch: Option<String>,
    head: Option<String>,
    is_main: bool,
    is_locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
    active_session: Option<ActiveSessionSummary>,
    git_status: Option<DetailedGitStatus>,
    status_unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ActiveSessionSummary {
    tool: Tool,
    status: SessionStatus,
    label: String,
    count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ListWorktreesResult {
    worktrees: Vec<WorktreeEntry>,
    total: usize,
    truncated: bool,
    as_of: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorktreeEntry {
    relative_path: String,
    branch: Option<String>,
    head: Option<String>,
    is_main: bool,
    is_locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
    active_session: Option<ActiveSessionSummary>,
    git_status: Option<DetailedGitStatus>,
    status_unavailable: bool,
    status_unavailable_reason: Option<String>,
}

trait WorktreeInspector {
    fn list_worktrees(&self, workspace_root: &Path) -> Result<Vec<WorktreeSnapshot>, McpInternalError>;
    fn git_status(&self, worktree_path: &Path) -> Result<DetailedGitStatus, McpInternalError>;
}

#[derive(Debug, Default, Clone, Copy)]
struct ProductionWorktreeInspector;

#[derive(Debug)]
enum CommandExecutionError {
    Io(std::io::Error),
    Failed(Output),
    TimedOut,
}

#[must_use]
pub fn descriptor() -> McpToolDescriptor {
    McpToolDescriptor {
        name: "list_worktrees".to_owned(),
        description: "List Git worktrees in the current Arborist workspace with branch, lock state, active-session info, and a git-status summary. Use this for programmatic processing; for a human-readable workspace overview prefer `workspace_status`.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "object",
                    "properties": {
                        "dirty": { "type": "boolean" },
                        "hasActiveSession": { "type": "boolean" },
                        "tool": { "type": "string", "enum": ["claude", "copilot", "codex"] },
                        "olderThanDays": { "type": "integer", "minimum": 1 }
                    },
                    "additionalProperties": false
                },
                "includeStatus": { "type": "boolean", "default": false },
                "includeLockReason": { "type": "boolean", "default": false },
                "includeNonRunningSessions": { "type": "boolean", "default": false },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 100 },
                "offset": { "type": "integer", "minimum": 0, "default": 0 }
            },
            "additionalProperties": false
        }),
    }
}

pub async fn invoke(registry: &McpSessionRegistry, session_id: &str, args: Value) -> Result<Value, McpInternalError> {
    let parsed_args = parse_args(args)?;
    let bound = bound_session_context(registry, session_id)?;

    let result = tokio::task::spawn_blocking(move || {
        let inspector = ProductionWorktreeInspector;
        list_worktrees_core(
            &bound.workspace_root,
            &bound.store,
            bound.pool.as_ref(),
            &inspector,
            parsed_args,
            SystemTime::now(),
            Instant::now(),
        )
    })
    .await
    .map_err(|err| McpInternalError::Internal {
        message: format!("list_worktrees task panicked: {err}"),
    })??;

    serde_json::to_value(result).map_err(|err| McpInternalError::Internal {
        message: format!("failed to serialize list_worktrees result: {err}"),
    })
}

fn parse_args(args: Value) -> Result<ListWorktreesArgs, McpInternalError> {
    let raw: RawListWorktreesArgs = serde_json::from_value(args).map_err(|err| McpInternalError::InvalidArg {
        message: format!("invalid list_worktrees arguments: {err}"),
    })?;

    let limit_raw = raw.limit.unwrap_or(DEFAULT_LIMIT as u64);
    if !(1..=MAX_LIMIT as u64).contains(&limit_raw) {
        return Err(McpInternalError::InvalidArg {
            message: format!("limit must be between 1 and {MAX_LIMIT}"),
        });
    }
    let offset_raw = raw.offset.unwrap_or(0);
    let limit = usize::try_from(limit_raw).map_err(|_| McpInternalError::InvalidArg {
        message: "limit is too large for this platform".to_owned(),
    })?;
    let offset = usize::try_from(offset_raw).map_err(|_| McpInternalError::InvalidArg {
        message: "offset is too large for this platform".to_owned(),
    })?;

    if raw.filter.older_than_days.is_some_and(|days| days == 0) {
        return Err(McpInternalError::InvalidArg {
            message: "filter.olderThanDays must be at least 1".to_owned(),
        });
    }

    Ok(ListWorktreesArgs {
        filter: ListWorktreesFilter {
            dirty: raw.filter.dirty,
            has_active_session: raw.filter.has_active_session,
            tool: raw.filter.tool,
            older_than_days: raw.filter.older_than_days,
        },
        include_status: raw.include_status,
        include_lock_reason: raw.include_lock_reason,
        include_non_running_sessions: raw.include_non_running_sessions,
        limit,
        offset,
    })
}

fn bound_session_context(registry: &McpSessionRegistry, session_id: &str) -> Result<BoundSessionContext, McpInternalError> {
    let context = registry_context(registry);
    let (workspace_root, store) = {
        let workspace = match context.app.workspace.read() {
            Ok(workspace) => workspace,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(workspace_root) = workspace.workspace_root.clone() else {
            return Err(McpInternalError::WorkspaceUnbound {
                message: "Open a workspace in Arborist before retrying list_worktrees".to_owned(),
            });
        };
        let Some(store) = workspace.store.clone() else {
            return Err(McpInternalError::WorkspaceUnbound {
                message: "Open a workspace in Arborist before retrying list_worktrees".to_owned(),
            });
        };
        (workspace_root, store)
    };

    let parsed_session_id = SessionId(Uuid::parse_str(session_id).map_err(|err| McpInternalError::InvalidArg {
        message: format!("session_id must be a UUID: {err}"),
    })?);
    if !store.load_sessions().contains_key(&parsed_session_id) {
        return Err(McpInternalError::WorkspaceUnbound {
            message: format!("session '{session_id}' is not bound to a workspace"),
        });
    }

    Ok(BoundSessionContext {
        workspace_root,
        store,
        pool: Arc::clone(&context.app.pool),
    })
}

fn list_worktrees_core(
    workspace_root: &Path,
    store: &ConfigStore,
    pool: &PtyPool,
    inspector: &dyn WorktreeInspector,
    args: ListWorktreesArgs,
    now: SystemTime,
    started_at: Instant,
) -> Result<ListWorktreesResult, McpInternalError> {
    let workspace_root = compose::validate_worktree(workspace_root).map_err(|_| McpInternalError::WorkspaceUnbound {
        message: "Open a workspace in Arborist before retrying list_worktrees".to_owned(),
    })?;
    let effective_include_status = args.include_status || args.filter.dirty == Some(true);
    let sessions_by_worktree = build_session_index(store, pool, args.include_non_running_sessions);
    let mut candidates = Vec::new();

    for worktree in inspector.list_worktrees(&workspace_root)? {
        ensure_call_not_timed_out(started_at)?;
        let Some((validated_path, relative_path)) = validate_relative_worktree_path(&workspace_root, &worktree.path) else {
            continue;
        };
        let matching_sessions = sessions_by_worktree.get(&validated_path).cloned().unwrap_or_default();
        if !matches_session_filters(&matching_sessions, &args.filter) {
            continue;
        }
        if !matches_age_filter(worktree.last_modified, args.filter.older_than_days, now) {
            continue;
        }

        candidates.push(WorktreeCandidate {
            path: validated_path,
            relative_path,
            branch: worktree.branch,
            head: worktree.head,
            is_main: worktree.is_main,
            is_locked: worktree.is_locked,
            lock_reason: worktree.lock_reason,
            prunable: worktree.prunable,
            active_session: summarize_active_session(&matching_sessions, pool),
            git_status: None,
            status_unavailable_reason: None,
        });
    }

    if effective_include_status {
        for (index, candidate) in candidates.iter_mut().enumerate() {
            ensure_call_not_timed_out(started_at)?;
            if index >= STATUS_FETCH_CAP {
                candidate.status_unavailable_reason = Some("status-fetch-capped".to_owned());
                continue;
            }
            if !candidate.path.is_dir() {
                continue;
            }
            match inspector.git_status(&candidate.path) {
                Ok(status) => candidate.git_status = Some(status),
                Err(McpInternalError::Busy { .. }) => {
                    return Err(McpInternalError::Busy {
                        message: "list_worktrees timed out after 5 seconds".to_owned(),
                    })
                }
                Err(_) => candidate.git_status = None,
            }
        }
    }

    if args.filter.dirty == Some(true) {
        candidates.retain(|candidate| candidate.git_status.as_ref().is_some_and(|status| status.dirty));
    }

    ensure_call_not_timed_out(started_at)?;

    let total = candidates.len();
    let worktrees = candidates
        .into_iter()
        .skip(args.offset)
        .take(args.limit)
        .map(|candidate| WorktreeEntry {
            relative_path: candidate.relative_path,
            branch: candidate.branch,
            head: candidate.head,
            is_main: candidate.is_main,
            is_locked: candidate.is_locked,
            lock_reason: if args.include_lock_reason {
                candidate.lock_reason.as_deref().and_then(sanitize_lock_reason)
            } else {
                None
            },
            prunable: candidate.prunable,
            active_session: candidate.active_session,
            git_status: candidate.git_status,
            status_unavailable: candidate.status_unavailable_reason.is_some(),
            status_unavailable_reason: candidate.status_unavailable_reason,
        })
        .collect::<Vec<_>>();
    let truncated = args.offset.saturating_add(worktrees.len()) < total;

    Ok(ListWorktreesResult {
        worktrees,
        total,
        truncated,
        as_of: format_rfc3339(now)?,
    })
}

fn build_session_index(store: &ConfigStore, pool: &PtyPool, include_non_running_sessions: bool) -> HashMap<PathBuf, Vec<Session>> {
    let mut sessions_by_worktree = HashMap::<PathBuf, Vec<Session>>::new();

    for session in store.load_sessions().into_values() {
        let is_running = pool.contains(&session.id);
        if !include_non_running_sessions && !is_running {
            continue;
        }
        let Ok(worktree_path) = compose::validate_worktree(&session.worktree_path) else {
            continue;
        };
        sessions_by_worktree.entry(worktree_path).or_default().push(session);
    }

    for sessions in sessions_by_worktree.values_mut() {
        sessions.sort_by(|left, right| {
            let left_key = (pool.contains(&left.id), left.created_at);
            let right_key = (pool.contains(&right.id), right.created_at);
            right_key.cmp(&left_key)
        });
    }

    sessions_by_worktree
}

fn summarize_active_session(sessions: &[Session], pool: &PtyPool) -> Option<ActiveSessionSummary> {
    let session = sessions.first()?;
    Some(ActiveSessionSummary {
        tool: session.tool,
        status: if pool.contains(&session.id) {
            SessionStatus::Running
        } else {
            session.status
        },
        label: session.label.clone(),
        count: sessions.len(),
    })
}

fn matches_session_filters(sessions: &[Session], filter: &ListWorktreesFilter) -> bool {
    if let Some(has_active_session) = filter.has_active_session {
        if has_active_session == sessions.is_empty() {
            return false;
        }
    }
    if let Some(tool) = filter.tool {
        if !sessions.iter().any(|session| session.tool == tool) {
            return false;
        }
    }
    true
}

fn matches_age_filter(last_modified: Option<SystemTime>, older_than_days: Option<u64>, now: SystemTime) -> bool {
    let Some(days) = older_than_days else {
        return true;
    };
    let Some(last_modified) = last_modified else {
        return false;
    };
    let threshold = Duration::from_secs(days.saturating_mul(24 * 60 * 60));
    match now.duration_since(last_modified) {
        Ok(age) => age >= threshold,
        Err(_) => false,
    }
}

fn validate_relative_worktree_path(workspace_root: &Path, worktree_path: &Path) -> Option<(PathBuf, String)> {
    let resolved_path = if worktree_path.exists() {
        dunce::canonicalize(worktree_path).ok()?
    } else if worktree_path.is_absolute() {
        worktree_path.to_path_buf()
    } else {
        return None;
    };
    let relative_path = resolved_path.strip_prefix(workspace_root).ok()?;
    if relative_path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return None;
    }
    let relative_path = if relative_path.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        relative_path.display().to_string()
    };
    Some((resolved_path, relative_path))
}

fn sanitize_lock_reason(reason: &str) -> Option<String> {
    let cleaned = reason.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect::<String>();
    if cleaned.trim().is_empty() {
        return None;
    }
    let redacted = cleaned
        .split_whitespace()
        .map(|token| if looks_like_path(token) { "<path>" } else { token })
        .collect::<Vec<_>>()
        .join(" ");
    let sanitized = redacted.chars().take(LOCK_REASON_MAX_CHARS).collect::<String>();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn looks_like_path(token: &str) -> bool {
    token.contains('/') || token.contains('\\') || token.contains(':')
}

fn ensure_call_not_timed_out(started_at: Instant) -> Result<(), McpInternalError> {
    if started_at.elapsed() > CALL_TIMEOUT {
        return Err(McpInternalError::Busy {
            message: "list_worktrees timed out after 5 seconds".to_owned(),
        });
    }
    Ok(())
}

fn format_rfc3339(now: SystemTime) -> Result<String, McpInternalError> {
    OffsetDateTime::from(now).format(&Rfc3339).map_err(|err| McpInternalError::Internal {
        message: format!("failed to format list_worktrees timestamp: {err}"),
    })
}

impl WorktreeInspector for ProductionWorktreeInspector {
    fn list_worktrees(&self, workspace_root: &Path) -> Result<Vec<WorktreeSnapshot>, McpInternalError> {
        if !workspace_root.is_dir() {
            return Ok(Vec::new());
        }

        let mut cmd = git_command_mcp_ro(workspace_root);
        cmd.args(["worktree", "list", "--porcelain"]);
        let output = match run_command_output_with_timeout(cmd, CALL_TIMEOUT) {
            Ok(output) => output,
            Err(CommandExecutionError::TimedOut) => {
                return Err(McpInternalError::Busy {
                    message: "list_worktrees timed out after 5 seconds".to_owned(),
                })
            }
            Err(CommandExecutionError::Io(err)) => {
                warn!(code = "GitUnavailable", path = %workspace_root.display(), %err, "git worktree list failed");
                return Ok(Vec::new());
            }
            Err(CommandExecutionError::Failed(output)) => {
                warn!(
                    code = "GitUnavailable",
                    path = %workspace_root.display(),
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "git worktree list failed"
                );
                return Ok(Vec::new());
            }
        };

        Ok(parse_worktree_porcelain(&String::from_utf8_lossy(&output.stdout)))
    }

    fn git_status(&self, worktree_path: &Path) -> Result<DetailedGitStatus, McpInternalError> {
        let mut cmd = git_command_mcp_ro(worktree_path);
        cmd.args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"]);
        let output = match run_command_output_with_timeout(cmd, CALL_TIMEOUT) {
            Ok(output) => output,
            Err(CommandExecutionError::TimedOut) => {
                return Err(McpInternalError::Busy {
                    message: "list_worktrees timed out after 5 seconds".to_owned(),
                })
            }
            Err(CommandExecutionError::Io(err)) => {
                return Err(McpInternalError::Internal {
                    message: format!("git status failed: {err}"),
                })
            }
            Err(CommandExecutionError::Failed(output)) => {
                return Err(McpInternalError::Internal {
                    message: format!("git status failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
                })
            }
        };

        Ok(parse_git_status(&output.stdout))
    }
}

fn parse_worktree_porcelain(input: &str) -> Vec<WorktreeSnapshot> {
    let mut worktrees = Vec::new();
    let mut current: Option<PartialWorktreeSnapshot> = None;
    let mut is_first_block = true;

    for raw_line in input.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            if let Some(worktree) = current.take() {
                if let Some(snapshot) = worktree.finish(is_first_block) {
                    worktrees.push(snapshot);
                }
                is_first_block = false;
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(worktree) = current.take() {
                if let Some(snapshot) = worktree.finish(is_first_block) {
                    worktrees.push(snapshot);
                }
                is_first_block = false;
            }
            current = Some(PartialWorktreeSnapshot::new(PathBuf::from(path)));
            continue;
        }

        let Some(worktree) = current.as_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            worktree.head = non_empty(head.trim());
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            worktree.branch = Some(branch_ref.strip_prefix("refs/heads/").unwrap_or(branch_ref).to_owned());
        } else if line == "detached" {
            worktree.branch = None;
        } else if line == "locked" {
            worktree.is_locked = true;
            worktree.lock_reason = None;
        } else if let Some(reason) = line.strip_prefix("locked ") {
            worktree.is_locked = true;
            worktree.lock_reason = non_empty(reason.trim());
        } else if line == "prunable" || line.starts_with("prunable ") {
            worktree.prunable = true;
        }
    }

    if let Some(worktree) = current.take() {
        if let Some(snapshot) = worktree.finish(is_first_block) {
            worktrees.push(snapshot);
        }
    }

    worktrees
}

#[derive(Debug)]
struct PartialWorktreeSnapshot {
    path: PathBuf,
    branch: Option<String>,
    head: Option<String>,
    is_locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
}

impl PartialWorktreeSnapshot {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            branch: None,
            head: None,
            is_locked: false,
            lock_reason: None,
            prunable: false,
        }
    }

    fn finish(self, is_main: bool) -> Option<WorktreeSnapshot> {
        if self.path.as_os_str().is_empty() {
            return None;
        }
        let last_modified = std::fs::metadata(&self.path).ok().and_then(|metadata| metadata.modified().ok());
        Some(WorktreeSnapshot {
            path: self.path,
            branch: self.branch,
            head: self.head,
            is_main,
            is_locked: self.is_locked,
            lock_reason: self.lock_reason,
            prunable: self.prunable,
            last_modified,
        })
    }
}

fn parse_git_status(input: &[u8]) -> DetailedGitStatus {
    let mut status = DetailedGitStatus::default();

    for raw_record in input.split(|byte| *byte == 0) {
        if raw_record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(raw_record);
        if let Some(rest) = record.strip_prefix("# branch.ab ") {
            parse_branch_ab(&mut status, rest);
            continue;
        }
        match record.as_bytes().first().copied() {
            Some(b'1') | Some(b'2') | Some(b'u') => parse_xy_record(&mut status, &record),
            Some(b'?') => status.untracked = status.untracked.saturating_add(1),
            _ => {}
        }
    }

    status.dirty = status.staged > 0 || status.unstaged > 0 || status.untracked > 0;
    status
}

fn parse_branch_ab(status: &mut DetailedGitStatus, rest: &str) {
    let mut parts = rest.split_whitespace();
    if let Some(ahead) = parts
        .next()
        .and_then(|part| part.strip_prefix('+'))
        .and_then(|part| part.parse::<u32>().ok())
    {
        status.ahead = Some(ahead);
    }
    if let Some(behind) = parts
        .next()
        .and_then(|part| part.strip_prefix('-'))
        .and_then(|part| part.parse::<u32>().ok())
    {
        status.behind = Some(behind);
    }
}

fn parse_xy_record(status: &mut DetailedGitStatus, record: &str) {
    let mut parts = record.split_whitespace();
    let _ = parts.next();
    let xy = parts.next().unwrap_or("..");
    let mut chars = xy.chars();
    let staged = chars.next().unwrap_or('.');
    let unstaged = chars.next().unwrap_or('.');
    if staged != '.' && staged != ' ' {
        status.staged = status.staged.saturating_add(1);
    }
    if unstaged != '.' && unstaged != ' ' {
        status.unstaged = status.unstaged.saturating_add(1);
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn run_command_output_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, CommandExecutionError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(CommandExecutionError::Io)?;
    let started_at = Instant::now();

    loop {
        match child.try_wait().map_err(CommandExecutionError::Io)? {
            Some(status) => {
                let output = child.wait_with_output().map_err(CommandExecutionError::Io)?;
                if status.success() {
                    return Ok(output);
                }
                return Err(CommandExecutionError::Failed(output));
            }
            None if started_at.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait_with_output();
                return Err(CommandExecutionError::TimedOut);
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
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
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    use super::*;
    use crate::commands::AppContext;
    use crate::config_store::ConfigStore;
    use crate::mcp::types::McpContextConfig;
    use crate::pty_pool::{PortablePtySpawner, PtyPool, PtySink};
    use crate::types::{Error, SessionId};
    use crate::workspace_scope::WorkspaceScope;

    #[derive(Default)]
    struct FakeInspector {
        worktrees: Vec<WorktreeSnapshot>,
        statuses: HashMap<PathBuf, Result<DetailedGitStatus, McpInternalError>>,
    }

    impl WorktreeInspector for FakeInspector {
        fn list_worktrees(&self, _workspace_root: &Path) -> Result<Vec<WorktreeSnapshot>, McpInternalError> {
            Ok(self.worktrees.clone())
        }

        fn git_status(&self, worktree_path: &Path) -> Result<DetailedGitStatus, McpInternalError> {
            self.statuses
                .get(worktree_path)
                .cloned()
                .unwrap_or_else(|| Ok(DetailedGitStatus::default()))
        }
    }

    fn null_sink() -> PtySink {
        let output = Arc::new(|_id: &SessionId, _data: String| {});
        let status = Arc::new(|_id: &SessionId, _status: SessionStatus, _pid: Option<u32>, _message: Option<String>| {});
        PtySink::new(output, status, Arc::new(|_id, _evt| {}))
    }

    fn build_store(workspace_root: &Path) -> ConfigStore {
        let store = ConfigStore::open(workspace_root.join("store")).expect("store");
        store
            .save_config(crate::types::PartialAppConfig {
                workspace_root: Some(Some(workspace_root.to_path_buf())),
                ..Default::default()
            })
            .expect("save config");
        store
    }

    fn build_pool() -> Arc<PtyPool> {
        Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)))
    }

    fn build_registry(workspace_root: Option<PathBuf>, store: Option<ConfigStore>) -> Arc<McpSessionRegistry> {
        let pool = build_pool();
        let workspace = if let (Some(workspace_root), Some(store)) = (workspace_root, store) {
            WorkspaceScope::for_test(store, Some(workspace_root))
        } else {
            WorkspaceScope::unbound()
        };
        let app = Arc::new(AppContext::with_workspace(
            pool,
            Arc::new(RwLock::new(workspace)),
            null_sink(),
            Arc::new(NoopGitRunner),
            Arc::new(|_| {}),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| {}),
        ));
        let state_dir = TempDir::new().expect("state dir");
        let state_path = state_dir.path().to_path_buf();
        let mcp = Arc::new(crate::mcp::McpContext::new(app, McpContextConfig::default(), state_path).expect("mcp ctx"));
        Arc::new(McpSessionRegistry::new(mcp))
    }

    struct NoopGitRunner;

    impl crate::git::GitRunner for NoopGitRunner {
        fn list_worktrees(&self, _repo_root: &Path) -> Result<Vec<crate::types::WorktreeInfo>, Error> {
            Ok(Vec::new())
        }
        fn git_toplevel(&self, _path: &Path) -> Result<Option<PathBuf>, Error> {
            Ok(None)
        }
        fn create_worktree(&self, repo_root: &Path, relative_path: &Path, _branch: &str) -> Result<PathBuf, Error> {
            Ok(repo_root.join(relative_path))
        }
        fn remove_worktree(&self, _repo_root: &Path, _worktree_path: &Path) -> Result<(), Error> {
            Ok(())
        }
        fn git_status(&self, _worktree_path: &Path) -> Result<crate::types::WorktreeGitStatus, Error> {
            Ok(crate::types::WorktreeGitStatus::default())
        }
        fn fetch_origin(&self, _root: &Path, _timeout: Duration) -> Result<(), crate::git::GitError> {
            Ok(())
        }
        fn branches_merged_into(&self, _root: &Path, _target_oid: &str) -> Result<std::collections::HashSet<String>, crate::git::GitError> {
            Ok(std::collections::HashSet::new())
        }
        fn cherry_empty(&self, _root: &Path, _upstream_oid: &str, _branch: &str) -> Result<bool, crate::git::GitError> {
            Ok(true)
        }
        fn merge_from_branch(
            &self,
            _worktree: &Path,
            _source_oid: &str,
            _leave_conflicts: bool,
            _timeout: Duration,
        ) -> Result<crate::git::MergeFromBranchOutcome, crate::git::GitError> {
            Ok(crate::git::MergeFromBranchOutcome::AlreadyUpToDate)
        }
        fn default_branch(&self, _root: &Path) -> Result<crate::git::DefaultBranchInfo, crate::git::GitError> {
            Ok(crate::git::DefaultBranchInfo {
                branch: "main".to_owned(),
                source: crate::git::DefaultBranchSource::Main,
            })
        }
        fn rev_parse_verify(&self, _root: &Path, _ref_expr: &str) -> Result<String, crate::git::GitError> {
            Ok("deadbeef".to_owned())
        }
        fn git_status_mcp(&self, _worktree: &Path) -> Result<crate::git::WorktreeGitStatusSummary, crate::git::GitError> {
            Ok(crate::git::WorktreeGitStatusSummary::default())
        }
        fn merge_tree_dry_run(&self, _root: &Path, _base_oid: &str, _source_oid: &str) -> Result<crate::git::MergeTreeOutcome, crate::git::GitError> {
            Ok(crate::git::MergeTreeOutcome::Unsupported)
        }
        fn merge_abort(&self, _worktree: &Path) -> Result<(), crate::git::GitError> {
            Ok(())
        }
        fn has_merge_head(&self, _worktree: &Path) -> Result<bool, crate::git::GitError> {
            Ok(false)
        }
    }

    fn run_core(
        workspace_root: &Path,
        store: &ConfigStore,
        inspector: &FakeInspector,
        args: ListWorktreesArgs,
        now: SystemTime,
    ) -> ListWorktreesResult {
        let pool = build_pool();
        list_worktrees_core(workspace_root, store, pool.as_ref(), inspector, args, now, Instant::now()).expect("list worktrees")
    }

    fn args_with(filter: ListWorktreesFilter) -> ListWorktreesArgs {
        ListWorktreesArgs {
            filter,
            include_status: false,
            include_lock_reason: false,
            include_non_running_sessions: false,
            limit: DEFAULT_LIMIT,
            offset: 0,
        }
    }

    #[test]
    fn rejects_unknown_argument_field() {
        let err = parse_args(json!({"extra": true})).expect_err("invalid args");
        assert!(matches!(err, McpInternalError::InvalidArg { .. }));
    }

    #[test]
    fn rejects_limit_above_cap() {
        let err = parse_args(json!({"limit": 501})).expect_err("invalid args");
        assert!(matches!(err, McpInternalError::InvalidArg { .. }));
    }

    #[test]
    fn rejects_negative_offset() {
        let err = parse_args(json!({"offset": -1})).expect_err("invalid args");
        assert!(matches!(err, McpInternalError::InvalidArg { .. }));
    }

    #[test]
    fn happy_path_returns_three_entries_without_status() {
        let workspace = TempDir::new().expect("workspace");
        let main = workspace.path().to_path_buf();
        let feature_a = workspace.path().join(".arborist").join(".worktrees").join("feature-a");
        let feature_b = workspace.path().join(".arborist").join(".worktrees").join("feature-b");
        std::fs::create_dir_all(&feature_a).expect("feature a");
        std::fs::create_dir_all(&feature_b).expect("feature b");
        let store = build_store(workspace.path());
        let inspector = FakeInspector {
            worktrees: vec![
                WorktreeSnapshot {
                    path: main.clone(),
                    branch: Some("main".to_owned()),
                    head: Some("a1".to_owned()),
                    is_main: true,
                    is_locked: false,
                    lock_reason: None,
                    prunable: false,
                    last_modified: Some(SystemTime::now()),
                },
                WorktreeSnapshot {
                    path: feature_a.clone(),
                    branch: Some("feature-a".to_owned()),
                    head: Some("b2".to_owned()),
                    is_main: false,
                    is_locked: false,
                    lock_reason: None,
                    prunable: false,
                    last_modified: Some(SystemTime::now()),
                },
                WorktreeSnapshot {
                    path: feature_b.clone(),
                    branch: Some("feature-b".to_owned()),
                    head: Some("c3".to_owned()),
                    is_main: false,
                    is_locked: false,
                    lock_reason: None,
                    prunable: false,
                    last_modified: Some(SystemTime::now()),
                },
            ],
            statuses: HashMap::new(),
        };

        let result = run_core(
            workspace.path(),
            &store,
            &inspector,
            args_with(ListWorktreesFilter::default()),
            SystemTime::now(),
        );
        assert_eq!(result.worktrees.len(), 3);
        assert!(result.worktrees.iter().all(|worktree| worktree.git_status.is_none()));
        assert_eq!(result.total, 3);
        assert!(!result.truncated);
    }

    #[test]
    fn dirty_filter_returns_only_dirty_worktrees() {
        let workspace = TempDir::new().expect("workspace");
        let dirty = workspace.path().join(".arborist").join(".worktrees").join("dirty");
        let clean = workspace.path().join(".arborist").join(".worktrees").join("clean");
        std::fs::create_dir_all(&dirty).expect("dirty dir");
        std::fs::create_dir_all(&clean).expect("clean dir");
        let store = build_store(workspace.path());
        let inspector = FakeInspector {
            worktrees: vec![
                WorktreeSnapshot {
                    path: dirty.clone(),
                    branch: Some("dirty".to_owned()),
                    head: Some("d1".to_owned()),
                    is_main: false,
                    is_locked: false,
                    lock_reason: None,
                    prunable: false,
                    last_modified: Some(SystemTime::now()),
                },
                WorktreeSnapshot {
                    path: clean.clone(),
                    branch: Some("clean".to_owned()),
                    head: Some("c1".to_owned()),
                    is_main: false,
                    is_locked: false,
                    lock_reason: None,
                    prunable: false,
                    last_modified: Some(SystemTime::now()),
                },
            ],
            statuses: HashMap::from([
                (
                    dirty.clone(),
                    Ok(DetailedGitStatus {
                        dirty: true,
                        staged: 1,
                        unstaged: 0,
                        untracked: 0,
                        ahead: Some(1),
                        behind: Some(0),
                    }),
                ),
                (clean.clone(), Ok(DetailedGitStatus::default())),
            ]),
        };

        let result = run_core(
            workspace.path(),
            &store,
            &inspector,
            args_with(ListWorktreesFilter {
                dirty: Some(true),
                ..Default::default()
            }),
            SystemTime::now(),
        );
        assert_eq!(result.worktrees.len(), 1);
        assert_eq!(result.worktrees[0].relative_path, ".arborist\\.worktrees\\dirty");
    }

    #[test]
    fn include_status_populates_git_status() {
        let workspace = TempDir::new().expect("workspace");
        let dirty = workspace.path().join(".arborist").join(".worktrees").join("dirty");
        std::fs::create_dir_all(&dirty).expect("dirty dir");
        let store = build_store(workspace.path());
        let inspector = FakeInspector {
            worktrees: vec![WorktreeSnapshot {
                path: dirty.clone(),
                branch: Some("dirty".to_owned()),
                head: Some("d1".to_owned()),
                is_main: false,
                is_locked: false,
                lock_reason: None,
                prunable: false,
                last_modified: Some(SystemTime::now()),
            }],
            statuses: HashMap::from([(
                dirty.clone(),
                Ok(DetailedGitStatus {
                    dirty: true,
                    staged: 2,
                    unstaged: 1,
                    untracked: 3,
                    ahead: Some(4),
                    behind: Some(2),
                }),
            )]),
        };

        let result = run_core(
            workspace.path(),
            &store,
            &inspector,
            ListWorktreesArgs {
                include_status: true,
                ..args_with(ListWorktreesFilter::default())
            },
            SystemTime::now(),
        );
        assert_eq!(result.worktrees[0].git_status.as_ref().expect("status").staged, 2);
        assert_eq!(result.worktrees[0].git_status.as_ref().expect("status").untracked, 3);
    }

    #[test]
    fn pagination_returns_requested_window_and_sets_truncated() {
        let workspace = TempDir::new().expect("workspace");
        let store = build_store(workspace.path());
        let mut worktrees = Vec::new();
        for index in 0..10 {
            let worktree = workspace.path().join(".arborist").join(".worktrees").join(format!("wt-{index}"));
            std::fs::create_dir_all(&worktree).expect("worktree dir");
            worktrees.push(WorktreeSnapshot {
                path: worktree,
                branch: Some(format!("wt-{index}")),
                head: Some(format!("{index}")),
                is_main: index == 0,
                is_locked: false,
                lock_reason: None,
                prunable: false,
                last_modified: Some(SystemTime::now()),
            });
        }
        let inspector = FakeInspector {
            worktrees,
            statuses: HashMap::new(),
        };

        let result = run_core(
            workspace.path(),
            &store,
            &inspector,
            ListWorktreesArgs {
                limit: 3,
                offset: 3,
                ..args_with(ListWorktreesFilter::default())
            },
            SystemTime::now(),
        );
        assert_eq!(result.worktrees.len(), 3);
        assert_eq!(result.worktrees[0].head.as_deref(), Some("3"));
        assert_eq!(result.worktrees[2].head.as_deref(), Some("5"));
        assert!(result.truncated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn workspace_unbound_returns_error() {
        let workspace = TempDir::new().expect("workspace");
        let store = build_store(workspace.path());
        let registry = build_registry(Some(workspace.path().to_path_buf()), Some(store));
        let err = invoke(registry.as_ref(), &SessionId::new().to_string(), json!({}))
            .await
            .expect_err("must fail");
        assert!(matches!(err, McpInternalError::WorkspaceUnbound { .. }));
    }

    #[test]
    fn include_lock_reason_returns_sanitized_reason() {
        let workspace = TempDir::new().expect("workspace");
        let locked = workspace.path().join(".arborist").join(".worktrees").join("locked");
        std::fs::create_dir_all(&locked).expect("locked dir");
        let store = build_store(workspace.path());
        let inspector = FakeInspector {
            worktrees: vec![WorktreeSnapshot {
                path: locked,
                branch: Some("locked".to_owned()),
                head: Some("l1".to_owned()),
                is_main: false,
                is_locked: true,
                lock_reason: Some("syncing C:\\secret\\path\nnow".to_owned()),
                prunable: false,
                last_modified: Some(SystemTime::now()),
            }],
            statuses: HashMap::new(),
        };

        let result = run_core(
            workspace.path(),
            &store,
            &inspector,
            ListWorktreesArgs {
                include_lock_reason: true,
                ..args_with(ListWorktreesFilter::default())
            },
            SystemTime::now(),
        );
        assert_eq!(result.worktrees[0].lock_reason.as_deref(), Some("syncing <path> now"));
    }

    #[test]
    fn older_than_days_filters_recent_worktrees() {
        let workspace = TempDir::new().expect("workspace");
        let old = workspace.path().join(".arborist").join(".worktrees").join("old");
        let recent = workspace.path().join(".arborist").join(".worktrees").join("recent");
        std::fs::create_dir_all(&old).expect("old dir");
        std::fs::create_dir_all(&recent).expect("recent dir");
        let store = build_store(workspace.path());
        let now = SystemTime::now();
        let inspector = FakeInspector {
            worktrees: vec![
                WorktreeSnapshot {
                    path: old,
                    branch: Some("old".to_owned()),
                    head: Some("o1".to_owned()),
                    is_main: false,
                    is_locked: false,
                    lock_reason: None,
                    prunable: false,
                    last_modified: Some(now - Duration::from_secs(31 * 24 * 60 * 60)),
                },
                WorktreeSnapshot {
                    path: recent,
                    branch: Some("recent".to_owned()),
                    head: Some("r1".to_owned()),
                    is_main: false,
                    is_locked: false,
                    lock_reason: None,
                    prunable: false,
                    last_modified: Some(now - Duration::from_secs(2 * 24 * 60 * 60)),
                },
            ],
            statuses: HashMap::new(),
        };

        let result = run_core(
            workspace.path(),
            &store,
            &inspector,
            args_with(ListWorktreesFilter {
                older_than_days: Some(30),
                ..Default::default()
            }),
            now,
        );
        assert_eq!(result.worktrees.len(), 1);
        assert_eq!(result.worktrees[0].branch.as_deref(), Some("old"));
    }
}
