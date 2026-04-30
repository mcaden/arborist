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

    /// Probe whether `path` is a git repository — runs
    /// `git -C <path> rev-parse --is-inside-work-tree`. Returns `Ok(true)`
    /// only on a clean exit-code-0 with stdout `true`. Used by the
    /// `workspace_validate` command (Roadmap §1.1).
    /// Run `git -C <path> rev-parse --show-toplevel`. Returns
    /// `Ok(Some(canonical_toplevel))` if `path` lies inside a git working
    /// tree, or `Ok(None)` otherwise (missing dir, non-repo, or git
    /// unavailable). Never errors on the "not a repo" case so the picker
    /// can show inline feedback without a toast.
    fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, Error>;

    /// Run `git -C <repo_root> worktree add <relative_path> -b <branch>`.
    /// `relative_path` is interpreted relative to `repo_root` (typically
    /// `.worktrees/<branch>`). Returns the canonical absolute path of the
    /// new worktree on success; otherwise an [`Error::Internal`] carrying
    /// the captured stderr.
    fn create_worktree(
        &self,
        repo_root: &Path,
        relative_path: &Path,
        branch: &str,
    ) -> Result<PathBuf, Error>;
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

    fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, Error> {
        if !path.is_dir() {
            return Ok(None);
        }
        let output = match Command::new("git")
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
        // `--show-toplevel` returns an already-absolute path; canonicalize it
        // so symlink hops collapse to the same form the caller will pass in.
        let canon = dunce::canonicalize(&raw).unwrap_or_else(|_| PathBuf::from(raw));
        Ok(Some(canon))
    }

    fn create_worktree(
        &self,
        repo_root: &Path,
        relative_path: &Path,
        branch: &str,
    ) -> Result<PathBuf, Error> {
        if !repo_root.is_dir() {
            return Err(Error::WorktreeMissing(repo_root.to_path_buf()));
        }
        let output = Command::new("git")
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
                if stderr.is_empty() {
                    "<no stderr>".to_owned()
                } else {
                    stderr
                }
            )));
        }
        // The new worktree lives at <repo_root>/<relative_path>.
        let new_path = repo_root.join(relative_path);
        dunce::canonicalize(&new_path).map_err(|e| {
            Error::Internal(format!(
                "worktree created but canonicalization failed: {}: {e}",
                new_path.display()
            ))
        })
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
            .list_worktrees(Path::new("/this/path/does/not/exist/arborist-test"))
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

    /// Initialise a fresh git repo in `dir`, with a single committed file so
    /// `worktree add -b` succeeds (it requires HEAD to point at a commit).
    fn init_git_repo(dir: &Path) {
        use std::process::Command;
        let run = |args: &[&str]| {
            let s = Command::new("git")
                // Pin process CWD to the tempdir as well as `-C`. On Windows,
                // `git worktree add` resolves the new worktree's relative
                // path *and* picks the repo to register against using the
                // process CWD, not just `-C`. Without this, a test run from
                // inside another git repo (which is the normal case for
                // `cargo test`) can end up registering a worktree against
                // the *outer* repo, polluting it with stale entries.
                .current_dir(dir)
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git invocation");
            assert!(
                s.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&s.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@arborist.local"]);
        run(&["config", "user.name", "Arborist Test"]);
        std::fs::write(dir.join("README"), b"hi").unwrap();
        run(&["add", "README"]);
        run(&["commit", "-q", "-m", "init"]);
    }

    /// RAII guard that force-removes any worktrees registered against
    /// `repo_root` and prunes stale entries before the underlying `TempDir`
    /// is dropped. Required on Windows where lingering files inside
    /// `.git/worktrees/<name>/` can defeat `TempDir`'s recursive-delete and
    /// leave junk on disk.
    ///
    /// As an extra safety net, also scrubs any worktree pointing into the
    /// tempdir from the *outer* repo containing the test process's CWD
    /// (typically the arborist checkout that hosts `cargo test`). This
    /// guards against regression of the historical bug where
    /// `git -C <tempdir> worktree add` registered against the outer repo
    /// instead of the tempdir repo.
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
            use std::process::Command;

            // Helper: in-repo cleanup of any linked worktrees.
            let scrub = |repo: &Path, predicate: &dyn Fn(&Path) -> bool| {
                let Ok(out) = Command::new("git")
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
                    // Retry once: under parallel test contention, a transient
                    // git lock can fail the first remove. Swallow the second
                    // failure — Drop must not panic.
                    let mut ok = false;
                    for _ in 0..2 {
                        let st = Command::new("git")
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
                        eprintln!(
                            "WorktreeCleanup: failed to remove {} from {}",
                            wt.path.display(),
                            repo.display()
                        );
                    }
                }
                let _ = Command::new("git")
                    .current_dir(repo)
                    .arg("-C")
                    .arg(repo)
                    .args(["worktree", "prune"])
                    .output();
            };

            // 1. Clean every linked worktree registered in the tempdir repo.
            scrub(&self.repo_root, &|_| true);

            // 2. Belt-and-braces: if the test process's CWD is inside another
            //    git repo, scrub any worktree there whose path lies under our
            //    tempdir. This is the safety net for the historical
            //    outer-repo-pollution bug.
            if let Ok(cwd) = std::env::current_dir() {
                let canon_temp =
                    dunce::canonicalize(&self.repo_root).unwrap_or_else(|_| self.repo_root.clone());
                scrub(&cwd, &|p| {
                    let cp = dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                    cp.starts_with(&canon_temp)
                });
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
        assert_eq!(
            runner
                .git_toplevel(Path::new("/no/such/path/arborist-test"))
                .unwrap(),
            None
        );
    }

    #[test]
    fn real_runner_create_worktree_creates_branch_and_directory() {
        let dir = WorktreeCleanup::new(tempfile::TempDir::new().unwrap());
        init_git_repo(dir.path());
        let runner = RealGitRunner;
        // Pass a *relative* path — this matches the production contract
        // (`worktree_create_impl` always passes `.worktrees/<branch>`) and
        // is exactly the input shape that previously triggered the
        // outer-repo-pollution bug. The cleanup guard + production
        // current_dir fix together keep this hermetic.
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
            .create_worktree(
                Path::new("/no/such/repo/arborist-test"),
                Path::new(".worktrees/x"),
                "x",
            )
            .expect_err("must error");
        assert!(matches!(err, Error::WorktreeMissing(_)), "got {err:?}");
    }
}
