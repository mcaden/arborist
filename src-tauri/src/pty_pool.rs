//! Cross-platform PTY pool — Phase 6 of the implementation plan.
//!
//! Implements DESIGN §2.1 (PTY Pool), §5.1 step 2 + 7-9 (spawn / read thread / backpressure), §5.4 (restart from stored `composedCommand`), §5.6
//! (`cwd` is discrete, never interpolated), §8.3 (resource management).
//!
//! ## Architecture (reflecting the rules in `copilot-instructions.md`)
//!
//! - The pool is **Tauri-agnostic**. The only seam between the pool and the
//!   rest of the app is [`PtySink`], a pair of callbacks the caller supplies.
//! - The pool **never** constructs a production spawner. [`PtyPool::new`] takes
//!   `Arc<dyn PtySpawner>`. Production code wires the [`PortablePtySpawner`];
//!   tests wire a fake.
//! - One **OS thread** per session reads bytes from the PTY (per
//!   `copilot-instructions.md` — `portable-pty` reads block, so they cannot
//!   live on a tokio task).
//! - One **OS thread** per session blocks in `child.wait()` and reports the
//!   final status via the sink.
//! - One **tokio task** per session drains a bounded
//!   `mpsc::channel::<String>(512)` and dispatches each chunk to
//!   `sink.output(...)`. The bounded channel is the backpressure boundary
//!   (DESIGN §8.3).
//! - The pool's lock is a `std::sync::Mutex` over a `BTreeMap`. **It is never
//!   held across an `.await`** — callers `lock → take → drop → await`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use portable_pty::{native_pty_system, CommandBuilder, ExitStatus, PtySize};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::activity::{ActivityEvent, ActivityScanner, TICK_INTERVAL};
use crate::compose::{self, platform_shell};
use crate::types::{Error, Session, SessionId, SessionStatus, Tool};

// --------------------------------------------------------------------------- Tunables (DESIGN §8.3 / SPEC NF-09)
// ---------------------------------------------------------------------------

/// Bounded capacity for the per-session output channel. Once full, new chunks are **dropped** (newest-first) and a counter is incremented. DESIGN
/// §8.3 pins this at 512.
pub const OUTPUT_CHANNEL_CAPACITY: usize = 512;

/// How many drops between successive backpressure warnings.
pub const DROP_LOG_EVERY: usize = 256;

/// SIGTERM → SIGKILL grace period on Unix.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// Maximum time we wait for the drain task to finish after `kill`.
pub const DRAIN_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Default initial PTY size (mirrors xterm.js's default until the frontend resizes it).
pub const DEFAULT_PTY_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// ANSI full-reset sequence (`ESC c`). Prepended to the next emitted chunk after a backpressure drop so xterm.js cannot be left mid-escape (DESIGN
/// §8.3 — added in Phase 6).
pub const ANSI_FULL_RESET: &str = "\x1bc";

/// Orphan temp-dir age threshold for [`cleanup_orphans`].
pub const ORPHAN_AGE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Outcome of a [`PtyPool::kill`] call.
///
/// `kill` removes the runtime entry from the pool unconditionally and always issues `killer.kill()` (SIGKILL on Unix / `TerminateProcess` on Windows
/// — both unconditional process-termination primitives). The OS-level kill primitive almost never fails for a child we just spawned and own, but the
/// post-kill wait-thread join (which calls `child.wait()` to reap the process) **can** time out in pathological cases (e.g. Unix zombie reaping is
/// delayed, Windows handle still held by a debugger). The outcome captures whether we actually observed the process being reaped within
/// [`KILL_GRACE`] so callers can decide whether to log a possible
/// orphan PID.
///
/// `park_session_for_switch_impl` uses this to surface the rare "kill-issued-but-unconfirmed" case during an in-app workspace switch — without that
/// visibility, an orphaned CLI from a parked session could be silently respawned as a second live process for the same tab on the next switch-back.
/// See PR #32 round-12 review thread for the underlying concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// `killer.kill()` returned `Ok` AND the wait thread joined cleanly within [`KILL_GRACE`]. The OS has reaped the process.
    Reaped,
    /// `killer.kill()` returned `Err`, OR the wait thread did not join within [`KILL_GRACE`]. The process **may** still be alive at the recorded PID
    /// — callers should log loudly so a human can find and clean up the orphan.
    Unconfirmed { pid: u32 },
}

// --------------------------------------------------------------------------- Spawner / child trait seam
// ---------------------------------------------------------------------------

/// Minimal description of a child process to spawn. Decoupled from `portable_pty::CommandBuilder` so test spawners don't need to depend on
/// portable-pty's API surface.
#[derive(Debug, Clone)]
pub struct ChildCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Environment variable additions/overrides applied on top of the parent process's inherited env. Used to inject per-session telemetry settings
    /// (e.g. Copilot's OTel file exporter path) without touching the persisted `Session.composed_command`. Empty for tools that need no extra env
    /// (e.g. Claude today).
    pub env: Vec<(String, std::ffi::OsString)>,
}

/// Result of a successful spawn — a bundle of independent handles. Splitting them up means the wait-thread can block in `wait()` without holding any
/// lock that `write`/`resize`/`kill` need.
pub struct SpawnedChild {
    /// OS PID of the child.
    pub pid: u32,
    /// Bytes-out side of the PTY master.
    pub reader: Box<dyn Read + Send>,
    /// Bytes-in side of the PTY master.
    pub writer: Box<dyn Write + Send>,
    /// PTY resize handle (callable from any thread, no `&mut`).
    pub resize: Arc<dyn PtyResize>,
    /// Owned exclusively by the wait thread; consumed by [`PtyWaiter::wait`].
    pub waiter: Box<dyn PtyWaiter>,
    /// Independent kill handle, callable from any thread.
    pub killer: Arc<dyn PtyKiller>,
}

/// Trait seam over `portable-pty`'s `PtySystem`. `PtyPool` accepts any implementor — production wires [`PortablePtySpawner`], tests wire fakes.
pub trait PtySpawner: Send + Sync {
    /// Open a PTY pair, spawn `cmd` inside it with the given working directory and initial size, and return a fully decoupled handle bundle.
    fn spawn(&self, cmd: ChildCommand, cwd: &Path, size: PtySize) -> Result<SpawnedChild, Error>;
}

/// PTY resize handle. Implementations must be safe to call from any thread.
pub trait PtyResize: Send + Sync {
    fn resize(&self, cols: u16, rows: u16) -> Result<(), Error>;
}

/// Wait handle. Owned by exactly one thread (the per-session wait thread) and consumed by [`Self::wait`].
pub trait PtyWaiter: Send {
    fn wait(self: Box<Self>) -> Result<ExitStatus, Error>;
}

/// Kill handle. Independent of the waiter so the kill caller never has to contend with the wait thread.
///
/// On Unix the production impl sends SIGTERM, waits up to [`KILL_GRACE`], then escalates to SIGKILL. On Windows it forwards to portable-pty's `kill`,
/// which terminates the child via `TerminateProcess`.
pub trait PtyKiller: Send + Sync {
    fn kill(&self) -> Result<(), Error>;
}

// --------------------------------------------------------------------------- Production spawner (portable-pty)
// ---------------------------------------------------------------------------

/// Production [`PtySpawner`] backed by `portable_pty::native_pty_system()`.
pub struct PortablePtySpawner;

impl PortablePtySpawner {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for PortablePtySpawner {
    fn default() -> Self {
        Self::new()
    }
}

impl PtySpawner for PortablePtySpawner {
    fn spawn(&self, cmd: ChildCommand, cwd: &Path, size: PtySize) -> Result<SpawnedChild, Error> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size)
            .map_err(|e| Error::PtySpawnFailed(format!("openpty failed: {e}")))?;

        let mut builder = CommandBuilder::new(&cmd.program);
        for a in &cmd.args {
            builder.arg(a);
        }
        // DESIGN §5.6: cwd is the discrete worktree path — never spliced into the command string.
        builder.cwd(cwd);
        // Per-session env additions. The child still inherits the parent process's env (we never call `env_clear`); these are overrides/additions
        // only — see `compose::env_for_tool`.
        for (k, v) in &cmd.env {
            builder.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| Error::PtySpawnFailed(format!("spawn_command failed: {e}")))?;

        // Drop the slave side immediately (per portable-pty docs) so the child doesn't keep the pty alive after it exits.
        drop(pair.slave);

        let pid = child.process_id().ok_or_else(|| Error::PtySpawnFailed("child has no pid".into()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| Error::PtySpawnFailed(format!("try_clone_reader failed: {e}")))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| Error::PtyWriteFailed(format!("take_writer failed: {e}")))?;

        let killer_inner = child.clone_killer();
        let resize_handle = Arc::new(PortableResize {
            master: Mutex::new(pair.master),
        });
        let killer = Arc::new(PortableKiller {
            inner: Mutex::new(killer_inner),
            pid,
        });
        let waiter = Box::new(PortableWaiter { child });

        Ok(SpawnedChild {
            pid,
            reader,
            writer,
            resize: resize_handle,
            waiter,
            killer,
        })
    }
}

struct PortableResize {
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
}

impl PtyResize for PortableResize {
    fn resize(&self, cols: u16, rows: u16) -> Result<(), Error> {
        let guard = self.master.lock().map_err(|_| Error::PtyResizeFailed("master mutex poisoned".into()))?;
        guard
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::PtyResizeFailed(format!("resize failed: {e}")))
    }
}

struct PortableWaiter {
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtyWaiter for PortableWaiter {
    fn wait(mut self: Box<Self>) -> Result<ExitStatus, Error> {
        self.child.wait().map_err(|e| Error::Internal(format!("wait failed: {e}")))
    }
}

struct PortableKiller {
    inner: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    #[allow(dead_code)]
    pid: u32,
}

impl PtyKiller for PortableKiller {
    fn kill(&self) -> Result<(), Error> {
        {
            let mut guard = self.inner.lock().map_err(|_| Error::PtyKillFailed("killer mutex poisoned".into()))?;
            guard.kill().map_err(|e| Error::PtyKillFailed(format!("kill failed: {e}")))?;
        }

        // Unix-only SIGKILL escalation if the child doesn't react to SIGTERM within KILL_GRACE.
        #[cfg(unix)]
        {
            std::thread::sleep(KILL_GRACE);
            // SAFETY: kill(2) is thread-safe.
            unsafe {
                libc_kill(self.pid as i32, 9);
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    // Best-effort SIGKILL escalation; ignore the return value because the child may already have exited between try_wait() and now.
    let _ = unsafe { kill(pid, sig) };
}

// --------------------------------------------------------------------------- Sink — Tauri-agnostic seam for output and status
// ---------------------------------------------------------------------------

/// Output callback type alias.
pub type OutputCb = Arc<dyn Fn(&SessionId, String) + Send + Sync>;
/// Status callback type alias. The `Option<u32>` is the PID; cleared on exit.
pub type StatusCb = Arc<dyn Fn(&SessionId, SessionStatus, Option<u32>, Option<String>) + Send + Sync>;
/// Activity callback type alias. Fired by the per-session activity scanner (see [`crate::activity`]). Carries semantic events derived from the raw
/// PTY stream — title changes, attention cues, working/idle transitions.
pub type ActivityCb = Arc<dyn Fn(&SessionId, ActivityEvent) + Send + Sync>;

/// The pool talks to the rest of the app exclusively through this struct.
///
/// In production (Phase 7), `output` will both `AppHandle::emit` a `session://output` event and `status` will both emit `session://status` AND call
/// `config_store::update_session_status`. The pool does not know or care.
#[derive(Clone)]
pub struct PtySink {
    pub output: OutputCb,
    pub status: StatusCb,
    pub activity: ActivityCb,
}

impl PtySink {
    #[must_use]
    pub fn new(output: OutputCb, status: StatusCb, activity: ActivityCb) -> Self {
        Self { output, status, activity }
    }
}

// --------------------------------------------------------------------------- Streaming UTF-8 decoder
// ---------------------------------------------------------------------------

/// Tiny streaming UTF-8 decoder. Holds at most 3 trailing bytes (the maximum length of a partial UTF-8 character minus one).
///
/// Design rule: **on each `feed`, return a `String` containing every fully decoded scalar; retain any trailing partial sequence for the next call**.
/// Invalid bytes are replaced with U+FFFD (REPLACEMENT CHARACTER).
///
/// Visibility is `pub(crate)` rather than `pub`: the only external caller is `crate::sub_sessions`, and we don't want to commit to this type as part
/// of the crate's public API surface.
#[derive(Debug, Default)]
pub(crate) struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> String {
        if self.pending.is_empty() && bytes.is_empty() {
            return String::new();
        }
        // Concatenate any held-over bytes with the new chunk.
        let mut buf: Vec<u8>;
        let slice: &[u8] = if self.pending.is_empty() {
            bytes
        } else {
            buf = std::mem::take(&mut self.pending);
            buf.extend_from_slice(bytes);
            &buf[..]
        };

        // Walk Utf8Chunks: every chunk has a `valid()` prefix and an `invalid()` suffix. The trailing chunk's `invalid()` is the only candidate for
        // "partial multibyte at end of buffer".
        let mut out = String::with_capacity(slice.len());
        let mut chunks = slice.utf8_chunks().peekable();
        while let Some(chunk) = chunks.next() {
            out.push_str(chunk.valid());
            let inv = chunk.invalid();
            if inv.is_empty() {
                continue;
            }
            // If this is the final chunk AND the invalid tail is shorter than the maximum UTF-8 sequence length AND it could plausibly be the prefix
            // of a valid sequence, hold it for next time.
            if chunks.peek().is_none() && is_possible_partial(inv) {
                self.pending = inv.to_vec();
            } else {
                // Truly invalid — emit U+FFFD per the WHATWG spec.
                out.push('\u{FFFD}');
            }
        }
        out
    }

    /// Flush any held-back bytes as REPLACEMENT CHARACTERs (called on EOF).
    pub fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            String::new()
        } else {
            self.pending.clear();
            "\u{FFFD}".to_string()
        }
    }
}

/// True if `bytes` could be a strict prefix of a valid UTF-8 scalar.
fn is_possible_partial(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() >= 4 {
        return false;
    }
    let lead = bytes[0];
    let expected_len = if lead < 0x80 {
        1
    } else if lead & 0b1110_0000 == 0b1100_0000 {
        2
    } else if lead & 0b1111_0000 == 0b1110_0000 {
        3
    } else if lead & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        return false; // continuation byte or invalid lead — not a partial
    };
    if bytes.len() >= expected_len {
        return false;
    }
    // All remaining bytes after the lead must look like continuations.
    bytes[1..].iter().all(|b| b & 0b1100_0000 == 0b1000_0000)
}

// --------------------------------------------------------------------------- PtyPool
// ---------------------------------------------------------------------------

/// Per-session runtime state held inside the pool.
struct SessionRuntime {
    pid: u32,
    /// Writer over the PTY master — taken once at spawn, used by every `write` call. Held in its own mutex so it never contends with
    /// `wait`/`kill`/`resize`.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Independent resize handle.
    resize: Arc<dyn PtyResize>,
    /// Independent kill handle.
    killer: Arc<dyn PtyKiller>,
    /// Sender side of the bounded output channel — dropping it tells the drain task to finish.
    sender: mpsc::Sender<String>,
    /// Cancellation token for the drain task.
    cancel: CancellationToken,
    /// Drain-task handle.
    drain: tokio::task::JoinHandle<()>,
    /// Wait-thread handle. Detached on Drop; explicitly joined by `kill`.
    wait_thread: Option<std::thread::JoinHandle<()>>,
    /// Shared with the wait thread; flipped on `kill` so the wait thread knows the exit was requested and shouldn't re-emit status.
    killed: Arc<AtomicBool>,
    /// Backpressure counter — exposed for tests / observability.
    dropped_chunks: Arc<AtomicUsize>,
}

/// The pool itself.
pub struct PtyPool {
    spawner: Arc<dyn PtySpawner>,
    inner: Mutex<BTreeMap<SessionId, SessionRuntime>>,
}

impl PtyPool {
    /// Construct a pool over the given spawner. The spawner is **always injected**.
    #[must_use]
    pub fn new(spawner: Arc<dyn PtySpawner>) -> Self {
        Self {
            spawner,
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Number of live sessions (test/debug aid).
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or_default()
    }

    /// True if no sessions are live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True if the given session is currently tracked by the pool.
    pub fn contains(&self, id: &SessionId) -> bool {
        self.inner.lock().map(|g| g.contains_key(id)).unwrap_or(false)
    }

    /// PID of the given session, if it's live in the pool.
    pub fn pid_of(&self, id: &SessionId) -> Option<u32> {
        self.inner.lock().ok().and_then(|g| g.get(id).map(|rt| rt.pid))
    }

    /// Atomic dropped-chunks counter for the given session — exposed for observability and tests.
    pub fn dropped_chunks(&self, id: &SessionId) -> Option<Arc<AtomicUsize>> {
        self.inner.lock().ok().and_then(|g| g.get(id).map(|rt| Arc::clone(&rt.dropped_chunks)))
    }

    /// Spawn a fresh PTY child for `session`. Composes the platform shell invocation `[shell, flag, session.composed_command]` and passes the
    /// session's worktree as the discrete `cwd`.
    ///
    /// The `size` is the initial PTY dimensions the child sees at startup. Callers must measure the host terminal first — passing the wrong size here
    /// is the exact race that caused the long-standing "splash screen rendered at 80 cols then never re-laid-out" bug.
    ///
    /// Returns the assigned PID.
    pub fn spawn(&self, session: &Session, sink: PtySink, size: PtySize) -> Result<u32, Error> {
        self.spawn_internal(session, sink, size)
    }

    /// Re-spawn a session from its **already-stored** `composed_command`. This is the entry point Phase 7 uses for restart and restore. The behaviour
    /// is identical to [`spawn`]; the distinct name documents the "do not recompose at restart time" rule from DESIGN §5.4.
    ///
    /// [`spawn`]: Self::spawn
    pub fn respawn_existing(&self, session: &Session, sink: PtySink, size: PtySize) -> Result<u32, Error> {
        // If a previous runtime entry exists (e.g. from a prior spawn that hasn't exited yet), tear it down first.
        if self.contains(&session.id) {
            // Best-effort kill; if the child is already dead this is a no-op.
            let _ = self.kill_blocking(&session.id);
        }
        self.spawn_internal(session, sink, size)
    }

    fn spawn_internal(&self, session: &Session, sink: PtySink, size: PtySize) -> Result<u32, Error> {
        // ------- 1. Per-session spawn prep (telemetry env, temp dir).
        //
        // We do this here — not in `commands/session.rs` — so every spawn path (create, restart, restore-on-launch) gets the same treatment without
        // each call site having to remember. Mirror of the post-close cleanup that already lives next to `kill`.
        let env = compose::env_for_tool(session.tool, &session.id);
        // Tool-specific spawn prep is keyed off `session.tool`, NOT off "env is non-empty" — those concepts are independent. A future tool that needs
        // env injection but no temp file (or vice versa) must not get Copilot's stale-OTel cleanup applied to it. Match on Tool explicitly.
        match session.tool {
            Tool::Copilot => {
                // Copilot's OTel exporter writes to a per-session JSONL in a temp dir. Ensure the dir exists before the child opens the file, and
                // remove any stale JSONL from a previous run so restart / restore-on-launch don't replay old spans and double-count totals.
                let dir = compose::session_temp_dir(&session.id);
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    debug!(session_id = %session.id, error = %e, dir = %dir.display(), "session temp dir create failed");
                }
                // Single source of truth for the path is `compose::copilot_otel_path` — no string literal here. Best-effort removal; missing file is
                // fine.
                let stale = compose::copilot_otel_path(&session.id);
                match std::fs::remove_file(&stale) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        debug!(session_id = %session.id, error = %e, "stale otel.jsonl removal failed");
                    }
                }
            }
            Tool::Claude => {
                // No spawn-time prep needed today.
            }
        }

        // ------- 2. Compose ChildCommand from platform shell + composed_command
        let shell = platform_shell();
        let cmd = ChildCommand {
            program: shell.program.clone(),
            args: vec![shell.flag.to_string(), session.composed_command.clone()],
            env,
        };

        // ------- 3. Spawn via injected spawner; cwd is discrete (DESIGN §5.6)
        let spawned = self.spawner.spawn(cmd, &session.worktree_path, size)?;
        let SpawnedChild {
            pid,
            reader,
            writer,
            resize,
            waiter,
            killer,
        } = spawned;

        // ------- 4. Build channel + drain task
        let (tx, mut rx) = mpsc::channel::<String>(OUTPUT_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let drain_cancel = cancel.clone();
        let sink_for_drain = sink.clone();
        let id_for_drain = session.id;
        let drain = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = drain_cancel.cancelled() => break,
                    msg = rx.recv() => {
                        match msg {
                            Some(chunk) => (sink_for_drain.output)(&id_for_drain, chunk),
                            None => break, // sender dropped
                        }
                    }
                }
            }
        });

        let dropped = Arc::new(AtomicUsize::new(0));
        let killed = Arc::new(AtomicBool::new(false));
        let scanner = Arc::new(Mutex::new(ActivityScanner::new()));

        // ------- 5. Read thread (OS thread; portable-pty reads are blocking)
        let read_id = session.id;
        let read_tx = tx.clone();
        let read_dropped = Arc::clone(&dropped);
        let read_scanner = Arc::clone(&scanner);
        let read_sink = sink.clone();
        std::thread::Builder::new()
            .name(format!("arborist-pty-read-{pid}"))
            .spawn(move || {
                pty_read_loop(read_id, reader, read_tx, read_dropped, read_scanner, read_sink);
            })
            .map_err(|e| Error::PtySpawnFailed(format!("spawn read thread failed: {e}")))?;

        // ------- 5b. Activity tick task — emits Idle transitions when the PTY has been quiescent. Runs on the tokio runtime so it shares the
        // existing CancellationToken plumbing for clean shutdown.
        let tick_cancel = cancel.clone();
        let tick_sink = sink.clone();
        let tick_id = session.id;
        let tick_scanner = Arc::clone(&scanner);
        let _tick_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = tick_cancel.cancelled() => break,
                    _ = interval.tick() => {
                        // Lock → take → drop, never across await.
                        let evt = match tick_scanner.lock() {
                            Ok(mut g) => g.tick(),
                            Err(_) => break,
                        };
                        if let Some(evt) = evt {
                            (tick_sink.activity)(&tick_id, evt);
                        }
                    }
                }
            }
        });

        // ------- 6. Wait thread (OS thread)
        let wait_id = session.id;
        let wait_sink = sink.clone();
        let wait_killed = Arc::clone(&killed);
        let wait_thread = std::thread::Builder::new()
            .name(format!("arborist-pty-wait-{pid}"))
            .spawn(move || {
                pty_wait_loop(wait_id, waiter, wait_sink, wait_killed);
            })
            .map_err(|e| Error::PtySpawnFailed(format!("spawn wait thread failed: {e}")))?;

        // ------- 7. Insert runtime entry
        {
            let mut guard = self.inner.lock().map_err(|_| Error::Internal("pty pool mutex poisoned".into()))?;
            guard.insert(
                session.id,
                SessionRuntime {
                    pid,
                    writer: Arc::new(Mutex::new(writer)),
                    resize,
                    killer,
                    sender: tx,
                    cancel,
                    drain,
                    wait_thread: Some(wait_thread),
                    killed,
                    dropped_chunks: dropped,
                },
            );
        }

        // ------- 8. Announce Running
        (sink.status)(&session.id, SessionStatus::Running, Some(pid), None);

        Ok(pid)
    }

    /// Write bytes to the PTY master.
    pub fn write(&self, id: &SessionId, data: &[u8]) -> Result<(), Error> {
        let writer = {
            let guard = self.inner.lock().map_err(|_| Error::Internal("pty pool mutex poisoned".into()))?;
            let rt = guard.get(id).ok_or_else(|| Error::NotFound(format!("session {id} not in pty pool")))?;
            Arc::clone(&rt.writer)
        };
        let mut writer_guard = writer.lock().map_err(|_| Error::Internal("pty writer mutex poisoned".into()))?;
        writer_guard
            .write_all(data)
            .map_err(|e| Error::PtyWriteFailed(format!("write failed: {e}")))?;
        writer_guard.flush().map_err(|e| Error::PtyWriteFailed(format!("flush failed: {e}")))
    }

    /// Resize the PTY of session `id`.
    pub fn resize(&self, id: &SessionId, cols: u16, rows: u16) -> Result<(), Error> {
        let resize = {
            let guard = self.inner.lock().map_err(|_| Error::Internal("pty pool mutex poisoned".into()))?;
            let rt = guard.get(id).ok_or_else(|| Error::NotFound(format!("session {id} not in pty pool")))?;
            Arc::clone(&rt.resize)
        };
        resize.resize(cols, rows)
    }

    /// Kill the child, tear down its read/wait threads and drain task, and remove the entry. Also deletes the session's temp dir on disk.
    ///
    /// Async because we await the drain-task join with a timeout. **Never holds the pool lock across `.await`** (DESIGN/copilot-instructions).
    ///
    /// Returns [`KillOutcome::Reaped`] on the happy path (kill returned `Ok` AND the wait thread joined within [`KILL_GRACE`]) so callers can confirm
    /// the OS reaped the child. Returns
    /// [`KillOutcome::Unconfirmed`] when the kill primitive returned an
    /// error OR the wait thread did not join in time — both are rare, but in either case the child **may** still be alive at the recorded PID. The
    /// runtime entry is removed from the pool either way (so the SessionId is free for a fresh respawn). Callers that care about possible orphans
    /// (e.g. `park_session_for_switch_impl`) should log loudly when they see `Unconfirmed`.
    pub async fn kill(&self, id: &SessionId) -> Result<KillOutcome, Error> {
        // 1. Remove the runtime entry under the lock; everything else happens with no
        //    lock held.
        let rt = {
            let mut guard = self.inner.lock().map_err(|_| Error::Internal("pty pool mutex poisoned".into()))?;
            guard.remove(id)
        };
        let Some(rt) = rt else {
            return Err(Error::NotFound(format!("session {id} not in pty pool")));
        };
        let pid = rt.pid;

        // 2. Mark killed so the wait thread doesn't emit Exited/Error.
        rt.killed.store(true, Ordering::SeqCst);

        // 3. Kill the child via the independent killer handle. `PortableKiller::kill`
        //    issues SIGKILL on Unix (unconditional process termination) or
        //    `TerminateProcess` on Windows (also unconditional). Failure here is rare —
        //    typically only permission-denied or the child has already exited — but we
        //    capture it so we can surface it in `KillOutcome` rather than swallow it as
        //    we used to with `let _ =`.
        let killer_result = rt.killer.kill();

        // 4. Drop the sender; cancel the drain task; await with a timeout.
        drop(rt.sender);
        rt.cancel.cancel();
        match timeout(DRAIN_JOIN_TIMEOUT, rt.drain).await {
            Ok(Ok(())) => {}
            Ok(Err(join_err)) => {
                error!(session_id = %id, error = ?join_err, "drain task join failed");
            }
            Err(_) => {
                error!(session_id = %id, "drain task did not exit within timeout");
            }
        }

        // 5. Best-effort: join the wait thread (it should exit when the PTY closes).
        //    Use a tokio blocking task with a short timeout so we don't block the
        //    executor forever. The success/failure of this join is the load-bearing
        //    signal for `KillOutcome`: a clean join means the OS reaped the child
        //    within `KILL_GRACE`.
        let wait_joined = if let Some(handle) = rt.wait_thread {
            matches!(
                timeout(KILL_GRACE, tokio::task::spawn_blocking(move || handle.join()),).await,
                Ok(Ok(Ok(()))),
            )
        } else {
            // No wait thread to join (e.g. constructed without one in some test paths). Treat as confirmed reaped — there's nothing to verify.
            true
        };

        // 6. Delete the per-session temp dir.
        let dir = compose::session_temp_dir(id);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                debug!(session_id = %id, dir = %dir.display(), error = %e, "remove_dir_all failed");
            }
        }

        // 7. Decide on the outcome. The kill **was** issued in step 3 regardless of
        //    what we return here — we never re-spawn the same SessionId from the pool's
        //    perspective because step 1 already evicted it. The outcome is purely
        //    diagnostic, so the caller can record an orphan PID for human cleanup.
        if killer_result.is_err() || !wait_joined {
            if let Err(e) = killer_result {
                warn!(
                    session_id = %id,
                    pid,
                    error = ?e,
                    "kill: killer.kill() returned error; process may still be alive at this PID",
                );
            }
            if !wait_joined {
                warn!(
                    session_id = %id,
                    pid,
                    grace_secs = KILL_GRACE.as_secs(),
                    "kill: wait thread did not join within grace period; process may still be alive at this PID",
                );
            }
            Ok(KillOutcome::Unconfirmed { pid })
        } else {
            Ok(KillOutcome::Reaped)
        }
    }

    /// Synchronous best-effort kill used by `respawn_existing` (where we can't `.await` because we're in the synchronous spawn path) and from `Drop`
    /// (no async context available there at all).
    fn kill_blocking(&self, id: &SessionId) -> Result<(), Error> {
        let rt = {
            let mut guard = self.inner.lock().map_err(|_| Error::Internal("pty pool mutex poisoned".into()))?;
            guard.remove(id)
        };
        let Some(rt) = rt else {
            return Ok(());
        };
        rt.killed.store(true, Ordering::SeqCst);
        let _ = rt.killer.kill();
        rt.cancel.cancel();
        drop(rt.sender);
        // Don't await — abort the drain task and detach.
        rt.drain.abort();
        Ok(())
    }
}

impl Drop for PtyPool {
    fn drop(&mut self) {
        // Best-effort: kill all live children.
        let ids: Vec<SessionId> = self.inner.lock().map(|g| g.keys().copied().collect()).unwrap_or_default();
        for id in ids {
            let _ = self.kill_blocking(&id);
        }
    }
}

// --------------------------------------------------------------------------- Read / wait loops (free functions so they're easy to unit-test)
// ---------------------------------------------------------------------------

fn pty_read_loop(
    id: SessionId,
    mut reader: Box<dyn Read + Send>,
    sender: mpsc::Sender<String>,
    dropped: Arc<AtomicUsize>,
    scanner: Arc<Mutex<ActivityScanner>>,
    sink: PtySink,
) {
    let mut decoder = Utf8Stream::default();
    let mut buf = [0u8; 4096];
    let mut needs_reset = false;

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                // Activity scan first — the scanner needs raw bytes (OSC sequences are pure ASCII so they survive UTF-8 decode, but we want
                // byte-accurate timing). Lock → take → drop.
                let events = match scanner.lock() {
                    Ok(mut g) => g.feed_bytes(&buf[..n]),
                    Err(_) => Vec::new(),
                };
                for evt in events {
                    (sink.activity)(&id, evt);
                }

                let mut decoded = decoder.feed(&buf[..n]);
                if decoded.is_empty() {
                    continue;
                }
                if needs_reset {
                    decoded.insert_str(0, ANSI_FULL_RESET);
                    needs_reset = false;
                }
                if let Err(e) = sender.try_send(decoded) {
                    match e {
                        mpsc::error::TrySendError::Full(_) => {
                            let n = dropped.fetch_add(1, Ordering::Relaxed) + 1;
                            needs_reset = true;
                            if n.is_multiple_of(DROP_LOG_EVERY) {
                                warn!(
                                    target: "arborist::pty",
                                    session_id = %id,
                                    drop_count = n,
                                    "pty.backpressure dropping output chunks"
                                );
                            }
                        }
                        mpsc::error::TrySendError::Closed(_) => break,
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    // Flush any trailing partial bytes as U+FFFD.
    let tail = decoder.flush();
    if !tail.is_empty() {
        let _ = sender.try_send(tail);
    }
}

fn pty_wait_loop(id: SessionId, waiter: Box<dyn PtyWaiter>, sink: PtySink, killed: Arc<AtomicBool>) {
    let result = waiter.wait();

    if killed.load(Ordering::SeqCst) {
        // Sink has already been notified by `kill` (or will be by Phase 7's close handler). Don't double-emit.
        return;
    }

    let status = match result {
        Ok(exit) if exit.success() => SessionStatus::Exited,
        Ok(_) | Err(_) => SessionStatus::Error,
    };
    (sink.status)(&id, status, None, None);
}

// --------------------------------------------------------------------------- Orphan cleanup
// ---------------------------------------------------------------------------

/// Scan `<os-temp>/arborist/` for per-session directories whose UUID is **not** in `persisted_session_ids` and whose mtime is older than
/// [`ORPHAN_AGE_THRESHOLD`]. Returns the number deleted.
///
/// Restore-safety: a stale-mtime dir whose UUID **is** still persisted is **kept**, so a Phase 7 restart never races temp-file deletion against
/// rematerialisation (DESIGN §5.6 / Phase 6 spec).
pub fn cleanup_orphans(persisted_session_ids: &[SessionId]) -> Result<usize, Error> {
    let root = compose::session_temp_dir(&SessionId::new());
    let scan_root = root
        .parent()
        .ok_or_else(|| Error::Internal("session temp dir has no parent".into()))?
        .to_path_buf();

    if !scan_root.exists() {
        return Ok(0);
    }

    let persisted: std::collections::HashSet<String> = persisted_session_ids.iter().map(|id| id.0.to_string()).collect();

    let now = SystemTime::now();
    let mut deleted = 0usize;
    for entry in std::fs::read_dir(&scan_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Only touch UUID-named dirs — don't accidentally nuke unrelated siblings.
        if uuid::Uuid::parse_str(name).is_err() {
            continue;
        }
        if persisted.contains(name) {
            continue;
        }
        let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(now);
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age >= ORPHAN_AGE_THRESHOLD {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                warn!(dir = %path.display(), error = %e, "cleanup_orphans: remove_dir_all failed");
            } else {
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

// --------------------------------------------------------------------------- Tests (unit-level — integration tests live in tests/pty_pool.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn utf8_stream_handles_split_three_byte_char() {
        let mut s = Utf8Stream::default();
        // 世 = E4 B8 96
        let part1 = s.feed(&[0xE4, 0xB8]);
        assert_eq!(part1, "");
        let part2 = s.feed(&[0x96]);
        assert_eq!(part2, "世");
        assert!(s.pending.is_empty());
    }

    #[test]
    fn utf8_stream_handles_split_four_byte_char() {
        let mut s = Utf8Stream::default();
        // 😀 = F0 9F 98 80
        assert_eq!(s.feed(&[0xF0, 0x9F]), "");
        assert_eq!(s.feed(&[0x98]), "");
        assert_eq!(s.feed(&[0x80]), "😀");
    }

    #[test]
    fn utf8_stream_passes_ascii_through() {
        let mut s = Utf8Stream::default();
        assert_eq!(s.feed(b"hello"), "hello");
        assert_eq!(s.feed(b" world"), " world");
    }

    #[test]
    fn utf8_stream_replaces_invalid_bytes() {
        let mut s = Utf8Stream::default();
        // 0xFF is never valid UTF-8.
        let out = s.feed(&[b'a', 0xFF, b'b']);
        assert_eq!(out, "a\u{FFFD}b");
    }

    #[test]
    fn utf8_stream_flush_emits_replacement_for_dangling_partial() {
        let mut s = Utf8Stream::default();
        let _ = s.feed(&[0xE4, 0xB8]); // partial 世
        assert_eq!(s.flush(), "\u{FFFD}");
        assert!(s.pending.is_empty());
    }

    #[test]
    fn is_possible_partial_classifies_correctly() {
        assert!(is_possible_partial(&[0xE4]));
        assert!(is_possible_partial(&[0xE4, 0xB8]));
        assert!(is_possible_partial(&[0xF0, 0x9F, 0x98]));
        assert!(!is_possible_partial(&[0xE4, 0xB8, 0x96])); // complete
        assert!(!is_possible_partial(&[0xFF])); // invalid lead
        assert!(!is_possible_partial(&[0x80])); // continuation
        assert!(!is_possible_partial(&[]));
    }
}
