//! Phase 7 sub-session lifecycle integration tests: parent-close cascade,
//! closing-parent tombstone, restore-on-launch second pass, and relaunch.
//!
//! Mirrors the FakeSpawner pattern from `tests/session_lifecycle_fake.rs`
//! (each Rust integration test file is its own crate, so helpers must be
//! duplicated). Application-kind paths are exercised by the `app_launcher`
//! unit tests + the frontend `SidebarSubTab.test.tsx`; this file focuses
//! on terminal-kind cascade/tombstone/restore/relaunch behaviour, which is
//! the part with non-trivial cross-module coordination.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arborist_lib::app_launcher::{AppPool, AppSpawner, RealAppSpawner};
use arborist_lib::commands::session::{session_close_impl, session_create_impl, AppContext};
use arborist_lib::commands::subsession::{
    close_for_parent_impl, restore_all_sub_sessions_impl, subsession_create_impl,
    subsession_relaunch_impl,
};
use arborist_lib::config_store::ConfigStore;
use arborist_lib::pty_pool::{
    ChildCommand, PtyKiller, PtyPool, PtyResize, PtySink, PtySpawner, PtyWaiter, SpawnedChild,
};
use arborist_lib::sub_sessions::{SubPtyPool, SubPtySink, SubSessionStore};
use arborist_lib::types::{
    CustomProcessDef, CustomProcessDefId, CustomProcessKind, InstructionSetId, PartialAppConfig,
    PartialDefaultInstructionSets, SessionCreateArgs, SessionId, SessionStatus,
    SubSessionCreateArgs, SubSessionStatus, Tool,
};
use arborist_lib::window_focus::RecordingFocuser;
use portable_pty::{ExitStatus, PtySize};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fake parent-PTY spawner (cf. tests/session_lifecycle_fake.rs::FakeSpawner)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SpawnerState {
    spawn_count: usize,
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
        _cmd: ChildCommand,
        _cwd: &Path,
        _size: PtySize,
    ) -> Result<SpawnedChild, arborist_lib::types::Error> {
        let mut s = self.state.lock().unwrap();
        s.spawn_count += 1;
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
// Capturing sinks
// ---------------------------------------------------------------------------

type StatusTuple = (
    arborist_lib::types::SubSessionId,
    SubSessionStatus,
    Option<u32>,
    Option<String>,
);

#[derive(Default)]
struct CapturedSubEvents {
    statuses: Mutex<Vec<StatusTuple>>,
    restored: Mutex<Vec<arborist_lib::types::SubSession>>,
}

fn make_sub_sink(events: Arc<CapturedSubEvents>) -> SubPtySink {
    let s = Arc::clone(&events);
    let status = Arc::new(
        move |id: &arborist_lib::types::SubSessionId,
              st: SubSessionStatus,
              pid: Option<u32>,
              msg: Option<String>| {
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
    let status = Arc::new(
        move |id: &SessionId, st: SessionStatus, pid: Option<u32>, _msg: Option<String>| {
            if let Err(e) = store.update_session_status(id, st, pid) {
                use arborist_lib::types::Error as E;
                if !matches!(e, E::NotFound(_)) {
                    panic!("unexpected status persist error: {e:?}");
                }
            }
        },
    );
    PtySink::new(Arc::new(|_, _| {}), status, Arc::new(|_, _| {}))
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct Harness {
    ctx: Arc<AppContext>,
    sub_ctx: Arc<arborist_lib::sub_sessions::SubAppContext>,
    sub_pool: Arc<SubPtyPool>,
    sub_events: Arc<CapturedSubEvents>,
    _config_dir: TempDir,
    _instructions_dir: TempDir,
    worktree: TempDir,
    instruction_id: InstructionSetId,
    shell_def_id: CustomProcessDefId,
}

fn build_harness() -> Harness {
    let config_dir = TempDir::new().unwrap();
    let instructions_dir = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();

    let instruction_id = InstructionSetId("claude-default".into());
    std::fs::write(
        instructions_dir.path().join("claude-default.md"),
        "# Claude\nbe helpful",
    )
    .unwrap();

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

    let store = ConfigStore::open(config_dir.path()).unwrap();
    store
        .save_config(PartialAppConfig {
            instruction_sets_dir: Some(instructions_dir.path().to_path_buf()),
            default_instruction_sets: Some(PartialDefaultInstructionSets {
                claude: Some(instruction_id.clone()),
                copilot: None,
            }),
            custom_processes: Some(vec![shell_def]),
            ..Default::default()
        })
        .unwrap();

    let parent_spawner = Arc::new(FakeSpawner::new());
    let parent_pool = Arc::new(PtyPool::new(parent_spawner.clone() as Arc<dyn PtySpawner>));
    let parent_sink = make_parent_sink(store.clone());
    let ctx = Arc::new(AppContext::new(
        parent_pool,
        store.clone(),
        parent_sink,
        Arc::new(arborist_lib::git::RealGitRunner),
        Arc::new(|_| {}),
        Arc::new(|_, _| {}),
        Arc::new(|_, _| {}),
    ));

    let sub_spawner = Arc::new(FakeSpawner::new());
    let sub_pool = Arc::new(SubPtyPool::new(sub_spawner.clone() as Arc<dyn PtySpawner>));
    let sub_store = Arc::new(SubSessionStore::new());
    let sub_events = Arc::new(CapturedSubEvents::default());
    let sub_sink = make_sub_sink(Arc::clone(&sub_events));
    // App pool with the real spawner: cascade for terminal kind only
    // touches the SubPtyPool, so we never call AppPool::spawn in these
    // tests; passing RealAppSpawner is harmless.
    let app_pool = Arc::new(AppPool::new(Arc::new(RealAppSpawner) as Arc<dyn AppSpawner>));
    let focuser = Arc::new(RecordingFocuser::new());
    let icon_cache = Arc::new(arborist_lib::process_icon::IconCache::new(Arc::new(
        arborist_lib::process_icon::RealIconExtractor,
    )));
    let sub_ctx = Arc::new(arborist_lib::sub_sessions::SubAppContext::new(
        Arc::clone(&sub_pool),
        Arc::clone(&sub_store),
        sub_sink,
        app_pool,
        focuser,
        icon_cache,
    ));

    Harness {
        ctx,
        sub_ctx,
        sub_pool,
        sub_events,
        _config_dir: config_dir,
        _instructions_dir: instructions_dir,
        worktree,
        instruction_id,
        shell_def_id,
    }
}

fn create_parent(h: &Harness) -> arborist_lib::types::SessionView {
    session_create_impl(
        &h.ctx,
        SessionCreateArgs {
            tool: Tool::Claude,
            worktree_path: h.worktree.path().to_path_buf(),
            instruction_set_id: Some(h.instruction_id.clone()),
        },
    )
    .expect("parent create ok")
}

fn create_sub(
    h: &Harness,
    parent: SessionId,
) -> Result<arborist_lib::types::SubSession, arborist_lib::types::AppError> {
    subsession_create_impl(
        &h.ctx,
        &h.sub_ctx,
        SubSessionCreateArgs {
            parent_session_id: parent,
            def_id: h.shell_def_id.clone(),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cascade_kills_terminal_subs_and_prunes_persistence() {
    let h = build_harness();
    let parent = create_parent(&h).id;
    let sub_a = create_sub(&h, parent).expect("sub a created");
    let sub_b = create_sub(&h, parent).expect("sub b created");

    assert!(wait_until(
        || h.sub_pool.contains(&sub_a.id),
        Duration::from_secs(2)
    ));
    assert!(wait_until(
        || h.sub_pool.contains(&sub_b.id),
        Duration::from_secs(2)
    ));
    assert_eq!(h.sub_ctx.store.list_for(&parent).len(), 2);
    assert_eq!(h.ctx.store.load_config().last_open_sub_sessions.len(), 2);

    // Cascade and verify both sub-sessions are gone everywhere.
    close_for_parent_impl(&h.ctx, &h.sub_ctx, parent).await;

    assert!(!h.sub_pool.contains(&sub_a.id), "sub_a still in pool");
    assert!(!h.sub_pool.contains(&sub_b.id), "sub_b still in pool");
    assert!(
        h.sub_ctx.store.list_for(&parent).is_empty(),
        "store not pruned"
    );
    assert!(
        h.ctx.store.load_config().last_open_sub_sessions.is_empty(),
        "persistence not pruned"
    );
}

#[tokio::test]
async fn session_close_cascades_subs_via_tombstone() {
    let h = build_harness();
    let parent = create_parent(&h).id;
    let sub = create_sub(&h, parent).expect("sub created");
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));

    // Mark closing, cascade, close — mirrors the wrapper in commands/mod.rs.
    {
        let _guard = h.ctx.mark_parent_closing(parent);
        assert!(h.ctx.is_parent_closing(&parent));
        // While the tombstone is set, new sub-creates must be rejected.
        let blocked = create_sub(&h, parent);
        assert!(blocked.is_err(), "tombstone should reject new subs");
        let err = blocked.err().unwrap();
        assert_eq!(err.code, "InvalidArgument");

        close_for_parent_impl(&h.ctx, &h.sub_ctx, parent).await;
        session_close_impl(&h.ctx, parent, false)
            .await
            .expect("session close ok");
    }

    // Guard dropped: tombstone clears, parent + sub all gone.
    assert!(!h.ctx.is_parent_closing(&parent));
    assert!(h.sub_ctx.store.list_for(&parent).is_empty());
    assert!(h.ctx.store.load_config().last_open_sub_sessions.is_empty());
    assert!(!h.ctx.store.load_sessions().contains_key(&parent));
}

#[tokio::test]
async fn restore_drops_orphan_records_when_parent_is_gone() {
    let h = build_harness();
    let parent = create_parent(&h).id;
    let sub = create_sub(&h, parent).expect("sub created");
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));

    // Close the parent (cascade first so we mirror the real wrapper).
    close_for_parent_impl(&h.ctx, &h.sub_ctx, parent).await;
    session_close_impl(&h.ctx, parent, false)
        .await
        .expect("session close ok");

    // Manually re-add an orphan record (simulates a crash/rollback that
    // left a sub persisted under a now-gone parent).
    let orphan = arborist_lib::types::SubSessionRecord {
        id: arborist_lib::types::SubSessionId::default(),
        parent_session_id: parent,
        def_id: h.shell_def_id.clone(),
        kind: CustomProcessKind::Terminal,
        label: "Shell".into(),
        composed_command: "sh -i".into(),
    };
    let orphan_id = orphan.id;
    h.ctx.store.append_last_open_sub_session(orphan).unwrap();
    assert_eq!(h.ctx.store.load_config().last_open_sub_sessions.len(), 1);

    restore_all_sub_sessions_impl(&h.ctx, &h.sub_ctx);

    assert!(
        h.ctx.store.load_config().last_open_sub_sessions.is_empty(),
        "orphan should have been dropped from persistence"
    );
    assert!(
        h.sub_ctx.store.get(&orphan_id).is_none(),
        "orphan should not appear in store"
    );
    assert!(
        h.sub_events.restored.lock().unwrap().is_empty(),
        "no restored event should fire for the orphan"
    );
}

#[tokio::test]
async fn restore_respawns_terminal_subs_under_extant_parent() {
    let h = build_harness();
    let parent = create_parent(&h).id;
    let sub = create_sub(&h, parent).expect("sub created");
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));

    // Simulate a fresh app launch: drop the in-memory store + pool but
    // keep persistence (mirrors what happens between runs of the app).
    h.sub_ctx.store.remove(&sub.id);
    h.sub_pool.kill(&sub.id).await.ok();

    // Stable id: the persisted record still references it.
    let persisted = h.ctx.store.load_config().last_open_sub_sessions;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].id, sub.id);

    restore_all_sub_sessions_impl(&h.ctx, &h.sub_ctx);

    // restored event was emitted for the row.
    let restored_evs = h.sub_events.restored.lock().unwrap().clone();
    assert_eq!(restored_evs.len(), 1, "expected one restored event");
    assert_eq!(restored_evs[0].id, sub.id);
    assert_eq!(restored_evs[0].kind, CustomProcessKind::Terminal);
    // Status is Starting at restore time; the pool will emit Running
    // shortly after the spawn — we just check the row is in the store.
    assert!(h.sub_ctx.store.get(&sub.id).is_some());
    // Pool repopulated: a new spawn happened under the same id.
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));
}

#[tokio::test]
async fn restore_rejects_records_under_closing_parent() {
    let h = build_harness();
    let parent = create_parent(&h).id;
    let sub = create_sub(&h, parent).expect("sub created");
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));

    // Drop the in-memory store but keep persistence (mirrors restart).
    h.sub_ctx.store.remove(&sub.id);
    h.sub_pool.kill(&sub.id).await.ok();

    let _guard = h.ctx.mark_parent_closing(parent);
    restore_all_sub_sessions_impl(&h.ctx, &h.sub_ctx);

    assert!(
        h.ctx.store.load_config().last_open_sub_sessions.is_empty(),
        "records under closing parent must be pruned"
    );
    assert!(
        h.sub_ctx.store.get(&sub.id).is_none(),
        "no row should be inserted under a closing parent"
    );
}

#[tokio::test]
async fn relaunch_swaps_terminal_pty_under_same_id() {
    let h = build_harness();
    let parent = create_parent(&h).id;
    let sub = create_sub(&h, parent).expect("sub created");
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));

    let returned = subsession_relaunch_impl(&h.ctx, &h.sub_ctx, sub.id)
        .await
        .expect("relaunch ok");

    assert_eq!(returned.id, sub.id, "id must be stable across relaunch");
    // The kill path EOFs the old reader; the spawn path inserts a new
    // entry under the same id. Both observable as `contains`.
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));
    // Persistence row still present and points at the same id.
    let persisted = h.ctx.store.load_config().last_open_sub_sessions;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].id, sub.id);
}

#[tokio::test]
async fn relaunch_rejects_when_def_was_deleted() {
    let h = build_harness();
    let parent = create_parent(&h).id;
    let sub = create_sub(&h, parent).expect("sub created");
    assert!(wait_until(
        || h.sub_pool.contains(&sub.id),
        Duration::from_secs(2)
    ));

    // User deletes the def via Settings. Persisted sub record still
    // references the gone def id.
    h.ctx
        .store
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
async fn create_under_closing_parent_is_rejected() {
    let h = build_harness();
    let parent = create_parent(&h).id;

    let _guard = h.ctx.mark_parent_closing(parent);
    let result = create_sub(&h, parent);
    assert!(result.is_err());
    assert_eq!(result.err().unwrap().code, "InvalidArgument");
}
