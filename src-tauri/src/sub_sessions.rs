//! Sub-session backend (Phase 2 of `dev/ai/CONTEXT_MENU_PLAN.md`).
//!
//! Sub-sessions are user-launched secondary processes attached to a parent
//! [`Session`]. There are two flavours:
//!
//! * **Terminal** — a PTY hosted in-app, owned by the [`SubPtyPool`] defined
//!   here. Output flows over `session://output` (the existing stream — UUID id
//!   space is global), status flows over `subsession://status`.
//! * **Application** — a detached external GUI process. Phase 3 will own the
//!   spawn / window-focus implementation. Phase 2's `subsession_create` rejects
//!   this kind with `NotImplemented`.
//!
//! ## Design choices
//!
//! - **Separate pool, not a refactor of [`crate::pty_pool::PtyPool`].** We
//!   intentionally keep the existing session pool untouched. The sub-session
//!   pool reuses [`PtySpawner`] / [`ChildCommand`] / [`SpawnedChild`] /
//!   [`Utf8Stream`] from [`crate::pty_pool`] so the spawn primitive itself
//!   isn't duplicated, but the per-runtime state and lifecycle are owned
//!   independently. This matches the "compose-once, store-and-reuse" rule from
//!   DESIGN §5.4: sub-sessions have their own
//!   [`composed_command`](SubSession::composed_command).
//! - **No activity scanner.** Sub-tabs are plain terminals; we don't reuse the
//!   OTel/title/idle scanning today. Phase 7+ may revisit.
//! - **Bounded backpressure mirrors the session pool** (DESIGN §8.3).
//! - **Locks are never held across `.await`.** All async paths follow "lock →
//!   take → drop → await".
//!
//! ## Public surface
//!
//! - [`SubPtyPool`] — runtime pool for terminal sub-sessions.
//! - [`SubPtySink`] — Tauri-agnostic seam for output / status / exited
//!   callbacks (mirrors [`crate::pty_pool::PtySink`]).
//! - [`SubSessionStore`] — in-memory metadata store for live sub-sessions
//!   indexed by id and (orderedly) by parent.
//! - [`SubAppContext`] — managed Tauri state combining the pool, store, and
//!   sink for command handlers.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::PtySize;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::pty_pool::{
    ChildCommand, KillOutcome, PtyKiller, PtyResize, PtySpawner, PtyWaiter, SpawnedChild, Utf8Stream, DROP_LOG_EVERY, KILL_GRACE,
    OUTPUT_CHANNEL_CAPACITY,
};
use crate::types::{Error, SessionId, SubSession, SubSessionId, SubSessionStatus};

/// Default initial PTY size for sub-tabs. Identical to the session pool's default; the frontend re-fits on first attach.
pub const DEFAULT_SUB_PTY_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

const DRAIN_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

// --------------------------------------------------------------------------- Sink — Tauri-agnostic seam
// ---------------------------------------------------------------------------

/// Output bytes from a sub-session's PTY.
pub type SubOutputCb = Arc<dyn Fn(&SubSessionId, String) + Send + Sync>;
/// Status update for a sub-session — fires on `Running`, `Exited`, `Error`.
pub type SubStatusCb = Arc<dyn Fn(&SubSessionId, SubSessionStatus, Option<u32>, Option<String>) + Send + Sync>;
/// Exit notification used by Phase 3's application launcher. Phase 2's terminal pool does not call this directly (it uses [`SubStatusCb`] with
/// [`SubSessionStatus::Exited`]); kept on the sink so production wiring
/// has a single place to emit `subsession://exited`.
pub type SubExitedCb = Arc<dyn Fn(&SubSessionId, Option<i32>) + Send + Sync>;
/// Phase 7 restore-on-launch notification. Fired once per sub-session re-materialised from `AppConfig.lastOpenSubSessions` so the frontend store can
/// insert the entry **before** any subsequent `subsession://status` event for that id (otherwise the status event would be ignored as "unknown id").
/// Carries the full [`SubSession`] because hydrate has already returned by the time restore runs.
pub type SubRestoredCb = Arc<dyn Fn(&SubSession) + Send + Sync>;

#[derive(Clone)]
pub struct SubPtySink {
    pub output: SubOutputCb,
    pub status: SubStatusCb,
    pub exited: SubExitedCb,
    pub restored: SubRestoredCb,
}

impl SubPtySink {
    #[must_use]
    pub fn new(output: SubOutputCb, status: SubStatusCb, exited: SubExitedCb, restored: SubRestoredCb) -> Self {
        Self {
            output,
            status,
            exited,
            restored,
        }
    }

    /// All-no-op sink for tests that only assert on pool side-effects.
    #[must_use]
    pub fn noop() -> Self {
        Self {
            output: Arc::new(|_, _| {}),
            status: Arc::new(|_, _, _, _| {}),
            exited: Arc::new(|_, _| {}),
            restored: Arc::new(|_| {}),
        }
    }
}

// --------------------------------------------------------------------------- SubPtyPool — runtime pool for terminal sub-sessions
// ---------------------------------------------------------------------------

struct SubRuntime {
    pid: u32,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    resize: Arc<dyn PtyResize>,
    killer: Arc<dyn PtyKiller>,
    sender: mpsc::Sender<String>,
    cancel: CancellationToken,
    drain: tokio::task::JoinHandle<()>,
    wait_thread: Option<std::thread::JoinHandle<()>>,
    killed: Arc<AtomicBool>,
    #[allow(dead_code)]
    dropped_chunks: Arc<AtomicUsize>,
}

/// Tauri-agnostic pool of live terminal sub-sessions, keyed by
/// [`SubSessionId`]. Mirrors [`crate::pty_pool::PtyPool`] in shape but
/// without session-specific telemetry / temp-dir prep.
pub struct SubPtyPool {
    spawner: Arc<dyn PtySpawner>,
    inner: Arc<Mutex<BTreeMap<SubSessionId, SubRuntime>>>,
}

impl SubPtyPool {
    #[must_use]
    pub fn new(spawner: Arc<dyn PtySpawner>) -> Self {
        Self {
            spawner,
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, id: &SubSessionId) -> bool {
        self.inner.lock().map(|g| g.contains_key(id)).unwrap_or(false)
    }

    pub fn pid_of(&self, id: &SubSessionId) -> Option<u32> {
        self.inner.lock().ok().and_then(|g| g.get(id).map(|rt| rt.pid))
    }

    /// Spawn `composed_command` under the platform shell at `cwd`. Reuses
    /// [`crate::compose::platform_shell`] so the wrapper matches the
    /// session pool exactly. Returns the assigned PID.
    pub fn spawn_terminal(&self, id: SubSessionId, composed_command: String, cwd: PathBuf, sink: SubPtySink) -> Result<u32, Error> {
        let shell = crate::compose::platform_shell();
        let cmd = ChildCommand {
            program: shell.program.clone(),
            args: vec![shell.flag.to_string(), composed_command],
            env: Vec::new(),
        };
        self.spawn_raw(id, cmd, cwd, sink)
    }

    /// Lower-level spawn used by Phase 3's application launcher tests (and the public terminal entrypoint above). Bypasses the platform shell wrapper
    /// — the caller composes `cmd` directly.
    pub fn spawn_raw(&self, id: SubSessionId, cmd: ChildCommand, cwd: PathBuf, sink: SubPtySink) -> Result<u32, Error> {
        let spawned = self.spawner.spawn(cmd, &cwd, DEFAULT_SUB_PTY_SIZE)?;
        let SpawnedChild {
            pid,
            reader,
            writer,
            resize,
            waiter,
            killer,
        } = spawned;

        // ----- transactional setup -------------------------------------- Track resources we have to clean up if any subsequent step fails. On the
        // happy path we `take()` each piece into the `SubRuntime` and `forget()` the rollback set.
        let killer_for_rollback = killer.clone();
        let rollback = |reason: &str| {
            // Best-effort: kill the child if we haven't recorded it yet.
            if let Err(e) = killer_for_rollback.kill() {
                error!(
                    sub_session_id = %id,
                    pid,
                    error = ?e,
                    "spawn rollback: failed to kill orphaned child after {reason}"
                );
            }
        };

        let (tx, mut rx) = mpsc::channel::<String>(OUTPUT_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let drain_cancel = cancel.clone();
        let drain_sink = sink.clone();
        let drain_id = id;
        let drain = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = drain_cancel.cancelled() => break,
                    msg = rx.recv() => {
                        match msg {
                            Some(chunk) => (drain_sink.output)(&drain_id, chunk),
                            None => break,
                        }
                    }
                }
            }
        });

        let dropped = Arc::new(AtomicUsize::new(0));
        let killed = Arc::new(AtomicBool::new(false));

        let read_id = id;
        let read_tx = tx.clone();
        let read_dropped = Arc::clone(&dropped);
        if let Err(e) = std::thread::Builder::new()
            .name(format!("arborist-sub-pty-read-{pid}"))
            .spawn(move || sub_read_loop(read_id, reader, read_tx, read_dropped))
        {
            cancel.cancel();
            drain.abort();
            rollback("read thread spawn failure");
            return Err(Error::PtySpawnFailed(format!("spawn sub read thread failed: {e}")));
        }

        // Hand the wait thread a self-cleanup closure so naturally- exited sub-sessions don't accumulate runtime state in the pool. We only
        // Weak-reference `inner` so that pool teardown (Drop) doesn't have to wait for the thread.
        let wait_id = id;
        let wait_sink = sink.clone();
        let wait_killed = Arc::clone(&killed);
        let weak_inner = Arc::downgrade(&self.inner);
        let cleanup = move || {
            if let Some(strong) = weak_inner.upgrade() {
                if let Ok(mut guard) = strong.lock() {
                    if let Some(rt) = guard.remove(&wait_id) {
                        rt.cancel.cancel();
                        rt.drain.abort();
                    }
                }
            }
        };
        let wait_thread = match std::thread::Builder::new()
            .name(format!("arborist-sub-pty-wait-{pid}"))
            .spawn(move || sub_wait_loop(wait_id, waiter, wait_sink, wait_killed, cleanup))
        {
            Ok(h) => h,
            Err(e) => {
                cancel.cancel();
                drain.abort();
                // The read thread is detached; killing the child closes its PTY which causes `read` to return Ok(0) and the loop exits.
                rollback("wait thread spawn failure");
                return Err(Error::PtySpawnFailed(format!("spawn sub wait thread failed: {e}")));
            }
        };

        {
            let mut guard = self.inner.lock().map_err(|_| Error::Internal("sub pty pool mutex poisoned".into()))?;
            guard.insert(
                id,
                SubRuntime {
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

        (sink.status)(&id, SubSessionStatus::Running, Some(pid), None);
        Ok(pid)
    }

    pub fn write(&self, id: &SubSessionId, data: &[u8]) -> Result<(), Error> {
        let writer = {
            let guard = self.inner.lock().map_err(|_| Error::Internal("sub pty pool mutex poisoned".into()))?;
            let rt = guard.get(id).ok_or_else(|| Error::NotFound(format!("sub session {id} not in pool")))?;
            Arc::clone(&rt.writer)
        };
        let mut g = writer.lock().map_err(|_| Error::Internal("sub pty writer mutex poisoned".into()))?;
        g.write_all(data).map_err(|e| Error::PtyWriteFailed(format!("write failed: {e}")))?;
        g.flush().map_err(|e| Error::PtyWriteFailed(format!("flush failed: {e}")))
    }

    pub fn resize(&self, id: &SubSessionId, cols: u16, rows: u16) -> Result<(), Error> {
        let resize = {
            let guard = self.inner.lock().map_err(|_| Error::Internal("sub pty pool mutex poisoned".into()))?;
            let rt = guard.get(id).ok_or_else(|| Error::NotFound(format!("sub session {id} not in pool")))?;
            Arc::clone(&rt.resize)
        };
        resize.resize(cols, rows)
    }

    /// Kill the child, tear down its read/wait threads + drain task, remove the entry. Mirrors [`PtyPool::kill`] (sans temp-dir cleanup since
    /// sub-sessions don't own one).
    ///
    /// Returns [`KillOutcome::Reaped`] when both `killer.kill()` and the wait-thread join succeeded within [`KILL_GRACE`]; returns
    /// [`KillOutcome::Unconfirmed`] when either signalled failure (the
    /// underlying OS process **may** still be alive at the recorded PID). The pool entry has been removed in either case so callers can safely
    /// re-spawn the same id; `Unconfirmed` is an advisory signal for cascade callers to keep an orphan record visible per CP-07 rather than silently
    /// leak a runaway process. `NotFound` is returned only when the id was already absent from the pool.
    pub async fn kill(&self, id: &SubSessionId) -> Result<KillOutcome, Error> {
        let rt = {
            let mut guard = self.inner.lock().map_err(|_| Error::Internal("sub pty pool mutex poisoned".into()))?;
            guard.remove(id)
        };
        let Some(rt) = rt else {
            return Err(Error::NotFound(format!("sub session {id} not in pool")));
        };
        let pid = rt.pid;
        rt.killed.store(true, Ordering::SeqCst);
        // Capture the killer result instead of swallowing it. SIGKILL / TerminateProcess rarely fail in practice, but when they do the OS process can
        // outlive the pool entry; cascade callers need this signal to keep the orphan visible per CP-07. Mirrors the change applied to
        // `PtyPool::kill` (see pty_pool.rs §"step 3").
        let killer_result = rt.killer.kill();
        drop(rt.sender);
        rt.cancel.cancel();
        match timeout(DRAIN_JOIN_TIMEOUT, rt.drain).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(sub_session_id = %id, error = ?e, "sub drain join failed"),
            Err(_) => error!(sub_session_id = %id, "sub drain did not exit within timeout"),
        }
        let wait_joined = if let Some(handle) = rt.wait_thread {
            matches!(
                timeout(KILL_GRACE, tokio::task::spawn_blocking(move || handle.join()),).await,
                Ok(Ok(Ok(()))),
            )
        } else {
            // No wait thread (some test paths). Treat as confirmed since there is nothing left to verify.
            true
        };
        if killer_result.is_err() || !wait_joined {
            if let Err(e) = killer_result {
                warn!(
                    sub_session_id = %id,
                    pid,
                    error = ?e,
                    "sub kill: killer.kill() returned error; process may still be alive at this PID",
                );
            }
            if !wait_joined {
                warn!(
                    sub_session_id = %id,
                    pid,
                    grace_secs = KILL_GRACE.as_secs(),
                    "sub kill: wait thread did not join within grace period; process may still be alive at this PID",
                );
            }
            Ok(KillOutcome::Unconfirmed { pid })
        } else {
            Ok(KillOutcome::Reaped)
        }
    }

    fn kill_blocking(&self, id: &SubSessionId) {
        let rt = match self.inner.lock() {
            Ok(mut g) => g.remove(id),
            Err(_) => None,
        };
        let Some(rt) = rt else { return };
        rt.killed.store(true, Ordering::SeqCst);
        let _ = rt.killer.kill();
        rt.cancel.cancel();
        drop(rt.sender);
        rt.drain.abort();
    }
}

impl Drop for SubPtyPool {
    fn drop(&mut self) {
        let ids: Vec<SubSessionId> = self.inner.lock().map(|g| g.keys().copied().collect()).unwrap_or_default();
        for id in ids {
            self.kill_blocking(&id);
        }
    }
}

fn sub_read_loop(id: SubSessionId, mut reader: Box<dyn Read + Send>, sender: mpsc::Sender<String>, dropped: Arc<AtomicUsize>) {
    let mut decoder = Utf8Stream::default();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let decoded = decoder.feed(&buf[..n]);
                if decoded.is_empty() {
                    continue;
                }
                if let Err(e) = sender.try_send(decoded) {
                    match e {
                        mpsc::error::TrySendError::Full(_) => {
                            let n = dropped.fetch_add(1, Ordering::Relaxed) + 1;
                            if n.is_multiple_of(DROP_LOG_EVERY) {
                                warn!(
                                    target: "arborist::sub_pty",
                                    sub_session_id = %id,
                                    drop_count = n,
                                    "sub_pty.backpressure dropping output chunks"
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
    let tail = decoder.flush();
    if !tail.is_empty() {
        let _ = sender.try_send(tail);
    }
}

fn sub_wait_loop(id: SubSessionId, waiter: Box<dyn PtyWaiter>, sink: SubPtySink, killed: Arc<AtomicBool>, cleanup: impl FnOnce() + Send + 'static) {
    let result = waiter.wait();
    let was_killed = killed.load(Ordering::SeqCst);
    // Always clean up runtime state — whether the exit was natural or triggered by `kill()` we no longer want to retain writers/handles.
    cleanup();
    if was_killed {
        return;
    }
    let status = match result {
        Ok(exit) if exit.success() => SubSessionStatus::Exited,
        Ok(_) | Err(_) => SubSessionStatus::Error,
    };
    (sink.status)(&id, status, None, None);
}

// --------------------------------------------------------------------------- SubSessionStore — in-memory metadata indexed by id and parent
// ---------------------------------------------------------------------------

/// Live metadata for the sub-sessions currently tracked by the runtime. Persistence (the lightweight [`crate::types::SubSessionRecord`] list) lives
/// in [`crate::config_store`]; this struct is the in-memory view that command handlers read/write while the app is running.
#[derive(Default)]
pub struct SubSessionStore {
    inner: Mutex<StoreInner>,
}

#[derive(Default)]
struct StoreInner {
    by_id: BTreeMap<SubSessionId, SubSession>,
    /// Insertion order per parent, so the sidebar renders sub-tabs underneath their parent in the same order they were created.
    by_parent: BTreeMap<SessionId, Vec<SubSessionId>>,
}

impl SubSessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a freshly-created sub-session. Returns an error if the id is already present (caller bug — UUIDs collide once per universe).
    pub fn insert(&self, sub: SubSession) -> Result<(), Error> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| Error::Internal("sub session store mutex poisoned".into()))?;
        if g.by_id.contains_key(&sub.id) {
            return Err(Error::Internal(format!("sub session {} already exists", sub.id)));
        }
        g.by_parent.entry(sub.parent_session_id).or_default().push(sub.id);
        g.by_id.insert(sub.id, sub);
        Ok(())
    }

    /// Update the [`SubSessionStatus`] of an existing sub-session and optionally its PID. No-op (returns `Ok`) if the id is unknown — the wait thread
    /// can race `subsession_close` and that's not a caller bug. The PID is forced to `None` for terminal states (`Exited`/`Error`) regardless of the
    /// supplied value.
    pub fn set_status(&self, id: &SubSessionId, status: SubSessionStatus, pid: Option<u32>) -> Result<(), Error> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| Error::Internal("sub session store mutex poisoned".into()))?;
        if let Some(sub) = g.by_id.get_mut(id) {
            sub.status = status;
            if matches!(status, SubSessionStatus::Exited | SubSessionStatus::Error) {
                sub.pid = None;
            } else if pid.is_some() {
                sub.pid = pid;
            }
        }
        Ok(())
    }

    /// Remove a sub-session by id. Returns the removed value (or `None`).
    pub fn remove(&self, id: &SubSessionId) -> Option<SubSession> {
        let mut g = self.inner.lock().ok()?;
        let sub = g.by_id.remove(id)?;
        if let Some(list) = g.by_parent.get_mut(&sub.parent_session_id) {
            list.retain(|x| x != id);
            if list.is_empty() {
                g.by_parent.remove(&sub.parent_session_id);
            }
        }
        Some(sub)
    }

    /// Snapshot of all sub-sessions, parent-grouped insertion order preserved within each parent group, parents in no guaranteed order. Use
    /// [`Self::list_for`] when the parent is known.
    pub fn list_all(&self) -> Vec<SubSession> {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::with_capacity(g.by_id.len());
        for (_parent, ids) in g.by_parent.iter() {
            for id in ids {
                if let Some(sub) = g.by_id.get(id) {
                    out.push(sub.clone());
                }
            }
        }
        out
    }

    /// Sub-sessions belonging to `parent`, in insertion order.
    pub fn list_for(&self, parent: &SessionId) -> Vec<SubSession> {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let Some(ids) = g.by_parent.get(parent) else {
            return Vec::new();
        };
        ids.iter().filter_map(|id| g.by_id.get(id).cloned()).collect()
    }

    pub fn get(&self, id: &SubSessionId) -> Option<SubSession> {
        self.inner.lock().ok().and_then(|g| g.by_id.get(id).cloned())
    }
}

// --------------------------------------------------------------------------- SubAppContext — managed Tauri state combining pool + store + sink
// ---------------------------------------------------------------------------

/// Wiring shared by every Phase 2+ sub-session command handler. Held in Tauri managed state alongside the existing [`crate::commands::AppContext`].
pub struct SubAppContext {
    pub pool: Arc<SubPtyPool>,
    pub store: Arc<SubSessionStore>,
    pub sink: SubPtySink,
    /// Phase 3: pool for application-kind sub-sessions (no PTY).
    pub app_pool: Arc<crate::app_launcher::AppPool>,
    /// Phase 3: window focuser used by `subsession_focus` for application-kind sub-sessions.
    pub focuser: Arc<dyn crate::window_focus::WindowFocuser>,
    /// Best-effort process-icon cache for application sub-tabs. See `crate::process_icon` module docs for the trade-offs (cache keyed by exe path, no
    /// negative caching).
    pub icon_cache: Arc<crate::process_icon::IconCache>,
}

impl SubAppContext {
    #[must_use]
    pub fn new(
        pool: Arc<SubPtyPool>,
        store: Arc<SubSessionStore>,
        sink: SubPtySink,
        app_pool: Arc<crate::app_launcher::AppPool>,
        focuser: Arc<dyn crate::window_focus::WindowFocuser>,
        icon_cache: Arc<crate::process_icon::IconCache>,
    ) -> Self {
        Self {
            pool,
            store,
            sink,
            app_pool,
            focuser,
            icon_cache,
        }
    }
}

// --------------------------------------------------------------------------- Helpers
// ---------------------------------------------------------------------------

/// Make a [`SubSession`] ready to insert into the store. The returned `composed_command` mirrors the session-pool rule: the launch command is
/// captured once at creation time (DESIGN §5.4 "compose once, store-and-reuse"); later edits to the source
/// [`crate::types::CustomProcessDef`] do not retroactively rewrite
/// already-running sub-sessions.
#[must_use]
pub fn build_sub_session(parent_session_id: SessionId, def: &crate::types::CustomProcessDef, composed_command: String) -> SubSession {
    SubSession {
        id: SubSessionId::default(),
        parent_session_id,
        def_id: def.id.clone(),
        kind: def.kind,
        label: def.name.clone(),
        status: SubSessionStatus::Starting,
        pid: None,
        composed_command,
        created_at: now_unix_seconds(),
    }
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|_| {
            tracing::warn!("system clock before UNIX epoch; using 0");
            0
        })
}

/// Compute the cwd a sub-session should spawn under: the parent session's worktree path. Centralised so future tools that need alternative working
/// directories (e.g. project-root over worktree) have a single place to override.
#[must_use]
pub fn sub_session_cwd(parent: &crate::types::Session) -> &Path {
    parent.worktree_path.as_path()
}

// --------------------------------------------------------------------------- Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty_pool::{ChildCommand, SpawnedChild};
    use portable_pty::ExitStatus;
    use std::io::{self, Read, Write};
    use std::sync::Mutex as StdMutex;

    // ----- fakes -------------------------------------------------------

    /// Test-only Read implementation backed by a shared byte queue. Blocking-poll model so it behaves like a real PTY reader.
    struct FakeReader {
        queue: Arc<StdMutex<Vec<u8>>>,
        eof: Arc<AtomicBool>,
    }
    impl FakeReader {
        fn new(queue: Arc<StdMutex<Vec<u8>>>, eof: Arc<AtomicBool>) -> Self {
            Self { queue, eof }
        }
    }
    impl Read for FakeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                {
                    let mut q = self.queue.lock().unwrap();
                    if !q.is_empty() {
                        let n = buf.len().min(q.len());
                        buf[..n].copy_from_slice(&q[..n]);
                        q.drain(..n);
                        return Ok(n);
                    }
                }
                if self.eof.load(Ordering::SeqCst) {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    struct FakeWriter {
        captured: Arc<StdMutex<Vec<u8>>>,
    }
    impl Write for FakeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.captured.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeResize;
    impl PtyResize for FakeResize {
        fn resize(&self, _cols: u16, _rows: u16) -> Result<(), Error> {
            Ok(())
        }
    }

    struct FakeWaiter {
        exit_signal: Arc<AtomicBool>,
    }
    impl PtyWaiter for FakeWaiter {
        fn wait(self: Box<Self>) -> Result<ExitStatus, Error> {
            while !self.exit_signal.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(ExitStatus::with_exit_code(0))
        }
    }

    struct FakeKiller {
        exit_signal: Arc<AtomicBool>,
    }
    impl PtyKiller for FakeKiller {
        fn kill(&self) -> Result<(), Error> {
            self.exit_signal.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Per-spawn handles the test can poke to push input / signal exit.
    struct FakeChild {
        reader_queue: Arc<StdMutex<Vec<u8>>>,
        reader_eof: Arc<AtomicBool>,
        writer_captured: Arc<StdMutex<Vec<u8>>>,
        exit_signal: Arc<AtomicBool>,
    }

    impl FakeChild {
        fn push(&self, bytes: &[u8]) {
            self.reader_queue.lock().unwrap().extend_from_slice(bytes);
        }
        fn close_reader(&self) {
            self.reader_eof.store(true, Ordering::SeqCst);
        }
        fn signal_exit(&self) {
            self.exit_signal.store(true, Ordering::SeqCst);
        }
    }

    struct FakeSpawner {
        next_pid: AtomicUsize,
        children: StdMutex<Vec<Arc<FakeChild>>>,
    }

    impl FakeSpawner {
        fn new() -> Self {
            Self {
                next_pid: AtomicUsize::new(1000),
                children: StdMutex::new(Vec::new()),
            }
        }
        fn child(&self, idx: usize) -> Arc<FakeChild> {
            Arc::clone(&self.children.lock().unwrap()[idx])
        }
    }

    impl PtySpawner for FakeSpawner {
        fn spawn(&self, _cmd: ChildCommand, _cwd: &Path, _size: PtySize) -> Result<SpawnedChild, Error> {
            let reader_queue = Arc::new(StdMutex::new(Vec::new()));
            let reader_eof = Arc::new(AtomicBool::new(false));
            let writer_captured = Arc::new(StdMutex::new(Vec::new()));
            let exit_signal = Arc::new(AtomicBool::new(false));

            let child = Arc::new(FakeChild {
                reader_queue: Arc::clone(&reader_queue),
                reader_eof: Arc::clone(&reader_eof),
                writer_captured: Arc::clone(&writer_captured),
                exit_signal: Arc::clone(&exit_signal),
            });
            self.children.lock().unwrap().push(child);

            let pid = self.next_pid.fetch_add(1, Ordering::SeqCst) as u32;
            Ok(SpawnedChild {
                pid,
                reader: Box::new(FakeReader::new(reader_queue, reader_eof)),
                writer: Box::new(FakeWriter { captured: writer_captured }),
                resize: Arc::new(FakeResize),
                waiter: Box::new(FakeWaiter {
                    exit_signal: Arc::clone(&exit_signal),
                }),
                killer: Arc::new(FakeKiller { exit_signal }),
            })
        }
    }

    // ----- store tests --------------------------------------------------

    fn fake_sub(parent: SessionId, label: &str) -> SubSession {
        SubSession {
            id: SubSessionId::default(),
            parent_session_id: parent,
            def_id: crate::types::CustomProcessDefId::new("shell"),
            kind: crate::types::CustomProcessKind::Terminal,
            label: label.to_owned(),
            status: SubSessionStatus::Starting,
            pid: None,
            composed_command: "sh -i".to_owned(),
            created_at: 0,
        }
    }

    #[test]
    fn store_preserves_per_parent_insertion_order() {
        let store = SubSessionStore::new();
        let parent = SessionId::new();
        let a = fake_sub(parent, "a");
        let b = fake_sub(parent, "b");
        let c = fake_sub(parent, "c");
        let (ai, bi, ci) = (a.id, b.id, c.id);
        store.insert(a).unwrap();
        store.insert(b).unwrap();
        store.insert(c).unwrap();
        let listed: Vec<_> = store.list_for(&parent).into_iter().map(|s| s.id).collect();
        assert_eq!(listed, vec![ai, bi, ci]);
    }

    #[test]
    fn store_remove_drops_index_and_parent_bucket() {
        let store = SubSessionStore::new();
        let parent = SessionId::new();
        let a = fake_sub(parent, "a");
        let aid = a.id;
        store.insert(a).unwrap();
        assert!(store.get(&aid).is_some());
        store.remove(&aid).expect("removed");
        assert!(store.get(&aid).is_none());
        assert!(store.list_for(&parent).is_empty());
    }

    #[test]
    fn store_set_status_clears_pid_on_exit() {
        let store = SubSessionStore::new();
        let parent = SessionId::new();
        let mut s = fake_sub(parent, "x");
        s.pid = Some(42);
        s.status = SubSessionStatus::Running;
        let id = s.id;
        store.insert(s).unwrap();
        store.set_status(&id, SubSessionStatus::Exited, None).unwrap();
        let after = store.get(&id).unwrap();
        assert_eq!(after.status, SubSessionStatus::Exited);
        assert_eq!(after.pid, None);
    }

    #[test]
    fn store_set_status_unknown_id_is_ok() {
        let store = SubSessionStore::new();
        let id = SubSessionId::default();
        // Race: wait thread fires after subsession_close already removed.
        store.set_status(&id, SubSessionStatus::Exited, None).unwrap();
    }

    #[test]
    fn store_insert_duplicate_id_errors() {
        let store = SubSessionStore::new();
        let parent = SessionId::new();
        let s = fake_sub(parent, "x");
        let dup = SubSession { ..s.clone() };
        store.insert(s).unwrap();
        let err = store.insert(dup).expect_err("dup");
        assert!(matches!(err, Error::Internal(_)));
    }

    // ----- pool tests ---------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_spawn_emits_running_then_exit() {
        let spawner = Arc::new(FakeSpawner::new());
        let pool = SubPtyPool::new(spawner.clone());
        type StatusObs = Arc<StdMutex<Vec<(SubSessionStatus, Option<u32>)>>>;
        let observed: StatusObs = Arc::new(StdMutex::new(Vec::new()));
        let observed_for_status = Arc::clone(&observed);
        let sink = SubPtySink::new(
            Arc::new(|_, _| {}),
            Arc::new(move |_, status, pid, _| {
                observed_for_status.lock().unwrap().push((status, pid));
            }),
            Arc::new(|_, _| {}),
            Arc::new(|_| {}),
        );

        let id = SubSessionId::default();
        let pid = pool.spawn_terminal(id, "echo hi".to_owned(), PathBuf::from("."), sink).expect("spawn");
        assert!(pid >= 1000);
        assert!(pool.contains(&id));

        // Trigger the fake child to exit.
        let child = spawner.child(0);
        child.signal_exit();
        child.close_reader();

        // Wait for the wait-thread to emit Exited.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if observed.lock().unwrap().iter().any(|(s, _)| matches!(s, SubSessionStatus::Exited)) {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("never observed Exited; events: {:?}", observed.lock().unwrap());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let events = observed.lock().unwrap().clone();
        assert!(matches!(events.first(), Some((SubSessionStatus::Running, Some(_)))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_natural_exit_removes_runtime() {
        let spawner = Arc::new(FakeSpawner::new());
        let pool = SubPtyPool::new(spawner.clone());
        let id = SubSessionId::default();
        pool.spawn_terminal(id, "echo".to_owned(), PathBuf::from("."), SubPtySink::noop())
            .expect("spawn");
        assert!(pool.contains(&id));

        // Simulate natural exit: child process ends, reader closes.
        let child = spawner.child(0);
        child.signal_exit();
        child.close_reader();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !pool.contains(&id) {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("runtime was not removed after natural exit");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_kill_removes_runtime() {
        let spawner = Arc::new(FakeSpawner::new());
        let pool = SubPtyPool::new(spawner);
        let id = SubSessionId::default();
        pool.spawn_terminal(id, "sleep".to_owned(), PathBuf::from("."), SubPtySink::noop())
            .expect("spawn");
        assert!(pool.contains(&id));
        pool.kill(&id).await.expect("kill");
        assert!(!pool.contains(&id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_output_flows_to_sink() {
        let spawner = Arc::new(FakeSpawner::new());
        let pool = SubPtyPool::new(spawner.clone());
        let captured: Arc<StdMutex<String>> = Arc::new(StdMutex::new(String::new()));
        let captured_for_out = Arc::clone(&captured);
        let sink = SubPtySink::new(
            Arc::new(move |_, chunk| captured_for_out.lock().unwrap().push_str(&chunk)),
            Arc::new(|_, _, _, _| {}),
            Arc::new(|_, _| {}),
            Arc::new(|_| {}),
        );
        let id = SubSessionId::default();
        pool.spawn_terminal(id, "x".to_owned(), PathBuf::from("."), sink).expect("spawn");
        spawner.child(0).push(b"hello world\n");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if captured.lock().unwrap().contains("hello world") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("output never reached sink");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = pool.kill(&id).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_write_round_trips_to_writer() {
        let spawner = Arc::new(FakeSpawner::new());
        let pool = SubPtyPool::new(spawner.clone());
        let id = SubSessionId::default();
        pool.spawn_terminal(id, "x".to_owned(), PathBuf::from("."), SubPtySink::noop())
            .expect("spawn");

        pool.write(&id, b"input bytes").expect("write");
        let captured = spawner.child(0).writer_captured.lock().unwrap().clone();
        assert_eq!(captured, b"input bytes");
        let _ = pool.kill(&id).await;
    }
}
