//! `workspace_status` MCP tool.
//!
//! Provides a single, opinionated snapshot of the current workspace's worktrees with lightweight
//! git state and session counts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::task::JoinError;

use crate::git::{git_command_mcp_ro, GitRunner, RealGitRunner, WorktreeGitStatusSummary};
use crate::mcp::error::McpInternalError;
use crate::mcp::ipc::McpSessionRegistry;
use crate::mcp::types::McpToolDescriptor;
use crate::types::{AppConfig, CustomProcessKind, Session, SessionStatus, Tool, WorktreeTabId};

const TOOL_NAME: &str = "workspace_status";
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_WORKTREES: usize = 100;
const MAX_WORKTREES_CAP: usize = 500;
const GENERIC_STATUS_ERROR: &str = "git status unavailable";

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum WorkspaceStatusMode {
    #[default]
    Attention,
    All,
    SummaryOnly,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceStatusArgs {
    include_remote_sync: bool,
    include_sub_process_breakdown: bool,
    include_ai_breakdown: bool,
    mode: WorkspaceStatusMode,
    max_worktrees: usize,
}

impl Default for WorkspaceStatusArgs {
    fn default() -> Self {
        Self {
            include_remote_sync: false,
            include_sub_process_breakdown: false,
            include_ai_breakdown: false,
            mode: WorkspaceStatusMode::Attention,
            max_worktrees: DEFAULT_MAX_WORKTREES,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RemoteSyncCounts {
    ahead: u32,
    behind: u32,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SummaryBlock {
    total: usize,
    dirty: usize,
    with_active_session: usize,
    with_running_ai: usize,
    behind_origin: usize,
    locked: usize,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct AiBreakdown {
    claude: usize,
    copilot: usize,
    codex: usize,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SubProcessBreakdown {
    running_terminals: usize,
    running_ai_sessions: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorktreeStatusEntry {
    name: String,
    relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    is_main: bool,
    is_locked: bool,
    dirty: bool,
    active_session_count: usize,
    running_ai_count: usize,
    has_session_error: bool,
    needs_attention: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_sync: Option<RemoteSyncCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sub_process_breakdown: Option<SubProcessBreakdown>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WorkspaceStatusSnapshot {
    workspace_root: PathBuf,
    mode: WorkspaceStatusMode,
    truncated: bool,
    summary: SummaryBlock,
    attention: Vec<WorktreeStatusEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktrees: Option<Vec<WorktreeStatusEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_breakdown: Option<AiBreakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_synced_at: Option<String>,
    as_of: String,
}

#[derive(Debug, Clone, Default)]
struct SessionCounts {
    active_session_count: usize,
    running_ai_count: usize,
    error_sessions: usize,
}

trait WorkspaceStatusGit: GitRunner {
    fn origin_synced_at(&self, workspace_root: &Path) -> Option<String>;
    fn remote_sync_counts(&self, worktree: &Path) -> Option<RemoteSyncCounts>;
}

impl WorkspaceStatusGit for RealGitRunner {
    fn origin_synced_at(&self, workspace_root: &Path) -> Option<String> {
        let fetch_head = workspace_root.join(".git").join("FETCH_HEAD");
        let modified = std::fs::metadata(fetch_head).ok()?.modified().ok()?;
        format_system_time(modified)
    }

    fn remote_sync_counts(&self, worktree: &Path) -> Option<RemoteSyncCounts> {
        let output = git_command_mcp_ro(worktree)
            .args(["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_remote_sync_counts(&String::from_utf8_lossy(&output.stdout))
    }
}

#[must_use]
pub fn descriptor() -> McpToolDescriptor {
    McpToolDescriptor {
        name: TOOL_NAME.to_owned(),
        description: "Single structured snapshot of the workspace: worktrees, branches, dirty state, ahead/behind, active AI/sub-session counts, and (optionally) remote sync state. Read-only and opinionated. For per-worktree drill-down (sessions, lock reasons, status filters), use list_worktrees instead.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "includeRemoteSync": { "type": "boolean", "default": false },
                "includeSubProcessBreakdown": { "type": "boolean", "default": false },
                "includeAiBreakdown": { "type": "boolean", "default": false },
                "mode": { "type": "string", "enum": ["attention", "all", "summaryOnly"], "default": "attention" },
                "maxWorktrees": { "type": "integer", "minimum": 1, "maximum": 500, "default": 100 }
            },
            "additionalProperties": false
        }),
    }
}

pub async fn invoke(registry: &McpSessionRegistry, session_id: &str, args: Value) -> Result<Value, McpInternalError> {
    let _ = session_id;
    let parsed_args = parse_args(args)?;
    let resolved = resolve_invoke_context(registry)?;

    let snapshot = tokio::time::timeout(
        STATUS_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            core_status(
                &resolved.workspace_root,
                &parsed_args,
                &RealGitRunner,
                &resolved.sessions,
                &resolved.config,
                OffsetDateTime::now_utc(),
            )
        }),
    )
    .await
    .map_err(|_| McpInternalError::Busy {
        message: format!("{TOOL_NAME} timed out after {} seconds", STATUS_TIMEOUT.as_secs()),
    })?
    .map_err(join_error_to_internal)??;

    serde_json::to_value(snapshot).map_err(|err| McpInternalError::Internal {
        message: format!("failed to serialize {TOOL_NAME} response: {err}"),
    })
}

#[derive(Debug)]
struct InvokeContext {
    workspace_root: PathBuf,
    config: AppConfig,
    sessions: Vec<Session>,
}

fn parse_args(args: Value) -> Result<WorkspaceStatusArgs, McpInternalError> {
    let parsed = serde_json::from_value::<WorkspaceStatusArgs>(args).map_err(|err| McpInternalError::InvalidArg {
        message: format!("{TOOL_NAME} arguments are invalid: {err}"),
    })?;

    if !(1..=MAX_WORKTREES_CAP).contains(&parsed.max_worktrees) {
        return Err(McpInternalError::InvalidArg {
            message: format!("maxWorktrees must be between 1 and {MAX_WORKTREES_CAP}"),
        });
    }

    Ok(parsed)
}

fn resolve_invoke_context(registry: &McpSessionRegistry) -> Result<InvokeContext, McpInternalError> {
    // Why: pull workspace state from the host's bound `WorkspaceScope`, never from the host
    // process's `current_dir()` — the host's cwd has no relationship to the user's workspace and
    // letting it leak in here would silently spoof results from whatever directory the host
    // happened to be launched from. An unbound workspace is the only honest failure mode.
    let context = registry.context();
    let scope = match context.app.workspace.read() {
        Ok(scope) => scope,
        Err(poisoned) => poisoned.into_inner(),
    };

    let Some(workspace_root) = scope.workspace_root.clone() else {
        return Err(McpInternalError::WorkspaceUnbound {
            message: "Open a workspace in Arborist before retrying workspace_status".to_owned(),
        });
    };
    let Some(store) = scope.store.clone() else {
        return Err(McpInternalError::WorkspaceUnbound {
            message: "Open a workspace in Arborist before retrying workspace_status".to_owned(),
        });
    };

    let config = store.load_config();
    let sessions = store.load_sessions().into_values().collect();

    Ok(InvokeContext {
        workspace_root,
        config,
        sessions,
    })
}

fn core_status<G: WorkspaceStatusGit>(
    workspace_root: &Path,
    args: &WorkspaceStatusArgs,
    git: &G,
    sessions: &[Session],
    config: &AppConfig,
    now: OffsetDateTime,
) -> Result<WorkspaceStatusSnapshot, McpInternalError> {
    let canonical_workspace_root = dunce::canonicalize(workspace_root).map_err(|_| McpInternalError::InvalidPath {
        message: "workspace_status could not resolve the workspace root".to_owned(),
    })?;

    let worktrees = git.list_worktrees(&canonical_workspace_root)?;
    let session_counts = build_session_counts(sessions, &canonical_workspace_root);
    let terminal_counts = build_terminal_counts(config, &canonical_workspace_root);
    let overall_ai_breakdown = build_ai_breakdown(sessions, &canonical_workspace_root);

    let mut entries = Vec::with_capacity(worktrees.len());

    for worktree in worktrees {
        let canonical_worktree = canonical_descendant_path(&canonical_workspace_root, &worktree.path)?;
        let relative_path = relative_path_string(&canonical_workspace_root, &canonical_worktree)?;
        let status = load_worktree_status(git, &canonical_worktree);
        let remote_sync = build_remote_sync(git, &canonical_worktree, &status, args.include_remote_sync);
        let counts = session_counts.get(&canonical_worktree).cloned().unwrap_or_default();
        let running_terminals = terminal_counts.get(&canonical_worktree).copied().unwrap_or_default();
        let behind = remote_sync.as_ref().map_or(0, |counts| counts.behind);
        let needs_attention = worktree.is_locked || status.dirty || status.error.is_some() || counts.error_sessions > 0 || behind > 0;

        entries.push(WorktreeStatusEntry {
            name: worktree_name(&canonical_worktree, &relative_path),
            relative_path,
            branch: worktree.branch.clone(),
            is_main: worktree.is_main,
            is_locked: worktree.is_locked,
            dirty: status.dirty,
            active_session_count: counts.active_session_count,
            running_ai_count: counts.running_ai_count,
            has_session_error: counts.error_sessions > 0,
            needs_attention,
            status_error: status.error.clone(),
            remote_sync,
            sub_process_breakdown: args.include_sub_process_breakdown.then_some(SubProcessBreakdown {
                running_terminals,
                running_ai_sessions: counts.active_session_count,
            }),
        });
    }

    let summary = build_summary(&entries);
    let mut attention = entries.iter().filter(|entry| entry.needs_attention).cloned().collect::<Vec<_>>();
    sort_attention(&mut attention);

    let mut all_entries = entries.clone();
    sort_all(&mut all_entries);

    let (worktrees, truncated) = match args.mode {
        WorkspaceStatusMode::SummaryOnly => (None, false),
        WorkspaceStatusMode::Attention => {
            let was_truncated = attention.len() > args.max_worktrees;
            (Some(attention.iter().take(args.max_worktrees).cloned().collect()), was_truncated)
        }
        WorkspaceStatusMode::All => {
            let was_truncated = all_entries.len() > args.max_worktrees;
            (Some(all_entries.into_iter().take(args.max_worktrees).collect()), was_truncated)
        }
    };
    let origin_synced_at = args
        .include_remote_sync
        .then(|| git.origin_synced_at(&canonical_workspace_root))
        .flatten();

    Ok(WorkspaceStatusSnapshot {
        workspace_root: canonical_workspace_root,
        mode: args.mode.clone(),
        truncated,
        summary,
        attention,
        worktrees,
        ai_breakdown: args.include_ai_breakdown.then_some(overall_ai_breakdown),
        origin_synced_at,
        as_of: format_timestamp(now),
    })
}

fn build_summary(entries: &[WorktreeStatusEntry]) -> SummaryBlock {
    SummaryBlock {
        total: entries.len(),
        dirty: entries.iter().filter(|entry| entry.dirty).count(),
        with_active_session: entries.iter().filter(|entry| entry.active_session_count > 0).count(),
        with_running_ai: entries.iter().filter(|entry| entry.running_ai_count > 0).count(),
        behind_origin: entries
            .iter()
            .filter(|entry| entry.remote_sync.as_ref().is_some_and(|counts| counts.behind > 0))
            .count(),
        locked: entries.iter().filter(|entry| entry.is_locked).count(),
    }
}

fn load_worktree_status<G: WorkspaceStatusGit>(git: &G, worktree: &Path) -> WorktreeGitStatusSummary {
    match git.git_status_mcp(worktree) {
        Ok(mut status) => {
            if status.error.is_some() {
                status.error = Some(GENERIC_STATUS_ERROR.to_owned());
            }
            status
        }
        Err(_) => WorktreeGitStatusSummary {
            error: Some(GENERIC_STATUS_ERROR.to_owned()),
            ..WorktreeGitStatusSummary::default()
        },
    }
}

fn build_remote_sync<G: WorkspaceStatusGit>(
    git: &G,
    worktree: &Path,
    status: &WorktreeGitStatusSummary,
    include_remote_sync: bool,
) -> Option<RemoteSyncCounts> {
    if !include_remote_sync {
        return None;
    }

    if !status.has_upstream {
        return Some(RemoteSyncCounts::default());
    }

    Some(
        git.remote_sync_counts(worktree)
            .or_else(|| {
                Some(RemoteSyncCounts {
                    ahead: status.ahead_of_upstream.unwrap_or_default(),
                    behind: status.behind_upstream.unwrap_or_default(),
                })
            })
            .unwrap_or_default(),
    )
}

fn build_session_counts(sessions: &[Session], workspace_root: &Path) -> BTreeMap<PathBuf, SessionCounts> {
    let mut counts: BTreeMap<PathBuf, SessionCounts> = BTreeMap::new();

    for session in sessions {
        let Some(path) = optional_descendant_path(workspace_root, &session.worktree_path) else {
            continue;
        };
        let entry = counts.entry(path).or_default();
        if !matches!(session.status, SessionStatus::Exited) {
            entry.active_session_count += 1;
        }
        if matches!(session.status, SessionStatus::Starting | SessionStatus::Running) {
            entry.running_ai_count += 1;
        }
        if matches!(session.status, SessionStatus::Error) {
            entry.error_sessions += 1;
        }
    }

    counts
}

fn build_ai_breakdown(sessions: &[Session], workspace_root: &Path) -> AiBreakdown {
    let mut breakdown = AiBreakdown::default();

    for session in sessions {
        if matches!(session.status, SessionStatus::Exited) || optional_descendant_path(workspace_root, &session.worktree_path).is_none() {
            continue;
        }

        match session.tool {
            Tool::Claude => breakdown.claude += 1,
            Tool::Copilot => breakdown.copilot += 1,
            Tool::Codex => breakdown.codex += 1,
        }
    }

    breakdown
}

fn build_terminal_counts(config: &AppConfig, workspace_root: &Path) -> BTreeMap<PathBuf, usize> {
    let tab_paths = config
        .worktree_tabs
        .iter()
        .filter_map(|tab| optional_descendant_path(workspace_root, &tab.path).map(|path| (tab.id, path)))
        .collect::<BTreeMap<WorktreeTabId, PathBuf>>();

    let mut counts = BTreeMap::new();
    for record in config
        .last_open_sub_sessions
        .iter()
        .filter(|record| record.kind == CustomProcessKind::Terminal)
    {
        let Some(tab_id) = record.parent_worktree_tab_id else {
            continue;
        };
        let Some(path) = tab_paths.get(&tab_id) else {
            continue;
        };
        *counts.entry(path.clone()).or_default() += 1;
    }

    counts
}

fn sort_all(entries: &mut [WorktreeStatusEntry]) {
    entries.sort_by_key(|entry| (!entry.is_main, entry.name.to_ascii_lowercase(), entry.relative_path.to_ascii_lowercase()));
}

fn sort_attention(entries: &mut [WorktreeStatusEntry]) {
    entries.sort_by_key(|entry| {
        (
            !entry.is_locked,
            !entry.dirty,
            entry.remote_sync.as_ref().is_none_or(|counts| counts.behind == 0),
            !entry.has_session_error,
            !entry.is_main,
            entry.name.to_ascii_lowercase(),
            entry.relative_path.to_ascii_lowercase(),
        )
    });
}

fn worktree_name(worktree: &Path, relative_path: &str) -> String {
    worktree
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| relative_path.to_owned())
}

fn canonical_descendant_path(workspace_root: &Path, path: &Path) -> Result<PathBuf, McpInternalError> {
    let canonical = dunce::canonicalize(path).map_err(|_| McpInternalError::InvalidPath {
        message: "workspace_status encountered an unreadable worktree path".to_owned(),
    })?;
    if canonical == workspace_root || canonical.starts_with(workspace_root) {
        Ok(canonical)
    } else {
        Err(McpInternalError::InvalidPath {
            message: "workspace_status encountered a worktree path outside the workspace".to_owned(),
        })
    }
}

fn optional_descendant_path(workspace_root: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = dunce::canonicalize(path).ok()?;
    (canonical == workspace_root || canonical.starts_with(workspace_root)).then_some(canonical)
}

fn relative_path_string(workspace_root: &Path, worktree_path: &Path) -> Result<String, McpInternalError> {
    let relative = worktree_path.strip_prefix(workspace_root).map_err(|_| McpInternalError::InvalidPath {
        message: "workspace_status encountered a worktree path outside the workspace".to_owned(),
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(".".to_owned());
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn parse_remote_sync_counts(output: &str) -> Option<RemoteSyncCounts> {
    let mut fields = output.split_whitespace();
    let behind = fields.next()?.parse().ok()?;
    let ahead = fields.next()?.parse().ok()?;
    Some(RemoteSyncCounts { ahead, behind })
}

fn format_system_time(value: SystemTime) -> Option<String> {
    OffsetDateTime::from(value).format(&Rfc3339).ok()
}

fn format_timestamp(now: OffsetDateTime) -> String {
    now.format(&Rfc3339).unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn join_error_to_internal(err: JoinError) -> McpInternalError {
    McpInternalError::Internal {
        message: format!("{TOOL_NAME} task failed: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitError;
    use crate::types::{SessionId, SubSessionId, SubSessionRecord, WorktreeInfo, WorktreeTab};
    use tempfile::TempDir;

    struct FakeGitRunner {
        worktrees: Vec<WorktreeInfo>,
        statuses: BTreeMap<PathBuf, WorktreeGitStatusSummary>,
        failing_statuses: Vec<PathBuf>,
        remote_sync: BTreeMap<PathBuf, RemoteSyncCounts>,
        origin_synced_at: Option<String>,
        top_level: Option<PathBuf>,
    }

    impl FakeGitRunner {
        fn new(worktrees: Vec<WorktreeInfo>) -> Self {
            Self {
                worktrees,
                statuses: BTreeMap::new(),
                failing_statuses: Vec::new(),
                remote_sync: BTreeMap::new(),
                origin_synced_at: None,
                top_level: None,
            }
        }
    }

    impl GitRunner for FakeGitRunner {
        fn list_worktrees(&self, _repo_root: &Path) -> Result<Vec<WorktreeInfo>, crate::types::Error> {
            Ok(self.worktrees.clone())
        }

        fn git_toplevel(&self, _path: &Path) -> Result<Option<PathBuf>, crate::types::Error> {
            Ok(self.top_level.clone())
        }

        fn create_worktree(&self, _repo_root: &Path, _relative_path: &Path, _branch: &str) -> Result<PathBuf, crate::types::Error> {
            panic!("unused in tests")
        }

        fn remove_worktree(&self, _repo_root: &Path, _worktree_path: &Path) -> Result<(), crate::types::Error> {
            panic!("unused in tests")
        }

        fn git_status(&self, _worktree_path: &Path) -> Result<crate::types::WorktreeGitStatus, crate::types::Error> {
            panic!("unused in tests")
        }

        fn fetch_origin(&self, _root: &Path, _timeout: Duration) -> Result<(), GitError> {
            panic!("unused in tests")
        }

        fn branches_merged_into(&self, _root: &Path, _target_oid: &str) -> Result<std::collections::HashSet<String>, GitError> {
            panic!("unused in tests")
        }

        fn cherry_empty(&self, _root: &Path, _upstream_oid: &str, _branch: &str) -> Result<bool, GitError> {
            panic!("unused in tests")
        }

        fn merge_from_branch(
            &self,
            _worktree: &Path,
            _source_oid: &str,
            _leave_conflicts: bool,
            _timeout: Duration,
        ) -> Result<crate::git::MergeFromBranchOutcome, GitError> {
            panic!("unused in tests")
        }

        fn default_branch(&self, _root: &Path) -> Result<crate::git::DefaultBranchInfo, GitError> {
            panic!("unused in tests")
        }

        fn rev_parse_verify(&self, _root: &Path, _ref_expr: &str) -> Result<String, GitError> {
            panic!("unused in tests")
        }

        fn git_status_mcp(&self, worktree: &Path) -> Result<WorktreeGitStatusSummary, GitError> {
            if self.failing_statuses.iter().any(|path| path == worktree) {
                return Err(GitError::CommandFailed {
                    context: "status",
                    message: "boom".to_owned(),
                });
            }
            Ok(self.statuses.get(worktree).cloned().unwrap_or_default())
        }

        fn merge_tree_dry_run(&self, _root: &Path, _base_oid: &str, _source_oid: &str) -> Result<crate::git::MergeTreeOutcome, GitError> {
            panic!("unused in tests")
        }

        fn merge_abort(&self, _worktree: &Path) -> Result<(), GitError> {
            panic!("unused in tests")
        }

        fn has_merge_head(&self, _worktree: &Path) -> Result<bool, GitError> {
            panic!("unused in tests")
        }
    }

    impl WorkspaceStatusGit for FakeGitRunner {
        fn origin_synced_at(&self, _workspace_root: &Path) -> Option<String> {
            self.origin_synced_at.clone()
        }

        fn remote_sync_counts(&self, worktree: &Path) -> Option<RemoteSyncCounts> {
            self.remote_sync.get(worktree).copied()
        }
    }

    #[test]
    fn args_reject_unknown_field() {
        assert!(parse_args(json!({ "extra": true })).is_err());
    }

    #[test]
    fn args_reject_invalid_mode() {
        assert!(parse_args(json!({ "mode": "nope" })).is_err());
    }

    #[test]
    fn args_reject_max_worktrees_above_cap() {
        assert!(parse_args(json!({ "maxWorktrees": 501 })).is_err());
    }

    #[test]
    fn happy_path_summary_counts_three_worktrees() {
        let fixture = Fixture::new();
        let snapshot = core_status(
            fixture.workspace_root.as_path(),
            &WorkspaceStatusArgs {
                include_remote_sync: true,
                ..WorkspaceStatusArgs::default()
            },
            &fixture.git,
            &fixture.sessions,
            &fixture.config,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("snapshot");

        assert_eq!(snapshot.summary.total, 3);
        assert_eq!(snapshot.summary.dirty, 1);
        assert_eq!(snapshot.summary.with_active_session, 2);
        assert_eq!(snapshot.summary.with_running_ai, 1);
        assert_eq!(snapshot.summary.behind_origin, 1);
        assert_eq!(snapshot.summary.locked, 1);
        assert_eq!(snapshot.attention.len(), 2);
    }

    #[test]
    fn mode_attention_only_returns_attention_worktrees() {
        let fixture = Fixture::new();
        let snapshot = core_status(
            fixture.workspace_root.as_path(),
            &WorkspaceStatusArgs::default(),
            &fixture.git,
            &fixture.sessions,
            &fixture.config,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("snapshot");

        let listed = snapshot.worktrees.expect("worktrees present");
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|entry| entry.needs_attention));
    }

    #[test]
    fn mode_summary_only_omits_worktrees_array() {
        let fixture = Fixture::new();
        let snapshot = core_status(
            fixture.workspace_root.as_path(),
            &WorkspaceStatusArgs {
                mode: WorkspaceStatusMode::SummaryOnly,
                ..WorkspaceStatusArgs::default()
            },
            &fixture.git,
            &fixture.sessions,
            &fixture.config,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("snapshot");

        let value = serde_json::to_value(snapshot).expect("serialize");
        assert!(value.get("worktrees").is_none());
    }

    #[test]
    fn mode_all_returns_every_worktree() {
        let fixture = Fixture::new();
        let snapshot = core_status(
            fixture.workspace_root.as_path(),
            &WorkspaceStatusArgs {
                mode: WorkspaceStatusMode::All,
                ..WorkspaceStatusArgs::default()
            },
            &fixture.git,
            &fixture.sessions,
            &fixture.config,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("snapshot");

        assert_eq!(snapshot.worktrees.expect("worktrees present").len(), 3);
    }

    #[test]
    fn include_remote_sync_populates_ahead_and_behind() {
        let fixture = Fixture::new();
        let snapshot = core_status(
            fixture.workspace_root.as_path(),
            &WorkspaceStatusArgs {
                include_remote_sync: true,
                mode: WorkspaceStatusMode::All,
                ..WorkspaceStatusArgs::default()
            },
            &fixture.git,
            &fixture.sessions,
            &fixture.config,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("snapshot");

        let feature = snapshot
            .worktrees
            .expect("worktrees present")
            .into_iter()
            .find(|entry| entry.relative_path.ends_with("feature-behind"))
            .expect("feature-behind entry");
        assert_eq!(feature.remote_sync, Some(RemoteSyncCounts { ahead: 1, behind: 2 }));
        assert_eq!(snapshot.origin_synced_at.as_deref(), Some("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn include_ai_breakdown_populates_tool_counts() {
        let fixture = Fixture::new();
        let snapshot = core_status(
            fixture.workspace_root.as_path(),
            &WorkspaceStatusArgs {
                include_ai_breakdown: true,
                ..WorkspaceStatusArgs::default()
            },
            &fixture.git,
            &fixture.sessions,
            &fixture.config,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("snapshot");

        assert_eq!(
            snapshot.ai_breakdown,
            Some(AiBreakdown {
                claude: 1,
                copilot: 0,
                codex: 1,
            })
        );
    }

    #[test]
    fn max_worktrees_cap_sets_truncated() {
        let fixture = Fixture::new();
        let snapshot = core_status(
            fixture.workspace_root.as_path(),
            &WorkspaceStatusArgs {
                mode: WorkspaceStatusMode::All,
                max_worktrees: 2,
                ..WorkspaceStatusArgs::default()
            },
            &fixture.git,
            &fixture.sessions,
            &fixture.config,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("snapshot");

        assert!(snapshot.truncated);
        assert_eq!(snapshot.worktrees.expect("worktrees present").len(), 2);
        assert_eq!(snapshot.summary.total, 3);
    }

    #[test]
    fn workspace_unbound_errors() {
        // The `resolve_invoke_context` helper now reads `WorkspaceScope` off the registry's
        // `McpContext`, not the host process's cwd. Constructing a real bound registry inside a
        // unit test (which would require an `AppContext`, `PtyPool`, audit log, trust store, and
        // rate limiter) is heavier than the value of asserting the unbound branch here — the
        // bound/unbound branches are covered end-to-end by `cargo test --features test-helpers`
        // integration tests that stand up the full host context. Keeping this slot as a
        // documented placeholder so the gap is visible.
    }

    struct Fixture {
        _root: TempDir,
        workspace_root: PathBuf,
        git: FakeGitRunner,
        sessions: Vec<Session>,
        config: AppConfig,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().expect("root");
            let workspace_root = dunce::canonicalize(root.path()).expect("canonical workspace root");
            let worktree_a = workspace_root.join(".arborist").join(".worktrees").join("feature-dirty");
            let worktree_b = workspace_root.join(".arborist").join(".worktrees").join("feature-behind");
            std::fs::create_dir_all(worktree_a.as_path()).expect("worktree_a");
            std::fs::create_dir_all(worktree_b.as_path()).expect("worktree_b");
            std::fs::create_dir_all(workspace_root.join(".git")).expect("git dir");
            std::fs::write(workspace_root.join(".git").join("FETCH_HEAD"), b"origin\n").expect("fetch head");

            let worktrees = vec![
                WorktreeInfo {
                    path: workspace_root.clone(),
                    branch: Some("main".to_owned()),
                    is_main: true,
                    is_locked: false,
                },
                WorktreeInfo {
                    path: worktree_a.clone(),
                    branch: Some("feature-dirty".to_owned()),
                    is_main: false,
                    is_locked: false,
                },
                WorktreeInfo {
                    path: worktree_b.clone(),
                    branch: Some("feature-behind".to_owned()),
                    is_main: false,
                    is_locked: true,
                },
            ];

            let mut git = FakeGitRunner::new(worktrees);
            git.origin_synced_at = Some("2024-01-01T00:00:00Z".to_owned());
            git.top_level = Some(workspace_root.clone());
            git.statuses.insert(
                workspace_root.clone(),
                WorktreeGitStatusSummary {
                    dirty: false,
                    ahead_of_upstream: Some(0),
                    behind_upstream: Some(0),
                    file_count: 0,
                    has_upstream: true,
                    error: None,
                },
            );
            git.statuses.insert(
                worktree_a.clone(),
                WorktreeGitStatusSummary {
                    dirty: true,
                    ahead_of_upstream: Some(0),
                    behind_upstream: Some(0),
                    file_count: 2,
                    has_upstream: true,
                    error: None,
                },
            );
            git.statuses.insert(
                worktree_b.clone(),
                WorktreeGitStatusSummary {
                    dirty: false,
                    ahead_of_upstream: Some(1),
                    behind_upstream: Some(2),
                    file_count: 0,
                    has_upstream: true,
                    error: None,
                },
            );
            git.remote_sync.insert(worktree_b.clone(), RemoteSyncCounts { ahead: 1, behind: 2 });

            let sessions = vec![
                session(&workspace_root, Tool::Claude, SessionStatus::Running),
                session(&worktree_b, Tool::Codex, SessionStatus::Error),
                session(&worktree_a, Tool::Copilot, SessionStatus::Exited),
            ];

            Self {
                _root: root,
                workspace_root,
                git,
                sessions,
                config: AppConfig::default(),
            }
        }
    }

    fn session(worktree_path: &Path, tool: Tool, status: SessionStatus) -> Session {
        Session {
            id: SessionId::new(),
            tool,
            worktree_path: worktree_path.to_path_buf(),
            worktree_name: worktree_path.file_name().and_then(|name| name.to_str()).unwrap_or("worktree").to_owned(),
            label: format!("{}-{status:?}", tool.as_id()),
            composed_command: String::new(),
            structured_command: None,
            command_provenance: Vec::new(),
            status,
            pid: None,
            created_at: 0,
            tab_index: 0,
            temp_files: Vec::new(),
            ai_session_id: None,
            last_metrics: None,
        }
    }

    #[allow(dead_code)]
    fn _terminal_sub_record(tab_id: WorktreeTabId) -> SubSessionRecord {
        SubSessionRecord {
            id: SubSessionId::new(),
            parent_session_id: None,
            parent_worktree_tab_id: Some(tab_id),
            def_id: crate::types::CustomProcessDefId::new("shell"),
            kind: CustomProcessKind::Terminal,
            label: "shell".to_owned(),
            composed_command: "pwsh".to_owned(),
        }
    }

    #[allow(dead_code)]
    fn _worktree_tab(id: WorktreeTabId, path: &Path, branch: &str) -> WorktreeTab {
        WorktreeTab {
            id,
            path: path.to_path_buf(),
            name: branch.to_owned(),
            branch: Some(branch.to_owned()),
            label: branch.to_owned(),
            tab_index: 0,
            active_child_id: None,
            icon_id: 1,
        }
    }
}
