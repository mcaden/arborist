//! Phase 7 session-lifecycle integration tests using a deterministic fake PTY spawner. These tests drive the same `*_impl` business-logic functions
//! the production Tauri command wrappers call, so they cover the full session-create → spawn → input → resize → close path without depending on a
//! real Claude/Copilot install.
//!
//! ## Why duplicate the FakeSpawner?
//!
//! Rust integration tests (`tests/*.rs`) are compiled as separate crates, so each test file has to bring its own helpers. We could promote the one in
//! `tests/pty_pool.rs` to a `pub(crate)` test-support module, but that drags `portable-pty` into the public surface for the sake of test ergonomics.
//! Copying the small helper here keeps the production crate's public surface honest.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arborist_lib::commands::session::{
    frontend_ready_impl, restore_all_sessions, session_close_impl, session_close_locked, session_create_impl, session_focus_impl, session_input_impl,
    session_list_impl, session_resize_impl, session_restart_impl, AppContext,
};
use arborist_lib::commands::worktree_tab::worktree_tab_open_impl;
use arborist_lib::compose::session_temp_dir;
use arborist_lib::config_store::ConfigStore;
use arborist_lib::git::GitRunner;
use arborist_lib::pty_pool::{ChildCommand, PtyKiller, PtyPool, PtyResize, PtySink, PtySpawner, PtyWaiter, SpawnedChild};
use arborist_lib::types::{
    ChildId, PartialAppConfig, SessionCreateArgs, SessionId, SessionInputArgs, SessionResizeArgs, SessionRestartArgs, SessionStatus, Tool,
    WorktreeInfo, WorktreeTabOpenArgs,
};
use portable_pty::{ExitStatus, PtySize};
use tempfile::TempDir;

// --------------------------------------------------------------------------- Deterministic fake spawner (cf. tests/pty_pool.rs::FakeSpawner)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SpawnerState {
    spawn_count: usize,
    last_cwd: Option<PathBuf>,
    last_cmd: Option<ChildCommand>,
    /// PtySize handed to the most recent `spawn` call. Used by regression tests that pin the deferred-spawn path: `restore_all_sessions` no longer
    /// spawns directly; the first `session_resize` from the frontend triggers the spawn and *that* size — not `DEFAULT_PTY_SIZE` — must reach the
    /// spawner so the CLI's first paint matches the real terminal width.
    last_size: Option<PtySize>,
    /// One entry per spawn so a test can keep killing/respawning.
    eofs: Vec<Arc<AtomicBool>>,
    next_pid: u32,
    /// If set, the next call to `spawn` returns this error and clears the flag (one-shot). Used to drive failed-restart regression tests that need to
    /// assert on persisted state after the failure.
    fail_next_with: Option<arborist_lib::types::Error>,
}

struct FakeSpawner {
    state: Mutex<SpawnerState>,
    kill_fails: Arc<AtomicBool>,
}

impl FakeSpawner {
    fn new() -> Self {
        Self {
            state: Mutex::new(SpawnerState {
                next_pid: 9000,
                ..SpawnerState::default()
            }),
            kill_fails: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set_kill_fails(&self, value: bool) {
        self.kill_fails.store(value, Ordering::SeqCst);
    }
}

impl PtySpawner for FakeSpawner {
    fn spawn(&self, cmd: ChildCommand, cwd: &Path, size: PtySize) -> Result<SpawnedChild, arborist_lib::types::Error> {
        let mut s = self.state.lock().unwrap();
        if let Some(err) = s.fail_next_with.take() {
            // Capture the inputs of the failed attempt too — assertions about *which* command was attempted still need to work.
            s.spawn_count += 1;
            s.last_cwd = Some(cwd.to_path_buf());
            s.last_cmd = Some(cmd);
            return Err(err);
        }
        s.spawn_count += 1;
        s.last_cwd = Some(cwd.to_path_buf());
        s.last_cmd = Some(cmd);
        s.last_size = Some(size);
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
                fail: Arc::clone(&self.kill_fails),
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

#[derive(Default)]
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
    fn resize(&self, _cols: u16, _rows: u16) -> Result<(), arborist_lib::types::Error> {
        Ok(())
    }
}

struct EofKiller {
    eof: Arc<AtomicBool>,
    fail: Arc<AtomicBool>,
}
impl PtyKiller for EofKiller {
    fn kill(&self) -> Result<(), arborist_lib::types::Error> {
        self.eof.store(true, Ordering::Relaxed);
        if self.fail.load(Ordering::SeqCst) {
            return Err(arborist_lib::types::Error::PtyKillFailed("injected kill failure".into()));
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

// --------------------------------------------------------------------------- Sink that captures emissions for assertion
// ---------------------------------------------------------------------------

type StatusTuple = (SessionId, SessionStatus, Option<u32>, Option<String>);

#[derive(Default)]
struct CapturedEvents {
    output: Mutex<Vec<(SessionId, String)>>,
    status: Mutex<Vec<StatusTuple>>,
}

fn capture_sink(events: Arc<CapturedEvents>, store: ConfigStore) -> PtySink {
    let out_events = Arc::clone(&events);
    let output = Arc::new(move |id: &SessionId, data: String| {
        out_events.output.lock().unwrap().push((*id, data));
    });
    let status_events = Arc::clone(&events);
    let status = Arc::new(move |id: &SessionId, st: SessionStatus, pid: Option<u32>, msg: Option<String>| {
        // Mirror production wiring: persist status, swallow NotFound.
        if let Err(e) = store.update_session_status(id, st, pid) {
            use arborist_lib::types::Error as E;
            if !matches!(e, E::NotFound(_)) {
                panic!("unexpected status persist error: {e:?}");
            }
        }
        status_events.status.lock().unwrap().push((*id, st, pid, msg));
    });
    PtySink::new(output, status, Arc::new(|_id, _evt| {}))
}

// --------------------------------------------------------------------------- Test harness builder
// ---------------------------------------------------------------------------

struct Harness {
    ctx: Arc<AppContext>,
    spawner: Arc<FakeSpawner>,
    events: Arc<CapturedEvents>,
    _config_dir: TempDir,
    worktree: TempDir,
}

/// Records `remove_worktree` invocations so a test can assert opt-in deletion was forwarded to the git layer.
#[derive(Default)]
struct RecordingGitRunner {
    removes: Mutex<Vec<(PathBuf, PathBuf)>>,
    fail_with: Mutex<Option<String>>,
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
    fn git_toplevel(&self, p: &Path) -> Result<Option<PathBuf>, arborist_lib::types::Error> {
        Ok(Some(p.to_path_buf()))
    }
    fn create_worktree(&self, repo_root: &Path, relative_path: &Path, _branch: &str) -> Result<PathBuf, arborist_lib::types::Error> {
        Ok(repo_root.join(relative_path))
    }
    fn remove_worktree(&self, repo_root: &Path, worktree_path: &Path) -> Result<(), arborist_lib::types::Error> {
        self.removes.lock().unwrap().push((repo_root.to_path_buf(), worktree_path.to_path_buf()));
        if let Some(msg) = self.fail_with.lock().unwrap().clone() {
            return Err(arborist_lib::types::Error::Internal(msg));
        }
        Ok(())
    }
    fn git_status(&self, _worktree_path: &Path) -> Result<arborist_lib::types::WorktreeGitStatus, arborist_lib::types::Error> {
        Ok(arborist_lib::types::WorktreeGitStatus::default())
    }
}

fn build_harness() -> Harness {
    build_harness_with_git(Arc::new(arborist_lib::git::RealGitRunner) as Arc<dyn GitRunner>)
}

fn build_harness_with_git(git: Arc<dyn GitRunner>) -> Harness {
    let config_dir = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();

    let store = ConfigStore::open(config_dir.path()).unwrap();

    let spawner = Arc::new(FakeSpawner::new());
    let pool = Arc::new(PtyPool::new(spawner.clone() as Arc<dyn PtySpawner>));
    let events = Arc::new(CapturedEvents::default());
    let sink = capture_sink(Arc::clone(&events), store.clone());
    let ctx = Arc::new(AppContext::new(
        pool,
        store,
        sink,
        git,
        Arc::new(|_| {}),
        Arc::new(|_, _| {}),
        Arc::new(|_, _| {}),
    ));

    Harness {
        ctx,
        spawner,
        events,
        _config_dir: config_dir,
        worktree,
    }
}

fn create_args(h: &Harness) -> SessionCreateArgs {
    SessionCreateArgs {
        tool: Tool::Claude,
        worktree_path: h.worktree.path().to_path_buf(),
        cols: 80,
        rows: 24,
    }
}

/// Wait until predicate holds or `dur` elapses. Avoids fixed `sleep`s.
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
async fn create_emits_starting_then_running_and_persists_session() {
    let h = build_harness();

    let view = session_create_impl(&h.ctx, create_args(&h)).expect("create ok");
    assert_eq!(view.status, SessionStatus::Running);
    assert!(view.pid.is_some());

    // Status sequence: Starting (from impl) then Running (from pool).
    let statuses = h.events.status.lock().unwrap().clone();
    assert!(statuses.len() >= 2, "expected ≥2 status events, got {statuses:?}");
    assert_eq!(statuses[0].1, SessionStatus::Starting);
    assert_eq!(statuses[0].2, None);
    assert_eq!(statuses[1].1, SessionStatus::Running);
    assert_eq!(statuses[1].2, view.pid);

    // The persisted session record should reflect Running + pid.
    let listed = session_list_impl(&h.ctx).expect("list ok");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, view.id);
    assert_eq!(listed[0].status, SessionStatus::Running);
    assert_eq!(listed[0].pid, view.pid);

    // AppConfig active selection should point at the new session.
    let cfg = h.ctx.store().load_config();
    assert_eq!(cfg.active_session_id, Some(view.id));
    assert_eq!(cfg.tab_order, vec![view.id]);
    assert_eq!(cfg.last_open_sessions, vec![view.id]);

    // Spawn was called with the discrete cwd, never interpolated.
    let st = h.spawner.state.lock().unwrap();
    let cwd = st.last_cwd.as_ref().unwrap();
    let cwd_canon = dunce::canonicalize(cwd).unwrap();
    let wt_canon = dunce::canonicalize(h.worktree.path()).unwrap();
    assert_eq!(cwd_canon, wt_canon, "fake spawner cwd should canonicalize to the worktree");
    let cmd = st.last_cmd.as_ref().unwrap();
    assert!(
        !cmd.args.iter().any(|a| a.contains("cd ")),
        "composed command must not contain `cd <path>` interpolation"
    );
}

#[tokio::test]
async fn create_passes_initial_size_to_spawner() {
    // Regression: pre-fix, the PTY was always opened at DEFAULT_PTY_SIZE (80×24) regardless of what the frontend actually rendered, so the CLI's
    // first paint (e.g. a Copilot/Claude splash) was always at 80 cols. The frontend now measures its host first and passes the real dims through
    // SessionCreateArgs; this test pins that they actually reach `PtySpawner::spawn`.
    let h = build_harness();
    let args = SessionCreateArgs {
        tool: Tool::Claude,
        worktree_path: h.worktree.path().to_path_buf(),
        cols: 173,
        rows: 47,
    };
    session_create_impl(&h.ctx, args).expect("create ok");
    let st = h.spawner.state.lock().unwrap();
    let size = st.last_size.expect("spawner should have recorded a size");
    assert_eq!(size.cols, 173);
    assert_eq!(size.rows, 47);
}

#[tokio::test]
async fn restart_passes_dims_to_respawn() {
    // Companion to `create_passes_initial_size_to_spawner` — restart goes through the same race and is fixed the same way (frontend reads
    // term.cols/rows and passes them in `SessionRestartArgs`).
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    session_restart_impl(
        &h.ctx,
        SessionRestartArgs {
            session_id: view.id,
            cols: 200,
            rows: 60,
        },
    )
    .expect("restart ok");
    let st = h.spawner.state.lock().unwrap();
    let size = st.last_size.expect("respawn should have recorded a size");
    assert_eq!(size.cols, 200);
    assert_eq!(size.rows, 60);
}

#[tokio::test]
async fn create_rejects_zero_dimensions() {
    // PR #28 review: raw u16 lets `0` slip through, which then fails deep inside `portable_pty::openpty` with an opaque OS error. The command
    // boundary now rejects it with a stable, branchable code so the frontend can surface a real diagnostic (or a future refactor that bypasses
    // `measureInitialPtyDimensions` is caught at the boundary instead of as a cryptic "PTY spawn failed").
    let h = build_harness();
    for (cols, rows) in [(0u16, 24u16), (80, 0), (0, 0)] {
        let args = SessionCreateArgs {
            tool: Tool::Claude,
            worktree_path: h.worktree.path().to_path_buf(),
            cols,
            rows,
        };
        let err = session_create_impl(&h.ctx, args).expect_err(&format!("create({cols}x{rows}) should have failed"));
        assert_eq!(err.code, "InvalidArgs", "unexpected code for {cols}x{rows}");
        assert!(err.message.contains("pty dimensions"), "unexpected message: {}", err.message);
    }
    // Sanity: the spawner must NOT have been touched.
    let st = h.spawner.state.lock().unwrap();
    assert!(st.last_size.is_none(), "spawner.spawn should not run when dims are 0");
}

#[tokio::test]
async fn restart_rejects_zero_dimensions() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let err = session_restart_impl(
        &h.ctx,
        SessionRestartArgs {
            session_id: view.id,
            cols: 0,
            rows: 24,
        },
    )
    .expect_err("restart with cols=0 should fail");
    assert_eq!(err.code, "InvalidArgs");
}

#[tokio::test]
async fn resize_rejects_zero_dimensions() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let err = session_resize_impl(
        &h.ctx,
        SessionResizeArgs {
            session_id: view.id,
            cols: 0,
            rows: 0,
        },
    )
    .expect_err("resize with 0×0 should fail");
    assert_eq!(err.code, "InvalidArgs");
}

#[tokio::test]
async fn input_writes_through_to_pty() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    // Write should not error.
    session_input_impl(
        &h.ctx,
        SessionInputArgs {
            session_id: view.id,
            data: "ls\r".into(),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn resize_routes_to_pool() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    session_resize_impl(
        &h.ctx,
        SessionResizeArgs {
            session_id: view.id,
            cols: 132,
            rows: 50,
        },
    )
    .unwrap();
}

#[tokio::test]
async fn focus_updates_active_session_id_and_rejects_unknown() {
    let h = build_harness();
    let v1 = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let v2 = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    assert_ne!(v1.id, v2.id);

    // After creating two, the second is the active one (most recent create).
    assert_eq!(h.ctx.store().load_config().active_session_id, Some(v2.id));

    session_focus_impl(&h.ctx, v1.id).unwrap();
    assert_eq!(h.ctx.store().load_config().active_session_id, Some(v1.id));

    let unknown = SessionId::new();
    let err = session_focus_impl(&h.ctx, unknown).expect_err("should fail");
    assert_eq!(err.code, "NotFound");
}

/// Regression for Phase 9 review Issue 3: `session_focus_impl` must refuse while a workspace switch is in progress. Without this gate, a stale
/// tab-click from the frontend could write `active_session_id` for a not-yet-torn-down old-workspace session into a snapshot of the *old* store that
/// races the swap.
#[tokio::test]
async fn focus_refuses_while_workspace_switch_in_progress() {
    let h = build_harness();
    let v = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    {
        // Simulate a queued/active workspace switch by holding the write guard on the unified switch barrier. Lifecycle handlers' `try_read()` then
        // fails with TryLockError → `WorkspaceSwitchInProgress`.
        let _w = h.ctx.switch_lock.try_write().expect("switch_lock should be free in test");
        let err = session_focus_impl(&h.ctx, v.id).expect_err("must refuse mid-switch");
        assert_eq!(err.code, "WorkspaceSwitchInProgress");
    }
    session_focus_impl(&h.ctx, v.id).expect("succeeds once gate clears");
}

/// Companion to `focus_refuses_…`. Each gated lifecycle handler must return `WorkspaceSwitchInProgress` while the unified switch barrier is
/// write-held, then succeed once it drops. Covers the active-writer arm of the gate; the queued-writer arm is exercised separately below.
#[tokio::test]
async fn lifecycle_handlers_refuse_while_switch_write_held() {
    let h = build_harness();
    let v = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    {
        let _w = h.ctx.switch_lock.try_write().expect("switch_lock should be free in test");

        // session_create
        let err = session_create_impl(&h.ctx, create_args(&h)).expect_err("create must refuse");
        assert_eq!(err.code, "WorkspaceSwitchInProgress");

        // session_close (async; the impl takes the read guard internally)
        let err = session_close_impl(&h.ctx, v.id, false).await.expect_err("close must refuse");
        assert_eq!(err.code, "WorkspaceSwitchInProgress");

        // session_restart
        let err = session_restart_impl(
            &h.ctx,
            SessionRestartArgs {
                session_id: v.id,
                cols: 80,
                rows: 24,
            },
        )
        .expect_err("restart must refuse");
        assert_eq!(err.code, "WorkspaceSwitchInProgress");
    }

    // Once the switch guard drops, normal operation resumes.
    session_restart_impl(
        &h.ctx,
        SessionRestartArgs {
            session_id: v.id,
            cols: 80,
            rows: 24,
        },
    )
    .expect("restart succeeds once gate clears");
}

/// `session_resize_impl` is the one gated handler that does **not** surface `WorkspaceSwitchInProgress` to the UI — it returns `Ok(())` silently and
/// lets the next `ResizeObserver` event correct dimensions after the switch completes. Without this contract a flurry of resizes during a switch
/// would spam error toasts (see docs/runtime-flows.md#workspace-switching).
#[tokio::test]
async fn resize_silently_skips_while_switch_write_held() {
    let h = build_harness();
    let v = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let _w = h.ctx.switch_lock.try_write().expect("switch_lock should be free in test");

    let res = session_resize_impl(
        &h.ctx,
        SessionResizeArgs {
            session_id: v.id,
            cols: 100,
            rows: 30,
        },
    );
    assert!(res.is_ok(), "resize during switch must return Ok(()) silently, got {res:?}",);
}

/// Regression for the rubber-duck's "queued-writer" finding. The rejection contract here is the **`switch_pending` counter**, not tokio `RwLock`
/// fairness alone: a queued writer does NOT bump out new `try_read()` calls (the lock is permit-based and `try_read` consults only the current permit
/// count, not the wait queue), so the switch increments `switch_pending` *before* awaiting the write lock, and handlers detect the queued switch by
/// loading the counter after taking their read guard (see `acquire_switch_read`). This test simulates that exact prologue: hold a read guard, spawn a
/// task that bumps `switch_pending` and queues for write, then assert that gated handlers reject (and resize is silent-Ok).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_handlers_refuse_when_switch_writer_is_queued() {
    let h = build_harness();
    let v = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    // Take a read guard so writers must queue.
    let read_guard = h.ctx.switch_lock.try_read().expect("read guard available initially");

    // Spawn a task that mimics the *prologue* of `workspace_switch_impl_inner`: bump `switch_pending` BEFORE awaiting the write lock, decrement on
    // drop. The task signals on `bumped_tx` immediately after the increment so the test can proceed deterministically — no sleeps, no fixed yield
    // counts.
    let lock_for_writer = Arc::clone(&h.ctx.switch_lock);
    let pending_for_writer = Arc::clone(&h.ctx.switch_pending);
    let (bumped_tx, bumped_rx) = tokio::sync::oneshot::channel::<()>();
    let (writer_done_tx, writer_done_rx) = tokio::sync::oneshot::channel::<()>();
    let writer_task = tokio::spawn(async move {
        pending_for_writer.fetch_add(1, Ordering::SeqCst);
        struct Decr(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Decr {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _decr = Decr(pending_for_writer);
        // Signal AFTER incrementing the counter but BEFORE awaiting the write lock. The send is synchronous, so by the time the test's
        // `bumped_rx.await` resolves the counter is bumped and this task immediately suspends on `write().await` — the exact state the
        // take-then-check contract is designed for.
        let _ = bumped_tx.send(());
        let _w = lock_for_writer.write().await;
        let _ = writer_done_tx.send(());
    });

    // Deterministic synchronisation point: writer task has bumped the counter and is now queued for write.
    bumped_rx.await.expect("writer task signals after bumping switch_pending");
    assert_eq!(
        h.ctx.switch_pending.load(Ordering::SeqCst),
        1,
        "writer task must have bumped switch_pending before we proceed",
    );

    // With a queued writer (and a non-zero `switch_pending`), lifecycle handlers must reject.
    let err = session_focus_impl(&h.ctx, v.id).expect_err("queued writer must block focus");
    assert_eq!(err.code, "WorkspaceSwitchInProgress");

    let err = session_create_impl(&h.ctx, create_args(&h)).expect_err("queued writer blocks create");
    assert_eq!(err.code, "WorkspaceSwitchInProgress");

    // …and resize is silently `Ok`.
    let res = session_resize_impl(
        &h.ctx,
        SessionResizeArgs {
            session_id: v.id,
            cols: 90,
            rows: 25,
        },
    );
    assert!(res.is_ok(), "resize while writer queued must return Ok(()) silently, got {res:?}",);

    // Release the read guard and let the queued writer drain.
    drop(read_guard);
    tokio::time::timeout(Duration::from_secs(2), writer_done_rx)
        .await
        .expect("writer must acquire and release after readers drain")
        .expect("writer task must signal completion");
    writer_task.await.expect("writer task must complete");

    // `switch_pending` is back to zero (the writer's `Decr` guard dropped). After the switch's writer finishes, lifecycle handlers succeed again.
    assert_eq!(h.ctx.switch_pending.load(Ordering::SeqCst), 0);
    session_focus_impl(&h.ctx, v.id).expect("focus succeeds once writer drains");
}

/// Regression for PR #65 sixth-review-round thread on `worktree_tab_close_impl`'s child-cascade self-rejection.
///
/// The bug: cascading close paths (`session_close` command wrapper in `commands/mod.rs`, and `worktree_tab_close_impl`) acquire a switch
/// read-guard for the full cascade, then call `session_close_impl` per child — which **also** calls `acquire_switch_read` internally. Because
/// `acquire_switch_read` rejects whenever `switch_pending > 0` (regardless of whether the same task already holds a read guard), a workspace
/// switch queued mid-cascade caused the inner per-child close to fail with `WorkspaceSwitchInProgress` while the outer code proceeded to remove
/// the parent record — orphaning the child sessions in the store.
///
/// The fix: extract the body of `session_close_impl` into `session_close_locked`, which assumes the caller has already passed `acquire_switch_read`.
/// Cascading callers now invoke `session_close_locked` directly so the inner close cannot self-reject.
///
/// This test pins the precondition: with `switch_pending == 1` (a switch queued just after the outer guard was acquired), `session_close_locked`
/// must succeed. The companion `session_close_impl` call below must still reject — proving the gated wrapper retains its standalone barrier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_close_locked_does_not_self_reject_when_switch_pending_is_set() {
    let h = build_harness();
    let v_locked = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let v_impl = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    // Simulate the cascade prologue: outer caller already holds a read guard, then a workspace switch bumps `switch_pending` while preparing to
    // queue for write. The outer guard is intentionally NOT held by *this* test task (we are exercising the inner helper directly), but the
    // counter-bump is what matters — `acquire_switch_read` checks the counter, not the guard's task ownership.
    h.ctx.switch_pending.fetch_add(1, Ordering::SeqCst);
    struct Decr<'a>(&'a std::sync::atomic::AtomicUsize);
    impl<'a> Drop for Decr<'a> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _decr = Decr(&h.ctx.switch_pending);

    // The locked helper must complete despite the queued switch. Without the fix, the cascade's per-child close would reject here.
    session_close_locked(&h.ctx, v_locked.id, false)
        .await
        .expect("session_close_locked must NOT consult switch_pending; the cascade caller already passed the barrier");

    // The gated wrapper, in contrast, MUST still reject — its job is to reject when the counter is non-zero so standalone callers without an outer
    // guard cannot enter the close path during a switch.
    let err = session_close_impl(&h.ctx, v_impl.id, false)
        .await
        .expect_err("session_close_impl must reject when switch_pending is set; only the locked variant skips the check");
    assert_eq!(err.code, "WorkspaceSwitchInProgress");
}

/// Regression for PR #65 seventh-review-round thread on `session_close_locked` leaving a dangling `WorktreeTab.active_child_id`.
///
/// The bug: a worktree tab can persist a `ChildId::Session(sid)` pointer (set by migration or by `worktree_tab_set_active_child`). When that session
/// is later closed via `session_close_impl` (or its locked inner), the tab's pointer was NOT cleared, so the persisted config could reference a
/// non-existent session — surfacing as broken restore/focus on the next launch.
///
/// The fix: `session_close_locked`'s atomic config mutation now also clears any tab whose `active_child_id == Some(ChildId::Session(id))`.
#[tokio::test]
async fn session_close_clears_worktree_tab_active_child_id_pointing_at_closed_session() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).expect("session create ok");

    // Open a worktree tab for the same path the session is bound to (worktree_tab_open is idempotent on canonical path), then directly seed its
    // `active_child_id` to point at the session. Bypassing `worktree_tab_set_active_child_impl` keeps this test independent of `SubAppContext`
    // wiring — what matters for the regression is that close clears any pre-existing pointer, regardless of how it got there (real callers can
    // seed it via the public command, the v4→v5 migration in `config_store.rs::migrate_v4_to_v5`, or any future code path).
    let tab = worktree_tab_open_impl(
        &h.ctx,
        WorktreeTabOpenArgs {
            path: h.worktree.path().to_string_lossy().into_owned(),
        },
    )
    .expect("worktree_tab_open must succeed for the session's worktree path");
    let tab_id = tab.id;
    h.ctx
        .store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            let t = cfg.worktree_tabs.iter_mut().find(|t| t.id == tab_id).expect("tab present after open");
            t.active_child_id = Some(ChildId::Session(view.id));
            true
        })
        .expect("seed active_child_id");

    // Sanity: the seed actually landed.
    let cfg_pre = h.ctx.store().load_config();
    let tab_pre = cfg_pre.worktree_tabs.iter().find(|t| t.id == tab_id).expect("tab persisted");
    assert_eq!(tab_pre.active_child_id, Some(ChildId::Session(view.id)), "seed precondition");

    // Close the session via the gated wrapper (the production path the frontend uses).
    session_close_impl(&h.ctx, view.id, false).await.expect("close ok");

    // Post-condition: the tab survives (this isn't a worktree-tab close), but its dangling pointer is cleared.
    let cfg_post = h.ctx.store().load_config();
    let tab_post = cfg_post
        .worktree_tabs
        .iter()
        .find(|t| t.id == tab_id)
        .expect("tab must survive standalone session close");
    assert_eq!(
        tab_post.active_child_id, None,
        "session close MUST clear any worktree-tab `active_child_id` pointing at the closed session — see PR #65 review-7"
    );
    // Companion: a non-matching tab pointer would have been left alone (covered by helper unit test); here we just pin the cleanup wiring works at
    // the integration level.
}

/// Companion to the above: when a worktree-tab's `active_child_id` points at a DIFFERENT session, closing the unrelated session must NOT clear the
/// pointer. Pins that the cleanup is targeted (only matching pointers cleared), not a blanket reset of every tab.
#[tokio::test]
async fn session_close_does_not_clear_unrelated_worktree_tab_active_child_id() {
    let h = build_harness();
    let view_keep = session_create_impl(&h.ctx, create_args(&h)).expect("first session ok");
    let view_close = session_create_impl(&h.ctx, create_args(&h)).expect("second session ok");

    let tab = worktree_tab_open_impl(
        &h.ctx,
        WorktreeTabOpenArgs {
            path: h.worktree.path().to_string_lossy().into_owned(),
        },
    )
    .expect("worktree_tab_open ok");
    let tab_id = tab.id;
    h.ctx
        .store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            let t = cfg.worktree_tabs.iter_mut().find(|t| t.id == tab_id).expect("tab present");
            t.active_child_id = Some(ChildId::Session(view_keep.id));
            true
        })
        .expect("seed");

    session_close_impl(&h.ctx, view_close.id, false).await.expect("close unrelated ok");

    let cfg_post = h.ctx.store().load_config();
    let tab_post = cfg_post.worktree_tabs.iter().find(|t| t.id == tab_id).expect("tab survives");
    assert_eq!(
        tab_post.active_child_id,
        Some(ChildId::Session(view_keep.id)),
        "closing an unrelated session must not touch the tab's pointer to a still-live session"
    );
}

#[tokio::test]
async fn close_kills_pty_removes_record_and_clears_active() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let temp = session_temp_dir(&view.id);
    assert!(!temp.exists(), "new Claude sessions must not create prompt temp dirs");
    std::fs::create_dir_all(&temp).expect("create legacy temp dir");
    std::fs::write(temp.join("legacy-system-prompt.md"), b"legacy prompt").expect("write legacy temp file");

    session_close_impl(&h.ctx, view.id, false).await.unwrap();

    assert!(!h.ctx.pool.contains(&view.id));
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
    let cfg = h.ctx.store().load_config();
    assert_eq!(cfg.active_session_id, None);
    assert!(cfg.tab_order.is_empty());
    assert!(cfg.last_open_sessions.is_empty());

    // Legacy temp dirs are swept by either pool.kill or the post-close belt-and-braces cleanup. Allow a beat for filesystem to settle on Windows.
    let cleared = wait_until(|| !temp.exists(), Duration::from_secs(2));
    assert!(cleared, "session temp dir {temp:?} should be removed");
}

#[tokio::test]
async fn close_with_delete_worktree_invokes_git_runner_remove() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    // Configure a workspace root that *contains* the session's worktree so the containment check passes.
    let ws_root = h.worktree.path().parent().unwrap().to_path_buf();
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(ws_root.clone())),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let worktree_path = h.worktree.path().to_path_buf();

    session_close_impl(&h.ctx, view.id, true).await.unwrap();

    let removes = git.removes.lock().unwrap().clone();
    assert_eq!(removes.len(), 1, "expected one remove_worktree call");
    let (_repo, wt) = &removes[0];
    let wt_canon = dunce::canonicalize(wt).unwrap_or_else(|_| wt.clone());
    let expected_canon = dunce::canonicalize(&worktree_path).unwrap_or_else(|_| worktree_path.clone());
    assert_eq!(
        wt_canon, expected_canon,
        "remove_worktree must be called with the session's worktree path"
    );
    // Session record is gone regardless.
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
}

#[tokio::test]
async fn close_with_delete_worktree_refuses_git_remove_when_kill_unconfirmed() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    let ws_root = h.worktree.path().parent().unwrap().to_path_buf();
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(ws_root)),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    h.spawner.set_kill_fails(true);

    let result = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect("close itself should succeed so the UI can converge");

    let teardown = result.teardown_error.expect("unconfirmed kill should be reported");
    assert!(teardown.contains("unconfirmed"), "unexpected teardown error: {teardown}");
    let delete_error = result.worktree_delete_error.expect("delete should be refused when kill is unconfirmed");
    assert!(
        delete_error.contains("refusing to delete worktree") && delete_error.contains("unconfirmed"),
        "unexpected delete error: {delete_error}",
    );
    assert!(
        git.removes.lock().unwrap().is_empty(),
        "git worktree remove must not run when the PTY may still be alive in that worktree"
    );
    assert!(
        session_list_impl(&h.ctx).unwrap().is_empty(),
        "session record should still be removed for close convergence"
    );
}

#[tokio::test]
async fn close_without_delete_worktree_does_not_invoke_remove() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    session_close_impl(&h.ctx, view.id, false).await.unwrap();

    assert!(
        git.removes.lock().unwrap().is_empty(),
        "remove_worktree must NOT be called when delete_worktree is false"
    );
}

#[tokio::test]
async fn close_with_delete_worktree_refuses_main_workspace_root() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    // Point workspace_root at the same path as the session's worktree so the safety check trips.
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(h.worktree.path().to_path_buf())),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let result = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect("close itself should succeed even when worktree deletion is refused");
    let msg = result.worktree_delete_error.expect("expected a worktree-delete-error in the result");
    assert!(
        msg.contains("workspace root") || msg.contains("main worktree"),
        "expected a workspace-root refusal message, got: {msg}",
    );
    assert!(git.removes.lock().unwrap().is_empty(), "remove_worktree must not be invoked when refused");
    // The session is still removed because the kill+config cleanup happens before the worktree-deletion attempt — the user opted in to losing the
    // session even if the worktree is preserved.
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
}

#[tokio::test]
async fn close_with_delete_worktree_propagates_git_failure() {
    let git = RecordingGitRunner::new();
    *git.fail_with.lock().unwrap() = Some("fatal: not a working tree".into());
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    let ws_root = h.worktree.path().parent().unwrap().to_path_buf();
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(ws_root)),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let result = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect("close itself should succeed even when git fails");
    let msg = result.worktree_delete_error.expect("git failure must surface as a worktree-delete-error");
    assert!(msg.contains("not a working tree"), "expected the git stderr to bubble up, got: {msg}",);
    assert_eq!(git.removes.lock().unwrap().len(), 1, "remove_worktree should still have been attempted");
    // Session record is gone regardless of the post-close worktree failure.
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
}

#[tokio::test]
async fn close_with_delete_worktree_refuses_when_no_workspace_root() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    // No workspace_root configured.
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let result = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect("close itself should succeed even without workspace_root");
    let msg = result.worktree_delete_error.expect("expected workspace-root error in result");
    assert!(msg.contains("workspace root"), "expected workspace-root error, got: {msg}",);
    assert!(git.removes.lock().unwrap().is_empty(), "remove_worktree must not be invoked");
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
}

#[tokio::test]
async fn close_with_delete_worktree_refuses_when_sessions_snapshot_unreadable() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    let ws_root = h.worktree.path().parent().unwrap().to_path_buf();
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(ws_root)),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    // Corrupt sessions.json so the strict snapshot read at the start of close fails. The destructive worktree-delete path must refuse rather than
    // treat the unreadable snapshot as "session not found, skip silently".
    std::fs::write(h.ctx.store().dir().join("sessions.json"), b"###bad###").unwrap();

    let result = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect("close itself should succeed even when sessions.json is corrupt");
    let msg = result.worktree_delete_error.expect("expected snapshot-read refusal in result");
    assert!(msg.contains("sessions snapshot"), "expected snapshot-read refusal, got: {msg}",);
    assert!(
        git.removes.lock().unwrap().is_empty(),
        "remove_worktree must not be invoked when sessions snapshot is unreadable"
    );
}

#[tokio::test]
async fn close_with_delete_worktree_refuses_when_outside_workspace_root() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    // Configure workspace_root at an unrelated TempDir so the session's worktree is *not* contained under it.
    let unrelated_root = TempDir::new().unwrap();
    h.ctx
        .store()
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(unrelated_root.path().to_path_buf())),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let result = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect("close itself should succeed even when path is outside workspace_root");
    let msg = result.worktree_delete_error.expect("expected containment error in result");
    assert!(msg.contains("outside workspace root"), "expected containment error, got: {msg}",);
    assert!(git.removes.lock().unwrap().is_empty(), "remove_worktree must not be invoked");
}

#[tokio::test]
async fn restart_reuses_composed_command_and_yields_new_pid() {
    let h = build_harness();
    let v1 = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let original_pid = v1.pid.unwrap();

    session_restart_impl(
        &h.ctx,
        SessionRestartArgs {
            session_id: v1.id,
            cols: 80,
            rows: 24,
        },
    )
    .unwrap();

    // After restart, list reports a Running session with a new PID.
    let listed = session_list_impl(&h.ctx).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, SessionStatus::Running);
    let new_pid = listed[0].pid.unwrap();
    assert_ne!(new_pid, original_pid, "restart must yield a fresh PID");

    // Spawner saw two spawns total (initial + restart).
    assert_eq!(h.spawner.state.lock().unwrap().spawn_count, 2);
}

#[tokio::test]
async fn frontend_ready_is_one_shot() {
    let h = build_harness();
    assert!(frontend_ready_impl(&h.ctx), "first call must win the CAS");
    assert!(!frontend_ready_impl(&h.ctx), "subsequent call must be a no-op");
}

#[tokio::test]
async fn restore_defers_spawn_until_first_session_resize() {
    // Bootstrap a harness, persist a session, then drop the pool and rebuild a fresh ctx around the same store — simulating an app restart.
    let h = build_harness();
    let original = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let persisted = h.ctx.store().load_sessions().get(&original.id).cloned().unwrap();
    let original_command = persisted.composed_command.clone();

    // "Restart": new pool, new spawner, same store.
    let spawner2 = Arc::new(FakeSpawner::new());
    let pool2 = Arc::new(PtyPool::new(spawner2.clone() as Arc<dyn PtySpawner>));
    let events2 = Arc::new(CapturedEvents::default());
    let sink2 = capture_sink(Arc::clone(&events2), h.ctx.store().clone());
    let ctx2 = Arc::new(AppContext::with_real_git(pool2, h.ctx.store().clone(), sink2));

    restore_all_sessions(&ctx2);

    // Restore *registers* the session for deferred spawn but doesn't spawn yet — so the spawner stays untouched and status is still Starting.
    assert!(
        spawner2.state.lock().unwrap().last_cmd.is_none(),
        "restore_all_sessions must not invoke the spawner directly anymore"
    );
    let listed_pre = session_list_impl(&ctx2).unwrap();
    assert_eq!(listed_pre.len(), 1);
    assert_eq!(listed_pre[0].status, SessionStatus::Starting);
    assert_eq!(listed_pre[0].pid, None);

    // The first session_resize from the (now-mounted) frontend triggers the actual spawn — at the freshly-measured size, so the CLI's first paint
    // sees the right cols/rows.
    session_resize_impl(
        &ctx2,
        SessionResizeArgs {
            session_id: original.id,
            cols: 132,
            rows: 50,
        },
    )
    .expect("deferred spawn via resize ok");

    let listed = session_list_impl(&ctx2).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, original.id);
    assert_eq!(listed[0].status, SessionStatus::Running);
    assert_eq!(listed[0].pid, Some(9000));

    // The PtySize handed to the spawner reflects the frontend-measured dims, not DEFAULT_PTY_SIZE — this is the whole point of the fix.
    let st = spawner2.state.lock().unwrap();
    let size = st.last_size.expect("spawner should have recorded a size");
    assert_eq!(size.cols, 132);
    assert_eq!(size.rows, 50);

    // Composed command was reused verbatim — never recomposed.
    let cmd = st.last_cmd.as_ref().unwrap();
    let composed_in_args = cmd.args.iter().find(|a| a.contains("claude")).cloned();
    assert!(
        composed_in_args
            .as_deref()
            .is_some_and(|s| s == original_command.as_str() || s.contains(&original_command)),
        "expected restored spawn to receive original composed command verbatim; got {cmd:?}, original {original_command:?}"
    );
}

// (no extra trailing helpers)

#[tokio::test]
async fn restore_drops_session_record_when_worktree_directory_is_missing() {
    // Bootstrap a harness, persist a session, then delete the worktree directory before restore. The restore loop must drop the persisted record and
    // trim its id from `last_open_sessions` / `tab_order` / `active_session_id` rather than spawn (which would fail with an opaque OS error) or leave
    // a permanent ghost tab in `Error` state.
    //
    // This is the cross-restart counterpart to the workspace-switch park flow: parked sessions are revived by the same restore path, so a worktree
    // that disappeared while parked must be cleaned up during restore rather than re-projected as a phantom tab.
    let h = build_harness();
    let original = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    // Seed config so we can prove last_open_sessions/tab_order/active_session_id get trimmed too.
    h.ctx
        .store()
        .save_config(arborist_lib::types::PartialAppConfig {
            last_open_sessions: Some(vec![original.id]),
            tab_order: Some(vec![original.id]),
            active_session_id: Some(Some(original.id)),
            ..Default::default()
        })
        .unwrap();

    let worktree_path = h.worktree.path().to_path_buf();
    drop(h.worktree);
    assert!(!worktree_path.exists(), "precondition: worktree was removed");

    // "Restart": new pool + sink + ctx around the same store.
    let spawner2 = Arc::new(FakeSpawner::new());
    let pool2 = Arc::new(PtyPool::new(spawner2.clone() as Arc<dyn PtySpawner>));
    let events2 = Arc::new(CapturedEvents::default());
    let sink2 = capture_sink(Arc::clone(&events2), h.ctx.store().clone());
    let ctx2 = AppContext::with_real_git(pool2, h.ctx.store().clone(), sink2);

    restore_all_sessions(&ctx2);

    // Spawner must not have been invoked.
    assert!(
        spawner2.state.lock().unwrap().last_cmd.is_none(),
        "spawn must not be attempted when worktree is missing"
    );

    // The persisted record is gone — no phantom tab.
    let listed = session_list_impl(&Arc::new(ctx2)).unwrap();
    assert!(listed.is_empty(), "stale-worktree session record must be dropped, got: {listed:?}");

    // Config bookkeeping is trimmed too.
    let cfg = h.ctx.store().load_config();
    assert!(cfg.last_open_sessions.is_empty(), "stale id must be trimmed from last_open_sessions");
    assert!(cfg.tab_order.is_empty(), "stale id must be trimmed from tab_order");
    assert_eq!(
        cfg.active_session_id, None,
        "active_session_id pointing at a dropped session must be cleared"
    );
}

#[tokio::test]
async fn restore_trims_orphan_ids_in_config_with_no_session_record() {
    // Defense-in-depth: the seed-fix in `seed.rs` strips `lastOpenSessions` / `tabOrder` / `activeSessionId` from `config.json` when a branch build
    // seeds without a paired `sessions.json`. Pre-fix-state stores already have phantom IDs in config that don't correspond to any record. The
    // `trim_unknown_session_refs` step in `restore_all_sessions` cleans those up on first restore after the upgrade.
    //
    // Distinct from the worktree-missing test above: there, the record DOES exist but its worktree is gone. Here, the record never existed at all —
    // only the config refers to the IDs.
    let h = build_harness();

    // Stuff config with phantom IDs only (no `session_create_impl`).
    let phantom_a = arborist_lib::types::SessionId(uuid::Uuid::new_v4());
    let phantom_b = arborist_lib::types::SessionId(uuid::Uuid::new_v4());
    h.ctx
        .store()
        .save_config(arborist_lib::types::PartialAppConfig {
            last_open_sessions: Some(vec![phantom_a, phantom_b]),
            tab_order: Some(vec![phantom_a, phantom_b]),
            active_session_id: Some(Some(phantom_a)),
            ..Default::default()
        })
        .unwrap();

    // Sessions store is empty (matches the bug scenario where the seed copied config.json but skipped sessions.json).
    assert!(h.ctx.store().load_sessions().is_empty());

    // "Restart": new pool + sink + ctx around the same store.
    let spawner2 = Arc::new(FakeSpawner::new());
    let pool2 = Arc::new(PtyPool::new(spawner2.clone() as Arc<dyn PtySpawner>));
    let events2 = Arc::new(CapturedEvents::default());
    let sink2 = capture_sink(Arc::clone(&events2), h.ctx.store().clone());
    let ctx2 = AppContext::with_real_git(pool2, h.ctx.store().clone(), sink2);

    restore_all_sessions(&ctx2);

    // Spawner must not have been invoked — there are no real records.
    assert!(
        spawner2.state.lock().unwrap().last_cmd.is_none(),
        "spawn must not be attempted for phantom-only IDs"
    );

    // Config orphan IDs must be trimmed.
    let cfg = h.ctx.store().load_config();
    assert!(
        cfg.last_open_sessions.is_empty(),
        "phantom IDs must be trimmed from last_open_sessions, got {:?}",
        cfg.last_open_sessions
    );
    assert!(
        cfg.tab_order.is_empty(),
        "phantom IDs must be trimmed from tab_order, got {:?}",
        cfg.tab_order
    );
    assert_eq!(cfg.active_session_id, None, "phantom active_session_id must be cleared");
}

#[tokio::test]
async fn restore_does_not_rewrite_config_when_no_orphans_present() {
    // The trim helper must be a no-op when nothing needs trimming (otherwise every launch would needlessly rewrite config.json). We can't directly
    // observe "no write" without a fake store, but we can prove value-stability: a pre-existing config with all valid IDs must round-trip
    // byte-for-byte after restore.
    let h = build_harness();
    let session = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    h.ctx
        .store()
        .save_config(arborist_lib::types::PartialAppConfig {
            last_open_sessions: Some(vec![session.id]),
            tab_order: Some(vec![session.id]),
            active_session_id: Some(Some(session.id)),
            ..Default::default()
        })
        .unwrap();
    let cfg_before = h.ctx.store().load_config();

    // Fresh ctx so restore actually runs.
    let spawner2 = Arc::new(FakeSpawner::new());
    let pool2 = Arc::new(PtyPool::new(spawner2.clone() as Arc<dyn PtySpawner>));
    let events2 = Arc::new(CapturedEvents::default());
    let sink2 = capture_sink(Arc::clone(&events2), h.ctx.store().clone());
    let ctx2 = AppContext::with_real_git(pool2, h.ctx.store().clone(), sink2);

    restore_all_sessions(&ctx2);

    let cfg_after = h.ctx.store().load_config();
    assert_eq!(
        cfg_before.last_open_sessions, cfg_after.last_open_sessions,
        "no-orphan restore must not mutate last_open_sessions"
    );
    assert_eq!(cfg_before.tab_order, cfg_after.tab_order, "no-orphan restore must not mutate tab_order");
    assert_eq!(
        cfg_before.active_session_id, cfg_after.active_session_id,
        "no-orphan restore must not mutate active_session_id"
    );
}

// --------------------------------------------------------------------------- AI session-id pre-allocation (Phase 2)
// ---------------------------------------------------------------------------
//
// Background. Pre-pre-allocation, `Session.ai_session_id` was discovered post-spawn from a CLI-side write (Claude transcript / Copilot OTel chat
// span). A session that was created and never prompted before app shutdown therefore had `ai_session_id == None`, and restore-on-launch dropped the
// `--resume` augmentation entirely — the session came back as a fresh CLI conversation. The user reported this as "only my open tab fully resumes".
// Pre-allocation closes the gap for Copilot by deciding the conversation id at create-time and binding the spawn to it via `--resume <uuid>`. Copilot
// starts a fresh session at that uuid (verified against `copilot --help`), so no on-disk transcript is required at spawn time. Claude has no
// equivalent flag and continues the discovery path.

fn create_args_for(h: &Harness, tool: Tool) -> SessionCreateArgs {
    SessionCreateArgs {
        tool,
        worktree_path: h.worktree.path().to_path_buf(),
        cols: 80,
        rows: 24,
    }
}

/// Returns the spawned shell argv as a single string for substring asserts. The spawn cmd is platform-shaped (`cmd.exe /c <cmd>` on Windows, `$SHELL
/// -c <cmd>` elsewhere); the composed command lives in the trailing arg in both cases.
fn last_spawn_args_joined(spawner: &FakeSpawner) -> String {
    let st = spawner.state.lock().unwrap();
    let cmd = st.last_cmd.as_ref().expect("expected at least one spawn");
    cmd.args.join(" ")
}

#[tokio::test]
async fn session_create_preallocates_ai_session_id_for_copilot() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args_for(&h, Tool::Copilot)).unwrap();

    let persisted = h.ctx.store().load_sessions().get(&view.id).cloned().expect("session must persist");
    let aid = persisted
        .ai_session_id
        .as_deref()
        .expect("Copilot create must pre-allocate ai_session_id");
    assert!(uuid::Uuid::parse_str(aid).is_ok(), "pre-allocated id should be a uuid; got {aid:?}",);

    // composed_command itself stays bare; the persisted record is immutable. The splice happens on a clone at spawn time only.
    assert!(
        !persisted.composed_command.contains("--resume"),
        "persisted composed_command must stay bare; got {:?}",
        persisted.composed_command
    );

    // The actual spawn DID receive the augmented command.
    let spawned = last_spawn_args_joined(&h.spawner);
    assert!(
        spawned.contains(&format!("--resume {aid}")),
        "spawn args must include `--resume <preallocated-uuid>`; got {spawned:?}",
    );
}

#[tokio::test]
async fn session_create_does_not_preallocate_for_claude() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args_for(&h, Tool::Claude)).unwrap();

    let persisted = h.ctx.store().load_sessions().get(&view.id).cloned().unwrap();
    assert_eq!(
        persisted.ai_session_id, None,
        "Claude must not pre-allocate; id is discovered from transcript",
    );

    let spawned = last_spawn_args_joined(&h.spawner);
    assert!(
        !spawned.contains("--resume"),
        "Claude spawn must not splice --resume on create; got {spawned:?}",
    );
}

#[tokio::test]
async fn session_restart_reallocates_ai_session_id_for_copilot() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args_for(&h, Tool::Copilot)).unwrap();
    let id_before = h
        .ctx
        .store()
        .load_sessions()
        .get(&view.id)
        .and_then(|s| s.ai_session_id.clone())
        .expect("Copilot create must pre-allocate");

    session_restart_impl(
        &h.ctx,
        SessionRestartArgs {
            session_id: view.id,
            cols: 80,
            rows: 24,
        },
    )
    .unwrap();

    let id_after = h
        .ctx
        .store()
        .load_sessions()
        .get(&view.id)
        .and_then(|s| s.ai_session_id.clone())
        .expect("Copilot restart must keep an ai_session_id set");
    assert_ne!(
        id_before, id_after,
        "restart must allocate a fresh uuid (otherwise Copilot would resume the pre-restart conversation)",
    );
    assert!(uuid::Uuid::parse_str(&id_after).is_ok());

    let spawned = last_spawn_args_joined(&h.spawner);
    assert!(
        spawned.contains(&format!("--resume {id_after}")),
        "restart spawn must splice the freshly-allocated uuid; got {spawned:?}",
    );
}

#[tokio::test]
async fn session_restart_clears_ai_session_id_for_claude() {
    // Claude's restart contract: drop the prior conversation id eagerly so a crash between restart and the new watcher's first discovery can't leave
    // us pointing at the pre-restart transcript.
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args_for(&h, Tool::Claude)).unwrap();

    // Simulate the metrics watcher having discovered an id post-spawn.
    h.ctx
        .store()
        .update_session_ai_session_id(&view.id, Some("preexisting-claude-id".to_owned()))
        .unwrap();

    session_restart_impl(
        &h.ctx,
        SessionRestartArgs {
            session_id: view.id,
            cols: 80,
            rows: 24,
        },
    )
    .unwrap();

    let after = h.ctx.store().load_sessions().get(&view.id).cloned().unwrap();
    assert_eq!(
        after.ai_session_id, None,
        "Claude restart must clear ai_session_id (no equivalent of --resume <uuid>)",
    );

    let spawned = last_spawn_args_joined(&h.spawner);
    assert!(
        !spawned.contains("--resume"),
        "Claude restart spawn must not carry --resume; got {spawned:?}",
    );
}

#[tokio::test]
async fn restore_splices_resume_for_copilot_even_when_session_state_dir_absent() {
    // Pre-allocated Copilot uuids may legitimately have no ~/.copilot/session-state/<uuid>/ dir yet (e.g. app crashed before Copilot's first
    // session.start flush, or the dir was swept). The pre-Phase-2 code would have dropped the splice in that case via `ai_session_transcript_exists`.
    // With pre-allocation we rely on the fact that `copilot --resume <unknown-uuid>` safely creates a fresh session at that uuid — so we always
    // splice and let the persisted id win.
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args_for(&h, Tool::Copilot)).unwrap();
    let preallocated = h
        .ctx
        .store()
        .load_sessions()
        .get(&view.id)
        .and_then(|s| s.ai_session_id.clone())
        .expect("Copilot create must pre-allocate");

    // "App restart": new pool + sink + ctx around the same store. The pre-allocated uuid almost certainly has no ~/.copilot/session-state dir yet (we
    // never ran a real Copilot in this test), but restore must splice anyway.
    let spawner2 = Arc::new(FakeSpawner::new());
    let pool2 = Arc::new(PtyPool::new(spawner2.clone() as Arc<dyn PtySpawner>));
    let events2 = Arc::new(CapturedEvents::default());
    let sink2 = capture_sink(Arc::clone(&events2), h.ctx.store().clone());
    let ctx2 = AppContext::with_real_git(pool2, h.ctx.store().clone(), sink2);

    restore_all_sessions(&ctx2);

    // Restore now defers the actual spawn until first session_resize. Trigger that explicitly so we can assert on the splice the spawner sees.
    session_resize_impl(
        &ctx2,
        SessionResizeArgs {
            session_id: view.id,
            cols: 80,
            rows: 24,
        },
    )
    .expect("deferred spawn via resize ok");

    let spawned = last_spawn_args_joined(&spawner2);
    assert!(
        spawned.contains(&format!("--resume {preallocated}")),
        "Copilot restore must always splice when ai_session_id is set, even if the on-disk dir is missing; got {spawned:?}",
    );

    // The persisted id must NOT have been cleared.
    let after = h.ctx.store().load_sessions().get(&view.id).cloned().unwrap();
    assert_eq!(after.ai_session_id.as_deref(), Some(preallocated.as_str()));
}

#[tokio::test]
async fn failed_copilot_restart_preserves_prior_ai_session_id() {
    // Regression for the restart write-order bug. Pre-fix, `session_restart_impl` rotated the persisted Copilot `ai_session_id` to a
    // freshly-allocated uuid *before* attempting `respawn_existing`. If respawn failed (e.g. transient PTY error, `cmd.exe` not found, etc.), the
    // store was left pointing at a brand-new uuid that has no Copilot session-state directory and the prior — still-resumable — conversation was
    // orphaned.
    //
    // The fix defers the persist to *after* respawn succeeds. This test drives the failure path via `FakeSpawner::fail_next_with` and asserts the
    // persisted record still carries the original pre-allocated uuid so the user can recover by restarting again.
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args_for(&h, Tool::Copilot)).unwrap();
    let original_id = h
        .ctx
        .store()
        .load_sessions()
        .get(&view.id)
        .and_then(|s| s.ai_session_id.clone())
        .expect("Copilot create must pre-allocate");

    // Arm the spawner to reject the next spawn (the restart path's `respawn_existing` call).
    {
        let mut s = h.spawner.state.lock().unwrap();
        s.fail_next_with = Some(arborist_lib::types::Error::PtySpawnFailed("simulated transient spawn failure".to_owned()));
    }

    let result = session_restart_impl(
        &h.ctx,
        SessionRestartArgs {
            session_id: view.id,
            cols: 80,
            rows: 24,
        },
    );
    assert!(result.is_err(), "restart must surface the spawner failure to the caller; got {result:?}",);

    let after = h
        .ctx
        .store()
        .load_sessions()
        .get(&view.id)
        .cloned()
        .expect("session record must still exist after a failed restart");
    assert_eq!(
        after.ai_session_id.as_deref(),
        Some(original_id.as_str()),
        "failed restart must NOT rotate ai_session_id (would orphan the prior conversation)",
    );
    // The session is in Error status (the restart path explicitly sets it).
    assert_eq!(after.status, SessionStatus::Error);
}
