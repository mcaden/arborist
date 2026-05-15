//! Phase 7 sub-session lifecycle integration tests: parent-close cascade, closing-parent tombstone, restore-on-launch second pass, and relaunch.
//!
//! Mirrors the FakeSpawner pattern from `tests/session_lifecycle_fake.rs` (each Rust integration test file is its own crate, so helpers must be
//! duplicated). Includes both terminal and application cascade behaviour (detach vs terminate policy), while finer-grained app-runtime semantics are
//! still covered in `app_launcher` unit tests.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arborist_lib::app_launcher::{AppKiller, AppPool, AppSpawner, AppWaiter, SpawnedApp};
use arborist_lib::commands::session::{session_create_impl, AppContext};
use arborist_lib::commands::subsession::{
    close_for_worktree_tab_impl, restore_all_sub_sessions_impl, subsession_close_impl, subsession_create_impl, subsession_relaunch_impl,
};
use arborist_lib::commands::worktree_tab::worktree_tab_close_impl;
use arborist_lib::compose::{copilot_otel_path, session_temp_dir};
use arborist_lib::config_store::ConfigStore;
use arborist_lib::git::GitRunner;
use arborist_lib::pty_pool::{ChildCommand, PtyKiller, PtyPool, PtyResize, PtySink, PtySpawner, PtyWaiter, SpawnedChild};
use arborist_lib::sub_sessions::{SubPtyPool, SubPtySink, SubSessionStore};
use arborist_lib::types::{
    ChildId, CustomProcessDef, CustomProcessDefId, CustomProcessKind, PartialAppConfig, SessionCreateArgs, SessionId, SessionStatus,
    SubSessionCloseIntent, SubSessionCreateArgs, SubSessionStatus, Tool, WorktreeInfo, WorktreeTab, WorktreeTabAppClosePolicy, WorktreeTabId,
};
use arborist_lib::window_focus::RecordingFocuser;
use portable_pty::{ExitStatus, PtySize};
use tempfile::TempDir;

// --------------------------------------------------------------------------- Fake parent-PTY spawner (cf.
// tests/session_lifecycle_fake.rs::FakeSpawner) ---------------------------------------------------------------------------

#[derive(Default)]
struct SpawnerState {
    spawn_count: usize,
    eofs: Vec<Arc<AtomicBool>>,
    next_pid: u32,
}

/// Failure-injection knobs for [`FakeSpawner`]. Tests flip these to exercise spawn/kill failure paths (see CP-07 cascade orphan branch and the
/// restore spawn-failure path).
#[derive(Default, Clone)]
struct SpawnerFlags {
    fail_spawn: Arc<AtomicBool>,
    kill_fails: Arc<AtomicBool>,
}

struct FakeSpawner {
    state: Mutex<SpawnerState>,
    flags: SpawnerFlags,
}

impl FakeSpawner {
    fn new() -> Self {
        Self {
            state: Mutex::new(SpawnerState {
                next_pid: 9000,
                ..SpawnerState::default()
            }),
            flags: SpawnerFlags::default(),
        }
    }

    fn flags(&self) -> SpawnerFlags {
        self.flags.clone()
    }
}

impl PtySpawner for FakeSpawner {
    fn spawn(&self, _cmd: ChildCommand, _cwd: &Path, _size: PtySize) -> Result<SpawnedChild, arborist_lib::types::Error> {
        if self.flags.fail_spawn.load(Ordering::SeqCst) {
            return Err(arborist_lib::types::Error::PtySpawnFailed("injected spawn failure".into()));
        }
        let mut s = self.state.lock().unwrap();
        s.spawn_count += 1;
        let pid = s.next_pid;
        s.next_pid += 1;
        let eof = Arc::new(AtomicBool::new(false));
        s.eofs.push(Arc::clone(&eof));

        Ok(SpawnedChild {
            pid,
            reader: Box::new(ParkedReader { eof: Arc::clone(&eof) }),
            writer: Box::new(WriteCapture),
            resize: Arc::new(NoopResize),
            waiter: Box::new(BlockingWaiter { eof: Arc::clone(&eof) }),
            killer: Arc::new(EofKiller {
                eof,
                fails: Arc::clone(&self.flags.kill_fails),
            }),
        })
    }
}

struct ParkedReader {
    eof: Arc<AtomicBool>,
}
impl Read for ParkedReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        while !self.eof.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(0)
    }
}
struct WriteCapture;
impl std::io::Write for WriteCapture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct NoopResize;
impl PtyResize for NoopResize {
    fn resize(&self, _: u16, _: u16) -> Result<(), arborist_lib::types::Error> {
        Ok(())
    }
}
struct EofKiller {
    eof: Arc<AtomicBool>,
    fails: Arc<AtomicBool>,
}
impl PtyKiller for EofKiller {
    fn kill(&self) -> Result<(), arborist_lib::types::Error> {
        // Always EOF so the reader/waiter threads can exit cleanly even on the failure path — otherwise leaked threads would block the test runner on
        // process exit. The pool only reads the killer's `Result` to decide Reaped vs Unconfirmed; the EOF signal is independent.
        self.eof.store(true, Ordering::Relaxed);
        if self.fails.load(Ordering::SeqCst) {
            return Err(arborist_lib::types::Error::Internal("injected killer failure".into()));
        }
        Ok(())
    }
}
struct BlockingWaiter {
    eof: Arc<AtomicBool>,
}
impl PtyWaiter for BlockingWaiter {
    fn wait(self: Box<Self>) -> Result<ExitStatus, arborist_lib::types::Error> {
        while !self.eof.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(ExitStatus::with_exit_code(0))
    }
}

// --------------------------------------------------------------------------- Fake app spawner
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeAppState {
    next_pid: u32,
    kill_calls: BTreeMap<u32, usize>,
}

#[derive(Clone)]
struct FakeAppSpawner {
    state: Arc<Mutex<FakeAppState>>,
}

impl FakeAppSpawner {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeAppState {
                next_pid: 40_000,
                kill_calls: BTreeMap::new(),
            })),
        }
    }

    fn kill_calls_for_pid(&self, pid: u32) -> usize {
        self.state.lock().ok().and_then(|s| s.kill_calls.get(&pid).copied()).unwrap_or(0)
    }
}

impl AppSpawner for FakeAppSpawner {
    fn spawn(&self, _cmd: &str, _cwd: &Path) -> Result<SpawnedApp, arborist_lib::types::Error> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| arborist_lib::types::Error::Internal("fake app state mutex poisoned".into()))?;
        let pid = state.next_pid;
        state.next_pid += 1;
        state.kill_calls.entry(pid).or_insert(0);
        drop(state);

        let done = Arc::new(AtomicBool::new(false));
        Ok(SpawnedApp {
            pid,
            waiter: Box::new(FakeAppWaiter { done: Arc::clone(&done) }),
            killer: Arc::new(FakeAppKiller {
                pid,
                done,
                state: Arc::clone(&self.state),
            }),
        })
    }
}

struct FakeAppWaiter {
    done: Arc<AtomicBool>,
}

impl AppWaiter for FakeAppWaiter {
    fn wait(self: Box<Self>) -> Result<bool, arborist_lib::types::Error> {
        let start = std::time::Instant::now();
        while !self.done.load(Ordering::Relaxed) && start.elapsed() < Duration::from_millis(250) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(true)
    }
}

struct FakeAppKiller {
    pid: u32,
    done: Arc<AtomicBool>,
    state: Arc<Mutex<FakeAppState>>,
}

impl AppKiller for FakeAppKiller {
    fn kill(&self) -> Result<(), arborist_lib::types::Error> {
        self.done.store(true, Ordering::Relaxed);
        if let Ok(mut s) = self.state.lock() {
            *s.kill_calls.entry(self.pid).or_insert(0) += 1;
        }
        Ok(())
    }
}

#[derive(Default)]
struct RecordingGitRunner {
    removes: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl RecordingGitRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl GitRunner for RecordingGitRunner {
    fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, arborist_lib::types::Error> {
        Ok(vec![])
    }

    fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, arborist_lib::types::Error> {
        Ok(Some(path.to_path_buf()))
    }

    fn create_worktree(&self, repo_root: &Path, relative_path: &Path, _branch: &str) -> Result<PathBuf, arborist_lib::types::Error> {
        Ok(repo_root.join(relative_path))
    }

    fn remove_worktree(&self, repo_root: &Path, worktree_path: &Path) -> Result<(), arborist_lib::types::Error> {
        self.removes.lock().unwrap().push((repo_root.to_path_buf(), worktree_path.to_path_buf()));
        Ok(())
    }

    fn git_status(&self, _worktree_path: &Path) -> Result<arborist_lib::types::WorktreeGitStatus, arborist_lib::types::Error> {
        Ok(arborist_lib::types::WorktreeGitStatus::default())
    }
}

// --------------------------------------------------------------------------- Capturing sinks
// ---------------------------------------------------------------------------

type StatusTuple = (arborist_lib::types::SubSessionId, SubSessionStatus, Option<u32>, Option<String>);

#[derive(Default)]
struct CapturedSubEvents {
    statuses: Mutex<Vec<StatusTuple>>,
    restored: Mutex<Vec<arborist_lib::types::SubSession>>,
}

fn make_sub_sink(events: Arc<CapturedSubEvents>) -> SubPtySink {
    let s = Arc::clone(&events);
    let status = Arc::new(
        move |id: &arborist_lib::types::SubSessionId, st: SubSessionStatus, pid: Option<u32>, msg: Option<String>| {
            s.statuses.lock().unwrap().push((*id, st, pid, msg));
        },
    );
    let r = Arc::clone(&events);
    let restored = Arc::new(move |sub: &arborist_lib::types::SubSession| {
        r.restored.lock().unwrap().push(sub.clone());
    });
    SubPtySink::new(Arc::new(|_, _| {}), status, Arc::new(|_, _| {}), restored)
}

fn make_parent_sink(store: ConfigStore) -> PtySink {
    let status = Arc::new(move |id: &SessionId, st: SessionStatus, pid: Option<u32>, _msg: Option<String>| {
        if let Err(e) = store.update_session_status(id, st, pid) {
            use arborist_lib::types::Error as E;
            if !matches!(e, E::NotFound(_)) {
                panic!("unexpected status persist error: {e:?}");
            }
        }
    });
    PtySink::new(Arc::new(|_, _| {}), status, Arc::new(|_, _| {}))
}

// --------------------------------------------------------------------------- Test harness
// ---------------------------------------------------------------------------

struct Harness {
    ctx: Arc<AppContext>,
    sub_ctx: Arc<arborist_lib::sub_sessions::SubAppContext>,
    sub_pool: Arc<SubPtyPool>,
    parent_spawner_flags: SpawnerFlags,
    sub_spawner_flags: SpawnerFlags,
    app_spawner: FakeAppSpawner,
    sub_events: Arc<CapturedSubEvents>,
    config_dir: TempDir,
    worktree: TempDir,
    shell_def_id: CustomProcessDefId,
    app_def_id: CustomProcessDefId,
    worktree_tab_id: WorktreeTabId,
}

fn build_harness() -> Harness {
    build_harness_with_git(Arc::new(arborist_lib::git::RealGitRunner) as Arc<dyn GitRunner>)
}

fn build_harness_with_git(git: Arc<dyn GitRunner>) -> Harness {
    let config_dir = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();

    let shell_def_id = CustomProcessDefId("shell".into());
    let shell_def = CustomProcessDef {
        id: shell_def_id.clone(),
        name: "Shell".into(),
        kind: CustomProcessKind::Terminal,
        command: "sh -i".into(),
        enabled: true,
        icon: None,
        icon_data_uri: None,
    };
    let app_def_id = CustomProcessDefId("app".into());
    let app_def = CustomProcessDef {
        id: app_def_id.clone(),
        name: "App".into(),
        kind: CustomProcessKind::Application,
        command: "app-launch".into(),
        enabled: true,
        icon: None,
        icon_data_uri: None,
    };

    let store = ConfigStore::open(config_dir.path()).unwrap();
    store
        .save_config(PartialAppConfig {
            custom_processes: Some(vec![shell_def, app_def]),
            ..Default::default()
        })
        .unwrap();

    // Create a worktree tab so sub-sessions have a parent.
    let worktree_tab_id = WorktreeTabId::new();
    let wt_tab = WorktreeTab {
        id: worktree_tab_id,
        path: worktree.path().to_path_buf(),
        name: "wt".into(),
        branch: None,
        label: "wt".into(),
        active_child_id: None,
        tab_index: 0,
        icon_id: 1,
    };
    store
        .save_config_with(PartialAppConfig::default(), |cfg| {
            cfg.worktree_tabs.push(wt_tab.clone());
            cfg.worktree_tab_order.push(worktree_tab_id);
            true
        })
        .unwrap();

    let parent_spawner = Arc::new(FakeSpawner::new());
    let parent_spawner_flags = parent_spawner.flags();
    let parent_pool = Arc::new(PtyPool::new(parent_spawner.clone() as Arc<dyn PtySpawner>));
    let parent_sink = make_parent_sink(store.clone());
    let ctx = Arc::new(AppContext::new(
        parent_pool,
        store.clone(),
        parent_sink,
        git,
        Arc::new(|_| {}),
        Arc::new(|_, _| {}),
        Arc::new(|_, _| {}),
    ));

    let sub_spawner = Arc::new(FakeSpawner::new());
    let sub_spawner_flags = sub_spawner.flags();
    let sub_pool = Arc::new(SubPtyPool::new(sub_spawner.clone() as Arc<dyn PtySpawner>));
    let sub_store = Arc::new(SubSessionStore::new());
    let sub_events = Arc::new(CapturedSubEvents::default());
    let sub_sink = make_sub_sink(Arc::clone(&sub_events));
    let app_spawner = FakeAppSpawner::new();
    let app_pool = Arc::new(AppPool::new(Arc::new(app_spawner.clone()) as Arc<dyn AppSpawner>));
    let focuser = Arc::new(RecordingFocuser::new());
    let icon_cache = Arc::new(arborist_lib::process_icon::IconCache::new(Arc::new(
        arborist_lib::process_icon::RealIconExtractor,
    )));
    let plugin_registry = Arc::new(arborist_lib::plugins::build_registry().expect("plugin registry build"));
    let sub_ctx = Arc::new(arborist_lib::sub_sessions::SubAppContext::new(
        Arc::clone(&sub_pool),
        Arc::clone(&sub_store),
        sub_sink,
        plugin_registry,
        app_pool,
        focuser,
        icon_cache,
    ));

    Harness {
        ctx,
        sub_ctx,
        sub_pool,
        parent_spawner_flags,
        sub_spawner_flags,
        app_spawner,
        sub_events,
        config_dir,
        worktree,
        shell_def_id,
        app_def_id,
        worktree_tab_id,
    }
}

fn create_parent(h: &Harness) -> arborist_lib::types::SessionView {
    session_create_impl(
        &h.ctx,
        SessionCreateArgs {
            tool: Tool::Claude,
            worktree_path: h.worktree.path().to_path_buf(),
            cols: 80,
            rows: 24,
        },
    )
    .expect("parent create ok")
}

fn create_copilot_parent(h: &Harness) -> arborist_lib::types::SessionView {
    session_create_impl(
        &h.ctx,
        SessionCreateArgs {
            tool: Tool::Copilot,
            worktree_path: h.worktree.path().to_path_buf(),
            cols: 80,
            rows: 24,
        },
    )
    .expect("copilot parent create ok")
}

fn create_sub(h: &Harness, tab_id: WorktreeTabId) -> Result<arborist_lib::types::SubSession, arborist_lib::types::AppError> {
    subsession_create_impl(
        &h.ctx,
        &h.sub_ctx,
        SubSessionCreateArgs {
            parent_worktree_tab_id: tab_id,
            def_id: h.shell_def_id.clone(),
        },
    )
}

fn create_app_sub(h: &Harness, tab_id: WorktreeTabId) -> Result<arborist_lib::types::SubSession, arborist_lib::types::AppError> {
    subsession_create_impl(
        &h.ctx,
        &h.sub_ctx,
        SubSessionCreateArgs {
            parent_worktree_tab_id: tab_id,
            def_id: h.app_def_id.clone(),
        },
    )
}

fn wait_until<F: FnMut() -> bool>(mut f: F, dur: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

// --------------------------------------------------------------------------- Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cascade_kills_terminal_subs_and_prunes_persistence() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub_a = create_sub(&h, h.worktree_tab_id).expect("sub a created");
    let sub_b = create_sub(&h, h.worktree_tab_id).expect("sub b created");

    assert!(wait_until(|| h.sub_pool.contains(&sub_a.id), Duration::from_secs(2)));
    assert!(wait_until(|| h.sub_pool.contains(&sub_b.id), Duration::from_secs(2)));
    assert_eq!(h.sub_ctx.store.list_for_worktree_tab(&h.worktree_tab_id).len(), 2);
    assert_eq!(h.ctx.store().load_config().last_open_sub_sessions.len(), 2);

    // Cascade and verify both sub-sessions are gone everywhere.
    let _ = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Detach).await;

    assert!(!h.sub_pool.contains(&sub_a.id), "sub_a still in pool");
    assert!(!h.sub_pool.contains(&sub_b.id), "sub_b still in pool");
    assert!(h.sub_ctx.store.list_for_worktree_tab(&h.worktree_tab_id).is_empty(), "store not pruned");
    assert!(h.ctx.store().load_config().last_open_sub_sessions.is_empty(), "persistence not pruned");
}

#[tokio::test]
async fn worktree_tab_close_refuses_delete_when_child_session_kill_is_unconfirmed() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(h.worktree.path().parent().unwrap().to_path_buf())),
            ..Default::default()
        })
        .unwrap();
    let parent = create_parent(&h);
    h.parent_spawner_flags.kill_fails.store(true, Ordering::SeqCst);

    let result = worktree_tab_close_impl(
        &h.ctx,
        Arc::clone(&h.sub_ctx),
        h.worktree_tab_id,
        true,
        WorktreeTabAppClosePolicy::Terminate,
    )
    .await
    .expect("worktree tab close should converge even when child teardown is unconfirmed");

    assert!(
        result
            .child_errors
            .iter()
            .any(|msg| msg.contains(&parent.id.to_string()) && msg.contains("unconfirmed")),
        "child teardown warning should be returned, got {:?}",
        result.child_errors,
    );
    let delete_error = result
        .worktree_delete_error
        .expect("worktree deletion should be refused when a child may still be alive");
    assert!(
        delete_error.contains("refusing to delete worktree") && delete_error.contains("child teardown") && delete_error.contains("unconfirmed"),
        "unexpected delete error: {delete_error}",
    );
    assert!(
        git.removes.lock().unwrap().is_empty(),
        "git worktree remove must not run after unconfirmed child teardown"
    );
}

#[tokio::test]
async fn worktree_tab_close_removes_copilot_otel_temp_file() {
    let h = build_harness();
    let parent = create_copilot_parent(&h);
    let otel_path = copilot_otel_path(&parent.id);
    let temp_dir = session_temp_dir(&parent.id);
    assert!(otel_path.exists(), "Copilot spawn prep should create otel.jsonl");

    worktree_tab_close_impl(
        &h.ctx,
        Arc::clone(&h.sub_ctx),
        h.worktree_tab_id,
        false,
        WorktreeTabAppClosePolicy::Terminate,
    )
    .await
    .expect("worktree tab close ok");

    assert!(
        !otel_path.exists(),
        "worktree-tab close should remove Copilot otel.jsonl through child session teardown"
    );
    assert!(!temp_dir.exists(), "worktree-tab close should remove the session temp dir");
}

#[tokio::test]
async fn cascade_terminate_policy_still_kills_terminal_subs() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    let child_errors = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Terminate).await;

    assert!(child_errors.is_empty(), "unexpected child errors: {child_errors:?}");
    assert!(!h.sub_pool.contains(&sub.id), "terminal sub should be killed regardless of app policy");
    assert!(h.sub_ctx.store.list_for_worktree_tab(&h.worktree_tab_id).is_empty(), "store not pruned");
    assert!(h.ctx.store().load_config().last_open_sub_sessions.is_empty(), "persistence not pruned");
}

#[tokio::test]
async fn cascade_detach_policy_keeps_application_process_running() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let app_sub = create_app_sub(&h, h.worktree_tab_id).expect("app sub created");
    let app_pid = app_sub.pid.expect("app pid");
    assert!(wait_until(|| h.sub_ctx.app_pool.contains(&app_sub.id), Duration::from_secs(1)));

    let child_errors = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Detach).await;

    assert!(child_errors.is_empty(), "unexpected child errors: {child_errors:?}");
    assert_eq!(h.app_spawner.kill_calls_for_pid(app_pid), 0, "detach policy must not kill app runtimes");
    assert!(!h.sub_ctx.app_pool.contains(&app_sub.id), "app runtime should be detached from pool");
    assert!(
        h.sub_ctx.store.get(&app_sub.id).is_none(),
        "app sub should be removed from in-memory store"
    );
}

#[tokio::test]
async fn cascade_terminate_policy_kills_non_retargeted_application_runtime() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let app_sub = create_app_sub(&h, h.worktree_tab_id).expect("app sub created");
    let app_pid = app_sub.pid.expect("app pid");
    assert!(wait_until(|| h.sub_ctx.app_pool.contains(&app_sub.id), Duration::from_secs(1)));

    let child_errors = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Terminate).await;

    assert!(child_errors.is_empty(), "unexpected child errors: {child_errors:?}");
    assert_eq!(
        h.app_spawner.kill_calls_for_pid(app_pid),
        1,
        "terminate policy should kill non-re-targeted app runtimes"
    );
    assert!(!h.sub_ctx.app_pool.contains(&app_sub.id), "app runtime should be gone after terminate");
    assert!(
        h.sub_ctx.store.get(&app_sub.id).is_none(),
        "app sub should be removed from in-memory store"
    );
}

#[tokio::test]
async fn worktree_tab_closing_tombstone_rejects_new_subs_and_cascades() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Mark worktree tab as closing, cascade, then verify cleanup.
    {
        let _guard = h.ctx.mark_worktree_tab_closing(h.worktree_tab_id);
        assert!(h.ctx.is_worktree_tab_closing(&h.worktree_tab_id));
        // While the tombstone is set, new sub-creates must be rejected.
        let blocked = create_sub(&h, h.worktree_tab_id);
        assert!(blocked.is_err(), "tombstone should reject new subs");
        let err = blocked.err().unwrap();
        assert_eq!(err.code, "InvalidArgument");

        let _ = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Detach).await;
    }

    // Guard dropped: tombstone clears, sub gone.
    assert!(!h.ctx.is_worktree_tab_closing(&h.worktree_tab_id));
    assert!(h.sub_ctx.store.list_for_worktree_tab(&h.worktree_tab_id).is_empty());
    assert!(h.ctx.store().load_config().last_open_sub_sessions.is_empty());
}

#[tokio::test]
async fn restore_drops_orphan_records_when_worktree_tab_is_gone() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Tear down existing subs.
    let _ = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Detach).await;

    // Remove the worktree tab from config so the orphan record references a non-existent tab.
    h.ctx
        .store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            cfg.worktree_tabs.retain(|t| t.id != h.worktree_tab_id);
            cfg.worktree_tab_order.retain(|tid| *tid != h.worktree_tab_id);
            true
        })
        .unwrap();

    // Manually re-add an orphan record (simulates a crash/rollback that left a sub persisted under a now-gone tab).
    let orphan = arborist_lib::types::SubSessionRecord {
        id: arborist_lib::types::SubSessionId::default(),
        parent_session_id: None,
        parent_worktree_tab_id: Some(h.worktree_tab_id),
        def_id: h.shell_def_id.clone(),
        kind: CustomProcessKind::Terminal,
        label: "Shell".into(),
        composed_command: "sh -i".into(),
    };
    let orphan_id = orphan.id;
    h.ctx.store().append_last_open_sub_session(orphan).unwrap();
    // The orphan record is written to disk, but load_config() sanitizes it away since its tab is gone.
    // We verify that restore_all_sub_sessions_impl completes cleanly regardless.

    restore_all_sub_sessions_impl(&h.ctx, &h.sub_ctx);

    assert!(
        h.ctx.store().load_config().last_open_sub_sessions.is_empty(),
        "orphan should have been dropped from persistence"
    );
    assert!(h.sub_ctx.store.get(&orphan_id).is_none(), "orphan should not appear in store");
    assert!(
        h.sub_events.restored.lock().unwrap().is_empty(),
        "no restored event should fire for the orphan"
    );
}

#[tokio::test]
async fn restore_respawns_terminal_subs_under_extant_parent() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Simulate a fresh app launch: drop the in-memory store + pool but keep persistence (mirrors what happens between runs of the app).
    h.sub_ctx.store.remove(&sub.id);
    h.sub_pool.kill(&sub.id).await.ok();

    // Stable id: the persisted record still references it.
    let persisted = h.ctx.store().load_config().last_open_sub_sessions;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].id, sub.id);

    restore_all_sub_sessions_impl(&h.ctx, &h.sub_ctx);

    // restored event was emitted for the row.
    let restored_evs = h.sub_events.restored.lock().unwrap().clone();
    assert_eq!(restored_evs.len(), 1, "expected one restored event");
    assert_eq!(restored_evs[0].id, sub.id);
    assert_eq!(restored_evs[0].kind, CustomProcessKind::Terminal);
    // Status is Starting at restore time; the pool will emit Running shortly after the spawn — we just check the row is in the store.
    assert!(h.sub_ctx.store.get(&sub.id).is_some());
    // Pool repopulated: a new spawn happened under the same id.
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));
}

#[tokio::test]
async fn restore_rejects_records_under_closing_worktree_tab() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Drop the in-memory store but keep persistence (mirrors restart).
    h.sub_ctx.store.remove(&sub.id);
    h.sub_pool.kill(&sub.id).await.ok();

    let _guard = h.ctx.mark_worktree_tab_closing(h.worktree_tab_id);
    restore_all_sub_sessions_impl(&h.ctx, &h.sub_ctx);

    assert!(
        h.ctx.store().load_config().last_open_sub_sessions.is_empty(),
        "records under closing worktree tab must be pruned"
    );
    assert!(
        h.sub_ctx.store.get(&sub.id).is_none(),
        "no row should be inserted under a closing worktree tab"
    );
}

#[tokio::test]
async fn relaunch_swaps_terminal_pty_under_same_id() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    let returned = subsession_relaunch_impl(&h.ctx, &h.sub_ctx, sub.id).await.expect("relaunch ok");

    assert_eq!(returned.id, sub.id, "id must be stable across relaunch");
    // The kill path EOFs the old reader; the spawn path inserts a new entry under the same id. Both observable as `contains`.
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));
    // Persistence row still present and points at the same id.
    let persisted = h.ctx.store().load_config().last_open_sub_sessions;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].id, sub.id);
}

#[tokio::test]
async fn relaunch_rejects_when_def_was_deleted() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // User deletes the def via Settings. Persisted sub record still references the gone def id.
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            custom_processes: Some(vec![]),
            ..Default::default()
        })
        .unwrap();

    let err = subsession_relaunch_impl(&h.ctx, &h.sub_ctx, sub.id)
        .await
        .expect_err("relaunch must fail when def is gone");
    assert_eq!(err.code, "NotFound");
}

#[tokio::test]
async fn create_under_closing_worktree_tab_is_rejected() {
    let h = build_harness();
    let _parent = create_parent(&h).id;

    let _guard = h.ctx.mark_worktree_tab_closing(h.worktree_tab_id);
    let result = create_sub(&h, h.worktree_tab_id);
    assert!(result.is_err());
    assert_eq!(result.err().unwrap().code, "InvalidArgument");
}

// --------------------------------------------------------------------------- Failure-path tests (CP-07: orphans must stay visible, never silently
// leak) ---------------------------------------------------------------------------

/// On `subsession_create_impl` persistence failure (e.g. config dir vanished) the in-memory store row must be rolled back AND the runtime PTY must be
/// torn down so the user retains a consistent view + can retry without leaking a child PTY. Mirrors the relaunch rollback path above (see
/// `subsession_relaunch_impl` in commands/subsession.rs).
#[tokio::test]
async fn create_rolls_back_inmemory_on_persist_failure() {
    let h = build_harness();
    let _parent = create_parent(&h).id;

    // Force every subsequent `write_atomic` to fail by replacing the config file with a directory of the same name. `tmp.persist()` can't rename a
    // NamedTempFile over a directory on either OS, so `append_last_open_sub_session` will return Err and trip the create-path rollback.
    let cfg_path = h.config_dir.path().join("config.json");
    std::fs::remove_file(&cfg_path).ok();
    std::fs::create_dir(&cfg_path).expect("replace config.json with dir");

    let result = create_sub(&h, h.worktree_tab_id);
    assert!(result.is_err(), "subsession_create must surface persist failure to caller");

    assert!(
        h.sub_ctx.store.list_for_worktree_tab(&h.worktree_tab_id).is_empty(),
        "in-memory store must be rolled back on persist failure"
    );
    // Pool entry rolled back too (otherwise the PTY child is leaked).
    let live_ids: Vec<_> = h
        .sub_ctx
        .store
        .list_for_worktree_tab(&h.worktree_tab_id)
        .into_iter()
        .filter(|s| h.sub_pool.contains(&s.id))
        .collect();
    assert!(live_ids.is_empty(), "no PTY child should remain in the pool after rollback");
}

/// CP-07 cascade orphan branch: when `pool.kill` returns `KillOutcome::Unconfirmed` the cascade must keep the sub-session row visible (in-memory +
/// persisted) and emit a status=Error event so the user can see and clean it up — never silently prune.
#[tokio::test]
async fn cascade_kill_failure_leaves_orphan_visible() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Flip the killer to fail. Cascade hits this branch on the next pool.kill call.
    h.sub_spawner_flags.kill_fails.store(true, Ordering::SeqCst);

    let _ = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Detach).await;

    // Orphan record kept in the in-memory store and on disk so the user can see the runaway PID.
    assert_eq!(
        h.sub_ctx.store.list_for_worktree_tab(&h.worktree_tab_id).len(),
        1,
        "in-memory store must keep the orphan visible"
    );
    let persisted = h.ctx.store().load_config().last_open_sub_sessions;
    assert_eq!(persisted.len(), 1, "persisted slot must keep the orphan visible");
    assert_eq!(persisted[0].id, sub.id);

    // Cascade emitted a status=Error event with the recorded PID so the frontend can surface the orphan to the user.
    let statuses = h.sub_events.statuses.lock().unwrap().clone();
    let error_evs: Vec<_> = statuses
        .iter()
        .filter(|(id, st, ..)| *id == sub.id && matches!(st, SubSessionStatus::Error))
        .collect();
    assert!(!error_evs.is_empty(), "cascade must emit at least one status=Error event for the orphan");
}

/// On restore-on-launch the second pass re-spawns terminal subs. If the spawner fails (e.g. ConPTY exhaustion), the persisted record must stay so the
/// user can retry — never silently dropped from disk. The sub-session row should also surface in the in-memory store with status=Error so the UI can
/// show the failure rather than a missing tab.
#[tokio::test]
async fn restore_spawn_failure_keeps_record() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Simulate fresh app launch: drop in-memory + pool, keep persistence.
    h.sub_ctx.store.remove(&sub.id);
    h.sub_pool.kill(&sub.id).await.ok();

    // Sanity: persistence still has the row.
    let persisted_before = h.ctx.store().load_config().last_open_sub_sessions;
    assert_eq!(persisted_before.len(), 1);
    assert_eq!(persisted_before[0].id, sub.id);

    // Force the next spawn to fail.
    h.sub_spawner_flags.fail_spawn.store(true, Ordering::SeqCst);

    restore_all_sub_sessions_impl(&h.ctx, &h.sub_ctx);

    // Persistence must NOT be pruned — the user can retry by re-running restore (or by relaunch from the UI). Silently dropping persisted rows on a
    // transient spawn failure would erase legitimate user work.
    let persisted_after = h.ctx.store().load_config().last_open_sub_sessions;
    assert_eq!(persisted_after.len(), 1, "persisted row must survive a restore-time spawn failure");
    assert_eq!(persisted_after[0].id, sub.id);

    // In-memory row was inserted *before* the spawn attempt and stays visible so the UI can render the failed tab. The pool has no entry because
    // spawn never succeeded.
    assert!(
        h.sub_ctx.store.get(&sub.id).is_some(),
        "in-memory sub row must remain visible after restore spawn failure"
    );
    assert!(!h.sub_pool.contains(&sub.id), "no pool entry should exist after restore spawn failure");

    // restored event fired (with status=Starting), then a status=Error event flips the row to the visible failure state. Both must fire in that order
    // so the frontend store has the row before the error arrives.
    let restored_evs = h.sub_events.restored.lock().unwrap().clone();
    assert_eq!(restored_evs.len(), 1, "expected one restored event");
    assert_eq!(restored_evs[0].id, sub.id);
    let statuses = h.sub_events.statuses.lock().unwrap().clone();
    let error_evs: Vec<_> = statuses
        .iter()
        .filter(|(id, st, ..)| *id == sub.id && matches!(st, SubSessionStatus::Error))
        .collect();
    assert!(!error_evs.is_empty(), "restore must emit status=Error after spawn failure");
}

/// `subsession_relaunch_impl` must refresh `composed_command` from the current def — this is the one place we
/// re-derive the command (everywhere else the compose-once invariant holds). User-facing impact: editing a Custom Process def's `command` field must
/// take effect on the next relaunch of any existing sub-session bound to that def.
#[tokio::test]
async fn relaunch_refreshes_composed_command_from_current_def() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Sanity: original composed_command matches the original def.
    assert_eq!(sub.composed_command, "sh -i");

    // User edits the def via Settings: command changes from "sh -i" to "bash -i". The persisted sub record still references the same def_id
    // ("shell").
    let edited_def = CustomProcessDef {
        id: h.shell_def_id.clone(),
        name: "Bash".into(),
        kind: CustomProcessKind::Terminal,
        command: "bash -i".into(),
        enabled: true,
        icon: None,
        icon_data_uri: None,
    };
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            custom_processes: Some(vec![edited_def]),
            ..Default::default()
        })
        .unwrap();

    let returned = subsession_relaunch_impl(&h.ctx, &h.sub_ctx, sub.id).await.expect("relaunch ok");

    // Returned snapshot reflects the refreshed command + label.
    assert_eq!(
        returned.composed_command, "bash -i",
        "relaunch must use the current def's command, not the persisted one"
    );
    assert_eq!(returned.label, "Bash", "relaunch must refresh the label too");

    // Persistence reflects the refreshed command.
    let persisted = h.ctx.store().load_config().last_open_sub_sessions;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].id, sub.id);
    assert_eq!(persisted[0].composed_command, "bash -i");
    assert_eq!(persisted[0].label, "Bash");
}

// --------------------------------------------------------------------------- subsession_close active_child_id cleanup + best-effort persistence
// (PR #65 review-7 + review-8) ---------------------------------------------------------------------------

/// Helper: insert a worktree tab with `active_child_id = child` directly into config (bypasses `worktree_tab_open_impl` to avoid a SubAppContext on
/// the test side and keep the test focused on the close-path invariant). Returns the synthesised tab id.
fn seed_tab_with_active_child(h: &Harness, path: &Path, child: ChildId) -> WorktreeTabId {
    let tab_id = WorktreeTabId::new();
    h.ctx
        .store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            cfg.worktree_tabs.push(WorktreeTab {
                id: tab_id,
                path: path.to_path_buf(),
                name: "wt".into(),
                branch: None,
                label: "wt".into(),
                tab_index: 0,
                active_child_id: Some(child),
                icon_id: 1,
            });
            cfg.worktree_tab_order.push(tab_id);
            true
        })
        .expect("seed worktree tab");
    tab_id
}

/// Normal-path regression for PR #65 review-7: closing a sub-session must clear any worktree tab's `active_child_id` that points at the closing
/// sub. Mirrors the equivalent test for top-level sessions in `tests/session_lifecycle_fake.rs`.
#[tokio::test]
async fn subsession_close_clears_worktree_tab_active_child_id_pointing_at_closed_subsession() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    let tab_id = seed_tab_with_active_child(&h, h.worktree.path(), ChildId::SubSession(sub.id));

    // Close via the public command path.
    subsession_close_impl(&h.ctx, Arc::clone(&h.sub_ctx), sub.id, SubSessionCloseIntent::TabOnly)
        .await
        .expect("close ok");

    let cfg = h.ctx.store().load_config();
    let tab = cfg
        .worktree_tabs
        .iter()
        .find(|t| t.id == tab_id)
        .expect("tab still present after sub close");
    assert_eq!(
        tab.active_child_id, None,
        "active_child_id pointing at closed sub-session must be cleared"
    );
    assert!(
        cfg.last_open_sub_sessions.iter().all(|r| r.id != sub.id),
        "persisted last_open_sub_sessions must drop the closed sub"
    );
}

/// PR #65 review-8 fix: if `save_config_with` fails after the kill + in-memory removal, `subsession_close_impl` must still return `Ok` because the
/// close has already happened (PTY killed, runtime gone, in-memory store cleared). Returning `Err` would surface "close failed" for an actually-
/// completed close AND make retry impossible (sub no longer exists in the store, so a retry would NotFound). The persistence anomaly is the
/// accepted trade-off and is documented in the swallow site's comment.
#[tokio::test]
async fn subsession_close_returns_ok_when_config_cleanup_fails_after_runtime_teardown() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Force every subsequent `write_atomic` to fail by replacing the config file with a directory of the same name. Same trick as
    // `create_rolls_back_inmemory_on_persist_failure` above (line ~518) — `tmp.persist()` cannot rename a NamedTempFile over a directory on either
    // OS, so the post-teardown `save_config_with` will trip Err inside the closure path.
    let cfg_path = h.config_dir.path().join("config.json");
    std::fs::remove_file(&cfg_path).ok();
    std::fs::create_dir(&cfg_path).expect("replace config.json with dir");

    // Close must still succeed — the kill + in-memory removal already happened, and the documented best-effort cleanup contract says we log+continue
    // rather than make the close non-retryable for the user.
    subsession_close_impl(&h.ctx, Arc::clone(&h.sub_ctx), sub.id, SubSessionCloseIntent::TabOnly)
        .await
        .expect("close must return Ok despite config cleanup failure");

    // Runtime teardown actually happened: in-memory store is empty for this sub, and the PTY child is gone from the pool.
    assert!(h.sub_ctx.store.get(&sub.id).is_none(), "in-memory store must drop the sub");
    assert!(!h.sub_pool.contains(&sub.id), "PTY pool must drop the sub");
}

// --------------------------------------------------------------------------- close_for_worktree_tab_impl conditional save (PR #65 review-9)
// ---------------------------------------------------------------------------

/// Roll back `path`'s mtime by 60s (and snapshot its bytes) so a subsequent rewrite is detectable across filesystems with coarse mtime resolution
/// (FAT 2s, HFS+ 1s). Mirrors the helper of the same name in `tests/worktree_tab_command.rs` — each Rust integration test file is its own crate, so
/// duplication here is the standard pattern.
fn snapshot_with_rolled_back_mtime(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
    let bytes = std::fs::read(path).expect("read config.json snapshot");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open config.json for set_modified (write(true) does not truncate)");
    f.set_modified(old)
        .expect("set_modified must succeed (Rust 1.75+); fail loudly on platforms without timestamp support");
    drop(f);
    let mtime = std::fs::metadata(path).expect("stat").modified().expect("mtime");
    (bytes, mtime)
}

fn snapshot_file(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
    let bytes = std::fs::read(path).expect("read config.json snapshot");
    let mtime = std::fs::metadata(path).expect("stat config.json").modified().expect("mtime");
    (bytes, mtime)
}

/// PR #65 review-9: `close_for_worktree_tab_impl` must skip its `save_config_with` cleanup when no worktree tab's `active_child_id` references any
/// sub in the cascade. Without the conditional pre-check, every tab close with subs rewrites `config.json` — pure disk churn for the common case
/// where the user wasn't focused on a sub when closing.
///
/// To isolate the pre-pass write from the per-iteration `remove_last_open_sub_session` writes that prune `last_open_sub_sessions`, we manually
/// pre-prune that record before snapshotting — making the per-iteration prune a no-op (the helper exits early when the id isn't found, see
/// `ConfigStore::remove_last_open_sub_session`). With both writes silent, any mtime advance proves the pre-pass write fired unnecessarily.
#[tokio::test]
async fn cascade_with_no_matching_active_child_skips_config_write() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    // Seed a tab with `active_child_id = None` — explicitly NOT pointing at the cascade sub.
    let tab_id = WorktreeTabId::new();
    h.ctx
        .store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            cfg.worktree_tabs.push(WorktreeTab {
                id: tab_id,
                path: h.worktree.path().to_path_buf(),
                name: "wt".into(),
                branch: None,
                label: "wt".into(),
                tab_index: 0,
                active_child_id: None,
                icon_id: 1,
            });
            cfg.worktree_tab_order.push(tab_id);
            true
        })
        .expect("seed tab");

    // Pre-prune the sub record so the per-iteration `remove_last_open_sub_session` call below is a no-op (helper exits before `write_atomic` when the
    // id is absent). This isolates the mtime check to the pre-pass write that we're testing.
    h.ctx.store().remove_last_open_sub_session(&sub.id).expect("pre-prune");

    let cfg_path = h.config_dir.path().join("config.json");
    let (bytes_before, mtime_before) = snapshot_with_rolled_back_mtime(&cfg_path);

    let _ = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Detach).await;

    let (bytes_after, mtime_after) = snapshot_file(&cfg_path);
    assert_eq!(
        mtime_before, mtime_after,
        "cascade pre-pass must NOT rewrite config.json when no tab's active_child_id matches any cascade sub"
    );
    assert_eq!(bytes_before, bytes_after, "config.json bytes must be unchanged");
}

/// Conjugate of the previous test: when a tab DOES point at a cascade sub, the cleanup must run and the file must be rewritten with the pointer
/// cleared. Confirms the conditional pre-check doesn't accidentally suppress the legitimate cleanup write.
#[tokio::test]
async fn cascade_with_matching_active_child_rewrites_config_and_clears_pointer() {
    let h = build_harness();
    let _parent = create_parent(&h).id;
    let sub = create_sub(&h, h.worktree_tab_id).expect("sub created");
    assert!(wait_until(|| h.sub_pool.contains(&sub.id), Duration::from_secs(2)));

    let tab_id = seed_tab_with_active_child(&h, h.worktree.path(), ChildId::SubSession(sub.id));

    let cfg_path = h.config_dir.path().join("config.json");
    let (_, mtime_before) = snapshot_with_rolled_back_mtime(&cfg_path);

    let _ = close_for_worktree_tab_impl(&h.ctx, &h.sub_ctx, h.worktree_tab_id, WorktreeTabAppClosePolicy::Detach).await;

    let (_, mtime_after) = snapshot_file(&cfg_path);
    assert!(
        mtime_after > mtime_before,
        "cascade pre-pass MUST rewrite config.json when a tab's active_child_id matches a cascade sub (mtime advanced)"
    );

    let cfg = h.ctx.store().load_config();
    let tab = cfg
        .worktree_tabs
        .iter()
        .find(|t| t.id == tab_id)
        .expect("tab still present after cascade");
    assert_eq!(
        tab.active_child_id, None,
        "active_child_id pointing at a cascade sub must be cleared by the pre-pass"
    );
}
