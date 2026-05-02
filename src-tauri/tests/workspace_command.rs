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
    /// Test-only: forget the configured repo root so subsequent
    /// `git_toplevel` queries return `None` ("not a repo").
    fn clear_repo_root(&self) {
        *self.toplevel.lock().unwrap() = None;
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
    fn remove_worktree(&self, _repo_root: &Path, _worktree_path: &Path) -> Result<(), Error> {
        Ok(())
    }
}

fn null_sink() -> PtySink {
    let output = Arc::new(|_: &SessionId, _: String| {});
    let status = Arc::new(|_: &SessionId, _: SessionStatus, _: Option<u32>, _: Option<String>| {});
    PtySink::new(output, status, Arc::new(|_id, _evt| {}))
}

/// Make a tempdir that looks like a *primary* git clone to
/// [`workspace_validate_impl`]: it has a `.git` *directory* at the
/// root. (Linked worktrees, which `validate_repo_root` rejects, would
/// have `.git` as a *file*.) Tests that pass the resulting path to
/// validate / switch / etc. must use this helper, not `TempDir::new()`.
fn make_repo_tempdir() -> TempDir {
    let td = TempDir::new().unwrap();
    std::fs::create_dir_all(td.path().join(".git")).unwrap();
    td
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
        Arc::new(|_, _| {}),
        Arc::new(|_, _| {}),
    ))
}

// ---------- workspace_validate ----------

#[test]
fn workspace_validate_rejects_empty_path() {
    let store = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    let out = workspace_validate_impl(&ctx, Path::new(""), None, "").unwrap();
    assert!(!out.valid);
    assert!(out.error.unwrap().contains("empty"));
}

#[test]
fn workspace_validate_rejects_relative_path() {
    let store = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    let out = workspace_validate_impl(&ctx, Path::new("relative/path"), None, "").unwrap();
    assert!(!out.valid);
    assert!(out.error.unwrap().contains("absolute"));
}

#[test]
fn workspace_validate_rejects_missing_path() {
    let store = TempDir::new().unwrap();
    let ctx = build_ctx(FakeGitRunner::new(), &store);
    let out = workspace_validate_impl(
        &ctx,
        Path::new("/this/does/not/exist/arborist-test-xyz"),
        None,
        "",
    )
    .unwrap();
    assert!(!out.valid);
}

#[test]
fn workspace_validate_rejects_non_git_directory() {
    let store = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let runner = FakeGitRunner::new();
    // toplevel left as None ⇒ "not a repo"
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, dir.path(), None, "").unwrap();
    assert!(!out.valid);
    assert!(out.error.unwrap().contains("git repository"));
}

#[test]
fn workspace_validate_accepts_real_git_dir() {
    let store = TempDir::new().unwrap();
    let dir = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(dir.path());
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, dir.path(), None, "").unwrap();
    assert!(out.valid, "got {:?}", out.error);
    assert!(out.error.is_none());
}

#[test]
fn workspace_validate_rejects_linked_worktree() {
    // Regression: the picker (and the boot resolution chain) used to
    // accept any path whose `git rev-parse --show-toplevel` equalled
    // itself. That signal is true for BOTH primary clones and linked
    // worktrees, but Arborist's session model requires a primary
    // root (you cannot `git worktree add` from inside a worktree).
    // The fix in `validate_repo_root` / `workspace_validate_impl`
    // additionally requires `<canon>/.git` to be a *directory*. A
    // linked worktree has `.git` as a *file* containing `gitdir: ...`.
    let store = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    // Simulate a linked worktree on disk: `.git` is a *file*, not a dir.
    std::fs::write(
        dir.path().join(".git"),
        "gitdir: /some/primary/repo/.git/worktrees/branch\n",
    )
    .unwrap();
    let runner = FakeGitRunner::new();
    // git_toplevel still returns the path itself — the runner can't
    // tell the difference, just like the real `git rev-parse`.
    runner.set_repo_root(dir.path());
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, dir.path(), None, "").unwrap();
    assert!(!out.valid, "linked worktree must be rejected");
    let err = out.error.unwrap();
    assert!(
        err.contains("linked git worktree"),
        "error must explain the worktree-vs-primary distinction; got {err}"
    );
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
    let out = workspace_validate_impl(&ctx, &nested, None, "").unwrap();
    assert!(!out.valid);
    let err = out.error.unwrap();
    assert!(err.contains("repository root"), "got {err}");
}

// ---------- workspace_validate advisory lock probe (Phase 8) ----------

#[test]
fn workspace_validate_skips_lock_probe_when_app_data_dir_is_none() {
    // Sanity: passing None for app_data_dir leaves
    // already_open_in_another_instance unset (advisory signal absent).
    let store = TempDir::new().unwrap();
    let dir = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(dir.path());
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, dir.path(), None, "").unwrap();
    assert!(out.valid);
    assert_eq!(out.already_open_in_another_instance, None);
}

#[test]
fn workspace_validate_reports_free_lock_as_not_already_open() {
    // Fresh app_data_dir → lock file doesn't exist → probe's
    // missing-file fast-path returns true (free) without creating
    // anything on disk → Some(false) on the wire.
    let app_data_dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    let dir = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(dir.path());
    let ctx = build_ctx(runner, &store);
    let out = workspace_validate_impl(&ctx, dir.path(), Some(app_data_dir.path()), "main").unwrap();
    assert!(out.valid);
    assert_eq!(out.already_open_in_another_instance, Some(false));
}

#[cfg(target_os = "windows")]
#[test]
fn workspace_validate_reports_held_lock_as_already_open_windows() {
    // Hold the (branch, workspace) lock from this test process; on
    // Windows LockFileEx is per-handle so the probe inside
    // workspace_validate_impl correctly observes contention.
    let app_data_dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    let dir = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(dir.path());
    let ctx = build_ctx(runner, &store);

    let canon = dunce::canonicalize(dir.path()).unwrap();
    let layout = arborist_lib::store_layout::StoreRoot::new(app_data_dir.path(), "main")
        .for_workspace(canon);
    std::fs::create_dir_all(layout.workspace_dir()).unwrap();
    let _holder = arborist_lib::workspace_lock::WorkspaceLockGuard::acquire(layout.lock_path())
        .expect("hold the lock from the test process");

    let out = workspace_validate_impl(&ctx, dir.path(), Some(app_data_dir.path()), "main").unwrap();
    assert!(out.valid);
    assert_eq!(out.already_open_in_another_instance, Some(true));
}

// ---------- worktree_create ----------

fn set_workspace(ctx: &AppContext, root: &Path) {
    ctx.store()
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

// ---------- workspace_switch (Phase 7) ----------

use arborist_lib::commands::session::workspace_switch_impl_inner;
use arborist_lib::workspace_lock::WorkspaceLockGuard;
use arborist_lib::workspace_scope::WorkspaceScope;
use std::sync::RwLock;

fn switch_emit_capturing(
    captured: Arc<Mutex<Vec<arborist_lib::types::WorkspaceChangedEvent>>>,
) -> Arc<dyn Fn(&arborist_lib::types::WorkspaceChangedEvent) + Send + Sync> {
    Arc::new(move |evt| {
        captured.lock().unwrap().push(evt.clone());
    })
}

/// Build a `Arc<AppContext>` whose `WorkspaceScope` is bound to a real
/// in-process `WorkspaceLockGuard` rooted at `workspace_a`. Returns the
/// AppContext and the path that's been canonicalised + locked.
fn build_switch_ctx(
    git: Arc<dyn GitRunner>,
    app_data_dir: &Path,
    workspace_a: &Path,
    branch: &str,
) -> (Arc<AppContext>, PathBuf) {
    let canon =
        dunce::canonicalize(workspace_a).expect("canonicalise initial workspace for test ctx");
    let layout = arborist_lib::store_layout::StoreRoot::new(app_data_dir, branch)
        .for_workspace(canon.clone());
    std::fs::create_dir_all(layout.workspace_dir()).unwrap();
    let lock = WorkspaceLockGuard::acquire(layout.lock_path()).expect("acquire initial test lock");
    let store = ConfigStore::from_layout(layout).expect("from_layout for test");
    let scope = WorkspaceScope::new(Some(canon.clone()), store, lock);
    let workspace = Arc::new(RwLock::new(scope));
    let pool = Arc::new(PtyPool::new(Arc::new(PortablePtySpawner)));
    let ctx = Arc::new(AppContext::with_workspace(
        pool,
        workspace,
        null_sink(),
        git,
        Arc::new(|_| {}),
        Arc::new(|_, _| {}),
        Arc::new(|_, _| {}),
    ));
    (ctx, canon)
}

#[tokio::test]
async fn workspace_switch_happy_path_swaps_and_emits() {
    let app_data_dir = TempDir::new().unwrap();
    let ws_a = make_repo_tempdir();
    let ws_b = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    // Both workspaces look like valid git repos to the runner.
    runner.set_repo_root(ws_a.path()); // initial set; for_repo_root currently returns one value
                                       // but our switch validates against ws_b — we'll re-set.

    let (ctx, ws_a_canon) = build_switch_ctx(
        Arc::clone(&runner) as Arc<dyn GitRunner>,
        app_data_dir.path(),
        ws_a.path(),
        "main",
    );

    // Re-point the runner at ws_b so workspace_validate_impl accepts it.
    runner.set_repo_root(ws_b.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let result = workspace_switch_impl_inner(
        &ctx,
        app_data_dir.path(),
        "main",
        ws_b.path(),
        switch_emit_capturing(captured.clone()),
    )
    .await
    .expect("switch must succeed");

    assert!(!result.no_op, "expected a real swap, not a no-op");
    let canon_b = dunce::canonicalize(ws_b.path()).unwrap();
    assert_eq!(result.workspace_root, canon_b);

    // The bound workspace_root snapshot must reflect ws_b.
    let bound = ctx
        .workspace
        .read()
        .unwrap()
        .workspace_root
        .clone()
        .expect("bound workspace_root present after switch");
    assert_eq!(bound, canon_b);

    // The new store must have workspace_root persisted (single source of truth).
    let cfg = ctx.store().load_config();
    assert_eq!(cfg.workspace_root, Some(canon_b.clone()));

    // workspace://changed emitted exactly once with the canonical path.
    let evts = captured.lock().unwrap();
    assert_eq!(evts.len(), 1);
    assert_eq!(evts[0].workspace_root, canon_b);

    // The OLD lock has been dropped; we can re-acquire it.
    let old_layout = arborist_lib::store_layout::StoreRoot::new(app_data_dir.path(), "main")
        .for_workspace(ws_a_canon.clone());
    let _re = WorkspaceLockGuard::acquire(old_layout.lock_path())
        .expect("old workspace lock must be free after switch");

    // Switch gate is open again so subsequent commands are not blocked.
    assert!(!ctx
        .switch_in_progress
        .load(std::sync::atomic::Ordering::SeqCst));

    // Restored gate was reset so the new workspace's restore_all_sessions
    // can fire when frontend_ready re-issues.
    assert!(!ctx.restored.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn workspace_switch_no_op_when_target_equals_current() {
    let app_data_dir = TempDir::new().unwrap();
    let ws = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(ws.path());

    let (ctx, ws_canon) = build_switch_ctx(
        Arc::clone(&runner) as Arc<dyn GitRunner>,
        app_data_dir.path(),
        ws.path(),
        "main",
    );

    let captured = Arc::new(Mutex::new(Vec::new()));
    let result = workspace_switch_impl_inner(
        &ctx,
        app_data_dir.path(),
        "main",
        ws.path(),
        switch_emit_capturing(captured.clone()),
    )
    .await
    .expect("no-op switch must succeed");

    assert!(result.no_op);
    assert_eq!(result.workspace_root, ws_canon);
    assert!(captured.lock().unwrap().is_empty(), "no event on no-op");
}

#[tokio::test]
async fn workspace_switch_refuses_invalid_target() {
    let app_data_dir = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(ws.path()); // for the initial scope

    let (ctx, _) = build_switch_ctx(
        Arc::clone(&runner) as Arc<dyn GitRunner>,
        app_data_dir.path(),
        ws.path(),
        "main",
    );

    // Drop the runner's repo-root so workspace_validate_impl rejects.
    runner.clear_repo_root();

    let captured = Arc::new(Mutex::new(Vec::new()));
    let bad_target = TempDir::new().unwrap();
    let err = workspace_switch_impl_inner(
        &ctx,
        app_data_dir.path(),
        "main",
        bad_target.path(),
        switch_emit_capturing(captured.clone()),
    )
    .await
    .expect_err("non-git target must error");
    assert_eq!(err.code, "InvalidPath");
    assert!(captured.lock().unwrap().is_empty());

    // Gate restored on failure.
    assert!(!ctx
        .switch_in_progress
        .load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn workspace_switch_returns_locked_when_target_is_held() {
    let app_data_dir = TempDir::new().unwrap();
    let ws_a = make_repo_tempdir();
    let ws_b = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(ws_a.path());
    let (ctx, _) = build_switch_ctx(
        Arc::clone(&runner) as Arc<dyn GitRunner>,
        app_data_dir.path(),
        ws_a.path(),
        "main",
    );

    // Pre-acquire ws_b's lock via the same layout so the switch races
    // against an in-process holder.
    runner.set_repo_root(ws_b.path());
    let canon_b = dunce::canonicalize(ws_b.path()).unwrap();
    let layout_b = arborist_lib::store_layout::StoreRoot::new(app_data_dir.path(), "main")
        .for_workspace(canon_b.clone());
    std::fs::create_dir_all(layout_b.workspace_dir()).unwrap();

    // Note: on Unix, fs2 per-process flock semantics may allow same-process
    // re-acquire — this test is meaningful only on Windows where LockFileEx
    // is per-handle. Cross-process contention is exercised by the
    // arborist-test-locker integration test.
    if cfg!(target_os = "windows") {
        let _holder =
            WorkspaceLockGuard::acquire(layout_b.lock_path()).expect("pre-acquire ws_b lock");

        let captured = Arc::new(Mutex::new(Vec::new()));
        let err = workspace_switch_impl_inner(
            &ctx,
            app_data_dir.path(),
            "main",
            ws_b.path(),
            switch_emit_capturing(captured.clone()),
        )
        .await
        .expect_err("must report contention");
        assert_eq!(err.code, "WorkspaceLocked");
        assert!(captured.lock().unwrap().is_empty());
        // Gate restored on contention failure.
        assert!(!ctx
            .switch_in_progress
            .load(std::sync::atomic::Ordering::SeqCst));
    }
}

// ---------- workspace switch park / restore round-trip ----------

/// Build a `Session` record for parking-round-trip tests. We only need
/// the persisted record (workspace switch step 7 enumerates sessions
/// from the store) — we deliberately avoid spawning a real PTY because
/// the assertions are about *record preservation*, not PTY lifecycle.
fn fake_session(worktree: &Path, label: &str) -> arborist_lib::types::Session {
    arborist_lib::types::Session {
        id: SessionId::new(),
        tool: arborist_lib::types::Tool::Claude,
        worktree_path: worktree.to_path_buf(),
        worktree_name: label.to_owned(),
        label: label.to_owned(),
        instruction_set_id: None,
        composed_command: format!("echo {label}"),
        status: SessionStatus::Running,
        pid: None,
        created_at: 1_700_000_000,
        tab_index: 0,
        temp_files: Vec::new(),
        ai_session_id: None,
    }
}

/// Regression for the in-app workspace-switch park semantics: the
/// switch-out path must **preserve** every old-workspace session
/// record (sessions.json, last_open_sessions, tab_order,
/// active_session_id) so that a later switch-back can restore them.
/// Previously the switch closed every session, permanently removing
/// the records.
#[tokio::test]
async fn workspace_switch_parks_old_sessions_preserving_records() {
    let app_data_dir = TempDir::new().unwrap();
    let ws_a = make_repo_tempdir();
    let ws_b = make_repo_tempdir();
    let runner = FakeGitRunner::new();
    runner.set_repo_root(ws_a.path());
    let (ctx, ws_a_canon) = build_switch_ctx(
        Arc::clone(&runner) as Arc<dyn GitRunner>,
        app_data_dir.path(),
        ws_a.path(),
        "main",
    );

    // Seed two sessions in workspace A's store + matching config keys.
    // No PTY: park's pool.kill is a no-op for sessions not in the pool.
    let s1 = fake_session(ws_a.path(), "alpha");
    let s2 = fake_session(ws_a.path(), "beta");
    ctx.store().save_session(&s1).unwrap();
    ctx.store().save_session(&s2).unwrap();
    ctx.store()
        .save_config(PartialAppConfig {
            last_open_sessions: Some(vec![s1.id, s2.id]),
            tab_order: Some(vec![s1.id, s2.id]),
            active_session_id: Some(Some(s1.id)),
            ..Default::default()
        })
        .unwrap();

    // Switch A → B.
    runner.set_repo_root(ws_b.path());
    let captured = Arc::new(Mutex::new(Vec::new()));
    workspace_switch_impl_inner(
        &ctx,
        app_data_dir.path(),
        "main",
        ws_b.path(),
        switch_emit_capturing(captured.clone()),
    )
    .await
    .expect("switch must succeed");

    // The bound store is now B's; B starts empty.
    assert!(ctx.store().load_sessions().is_empty(), "B starts empty");
    assert!(ctx.store().load_config().last_open_sessions.is_empty());

    // ❗ The crucial assertion: A's records must STILL be on disk under
    // its branch-scoped workspace dir, untouched by the park.
    let layout_a = arborist_lib::store_layout::StoreRoot::new(app_data_dir.path(), "main")
        .for_workspace(ws_a_canon.clone());
    let store_a_after =
        ConfigStore::from_layout(layout_a.clone()).expect("re-open A's store after switch");
    let a_sessions = store_a_after.load_sessions();
    assert_eq!(
        a_sessions.len(),
        2,
        "park must preserve A's session records; got {a_sessions:?}"
    );
    assert!(a_sessions.contains_key(&s1.id));
    assert!(a_sessions.contains_key(&s2.id));
    let a_cfg = store_a_after.load_config();
    assert_eq!(a_cfg.last_open_sessions, vec![s1.id, s2.id]);
    assert_eq!(a_cfg.tab_order, vec![s1.id, s2.id]);
    assert_eq!(a_cfg.active_session_id, Some(s1.id));
    drop(store_a_after);

    // Switch B → A. The records seeded above must still be present in
    // the freshly-bound A store (so restore_all_sessions has something
    // to spawn).
    runner.set_repo_root(ws_a.path());
    workspace_switch_impl_inner(
        &ctx,
        app_data_dir.path(),
        "main",
        ws_a.path(),
        switch_emit_capturing(captured.clone()),
    )
    .await
    .expect("switch back must succeed");

    let after = ctx.store().load_sessions();
    assert_eq!(after.len(), 2, "round-trip must preserve session count");
    assert!(after.contains_key(&s1.id));
    assert!(after.contains_key(&s2.id));
    let cfg_back = ctx.store().load_config();
    assert_eq!(cfg_back.last_open_sessions, vec![s1.id, s2.id]);
    assert_eq!(cfg_back.tab_order, vec![s1.id, s2.id]);
    assert_eq!(cfg_back.active_session_id, Some(s1.id));
}

/// Regression for the restore stale-worktree drop: if a parked
/// session's worktree was deleted by some other means (e.g.
/// `git worktree remove` while parked across a workspace switch),
/// `restore_all_sessions` must drop the persisted record + trim the
/// id from last_open_sessions / tab_order / active_session_id rather
/// than leave a permanent ghost tab in error state.
#[test]
fn restore_drops_session_when_worktree_directory_is_missing() {
    use arborist_lib::commands::session::restore_all_sessions;

    let store_dir = TempDir::new().unwrap();
    let runner = FakeGitRunner::new();
    let ctx = build_ctx(runner as Arc<dyn GitRunner>, &store_dir);

    // Persist a session pointing at a path that does not exist.
    let dead = store_dir.path().join("never-existed");
    let stale = fake_session(&dead, "ghost");
    ctx.store().save_session(&stale).unwrap();
    ctx.store()
        .save_config(PartialAppConfig {
            last_open_sessions: Some(vec![stale.id]),
            tab_order: Some(vec![stale.id]),
            active_session_id: Some(Some(stale.id)),
            ..Default::default()
        })
        .unwrap();

    restore_all_sessions(&ctx);

    assert!(
        ctx.store().load_sessions().is_empty(),
        "stale-worktree session must be removed from sessions.json"
    );
    let cfg = ctx.store().load_config();
    assert!(
        cfg.last_open_sessions.is_empty(),
        "stale id must be trimmed from last_open_sessions"
    );
    assert!(
        cfg.tab_order.is_empty(),
        "stale id must be trimmed from tab_order"
    );
    assert_eq!(
        cfg.active_session_id, None,
        "active_session_id pointing at the dropped session must be cleared"
    );
}
