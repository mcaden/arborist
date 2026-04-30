//! Phase 7 session-lifecycle integration tests using a deterministic fake
//! PTY spawner. These tests drive the same `*_impl` business-logic
//! functions the production Tauri command wrappers call, so they cover the
//! full session-create → spawn → input → resize → close path without
//! depending on a real Claude/Copilot install.
//!
//! ## Why duplicate the FakeSpawner?
//!
//! Rust integration tests (`tests/*.rs`) are compiled as separate crates,
//! so each test file has to bring its own helpers. We could promote the
//! one in `tests/pty_pool.rs` to a `pub(crate)` test-support module, but
//! that drags `portable-pty` into the public surface for the sake of test
//! ergonomics. Copying the small helper here keeps the production crate's
//! public surface honest.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arborist_lib::commands::session::{
    frontend_ready_impl, restore_all_sessions, session_close_impl, session_create_impl,
    session_focus_impl, session_input_impl, session_list_impl, session_resize_impl,
    session_restart_impl, AppContext,
};
use arborist_lib::compose::session_temp_dir;
use arborist_lib::config_store::ConfigStore;
use arborist_lib::git::GitRunner;
use arborist_lib::pty_pool::{
    ChildCommand, PtyKiller, PtyPool, PtyResize, PtySink, PtySpawner, PtyWaiter, SpawnedChild,
};
use arborist_lib::types::{
    InstructionSetId, PartialAppConfig, PartialDefaultInstructionSets, SessionCreateArgs,
    SessionId, SessionInputArgs, SessionResizeArgs, SessionStatus, Tool, WorktreeInfo,
};
use portable_pty::{ExitStatus, PtySize};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Deterministic fake spawner (cf. tests/pty_pool.rs::FakeSpawner)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SpawnerState {
    spawn_count: usize,
    last_cwd: Option<PathBuf>,
    last_cmd: Option<ChildCommand>,
    /// One entry per spawn so a test can keep killing/respawning.
    eofs: Vec<Arc<AtomicBool>>,
    next_pid: u32,
}

struct FakeSpawner {
    state: Mutex<SpawnerState>,
}

impl FakeSpawner {
    fn new() -> Self {
        Self {
            state: Mutex::new(SpawnerState {
                next_pid: 9000,
                ..SpawnerState::default()
            }),
        }
    }
}

impl PtySpawner for FakeSpawner {
    fn spawn(
        &self,
        cmd: ChildCommand,
        cwd: &Path,
        _size: PtySize,
    ) -> Result<SpawnedChild, arborist_lib::types::Error> {
        let mut s = self.state.lock().unwrap();
        s.spawn_count += 1;
        s.last_cwd = Some(cwd.to_path_buf());
        s.last_cmd = Some(cmd);
        let pid = s.next_pid;
        s.next_pid += 1;
        let eof = Arc::new(AtomicBool::new(false));
        s.eofs.push(Arc::clone(&eof));

        Ok(SpawnedChild {
            pid,
            reader: Box::new(ParkedReader {
                eof: Arc::clone(&eof),
            }),
            writer: Box::new(WriteCapture),
            resize: Arc::new(NoopResize),
            waiter: Box::new(BlockingWaiter {
                eof: Arc::clone(&eof),
            }),
            killer: Arc::new(EofKiller { eof }),
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
}
impl PtyKiller for EofKiller {
    fn kill(&self) -> Result<(), arborist_lib::types::Error> {
        self.eof.store(true, Ordering::Relaxed);
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

// ---------------------------------------------------------------------------
// Sink that captures emissions for assertion
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
    let status = Arc::new(
        move |id: &SessionId, st: SessionStatus, pid: Option<u32>, msg: Option<String>| {
            // Mirror production wiring: persist status, swallow NotFound.
            if let Err(e) = store.update_session_status(id, st, pid) {
                use arborist_lib::types::Error as E;
                if !matches!(e, E::NotFound(_)) {
                    panic!("unexpected status persist error: {e:?}");
                }
            }
            status_events
                .status
                .lock()
                .unwrap()
                .push((*id, st, pid, msg));
        },
    );
    PtySink::new(output, status, Arc::new(|_id, _evt| {}))
}

// ---------------------------------------------------------------------------
// Test harness builder
// ---------------------------------------------------------------------------

struct Harness {
    ctx: Arc<AppContext>,
    spawner: Arc<FakeSpawner>,
    events: Arc<CapturedEvents>,
    _config_dir: TempDir,
    _instructions_dir: TempDir,
    worktree: TempDir,
    instruction_id: InstructionSetId,
}

/// Records `remove_worktree` invocations so a test can assert opt-in
/// deletion was forwarded to the git layer.
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
    fn create_worktree(
        &self,
        repo_root: &Path,
        relative_path: &Path,
        _branch: &str,
    ) -> Result<PathBuf, arborist_lib::types::Error> {
        Ok(repo_root.join(relative_path))
    }
    fn remove_worktree(
        &self,
        repo_root: &Path,
        worktree_path: &Path,
    ) -> Result<(), arborist_lib::types::Error> {
        self.removes
            .lock()
            .unwrap()
            .push((repo_root.to_path_buf(), worktree_path.to_path_buf()));
        if let Some(msg) = self.fail_with.lock().unwrap().clone() {
            return Err(arborist_lib::types::Error::Internal(msg));
        }
        Ok(())
    }
}

fn build_harness() -> Harness {
    build_harness_with_git(Arc::new(arborist_lib::git::RealGitRunner) as Arc<dyn GitRunner>)
}

fn build_harness_with_git(git: Arc<dyn GitRunner>) -> Harness {
    let config_dir = TempDir::new().unwrap();
    let instructions_dir = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();

    // Seed a single instruction set on disk so session_create can resolve it.
    let instruction_id = InstructionSetId("claude-default".into());
    let instr_path = instructions_dir.path().join("claude-default.md");
    std::fs::write(&instr_path, "# Claude default instructions\nbe helpful").unwrap();

    let store = ConfigStore::open(config_dir.path()).unwrap();
    // Wire the discovery dir so the instruction lookup succeeds.
    store
        .save_config(PartialAppConfig {
            instruction_sets_dir: Some(instructions_dir.path().to_path_buf()),
            default_instruction_sets: Some(PartialDefaultInstructionSets {
                claude: Some(instruction_id.clone()),
                copilot: None,
            }),
            ..Default::default()
        })
        .unwrap();

    let spawner = Arc::new(FakeSpawner::new());
    let pool = Arc::new(PtyPool::new(spawner.clone() as Arc<dyn PtySpawner>));
    let events = Arc::new(CapturedEvents::default());
    let sink = capture_sink(Arc::clone(&events), store.clone());
    let ctx = Arc::new(AppContext::new(pool, store, sink, git, Arc::new(|_| {})));

    Harness {
        ctx,
        spawner,
        events,
        _config_dir: config_dir,
        _instructions_dir: instructions_dir,
        worktree,
        instruction_id,
    }
}

fn create_args(h: &Harness) -> SessionCreateArgs {
    SessionCreateArgs {
        tool: Tool::Claude,
        worktree_path: h.worktree.path().to_path_buf(),
        instruction_set_id: Some(h.instruction_id.clone()),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_emits_starting_then_running_and_persists_session() {
    let h = build_harness();

    let view = session_create_impl(&h.ctx, create_args(&h)).expect("create ok");
    assert_eq!(view.status, SessionStatus::Running);
    assert!(view.pid.is_some());

    // Status sequence: Starting (from impl) then Running (from pool).
    let statuses = h.events.status.lock().unwrap().clone();
    assert!(
        statuses.len() >= 2,
        "expected ≥2 status events, got {statuses:?}"
    );
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
    let cfg = h.ctx.store.load_config();
    assert_eq!(cfg.active_session_id, Some(view.id));
    assert_eq!(cfg.tab_order, vec![view.id]);
    assert_eq!(cfg.last_open_sessions, vec![view.id]);

    // Spawn was called with the discrete cwd, never interpolated.
    let st = h.spawner.state.lock().unwrap();
    let cwd = st.last_cwd.as_ref().unwrap();
    let cwd_canon = dunce::canonicalize(cwd).unwrap();
    let wt_canon = dunce::canonicalize(h.worktree.path()).unwrap();
    assert_eq!(
        cwd_canon, wt_canon,
        "fake spawner cwd should canonicalize to the worktree"
    );
    let cmd = st.last_cmd.as_ref().unwrap();
    assert!(
        !cmd.args.iter().any(|a| a.contains("cd ")),
        "composed command must not contain `cd <path>` interpolation"
    );
}

#[tokio::test]
async fn create_with_unknown_instruction_set_returns_notfound() {
    let h = build_harness();
    let args = SessionCreateArgs {
        tool: Tool::Claude,
        worktree_path: h.worktree.path().to_path_buf(),
        instruction_set_id: Some(InstructionSetId("does-not-exist".into())),
    };
    let err = session_create_impl(&h.ctx, args).expect_err("should fail");
    assert_eq!(err.code, "NotFound");
}

#[tokio::test]
async fn create_with_tool_mismatch_returns_toolmismatch() {
    let h = build_harness();
    // Add a copilot-tagged instruction set under the same dir.
    let copilot_path = h._instructions_dir.path().join("copilot-only.md");
    std::fs::write(&copilot_path, "for copilot").unwrap();
    let args = SessionCreateArgs {
        tool: Tool::Claude,
        worktree_path: h.worktree.path().to_path_buf(),
        instruction_set_id: Some(InstructionSetId("copilot-only".into())),
    };
    let err = session_create_impl(&h.ctx, args).expect_err("should fail");
    assert_eq!(err.code, "ToolMismatch");
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
    assert_eq!(h.ctx.store.load_config().active_session_id, Some(v2.id));

    session_focus_impl(&h.ctx, v1.id).unwrap();
    assert_eq!(h.ctx.store.load_config().active_session_id, Some(v1.id));

    let unknown = SessionId::new();
    let err = session_focus_impl(&h.ctx, unknown).expect_err("should fail");
    assert_eq!(err.code, "NotFound");
}

#[tokio::test]
async fn close_kills_pty_removes_record_and_clears_active() {
    let h = build_harness();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let temp = session_temp_dir(&view.id);
    assert!(temp.exists(), "Claude temp dir must exist after create");

    session_close_impl(&h.ctx, view.id, false).await.unwrap();

    assert!(!h.ctx.pool.contains(&view.id));
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
    let cfg = h.ctx.store.load_config();
    assert_eq!(cfg.active_session_id, None);
    assert!(cfg.tab_order.is_empty());
    assert!(cfg.last_open_sessions.is_empty());

    // Temp dir is swept by either pool.kill or the post-close belt-and-braces
    // cleanup. Allow a beat for filesystem to settle on Windows.
    let cleared = wait_until(|| !temp.exists(), Duration::from_secs(2));
    assert!(cleared, "session temp dir {temp:?} should be removed");
}

#[tokio::test]
async fn close_with_delete_worktree_invokes_git_runner_remove() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    // Configure a workspace root that *contains* the session's worktree so
    // the containment check passes.
    let ws_root = h.worktree.path().parent().unwrap().to_path_buf();
    h.ctx
        .store
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
    let expected_canon =
        dunce::canonicalize(&worktree_path).unwrap_or_else(|_| worktree_path.clone());
    assert_eq!(
        wt_canon, expected_canon,
        "remove_worktree must be called with the session's worktree path"
    );
    // Session record is gone regardless.
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
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
    // Point workspace_root at the same path as the session's worktree so
    // the safety check trips.
    h.ctx
        .store
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(h.worktree.path().to_path_buf())),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let err = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect_err("must refuse to delete the main worktree");
    assert!(
        err.message.contains("workspace root") || err.message.contains("main worktree"),
        "expected a workspace-root refusal message, got: {}",
        err.message
    );
    assert!(
        git.removes.lock().unwrap().is_empty(),
        "remove_worktree must not be invoked when refused"
    );
    // The session is still removed because the kill+config cleanup happens
    // before the worktree-deletion attempt — the user opted in to losing
    // the session even if the worktree is preserved.
    assert!(session_list_impl(&h.ctx).unwrap().is_empty());
}

#[tokio::test]
async fn close_with_delete_worktree_propagates_git_failure() {
    let git = RecordingGitRunner::new();
    *git.fail_with.lock().unwrap() = Some("fatal: not a working tree".into());
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    let ws_root = h.worktree.path().parent().unwrap().to_path_buf();
    h.ctx
        .store
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(ws_root)),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let err = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect_err("git failure must surface to caller");
    assert!(
        err.message.contains("not a working tree"),
        "expected the git stderr to bubble up, got: {}",
        err.message
    );
    assert_eq!(
        git.removes.lock().unwrap().len(),
        1,
        "remove_worktree should still have been attempted"
    );
}

#[tokio::test]
async fn close_with_delete_worktree_refuses_when_no_workspace_root() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    // No workspace_root configured.
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let err = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect_err("must refuse without workspace_root");
    assert!(
        err.message.contains("workspace root"),
        "expected workspace-root error, got: {}",
        err.message
    );
    assert!(
        git.removes.lock().unwrap().is_empty(),
        "remove_worktree must not be invoked"
    );
}

#[tokio::test]
async fn close_with_delete_worktree_refuses_when_outside_workspace_root() {
    let git = RecordingGitRunner::new();
    let h = build_harness_with_git(git.clone() as Arc<dyn GitRunner>);
    // Configure workspace_root at an unrelated TempDir so the session's
    // worktree is *not* contained under it.
    let unrelated_root = TempDir::new().unwrap();
    h.ctx
        .store
        .save_config(PartialAppConfig {
            workspace_root: Some(Some(unrelated_root.path().to_path_buf())),
            ..Default::default()
        })
        .unwrap();
    let view = session_create_impl(&h.ctx, create_args(&h)).unwrap();

    let err = session_close_impl(&h.ctx, view.id, true)
        .await
        .expect_err("must refuse paths outside workspace_root");
    assert!(
        err.message.contains("outside workspace root"),
        "expected containment error, got: {}",
        err.message
    );
    assert!(
        git.removes.lock().unwrap().is_empty(),
        "remove_worktree must not be invoked"
    );
}

#[tokio::test]
async fn restart_reuses_composed_command_and_yields_new_pid() {
    let h = build_harness();
    let v1 = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let original_pid = v1.pid.unwrap();

    session_restart_impl(&h.ctx, v1.id).unwrap();

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
    assert!(
        !frontend_ready_impl(&h.ctx),
        "subsequent call must be a no-op"
    );
}

#[tokio::test]
async fn restore_respawns_persisted_sessions_without_recomposing() {
    // Bootstrap a harness, persist a session, then drop the pool and rebuild
    // a fresh ctx around the same store — simulating an app restart.
    let h = build_harness();
    let original = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let persisted = h
        .ctx
        .store
        .load_sessions()
        .get(&original.id)
        .cloned()
        .unwrap();
    let original_command = persisted.composed_command.clone();

    // "Restart": new pool, new spawner, same store.
    let spawner2 = Arc::new(FakeSpawner::new());
    let pool2 = Arc::new(PtyPool::new(spawner2.clone() as Arc<dyn PtySpawner>));
    let events2 = Arc::new(CapturedEvents::default());
    let sink2 = capture_sink(Arc::clone(&events2), h.ctx.store.clone());
    let ctx2 = AppContext::with_real_git(pool2, h.ctx.store.clone(), sink2);

    restore_all_sessions(&ctx2);

    // The restored session should still appear and now have a fresh PID
    // (the new spawner starts at 9000).
    let listed = session_list_impl(&Arc::new(ctx2)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, original.id);
    assert_eq!(listed[0].status, SessionStatus::Running);
    assert_eq!(listed[0].pid, Some(9000));

    // Composed command was reused verbatim — never recomposed.
    let st = spawner2.state.lock().unwrap();
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
async fn restore_emits_error_with_message_when_worktree_directory_is_missing() {
    // Bootstrap a harness, persist a session, then delete the worktree
    // directory before restore. The restore loop should emit an Error
    // status with a human-readable "Worktree path no longer exists"
    // message instead of attempting to spawn (which would fail with an
    // opaque OS error).
    let h = build_harness();
    let original = session_create_impl(&h.ctx, create_args(&h)).unwrap();
    let worktree_path = h.worktree.path().to_path_buf();

    // Drop the harness's TempDir handle so the directory disappears.
    drop(h.worktree);
    assert!(
        !worktree_path.exists(),
        "precondition: worktree was removed"
    );

    // "Restart": new pool + sink + ctx around the same store.
    let spawner2 = Arc::new(FakeSpawner::new());
    let pool2 = Arc::new(PtyPool::new(spawner2.clone() as Arc<dyn PtySpawner>));
    let events2 = Arc::new(CapturedEvents::default());
    let sink2 = capture_sink(Arc::clone(&events2), h.ctx.store.clone());
    let ctx2 = AppContext::with_real_git(pool2, h.ctx.store.clone(), sink2);

    restore_all_sessions(&ctx2);

    // Spawner must not have been invoked — we short-circuited before the
    // PTY pool got involved.
    assert!(
        spawner2.state.lock().unwrap().last_cmd.is_none(),
        "spawn must not be attempted when worktree is missing"
    );

    // The status sink received exactly one Error with the annotated
    // message naming the missing path.
    let statuses = events2.status.lock().unwrap();
    let entry = statuses
        .iter()
        .find(|(id, _, _, _)| *id == original.id)
        .expect("expected a status entry for the restored session");
    assert_eq!(entry.1, SessionStatus::Error);
    assert_eq!(entry.2, None);
    let message = entry.3.as_deref().expect("message should be present");
    assert!(
        message.contains("Worktree path is no longer available"),
        "message did not mention stale worktree: {message}"
    );
    // The path mentioned in the message comes from the canonicalized
    // session record, which may differ in formatting from the original
    // temp-dir path. Just assert the message includes *some* path
    // component matching the temp-dir basename.
    let basename = worktree_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    assert!(
        !basename.is_empty() && message.contains(basename),
        "message did not include the missing path basename {basename}: {message}"
    );

    // The persisted record reflects the Error status too.
    let listed = session_list_impl(&Arc::new(ctx2)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, SessionStatus::Error);
}
