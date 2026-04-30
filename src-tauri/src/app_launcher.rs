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
    /// Held so dropping the pool joins the wait thread (best-effort).
    /// Wait threads are short-lived for delegated launchers.
    _wait_thread: Option<JoinHandle<()>>,
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
    pub fn spawn(
        &self,
        id: SubSessionId,
        cmd: String,
        cwd: PathBuf,
        sink: AppPoolSink,
    ) -> Result<u32, Error> {
        let SpawnedApp {
            pid,
            waiter,
            killer,
        } = self.spawner.spawn(&cmd, &cwd)?;

        let killed = Arc::new(AtomicBool::new(false));
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
                    _wait_thread: Some(wait_thread),
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
    // Atomically claim ownership of emission by removing ourselves from
    // the pool. If `kill` or `detach` already removed the entry, we lose
    // the race and stay silent — the user already considers this
    // sub-session gone.
    let removed_by_us = if let Some(strong) = pool_weak.upgrade() {
        if let Ok(mut g) = strong.lock() {
            g.remove(&id).is_some()
        } else {
            false
        }
    } else {
        false
    };
    if !removed_by_us || killed.load(Ordering::SeqCst) {
        return;
    }
    let status = match result {
        Ok(true) => SubSessionStatus::Exited,
        Ok(false) | Err(_) => SubSessionStatus::Error,
    };
    (sink.status)(&id, status, None, None);
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
            .spawn(id, "code .".to_owned(), PathBuf::from("."), sink)
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
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink)
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
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink)
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
            .spawn(id, "x".to_owned(), PathBuf::from("."), sink)
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
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink)
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
        pool.spawn(id, cmd, cwd, sink).expect("real spawn");

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
