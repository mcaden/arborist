//! Git integration — currently just `git worktree list --porcelain` parsing.
//!
//! The trait seam ([`GitRunner`]) lets tests inject canned outputs without depending on a real `git` binary. The production implementation
//! ([`RealGitRunner`]) shells out to `git` and degrades gracefully: any failure (binary missing, not a repo, parse error, IO) yields `Ok(vec![])`
//! with a `warn!` carrying a stable structured `code` so the frontend never blocks on discovery — the manual "Browse…" affordance is
//! always present).
//!
//! Porcelain format reference: <https://git-scm.com/docs/git-worktree#_porcelain_format>
//!
//! Each worktree block looks like:
//! ```text
//! worktree /abs/path HEAD <sha> branch refs/heads/<name> # OR `detached` locked [<reason>]? # optional prunable [<reason>]? # optional
//! ```
//! Blocks are separated by blank lines; the very first one is the main worktree.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use thiserror::Error as ThisError;
use tracing::{debug, warn};

use crate::types::{Error, GitStatusFile, GitStatusFileKind, WorktreeGitStatus, WorktreeInfo, MAX_GIT_STATUS_FILES};

// Re-export the canonical wire types. Their definitions live in `arborist-types` so that the
// MCP sidecar, the frontend MIRROR, and the host backend all see the same structure (a single
// canonical Rust wire type per shape — see `.github/copilot-instructions.md`).
pub use arborist_types::git::{DefaultBranchInfo, DefaultBranchSource, MergeFromBranchOutcome, MergeTreeOutcome, WorktreeGitStatusSummary};

#[cfg(windows)]
const WORKTREE_REMOVE_RETRY_DELAYS_MS: &[u64] = &[25, 50, 100, 200, 400, 800, 1_000];
#[cfg(not(windows))]
const WORKTREE_REMOVE_RETRY_DELAYS_MS: &[u64] = &[];

#[derive(Debug, ThisError)]
pub enum GitError {
    #[error("git invocation failed ({context}): {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("git command failed ({context}): {message}")]
    CommandFailed { context: &'static str, message: String },
    #[error("git command timed out ({context}) after {timeout:?}")]
    TimedOut { context: &'static str, timeout: Duration },
    #[error("default branch unknown")]
    DefaultBranchUnknown,
    #[error("ref not found: {ref_expr}")]
    RefNotFound { ref_expr: String },
}

static MERGE_TREE_CAPABILITY: std::sync::LazyLock<Mutex<Option<bool>>> = std::sync::LazyLock::new(|| Mutex::new(None));

/// Build a `git` [`Command`] with the repo-selection environment variables stripped.
///
/// When arborist (or its test suite) runs as a child of another `git` invocation — most importantly the husky `pre-push` hook — git exports
/// `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, etc. into the child environment. These take precedence over `-C <path>` / `current_dir(...)`, so a
/// naively-spawned `git` ends up operating on the *outer* repo regardless of how the caller pinned the working directory. In the worst case that
/// means writing commits onto the developer's checked-out branch or unregistering real worktrees from the bare repo (issue #13).
///
/// Strip the repo-selection variables here so every `git` we spawn really does target the repo the caller asked for.
pub(crate) fn git_command() -> Command {
    let mut cmd = Command::new("git");
    // On Windows, suppress the transient console window that appears when a GUI application spawns a console subprocess. Without this flag every
    // `git` invocation during boot (validate_repo_root, list_worktrees, etc.) briefly flashes a black window.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

pub(crate) fn git_command_mcp(root: &Path) -> Command {
    let mut cmd = git_command();
    cmd.current_dir(root).arg("-C").arg(root);
    for var in [
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_ASKPASS",
        "GIT_EDITOR",
        "GIT_SEQUENCE_EDITOR",
        "SSH_ASKPASS",
    ] {
        cmd.env_remove(var);
    }
    for (key, _) in std::env::vars_os() {
        if let Some(name) = key.to_str() {
            if name.starts_with("GIT_CONFIG_") {
                cmd.env_remove(&key);
            }
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_EDITOR", "true");
    cmd.env("GIT_MERGE_AUTOEDIT", "no");
    cmd
}

pub(crate) fn git_command_mcp_ro(root: &Path) -> Command {
    let mut cmd = git_command_mcp(root);
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

pub(crate) fn git_command_mcp_mut(root: &Path) -> Command {
    let mut cmd = git_command_mcp(root);
    let hooks_path = if cfg!(target_os = "windows") { "NUL" } else { "/dev/null" };
    cmd.arg("-c")
        .arg(format!("core.hooksPath={hooks_path}"))
        .arg("-c")
        .arg("merge.tool=false");
    cmd
}

/// Minimal seam over `git worktree list --porcelain`. Implementors must be `Send + Sync` so we can stash one in `Arc<dyn GitRunner>` on the
/// `AppContext` and share it across worker threads.
pub trait GitRunner: Send + Sync {
    /// Enumerate the worktrees rooted at `repo_root`. Implementations MUST return `Ok(vec![])` rather than an error if discovery is impossible
    /// (missing binary, not a repo, IO error) — graceful degradation is a load-bearing requirement of the create flow.
    fn list_worktrees(&self, repo_root: &Path) -> Result<Vec<WorktreeInfo>, Error>;

    /// Probe whether `path` is a git repository — runs `git -C <path> rev-parse --is-inside-work-tree`. Returns `Ok(true)` only on a clean
    /// exit-code-0 with stdout `true`. Used by the `workspace_validate` command (Roadmap §1.1). Run `git -C <path> rev-parse --show-toplevel`.
    /// Returns `Ok(Some(canonical_toplevel))` if `path` lies inside a git working tree, or `Ok(None)` otherwise (missing dir, non-repo, or git
    /// unavailable). Never errors on the "not a repo" case so the picker can show inline feedback without a toast.
    fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, Error>;

    /// Run `git -C <repo_root> worktree add <relative_path> -b <branch>`. `relative_path` is interpreted relative to `repo_root` (typically
    /// `.worktrees/<branch>`). Returns the canonical absolute path of the new worktree on success; otherwise an [`Error::Internal`] carrying the
    /// captured stderr.
    fn create_worktree(&self, repo_root: &Path, relative_path: &Path, branch: &str) -> Result<PathBuf, Error>;

    /// Run `git -C <repo_root> worktree remove --force <worktree_path>`. `--force` is used because the user has explicitly confirmed deletion in the
    /// UI (CloseConfirmDialog) and we have just torn down the PTY that owned the cwd. The production runner retries transient Windows filesystem
    /// errors because process exit and handle release can lag the close call by a short interval.
    ///
    /// `repo_root` must be a stable checkout of the same repository *outside* the target `worktree_path` — typically the configured `workspace_root`.
    /// Callers must not pass `worktree_path` itself as `repo_root`: the spawned `git` would inherit it as its CWD, and on Windows the OS prevents
    /// deletion of a process's own CWD.
    ///
    /// Errors are surfaced as [`Error::Internal`] carrying git's stderr so the frontend can show the user a meaningful message.
    fn remove_worktree(&self, repo_root: &Path, worktree_path: &Path) -> Result<(), Error>;

    /// Snapshot `git status --porcelain=v2 --branch -z` for `worktree_path` (Issue #55: worktree dashboard). Returns a populated
    /// [`WorktreeGitStatus`] on success. On discovery failure (missing dir, not a repo, missing `git` binary, non-zero status exit) returns a
    /// default-valued struct with [`WorktreeGitStatus::error`] populated. Output parsing itself is best-effort and lossy: unrecognised porcelain
    /// records are silently skipped (see [`parse_status_v2`]), so callers never see a "parse error" — the worst case is missing entries, not a
    /// signalled failure. The dashboard distinguishes "clean tree" from "unreadable" by inspecting `error`.
    fn git_status(&self, worktree_path: &Path) -> Result<WorktreeGitStatus, Error>;

    fn fetch_origin(&self, root: &Path, timeout: Duration) -> Result<(), GitError>;
    fn branches_merged_into(&self, root: &Path, target_oid: &str) -> Result<HashSet<String>, GitError>;
    fn cherry_empty(&self, root: &Path, upstream_oid: &str, branch: &str) -> Result<bool, GitError>;
    fn merge_from_branch(
        &self,
        worktree: &Path,
        source_oid: &str,
        leave_conflicts: bool,
        timeout: Duration,
    ) -> Result<MergeFromBranchOutcome, GitError>;
    fn default_branch(&self, root: &Path) -> Result<DefaultBranchInfo, GitError>;
    fn rev_parse_verify(&self, root: &Path, ref_expr: &str) -> Result<String, GitError>;
    fn git_status_mcp(&self, worktree: &Path) -> Result<WorktreeGitStatusSummary, GitError>;
    fn merge_tree_dry_run(&self, root: &Path, base_oid: &str, source_oid: &str) -> Result<MergeTreeOutcome, GitError>;
    fn merge_abort(&self, worktree: &Path) -> Result<(), GitError>;
    fn has_merge_head(&self, worktree: &Path) -> Result<bool, GitError>;
}

/// Production [`GitRunner`] that shells out to the system `git`.
#[derive(Default, Debug, Clone, Copy)]
pub struct RealGitRunner;

impl GitRunner for RealGitRunner {
    fn list_worktrees(&self, repo_root: &Path) -> Result<Vec<WorktreeInfo>, Error> {
        if !repo_root.is_dir() {
            debug!(
                code = "GitUnavailable",
                repo_root = %repo_root.display(),
                "worktrees_list: repo_root is not a directory"
            );
            return Ok(Vec::new());
        }

        let output = match git_command()
            .current_dir(repo_root)
            .arg("-C")
            .arg(repo_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                warn!(
                    code = "GitUnavailable",
                    repo_root = %repo_root.display(),
                    error = %e,
                    "git binary not invokable; returning empty worktree list",
                );
                return Ok(Vec::new());
            }
        };

        if !output.status.success() {
            // Most common case: not a git repository. We don't bother distinguishing reasons — the contract is "empty list on any failure".
            warn!(
                code = "GitUnavailable",
                repo_root = %repo_root.display(),
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "git worktree list failed; returning empty list",
            );
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut worktrees = parse_porcelain(&stdout);
        // Canonicalize paths so symlink hops collapse to the same form
        // used by session tab paths (which go through validate_worktree →
        // dunce::canonicalize). Without this, UI comparisons between tab
        // paths and worktree-list paths can mismatch on POSIX with symlinks.
        for wt in &mut worktrees {
            if let Ok(canon) = dunce::canonicalize(&wt.path) {
                wt.path = canon;
            }
        }
        Ok(worktrees)
    }

    fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, Error> {
        if !path.is_dir() {
            return Ok(None);
        }
        let output = match git_command()
            .current_dir(path)
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                warn!(
                    code = "GitUnavailable",
                    path = %path.display(),
                    error = %e,
                    "git binary not invokable; treating as non-repo",
                );
                return Ok(None);
            }
        };
        if !output.status.success() {
            return Ok(None);
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if raw.is_empty() {
            return Ok(None);
        }
        // `--show-toplevel` returns an already-absolute path; canonicalize it so symlink hops collapse to the same form the caller will pass in.
        let canon = dunce::canonicalize(&raw).unwrap_or_else(|_| PathBuf::from(raw));
        Ok(Some(canon))
    }

    fn create_worktree(&self, repo_root: &Path, relative_path: &Path, branch: &str) -> Result<PathBuf, Error> {
        if !repo_root.is_dir() {
            return Err(Error::WorktreeMissing(repo_root.to_path_buf()));
        }
        let output = git_command()
            .current_dir(repo_root)
            .arg("-C")
            .arg(repo_root)
            .args(["worktree", "add"])
            .arg(relative_path)
            .arg("-b")
            .arg(branch)
            .output()
            .map_err(|e| Error::Internal(format!("git worktree add: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(Error::Internal(format!(
                "git worktree add failed: {}",
                if stderr.is_empty() { "<no stderr>".to_owned() } else { stderr }
            )));
        }
        // The new worktree lives at <repo_root>/<relative_path>.
        let new_path = repo_root.join(relative_path);
        dunce::canonicalize(&new_path)
            .map_err(|e| Error::Internal(format!("worktree created but canonicalization failed: {}: {e}", new_path.display())))
    }

    fn remove_worktree(&self, repo_root: &Path, worktree_path: &Path) -> Result<(), Error> {
        if !repo_root.is_dir() {
            return Err(Error::WorktreeMissing(repo_root.to_path_buf()));
        }
        remove_worktree_with_retry(
            repo_root,
            worktree_path,
            || run_git_worktree_remove(repo_root, worktree_path),
            || remove_residual_worktree_dir(worktree_path),
            std::thread::sleep,
        )
    }

    fn git_status(&self, worktree_path: &Path) -> Result<WorktreeGitStatus, Error> {
        if !worktree_path.is_dir() {
            debug!(
                code = "GitUnavailable",
                worktree = %worktree_path.display(),
                "git_status: worktree path is not a directory"
            );
            return Ok(WorktreeGitStatus {
                error: Some(format!("worktree path does not exist or is not a directory: {}", worktree_path.display())),
                ..Default::default()
            });
        }
        let output = match git_command()
            .current_dir(worktree_path)
            .arg("-C")
            .arg(worktree_path)
            // `--porcelain=v2` for unambiguous machine-readable output, `--branch` to surface the branch / upstream / ahead-behind header,
            // `-z` so paths are NUL-separated (no escaping or quoting), `--untracked-files=all` so each untracked file is enumerated
            // individually instead of git's default `?? dir/` directory shorthand — the dashboard count must match the file count, not the
            // top-level-directory count.
            .args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                warn!(
                    code = "GitUnavailable",
                    worktree = %worktree_path.display(),
                    error = %e,
                    "git_status: git binary not invokable; returning empty status",
                );
                return Ok(WorktreeGitStatus {
                    error: Some(format!("git binary unavailable: {e}")),
                    ..Default::default()
                });
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            warn!(
                code = "GitUnavailable",
                worktree = %worktree_path.display(),
                stderr = %stderr,
                "git_status: git status failed; returning empty status",
            );
            return Ok(WorktreeGitStatus {
                error: Some(if stderr.is_empty() {
                    "git status exited non-zero".to_string()
                } else {
                    stderr
                }),
                ..Default::default()
            });
        }
        let mut status = parse_status_v2(&output.stdout);
        enrich_with_source_branch(&mut status, worktree_path);
        Ok(status)
    }

    fn fetch_origin(&self, root: &Path, timeout: Duration) -> Result<(), GitError> {
        let mut cmd = git_command_mcp(root);
        cmd.args(["fetch", "--quiet", "origin"]);
        match run_git_with_timeout(cmd, "git fetch --quiet origin", timeout)? {
            TimedGitOutput::Completed(output) if output.status.success() => Ok(()),
            TimedGitOutput::Completed(output) => Err(git_command_failed("git fetch --quiet origin", &output)),
            TimedGitOutput::TimedOut => Err(GitError::TimedOut {
                context: "git fetch --quiet origin",
                timeout,
            }),
        }
    }

    fn branches_merged_into(&self, root: &Path, target_oid: &str) -> Result<HashSet<String>, GitError> {
        let mut cmd = git_command_mcp_ro(root);
        cmd.args(["branch", "--merged", target_oid]);
        let output = run_git_success(cmd, "git branch --merged")?;
        Ok(parse_merged_branches(&output.stdout))
    }

    fn cherry_empty(&self, root: &Path, upstream_oid: &str, branch: &str) -> Result<bool, GitError> {
        let mut cmd = git_command_mcp_ro(root);
        cmd.args(["cherry", "-v", upstream_oid, branch]);
        let output = run_git_success(cmd, "git cherry -v")?;
        Ok(!String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim_start().starts_with('+')))
    }

    fn merge_from_branch(
        &self,
        worktree: &Path,
        source_oid: &str,
        leave_conflicts: bool,
        timeout: Duration,
    ) -> Result<MergeFromBranchOutcome, GitError> {
        if is_ancestor(worktree, source_oid, "HEAD")? {
            return Ok(MergeFromBranchOutcome::AlreadyUpToDate);
        }

        let mut cmd = git_command_mcp_mut(worktree);
        cmd.args(["merge", "--no-edit", "--no-stat", "--no-progress", "--no-rerere-autoupdate", source_oid]);
        match run_git_with_timeout(cmd, "git merge", timeout)? {
            TimedGitOutput::Completed(output) if output.status.success() => {
                let (ahead, behind) = rev_list_left_right_count_refs(worktree, source_oid, "HEAD")?;
                Ok(MergeFromBranchOutcome::MergedCleanly { ahead, behind })
            }
            TimedGitOutput::Completed(output) => {
                let files = conflicted_files(worktree)?;
                if files.is_empty() {
                    return Err(git_command_failed("git merge", &output));
                }
                if leave_conflicts {
                    Ok(MergeFromBranchOutcome::LeftConflicted { files })
                } else {
                    self.merge_abort(worktree)?;
                    Ok(MergeFromBranchOutcome::AutoAborted { files })
                }
            }
            TimedGitOutput::TimedOut => {
                let recovered = if leave_conflicts {
                    !self.has_merge_head(worktree)?
                } else {
                    match self.merge_abort(worktree) {
                        Ok(()) => true,
                        Err(_) => !self.has_merge_head(worktree)?,
                    }
                };
                Ok(MergeFromBranchOutcome::TimedOut { recovered })
            }
        }
    }

    fn default_branch(&self, root: &Path) -> Result<DefaultBranchInfo, GitError> {
        let mut origin_head = git_command();
        origin_head
            .current_dir(root)
            .arg("-C")
            .arg(root)
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"]);
        let output = run_git_output(origin_head, "git symbolic-ref refs/remotes/origin/HEAD")?;
        if output.status.success() {
            let ref_name = trim_output(&output.stdout);
            if let Some(branch) = ref_name.strip_prefix("refs/remotes/origin/") {
                if !branch.is_empty() {
                    return Ok(DefaultBranchInfo {
                        branch: branch.to_owned(),
                        source: DefaultBranchSource::OriginHead,
                    });
                }
            }
        }

        for (ref_name, source) in [
            ("refs/heads/main", DefaultBranchSource::Main),
            ("refs/heads/master", DefaultBranchSource::Master),
        ] {
            let mut probe_cmd = git_command();
            probe_cmd
                .current_dir(root)
                .arg("-C")
                .arg(root)
                .args(["show-ref", "--verify", "--quiet", ref_name]);
            let probe = run_git_output(probe_cmd, "git show-ref --verify")?;
            if probe.status.success() {
                return Ok(DefaultBranchInfo {
                    branch: ref_name.trim_start_matches("refs/heads/").to_owned(),
                    source,
                });
            }
        }

        Err(GitError::DefaultBranchUnknown)
    }

    fn rev_parse_verify(&self, root: &Path, ref_expr: &str) -> Result<String, GitError> {
        let rev = format!("{ref_expr}^{{commit}}");
        let mut cmd = git_command();
        cmd.current_dir(root)
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--verify", "--end-of-options"])
            .arg(&rev);
        let output = run_git_output(cmd, "git rev-parse --verify")?;
        if !output.status.success() {
            return Err(GitError::RefNotFound {
                ref_expr: ref_expr.to_owned(),
            });
        }
        let oid = trim_output(&output.stdout);
        if oid.is_empty() {
            return Err(GitError::RefNotFound {
                ref_expr: ref_expr.to_owned(),
            });
        }
        Ok(oid)
    }

    fn git_status_mcp(&self, worktree: &Path) -> Result<WorktreeGitStatusSummary, GitError> {
        let mut cmd = git_command_mcp_ro(worktree);
        cmd.args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"]);
        let output = run_git_success(cmd, "git status --porcelain=v2")?;
        Ok(parse_status_summary(&output.stdout))
    }

    fn merge_tree_dry_run(&self, root: &Path, base_oid: &str, source_oid: &str) -> Result<MergeTreeOutcome, GitError> {
        if !merge_tree_supported(root)? {
            return Ok(MergeTreeOutcome::Unsupported);
        }
        if is_ancestor(root, source_oid, base_oid)? {
            return Ok(MergeTreeOutcome::AlreadyUpToDate);
        }

        let mut cmd = git_command_mcp_ro(root);
        cmd.args(["merge-tree", "--write-tree", "--no-messages", base_oid, source_oid]);
        let output = run_git_output(cmd, "git merge-tree --write-tree --no-messages")?;
        if output.status.success() {
            return Ok(MergeTreeOutcome::Clean);
        }
        if is_merge_tree_write_tree_unsupported(&output) {
            return merge_tree_legacy_dry_run(root, source_oid);
        }

        let files = parse_merge_tree_conflict_files(&output);
        if !files.is_empty() {
            return Ok(MergeTreeOutcome::Conflict { files });
        }

        match merge_tree_legacy_dry_run(root, source_oid)? {
            MergeTreeOutcome::Unsupported => Err(git_command_failed("git merge-tree --write-tree --no-messages", &output)),
            outcome => Ok(outcome),
        }
    }

    fn merge_abort(&self, worktree: &Path) -> Result<(), GitError> {
        let mut cmd = git_command_mcp_mut(worktree);
        cmd.args(["merge", "--abort"]);
        let output = run_git_output(cmd, "git merge --abort")?;
        if output.status.success() {
            return Ok(());
        }
        let message = output_message(&output);
        if message.contains("There is no merge to abort") {
            return Ok(());
        }
        Err(git_command_failed("git merge --abort", &output))
    }

    fn has_merge_head(&self, worktree: &Path) -> Result<bool, GitError> {
        let mut cmd = git_command_mcp_ro(worktree);
        cmd.args(["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]);
        let output = run_git_output(cmd, "git rev-parse --verify --quiet MERGE_HEAD")?;
        if output.status.success() {
            return Ok(true);
        }
        if output_message(&output).is_empty() {
            return Ok(false);
        }
        Err(git_command_failed("git rev-parse --verify --quiet MERGE_HEAD", &output))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitCommandResult {
    success: bool,
    stderr: String,
}

enum TimedGitOutput {
    Completed(Output),
    TimedOut,
}

fn trim_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn output_message(output: &Output) -> String {
    let stdout = trim_output(&output.stdout);
    let stderr = trim_output(&output.stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stderr}\n{stdout}"),
    }
}

fn git_command_failed(context: &'static str, output: &Output) -> GitError {
    let message = output_message(output);
    GitError::CommandFailed {
        context,
        message: if message.is_empty() { "<no output>".to_owned() } else { message },
    }
}

fn run_git_output(mut cmd: Command, context: &'static str) -> Result<Output, GitError> {
    cmd.output().map_err(|source| GitError::Io { context, source })
}

fn run_git_success(cmd: Command, context: &'static str) -> Result<Output, GitError> {
    let output = run_git_output(cmd, context)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(git_command_failed(context, &output))
    }
}

fn run_git_with_timeout(mut cmd: Command, context: &'static str, timeout: Duration) -> Result<TimedGitOutput, GitError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|source| GitError::Io { context, source })?;
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|source| GitError::Io { context, source })? {
            Some(_) => {
                let output = child.wait_with_output().map_err(|source| GitError::Io { context, source })?;
                return Ok(TimedGitOutput::Completed(output));
            }
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                child.wait_with_output().map_err(|source| GitError::Io { context, source })?;
                return Ok(TimedGitOutput::TimedOut);
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn parse_merged_branches(output: &[u8]) -> HashSet<String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let name = line.trim_start_matches('*').trim();
            if name.is_empty() || name.starts_with('(') {
                None
            } else {
                Some(name.to_owned())
            }
        })
        .collect()
}

fn conflicted_files(worktree: &Path) -> Result<Vec<String>, GitError> {
    let mut cmd = git_command_mcp_ro(worktree);
    cmd.args(["diff", "--name-only", "--diff-filter=U"]);
    let output = run_git_success(cmd, "git diff --name-only --diff-filter=U")?;
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

fn rev_list_left_right_count_refs(worktree: &Path, left: &str, right: &str) -> Result<(u32, u32), GitError> {
    let range = format!("{left}...{right}");
    let mut cmd = git_command_mcp_ro(worktree);
    cmd.args(["rev-list", "--left-right", "--count", &range]);
    let output = run_git_success(cmd, "git rev-list --left-right --count")?;
    parse_rev_list_count(&output.stdout).ok_or_else(|| git_command_failed("git rev-list --left-right --count", &output))
}

fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, GitError> {
    let mut cmd = git_command_mcp_ro(root);
    cmd.args(["merge-base", "--is-ancestor", ancestor, descendant]);
    let output = run_git_output(cmd, "git merge-base --is-ancestor")?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) && output_message(&output).is_empty() {
        return Ok(false);
    }
    Err(git_command_failed("git merge-base --is-ancestor", &output))
}

fn merge_base(root: &Path, left: &str, right: &str) -> Result<String, GitError> {
    let mut cmd = git_command_mcp_ro(root);
    cmd.args(["merge-base", left, right]);
    let output = run_git_success(cmd, "git merge-base")?;
    let oid = trim_output(&output.stdout);
    if oid.is_empty() {
        return Err(git_command_failed("git merge-base", &output));
    }
    Ok(oid)
}

fn merge_tree_supported(root: &Path) -> Result<bool, GitError> {
    let mut cache = MERGE_TREE_CAPABILITY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(supported) = *cache {
        return Ok(supported);
    }
    let mut cmd = git_command_mcp_ro(root);
    cmd.args(["merge-tree", "--version"]);
    let output = run_git_output(cmd, "git merge-tree --version")?;
    let text = output_message(&output).to_ascii_lowercase();
    let supported = output.status.success()
        || text.contains("usage: git merge-tree")
        || text.contains("unknown option `version'")
        || text.contains("unknown option 'version'");
    *cache = Some(supported);
    Ok(supported)
}

fn is_merge_tree_write_tree_unsupported(output: &Output) -> bool {
    if output.status.code() != Some(129) {
        return false;
    }
    let text = output_message(output).to_ascii_lowercase();
    text.contains("usage: git merge-tree") || text.contains("unknown option")
}

fn parse_merge_tree_conflict_files(output: &Output) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for source in [&output.stdout[..], &output.stderr[..]] {
        for line in String::from_utf8_lossy(source).lines() {
            if let Some(path) = parse_merge_tree_conflict_line(line) {
                if seen.insert(path.clone()) {
                    files.push(path);
                }
            }
        }
    }
    files
}

fn parse_merge_tree_conflict_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("base ") || trimmed.starts_with("our ") || trimmed.starts_with("their ") {
        return trimmed.split_whitespace().last().map(ToOwned::to_owned);
    }
    if trimmed.chars().next()?.is_ascii_digit() {
        return trimmed.split_whitespace().last().and_then(|token| {
            if token.chars().all(|ch| ch.is_ascii_digit()) {
                None
            } else {
                Some(token.to_owned())
            }
        });
    }
    None
}

fn merge_tree_legacy_dry_run(root: &Path, source_oid: &str) -> Result<MergeTreeOutcome, GitError> {
    let base = merge_base(root, "HEAD", source_oid)?;
    let mut cmd = git_command_mcp_ro(root);
    cmd.args(["merge-tree", &base, "HEAD", source_oid]);
    let output = run_git_output(cmd, "git merge-tree")?;
    if !output.status.success() {
        return Err(git_command_failed("git merge-tree", &output));
    }
    let files = parse_merge_tree_conflict_files(&output);
    if files.is_empty() {
        Ok(MergeTreeOutcome::Clean)
    } else {
        Ok(MergeTreeOutcome::Conflict { files })
    }
}

fn parse_status_summary(input: &[u8]) -> WorktreeGitStatusSummary {
    let mut summary = WorktreeGitStatusSummary::default();
    let mut iter = input.split(|b| *b == 0).peekable();
    while let Some(rec) = iter.next() {
        if rec.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(rec);
        let line = line.as_ref();
        if let Some(rest) = line.strip_prefix("# ") {
            parse_status_summary_branch_header(&mut summary, rest);
            continue;
        }
        match line.chars().next() {
            Some('1') | Some('u') | Some('?') => {
                summary.dirty = true;
                summary.file_count += 1;
            }
            Some('2') => {
                summary.dirty = true;
                summary.file_count += 1;
                let _ = iter.next();
            }
            _ => {}
        }
    }
    summary
}

fn parse_status_summary_branch_header(summary: &mut WorktreeGitStatusSummary, rest: &str) {
    let Some((key, value)) = rest.split_once(' ') else {
        return;
    };
    match key {
        "branch.upstream" if !value.trim().is_empty() => {
            summary.has_upstream = true;
        }
        "branch.ab" => {
            let mut parts = value.split_whitespace();
            if let Some(ahead) = parts.next().and_then(|part| part.strip_prefix('+')).and_then(|part| part.parse().ok()) {
                summary.ahead_of_upstream = Some(ahead);
            }
            if let Some(behind) = parts.next().and_then(|part| part.strip_prefix('-')).and_then(|part| part.parse().ok()) {
                summary.behind_upstream = Some(behind);
            }
        }
        _ => {}
    }
}

fn run_git_worktree_remove(repo_root: &Path, worktree_path: &Path) -> Result<GitCommandResult, Error> {
    let output = git_command()
        .current_dir(repo_root)
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output()
        .map_err(|e| Error::Internal(format!("git worktree remove: {e}")))?;

    Ok(GitCommandResult {
        success: output.status.success(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn remove_worktree_with_retry(
    repo_root: &Path,
    worktree_path: &Path,
    mut run: impl FnMut() -> Result<GitCommandResult, Error>,
    mut remove_residual: impl FnMut() -> io::Result<()>,
    mut sleep: impl FnMut(Duration),
) -> Result<(), Error> {
    let mut last_stderr = String::new();
    let mut saw_retryable_delete_failure = false;
    for attempt in 0.. {
        let output = run()?;
        if output.success {
            return Ok(());
        }

        last_stderr = if output.stderr.is_empty() {
            "<no stderr>".to_owned()
        } else {
            output.stderr
        };
        if saw_retryable_delete_failure && is_already_unregistered_worktree_failure(&last_stderr) {
            warn!(
                repo_root = %repo_root.display(),
                worktree_path = %worktree_path.display(),
                stderr = %last_stderr,
                "git worktree remove reports the worktree is already unregistered; deleting residual directory directly",
            );
            return remove_residual_worktree_dir_with_retry(repo_root, worktree_path, &mut remove_residual, &mut sleep);
        }
        let Some(delay_ms) = WORKTREE_REMOVE_RETRY_DELAYS_MS.get(attempt) else {
            break;
        };
        if !is_retryable_worktree_remove_failure(&last_stderr) {
            break;
        }
        saw_retryable_delete_failure = true;
        warn!(
            repo_root = %repo_root.display(),
            worktree_path = %worktree_path.display(),
            attempt = attempt + 1,
            delay_ms,
            stderr = %last_stderr,
            "git worktree remove hit transient filesystem error; retrying",
        );
        sleep(Duration::from_millis(*delay_ms));
    }

    Err(Error::Internal(format!("git worktree remove failed: {last_stderr}")))
}

fn remove_residual_worktree_dir(worktree_path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(worktree_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_residual_worktree_dir_with_retry(
    repo_root: &Path,
    worktree_path: &Path,
    remove_residual: &mut impl FnMut() -> io::Result<()>,
    sleep: &mut impl FnMut(Duration),
) -> Result<(), Error> {
    let mut last_error = String::new();
    for attempt in 0.. {
        match remove_residual() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                last_error = e.to_string();
            }
        }

        let Some(delay_ms) = WORKTREE_REMOVE_RETRY_DELAYS_MS.get(attempt) else {
            break;
        };
        warn!(
            repo_root = %repo_root.display(),
            worktree_path = %worktree_path.display(),
            attempt = attempt + 1,
            delay_ms,
            error = %last_error,
            "residual worktree directory cleanup hit filesystem error; retrying",
        );
        sleep(Duration::from_millis(*delay_ms));
    }

    Err(Error::Internal(format!(
        "git worktree remove unregistered the worktree but residual directory cleanup failed for {}: {last_error}",
        worktree_path.display()
    )))
}

fn is_retryable_worktree_remove_failure(stderr: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let lower = stderr.to_ascii_lowercase();
    lower.contains("directory not empty")
        || lower.contains("being used by another process")
        || lower.contains("access is denied")
        || lower.contains("permission denied")
}

fn is_already_unregistered_worktree_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not a working tree") || lower.contains("not a worktree")
}

/// Parse `git worktree list --porcelain` output. The first block is the main worktree; subsequent blocks are linked worktrees. Detached HEADs produce
/// a `detached` line in place of `branch …`. Locked worktrees carry a `locked` line (with an optional reason).
///
/// Pure function — no IO. Robust to empty input, trailing whitespace, and unknown porcelain keys (silently skipped).
pub(crate) fn parse_porcelain(input: &str) -> Vec<WorktreeInfo> {
    let mut out: Vec<WorktreeInfo> = Vec::new();
    let mut is_first_block = true;
    let mut cur: Option<PartialWorktree> = None;

    for raw_line in input.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            if let Some(p) = cur.take() {
                if let Some(info) = p.finish(is_first_block) {
                    out.push(info);
                }
                is_first_block = false;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("worktree ") {
            // Begin a new block. Flush any in-progress block first (defensive — porcelain blocks should be blank-line-separated, but we don't want a
            // missing blank line to swallow an entry).
            if let Some(p) = cur.take() {
                if let Some(info) = p.finish(is_first_block) {
                    out.push(info);
                }
                is_first_block = false;
            }
            cur = Some(PartialWorktree::new(PathBuf::from(rest)));
        } else if let Some(p) = cur.as_mut() {
            if let Some(branch_ref) = line.strip_prefix("branch ") {
                // Strip the conventional `refs/heads/` prefix to surface a friendly branch name. Anything else is passed through.
                let name = branch_ref.strip_prefix("refs/heads/").unwrap_or(branch_ref).to_owned();
                p.branch = Some(name);
            } else if line == "detached" {
                p.branch = None;
            } else if line == "locked" || line.starts_with("locked ") {
                p.is_locked = true;
            }
            // Unknown keys (HEAD, prunable, …) are intentionally ignored.
        }
    }

    if let Some(p) = cur.take() {
        if let Some(info) = p.finish(is_first_block) {
            out.push(info);
        }
    }

    out
}

struct PartialWorktree {
    path: PathBuf,
    branch: Option<String>,
    is_locked: bool,
}

impl PartialWorktree {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            branch: None,
            is_locked: false,
        }
    }

    fn finish(self, is_main: bool) -> Option<WorktreeInfo> {
        if self.path.as_os_str().is_empty() {
            return None;
        }
        Some(WorktreeInfo {
            path: self.path,
            branch: self.branch,
            is_main,
            is_locked: self.is_locked,
        })
    }
}

/// Parse `git status --porcelain=v2 --branch -z` output into a [`WorktreeGitStatus`] (Issue #55).
///
/// Format reference: <https://git-scm.com/docs/git-status#_porcelain_format_version_2>
///
/// Records are separated by NUL bytes (`-z`), so we split on `\0` and walk each record. The leading character determines record kind:
///
/// * `# branch.<key> <value>` — header lines emitted with `--branch`. We extract `head` (short SHA), `branch` (current branch name; `(detached)`
///   indicates detached HEAD), `upstream` (tracking branch, omitted when none), and `ahead/behind` (`+N -M`).
/// * `1 XY <subN..> <…>` — ordinary changed file. Counts contribute via the X/Y columns: X = staged, Y = unstaged.
/// * `2 XY <subN..> <…> <new_path>\0<orig_path>` — renamed/copied file. Same XY rules; with `-z`, the inter-record separator is also NUL, so the
///   original path lives in the *next* NUL-terminated record. We surface the new path (the one in the `2 …` record itself) and consume the
///   following record to keep the parser aligned.
/// * `u XY <subN..>` — unmerged (conflicted) entry.
/// * `? <path>` — untracked file.
/// * `! <path>` — ignored (we do not surface these).
///
/// Pure function — no IO. Robust to empty input and unrecognised record prefixes (silently skipped).
pub(crate) fn parse_status_v2(input: &[u8]) -> WorktreeGitStatus {
    let mut status = WorktreeGitStatus::default();

    // Split on NUL. `-z` produces a *trailing* NUL too, so the final element is typically an empty record we skip.
    let mut iter = input.split(|b| *b == 0).peekable();
    while let Some(rec) = iter.next() {
        if rec.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(rec);
        let line = line.as_ref();
        if let Some(rest) = line.strip_prefix("# ") {
            parse_status_branch_header(&mut status, rest);
            continue;
        }
        let mut chars = line.chars();
        let kind = chars.next();
        match kind {
            Some('1') => parse_status_changed_record(&mut status, line, false),
            Some('2') => {
                parse_status_changed_record(&mut status, line, true);
                // Renamed/copied records are followed by the original path as a *separate* NUL-terminated record. Consume and discard it so the
                // outer loop doesn't try to interpret a bare path as a status code.
                let _ = iter.next();
            }
            Some('u') => parse_status_unmerged_record(&mut status, line),
            Some('?') => parse_status_untracked_record(&mut status, line),
            Some('!') => { /* ignored (`--ignored` not requested but defensively skip) */ }
            _ => { /* unknown — ignore for forward compatibility */ }
        }
    }

    status
}

/// Parse one `# branch.<key> <value>` header, mutating `status` in place. Unknown keys are ignored.
fn parse_status_branch_header(status: &mut WorktreeGitStatus, rest: &str) {
    // `rest` looks like `branch.oid <sha>`, `branch.head <name>`, `branch.upstream <name>`, `branch.ab +N -M`.
    let Some((key, value)) = rest.split_once(' ') else {
        return;
    };
    match key {
        "branch.oid" => {
            let v = value.trim();
            if !v.is_empty() && v != "(initial)" {
                // 12-char short sha is plenty for a UI badge while staying unambiguous in any reasonable repo.
                let short = v.chars().take(12).collect::<String>();
                status.head = Some(short);
            }
        }
        "branch.head" => {
            let v = value.trim();
            if !v.is_empty() && v != "(detached)" {
                status.branch = Some(v.to_owned());
            }
        }
        "branch.upstream" => {
            let v = value.trim();
            if !v.is_empty() {
                status.upstream = Some(v.to_owned());
            }
        }
        "branch.ab" => {
            // Format: `+<ahead> -<behind>`.
            let mut parts = value.split_whitespace();
            if let Some(a) = parts.next() {
                if let Some(stripped) = a.strip_prefix('+') {
                    status.ahead = stripped.parse().unwrap_or(0);
                }
            }
            if let Some(b) = parts.next() {
                if let Some(stripped) = b.strip_prefix('-') {
                    status.behind = stripped.parse().unwrap_or(0);
                }
            }
        }
        _ => {}
    }
}

/// Parse a `1 XY ...` (ordinary) or `2 XY ... orig_path` (renamed/copied) record. The path lives at the end of the line for `1`, and immediately
/// after the rename-score/sub-summary for `2`. We don't parse the sub-fields beyond what we need — the categorical counts only depend on the XY
/// columns.
fn parse_status_changed_record(status: &mut WorktreeGitStatus, line: &str, is_renamed: bool) {
    // Layout (space-separated):
    //   ordinary:  `1 XY sub mH mI mW hH hI <path>`
    //   renamed:   `2 XY sub mH mI mW hH hI X<score> <new_path>` (then NUL-separated orig_path in the next record; we surface new_path only)
    let mut parts = line.splitn(if is_renamed { 10 } else { 9 }, ' ');
    let _ = parts.next(); // record kind
    let xy = parts.next().unwrap_or("..");
    let path = parts.last().unwrap_or("").to_owned();
    let (x, y) = parse_xy(xy);
    let mut staged_hit = false;
    let mut unstaged_hit = false;
    if x != '.' && x != ' ' {
        status.staged += 1;
        staged_hit = true;
    }
    if y != '.' && y != ' ' {
        status.unstaged += 1;
        unstaged_hit = true;
    }
    let kind = if staged_hit {
        GitStatusFileKind::Staged
    } else {
        GitStatusFileKind::Unstaged
    };
    push_status_file(status, path.clone(), kind, xy);
    // If both columns are dirty, also surface a second list entry so the file appears under both staged and unstaged when the user expands the
    // panel. The counts already reflect this above. We add at most one extra entry per file.
    if staged_hit && unstaged_hit {
        push_status_file(status, path, GitStatusFileKind::Unstaged, xy);
    }
}

fn parse_status_unmerged_record(status: &mut WorktreeGitStatus, line: &str) {
    // Layout: `u XY sub m1 m2 m3 mW h1 h2 h3 <path>`
    let mut parts = line.splitn(11, ' ');
    let _ = parts.next();
    let xy = parts.next().unwrap_or("..");
    let path = parts.last().unwrap_or("").to_owned();
    status.conflicted += 1;
    push_status_file(status, path, GitStatusFileKind::Conflicted, xy);
}

fn parse_status_untracked_record(status: &mut WorktreeGitStatus, line: &str) {
    // Layout: `? <path>`. We don't have an XY code; surface `??` to match porcelain-v1 conventions UIs already render.
    let path = line.get(2..).unwrap_or("").to_owned();
    if path.is_empty() {
        return;
    }
    status.untracked += 1;
    push_status_file(status, path, GitStatusFileKind::Untracked, "??");
}

fn parse_xy(xy: &str) -> (char, char) {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    (x, y)
}

fn push_status_file(status: &mut WorktreeGitStatus, path: String, kind: GitStatusFileKind, xy: &str) {
    if path.is_empty() {
        return;
    }
    if status.files.len() >= MAX_GIT_STATUS_FILES {
        status.files_truncated = true;
        return;
    }
    status.files.push(GitStatusFile {
        path,
        kind,
        status: xy.to_owned(),
    });
}

// --------------------------------------------------------------------------- Source branch detection
// ---------------------------------------------------------------------------

/// Cache for detected source branches per worktree path. The source branch rarely changes (only on remote HEAD updates
/// or branch renames), so caching avoids 1–2 subprocess calls per dashboard poll tick. The cache maps **canonicalized**
/// worktree paths to their detected source branch (or `None` if undetectable).
///
/// **Lifecycle:** entries are unbounded and never invalidated within a process lifetime. This is acceptable because:
/// - The source branch of a repo almost never changes during a session
/// - The worst case (stale entry after `git remote set-head`) shows slightly outdated but still valid info
/// - A process restart clears the cache naturally
/// - Path canonicalization (via `std::fs::canonicalize` at lookup time) prevents duplicate entries from path form
///   variations (trailing slash, symlinks, case differences on case-insensitive filesystems)
static SOURCE_BRANCH_CACHE: std::sync::LazyLock<Mutex<HashMap<PathBuf, Option<String>>>> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Detect the repo's default/source branch by probing remotes. Returns the short branch name (e.g. `"main"`), or `None` when undetectable.
///
/// **Limitation:** only probes the `origin` remote. Repos cloned with a non-standard remote name (e.g. `upstream`) will
/// not have source branch info detected. This is acceptable for v1 — the result gracefully degrades to `None`.
///
/// Strategy:
/// 1. `git symbolic-ref refs/remotes/origin/HEAD` → strip `refs/remotes/origin/` prefix
/// 2. Fall back: check if `refs/remotes/origin/main` exists, then `refs/remotes/origin/master`
///
/// This is best-effort — if none of the above work, we simply omit source branch info from the status.
fn detect_source_branch(worktree_path: &Path) -> Option<String> {
    // Try symbolic-ref first (set by `git clone`)
    if let Ok(output) = git_command()
        .current_dir(worktree_path)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output()
    {
        if output.status.success() {
            let refname = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // Output is like "origin/main" — strip the remote prefix
            if let Some(branch) = refname.strip_prefix("origin/") {
                if !branch.is_empty() {
                    return Some(branch.to_owned());
                }
            }
        }
    }

    // Fallback: check if origin/main or origin/master exists
    for candidate in &["main", "master"] {
        let ref_path = format!("refs/remotes/origin/{candidate}");
        if let Ok(output) = git_command()
            .current_dir(worktree_path)
            .args(["rev-parse", "--verify", "--quiet", &ref_path])
            .output()
        {
            if output.status.success() {
                return Some((*candidate).to_owned());
            }
        }
    }

    None
}

/// Get the ahead/behind counts between HEAD and a reference branch using `git rev-list --left-right --count`.
/// Returns `(ahead, behind)` or `None` on failure.
fn rev_list_left_right_count(worktree_path: &Path, reference: &str) -> Option<(u32, u32)> {
    let range = format!("origin/{reference}...HEAD");
    let output = git_command()
        .current_dir(worktree_path)
        .args(["rev-list", "--left-right", "--count", &range])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_rev_list_count(&output.stdout)
}

/// Parse the output of `git rev-list --left-right --count` which is `<left>\t<right>\n`.
///
/// In the context of `origin/<source>...HEAD`, left = commits on the reference side (behind) and right = commits on HEAD
/// side (ahead). Returns `(ahead, behind)` — i.e. `(right, left)` — so the caller receives the semantically named pair.
pub(crate) fn parse_rev_list_count(output: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(output).ok()?.trim();
    let mut parts = text.split_whitespace();
    let left: u32 = parts.next()?.parse().ok()?;
    let right: u32 = parts.next()?.parse().ok()?;
    Some((right, left))
}

/// Enrich a `WorktreeGitStatus` with source branch divergence info. Best-effort: failures are silently ignored.
///
/// Uses [`SOURCE_BRANCH_CACHE`] to avoid re-running source branch detection on every poll tick. Only the `rev-list`
/// count (a single fast subprocess) runs on each call; the detection probes are amortized to once per worktree path.
fn enrich_with_source_branch(status: &mut WorktreeGitStatus, worktree_path: &Path) {
    let current_branch = match &status.branch {
        Some(b) => b.clone(),
        None => return, // detached HEAD — skip source branch detection
    };

    // Canonicalize the path for consistent cache keys regardless of path form (trailing slash, symlinks, case)
    let canonical = worktree_path.canonicalize().unwrap_or_else(|_| worktree_path.to_path_buf());

    let source = {
        let mut cache = SOURCE_BRANCH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.entry(canonical).or_insert_with(|| detect_source_branch(worktree_path)).clone()
    };

    let source = match source {
        Some(s) => s,
        None => return,
    };

    // Skip if we're ON the source branch (showing 0/0 relative to self is noise)
    if current_branch == source {
        return;
    }

    if let Some((ahead, behind)) = rev_list_left_right_count(worktree_path, &source) {
        status.source_branch = Some(source);
        status.source_ahead = Some(ahead);
        status.source_behind = Some(behind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_main_worktree_with_branch() {
        let input = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\n";
        let got = parse_porcelain(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, PathBuf::from("/repo"));
        assert_eq!(got[0].branch.as_deref(), Some("main"));
        assert!(got[0].is_main);
        assert!(!got[0].is_locked);
    }

    #[test]
    fn parses_multiple_worktrees_main_flag_only_on_first() {
        let input = "\
worktree /repo
HEAD aaa
branch refs/heads/main

worktree /repo-feature
HEAD bbb
branch refs/heads/feature

worktree /repo-detached
HEAD ccc
detached
";
        let got = parse_porcelain(input);
        assert_eq!(got.len(), 3);
        assert!(got[0].is_main);
        assert!(!got[1].is_main);
        assert!(!got[2].is_main);
        assert_eq!(got[0].branch.as_deref(), Some("main"));
        assert_eq!(got[1].branch.as_deref(), Some("feature"));
        assert_eq!(got[2].branch, None);
    }

    #[test]
    fn parses_locked_worktree_with_and_without_reason() {
        let input = "\
worktree /repo
HEAD aaa
branch refs/heads/main

worktree /repo-locked-bare
HEAD bbb
branch refs/heads/x
locked

worktree /repo-locked-reason
HEAD ccc
branch refs/heads/y
locked migrating to slow disk
";
        let got = parse_porcelain(input);
        assert_eq!(got.len(), 3);
        assert!(!got[0].is_locked);
        assert!(got[1].is_locked);
        assert!(got[2].is_locked);
    }

    #[test]
    fn empty_input_yields_empty_vec() {
        assert!(parse_porcelain("").is_empty());
        assert!(parse_porcelain("\n\n\n").is_empty());
    }

    #[test]
    fn ignores_unknown_keys() {
        let input = "worktree /repo\nHEAD abc\nbranch refs/heads/main\nprunable gitdir invalid\n\n";
        let got = parse_porcelain(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn real_runner_returns_empty_for_missing_directory() {
        let runner = RealGitRunner;
        let out = runner
            .list_worktrees(Path::new("/this/path/does/not/exist/arborist-test"))
            .expect("graceful degradation must not error");
        assert!(out.is_empty());
    }

    #[test]
    fn real_runner_returns_empty_for_non_repo_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let runner = RealGitRunner;
        let out = runner.list_worktrees(dir.path()).expect("non-repo must degrade gracefully");
        // Empty even though `git` is on PATH: the command exits non-zero because it isn't a repository.
        assert!(out.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn remove_worktree_retries_transient_directory_not_empty_failure() {
        let repo_root = Path::new(r"C:\repo");
        let worktree_path = Path::new(r"C:\repo\.arborist\.worktrees\feature");
        let calls = std::cell::Cell::new(0usize);
        let sleeps = std::cell::RefCell::new(Vec::new());

        remove_worktree_with_retry(
            repo_root,
            worktree_path,
            || {
                let n = calls.get();
                calls.set(n + 1);
                Ok(if n == 0 {
                    GitCommandResult {
                        success: false,
                        stderr: "error: failed to delete 'feature': Directory not empty".to_owned(),
                    }
                } else {
                    GitCommandResult {
                        success: true,
                        stderr: String::new(),
                    }
                })
            },
            || panic!("residual cleanup must not run after a successful git retry"),
            |delay| sleeps.borrow_mut().push(delay),
        )
        .expect("second attempt should succeed");

        assert_eq!(calls.get(), 2);
        assert_eq!(&*sleeps.borrow(), &[Duration::from_millis(25)]);
    }

    #[test]
    fn remove_worktree_does_not_retry_non_transient_failure() {
        let repo_root = Path::new(if cfg!(windows) { r"C:\repo" } else { "/repo" });
        let worktree_path = Path::new(if cfg!(windows) {
            r"C:\repo\.arborist\.worktrees\feature"
        } else {
            "/repo/.arborist/.worktrees/feature"
        });
        let calls = std::cell::Cell::new(0usize);

        let err = remove_worktree_with_retry(
            repo_root,
            worktree_path,
            || {
                calls.set(calls.get() + 1);
                Ok(GitCommandResult {
                    success: false,
                    stderr: "fatal: not a git repository".to_owned(),
                })
            },
            || panic!("residual cleanup must not run for non-transient failures"),
            |_| panic!("non-transient failure must not sleep/retry"),
        )
        .expect_err("non-transient git failure should surface");

        assert_eq!(calls.get(), 1);
        assert!(matches!(err, Error::Internal(msg) if msg.contains("fatal: not a git repository")));
    }

    #[cfg(windows)]
    #[test]
    fn remove_worktree_cleans_residual_dir_when_git_unregistered_after_transient_delete_failure() {
        let repo_root = Path::new(r"C:\repo");
        let worktree_path = Path::new(r"C:\repo\.arborist\.worktrees\feature");
        let calls = std::cell::Cell::new(0usize);
        let residual_calls = std::cell::Cell::new(0usize);
        let sleeps = std::cell::RefCell::new(Vec::new());

        remove_worktree_with_retry(
            repo_root,
            worktree_path,
            || {
                let n = calls.get();
                calls.set(n + 1);
                Ok(if n == 0 {
                    GitCommandResult {
                        success: false,
                        stderr: "error: failed to delete 'feature': Directory not empty".to_owned(),
                    }
                } else {
                    GitCommandResult {
                        success: false,
                        stderr: "fatal: 'C:\\repo\\.arborist\\.worktrees\\feature' is not a working tree".to_owned(),
                    }
                })
            },
            || {
                residual_calls.set(residual_calls.get() + 1);
                Ok(())
            },
            |delay| sleeps.borrow_mut().push(delay),
        )
        .expect("residual directory cleanup should complete the partially unregistered removal");

        assert_eq!(calls.get(), 2);
        assert_eq!(residual_calls.get(), 1);
        assert_eq!(&*sleeps.borrow(), &[Duration::from_millis(25)]);
    }

    #[cfg(windows)]
    #[test]
    fn remove_worktree_reports_residual_cleanup_failure_after_git_unregisters() {
        let repo_root = Path::new(r"C:\repo");
        let worktree_path = Path::new(r"C:\repo\.arborist\.worktrees\feature");
        let calls = std::cell::Cell::new(0usize);
        let residual_calls = std::cell::Cell::new(0usize);
        let sleeps = std::cell::RefCell::new(Vec::new());

        let err = remove_worktree_with_retry(
            repo_root,
            worktree_path,
            || {
                let n = calls.get();
                calls.set(n + 1);
                Ok(if n == 0 {
                    GitCommandResult {
                        success: false,
                        stderr: "error: failed to delete 'feature': Directory not empty".to_owned(),
                    }
                } else {
                    GitCommandResult {
                        success: false,
                        stderr: "fatal: 'C:\\repo\\.arborist\\.worktrees\\feature' is not a working tree".to_owned(),
                    }
                })
            },
            || {
                residual_calls.set(residual_calls.get() + 1);
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "still locked"))
            },
            |delay| sleeps.borrow_mut().push(delay),
        )
        .expect_err("residual cleanup failure should surface");

        assert_eq!(calls.get(), 2);
        assert_eq!(residual_calls.get(), WORKTREE_REMOVE_RETRY_DELAYS_MS.len() + 1);
        assert_eq!(sleeps.borrow().len(), WORKTREE_REMOVE_RETRY_DELAYS_MS.len() + 1);
        assert!(matches!(err, Error::Internal(msg) if msg.contains("residual directory cleanup failed") && msg.contains("still locked")));
    }

    /// Build a `Command` with both repo-selection *and* identity/config `GIT_*` variables stripped. Tests get a stricter scrub than production
    /// because hostile env (set by an outer `git push` invoking the husky pre-push hook) can otherwise reroute commits, override the repo, or spoof
    /// committer identity in ways that pollute the developer's real checkout (issue #13).
    fn clean_test_git_command() -> Command {
        let mut cmd = git_command();
        // Identity vars: keep test commits authored by the local `git config user.{name,email}` we set, regardless of any GIT_AUTHOR_* /
        // GIT_COMMITTER_* the parent process exported.
        for var in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_AUTHOR_DATE",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_COMMITTER_DATE",
        ] {
            cmd.env_remove(var);
        }
        // `git -c k=v` style env-driven config (`GIT_CONFIG_COUNT` + `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`) can change behavior in subtle ways.
        // Iterate the inherited env to drop the dynamic numbered keys too.
        for (k, _) in std::env::vars_os() {
            if let Some(s) = k.to_str() {
                if s.starts_with("GIT_CONFIG_") {
                    cmd.env_remove(&k);
                }
            }
        }
        cmd
    }

    fn run_clean_git_output(dir: &Path, args: &[&str]) -> Output {
        clean_test_git_command()
            // Pin process CWD to the tempdir as well as `-C`. On Windows, `git worktree add` resolves the new worktree's relative path *and*
            // picks the repo to register against using the process CWD, not just `-C`. Without this, a test run from inside another git repo
            // (which is the normal case for `cargo test`) can end up registering a worktree against the *outer* repo, polluting it with stale
            // entries.
            .current_dir(dir)
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git invocation")
    }

    fn run_clean_git(dir: &Path, args: &[&str]) {
        let output = run_clean_git_output(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = run_clean_git_output(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Initialise a fresh git repo in `dir`, with a single committed file so `worktree add -b` succeeds (it requires HEAD to point at a commit).
    fn init_git_repo(dir: &Path) {
        init_git_repo_on_branch(dir, "main");
    }

    fn init_git_repo_on_branch(dir: &Path, branch: &str) {
        run_clean_git(dir, &["init", "-q", "-b", branch]);
        run_clean_git(dir, &["config", "user.email", "test@arborist.local"]);
        run_clean_git(dir, &["config", "user.name", "Arborist Test"]);
        std::fs::write(dir.join("README"), b"hi").unwrap();
        run_clean_git(dir, &["add", "README"]);
        run_clean_git(dir, &["commit", "-q", "-m", "init"]);
    }

    fn init_bare_git_repo(dir: &Path) {
        let output = clean_test_git_command()
            .current_dir(dir)
            .args(["init", "-q", "--bare"])
            .output()
            .expect("git invocation");
        assert!(
            output.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit_file(dir: &Path, relative_path: &str, contents: &str, message: &str) {
        let path = dir.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents.as_bytes()).unwrap();
        run_clean_git(dir, &["add", relative_path]);
        run_clean_git(dir, &["commit", "-q", "-m", message]);
    }

    fn checkout(dir: &Path, branch: &str) {
        run_clean_git(dir, &["checkout", "-q", branch]);
    }

    fn create_branch(dir: &Path, branch: &str) {
        run_clean_git(dir, &["checkout", "-q", "-b", branch]);
    }

    fn current_head(dir: &Path) -> String {
        git_stdout(dir, &["rev-parse", "HEAD"])
    }

    fn git_status_porcelain(dir: &Path) -> String {
        git_stdout(dir, &["status", "--porcelain"])
    }

    struct MergeTreeCacheGuard {
        previous: Option<bool>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static MERGE_TREE_TEST_LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

    impl MergeTreeCacheGuard {
        fn new(temp: Option<bool>) -> Self {
            let lock = MERGE_TREE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut cache = MERGE_TREE_CAPABILITY.lock().unwrap_or_else(|e| e.into_inner());
            let previous = *cache;
            *cache = temp;
            drop(cache);
            Self { previous, _lock: lock }
        }
    }

    impl Drop for MergeTreeCacheGuard {
        fn drop(&mut self) {
            *MERGE_TREE_CAPABILITY.lock().unwrap_or_else(|e| e.into_inner()) = self.previous;
        }
    }

    /// RAII guard that force-removes any worktrees registered against `repo_root` and prunes stale entries before the underlying `TempDir` is
    /// dropped. Required on Windows where lingering files inside `.git/worktrees/<name>/` can defeat `TempDir`'s recursive-delete and leave junk on
    /// disk.
    ///
    /// As an extra safety net, also scrubs any worktree pointing into the tempdir from the *outer* repo containing the test process's CWD (typically
    /// the arborist checkout that hosts `cargo test`). This guards against regression of the historical bug where `git -C <tempdir> worktree add`
    /// registered against the outer repo instead of the tempdir repo.
    struct WorktreeCleanup {
        repo_root: PathBuf,
        tempdir: tempfile::TempDir,
    }

    impl WorktreeCleanup {
        fn new(tempdir: tempfile::TempDir) -> Self {
            let repo_root = tempdir.path().to_path_buf();
            Self { repo_root, tempdir }
        }

        fn path(&self) -> &Path {
            self.tempdir.path()
        }
    }

    impl Drop for WorktreeCleanup {
        fn drop(&mut self) {
            // Restrict every removal to paths under the tempdir. With the `GIT_*` env strip in `clean_test_git_command` this is now belt-and-braces,
            // but the predicate is the load-bearing invariant: even if env-strip is ever bypassed, we MUST NOT delete a worktree that doesn't belong
            // to our tempdir.
            let canon_temp = dunce::canonicalize(&self.repo_root).unwrap_or_else(|_| self.repo_root.clone());
            let inside_temp = |p: &Path| -> bool {
                let cp = dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                cp.starts_with(&canon_temp)
            };

            // Helper: in-repo cleanup of any linked worktrees matching `predicate`.
            let scrub = |repo: &Path, predicate: &dyn Fn(&Path) -> bool| {
                let Ok(out) = clean_test_git_command()
                    .current_dir(repo)
                    .arg("-C")
                    .arg(repo)
                    .args(["worktree", "list", "--porcelain"])
                    .output()
                else {
                    return;
                };
                if !out.status.success() {
                    return;
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                let parsed = parse_porcelain(&stdout);
                // Skip the first entry (the main worktree).
                for wt in parsed.into_iter().skip(1) {
                    if !predicate(&wt.path) {
                        continue;
                    }
                    // Retry once: under parallel test contention, a transient git lock can fail the first remove. Swallow the second failure — Drop
                    // must not panic.
                    let mut ok = false;
                    for _ in 0..2 {
                        let st = clean_test_git_command()
                            .current_dir(repo)
                            .arg("-C")
                            .arg(repo)
                            .args(["worktree", "remove", "--force"])
                            .arg(&wt.path)
                            .output();
                        if matches!(&st, Ok(o) if o.status.success()) {
                            ok = true;
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    if !ok {
                        eprintln!("WorktreeCleanup: failed to remove {} from {}", wt.path.display(), repo.display());
                    }
                }
                let _ = clean_test_git_command()
                    .current_dir(repo)
                    .arg("-C")
                    .arg(repo)
                    .args(["worktree", "prune"])
                    .output();
            };

            // 1. Clean every linked worktree registered in the tempdir repo. Constrain to
            //    paths inside the tempdir as well — see comment above on `inside_temp`.
            scrub(&self.repo_root, &inside_temp);

            // 2. Belt-and-braces: if the test process's CWD is inside another git repo,
            //    scrub any worktree there whose path lies under our tempdir. This is the
            //    safety net for the historical outer-repo-pollution bug (issue #13).
            if let Ok(cwd) = std::env::current_dir() {
                scrub(&cwd, &inside_temp);
            }
        }
    }

    #[test]
    fn real_runner_git_toplevel_returns_path_for_initialised_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        let runner = RealGitRunner;
        let top = runner.git_toplevel(dir.path()).unwrap().expect("Some");
        assert_eq!(
            top,
            dunce::canonicalize(dir.path()).unwrap(),
            "toplevel must canonicalize back to the repo root"
        );
    }

    #[test]
    fn real_runner_git_toplevel_returns_repo_root_when_called_from_subdir() {
        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        let sub = dir.path().join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let runner = RealGitRunner;
        let top = runner.git_toplevel(&sub).unwrap().expect("Some");
        assert_eq!(
            top,
            dunce::canonicalize(dir.path()).unwrap(),
            "toplevel must collapse to the repo root, not the subdir"
        );
    }

    #[test]
    fn real_runner_git_toplevel_none_for_plain_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let runner = RealGitRunner;
        assert_eq!(runner.git_toplevel(dir.path()).unwrap(), None);
    }

    #[test]
    fn real_runner_git_toplevel_none_for_missing_dir() {
        let runner = RealGitRunner;
        assert_eq!(runner.git_toplevel(Path::new("/no/such/path/arborist-test")).unwrap(), None);
    }

    #[test]
    fn real_runner_create_worktree_creates_branch_and_directory() {
        let dir = WorktreeCleanup::new(tempfile::TempDir::new().unwrap());
        init_git_repo(dir.path());
        let runner = RealGitRunner;
        // Pass a *relative* path — this matches the production contract (`worktree_create_impl` always passes `.worktrees/<branch>`) and is exactly
        // the input shape that previously triggered the outer-repo-pollution bug. The cleanup guard + production current_dir fix together keep this
        // hermetic.
        let new_path = runner
            .create_worktree(dir.path(), Path::new(".worktrees/feat-x"), "feat-x")
            .expect("create");
        assert!(new_path.is_dir(), "expected new worktree dir to exist");
        // The new worktree should also show up in list_worktrees.
        let listed = runner.list_worktrees(dir.path()).unwrap();
        assert!(
            listed.iter().any(|w| w.branch.as_deref() == Some("feat-x")),
            "expected feat-x in {listed:?}"
        );
    }

    #[test]
    fn real_runner_create_worktree_errors_on_missing_repo() {
        let runner = RealGitRunner;
        let err = runner
            .create_worktree(Path::new("/no/such/repo/arborist-test"), Path::new(".worktrees/x"), "x")
            .expect_err("must error");
        assert!(matches!(err, Error::WorktreeMissing(_)), "got {err:?}");
    }

    /// Regression for issue #13: every `git` we spawn must drop the repo-selection env vars so a hostile parent (e.g. husky pre-push) cannot reroute
    /// commits or worktree-add calls onto the developer's real checkout.
    #[test]
    fn git_command_strips_repo_selection_env_vars() {
        let cmd = git_command();
        let removed: Vec<String> = cmd
            .get_envs()
            .filter_map(|(k, v)| if v.is_none() { Some(k.to_string_lossy().into_owned()) } else { None })
            .collect();
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ] {
            assert!(removed.iter().any(|r| r == var), "git_command() must remove {var}; got {removed:?}");
        }
    }

    /// The test-only helper additionally strips identity and config-injection vars so commits land with the deterministic `git config user.*` we set
    /// in `init_git_repo`, regardless of any `GIT_AUTHOR_*` / `GIT_CONFIG_*` the parent process exported.
    #[test]
    fn clean_test_git_command_strips_identity_and_config_env() {
        let cmd = clean_test_git_command();
        let removed: Vec<String> = cmd
            .get_envs()
            .filter_map(|(k, v)| if v.is_none() { Some(k.to_string_lossy().into_owned()) } else { None })
            .collect();
        for var in [
            "GIT_DIR",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_AUTHOR_DATE",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_COMMITTER_DATE",
        ] {
            assert!(
                removed.iter().any(|r| r == var),
                "clean_test_git_command() must remove {var}; got {removed:?}"
            );
        }
    }

    #[test]
    fn git_command_mcp_strips_config_and_prompting_env_vars() {
        let root = Path::new(if cfg!(windows) { r"C:\repo" } else { "/repo" });
        let cmd = git_command_mcp(root);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter_map(|(k, v)| if v.is_none() { Some(k.to_string_lossy().into_owned()) } else { None })
            .collect();
        for var in [
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
            "GIT_SEQUENCE_EDITOR",
            "SSH_ASKPASS",
        ] {
            assert!(removed.iter().any(|r| r == var), "git_command_mcp() must remove {var}; got {removed:?}");
        }
        let envs: HashMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| v.map(|value| (k.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())))
            .collect();
        assert_eq!(envs.get("GIT_TERMINAL_PROMPT").map(String::as_str), Some("0"));
        assert_eq!(envs.get("GIT_EDITOR").map(String::as_str), Some("true"));
        assert_eq!(envs.get("GIT_MERGE_AUTOEDIT").map(String::as_str), Some("no"));
    }

    #[test]
    fn git_command_mcp_ro_sets_optional_locks_zero() {
        let root = Path::new(if cfg!(windows) { r"C:\repo" } else { "/repo" });
        let cmd = git_command_mcp_ro(root);
        let envs: HashMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| v.map(|value| (k.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())))
            .collect();
        assert_eq!(envs.get("GIT_OPTIONAL_LOCKS").map(String::as_str), Some("0"));
    }

    #[test]
    fn git_command_mcp_mut_adds_hook_and_merge_tool_overrides() {
        let root = Path::new(if cfg!(windows) { r"C:\repo" } else { "/repo" });
        let cmd = git_command_mcp_mut(root);
        let args: Vec<String> = cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        let hooks = format!("core.hooksPath={}", if cfg!(target_os = "windows") { "NUL" } else { "/dev/null" });
        assert!(
            args.windows(2).any(|pair| pair[0] == "-c" && pair[1] == hooks),
            "expected hooks override in {args:?}"
        );
        assert!(
            args.windows(2).any(|pair| pair[0] == "-c" && pair[1] == "merge.tool=false"),
            "expected merge.tool override in {args:?}"
        );
    }

    #[test]
    fn fetch_origin_succeeds_against_local_bare_remote() {
        let origin = tempfile::TempDir::new().unwrap();
        init_bare_git_repo(origin.path());
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let origin_path = origin.path().to_string_lossy().to_string();
        run_clean_git(repo.path(), &["remote", "add", "origin", &origin_path]);
        run_clean_git(repo.path(), &["push", "-u", "origin", "main"]);

        let runner = RealGitRunner;
        runner
            .fetch_origin(repo.path(), Duration::from_secs(5))
            .expect("fetch origin should succeed");
    }

    #[test]
    fn fetch_origin_returns_error_when_origin_missing() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let runner = RealGitRunner;
        let err = runner
            .fetch_origin(repo.path(), Duration::from_secs(5))
            .expect_err("missing origin should fail");
        assert!(matches!(err, GitError::CommandFailed { .. }));
    }

    #[test]
    fn branches_merged_into_returns_only_merged_branches() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "merged-branch");
        commit_file(repo.path(), "merged.txt", "merged", "merged branch");
        checkout(repo.path(), "main");
        run_clean_git(repo.path(), &["merge", "-q", "--no-edit", "merged-branch"]);
        create_branch(repo.path(), "unmerged-branch");
        commit_file(repo.path(), "unmerged.txt", "unmerged", "unmerged branch");
        checkout(repo.path(), "main");

        let runner = RealGitRunner;
        let head = current_head(repo.path());
        let branches = runner.branches_merged_into(repo.path(), &head).expect("branch --merged should succeed");
        assert!(branches.contains("main"));
        assert!(branches.contains("merged-branch"));
        assert!(!branches.contains("unmerged-branch"));
    }

    #[test]
    fn cherry_empty_true_for_cherry_picked_branch_and_false_for_diverged_branch() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "source");
        commit_file(repo.path(), "shared.txt", "source", "source commit");
        let source_oid = current_head(repo.path());
        checkout(repo.path(), "main");
        create_branch(repo.path(), "cherry");
        run_clean_git(repo.path(), &["cherry-pick", &source_oid]);
        checkout(repo.path(), "main");
        create_branch(repo.path(), "diverged");
        commit_file(repo.path(), "diverged.txt", "diverged", "diverged commit");
        checkout(repo.path(), "main");

        let runner = RealGitRunner;
        assert!(runner
            .cherry_empty(repo.path(), &source_oid, "cherry")
            .expect("cherry compare should succeed"));
        assert!(!runner
            .cherry_empty(repo.path(), &source_oid, "diverged")
            .expect("diverged compare should succeed"));
    }

    #[test]
    fn default_branch_prefers_origin_head() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        run_clean_git(repo.path(), &["remote", "add", "origin", "."]);
        run_clean_git(repo.path(), &["fetch", "origin"]);
        run_clean_git(repo.path(), &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);

        let runner = RealGitRunner;
        let info = runner.default_branch(repo.path()).expect("origin/HEAD should win");
        assert_eq!(
            info,
            DefaultBranchInfo {
                branch: "main".to_owned(),
                source: DefaultBranchSource::OriginHead,
            }
        );
    }

    #[test]
    fn default_branch_falls_back_to_main() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo_on_branch(repo.path(), "main");

        let runner = RealGitRunner;
        let info = runner.default_branch(repo.path()).expect("main fallback should succeed");
        assert_eq!(
            info,
            DefaultBranchInfo {
                branch: "main".to_owned(),
                source: DefaultBranchSource::Main,
            }
        );
    }

    #[test]
    fn default_branch_falls_back_to_master() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo_on_branch(repo.path(), "master");

        let runner = RealGitRunner;
        let info = runner.default_branch(repo.path()).expect("master fallback should succeed");
        assert_eq!(
            info,
            DefaultBranchInfo {
                branch: "master".to_owned(),
                source: DefaultBranchSource::Master,
            }
        );
    }

    #[test]
    fn default_branch_errors_when_unknown() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo_on_branch(repo.path(), "dev");

        let runner = RealGitRunner;
        let err = runner.default_branch(repo.path()).expect_err("unknown default branch should error");
        assert!(matches!(err, GitError::DefaultBranchUnknown));
    }

    #[test]
    fn rev_parse_verify_resolves_commits() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());

        let runner = RealGitRunner;
        let head = current_head(repo.path());
        assert_eq!(runner.rev_parse_verify(repo.path(), "HEAD").unwrap(), head);
    }

    #[test]
    fn rev_parse_verify_rejects_missing_refs() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());

        let runner = RealGitRunner;
        let err = runner.rev_parse_verify(repo.path(), "missing-ref").expect_err("missing ref should error");
        assert!(matches!(err, GitError::RefNotFound { ref_expr } if ref_expr == "missing-ref"));
    }

    #[test]
    fn merge_from_branch_merges_cleanly() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "feature");
        commit_file(repo.path(), "feature.txt", "feature", "feature commit");
        let source_oid = current_head(repo.path());
        checkout(repo.path(), "main");

        let runner = RealGitRunner;
        let outcome = runner
            .merge_from_branch(repo.path(), &source_oid, false, Duration::from_secs(5))
            .expect("clean merge should succeed");
        match outcome {
            MergeFromBranchOutcome::MergedCleanly { behind, .. } => assert_eq!(behind, 0),
            other => panic!("expected clean merge outcome, got {other:?}"),
        }
        assert_eq!(current_head(repo.path()), source_oid);
        assert_eq!(git_status_porcelain(repo.path()), "");
    }

    #[test]
    fn merge_from_branch_auto_aborts_conflicts_and_restores_clean_repo() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "feature");
        commit_file(repo.path(), "README", "feature", "feature change");
        let source_oid = current_head(repo.path());
        checkout(repo.path(), "main");
        commit_file(repo.path(), "README", "main", "main change");
        let main_before_merge = current_head(repo.path());

        let runner = RealGitRunner;
        let outcome = runner
            .merge_from_branch(repo.path(), &source_oid, false, Duration::from_secs(5))
            .expect("conflicted merge should return an outcome");
        match outcome {
            MergeFromBranchOutcome::AutoAborted { files } => assert!(files.iter().any(|file| file == "README")),
            other => panic!("expected auto-aborted conflict outcome, got {other:?}"),
        }
        assert_eq!(current_head(repo.path()), main_before_merge);
        assert_eq!(git_status_porcelain(repo.path()), "");
        assert!(!runner.has_merge_head(repo.path()).unwrap());
    }

    #[test]
    fn merge_from_branch_can_leave_conflicts_in_place() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "feature");
        commit_file(repo.path(), "README", "feature", "feature change");
        let source_oid = current_head(repo.path());
        checkout(repo.path(), "main");
        commit_file(repo.path(), "README", "main", "main change");

        let runner = RealGitRunner;
        let outcome = runner
            .merge_from_branch(repo.path(), &source_oid, true, Duration::from_secs(5))
            .expect("conflicted merge should return an outcome");
        match outcome {
            MergeFromBranchOutcome::LeftConflicted { files } => assert!(files.iter().any(|file| file == "README")),
            other => panic!("expected left-conflicted outcome, got {other:?}"),
        }
        assert!(runner.has_merge_head(repo.path()).unwrap());
        assert!(git_status_porcelain(repo.path()).contains("README"));
        runner.merge_abort(repo.path()).unwrap();
    }

    #[test]
    fn git_status_mcp_summarizes_dirty_state() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        std::fs::write(repo.path().join("README"), b"changed").unwrap();
        std::fs::write(repo.path().join("new.txt"), b"new").unwrap();

        let runner = RealGitRunner;
        let summary = runner.git_status_mcp(repo.path()).expect("git status summary should succeed");
        assert!(summary.dirty);
        assert_eq!(summary.file_count, 2);
        assert!(!summary.has_upstream);
        assert_eq!(summary.ahead_of_upstream, None);
        assert_eq!(summary.behind_upstream, None);
        assert_eq!(summary.error, None);
    }

    #[test]
    fn merge_tree_dry_run_reports_clean_merges() {
        let _guard = MergeTreeCacheGuard::new(None);
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "feature");
        commit_file(repo.path(), "feature.txt", "feature", "feature commit");
        let source_oid = current_head(repo.path());
        checkout(repo.path(), "main");
        let base_oid = current_head(repo.path());

        let runner = RealGitRunner;
        let outcome = runner
            .merge_tree_dry_run(repo.path(), &base_oid, &source_oid)
            .expect("merge-tree clean probe should succeed");
        assert_eq!(outcome, MergeTreeOutcome::Clean);
    }

    #[test]
    fn merge_tree_dry_run_reports_conflicts_with_file_names() {
        let _guard = MergeTreeCacheGuard::new(None);
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "feature");
        commit_file(repo.path(), "README", "feature", "feature change");
        let source_oid = current_head(repo.path());
        checkout(repo.path(), "main");
        commit_file(repo.path(), "README", "main", "main change");
        let base_oid = current_head(repo.path());

        let runner = RealGitRunner;
        let outcome = runner
            .merge_tree_dry_run(repo.path(), &base_oid, &source_oid)
            .expect("merge-tree conflict probe should succeed");
        match outcome {
            MergeTreeOutcome::Conflict { files } => assert!(files.iter().any(|file| file == "README")),
            other => panic!("expected conflict outcome, got {other:?}"),
        }
    }

    #[test]
    fn merge_tree_dry_run_returns_unsupported_when_capability_is_disabled() {
        let _guard = MergeTreeCacheGuard::new(Some(false));
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let base_oid = current_head(repo.path());

        let runner = RealGitRunner;
        let outcome = runner
            .merge_tree_dry_run(repo.path(), &base_oid, &base_oid)
            .expect("unsupported outcome should not error");
        assert_eq!(outcome, MergeTreeOutcome::Unsupported);
    }

    #[test]
    fn merge_abort_is_no_op_when_not_merging() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());

        let runner = RealGitRunner;
        runner
            .merge_abort(repo.path())
            .expect("merge abort should be a no-op when no merge is active");
        assert!(!runner.has_merge_head(repo.path()).unwrap());
    }

    #[test]
    fn has_merge_head_tracks_merge_state() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        create_branch(repo.path(), "feature");
        commit_file(repo.path(), "README", "feature", "feature change");
        checkout(repo.path(), "main");
        commit_file(repo.path(), "README", "main", "main change");

        let runner = RealGitRunner;
        assert!(!runner.has_merge_head(repo.path()).unwrap());
        let merge = run_clean_git_output(repo.path(), &["merge", "--no-edit", "feature"]);
        assert!(!merge.status.success(), "setup merge should conflict");
        assert!(runner.has_merge_head(repo.path()).unwrap());
        runner.merge_abort(repo.path()).unwrap();
        assert!(!runner.has_merge_head(repo.path()).unwrap());
    }

    // --- parse_status_v2 unit tests --- //

    fn join_nul(records: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        for r in records {
            buf.extend_from_slice(r.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn parse_status_v2_clean_tree_with_branch_header() {
        let raw = join_nul(&[
            "# branch.oid 0123456789abcdef0123456789abcdef01234567",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +0 -0",
        ]);
        let s = parse_status_v2(&raw);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!(s.head.as_deref(), Some("0123456789ab"));
        assert_eq!(s.ahead, 0);
        assert_eq!(s.behind, 0);
        assert_eq!(s.staged, 0);
        assert_eq!(s.unstaged, 0);
        assert_eq!(s.untracked, 0);
        assert!(s.files.is_empty());
        assert!(!s.files_truncated);
    }

    #[test]
    fn parse_status_v2_detached_head_omits_branch() {
        let raw = join_nul(&["# branch.oid abcdef0000000000000000000000000000000000", "# branch.head (detached)"]);
        let s = parse_status_v2(&raw);
        assert_eq!(s.branch, None);
        assert_eq!(s.head.as_deref(), Some("abcdef000000"));
    }

    #[test]
    fn parse_status_v2_ahead_behind() {
        let raw = join_nul(&["# branch.head feat", "# branch.upstream origin/feat", "# branch.ab +3 -2"]);
        let s = parse_status_v2(&raw);
        assert_eq!(s.ahead, 3);
        assert_eq!(s.behind, 2);
    }

    #[test]
    fn parse_status_v2_staged_unstaged_untracked_conflicted() {
        let raw = join_nul(&[
            "# branch.head main",
            "1 M. N... 100644 100644 100644 aa bb staged.txt",
            "1 .M N... 100644 100644 100644 cc dd unstaged.txt",
            "1 MM N... 100644 100644 100644 ee ff both.txt",
            "u UU N... 100644 100644 100644 100644 11 22 33 conflict.txt",
            "? untracked.txt",
        ]);
        let s = parse_status_v2(&raw);
        assert_eq!(s.staged, 2, "M. and MM contribute to staged");
        assert_eq!(s.unstaged, 2, ".M and MM contribute to unstaged");
        assert_eq!(s.conflicted, 1);
        assert_eq!(s.untracked, 1);
        // 4 unique paths but `both.txt` appears twice (staged + unstaged) and untracked.txt + conflict.txt each contribute one => 6 entries.
        assert_eq!(s.files.len(), 6);
        assert!(s
            .files
            .iter()
            .any(|f| f.path == "untracked.txt" && f.kind == GitStatusFileKind::Untracked));
        assert!(s
            .files
            .iter()
            .any(|f| f.path == "conflict.txt" && f.kind == GitStatusFileKind::Conflicted));
        assert!(s.files.iter().any(|f| f.path == "staged.txt" && f.kind == GitStatusFileKind::Staged));
        assert!(s.files.iter().any(|f| f.path == "unstaged.txt" && f.kind == GitStatusFileKind::Unstaged));
    }

    #[test]
    fn parse_status_v2_renamed_record_consumes_orig_path() {
        let raw = join_nul(&[
            "# branch.head main",
            "2 R. N... 100644 100644 100644 aa bb R100 new.txt",
            "old.txt",
            "1 .M N... 100644 100644 100644 cc dd after.txt",
        ]);
        let s = parse_status_v2(&raw);
        assert_eq!(s.staged, 1);
        assert_eq!(s.unstaged, 1);
        // `old.txt` must NOT have been treated as its own status record (no false "after.txt is unstaged" mis-attribution).
        let new_paths: Vec<&str> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert!(new_paths.contains(&"new.txt"), "expected new.txt in {new_paths:?}");
        assert!(new_paths.contains(&"after.txt"), "expected after.txt in {new_paths:?}");
        assert!(!new_paths.contains(&"old.txt"), "old.txt must be discarded");
    }

    #[test]
    fn parse_status_v2_files_truncated_when_over_cap() {
        let mut records = vec!["# branch.head main".to_owned()];
        for i in 0..MAX_GIT_STATUS_FILES + 5 {
            records.push(format!("? f{i}.txt"));
        }
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let s = parse_status_v2(&join_nul(&refs));
        assert_eq!(
            s.untracked as usize,
            MAX_GIT_STATUS_FILES + 5,
            "counts authoritative regardless of truncation"
        );
        assert_eq!(s.files.len(), MAX_GIT_STATUS_FILES);
        assert!(s.files_truncated);
    }

    #[test]
    fn parse_status_v2_empty_input() {
        let s = parse_status_v2(b"");
        assert_eq!(s, WorktreeGitStatus::default());
    }

    #[test]
    fn real_runner_git_status_clean_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        let runner = RealGitRunner;
        let s = runner.git_status(dir.path()).expect("git_status ok");
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.staged, 0);
        assert_eq!(s.unstaged, 0);
        assert_eq!(s.untracked, 0);
        assert_eq!(s.conflicted, 0);
        assert!(s.files.is_empty());
    }

    #[test]
    fn real_runner_git_status_detects_unstaged_and_untracked() {
        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        // Modify the committed file (unstaged) and add a new untracked file.
        std::fs::write(dir.path().join("README"), b"changed").unwrap();
        std::fs::write(dir.path().join("new-file.txt"), b"hello").unwrap();
        let runner = RealGitRunner;
        let s = runner.git_status(dir.path()).expect("git_status ok");
        assert_eq!(s.unstaged, 1, "modified README is unstaged");
        assert_eq!(s.untracked, 1, "new-file.txt is untracked");
        assert!(s.files.iter().any(|f| f.path == "README" && f.kind == GitStatusFileKind::Unstaged));
        assert!(s.files.iter().any(|f| f.path == "new-file.txt" && f.kind == GitStatusFileKind::Untracked));
    }

    #[test]
    fn real_runner_git_status_enumerates_each_file_inside_untracked_directory() {
        // Regression for the `--untracked-files=normal` → `=all` switch (PR #89
        // review feedback): with `normal`, an entirely untracked directory is
        // collapsed to a single `?? dir/` record and the dashboard count would
        // read 1 regardless of how many files live inside. With `all`, every
        // file is enumerated and the count matches the user's intuition.
        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        let sub = dir.path().join("new-dir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), b"a").unwrap();
        std::fs::write(sub.join("b.txt"), b"b").unwrap();
        std::fs::write(sub.join("c.txt"), b"c").unwrap();
        let runner = RealGitRunner;
        let s = runner.git_status(dir.path()).expect("git_status ok");
        assert_eq!(
            s.untracked, 3,
            "each file inside an entirely-untracked directory must be counted individually"
        );
        for name in ["new-dir/a.txt", "new-dir/b.txt", "new-dir/c.txt"] {
            assert!(
                s.files.iter().any(|f| f.path == name && f.kind == GitStatusFileKind::Untracked),
                "expected {name} to appear as an untracked file, got {:?}",
                s.files
            );
        }
    }

    #[test]
    fn real_runner_git_status_returns_default_for_non_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let runner = RealGitRunner;
        let s = runner.git_status(dir.path()).expect("graceful degradation");
        // `error` is set so the dashboard can distinguish "clean" from "unreadable";
        // every other field is at its default.
        assert!(s.error.is_some(), "non-repo dir should populate `error`");
        assert_eq!(WorktreeGitStatus { error: None, ..s.clone() }, WorktreeGitStatus::default());
    }

    #[test]
    fn real_runner_git_status_returns_default_for_missing_dir() {
        // Build a definitely-nonexistent path under a fresh tempdir so this test is
        // hermetic on Windows too (where a hard-coded `/no/such/path/...` becomes
        // a drive-rooted Unix-y path that isn't *guaranteed* to be absent).
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("definitely-missing").join("arborist-test-status");
        let runner = RealGitRunner;
        let s = runner.git_status(&missing).expect("graceful degradation");
        assert!(s.error.as_deref().unwrap_or("").contains("does not exist"));
        assert_eq!(WorktreeGitStatus { error: None, ..s.clone() }, WorktreeGitStatus::default());
    }

    // --- parse_rev_list_count unit tests --- //

    #[test]
    fn parse_rev_list_count_typical() {
        // Output format: "<left>\t<right>\n" where left = behind, right = ahead
        let output = b"3\t12\n";
        assert_eq!(parse_rev_list_count(output), Some((12, 3)));
    }

    #[test]
    fn parse_rev_list_count_zeros() {
        let output = b"0\t0\n";
        assert_eq!(parse_rev_list_count(output), Some((0, 0)));
    }

    #[test]
    fn parse_rev_list_count_no_trailing_newline() {
        let output = b"5\t7";
        assert_eq!(parse_rev_list_count(output), Some((7, 5)));
    }

    #[test]
    fn parse_rev_list_count_empty() {
        assert_eq!(parse_rev_list_count(b""), None);
    }

    #[test]
    fn parse_rev_list_count_garbage() {
        assert_eq!(parse_rev_list_count(b"not a number"), None);
    }

    // --- enrich_with_source_branch unit tests --- //

    #[test]
    fn enrich_skips_detached_head() {
        let mut status = WorktreeGitStatus {
            branch: None, // detached HEAD
            head: Some("abc123".to_owned()),
            ..Default::default()
        };
        let dir = tempfile::TempDir::new().unwrap();
        enrich_with_source_branch(&mut status, dir.path());
        assert_eq!(status.source_branch, None);
        assert_eq!(status.source_ahead, None);
        assert_eq!(status.source_behind, None);
    }

    #[test]
    fn enrich_returns_none_when_no_origin_remote() {
        // A repo with no origin remote → detect_source_branch returns None → fields stay None
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();
        let _ = Command::new("git").current_dir(path).args(["init", "-b", "main"]).output();
        let _ = Command::new("git")
            .current_dir(path)
            .args(["commit", "--allow-empty", "-m", "init"])
            .output();

        let mut status = WorktreeGitStatus {
            branch: Some("main".to_owned()),
            ..Default::default()
        };

        // Clear cache for this path so we hit detect_source_branch fresh
        SOURCE_BRANCH_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(path);

        enrich_with_source_branch(&mut status, path);
        assert_eq!(status.source_branch, None);
        assert_eq!(status.source_ahead, None);
        assert_eq!(status.source_behind, None);
    }

    #[test]
    fn enrich_skips_when_on_source_branch() {
        // When current_branch == detected source branch, enrichment is skipped (showing 0/0 relative to self is noise).
        // Set up a repo with an origin remote whose HEAD points to "main", then check out "main".
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();
        let _ = Command::new("git").current_dir(path).args(["init", "-b", "main"]).output();
        let _ = Command::new("git")
            .current_dir(path)
            .args(["config", "user.email", "test@test.com"])
            .output();
        let _ = Command::new("git").current_dir(path).args(["config", "user.name", "Test"]).output();
        let _ = Command::new("git")
            .current_dir(path)
            .args(["commit", "--allow-empty", "-m", "init"])
            .output();
        // Create a local "origin" remote pointing at self, then set origin/HEAD
        let _ = Command::new("git").current_dir(path).args(["remote", "add", "origin", "."]).output();
        let _ = Command::new("git").current_dir(path).args(["fetch", "origin"]).output();
        let _ = Command::new("git")
            .current_dir(path)
            .args(["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"])
            .output();

        let mut status = WorktreeGitStatus {
            branch: Some("main".to_owned()),
            ..Default::default()
        };

        // Clear cache so detect_source_branch runs fresh
        SOURCE_BRANCH_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(path);

        enrich_with_source_branch(&mut status, path);
        // Source branch IS "main" and we're ON "main" → skip, all fields stay None
        assert_eq!(status.source_branch, None);
        assert_eq!(status.source_ahead, None);
        assert_eq!(status.source_behind, None);
    }

    #[test]
    fn enrich_sets_all_source_fields_together_or_none() {
        // Verify the invariant: source_branch/source_ahead/source_behind are either all set or all None
        let mut status = WorktreeGitStatus {
            branch: Some("feature-x".to_owned()),
            ..Default::default()
        };
        let dir = tempfile::TempDir::new().unwrap();

        // Clear cache so fresh detection runs (which will fail on a non-git dir → graceful None)
        SOURCE_BRANCH_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(dir.path());

        enrich_with_source_branch(&mut status, dir.path());

        // All three must be None together (detection failed since it's not a git repo)
        assert_eq!(status.source_branch, None);
        assert_eq!(status.source_ahead, None);
        assert_eq!(status.source_behind, None);
    }
}
