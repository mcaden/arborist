//! Git integration — currently just `git worktree list --porcelain` parsing.
//!
//! The trait seam ([`GitRunner`]) lets tests inject canned outputs without
//! depending on a real `git` binary. The production implementation
//! ([`RealGitRunner`]) shells out to `git` and degrades gracefully:
//! any failure (binary missing, not a repo, parse error, IO) yields
//! `Ok(vec![])` with a `warn!` carrying a stable structured `code` so the
//! frontend never blocks on discovery — see SPEC §5.2 (the manual
//! "Browse…" affordance is always present).
//!
//! Porcelain format reference:
//! <https://git-scm.com/docs/git-worktree#_porcelain_format>
//!
//! Each worktree block looks like:
//! ```text
//! worktree /abs/path
//! HEAD <sha>
//! branch refs/heads/<name>      # OR `detached`
//! locked [<reason>]?            # optional
//! prunable [<reason>]?          # optional
//!
//! ```
//! Blocks are separated by blank lines; the very first one is the main
//! worktree.

use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{debug, warn};

use crate::types::{Error, WorktreeInfo};

/// Minimal seam over `git worktree list --porcelain`. Implementors must be
/// `Send + Sync` so we can stash one in `Arc<dyn GitRunner>` on the
/// `AppContext` and share it across worker threads.
pub trait GitRunner: Send + Sync {
    /// Enumerate the worktrees rooted at `repo_root`. Implementations MUST
    /// return `Ok(vec![])` rather than an error if discovery is impossible
    /// (missing binary, not a repo, IO error) — graceful degradation is a
    /// load-bearing requirement of the SPEC §5.2 create flow.
    fn list_worktrees(&self, repo_root: &Path) -> Result<Vec<WorktreeInfo>, Error>;
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

        let output = match Command::new("git")
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
            // Most common case: not a git repository. We don't bother
            // distinguishing reasons — the contract is "empty list on any
            // failure".
            warn!(
                code = "GitUnavailable",
                repo_root = %repo_root.display(),
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "git worktree list failed; returning empty list",
            );
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_porcelain(&stdout))
    }
}

/// Parse `git worktree list --porcelain` output. The first block is the
/// main worktree; subsequent blocks are linked worktrees. Detached HEADs
/// produce a `detached` line in place of `branch …`. Locked worktrees
/// carry a `locked` line (with an optional reason).
///
/// Pure function — no IO. Robust to empty input, trailing whitespace,
/// and unknown porcelain keys (silently skipped).
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
            // Begin a new block. Flush any in-progress block first (defensive
            // — porcelain blocks should be blank-line-separated, but we
            // don't want a missing blank line to swallow an entry).
            if let Some(p) = cur.take() {
                if let Some(info) = p.finish(is_first_block) {
                    out.push(info);
                }
                is_first_block = false;
            }
            cur = Some(PartialWorktree::new(PathBuf::from(rest)));
        } else if let Some(p) = cur.as_mut() {
            if let Some(branch_ref) = line.strip_prefix("branch ") {
                // Strip the conventional `refs/heads/` prefix to surface a
                // friendly branch name. Anything else is passed through.
                let name = branch_ref
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch_ref)
                    .to_owned();
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
            .list_worktrees(Path::new("/this/path/does/not/exist/grove-test"))
            .expect("graceful degradation must not error");
        assert!(out.is_empty());
    }

    #[test]
    fn real_runner_returns_empty_for_non_repo_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let runner = RealGitRunner;
        let out = runner
            .list_worktrees(dir.path())
            .expect("non-repo must degrade gracefully");
        // Empty even though `git` is on PATH: the command exits non-zero
        // because it isn't a repository.
        assert!(out.is_empty());
    }
}
