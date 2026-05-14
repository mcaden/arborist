//! Phase 6 integration tests for the PTY pool.
//!
//! These tests exercise both the production [`PortablePtySpawner`] (against the purpose-built `arborist-test-child` binary) and a deterministic fake
//! spawner for backpressure / lifecycle / UTF-8 correctness.
//!
//! The path to the test child binary is provided automatically by Cargo via `env!("CARGO_BIN_EXE_arborist-test-child")`. The binary requires the
//! `test-helpers` feature: `cargo test --workspace --features test-helpers`.

#![cfg(feature = "test-helpers")]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arborist_lib::compose::{copilot_otel_path, session_temp_dir};
use arborist_lib::pty_pool::{
    cleanup_orphans, ChildCommand, PortablePtySpawner, PtyKiller, PtyPool, PtyResize, PtySink, PtySpawner, PtyWaiter, SpawnedChild, ANSI_FULL_RESET,
    DEFAULT_PTY_SIZE, OUTPUT_CHANNEL_CAPACITY,
};
use arborist_lib::session_temp::{ensure_session_temp_dir, prepare_copilot_otel_file, remove_copilot_otel_file, remove_session_temp_dir};
use arborist_lib::types::{Session, SessionId, SessionStatus, TempFileSpec, Tool};
use portable_pty::{ExitStatus, PtySize};
use uuid::Uuid;

// --------------------------------------------------------------------------- Shared test helpers
// ---------------------------------------------------------------------------

const TEST_CHILD_PATH: &str = env!("CARGO_BIN_EXE_arborist-test-child");

/// Build a minimal `Session` whose `composed_command` runs the test child.
///
/// On Windows the platform shell is `cmd.exe /c "<command>"` which strips surrounding quotes, so we pass the full path through the same quoting path
/// as the production composer would.
fn make_session(workdir: &Path) -> Session {
    let composed = quote_program(TEST_CHILD_PATH);
    Session {
        id: SessionId::new(),
        tool: Tool::Claude,
        worktree_path: workdir.to_path_buf(),
        worktree_name: "test".into(),
        label: "test".into(),
        composed_command: composed,
        status: SessionStatus::Starting,
        pid: None,
        created_at: 0,
        tab_index: 0,
        temp_files: Vec::new(),
        ai_session_id: None,
    }
}

#[cfg(windows)]
fn quote_program(p: &str) -> String {
    // cmd.exe /c parses the command string with its own rules. The portable-pty CommandBuilder already builds a properly escaped CreateProcess line
    // with `cmd.exe /c "<our-string>"`, so what we hand back here must be a valid cmd-shell expression. Wrap the path in double quotes and use cmd's
    // `^` escape on inner quotes (the test child path comes from Cargo, never has them, but we wrap for the "path may contain spaces" case).
    if p.contains(' ') {
        format!("\"{p}\"")
    } else {
        p.to_owned()
    }
}

#[cfg(not(windows))]
fn quote_program(p: &str) -> String {
    arborist_lib::compose::shell_quote_posix(p)
}

#[cfg(windows)]
fn parse_grandchild_pid(output: &str) -> Option<u32> {
    let marker = "ARBORIST-TEST-CHILD GRANDCHILD ";
    let (_, rest) = output.split_once(marker)?;
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

#[cfg(windows)]
type Handle = *mut std::ffi::c_void;
#[cfg(windows)]
const PROCESS_TERMINATE: u32 = 0x0001;
#[cfg(windows)]
const SYNCHRONIZE: u32 = 0x0010_0000;
#[cfg(windows)]
const WAIT_TIMEOUT: u32 = 0x0000_0102;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn TerminateProcess(process: Handle, exit_code: u32) -> i32;
    fn CloseHandle(object: Handle) -> i32;
}

#[cfg(windows)]
struct ProcessCleanupGuard {
    handle: Handle,
}

#[cfg(windows)]
impl ProcessCleanupGuard {
    fn new(pid: u32) -> Option<Self> {
        // SAFETY: OpenProcess accepts stale PIDs and returns NULL; no pointers are dereferenced.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            None
        } else {
            Some(Self { handle })
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessCleanupGuard {
    fn drop(&mut self) {
        // SAFETY: handle was opened while the child PID was known to be alive and is closed exactly once here. Keeping the handle avoids PID-reuse
        // cleanup hazards if the test fails before the normal pool.kill path reaps the process.
        unsafe {
            TerminateProcess(self.handle, 1);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
fn is_pid_running(pid: u32) -> bool {
    // SAFETY: OpenProcess accepts stale PIDs and returns NULL; no pointers are dereferenced.
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: handle is a valid process handle opened for SYNCHRONIZE.
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: handle is valid and closed exactly once.
    unsafe {
        CloseHandle(handle);
    }
    wait == WAIT_TIMEOUT
}

/// Construct a `(sink, recordings)` pair where output and status updates are pushed into shared `Vec`s for inspection.
type OutputLog = Arc<Mutex<Vec<String>>>;
type StatusLog = Arc<Mutex<Vec<(SessionStatus, Option<u32>)>>>;

fn recording_sink() -> (PtySink, OutputLog, StatusLog) {
    let outs: OutputLog = Arc::new(Mutex::new(Vec::new()));
    let stats: StatusLog = Arc::new(Mutex::new(Vec::new()));
    let outs_cb = Arc::clone(&outs);
    let stats_cb = Arc::clone(&stats);
    let sink = PtySink::new(
        Arc::new(move |_id, chunk| {
            outs_cb.lock().unwrap().push(chunk);
        }),
        Arc::new(move |_id, status, pid, _msg| {
            stats_cb.lock().unwrap().push((status, pid));
        }),
        Arc::new(|_id, _evt| {}),
    );
    (sink, outs, stats)
}

/// Block (with a budget) until `pred(joined_output)` is true. Returns the joined output on success.
fn wait_for<F: FnMut(&str) -> bool>(log: &OutputLog, mut pred: F, budget: Duration) -> Option<String> {
    let start = Instant::now();
    loop {
        let joined = log.lock().unwrap().concat();
        if pred(&joined) {
            return Some(joined);
        }
        if start.elapsed() > budget {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_status<F: Fn(&[(SessionStatus, Option<u32>)]) -> bool>(log: &StatusLog, pred: F, budget: Duration) -> bool {
    let start = Instant::now();
    loop {
        if pred(&log.lock().unwrap()) {
            return true;
        }
        if start.elapsed() > budget {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

// --------------------------------------------------------------------------- Real-spawner end-to-end tests
//
// Every test in this section drives a real ConPTY/portable-pty child. Spawning many ConPTY consoles in parallel is contended on Windows: under load
// (e.g. the husky pre-push hook running `npm test` and `cargo test --workspace` concurrently with linker work), the initial banner from the test
// child can stall well past the per-test 5s budget, causing intermittent `banner not seen` failures. Force these tests to run serially so they only
// ever compete with one PTY at a time. ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(real_pty)]
fn spawn_banner_then_quit_yields_exited_status() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let pool = PtyPool::new(Arc::new(PortablePtySpawner::new()));
    let (sink, outs, stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    let pid = pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");
    assert!(pid > 0);

    assert!(
        wait_for(&outs, |s| s.contains("ARBORIST-TEST-CHILD READY"), Duration::from_secs(5)).is_some(),
        "banner not seen: {:?}",
        outs.lock().unwrap()
    );

    pool.write(&session.id, b"quit\r\n").expect("write quit");
    let exited = wait_for_status(
        &stats,
        |s| s.iter().any(|(st, _)| matches!(st, SessionStatus::Exited)),
        Duration::from_secs(5),
    );
    if !exited {
        let outs_dump = outs.lock().unwrap().clone();
        let stats_dump = stats.lock().unwrap().clone();
        panic!("no Exited status; stats={stats_dump:?}; outs={outs_dump:?}");
    }

    rt.block_on(async {
        // Drain any lingering tasks.
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
}

#[test]
#[serial_test::serial(real_pty)]
fn echoes_input_back_through_sink() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let pool = PtyPool::new(Arc::new(PortablePtySpawner::new()));
    let (sink, outs, _stats) = recording_sink();

    let _rt = rt();
    let _g = _rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");
    wait_for(&outs, |s| s.contains("READY"), Duration::from_secs(5)).expect("ready");
    pool.write(&session.id, b"hello\r\n").expect("write");
    assert!(
        wait_for(&outs, |s| s.contains("echo: hello"), Duration::from_secs(5)).is_some(),
        "echo not seen: {:?}",
        outs.lock().unwrap()
    );
    pool.write(&session.id, b"quit\r\n").ok();
}

#[test]
#[serial_test::serial(real_pty)]
fn resize_calls_do_not_disrupt_io() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let pool = PtyPool::new(Arc::new(PortablePtySpawner::new()));
    let (sink, outs, _stats) = recording_sink();

    let _rt = rt();
    let _g = _rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");
    wait_for(&outs, |s| s.contains("READY"), Duration::from_secs(5)).expect("ready");
    pool.resize(&session.id, 100, 30).expect("resize");
    pool.resize(&session.id, 80, 24).expect("resize");
    pool.resize(&session.id, 200, 50).expect("resize");
    pool.write(&session.id, b"hello\r\n").expect("write");
    assert!(
        wait_for(&outs, |s| s.contains("echo: hello"), Duration::from_secs(5)).is_some(),
        "echo not seen after resizes: {:?}",
        outs.lock().unwrap()
    );
    pool.write(&session.id, b"quit\r\n").ok();
}

#[test]
#[serial_test::serial(real_pty)]
fn nonzero_exit_yields_error_status() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let pool = PtyPool::new(Arc::new(PortablePtySpawner::new()));
    let (sink, outs, stats) = recording_sink();

    let _rt = rt();
    let _g = _rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");
    wait_for(&outs, |s| s.contains("READY"), Duration::from_secs(5)).expect("ready");
    pool.write(&session.id, b"exit 7\r\n").expect("write");
    assert!(
        wait_for_status(
            &stats,
            |s| s.iter().any(|(st, pid)| matches!(st, SessionStatus::Error) && pid.is_none()),
            Duration::from_secs(5)
        ),
        "no Error status: {:?}",
        stats.lock().unwrap()
    );
}

#[test]
#[serial_test::serial(real_pty)]
fn kill_terminates_child_and_removes_entry_and_temp_dir() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let pool = PtyPool::new(Arc::new(PortablePtySpawner::new()));
    let (sink, outs, _stats) = recording_sink();

    // Pre-create the temp dir so kill has something to delete.
    let temp = session_temp_dir(&session.id);
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::write(temp.join("system-prompt.md"), b"hello").unwrap();
    assert!(temp.exists());

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");
    wait_for(&outs, |s| s.contains("READY"), Duration::from_secs(5)).expect("ready");

    rt.block_on(async {
        pool.kill(&session.id).await.expect("kill");
    });

    assert!(!pool.contains(&session.id));
    assert!(!temp.exists(), "temp dir not deleted: {}", temp.display());
}

#[test]
#[serial_test::serial(real_pty)]
fn respawn_existing_yields_a_new_pid() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let pool = PtyPool::new(Arc::new(PortablePtySpawner::new()));
    let (sink, outs, _stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    let pid1 = pool.spawn(&session, sink.clone(), DEFAULT_PTY_SIZE).expect("spawn 1");
    wait_for(&outs, |s| s.contains("READY"), Duration::from_secs(5)).expect("ready 1");
    rt.block_on(async { pool.kill(&session.id).await.expect("kill") });

    let (sink2, outs2, _stats2) = recording_sink();
    let pid2 = pool.respawn_existing(&session, sink2, DEFAULT_PTY_SIZE).expect("respawn");
    assert_ne!(pid1, pid2, "respawn should yield a new pid");
    assert!(
        wait_for(&outs2, |s| s.contains("READY"), Duration::from_secs(5)).is_some(),
        "no banner after respawn: {:?}",
        outs2.lock().unwrap()
    );
    pool.write(&session.id, b"quit\r\n").ok();
}

// --------------------------------------------------------------------------- Pool spawn-prep (env injection / temp-dir setup / stale-otel
// truncation) ---------------------------------------------------------------------------

fn make_copilot_session(workdir: &Path) -> Session {
    Session {
        id: SessionId::new(),
        tool: Tool::Copilot,
        worktree_path: workdir.to_path_buf(),
        worktree_name: "test".into(),
        label: "test".into(),
        // Composed command can be anything — the FakeSpawner doesn't run it. We just need pool.spawn to reach the spawner with the env populated.
        composed_command: "true".into(),
        status: SessionStatus::Starting,
        pid: None,
        created_at: 0,
        tab_index: 0,
        temp_files: Vec::new(),
        ai_session_id: None,
    }
}

#[test]
fn pool_spawn_prep_injects_otel_env_and_resets_stale_file_for_copilot() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_copilot_session(dir.path());

    // Pre-create the deterministic temp dir + a stale OTel JSONL from a hypothetical previous run. The pool must wipe it before spawn so the watcher
    // doesn't replay old totals.
    let temp = session_temp_dir(&session.id);
    std::fs::create_dir_all(&temp).unwrap();
    let stale = copilot_otel_path(&session.id);
    std::fs::write(&stale, b"stale-data\n").unwrap();
    assert!(stale.exists(), "precondition: stale file present");

    let spawner = Arc::new(FakeSpawner::new(FakeMode::Parked));
    let spawner_for_assert = Arc::clone(&spawner);
    let pool = PtyPool::new(spawner);
    let (sink, _outs, _stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");

    // Assert: the spawner saw a ChildCommand with the three OTel env keys.
    let cmd = spawner_for_assert.last_cmd.lock().unwrap().clone().expect("spawner received a command");
    let env: std::collections::HashMap<String, std::ffi::OsString> = cmd.env.into_iter().collect();
    let expected_path = copilot_otel_path(&session.id).into_os_string();
    assert_eq!(env.get("COPILOT_OTEL_FILE_EXPORTER_PATH"), Some(&expected_path),);
    assert_eq!(env.get("COPILOT_OTEL_ENABLED"), Some(&std::ffi::OsString::from("true")));
    assert_eq!(env.get("OTEL_BSP_SCHEDULE_DELAY"), Some(&std::ffi::OsString::from("1000")));

    // Assert: the temp dir still exists, and the stale file was reset to an empty owner-only exporter target.
    assert!(temp.exists(), "session temp dir must exist after prep");
    assert!(stale.exists(), "otel.jsonl must be recreated before spawn");
    assert_eq!(std::fs::read(&stale).unwrap(), b"", "stale otel.jsonl must be empty before spawn");

    rt.block_on(async {
        pool.kill(&session.id).await.ok();
    });
    // Post-kill cleanup: pool.kill removes session_temp_dir wholesale, so an otel.jsonl that the (real) child would have written goes with it. This
    // is the Copilot equivalent of the system-prompt.md cleanup covered by `kill_terminates_child_and_removes_entry_and_temp_dir`, and is the
    // regression assertion for the temp-cleanup-verify todo.
    assert!(!temp.exists(), "session_temp_dir must be removed by kill: {}", temp.display(),);
}

#[cfg(unix)]
#[test]
fn prepare_copilot_otel_file_uses_owner_only_unix_modes() {
    use std::os::unix::fs::PermissionsExt;

    let id = SessionId::new();
    let path = prepare_copilot_otel_file(&id).expect("prepare otel file");
    let root = arborist_lib::compose::session_temp_root();
    let dir = session_temp_dir(&id);

    assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    remove_session_temp_dir(&id).expect("cleanup");
}

#[test]
fn prepare_copilot_otel_file_refuses_symlinked_otel_path() {
    let id = SessionId::new();
    let dir = ensure_session_temp_dir(&id).expect("session temp dir");
    let otel = copilot_otel_path(&id);
    let victim_dir = tempfile::tempdir().unwrap();
    let victim = victim_dir.path().join("victim.jsonl");
    std::fs::write(&victim, b"do-not-touch").unwrap();

    if !symlink_file_or_skip(&victim, &otel) {
        remove_session_temp_dir(&id).ok();
        return;
    }

    let err = prepare_copilot_otel_file(&id).expect_err("symlinked otel path must be refused");
    assert!(format!("{err}").contains("refusing"), "unexpected error: {err}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"do-not-touch");

    remove_file_symlink(&otel);
    remove_session_temp_dir(&id).expect("cleanup session temp dir");
    assert!(!dir.exists(), "cleanup should remove the now-empty session temp dir");
}

#[test]
fn remove_copilot_otel_file_refuses_symlinked_session_temp_dir() {
    let anchor = SessionId::new();
    ensure_session_temp_dir(&anchor).expect("create temp root");
    remove_session_temp_dir(&anchor).expect("remove anchor dir");

    let id = SessionId::new();
    let link = session_temp_dir(&id);
    let victim = tempfile::tempdir().unwrap();
    let victim_file = victim.path().join("otel.jsonl");
    std::fs::write(&victim_file, b"do-not-touch").unwrap();

    if !symlink_dir_or_skip(victim.path(), &link) {
        return;
    }

    let err = remove_copilot_otel_file(&id).expect_err("symlinked session temp dir must be refused");
    assert!(format!("{err}").contains("refusing"), "unexpected error: {err}");
    assert_eq!(std::fs::read(&victim_file).unwrap(), b"do-not-touch");

    remove_dir_symlink(&link);
}

#[test]
fn pool_spawn_prep_is_noop_for_claude_session() {
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());

    // Confirm the temp dir does NOT exist before spawn — the pool should not create one for Claude sessions that have no compose-time temp files
    // (matches today's `materialise_temp_files`-only behaviour).
    let temp = session_temp_dir(&session.id);
    assert!(!temp.exists(), "precondition: no Claude temp dir");

    let spawner = Arc::new(FakeSpawner::new(FakeMode::Parked));
    let spawner_for_assert = Arc::clone(&spawner);
    let pool = PtyPool::new(spawner);
    let (sink, _outs, _stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");

    let cmd = spawner_for_assert.last_cmd.lock().unwrap().clone().expect("spawner received a command");
    assert!(cmd.env.is_empty(), "Claude must not get any extra env");
    assert!(!temp.exists(), "Claude spawn must not create the OTel temp dir");

    rt.block_on(async {
        pool.kill(&session.id).await.ok();
    });
}

// --------------------------------------------------------------------------- Fake spawner for deterministic lifecycle / backpressure / UTF-8 tests
// ---------------------------------------------------------------------------

/// Reader that yields a fixed sequence of byte chunks then EOFs.
struct ScriptedReader {
    chunks: std::vec::IntoIter<Vec<u8>>,
    pause: Duration,
}

impl Read for ScriptedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.chunks.next() {
            Some(c) => {
                let n = c.len().min(buf.len());
                buf[..n].copy_from_slice(&c[..n]);
                if !self.pause.is_zero() {
                    std::thread::sleep(self.pause);
                }
                Ok(n)
            }
            None => Ok(0),
        }
    }
}

/// Reader controlled by a flag — blocks (sleeping) until `eof` flips, then returns 0. Used to keep the wait thread parked while we test the pool's
/// runtime behaviour.
struct ParkedReader {
    eof: Arc<AtomicBool>,
}

impl Read for ParkedReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        while !self.eof.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(0)
    }
}

struct FakeKiller {
    eof_flag: Arc<AtomicBool>,
    /// When true, `kill()` returns an `Err` even though it still flips the eof flag so the reader/waiter unblock. Used by the
    /// `kill_returns_unconfirmed_when_killer_errors` regression test to drive the `KillOutcome::Unconfirmed` branch deterministically without waiting
    /// out `KILL_GRACE`.
    fail: bool,
}

impl PtyKiller for FakeKiller {
    fn kill(&self) -> Result<(), arborist_lib::types::Error> {
        self.eof_flag.store(true, Ordering::Relaxed);
        if self.fail {
            Err(arborist_lib::types::Error::PtyKillFailed("simulated kill failure".into()))
        } else {
            Ok(())
        }
    }
}

struct FakeResize;
impl PtyResize for FakeResize {
    fn resize(&self, _cols: u16, _rows: u16) -> Result<(), arborist_lib::types::Error> {
        Ok(())
    }
}

struct FakeWaiter {
    eof_flag: Arc<AtomicBool>,
    exit_code: u32,
    auto_exit_after: Option<Duration>,
}

impl PtyWaiter for FakeWaiter {
    fn wait(self: Box<Self>) -> Result<ExitStatus, arborist_lib::types::Error> {
        if let Some(d) = self.auto_exit_after {
            std::thread::sleep(d);
            self.eof_flag.store(true, Ordering::Relaxed);
            return Ok(ExitStatus::with_exit_code(self.exit_code));
        }
        // Block until the kill flag flips.
        while !self.eof_flag.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(ExitStatus::with_exit_code(self.exit_code))
    }
}

#[derive(Clone)]
enum FakeMode {
    /// Use a scripted reader with the given chunks, sleeping `pause` between.
    Scripted { chunks: Vec<Vec<u8>>, pause: Duration, exit_code: u32 },
    /// Use a parked reader that never returns until killed.
    Parked,
    /// Park reader; auto-exit waiter after `delay`.
    AutoExit { delay: Duration, exit_code: u32 },
    /// Parked reader, but `killer.kill()` returns `Err`. Used to exercise the `KillOutcome::Unconfirmed` branch of `pool.kill`.
    ParkedKillFails,
}

struct FakeSpawner {
    next_pid: AtomicUsize,
    mode: Mutex<FakeMode>,
    last_eof: Mutex<Option<Arc<AtomicBool>>>,
    last_cwd: Mutex<Option<PathBuf>>,
    last_cmd: Mutex<Option<ChildCommand>>,
}

impl FakeSpawner {
    fn new(mode: FakeMode) -> Self {
        Self {
            next_pid: AtomicUsize::new(1000),
            mode: Mutex::new(mode),
            last_eof: Mutex::new(None),
            last_cwd: Mutex::new(None),
            last_cmd: Mutex::new(None),
        }
    }
}

impl PtySpawner for FakeSpawner {
    fn spawn(&self, cmd: ChildCommand, cwd: &Path, _size: PtySize) -> Result<SpawnedChild, arborist_lib::types::Error> {
        *self.last_cwd.lock().unwrap() = Some(cwd.to_path_buf());
        *self.last_cmd.lock().unwrap() = Some(cmd);
        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed) as u32;
        let eof = Arc::new(AtomicBool::new(false));
        *self.last_eof.lock().unwrap() = Some(Arc::clone(&eof));

        let mode = self.mode.lock().unwrap().clone();
        let (reader, exit_code, auto_exit, fail_kill): (Box<dyn Read + Send>, u32, Option<Duration>, bool) = match mode {
            FakeMode::Scripted { chunks, pause, exit_code } => {
                let r = ScriptedReader {
                    chunks: chunks.into_iter(),
                    pause,
                };
                (Box::new(r), exit_code, None, false)
            }
            FakeMode::Parked => (Box::new(ParkedReader { eof: Arc::clone(&eof) }), 0, None, false),
            FakeMode::AutoExit { delay, exit_code } => (Box::new(ParkedReader { eof: Arc::clone(&eof) }), exit_code, Some(delay), false),
            FakeMode::ParkedKillFails => (Box::new(ParkedReader { eof: Arc::clone(&eof) }), 0, None, true),
        };

        Ok(SpawnedChild {
            pid,
            reader,
            writer: Box::new(std::io::sink()),
            resize: Arc::new(FakeResize),
            waiter: Box::new(FakeWaiter {
                eof_flag: Arc::clone(&eof),
                exit_code,
                auto_exit_after: auto_exit,
            }),
            killer: Arc::new(FakeKiller {
                eof_flag: eof,
                fail: fail_kill,
            }),
        })
    }
}

// --------------------------------------------------------------------------- Backpressure
// ---------------------------------------------------------------------------

#[test]
fn backpressure_drops_chunks_and_inserts_reset_after_drain() {
    // A scripted reader that emits MANY tiny chunks faster than the test can drain them — we make the sink "stall" so the channel fills up.
    let chunks: Vec<Vec<u8>> = (0..2000).map(|i| format!("c{i}|").into_bytes()).collect();
    let spawner = Arc::new(FakeSpawner::new(FakeMode::Scripted {
        chunks,
        // Small pause so the producer is still emitting after the consumer is released — otherwise all chunks are produced before the consumer
        // unblocks and there's no chunk left to carry the ESC-c reset.
        pause: Duration::from_micros(500),
        exit_code: 0,
    }));
    let pool = PtyPool::new(spawner);

    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());

    // A "stalling" sink that holds onto a Mutex while pretending to write. We block the consumer for ~200 ms so the channel saturates.
    let outs: OutputLog = Arc::new(Mutex::new(Vec::new()));
    let allow = Arc::new(AtomicBool::new(false));
    let outs_cb = Arc::clone(&outs);
    let allow_cb = Arc::clone(&allow);
    let sink = PtySink::new(
        Arc::new(move |_id, chunk| {
            while !allow_cb.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            outs_cb.lock().unwrap().push(chunk);
        }),
        Arc::new(|_id, _status, _pid, _msg| {}),
        Arc::new(|_id, _evt| {}),
    );

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");

    // Wait for the read thread to finish producing AND for the channel to fill up. The channel cap is 512; 2000 chunks > 512 so drops must occur.
    let dropped = pool.dropped_chunks(&session.id).expect("counter");
    let start = Instant::now();
    while dropped.load(Ordering::Relaxed) == 0 {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("no drops after 5s; dropped=0");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let n_dropped = dropped.load(Ordering::Relaxed);
    assert!(n_dropped > 0, "expected drops, got {n_dropped}");
    // The bounded channel cap is 512; channel can never grow beyond that. We can't directly inspect length, but we can assert producer + consumer
    // didn't deadlock by completing the test.

    // Now release the consumer. The next emitted chunk must be ESC-c prefixed.
    allow.store(true, Ordering::Relaxed);
    rt.block_on(async {
        // Wait for everything to flush.
        tokio::time::sleep(Duration::from_millis(300)).await;
    });
    let joined = outs.lock().unwrap().concat();
    assert!(
        joined.contains(ANSI_FULL_RESET),
        "expected ESC-c reset prefix in output; first 200 chars = {:?}",
        &joined.chars().take(200).collect::<String>()
    );

    // Also: the channel cap is exactly OUTPUT_CHANNEL_CAPACITY; verify the const hasn't drifted.
    assert_eq!(OUTPUT_CHANNEL_CAPACITY, 512);

    // Trigger the wait thread to end so the test cleanly exits.
    rt.block_on(async {
        pool.kill(&session.id).await.ok();
    });
}

// --------------------------------------------------------------------------- Late-output suppression after kill
// ---------------------------------------------------------------------------

#[test]
fn no_output_delivered_after_kill_returns() {
    // Scripted reader with one chunk so we know the read thread has emitted before kill.
    let spawner = Arc::new(FakeSpawner::new(FakeMode::Parked));
    let pool = PtyPool::new(spawner);

    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());

    let count = Arc::new(AtomicUsize::new(0));
    let count_cb = Arc::clone(&count);
    let sink = PtySink::new(
        Arc::new(move |_id, _chunk| {
            count_cb.fetch_add(1, Ordering::Relaxed);
        }),
        Arc::new(|_id, _status, _pid, _msg| {}),
        Arc::new(|_id, _evt| {}),
    );

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");
    rt.block_on(async {
        pool.kill(&session.id).await.expect("kill");
    });
    let after = count.load(Ordering::Relaxed);
    // Sleep then re-check; nothing should have been delivered after kill returned.
    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    assert_eq!(count.load(Ordering::Relaxed), after);
    assert!(!pool.contains(&session.id));
}

// --------------------------------------------------------------------------- UTF-8 split across reads
// ---------------------------------------------------------------------------

#[test]
fn utf8_character_split_across_reads_emerges_intact() {
    // Send first 2 bytes of 世 (E4 B8 96) in chunk 1, third in chunk 2.
    let chunks = vec![vec![b'a', 0xE4, 0xB8], vec![0x96, b'b']];
    let spawner = Arc::new(FakeSpawner::new(FakeMode::Scripted {
        chunks,
        pause: Duration::from_millis(10),
        exit_code: 0,
    }));
    let pool = PtyPool::new(spawner);

    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let (sink, outs, stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");

    assert!(
        wait_for(&outs, |s| s.contains("a世b"), Duration::from_secs(3)).is_some(),
        "expected 'a世b' in concatenated output; got {:?}",
        outs.lock().unwrap()
    );
    let joined = outs.lock().unwrap().concat();
    assert!(!joined.contains('\u{FFFD}'), "no replacement char expected: {joined:?}");

    rt.block_on(async {
        pool.kill(&session.id).await.ok();
    });
    let _ = stats; // silence unused
}

// --------------------------------------------------------------------------- Wait-thread → status callback (sink-level — Phase 7 will wire
// persistence) ---------------------------------------------------------------------------

#[test]
fn wait_thread_emits_status_with_cleared_pid_on_natural_exit() {
    let spawner = Arc::new(FakeSpawner::new(FakeMode::AutoExit {
        delay: Duration::from_millis(50),
        exit_code: 0,
    }));
    let pool = PtyPool::new(spawner);
    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let (sink, _outs, stats) = recording_sink();
    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");
    assert!(
        wait_for_status(
            &stats,
            |s| s.iter().any(|(st, pid)| matches!(st, SessionStatus::Exited) && pid.is_none()),
            Duration::from_secs(3)
        ),
        "no Exited+pid:None status: {:?}",
        stats.lock().unwrap()
    );
}

// --------------------------------------------------------------------------- cleanup_orphans
// ---------------------------------------------------------------------------

#[test]
fn cleanup_orphans_deletes_only_unpersisted_stale_dirs() {
    // Plant three dirs under <os-temp>/arborist/:
    //   - young: <1h, NOT in persisted   → keep (too young)
    //   - persisted_old: >1h, IN persisted → keep (restore-safety)
    //   - orphan_old: >1h, NOT in persisted → delete

    let young_id = SessionId::new();
    let persisted_id = SessionId::new();
    let orphan_id = SessionId::new();

    let young = session_temp_dir(&young_id);
    let persisted = session_temp_dir(&persisted_id);
    let orphan = session_temp_dir(&orphan_id);

    for d in [&young, &persisted, &orphan] {
        if d.exists() {
            std::fs::remove_dir_all(d).ok();
        }
        std::fs::create_dir_all(d).unwrap();
    }
    // Set mtimes: young = now; the other two = 2h ago.
    let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 60 * 60);
    set_mtime(&persisted, two_hours_ago);
    set_mtime(&orphan, two_hours_ago);

    let deleted = cleanup_orphans(&[persisted_id]).expect("cleanup");

    assert!(young.exists(), "young dir was incorrectly deleted");
    assert!(persisted.exists(), "persisted-old dir was incorrectly deleted");
    assert!(!orphan.exists(), "orphan-old dir was not deleted");
    assert!(deleted >= 1, "deleted count was {deleted}");

    // Cleanup test fixtures.
    std::fs::remove_dir_all(&young).ok();
    std::fs::remove_dir_all(&persisted).ok();
}

#[test]
fn remove_session_temp_dir_refuses_symlink_child_and_preserves_target() {
    let id = SessionId::new();
    let dir = ensure_session_temp_dir(&id).expect("session temp dir");
    let victim = tempfile::tempdir().unwrap();
    std::fs::write(victim.path().join("keep.txt"), b"keep").unwrap();
    let link = dir.join("linked-victim");

    if !symlink_dir_or_skip(victim.path(), &link) {
        remove_session_temp_dir(&id).ok();
        return;
    }

    let err = remove_session_temp_dir(&id).expect_err("session temp cleanup must refuse symlink children");
    assert!(format!("{err}").contains("refusing"), "unexpected error: {err}");
    assert!(
        victim.path().join("keep.txt").exists(),
        "cleanup followed a symlink and touched the victim"
    );

    remove_dir_symlink(&link);
    remove_session_temp_dir(&id).expect("cleanup after removing symlink");
}

#[test]
fn cleanup_orphans_skips_uuid_symlink_and_preserves_target() {
    let anchor = SessionId::new();
    ensure_session_temp_dir(&anchor).expect("create root");
    remove_session_temp_dir(&anchor).expect("remove anchor");

    let link_id = SessionId::new();
    let link = session_temp_dir(&link_id);
    let victim = tempfile::tempdir().unwrap();
    std::fs::write(victim.path().join("keep.txt"), b"keep").unwrap();

    if !symlink_dir_or_skip(victim.path(), &link) {
        return;
    }

    let _deleted = cleanup_orphans(&[]).expect("cleanup orphans");
    assert!(victim.path().join("keep.txt").exists(), "orphan cleanup followed a UUID symlink");
    assert!(link.exists(), "UUID symlink should be skipped, not removed");

    remove_dir_symlink(&link);
}

#[cfg(unix)]
fn symlink_file_or_skip(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).expect("create file symlink");
    true
}

#[cfg(windows)]
fn symlink_file_or_skip(target: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => true,
        Err(e) if is_windows_symlink_privilege_error(&e) => false,
        Err(e) => panic!("create file symlink: {e}"),
    }
}

fn remove_file_symlink(link: &Path) {
    std::fs::remove_file(link).expect("remove file symlink");
}

#[cfg(unix)]
fn symlink_dir_or_skip(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).expect("create dir symlink");
    true
}

#[cfg(windows)]
fn symlink_dir_or_skip(target: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => true,
        Err(e) if is_windows_symlink_privilege_error(&e) => false,
        Err(e) => panic!("create dir symlink: {e}"),
    }
}

#[cfg(windows)]
fn is_windows_symlink_privilege_error(e: &std::io::Error) -> bool {
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD)
}

#[cfg(unix)]
fn remove_dir_symlink(link: &Path) {
    std::fs::remove_file(link).expect("remove dir symlink");
}

#[cfg(windows)]
fn remove_dir_symlink(link: &Path) {
    std::fs::remove_dir(link).expect("remove dir symlink");
}

#[cfg(windows)]
fn set_mtime(path: &Path, when: std::time::SystemTime) {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_BACKUP_SEMANTICS lets us open a directory handle.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap();
    let times = std::fs::FileTimes::new().set_modified(when).set_accessed(when);
    f.set_times(times).unwrap();
}

#[cfg(not(windows))]
fn set_mtime(path: &Path, when: std::time::SystemTime) {
    let f = std::fs::OpenOptions::new().read(true).open(path).unwrap();
    let times = std::fs::FileTimes::new().set_modified(when).set_accessed(when);
    f.set_times(times).unwrap();
}

// Sanity: ensure the test child path constant points at something on disk.
#[test]
fn test_child_binary_exists() {
    assert!(Path::new(TEST_CHILD_PATH).exists(), "missing: {TEST_CHILD_PATH}");
}

// Sanity: ensure SessionId helpers are usable in tests too.
#[test]
fn session_id_new_yields_unique_uuids() {
    let a = SessionId::new();
    let b = SessionId::new();
    assert_ne!(a.0, Uuid::nil());
    assert_ne!(a, b);
}

// --------------------------------------------------------------------------- KillOutcome (PR #32 round-12 review): pool.kill must distinguish
// "process reaped" from "kill issued but reap unconfirmed" so callers like park_session_for_switch_impl can log a possible orphan PID instead of
// silently dropping the signal. ---------------------------------------------------------------------------

#[test]
fn kill_returns_reaped_on_clean_kill_and_join() {
    let spawner = Arc::new(FakeSpawner::new(FakeMode::Parked));
    let pool = PtyPool::new(spawner);

    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let (sink, _outs, _stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");

    let outcome = rt.block_on(async { pool.kill(&session.id).await.expect("kill") });
    assert_eq!(
        outcome,
        arborist_lib::pty_pool::KillOutcome::Reaped,
        "happy-path kill must report Reaped so callers know the OS reclaimed the process"
    );
    assert!(!pool.contains(&session.id));
}

#[test]
fn kill_returns_unconfirmed_when_killer_errors() {
    // Regression for PR #32 round-12 review finding: previously `pool.kill` did `let _ = rt.killer.kill()` and returned `Ok(())` even when the
    // OS-level kill primitive itself reported failure. `park_session_for_switch_impl` then proceeded as if the child had died — but the persisted
    // session record still said "live" and the next switch-back would respawn it as a SECOND live process for the same SessionId. The fix surfaces
    // the kill failure as `KillOutcome::Unconfirmed { pid }` so the caller can log the orphan PID for human cleanup.
    //
    // Here we drive the killer-error branch in isolation by flipping the FakeKiller's `fail` flag (the killer still nudges the eof flag so the
    // read/wait threads exit promptly, keeping the test fast — KILL_GRACE is 2s, which we don't want to wait through for every CI run).
    let spawner = Arc::new(FakeSpawner::new(FakeMode::ParkedKillFails));
    let pool = PtyPool::new(spawner);

    let dir = tempfile::tempdir().unwrap();
    let session = make_session(dir.path());
    let (sink, _outs, _stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    let pid = pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");

    let outcome = rt.block_on(async { pool.kill(&session.id).await.expect("kill") });

    match outcome {
        arborist_lib::pty_pool::KillOutcome::Unconfirmed { pid: reported_pid } => {
            assert_eq!(
                reported_pid, pid,
                "Unconfirmed must carry the recorded PID so the caller can log it for human cleanup"
            );
        }
        other => panic!("expected Unconfirmed when killer.kill() returns Err; got {:?}", other),
    }

    // Even on Unconfirmed, the runtime entry must be evicted so the SessionId is free for a fresh respawn (this matches the existing pool-eviction
    // contract that callers already rely on).
    assert!(!pool.contains(&session.id));
}

#[cfg(windows)]
#[test]
#[serial_test::serial(real_pty)]
fn kill_terminates_shell_descendants_on_windows() {
    let pool = PtyPool::new(Arc::new(PortablePtySpawner));
    let dir = tempfile::tempdir().unwrap();
    let mut session = make_session(dir.path());
    session.composed_command = format!("{} --spawn-grandchild", quote_program(TEST_CHILD_PATH));
    let (sink, outs, _stats) = recording_sink();

    let rt = rt();
    let _g = rt.enter();
    pool.spawn(&session, sink, DEFAULT_PTY_SIZE).expect("spawn");

    let Some(output) = wait_for(&outs, |out| parse_grandchild_pid(out).is_some(), Duration::from_secs(5)) else {
        let _ = rt.block_on(async { pool.kill(&session.id).await });
        panic!("test child did not report grandchild pid; output was {:?}", outs.lock().unwrap());
    };
    let grandchild_pid = parse_grandchild_pid(&output).expect("predicate already confirmed pid is present");
    assert!(
        is_pid_running(grandchild_pid),
        "grandchild pid {grandchild_pid} should be running before kill"
    );
    let _cleanup = ProcessCleanupGuard::new(grandchild_pid).expect("open cleanup handle for spawned grandchild");

    let outcome = rt.block_on(async { pool.kill(&session.id).await.expect("kill") });

    assert_eq!(
        outcome,
        arborist_lib::pty_pool::KillOutcome::Reaped,
        "Windows PTY kill should treat portable-pty's os-error-0 success as confirmed when the wait thread joins"
    );
    assert!(!is_pid_running(grandchild_pid), "grandchild pid {grandchild_pid} survived PTY kill");
}

// Silence unused-import lints when the platform-specific quoter selects only one of the two variants.
#[allow(dead_code)]
fn _silence_temp_file_spec(_: TempFileSpec) {}
