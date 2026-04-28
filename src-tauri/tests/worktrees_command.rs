//! Phase 10 integration test for the `worktrees_list` command business
//! logic. Exercises [`worktrees_list_impl`] with a fake [`GitRunner`] so
//! the test does not depend on a real `git` binary.
//!
//! Real-binary coverage of the porcelain parser lives in
//! `src/git.rs::tests` (the unit tests there spin up a real, empty
//! `tempdir` to assert the graceful-degradation path).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arborist_lib::commands::session::{worktrees_list_impl, AppContext};
use arborist_lib::config_store::ConfigStore;
use arborist_lib::git::GitRunner;
use arborist_lib::pty_pool::{PortablePtySpawner, PtyPool, PtySink};
use arborist_lib::types::{Error, SessionId, SessionStatus, WorktreeInfo};
use tempfile::TempDir;

#[derive(Default)]
struct FakeGitRunner {
    /// Recorded `repo_root` argument from the most recent call.
    last_root: Mutex<Option<PathBuf>>,
    /// Canned response returned to the next call.
    response: Mutex<Vec<WorktreeInfo>>,
}

impl FakeGitRunner {
    fn with(response: Vec<WorktreeInfo>) -> Arc<Self> {
        Arc::new(Self {
            last_root: Mutex::new(None),
            response: Mutex::new(response),
        })
    }
}

impl GitRunner for FakeGitRunner {
    fn list_worktrees(&self, repo_root: &Path) -> Result<Vec<WorktreeInfo>, Error> {
        *self.last_root.lock().unwrap() = Some(repo_root.to_path_buf());
        Ok(self.response.lock().unwrap().clone())
    }
    fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, Error> {
        Ok(Some(path.to_path_buf()))
    }
    fn create_worktree(
        &self,
        repo_root: &Path,
        relative_path: &Path,
        _branch: &str,
    ) -> Result<PathBuf, Error> {
        Ok(repo_root.join(relative_path))
    }
}

/// Minimal sink that swallows everything — we don't exercise the PTY here.
fn null_sink() -> PtySink {
    let output = Arc::new(|_id: &SessionId, _data: String| {});
    let status =
        Arc::new(|_id: &SessionId, _st: SessionStatus, _pid: Option<u32>, _msg: Option<String>| {});
    PtySink::new(output, status)
}

fn build_ctx(git: Arc<dyn GitRunner>, store_dir: &TempDir) -> Arc<AppContext> {
    let store = ConfigStore::open(store_dir.path()).unwrap();
    let pool = Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)));
    Arc::new(AppContext::new(pool, store, null_sink(), git))
}

#[test]
fn returns_canned_list_from_injected_runner() {
    let store_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let canned = vec![
        WorktreeInfo {
            path: repo_dir.path().to_path_buf(),
            branch: Some("main".into()),
            is_main: true,
            is_locked: false,
        },
        WorktreeInfo {
            path: repo_dir.path().join("..").join("repo-feature"),
            branch: Some("feature".into()),
            is_main: false,
            is_locked: false,
        },
    ];
    let runner = FakeGitRunner::with(canned.clone());
    let ctx = build_ctx(runner.clone() as Arc<dyn GitRunner>, &store_dir);

    let got = worktrees_list_impl(&ctx, repo_dir.path()).expect("ok");
    assert_eq!(got, canned);
    assert_eq!(
        runner.last_root.lock().unwrap().as_deref(),
        Some(repo_dir.path()),
    );
}

#[test]
fn missing_repo_root_returns_empty_without_invoking_runner() {
    let store_dir = TempDir::new().unwrap();
    let runner = FakeGitRunner::with(vec![WorktreeInfo {
        path: PathBuf::from("/nope"),
        branch: None,
        is_main: true,
        is_locked: false,
    }]);
    let ctx = build_ctx(runner.clone() as Arc<dyn GitRunner>, &store_dir);

    let bogus = PathBuf::from("/this/path/should/not/exist/arborist-phase10-test");
    let got = worktrees_list_impl(&ctx, &bogus).expect("graceful");
    assert!(got.is_empty(), "missing dir must short-circuit to empty");
    assert!(
        runner.last_root.lock().unwrap().is_none(),
        "runner must not be called for a missing dir",
    );
}

#[test]
fn empty_runner_response_passes_through() {
    let store_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();
    let runner = FakeGitRunner::with(Vec::new());
    let ctx = build_ctx(runner as Arc<dyn GitRunner>, &store_dir);
    let got = worktrees_list_impl(&ctx, repo_dir.path()).expect("ok");
    assert!(got.is_empty());
}
