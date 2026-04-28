//! Integration tests for `workspace_validate_impl` and
//! `worktree_create_impl` (Roadmap §1.1, §2.2). Use a fake [`GitRunner`]
//! so they don't require a real `git` binary.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arborist_lib::commands::session::{workspace_validate_impl, worktree_create_impl, AppContext};
use arborist_lib::config_store::ConfigStore;
use arborist_lib::git::GitRunner;
use arborist_lib::pty_pool::{PortablePtySpawner, PtyPool, PtySink};
use arborist_lib::types::{Error, PartialAppConfig, SessionId, SessionStatus, WorktreeInfo};
use tempfile::TempDir;

/// Configurable fake. `toplevel_for` controls `git_toplevel`'s response —
/// `None` means "not a repo", `Some(path)` means it returns that path
/// canonicalized.
struct FakeGitRunner {
    toplevel: Mutex<Option<PathBuf>>,
    /// `Ok(())` ⇒ create_worktree returns the joined path; `Err(s)` ⇒ it
    /// returns Error::Internal(s).
    create_outcome: Mutex<Result<(), String>>,
    last_create: Mutex<Option<(PathBuf, PathBuf, String)>>,
}

impl FakeGitRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            toplevel: Mutex::new(None),
            create_outcome: Mutex::new(Ok(())),
            last_create: Mutex::new(None),
        })
    }
    /// Configure git_toplevel to return `Some(canonical(path))` for any
    /// query — i.e. "this is a repo whose root is `path`".
    fn set_repo_root(&self, path: &Path) {
        *self.toplevel.lock().unwrap() = Some(dunce::canonicalize(path).unwrap());
    }
    fn set_create_err(&self, msg: &str) {
        *self.create_outcome.lock().unwrap() = Err(msg.to_owned());
    }
}

impl GitRunner for FakeGitRunner {
    fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, Error> {
        Ok(vec![])
    }
    fn git_toplevel(&self, _: &Path) -> Result<Option<PathBuf>, Error> {
        Ok(self.toplevel.lock().unwrap().clone())
    }
    fn create_worktree(
        &self,
        repo_root: &Path,
        relative_path: &Path,
        branch: &str,
    ) -> Result<PathBuf, Error> {
        *self.last_create.lock().unwrap() = Some((
            repo_root.to_path_buf(),
            relative_path.to_path_buf(),
            branch.to_owned(),
        ));
        match &*self.create_outcome.lock().unwrap() {
            Ok(()) => {
                let joined = repo_root.join(relative_path);
                std::fs::create_dir_all(&joined).ok();
                Ok(joined)
            }
            Err(msg) => Err(Error::Internal(msg.clone())),
        }
    }
}

fn null_sink() -> PtySink {
    let output = Arc::new(|_: &SessionId, _: String| {});
    let status = Arc::new(|_: &SessionId, _: SessionStatus, _: Option<u32>, _: Option<String>| {});
    PtySink::new(output, status, Arc::new(|_id, _evt| {}))
}

fn build_ctx(git: Arc<dyn GitRunner>, store_dir: &TempDir) -> Arc<AppContext> {
    let store = ConfigStore::open(store_dir.path()).unwrap();
    let pool = Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)));
    Arc::new(AppContext::new(
        pool,
        store,
        null_sink(),
        git,
        Arc::new(|_| {}),
    ))
}

// ---------- workspace_validate ----------

#[test]
fn workspace_validate_rejects_empty_path() {
    let store = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    let out = workspace_validate_impl(&ctx, Path::new("")).unwrap();
    assert!(!out.valid);
    assert!(out.error.unwrap().contains("empty"));
}

#[test]
fn workspace_validate_rejects_relative_path() {
    let store = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    let out = workspace_validate_impl(&ctx, Path::new("relative/path")).unwrap();
    assert!(!out.valid);
    assert!(out.error.unwrap().contains("absolute"));
}

#[test]
fn workspace_validate_rejects_missing_path() {
    let store = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    let out =
        workspace_validate_impl(&ctx, Path::new("/this/does/not/exist/arborist-test-xyz")).unwrap();
    assert!(!out.valid);
}

#[test]
fn workspace_validate_rejects_non_git_directory() {
    let store = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let runner = FakeGitRunner::new();
    // toplevel left as None ⇒ "not a repo"
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, dir.path()).unwrap();
    assert!(!out.valid);
    assert!(out.error.unwrap().contains("git repository"));
}

#[test]
fn workspace_validate_accepts_real_git_dir() {
    let store = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(dir.path());
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, dir.path()).unwrap();
    assert!(out.valid, "got {:?}", out.error);
    assert!(out.error.is_none());
}

#[test]
fn workspace_validate_rejects_subdirectory_of_a_repo() {
    // The user picked `<repo>/nested`, but the toplevel is `<repo>`.
    let store = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let nested = repo.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(repo.path());
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, &nested).unwrap();
    assert!(!out.valid);
    let err = out.error.unwrap();
    assert!(err.contains("repository root"), "got {err}");
}

// ---------- worktree_create ----------

fn set_workspace(ctx: &AppContext, root: &Path) {
    ctx.store
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(root.to_path_buf())),
            ..Default::default()
        })
        .unwrap();
}

#[test]
fn worktree_create_errors_when_workspace_unset() {
    let store = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    let err = worktree_create_impl(&ctx, "feat-x").expect_err("must err");
    let msg = format!("{err:?}");
    assert!(msg.contains("workspaceRoot"), "got {msg}");
}

#[test]
fn worktree_create_rejects_invalid_name() {
    let store = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    set_workspace(&ctx, ws.path());
    let err = worktree_create_impl(&ctx, "has space").expect_err("must err");
    assert!(format!("{err:?}").contains("space"));
}

#[test]
fn worktree_create_invokes_runner_with_relative_path() {
    let store = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let runner = FakeGitRunner::new();
    let ctx = build_ctx(runner.clone() as Arc<dyn GitRunner>, &store);
    set_workspace(&ctx, ws.path());

    let out = worktree_create_impl(&ctx, "feat-x").expect("ok");
    assert!(out.path.ends_with("feat-x"));

    let last = runner.last_create.lock().unwrap().clone().unwrap();
    let canon_ws = dunce::canonicalize(ws.path()).unwrap();
    assert_eq!(last.0, canon_ws);
    assert_eq!(last.1, PathBuf::from(".worktrees").join("feat-x"));
    assert_eq!(last.2, "feat-x");
}

#[test]
fn worktree_create_refuses_to_clobber_existing_directory() {
    let store = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    std::fs::create_dir_all(ws.path().join(".worktrees").join("feat-x")).unwrap();
    let runner = FakeGitRunner::new();
    let ctx = build_ctx(runner.clone() as Arc<dyn GitRunner>, &store);
    set_workspace(&ctx, ws.path());

    let err = worktree_create_impl(&ctx, "feat-x").expect_err("must err");
    assert!(format!("{err:?}").contains("already exists"));
    assert!(
        runner.last_create.lock().unwrap().is_none(),
        "must not invoke runner"
    );
}

#[test]
fn worktree_create_propagates_runner_failure() {
    let store = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let runner = FakeGitRunner::new();
    runner.set_create_err("simulated git failure");
    let ctx = build_ctx(runner as Arc<dyn GitRunner>, &store);
    set_workspace(&ctx, ws.path());

    let err = worktree_create_impl(&ctx, "feat-x").expect_err("must err");
    assert!(format!("{err:?}").contains("simulated git failure"));
}
