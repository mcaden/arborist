//! Application sub-session launcher (Phase 3 of `dev/ai/CONTEXT_MENU_PLAN.md`).
//!
//! Application-kind sub-tabs are external GUI processes (VS Code, Finder,
//! Explorer, etc.) launched into the parent session's worktree. Unlike
//! terminal sub-tabs they do **not** allocate a PTY: stdio is dropped and
//! the only signals we surface back to the UI are start (synchronous from
//! `spawn`) and exit (`subsession://exited` / status `Exited`).
//!
//! ## Honest limitations
//!
//! Many real-world app launchers are *delegators*: `code .`, `xdg-open .`,
//! `open .`, and `explorer .` typically hand off to an existing instance
//! and exit immediately. The PID we capture is the launcher's, not the
//! eventual GUI window's. This module is honest about that: the wait
//! thread will report `Exited` very quickly for those commands and a
//! later `focus_pid` may be a no-op. The frontend should treat
//! `subsession://exited` as informational and not assume the user closed
//! a window.
//!
//! ## Survival of the parent
//!
//! Spawned children inherit no controlling terminal and (on Unix) get a
//! new process session via `setsid` so closing Arborist doesn't take the
//! external app down with it. We still keep the [`std::process::Child`]
//! handle and use blocking `wait()` in a thread — detachment doesn't
//! preclude waiting on a child we own.
//!
//! ## Public surface
//!
//! - [`AppSpawner`] — trait seam over `std::process::Command`. Real impl
//!   is [`RealAppSpawner`]; tests use [`tests::FakeAppSpawner`].
//! - [`AppPool`] — runtime pool for application sub-sessions.
//! - [`AppPoolSink`] — alias for [`crate::sub_sessions::SubPtySink`]
//!   reused so a single sink type drives both pool flavours.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::sub_sessions::SubPtySink;
use crate::types::{Error, SubSessionId, SubSessionStatus};

/// Re-exported alias so call sites can name a single sink type.
pub type AppPoolSink = SubPtySink;

// ---------------------------------------------------------------------------
// Spawner trait
// ---------------------------------------------------------------------------

/// Anything that can launch a detached child process and return a
/// handle plus a waiter / killer pair. The trait exists so unit tests
/// can swap in a fake without touching the real OS.
pub trait AppSpawner: Send + Sync + 'static {
    /// Spawn `cmd` (a shell command string — wrapped in
    /// `cmd /c …` on Windows / `sh -c …` elsewhere) inside `cwd`.
    /// Returns the captured PID plus a [`SpawnedApp`] handle.
    fn spawn(&self, cmd: &str, cwd: &Path) -> Result<SpawnedApp, Error>;
}

/// Per-spawn handle: PID + waiter + killer. The waiter is consumed by
/// the wait thread (blocking `wait()`); the killer is retained in the
/// pool so explicit closes can terminate the launcher.
pub struct SpawnedApp {
    pub pid: u32,
    pub waiter: Box<dyn AppWaiter>,
    pub killer: Arc<dyn AppKiller>,
}

/// Blocking-wait abstraction for a spawned app. Implementors should not
/// share a single waiter across threads — the trait takes `self: Box<Self>`
/// so callers cannot accidentally call `wait` twice.
pub trait AppWaiter: Send + 'static {
    /// Block until the spawned process exits and return whether it
    /// exited successfully.
    fn wait(self: Box<Self>) -> Result<bool, Error>;
}

/// Kill abstraction for a spawned app. Cloning is cheap (typically an
/// `Arc` over a `Mutex<Option<Child>>`).
pub trait AppKiller: Send + Sync + 'static {
    /// Best-effort termination. Returns `Ok` even if the process has
    /// already exited (that's a benign race).
    fn kill(&self) -> Result<(), Error>;
}

// ---------------------------------------------------------------------------
// PID-by-PID killer (for re-targeted owners)
// ---------------------------------------------------------------------------

/// [`AppKiller`] keyed on a raw OS PID rather than a [`Child`] handle.
///
/// Used after [`OwnerResolver`] re-targets an [`AppRuntime`] to a
/// long-lived editor process the launcher handed off to (e.g. VS Code's
/// `Code.exe`). The launcher's [`RealKiller`] is moot at that point —
/// the launcher has exited and its `Child` slot is empty — so we swap
/// it for a [`PidKiller`] that issues `TerminateProcess` (Windows) or
/// `kill(SIGTERM)` (Unix) directly against the rediscovered PID. This
/// preserves the property that `pool.kill(id)` actually terminates the
/// process the user can see.
///
/// `kill` is best-effort and returns `Ok` even if the process has
/// already exited; the OS-level "no such process" error is benign.
pub struct PidKiller {
    pid: u32,
}

impl PidKiller {
    #[must_use]
    pub fn new(pid: u32) -> Self {
        Self { pid }
    }
}

impl AppKiller for PidKiller {
    fn kill(&self) -> Result<(), Error> {
        platform_kill_pid(self.pid)
    }
}

#[cfg(target_os = "windows")]
fn platform_kill_pid(pid: u32) -> Result<(), Error> {
    use std::ffi::c_void;
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type HANDLE = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    const PROCESS_TERMINATE: DWORD = 0x0001;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn TerminateProcess(handle: HANDLE, exit_code: u32) -> BOOL;
        fn CloseHandle(handle: HANDLE) -> BOOL;
    }
    // Best-effort: a NULL handle means the PID is already gone (benign
    // race) or we lack permission (rare for processes we spawned).
    // SAFETY: passing a literal access mask + PID; OpenProcess is
    // documented to handle invalid PIDs by returning NULL.
    let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if h.is_null() {
        return Ok(());
    }
    // SAFETY: `h` is a valid handle; exit code is arbitrary (1).
    unsafe {
        TerminateProcess(h, 1);
        CloseHandle(h);
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn platform_kill_pid(pid: u32) -> Result<(), Error> {
    // SAFETY: kill(2) is async-signal-safe; passing a possibly-stale
    // PID is documented to return ESRCH which we treat as benign.
    unsafe {
        let _ = libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Owner resolution / liveness (re-target after launcher exit)
// ---------------------------------------------------------------------------

/// Strategy for re-discovering the long-lived owner process after a
/// delegating launcher exits. Set per-spawn — most apps don't need one
/// (pass `None` to [`AppPool::spawn`]); VS Code does (its `code.cmd`
/// launcher hands off to `Code.exe` and exits within ~1 s).
///
/// Implementations encapsulate their own polling / timeout strategy:
/// `resolve()` blocks until a match is found OR a per-resolver
/// deadline expires (returns `None`). Called from a dedicated
/// `arborist-app-resolve-<pid>` thread so polling does not steal time
/// from the wait thread.
pub trait OwnerResolver: Send + Sync + 'static {
    /// Block until the rediscovered owner is identified or the
    /// resolver's internal timeout expires.
    fn resolve(&self) -> Option<RetargetedOwner>;
}

/// Bundle returned by [`OwnerResolver::resolve`].
///
/// `liveness` MUST observe the same PID as `pid` — the [`AppPool`]
/// hands the probe to a dedicated thread that emits `Exited` once
/// `wait_for_death` returns, and falsely reporting death would emit a
/// spurious tab-exit event.
pub struct RetargetedOwner {
    pub pid: u32,
    pub killer: Arc<dyn AppKiller>,
    pub liveness: Box<dyn LivenessProbe>,
}

/// Blocking wait for a re-targeted PID to die. Implementors are free
/// to poll or use OS-level wait (e.g. `WaitForSingleObject` on a
/// `SYNCHRONIZE` handle). Called from a dedicated
/// `arborist-app-liveness-<pid>` thread; `wait_for_death` is consumed
/// (`Box<Self>`) so it cannot be invoked twice.
pub trait LivenessProbe: Send + 'static {
    fn wait_for_death(self: Box<Self>);
}

// ---------------------------------------------------------------------------
// Real spawner
// ---------------------------------------------------------------------------

/// Production [`AppSpawner`] backed by [`std::process::Command`] with a
/// platform shell wrapper. Suppresses stdio and detaches from the parent
/// process group/session so the spawned app survives Arborist exiting.
#[derive(Default)]
pub struct RealAppSpawner;

impl AppSpawner for RealAppSpawner {
    fn spawn(&self, cmd: &str, cwd: &Path) -> Result<SpawnedApp, Error> {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return Err(Error::AppSpawnFailed("empty command".to_owned()));
        }

        // Best-effort preflight: if the command's first token looks like
        // a plain executable name (no shell metacharacters), verify it
        // exists on PATH. This converts the most common failure mode —
        // user typed `code` but VS Code's CLI isn't installed — into a
        // typed `ToolMissing` synchronously, instead of a delayed
        // shell-exit-with-status-Error. Skipped for commands that use
        // shell features (pipes, redirects, env expansion, etc.) since
        // the first token isn't necessarily the executable.
        if let Some(tool) = first_token_if_simple(trimmed) {
            if which_in_path(&tool).is_none() {
                return Err(Error::ToolMissing(tool));
            }
        }

        let mut command = build_shell_command(trimmed);
        command
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_detach(&mut command);

        let child = command
            .spawn()
            .map_err(|e| Error::AppSpawnFailed(format!("spawn `{trimmed}`: {e}")))?;
        let pid = child.id();
        let shared: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));

        Ok(SpawnedApp {
            pid,
            waiter: Box::new(RealWaiter {
                child: Arc::clone(&shared),
            }),
            killer: Arc::new(RealKiller { child: shared }),
        })
    }
}

/// Returns the first whitespace-delimited token of `cmd` iff the command
/// contains no shell metacharacters (so the first token is unambiguously
/// the executable name).
fn first_token_if_simple(cmd: &str) -> Option<String> {
    const META: &[char] = &[
        '|', '&', ';', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', ']', '{', '}',
        '~', '=',
    ];
    if cmd.contains(META) {
        return None;
    }
    let tok = cmd.split_whitespace().next()?;
    Some(tok.to_owned())
}

/// Best-effort "is this on PATH?" lookup. Returns the absolute path if
/// found, `None` otherwise. On Windows, also tries each PATHEXT
/// extension. Treats absolute / relative paths as already-resolved.
fn which_in_path(tool: &str) -> Option<PathBuf> {
    let p = Path::new(tool);
    if p.is_absolute() || tool.contains('/') || tool.contains('\\') {
        return if p.is_file() {
            Some(p.to_owned())
        } else {
            None
        };
    }
    let path_var = std::env::var_os("PATH")?;
    #[cfg(target_os = "windows")]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    for dir in std::env::split_paths(&path_var) {
        let direct = dir.join(tool);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(target_os = "windows")]
        for ext in &exts {
            let candidate = dir.join(format!("{tool}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn build_shell_command(cmd: &str) -> std::process::Command {
    let comspec = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut c = std::process::Command::new(comspec);
    c.arg("/c").arg(cmd);
    c
}

#[cfg(not(target_os = "windows"))]
fn build_shell_command(cmd: &str) -> std::process::Command {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut c = std::process::Command::new(shell);
    c.arg("-c").arg(cmd);
    c
}

#[cfg(target_os = "windows")]
fn configure_detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NEW_PROCESS_GROUP (0x0200) so Ctrl+Break to Arborist
    // doesn't propagate; DETACHED_PROCESS (0x0008) so the child has no
    // console attached. Combined, the child survives Arborist exiting.
    cmd.creation_flags(0x0008 | 0x0200);
}

#[cfg(not(target_os = "windows"))]
fn configure_detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and only mutates kernel state
    // for the new process. We're between fork and exec so this is the
    // documented safe place to call it.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

struct RealWaiter {
    child: Arc<Mutex<Option<Child>>>,
}

impl AppWaiter for RealWaiter {
    fn wait(self: Box<Self>) -> Result<bool, Error> {
        // Take the Child out of the shared slot so `kill` can't race
        // `wait` (Linux/macOS kill-after-wait is harmless but Windows
        // requires a still-valid handle for TerminateProcess).
        let mut child_opt = self
            .child
            .lock()
            .map_err(|_| Error::Internal("app child mutex poisoned".into()))?
            .take();
        let Some(mut child) = child_opt.take() else {
            // Already killed elsewhere; treat as natural exit.
            return Ok(true);
        };
        let status = child
            .wait()
            .map_err(|e| Error::AppSpawnFailed(format!("wait: {e}")))?;
        Ok(status.success())
    }
}

/// Production [`AppKiller`].
///
/// **Caveat (deliberate, see Phase 3 design notes):** once the wait
/// thread starts, [`RealWaiter::wait`] takes the `Child` out of the
/// shared slot so the underlying OS handle is held only by the wait
/// thread. After that point, [`RealKiller::kill`] becomes a no-op and
/// returns `Ok` without actually terminating the process. This is
/// acceptable because the Phase 3 close path uses
/// [`AppPool::detach`] (which never tries to kill); `kill` is retained
/// for tests and for the brief window between spawn and wait-thread
/// startup (see the wait-thread-spawn-failure cleanup in
/// [`AppPool::spawn`]).
struct RealKiller {
    child: Arc<Mutex<Option<Child>>>,
}

impl AppKiller for RealKiller {
    fn kill(&self) -> Result<(), Error> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| Error::Internal("app child mutex poisoned".into()))?;
        if let Some(child) = guard.as_mut() {
            // start_kill / kill returns Err if already exited — benign.
            let _ = child.kill();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

/// Runtime pool for application sub-sessions. Mirrors the lifecycle
/// pattern of [`crate::sub_sessions::SubPtyPool`]:
///
/// - `spawn` inserts a [`AppRuntime`] keyed by [`SubSessionId`], starts a
///   wait thread, and returns the captured PID synchronously. The
///   [`AppPoolSink::status`] callback fires `Running` immediately.
/// - The wait thread self-removes its runtime entry on natural exit
///   (via a `Weak` upgrade) so the pool can never leak entries.
/// - `kill` sets a `killed` guard, forwards to the killer, and removes
///   the runtime from the pool. The wait thread sees `killed == true`
///   and suppresses the status emission so the user-visible event is
///   the explicit close, not a synthetic "exited".
type Inner = Arc<Mutex<BTreeMap<SubSessionId, AppRuntime>>>;

pub struct AppPool {
    spawner: Arc<dyn AppSpawner>,
    inner: Inner,
}

struct AppRuntime {
    pid: u32,
    killer: Arc<dyn AppKiller>,
    killed: Arc<AtomicBool>,
    /// Set by the resolver thread once it has swapped `pid` and
    /// `killer` to the rediscovered owner (e.g. VS Code's long-lived
    /// `Code.exe` after `code.cmd` exits). The wait thread checks
    /// this flag and stays silent — emitting `Exited` would lie to the
    /// frontend about a window that's still open. The liveness thread
    /// is responsible for emitting `Exited` once the rediscovered PID
    /// actually dies.
    re_targeted: Arc<AtomicBool>,
    /// Held so dropping the pool joins the wait thread (best-effort).
    /// Wait threads are short-lived for delegated launchers.
    _wait_thread: Option<JoinHandle<()>>,
    /// Best-effort: the resolver thread polls for the rediscovered
    /// owner and exits once found (or the timeout fires). Held so the
    /// pool drop joins it.
    _resolver_thread: Option<JoinHandle<()>>,
    /// Set by the resolver thread on successful retarget; held so the
    /// pool drop joins the liveness thread (which blocks until the
    /// rediscovered PID dies).
    _liveness_thread: Option<JoinHandle<()>>,
}

impl AppPool {
    #[must_use]
    pub fn new(spawner: Arc<dyn AppSpawner>) -> Self {
        Self {
            spawner,
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Spawn `cmd` for `id` in `cwd`, register the runtime, and start
    /// the wait thread. Returns the captured PID. Emits `Running` via
    /// `sink.status` synchronously before returning.
    ///
    /// If `owner_resolver` is `Some`, the pool also starts a resolver
    /// thread that calls [`OwnerResolver::resolve`] in the background.
    /// On a successful resolution, the runtime's `pid` and `killer`
    /// are swapped to the rediscovered owner under the pool lock, the
    /// `re_targeted` flag is set so the wait thread stays silent when
    /// the launcher exits, a second `subsession://status` event is
    /// emitted with the new PID, and a liveness thread is started to
    /// emit `Exited` once the rediscovered process dies.
    pub fn spawn(
        &self,
        id: SubSessionId,
        cmd: String,
        cwd: PathBuf,
        sink: AppPoolSink,
        owner_resolver: Option<Arc<dyn OwnerResolver>>,
    ) -> Result<u32, Error> {
        let SpawnedApp {
            pid,
            waiter,
            killer,
        } = self.spawner.spawn(&cmd, &cwd)?;

        let killed = Arc::new(AtomicBool::new(false));
        let re_targeted = Arc::new(AtomicBool::new(false));
        let weak_inner = Arc::downgrade(&self.inner);

        let wait_id = id;
        let wait_killed = Arc::clone(&killed);
        let wait_sink = sink.clone();
        let wait_weak = weak_inner.clone();
        let wait_thread = match std::thread::Builder::new()
            .name(format!("arborist-app-wait-{pid}"))
            .spawn(move || app_wait_loop(wait_id, waiter, wait_sink, wait_killed, wait_weak))
        {
            Ok(t) => t,
            Err(e) => {
                // Critical: we already spawned a detached external
                // process. Without a wait thread we can't track it, so
                // best-effort kill it now to avoid leaking an untracked
                // GUI app every time thread creation fails.
                let _ = killer.kill();
                return Err(Error::AppSpawnFailed(format!(
                    "spawn app wait thread failed: {e}"
                )));
            }
        };

        // Optional resolver thread: re-target to the long-lived owner
        // process (e.g. `Code.exe`) once the launcher hands off and
        // exits. Best-effort — failure to spawn the thread leaves the
        // sub-session functional with the launcher PID.
        let resolver_thread = owner_resolver.and_then(|resolver| {
            let res_id = id;
            let res_killed = Arc::clone(&killed);
            let res_sink = sink.clone();
            let res_weak = weak_inner.clone();
            std::thread::Builder::new()
                .name(format!("arborist-app-resolve-{pid}"))
                .spawn(move || resolver_loop(res_id, resolver, res_sink, res_weak, res_killed))
                .map_err(|e| {
                    tracing::warn!(sub_session_id = %id, error = %e, "spawn app resolver thread failed");
                })
                .ok()
        });

        {
            let mut g = self
                .inner
                .lock()
                .map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            g.insert(
                id,
                AppRuntime {
                    pid,
                    killer,
                    killed,
                    re_targeted,
                    _wait_thread: Some(wait_thread),
                    _resolver_thread: resolver_thread,
                    _liveness_thread: None,
                },
            );
        }

        (sink.status)(&id, SubSessionStatus::Running, Some(pid), None);
        Ok(pid)
    }

    /// Whether `id` is in the pool right now. Inherently racy — for
    /// tests + diagnostics only.
    #[must_use]
    pub fn contains(&self, id: &SubSessionId) -> bool {
        self.inner
            .lock()
            .map(|g| g.contains_key(id))
            .unwrap_or(false)
    }

    /// Live PID for `id`, if known. `None` if the runtime has been
    /// removed (kill or natural exit).
    #[must_use]
    pub fn pid(&self, id: &SubSessionId) -> Option<u32> {
        self.inner.lock().ok()?.get(id).map(|r| r.pid)
    }

    /// Explicit close. Sets the `killed` guard so the wait thread will
    /// suppress its status emission, calls `killer.kill()`, and removes
    /// the runtime from the pool. Idempotent (`Ok` if the id is unknown).
    pub fn kill(&self, id: &SubSessionId) -> Result<(), Error> {
        let removed = {
            let mut g = self
                .inner
                .lock()
                .map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            g.remove(id)
        };
        if let Some(rt) = removed {
            rt.killed.store(true, Ordering::SeqCst);
            // Best-effort: the killer's child slot may already be empty
            // because the wait thread took it. That's fine — `kill`
            // returns Ok in that case.
            rt.killer.kill()?;
        }
        Ok(())
    }

    /// Detach `id` from the pool without terminating the underlying
    /// process. Used when an application sub-tab is closed: the user
    /// expects the tab to disappear but their editor / file browser to
    /// keep running. Sets the `killed` guard so the wait thread (if it
    /// completes after we've stopped caring) suppresses its emission.
    /// Idempotent (`Ok(())` if the id is unknown).
    pub fn detach(&self, id: &SubSessionId) {
        let removed = {
            let Ok(mut g) = self.inner.lock() else {
                return;
            };
            g.remove(id)
        };
        if let Some(rt) = removed {
            rt.killed.store(true, Ordering::SeqCst);
        }
    }
}

fn app_wait_loop(
    id: SubSessionId,
    waiter: Box<dyn AppWaiter>,
    sink: AppPoolSink,
    killed: Arc<AtomicBool>,
    pool_weak: std::sync::Weak<Mutex<BTreeMap<SubSessionId, AppRuntime>>>,
) {
    let result = waiter.wait();
    // Three possible outcomes once the launcher exits:
    //
    //   * `Retargeted` — resolver thread already swapped this entry to
    //     a long-lived owner PID. Stay silent; the liveness thread is
    //     responsible for eventually emitting `Exited`.
    //   * `Removed` — we won the race to claim emission rights for
    //     this entry. Remove ourselves from the pool and emit.
    //   * `AlreadyGone` — `kill` / `detach` removed us first; the
    //     user-facing event was the explicit close.
    enum Action {
        Retargeted,
        Removed,
        AlreadyGone,
    }
    let action = if let Some(strong) = pool_weak.upgrade() {
        if let Ok(mut g) = strong.lock() {
            match g.get(&id) {
                None => Action::AlreadyGone,
                Some(rt) if rt.re_targeted.load(Ordering::SeqCst) => Action::Retargeted,
                Some(_) => {
                    g.remove(&id);
                    Action::Removed
                }
            }
        } else {
            Action::AlreadyGone
        }
    } else {
        Action::AlreadyGone
    };
    if !matches!(action, Action::Removed) || killed.load(Ordering::SeqCst) {
        return;
    }
    let status = match result {
        Ok(true) => SubSessionStatus::Exited,
        Ok(false) | Err(_) => SubSessionStatus::Error,
    };
    (sink.status)(&id, status, None, None);
    (sink.exited)(&id, None);
}

/// Resolver thread body: blocks on [`OwnerResolver::resolve`] then,
/// under the pool lock, swaps the runtime's `pid` and `killer` to the
/// rediscovered owner. Emits a `Running` status event with the new
/// PID and starts the liveness thread that will emit `Exited` once
/// the rediscovered process dies.
fn resolver_loop(
    id: SubSessionId,
    resolver: Arc<dyn OwnerResolver>,
    sink: AppPoolSink,
    pool_weak: std::sync::Weak<Mutex<BTreeMap<SubSessionId, AppRuntime>>>,
    killed: Arc<AtomicBool>,
) {
    if killed.load(Ordering::SeqCst) {
        return;
    }
    let Some(bundle) = resolver.resolve() else {
        return;
    };
    if killed.load(Ordering::SeqCst) {
        return;
    }
    let RetargetedOwner {
        pid: new_pid,
        killer: new_killer,
        liveness,
    } = bundle;

    // Spawn the liveness thread first so `wait_for_death` is consumed
    // even if the swap below loses the race. `liveness_loop` does
    // nothing harmful when the runtime is already gone.
    let live_sink = sink.clone();
    let live_weak = pool_weak.clone();
    let live_killed = Arc::clone(&killed);
    let live_thread = std::thread::Builder::new()
        .name(format!("arborist-app-liveness-{new_pid}"))
        .spawn(move || liveness_loop(id, new_pid, liveness, live_sink, live_weak, live_killed))
        .map_err(|e| {
            tracing::warn!(sub_session_id = %id, error = %e, "spawn app liveness thread failed");
        })
        .ok();

    let did_retarget = if let Some(strong) = pool_weak.upgrade() {
        if let Ok(mut g) = strong.lock() {
            if let Some(rt) = g.get_mut(&id) {
                rt.re_targeted.store(true, Ordering::SeqCst);
                rt.pid = new_pid;
                rt.killer = Arc::clone(&new_killer);
                rt._liveness_thread = live_thread;
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if did_retarget {
        (sink.status)(&id, SubSessionStatus::Running, Some(new_pid), None);
    }
    // If !did_retarget the runtime was removed (close/relaunch) between
    // resolve() and the lock; the liveness thread will discover the
    // missing entry on its claim-and-emit attempt and exit silently.
}

/// Liveness thread body: blocks on the [`LivenessProbe`] until the
/// rediscovered PID dies, then claim-and-emits `Exited` provided the
/// runtime still belongs to this PID (i.e. the user didn't already
/// `detach` or `relaunch`).
fn liveness_loop(
    id: SubSessionId,
    new_pid: u32,
    liveness: Box<dyn LivenessProbe>,
    sink: AppPoolSink,
    pool_weak: std::sync::Weak<Mutex<BTreeMap<SubSessionId, AppRuntime>>>,
    killed: Arc<AtomicBool>,
) {
    liveness.wait_for_death();

    // Claim emission rights ONLY if the runtime is still ours
    // (matching pid). Other outcomes — entry gone (kill/detach), or
    // entry replaced via relaunch — mean someone else already owns the
    // user-facing event.
    let removed_by_us = if let Some(strong) = pool_weak.upgrade() {
        if let Ok(mut g) = strong.lock() {
            match g.get(&id) {
                Some(rt) if rt.pid == new_pid => g.remove(&id).is_some(),
                _ => false,
            }
        } else {
            false
        }
    } else {
        false
    };

    if !removed_by_us || killed.load(Ordering::SeqCst) {
        return;
    }
    (sink.status)(&id, SubSessionStatus::Exited, None, None);
    (sink.exited)(&id, None);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    /// Minimal fake spawner: returns sequentially-numbered PIDs and a
    /// waiter/killer pair backed by a per-spawn `FakeApp` handle the
    /// test can drive.
    pub struct FakeAppSpawner {
        children: StdMutex<Vec<Arc<FakeApp>>>,
        next_pid: std::sync::atomic::AtomicU32,
    }

    impl FakeAppSpawner {
        pub fn new() -> Self {
            Self {
                children: StdMutex::new(Vec::new()),
                next_pid: std::sync::atomic::AtomicU32::new(2000),
            }
        }
        pub fn child(&self, idx: usize) -> Arc<FakeApp> {
            self.children.lock().unwrap()[idx].clone()
        }
    }

    pub struct FakeApp {
        exit_signal: Arc<(StdMutex<Option<bool>>, std::sync::Condvar)>,
        killed: AtomicBool,
    }

    impl FakeApp {
        pub fn signal_exit(&self, success: bool) {
            let (lock, cvar) = &*self.exit_signal;
            *lock.lock().unwrap() = Some(success);
            cvar.notify_all();
        }
        pub fn was_killed(&self) -> bool {
            self.killed.load(Ordering::SeqCst)
        }
    }

    impl AppSpawner for FakeAppSpawner {
        fn spawn(&self, _cmd: &str, _cwd: &Path) -> Result<SpawnedApp, Error> {
            let pid = self
                .next_pid
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let app = Arc::new(FakeApp {
                exit_signal: Arc::new((StdMutex::new(None), std::sync::Condvar::new())),
                killed: AtomicBool::new(false),
            });
            self.children.lock().unwrap().push(Arc::clone(&app));
            let waiter_app = Arc::clone(&app);
            let killer_app = Arc::clone(&app);
            Ok(SpawnedApp {
                pid,
                waiter: Box::new(FakeWaiter { app: waiter_app }),
                killer: Arc::new(FakeKiller { app: killer_app }),
            })
        }
    }

    struct FakeWaiter {
        app: Arc<FakeApp>,
    }
    impl AppWaiter for FakeWaiter {
        fn wait(self: Box<Self>) -> Result<bool, Error> {
            let (lock, cvar) = &*self.app.exit_signal;
            let mut g = lock.lock().unwrap();
            while g.is_none() {
                g = cvar.wait(g).unwrap();
            }
            Ok(g.take().unwrap_or(false))
        }
    }

    struct FakeKiller {
        app: Arc<FakeApp>,
    }
    impl AppKiller for FakeKiller {
        fn kill(&self) -> Result<(), Error> {
            self.app.killed.store(true, Ordering::SeqCst);
            self.app.signal_exit(false);
            Ok(())
        }
    }

    type StatusObs = Arc<StdMutex<Vec<(SubSessionStatus, Option<u32>)>>>;
    type ExitObs = Arc<StdMutex<Vec<Option<i32>>>>;

    fn collect_sink() -> (AppPoolSink, StatusObs, ExitObs) {
        let status_obs: StatusObs = Arc::new(StdMutex::new(Vec::new()));
        let exit_obs: ExitObs = Arc::new(StdMutex::new(Vec::new()));
        let s_for_status = Arc::clone(&status_obs);
        let s_for_exit = Arc::clone(&exit_obs);
        let sink = SubPtySink::new(
            Arc::new(|_, _| {}),
            Arc::new(move |_, status, pid, _| {
                s_for_status.lock().unwrap().push((status, pid));
            }),
            Arc::new(move |_, code| s_for_exit.lock().unwrap().push(code)),
            Arc::new(|_| {}),
        );
        (sink, status_obs, exit_obs)
    }

    fn wait_until<F: Fn() -> bool>(f: F, timeout: Duration, msg: &str) {
        let deadline = Instant::now() + timeout;
        while !f() {
            if Instant::now() > deadline {
                panic!("timeout: {msg}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn pool_spawn_emits_running_then_exit_on_natural_termination() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();

        let pid = pool
            .spawn(id, "code .".to_owned(), PathBuf::from("."), sink, None)
            .expect("spawn");
        assert!(pid >= 2000);
        assert!(pool.contains(&id));

        spawner.child(0).signal_exit(true);

        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "pool should self-remove after natural exit",
        );

        let statuses = status_obs.lock().unwrap().clone();
        assert!(matches!(
            statuses.first(),
            Some((SubSessionStatus::Running, Some(_)))
        ));
        assert!(statuses
            .iter()
            .any(|(s, _)| matches!(s, SubSessionStatus::Exited)));
        assert!(!exit_obs.lock().unwrap().is_empty());
    }

    #[test]
    fn pool_spawn_failed_status_on_nonzero_exit() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, _) = collect_sink();
        let id = SubSessionId::default();
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None)
            .expect("spawn");
        spawner.child(0).signal_exit(false);
        wait_until(
            || {
                status_obs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(s, _)| matches!(s, SubSessionStatus::Error))
            },
            Duration::from_secs(2),
            "should observe Error",
        );
    }

    #[test]
    fn pool_kill_suppresses_status_event() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None)
            .expect("spawn");
        // Prime: only Running observed so far.
        wait_until(
            || !status_obs.lock().unwrap().is_empty(),
            Duration::from_secs(2),
            "Running",
        );
        pool.kill(&id).expect("kill");
        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "pool should drop entry on kill",
        );
        // Give the wait thread a beat to finish (it should have aborted
        // status emission via the killed guard).
        std::thread::sleep(Duration::from_millis(50));
        let statuses = status_obs.lock().unwrap().clone();
        assert_eq!(
            statuses
                .iter()
                .filter(|(s, _)| !matches!(s, SubSessionStatus::Running))
                .count(),
            0,
            "no post-Running status should be emitted after kill, got {statuses:?}"
        );
        assert!(exit_obs.lock().unwrap().is_empty());
    }

    #[test]
    fn pool_pid_returns_live_pid_then_none_after_exit() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, _, _) = collect_sink();
        let id = SubSessionId::default();
        let pid = pool
            .spawn(id, "x".to_owned(), PathBuf::from("."), sink, None)
            .unwrap();
        assert_eq!(pool.pid(&id), Some(pid));
        spawner.child(0).signal_exit(true);
        wait_until(
            || pool.pid(&id).is_none(),
            Duration::from_secs(2),
            "pid should clear after exit",
        );
    }

    #[test]
    fn pool_detach_removes_entry_without_killing_and_suppresses_exit_event() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None)
            .expect("spawn");
        wait_until(
            || !status_obs.lock().unwrap().is_empty(),
            Duration::from_secs(2),
            "Running",
        );
        let killed_before = spawner.child(0).was_killed();
        pool.detach(&id);
        assert!(!pool.contains(&id), "detach removes from pool");
        assert_eq!(
            spawner.child(0).was_killed(),
            killed_before,
            "detach must NOT kill the underlying process"
        );
        // Now let the (still-running, fake) child exit naturally; the
        // killed-guard should suppress any post-detach status emission.
        spawner.child(0).signal_exit(true);
        std::thread::sleep(Duration::from_millis(80));
        let statuses = status_obs.lock().unwrap().clone();
        assert_eq!(
            statuses
                .iter()
                .filter(|(s, _)| !matches!(s, SubSessionStatus::Running))
                .count(),
            0,
            "no post-Running status after detach, got {statuses:?}"
        );
        assert!(exit_obs.lock().unwrap().is_empty());
    }

    // -----------------------------------------------------------------
    // Re-target (OwnerResolver / LivenessProbe) tests
    // -----------------------------------------------------------------

    /// Test resolver: blocks on a barrier, then returns the queued
    /// [`RetargetedOwner`]. Tests can inspect whether `resolve` was
    /// called.
    struct FakeOwnerResolver {
        barrier: Arc<(StdMutex<Option<RetargetedOwner>>, std::sync::Condvar)>,
        called: AtomicBool,
    }

    impl FakeOwnerResolver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                barrier: Arc::new((StdMutex::new(None), std::sync::Condvar::new())),
                called: AtomicBool::new(false),
            })
        }
        /// Queue a successful retarget. Wakes the resolver thread.
        fn signal_retarget(&self, owner: RetargetedOwner) {
            let (lock, cvar) = &*self.barrier;
            *lock.lock().unwrap() = Some(owner);
            cvar.notify_all();
        }
        fn was_called(&self) -> bool {
            self.called.load(Ordering::SeqCst)
        }
    }

    impl OwnerResolver for FakeOwnerResolver {
        fn resolve(&self) -> Option<RetargetedOwner> {
            self.called.store(true, Ordering::SeqCst);
            let (lock, cvar) = &*self.barrier;
            let mut g = lock.lock().unwrap();
            // Block until either an owner is queued or 5s elapses (so a
            // misbehaving test fails loudly rather than hanging the
            // whole suite).
            let deadline = Instant::now() + Duration::from_secs(5);
            while g.is_none() {
                let remaining = deadline.checked_duration_since(Instant::now())?;
                let (next, timeout) = cvar.wait_timeout(g, remaining).unwrap();
                if timeout.timed_out() && next.is_none() {
                    return None;
                }
                g = next;
            }
            g.take()
        }
    }

    /// Test liveness probe: blocks on a Condvar until `signal_dead` is
    /// called. Mirrors `FakeWaiter`.
    struct FakeLivenessProbe {
        signal: Arc<(StdMutex<bool>, std::sync::Condvar)>,
    }

    impl FakeLivenessProbe {
        fn new_pair() -> (Box<Self>, Arc<(StdMutex<bool>, std::sync::Condvar)>) {
            let signal = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
            (
                Box::new(FakeLivenessProbe {
                    signal: Arc::clone(&signal),
                }),
                signal,
            )
        }
    }

    impl LivenessProbe for FakeLivenessProbe {
        fn wait_for_death(self: Box<Self>) {
            let (lock, cvar) = &*self.signal;
            let mut g = lock.lock().unwrap();
            while !*g {
                g = cvar.wait(g).unwrap();
            }
        }
    }

    fn signal_liveness_dead(signal: &Arc<(StdMutex<bool>, std::sync::Condvar)>) {
        let (lock, cvar) = &**signal;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    /// Standalone fake killer keyed by an Arc<AtomicBool>, used to
    /// observe whether `pool.kill` after retarget targets the
    /// rediscovered killer (not the original).
    struct FlagKiller {
        killed: Arc<AtomicBool>,
    }
    impl AppKiller for FlagKiller {
        fn kill(&self) -> Result<(), Error> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn pool_retarget_swaps_pid_and_killer_and_emits_running_with_new_pid() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();

        let resolver = FakeOwnerResolver::new();
        let initial_pid = pool
            .spawn(
                id,
                "code .".to_owned(),
                PathBuf::from("."),
                sink,
                Some(resolver.clone() as Arc<dyn OwnerResolver>),
            )
            .expect("spawn");

        // Resolver returns a NEW pid + a fresh killer + a probe we
        // hold the death-signal for.
        let new_pid = 99_001;
        let new_killed_flag = Arc::new(AtomicBool::new(false));
        let new_killer: Arc<dyn AppKiller> = Arc::new(FlagKiller {
            killed: Arc::clone(&new_killed_flag),
        });
        let (probe, _liveness_signal) = FakeLivenessProbe::new_pair();
        resolver.signal_retarget(RetargetedOwner {
            pid: new_pid,
            killer: Arc::clone(&new_killer),
            liveness: probe,
        });

        // Pool's pid for the entry should flip to new_pid.
        wait_until(
            || pool.pid(&id) == Some(new_pid),
            Duration::from_secs(2),
            "pool.pid should flip to rediscovered owner",
        );

        // Status events: Running(initial_pid) then Running(new_pid).
        wait_until(
            || {
                status_obs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(s, p)| matches!(s, SubSessionStatus::Running) && *p == Some(new_pid))
            },
            Duration::from_secs(2),
            "Running event with new pid",
        );

        let statuses = status_obs.lock().unwrap().clone();
        assert!(matches!(
            statuses.first(),
            Some((SubSessionStatus::Running, Some(p))) if *p == initial_pid
        ));

        // Now the launcher exits. With re_targeted=true, the wait
        // thread MUST stay silent — no Exited event.
        spawner.child(0).signal_exit(true);
        std::thread::sleep(Duration::from_millis(120));
        let post = status_obs.lock().unwrap().clone();
        assert!(
            !post
                .iter()
                .any(|(s, _)| matches!(s, SubSessionStatus::Exited)),
            "launcher exit must be suppressed after retarget; got {post:?}"
        );
        assert!(
            exit_obs.lock().unwrap().is_empty(),
            "no exited event after retarget+launcher-exit"
        );
        // Entry must still be in the pool (liveness thread owns the
        // exit emission now).
        assert!(
            pool.contains(&id),
            "entry must remain in pool after launcher exits if retargeted"
        );

        // pool.kill MUST hit the NEW killer, not the launcher's.
        pool.kill(&id).expect("kill");
        assert!(
            new_killed_flag.load(Ordering::SeqCst),
            "post-retarget pool.kill must invoke the rediscovered killer"
        );
        // The original launcher's killer must NOT receive the kill —
        // FakeAppSpawner.child(0) was already exited by us above; this
        // is just a no-double-kill assertion.
    }

    #[test]
    fn pool_retarget_then_liveness_death_emits_exited_with_no_pid() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();

        let resolver = FakeOwnerResolver::new();
        pool.spawn(
            id,
            "code .".to_owned(),
            PathBuf::from("."),
            sink,
            Some(resolver.clone() as Arc<dyn OwnerResolver>),
        )
        .expect("spawn");

        let new_pid = 99_002;
        let killed_flag = Arc::new(AtomicBool::new(false));
        let new_killer: Arc<dyn AppKiller> = Arc::new(FlagKiller {
            killed: Arc::clone(&killed_flag),
        });
        let (probe, liveness_signal) = FakeLivenessProbe::new_pair();
        resolver.signal_retarget(RetargetedOwner {
            pid: new_pid,
            killer: new_killer,
            liveness: probe,
        });
        wait_until(
            || pool.pid(&id) == Some(new_pid),
            Duration::from_secs(2),
            "pool.pid should flip",
        );

        // Launcher exits — suppressed.
        spawner.child(0).signal_exit(true);
        std::thread::sleep(Duration::from_millis(50));

        // Now the rediscovered owner dies (user closed VS Code).
        signal_liveness_dead(&liveness_signal);

        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "pool should drop entry after liveness death",
        );
        wait_until(
            || {
                status_obs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(s, p)| matches!(s, SubSessionStatus::Exited) && p.is_none())
            },
            Duration::from_secs(2),
            "Exited(None) emitted after rediscovered owner dies",
        );
        assert!(
            !exit_obs.lock().unwrap().is_empty(),
            "exited event after rediscovered owner dies"
        );
        // We did NOT call pool.kill — killer must not have been
        // invoked.
        assert!(
            !killed_flag.load(Ordering::SeqCst),
            "natural death must not invoke killer"
        );
    }

    #[test]
    fn pool_resolver_returning_none_falls_through_to_normal_lifecycle() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();

        // A resolver that always returns None (e.g. the resolver gave
        // up after timeout). We use FakeOwnerResolver and call
        // `signal_no_retarget` to release the wait without queuing an
        // owner.
        struct NeverFinds;
        impl OwnerResolver for NeverFinds {
            fn resolve(&self) -> Option<RetargetedOwner> {
                None
            }
        }
        pool.spawn(
            id,
            "x".to_owned(),
            PathBuf::from("."),
            sink,
            Some(Arc::new(NeverFinds) as Arc<dyn OwnerResolver>),
        )
        .expect("spawn");

        // Launcher exits naturally — Exited must fire (re_targeted is
        // still false because resolver returned None).
        spawner.child(0).signal_exit(true);
        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "pool should drop entry on natural exit when resolver returned None",
        );
        let statuses = status_obs.lock().unwrap().clone();
        assert!(statuses
            .iter()
            .any(|(s, _)| matches!(s, SubSessionStatus::Exited)));
        assert!(!exit_obs.lock().unwrap().is_empty());
    }

    #[test]
    fn pool_detach_after_retarget_does_not_kill_rediscovered_owner() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, _status_obs, _exit_obs) = collect_sink();
        let id = SubSessionId::default();

        let resolver = FakeOwnerResolver::new();
        pool.spawn(
            id,
            "code .".to_owned(),
            PathBuf::from("."),
            sink,
            Some(resolver.clone() as Arc<dyn OwnerResolver>),
        )
        .expect("spawn");
        let new_pid = 99_003;
        let killed_flag = Arc::new(AtomicBool::new(false));
        let new_killer: Arc<dyn AppKiller> = Arc::new(FlagKiller {
            killed: Arc::clone(&killed_flag),
        });
        let (probe, _) = FakeLivenessProbe::new_pair();
        resolver.signal_retarget(RetargetedOwner {
            pid: new_pid,
            killer: new_killer,
            liveness: probe,
        });
        wait_until(
            || pool.pid(&id) == Some(new_pid),
            Duration::from_secs(2),
            "retarget happened",
        );

        // Closing the sub-tab uses detach (not kill). The
        // rediscovered editor process must NOT be killed.
        pool.detach(&id);
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            !killed_flag.load(Ordering::SeqCst),
            "detach must not invoke the rediscovered killer"
        );
        assert!(resolver.was_called(), "resolver should have been invoked");
    }

    #[test]
    fn pid_killer_for_dead_pid_returns_ok_no_panic() {
        // PID extremely unlikely to exist. Cross-platform: this is the
        // single-process smoke test for `PidKiller` — no actual victim
        // is killed, but we exercise the OS call paths.
        let killer = PidKiller::new(4_294_967);
        assert!(killer.kill().is_ok());
    }

    #[test]
    fn real_spawner_returns_tool_missing_for_unknown_simple_command() {
        let r = RealAppSpawner.spawn("definitely-not-a-real-binary-xyzzy", Path::new("."));
        assert!(
            matches!(r, Err(Error::ToolMissing(t)) if t == "definitely-not-a-real-binary-xyzzy")
        );
    }

    #[test]
    fn first_token_if_simple_skips_metachars() {
        assert!(first_token_if_simple("echo hi | grep h").is_none());
        assert_eq!(first_token_if_simple("code .").as_deref(), Some("code"));
    }

    /// Real-process smoke test: spawn a trivial cross-platform "exits
    /// quickly" command and observe the lifecycle. Cheap, deterministic.
    #[test]
    fn real_spawner_smoke_test() {
        let spawner = Arc::new(RealAppSpawner);
        let pool = AppPool::new(spawner);
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();
        #[cfg(target_os = "windows")]
        let cmd = "cmd /c exit 0".to_owned();
        #[cfg(not(target_os = "windows"))]
        let cmd = "true".to_owned();

        let cwd = std::env::temp_dir();
        pool.spawn(id, cmd, cwd, sink, None).expect("real spawn");

        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(5),
            "real child should exit",
        );
        let statuses = status_obs.lock().unwrap().clone();
        assert!(matches!(
            statuses.first(),
            Some((SubSessionStatus::Running, Some(_)))
        ));
        assert!(!exit_obs.lock().unwrap().is_empty());
    }

    #[test]
    fn real_spawner_rejects_empty_command() {
        let spawner = RealAppSpawner;
        let result = spawner.spawn("   ", Path::new("."));
        match result {
            Err(Error::AppSpawnFailed(_)) => (),
            Err(other) => panic!("expected AppSpawnFailed, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }
}
