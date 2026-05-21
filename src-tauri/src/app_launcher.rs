//! Application sub-session launcher (Phase 3 of `dev/ai/CONTEXT_MENU_PLAN.md`).
//!
//! Application-kind sub-tabs are external GUI processes (VS Code, Finder, Explorer, etc.) launched into the parent session's worktree. Unlike
//! terminal sub-tabs they do **not** allocate a PTY: stdio is dropped and the only signals we surface back to the UI are start (synchronous from
//! `spawn`) and exit (`subsession://exited` / status `Exited`).
//!
//! ## Honest limitations
//!
//! Many real-world app launchers are *delegators*: `code .`, `xdg-open .`, `open .`, and `explorer .` typically hand off to an existing instance and
//! exit immediately. The PID we capture is the launcher's, not the eventual GUI window's. This module is honest about that: the wait thread will
//! report `Exited` very quickly for those commands and a later `focus_pid` may be a no-op. The frontend should treat `subsession://exited` as
//! informational and not assume the user closed a window.
//!
//! ## Survival of the parent
//!
//! Spawned children inherit no controlling terminal and (on Unix) get a new process session via `setsid` so closing Arborist doesn't take the
//! external app down with it. We still keep the [`std::process::Child`] handle and use blocking `wait()` in a thread — detachment doesn't preclude
//! waiting on a child we own.
//!
//! ## Public surface
//!
//! - [`AppSpawner`] — trait seam over `std::process::Command`. Real impl is
//!   [`RealAppSpawner`]; tests use [`tests::FakeAppSpawner`].
//! - [`AppPool`] — runtime pool for application sub-sessions.
//! - [`AppPoolSink`] — alias for [`crate::sub_sessions::SubPtySink`] reused so
//!   a single sink type drives both pool flavours.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Upper bound on how long [`app_wait_loop`] will wait for the owner resolver to publish its final result after the launcher process exits, before
/// falling through to its existing pool-state check.
///
/// Rationale: VS Code's resolver polls 500 ms × 16 = 8 s; this gives it the full window plus 1 s of slack so a slow paint doesn't race the wait
/// thread into emitting `Exited` for a sub-tab whose `Code.exe` is in the middle of warming up.
/// See `plugins/custom_process/vscode/owner.rs::POLL_DEADLINE`.
const RESOLVER_GRACE_DEADLINE: Duration = Duration::from_secs(9);

/// Poll tick inside the grace window. Tight enough that `kill` / `detach` mid-grace returns control to the wait thread quickly.
const RESOLVER_GRACE_POLL: Duration = Duration::from_millis(50);

use crate::sub_sessions::SubPtySink;
use crate::types::{Error, SubSessionId, SubSessionStatus};

/// Re-exported alias so call sites can name a single sink type.
pub type AppPoolSink = SubPtySink;

// --------------------------------------------------------------------------- Spawner trait
// ---------------------------------------------------------------------------

/// Anything that can launch a detached child process and return a handle plus a waiter / killer pair. The trait exists so unit tests can swap in a
/// fake without touching the real OS.
pub trait AppSpawner: Send + Sync + 'static {
    /// Spawn `cmd` (a shell command string — wrapped in `cmd /c …` on Windows / `sh -c …` elsewhere) inside `cwd`. Returns the captured PID plus a
    /// [`SpawnedApp`] handle.
    fn spawn(&self, cmd: &str, cwd: &Path) -> Result<SpawnedApp, Error>;
}

/// Per-spawn handle: PID + waiter + killer. The waiter is consumed by the wait thread (blocking `wait()`); the killer is retained in the pool so
/// explicit closes can terminate the launcher.
pub struct SpawnedApp {
    pub pid: u32,
    pub waiter: Box<dyn AppWaiter>,
    pub killer: Arc<dyn AppKiller>,
}

/// Blocking-wait abstraction for a spawned app. Implementors should not share a single waiter across threads — the trait takes `self: Box<Self>` so
/// callers cannot accidentally call `wait` twice.
pub trait AppWaiter: Send + 'static {
    /// Block until the spawned process exits and return whether it exited successfully.
    fn wait(self: Box<Self>) -> Result<bool, Error>;
}

/// Kill abstraction for a spawned app. Cloning is cheap (typically an `Arc` over a `Mutex<Option<Child>>`).
pub trait AppKiller: Send + Sync + 'static {
    /// Best-effort termination. Returns `Ok` even if the process has already exited (that's a benign race).
    fn kill(&self) -> Result<(), Error>;
}

// --------------------------------------------------------------------------- PID-by-PID killer (for re-targeted owners)
// ---------------------------------------------------------------------------

/// [`AppKiller`] keyed on a raw OS PID rather than a [`Child`] handle. Used in two situations:
///
/// 1. At spawn time, for the launcher process itself (see `RealAppSpawner::spawn`). Earlier revisions used a `RealKiller` that shared the `Child`
///    handle with the wait thread via `Arc<Mutex<Option<Child>>>`; once the waiter took the slot, kill became a documented no-op. Switching to
///    `PidKiller` lets `AppPool::kill` (and the new `AppPool::kill_async`) always work — verification happens via the OS, not via the in-process
///    handle.
///
/// 2. After [`OwnerResolver`] re-targets an [`AppRuntime`] to a long-lived editor process the launcher handed off to (e.g. VS Code's `Code.exe`).
///    The launcher's killer is moot at that point; the resolver hands the pool a fresh `PidKiller` for the rediscovered owner.
///
/// On Windows this issues `TerminateProcess` (PROCESS_TERMINATE access mask). On Unix it sends `SIGKILL` — uninterruptible to match the
/// user-facing "Force kill" semantics. Polite close (WM_CLOSE / SIGTERM-equivalent) is a separate primitive on [`AppPool`] and never reaches here.
///
/// `kill` is best-effort and returns `Ok` even if the process has already exited; the OS-level "no such process" error is benign.
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
    // Best-effort: a NULL handle means the PID is already gone (benign race) or we lack permission (rare for processes we spawned). SAFETY: passing a
    // literal access mask + PID; OpenProcess is documented to handle invalid PIDs by returning NULL.
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
    // SIGKILL — termination is uninterruptible. Used by both [`PidKiller`] (post-resolver-retarget) and the launcher-side killer attached at
    // `RealAppSpawner::spawn`. Earlier revisions sent SIGTERM here, which made `kill_async` racy on apps that ignored SIGTERM (Electron apps with
    // `before-quit` listeners do, until the user dismisses the prompt). SIGKILL matches the documented "ForceKill" intent: the user has explicitly
    // chosen the destructive path. Polite-close is a separate primitive (`request_window_close_then_wait_async`) and never reaches here.
    //
    // SAFETY: kill(2) is async-signal-safe; passing a possibly-stale PID is documented to return ESRCH which we treat as benign.
    unsafe {
        let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    Ok(())
}

// --------------------------------------------------------------------------- Owner resolution / liveness (re-target after launcher exit)
// ---------------------------------------------------------------------------

/// Strategy for re-discovering the long-lived owner process after a delegating launcher exits. Set per-spawn — most apps don't need one (pass `None`
/// to [`AppPool::spawn`]); VS Code does (its `code.cmd` launcher hands off to `Code.exe` and exits within ~1 s).
///
/// Implementations encapsulate their own polling / timeout strategy: `resolve()` blocks until a match is found OR a per-resolver deadline expires
/// (returns `None`). Called from a dedicated `arborist-app-resolve-<pid>` thread so polling does not steal time from the wait thread.
pub trait OwnerResolver: Send + Sync + 'static {
    /// Block until the rediscovered owner is identified or the resolver's internal timeout expires.
    fn resolve(&self) -> Option<RetargetedOwner>;
}

/// Bundle returned by [`OwnerResolver::resolve`].
///
/// `liveness` MUST observe the same PID as `pid` — the [`AppPool`] hands the probe to a dedicated thread that emits `Exited` once `wait_for_death`
/// returns, and falsely reporting death would emit a spurious tab-exit event.
///
/// `window_target` is `Some` when the resolver also identified a specific OS window (HWND on Windows) belonging to the rediscovered owner.
/// [`AppPool::focus`] and [`AppPool::request_window_close`] prefer that exact window over a generic "first window for this PID" lookup, which is
/// critical for multi-window apps like VS Code where every workspace window is owned by the same process.
pub struct RetargetedOwner {
    pub pid: u32,
    pub killer: Arc<dyn AppKiller>,
    pub liveness: Box<dyn LivenessProbe>,
    pub window_target: Option<WindowTarget>,
}

/// Pointer to the specific OS window the resolver matched, with a re-find escape hatch so a stale handle (window recreated mid-flight, e.g. VS Code
/// "Reload Window") doesn't permanently break focus/close.
///
/// The `hwnd` field is a platform window handle cast to `usize` (HWND on Windows). It's plain data so it can be cloned freely. `refinder` is `Some`
/// when the resolver knows how to recompute the handle from durable inputs (e.g. a workspace basename); callers fall back to it on the stale-handle
/// path.
pub struct WindowTarget {
    pub pid: u32,
    pub hwnd: usize,
    pub refinder: Option<Arc<dyn WindowFinder>>,
}

impl WindowTarget {
    /// Best-effort re-resolution of `hwnd` after a stale-handle error. Mutates self so subsequent calls reuse the new handle.
    pub fn refresh(&mut self) -> bool {
        let Some(finder) = &self.refinder else {
            return false;
        };
        match finder.find_window() {
            Some(new) if new != 0 => {
                self.hwnd = new;
                true
            }
            _ => false,
        }
    }
}

/// Locates a window for an app whose process may have multiple concurrent windows (e.g. a multi-workspace VS Code instance). Implementations re-run
/// their identifying heuristic on each call — HWNDs become invalid when the user closes the matching window, and in some flows (e.g. Reload Window)
/// the same workspace gets a fresh HWND while the process keeps running.
pub trait WindowFinder: Send + Sync + 'static {
    /// Returns the platform window handle (HWND on Windows) for the owner this finder was constructed for, or `None` if no matching window is
    /// currently visible.
    fn find_window(&self) -> Option<usize>;
}

/// Blocking wait for a re-targeted PID to die. Implementors are free to poll or use OS-level wait (e.g. `WaitForSingleObject` on a `SYNCHRONIZE`
/// handle). Called from a dedicated `arborist-app-liveness-<pid>` thread; `wait_for_death` is consumed (`Box<Self>`) so it cannot be invoked twice.
pub trait LivenessProbe: Send + 'static {
    fn wait_for_death(self: Box<Self>);
}

// --------------------------------------------------------------------------- Async kill / polite-close outcome types
// ---------------------------------------------------------------------------

/// Grace window for verifying that an [`AppPool::kill_async`] actually terminated the target process. Sized at 2 seconds to match
/// [`crate::pty_pool::KILL_GRACE`] — short enough that the UI doesn't appear hung, long enough that a normally-shutting-down GUI app on a busy
/// machine has time to actually exit.
pub const APP_KILL_GRACE: Duration = Duration::from_secs(2);

/// Grace window for verifying that a polite [`AppPool::request_window_close_then_wait_async`] actually closed the matched window. Larger than
/// [`APP_KILL_GRACE`] because a polite close may flash a save-changes prompt the user has to dismiss; we want to give well-behaved apps time to
/// either complete their shutdown OR cancel out and re-show their window before we report `Unconfirmed`.
pub const APP_POLITE_CLOSE_GRACE: Duration = Duration::from_secs(3);

/// How often the async verification loops poll the OS for liveness. Tight enough that a successful close lands quickly; loose enough that the loop
/// isn't a hot wakeup on every machine.
pub const APP_LIVENESS_POLL: Duration = Duration::from_millis(50);

/// Outcome of [`AppPool::kill_async`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKillOutcome {
    /// `killer.kill()` returned `Ok` AND the OS reported the PID as gone within [`APP_KILL_GRACE`].
    Reaped { pid: u32 },
    /// `killer.kill()` returned `Err`, OR the PID was still alive at the end of the grace window. The runtime entry was removed from the pool either
    /// way (the SubSessionId is free for a fresh launch), but the underlying process **may** still be alive. Caller is expected to log loudly and
    /// surface the outcome to the user.
    Unconfirmed { pid: u32 },
}

/// Outcome of [`AppPool::request_window_close_then_wait_async`]. Mirrors [`AppKillOutcome`] but distinguishes the "we don't have a window target" /
/// "platform doesn't support polite close" branches from "we posted WM_CLOSE and verified".
#[derive(Debug, Clone)]
pub enum PoliteCloseOutcome {
    /// `post_close_message` succeeded AND verification (window-handle check on platforms that expose one, PID-liveness otherwise) reported the
    /// target as gone within [`APP_POLITE_CLOSE_GRACE`].
    Confirmed { pid: u32 },
    /// `post_close_message` succeeded but verification timed out — the window may be showing a save-changes prompt, or the app simply refused to
    /// close. The runtime entry is **not** removed by this primitive (callers detach explicitly).
    Posted { pid: u32 },
    /// The platform's [`crate::window_focus::WindowFocuser`] returned [`Error::Unsupported`] for `post_close_message` (non-Windows today). Callers
    /// fall back to "just detach the tab" and surface this outcome to the user so they know nothing was sent to the app.
    Unsupported,
    /// No window target was ever recorded for this runtime (e.g. resolver hasn't run yet, or the runtime has no resolver attached). Callers fall
    /// back to "just detach the tab".
    NoTarget,
    /// The runtime is no longer in the pool (idempotent caller, or a race with another close). The caller should treat as a no-op.
    Gone,
}

/// Cross-platform "is this PID still alive?" probe. Best-effort: a PID we never owned that happens to match a system process will still report
/// `true`; that's an acceptable false positive for the close-verification use case (we'd rather report `Unconfirmed` than synthesise a fake
/// "Reaped" event). On Windows uses `OpenProcess` + `GetExitCodeProcess` (returns `STILL_ACTIVE` while alive). On Unix uses `kill(pid, 0)` which is
/// documented to return success iff the PID exists and we have permission to signal it.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        type HANDLE = *mut c_void;
        #[allow(clippy::upper_case_acronyms)]
        type DWORD = u32;
        #[allow(clippy::upper_case_acronyms)]
        type BOOL = i32;
        const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
        const STILL_ACTIVE: DWORD = 259;

        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
            fn GetExitCodeProcess(handle: HANDLE, code: *mut DWORD) -> BOOL;
            fn CloseHandle(handle: HANDLE) -> BOOL;
        }
        // SAFETY: OpenProcess accepts any PID; NULL handle just means "no such pid / access denied" which we treat as "not alive enough to care".
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return false;
        }
        let mut code: DWORD = 0;
        // SAFETY: `h` is a valid handle from OpenProcess; `code` is a stack-allocated u32 we can write into.
        unsafe {
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == STILL_ACTIVE
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // SAFETY: kill(2) with signal 0 is documented as a permission-and-existence check. ESRCH means "no such process" — that's our `false`. Any
        // other error (EPERM in particular) means the PID exists but we can't signal it; we still want to report `true` because the process is
        // demonstrably alive.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        // SAFETY: errno is a thread-local; reading it after a libc call is the documented idiom.
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ESRCH)
    }
}

// --------------------------------------------------------------------------- Real spawner
// ---------------------------------------------------------------------------

/// Production [`AppSpawner`] backed by [`std::process::Command`] with a platform shell wrapper. Suppresses stdio and detaches from the parent process
/// group/session so the spawned app survives Arborist exiting.
#[derive(Default)]
pub struct RealAppSpawner;

impl AppSpawner for RealAppSpawner {
    fn spawn(&self, cmd: &str, cwd: &Path) -> Result<SpawnedApp, Error> {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return Err(Error::AppSpawnFailed("empty command".to_owned()));
        }

        // Best-effort preflight: if the command's first token looks like a plain executable name (no shell metacharacters), verify it exists on PATH.
        // This converts the most common failure mode — user typed `code` but VS Code's CLI isn't installed — into a typed `ToolMissing`
        // synchronously, instead of a delayed shell-exit-with-status-Error. Skipped for commands that use shell features (pipes, redirects, env
        // expansion, etc.) since the first token isn't necessarily the executable.
        if let Some(tool) = first_token_if_simple(trimmed) {
            if which_in_path(&tool).is_none() {
                return Err(Error::ToolMissing(tool));
            }
        }

        let mut command = build_shell_command(trimmed);
        command.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        configure_detach(&mut command);

        let child = command.spawn().map_err(|e| Error::AppSpawnFailed(format!("spawn `{trimmed}`: {e}")))?;
        let pid = child.id();
        // The wait thread owns the `Child` handle exclusively (via `RealWaiter`). The killer is keyed on the launcher PID instead — `PidKiller`
        // issues the OS-native terminate by PID and works whether or not the wait thread has already consumed the `Child`. Earlier revisions used a
        // `RealKiller` that shared the `Child` with the waiter through `Arc<Mutex<Option<Child>>>`; once the waiter took the slot, `RealKiller::kill`
        // became a documented no-op which made `kill_async` silently incorrect on the launcher-still-alive path. See the BLOCKER discussion on the
        // session-kill hardening plan.
        let waiter = Box::new(RealWaiter {
            child: Mutex::new(Some(child)),
        });
        let killer: Arc<dyn AppKiller> = Arc::new(PidKiller::new(pid));

        Ok(SpawnedApp { pid, waiter, killer })
    }
}

/// Returns the first whitespace-delimited token of `cmd` iff the command contains no shell metacharacters (so the first token is unambiguously the
/// executable name).
fn first_token_if_simple(cmd: &str) -> Option<String> {
    const META: &[char] = &[
        '|', '&', ';', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', ']', '{', '}', '~', '=',
    ];
    if cmd.contains(META) {
        return None;
    }
    let tok = cmd.split_whitespace().next()?;
    Some(tok.to_owned())
}

/// Best-effort "is this on PATH?" lookup. Returns the absolute path if found, `None` otherwise. On Windows, also tries each PATHEXT extension. Treats
/// absolute / relative paths as already-resolved.
fn which_in_path(tool: &str) -> Option<PathBuf> {
    let p = Path::new(tool);
    if p.is_absolute() || tool.contains('/') || tool.contains('\\') {
        return if p.is_file() { Some(p.to_owned()) } else { None };
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
    // CREATE_NEW_PROCESS_GROUP (0x0200) so Ctrl+Break to Arborist doesn't propagate; DETACHED_PROCESS (0x0008) so the child has no console attached.
    // Combined, the child survives Arborist exiting.
    cmd.creation_flags(0x0008 | 0x0200);
}

#[cfg(not(target_os = "windows"))]
fn configure_detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and only mutates kernel state for the new process. We're between fork and exec so this is the documented
    // safe place to call it.
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
    /// Owned slot for the spawned `Child`. Mutex is only here because the `wait` trait method takes `Box<Self>` not `Box<Self> mut`; in practice the
    /// waiter is consumed (Box) by the wait thread, so contention is impossible.
    child: Mutex<Option<Child>>,
}

impl AppWaiter for RealWaiter {
    fn wait(self: Box<Self>) -> Result<bool, Error> {
        let mut child_opt = self.child.lock().map_err(|_| Error::Internal("app child mutex poisoned".into()))?.take();
        let Some(mut child) = child_opt.take() else {
            // Already taken — only path here is a future refactor moving the Child out before we run; treat as natural exit.
            return Ok(true);
        };
        let status = child.wait().map_err(|e| Error::AppSpawnFailed(format!("wait: {e}")))?;
        Ok(status.success())
    }
}

// --------------------------------------------------------------------------- Pool
// ---------------------------------------------------------------------------

/// Runtime pool for application sub-sessions. Mirrors the lifecycle pattern of [`crate::sub_sessions::SubPtyPool`]:
///
/// - `spawn` inserts a [`AppRuntime`] keyed by [`SubSessionId`], starts a wait
///   thread, and returns the captured PID synchronously. The
///   [`AppPoolSink::status`] callback fires `Running` immediately.
/// - The wait thread self-removes its runtime entry on natural exit (via a
///   `Weak` upgrade) so the pool can never leak entries.
/// - `kill` sets a `killed` guard, forwards to the killer, and removes the
///   runtime from the pool. The wait thread sees `killed == true` and
///   suppresses the status emission so the user-visible event is the explicit
///   close, not a synthetic "exited".
type Inner = Arc<Mutex<BTreeMap<SubSessionId, AppRuntime>>>;

pub struct AppPool {
    spawner: Arc<dyn AppSpawner>,
    inner: Inner,
}

struct AppRuntime {
    pid: u32,
    killer: Arc<dyn AppKiller>,
    killed: Arc<AtomicBool>,
    /// Set by the resolver thread once it has swapped `pid` and `killer` to the rediscovered owner (e.g. VS Code's long-lived `Code.exe` after
    /// `code.cmd` exits). The wait thread checks this flag and stays silent — emitting `Exited` would lie to the frontend about a window that's still
    /// open. The liveness thread is responsible for emitting `Exited` once the rediscovered PID actually dies.
    re_targeted: Arc<AtomicBool>,
    /// Set to `true` once the resolver thread has published its final result (success → `re_targeted=true` and pid/killer swapped; failure → resolver
    /// returned `None` or was never started). The wait thread polls this after the launcher exits so a slow paint of the rediscovered owner
    /// (cold-start `Code.exe` can take several seconds) doesn't race the wait thread into emitting a premature `Exited`.
    ///
    /// Set synchronously to `true` at spawn time when no `OwnerResolver` was provided, so non-VSCode app sub-tabs skip the grace window entirely.
    ///
    /// Held on the runtime to keep the `Arc` alive (and ownership symmetric with `re_targeted` and `killed`) even though only the worker threads read
    /// or write it.
    _resolver_done: Arc<AtomicBool>,
    /// Held so dropping the pool joins the wait thread (best-effort). Wait threads are short-lived for delegated launchers.
    _wait_thread: Option<JoinHandle<()>>,
    /// Best-effort: the resolver thread polls for the rediscovered owner and exits once found (or the timeout fires). Held so the pool drop joins it.
    _resolver_thread: Option<JoinHandle<()>>,
    /// Set by the resolver thread on successful retarget; held so the pool drop joins the liveness thread (which blocks until the rediscovered PID
    /// dies).
    _liveness_thread: Option<JoinHandle<()>>,
    /// Optional pointer to the specific OS window the resolver matched. Used by [`AppPool::focus`] and
    /// [`AppPool::request_window_close`] to act on the precise window
    /// the user expects, not "the first visible window owned by this PID" (which picks the wrong workspace for VS Code).
    window_target: Option<WindowTarget>,
}

impl AppPool {
    #[must_use]
    pub fn new(spawner: Arc<dyn AppSpawner>) -> Self {
        Self {
            spawner,
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Spawn `cmd` for `id` in `cwd`, register the runtime, and start the wait thread. Returns the captured PID. Emits `Running` via `sink.status`
    /// synchronously before returning.
    ///
    /// If `owner_resolver` is `Some`, the pool also starts a resolver thread that calls [`OwnerResolver::resolve`] in the background. On a successful
    /// resolution, the runtime's `pid` and `killer` are swapped to the rediscovered owner under the pool lock, the `re_targeted` flag is set so the
    /// wait thread stays silent when the launcher exits, a second `subsession://status` event is emitted with the new PID, and a liveness thread is
    /// started to emit `Exited` once the rediscovered process dies.
    ///
    /// ## Ordering invariant
    ///
    /// The runtime is inserted into the pool **before** the wait / resolver threads are spawned. Both worker loops upgrade the pool weak-ref then
    /// look up `id`; if the entry isn't there they silently treat the runtime as "already gone". Inserting first closes the pre-registration race
    /// where a fast launcher exit (or a synchronous-returning resolver in tests) could otherwise observe an empty pool and orphan the entry that this
    /// method then inserted after the fact.
    pub fn spawn(
        &self,
        id: SubSessionId,
        cmd: String,
        cwd: PathBuf,
        sink: AppPoolSink,
        owner_resolver: Option<Arc<dyn OwnerResolver>>,
    ) -> Result<u32, Error> {
        let SpawnedApp { pid, waiter, killer } = self.spawner.spawn(&cmd, &cwd)?;

        let killed = Arc::new(AtomicBool::new(false));
        let re_targeted = Arc::new(AtomicBool::new(false));
        // `resolver_done` defaults to `true` when no resolver is attached so the wait thread skips its grace poll entirely for non-VSCode apps.
        let resolver_done = Arc::new(AtomicBool::new(owner_resolver.is_none()));

        // Insert the runtime FIRST (with no thread handles yet) so the worker loops always observe the entry. See "Ordering invariant" above.
        {
            let mut g = self.inner.lock().map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            g.insert(
                id,
                AppRuntime {
                    pid,
                    killer: Arc::clone(&killer),
                    killed: Arc::clone(&killed),
                    re_targeted: Arc::clone(&re_targeted),
                    _resolver_done: Arc::clone(&resolver_done),
                    _wait_thread: None,
                    _resolver_thread: None,
                    _liveness_thread: None,
                    window_target: None,
                },
            );
        }

        let weak_inner = Arc::downgrade(&self.inner);

        // Spawn the wait thread. If creation fails we must roll back the inserted runtime AND kill the spawned child so we don't leak an untracked
        // GUI process. With the new `PidKiller`-based killer (see `RealAppSpawner::spawn`), kill works on the launcher PID regardless of whether the
        // wait thread ever started — so this rollback path is now strictly correct (the older `RealKiller`-via-shared-`Child` design required this
        // branch to run before the wait thread ever took the slot).
        let wait_id = id;
        let wait_killed = Arc::clone(&killed);
        let wait_resolver_done = Arc::clone(&resolver_done);
        let wait_sink = sink.clone();
        let wait_weak = weak_inner.clone();
        let wait_thread = match std::thread::Builder::new()
            .name(format!("arborist-app-wait-{pid}"))
            .spawn(move || app_wait_loop(wait_id, waiter, wait_sink, wait_killed, wait_resolver_done, wait_weak))
        {
            Ok(t) => t,
            Err(e) => {
                // Roll back the registration and best-effort kill the child. Without a wait thread we can't track the process, so leaking it would be
                // worse.
                if let Ok(mut g) = self.inner.lock() {
                    g.remove(&id);
                }
                let _ = killer.kill();
                return Err(Error::AppSpawnFailed(format!("spawn app wait thread failed: {e}")));
            }
        };

        // Optional resolver thread: re-target to the long-lived owner process (e.g. `Code.exe`) once the launcher hands off and exits. Best-effort —
        // if the thread fails to spawn we mark `resolver_done=true` so the wait thread doesn't sit in its grace window for nothing.
        let resolver_thread = if let Some(resolver) = owner_resolver {
            let res_id = id;
            let res_killed = Arc::clone(&killed);
            let res_resolver_done = Arc::clone(&resolver_done);
            let res_sink = sink.clone();
            let res_weak = weak_inner.clone();
            match std::thread::Builder::new()
                .name(format!("arborist-app-resolve-{pid}"))
                .spawn(move || resolver_loop(res_id, resolver, res_sink, res_weak, res_killed, res_resolver_done))
            {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!(sub_session_id = %id, error = %e, "spawn app resolver thread failed");
                    resolver_done.store(true, Ordering::SeqCst);
                    None
                }
            }
        } else {
            None
        };

        // Patch the thread join handles into the already-registered runtime so pool-drop can join them.
        {
            if let Ok(mut g) = self.inner.lock() {
                if let Some(rt) = g.get_mut(&id) {
                    rt._wait_thread = Some(wait_thread);
                    rt._resolver_thread = resolver_thread;
                }
            }
        }

        (sink.status)(&id, SubSessionStatus::Running, Some(pid), None);
        Ok(pid)
    }

    /// Whether `id` is in the pool right now. Inherently racy — for tests + diagnostics only.
    #[must_use]
    pub fn contains(&self, id: &SubSessionId) -> bool {
        self.inner.lock().map(|g| g.contains_key(id)).unwrap_or(false)
    }

    /// Live PID for `id`, if known. `None` if the runtime has been removed (kill or natural exit).
    #[must_use]
    pub fn pid(&self, id: &SubSessionId) -> Option<u32> {
        self.inner.lock().ok()?.get(id).map(|r| r.pid)
    }

    /// Whether `id` has been re-targeted to a rediscovered owner process
    /// (e.g. VS Code's long-lived owner) after launcher handoff.
    #[must_use]
    pub fn is_retargeted(&self, id: &SubSessionId) -> Option<bool> {
        let g = self.inner.lock().ok()?;
        let rt = g.get(id)?;
        Some(rt.re_targeted.load(Ordering::SeqCst))
    }

    /// Test-only helper to mark a runtime as re-targeted without driving the resolver flow. Production code paths must NOT call this; the
    /// re-target flag is normally toggled by the resolver thread under the pool lock. Kept always-available (rather than `#[cfg(test)]`) because the
    /// integration test crate (`tests/sub_sessions_e2e.rs`) compiles against the lib as an external crate where `cfg(test)` doesn't apply, and
    /// clippy with `--all-targets` builds those without the `test-helpers` feature.
    pub fn force_retargeted_for_test(&self, id: &SubSessionId, value: bool) -> bool {
        let Ok(g) = self.inner.lock() else { return false };
        match g.get(id) {
            Some(rt) => {
                rt.re_targeted.store(value, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// Explicit close. Sets the `killed` guard so the wait thread will suppress its status emission, calls `killer.kill()`, and removes the runtime
    /// from the pool. Idempotent (`Ok` if the id is unknown).
    pub fn kill(&self, id: &SubSessionId) -> Result<(), Error> {
        let removed = {
            let mut g = self.inner.lock().map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            g.remove(id)
        };
        if let Some(rt) = removed {
            rt.killed.store(true, Ordering::SeqCst);
            // Best-effort: the killer's child slot may already be empty because the wait thread took it. That's fine — `kill` returns Ok in that
            // case.
            rt.killer.kill()?;
        }
        Ok(())
    }

    /// Detach `id` from the pool without terminating the underlying process. Used when an application sub-tab is closed: the user expects the tab to
    /// disappear but their editor / file browser to keep running. Sets the `killed` guard so the wait thread (if it completes after we've stopped
    /// caring) suppresses its emission. Idempotent (`Ok(())` if the id is unknown).
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

    /// Best-effort focus on the runtime's window. Tries the stored HWND first (so VS Code workspaces don't get confused for one another), refreshes
    /// via the resolver's [`WindowFinder`] if the stored handle is stale, then falls back to `fallback.focus_pid` for the runtime PID. Returns the
    /// underlying error if every strategy fails.
    ///
    /// Returns `Err(Error::NotFound)` when no runtime is registered for `id` (the caller should treat this as a no-op — the sub-tab is already gone).
    pub fn focus(&self, id: &SubSessionId, fallback: &dyn crate::window_focus::WindowFocuser) -> Result<(), Error> {
        // Snapshot pid + window_target under the lock and drop the guard before any focus syscall — focus_hwnd / focus_pid can call into Win32, which
        // we never want to do while holding the pool mutex.
        let snapshot = {
            let g = self.inner.lock().map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            let Some(rt) = g.get(id) else {
                return Err(Error::NotFound(format!("no runtime for sub-session {id}")));
            };
            (rt.pid, rt.window_target.as_ref().map(|wt| (wt.hwnd, wt.refinder.clone())))
        };
        let pid = snapshot.0;

        if let Some((hwnd, refinder)) = snapshot.1 {
            match fallback.focus_hwnd(hwnd) {
                Ok(()) => return Ok(()),
                Err(Error::NotFound(_)) => {
                    // Stale handle. Try to re-find via the resolver's WindowFinder (if any) and update the stored HWND for next time.
                    if let Some(finder) = refinder {
                        if let Some(fresh) = finder.find_window().filter(|h| *h != 0) {
                            let updated = self.update_window_handle(id, fresh);
                            if updated {
                                if let Ok(()) = fallback.focus_hwnd(fresh) {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    // Fall through to PID-based fallback.
                }
                Err(Error::Unsupported(_)) => {
                    // Platform doesn't support hwnd-based focus; fall through to PID fallback silently.
                }
                Err(other) => return Err(other),
            }
        }

        fallback.focus_pid(pid)
    }

    /// Best-effort: ask the OS to politely close the window the resolver matched for `id`. The runtime is **not** removed from the pool — callers
    /// that want to detach the sub-tab too must also call [`detach`]. Idempotent across stale handles.
    ///
    /// Returns:
    /// * `Ok(())` on a successful PostMessage (the app may still prompt the
    ///   user before actually closing).
    /// * `Err(Error::NotFound)` when no runtime is registered, or when no
    ///   `window_target` is known and we can't act.
    /// * `Err(Error::Unsupported)` when the platform doesn't support
    ///   window-handle close (non-Windows today).
    pub fn request_window_close(&self, id: &SubSessionId, focuser: &dyn crate::window_focus::WindowFocuser) -> Result<(), Error> {
        let snapshot = {
            let g = self.inner.lock().map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            let Some(rt) = g.get(id) else {
                return Err(Error::NotFound(format!("no runtime for sub-session {id}")));
            };
            rt.window_target.as_ref().map(|wt| (wt.hwnd, wt.refinder.clone()))
        };
        let Some((hwnd, refinder)) = snapshot else {
            return Err(Error::NotFound(format!("no window target known for sub-session {id}")));
        };
        match focuser.post_close_message(hwnd) {
            Ok(()) => Ok(()),
            Err(Error::NotFound(_)) => {
                if let Some(finder) = refinder {
                    if let Some(fresh) = finder.find_window().filter(|h| *h != 0) {
                        let _ = self.update_window_handle(id, fresh);
                        return focuser.post_close_message(fresh);
                    }
                }
                Err(Error::NotFound(format!("window for sub-session {id} no longer exists")))
            }
            Err(other) => Err(other),
        }
    }

    /// Force-kill the runtime and asynchronously verify the OS actually terminated the target PID within [`APP_KILL_GRACE`]. The runtime entry is
    /// always removed from the pool by this method (the SubSessionId is free for a fresh launch) regardless of the verification outcome.
    ///
    /// **Never holds the pool lock across `.await`.** The snapshot is taken inside a tight critical section; verification polls via [`pid_alive`]
    /// off-lock.
    ///
    /// Returns [`AppKillOutcome::Unconfirmed`] when either the killer returned an error OR the PID was still alive at the end of the grace window.
    /// In both cases the underlying process **may** still be alive; the caller is expected to surface this to the user (the runtime is gone from
    /// Arborist's perspective but the user's editor/file-explorer might still be running).
    pub async fn kill_async(&self, id: &SubSessionId) -> Result<AppKillOutcome, Error> {
        self.kill_async_with_grace(id, APP_KILL_GRACE).await
    }

    /// Test seam for [`Self::kill_async`] that accepts a custom grace window. Production code paths must use [`Self::kill_async`] so the user-visible
    /// timing stays consistent.
    pub(crate) async fn kill_async_with_grace(&self, id: &SubSessionId, grace: Duration) -> Result<AppKillOutcome, Error> {
        // Snapshot the pid + killer + killed flag under a tight lock, then drop the guard before any kill/sleep work.
        let snapshot = {
            let mut g = self.inner.lock().map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            g.remove(id)
        };
        let Some(rt) = snapshot else {
            return Err(Error::NotFound(format!("no runtime for sub-session {id}")));
        };
        let pid = rt.pid;
        rt.killed.store(true, Ordering::SeqCst);

        // Issue the kill. Any error is captured (not bubbled) so we can return Unconfirmed with diagnostics intact — the caller cares whether the
        // process is gone, not whether the syscall succeeded.
        let killer_result = rt.killer.kill();
        if let Err(ref e) = killer_result {
            tracing::warn!(sub_session_id = %id, pid, error = ?e, "kill_async: killer.kill() returned error; process may still be alive at this PID");
        }

        // Verify via PID liveness with a polled grace window. We never hold the pool mutex across .await — the runtime is already removed and the
        // killer Arc is owned by this stack frame.
        let deadline = Instant::now() + grace;
        loop {
            if !pid_alive(pid) {
                drop(rt);
                return Ok(AppKillOutcome::Reaped { pid });
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(APP_LIVENESS_POLL).await;
        }
        // Drop the runtime explicitly so the wait/resolver/liveness join handles get a chance to be cleaned up (best-effort — they may still be in
        // their own loops, that's fine).
        drop(rt);
        if killer_result.is_err() {
            return Ok(AppKillOutcome::Unconfirmed { pid });
        }
        tracing::warn!(
            sub_session_id = %id,
            pid,
            grace_ms = grace.as_millis() as u64,
            "kill_async: process still alive at end of grace window; reporting Unconfirmed",
        );
        Ok(AppKillOutcome::Unconfirmed { pid })
    }

    /// Politely ask the OS to close the runtime's matched window and verify (off-lock, async) whether it actually went away within
    /// [`APP_POLITE_CLOSE_GRACE`].
    ///
    /// The runtime entry is **not** removed by this primitive — callers that want to detach the sub-tab must also call [`detach`]. This separation
    /// matches the semantics of the user-facing "ask the app to close" intent: success means "we asked, and it complied within the grace window";
    /// the tab disposition is the caller's decision.
    ///
    /// Verification strategy: if the runtime has a `window_target`, verify by window-handle existence ([`crate::window_focus::WindowFocuser::is_window_alive`]) — this is the only correct check for
    /// multi-window apps (closing one VS Code workspace doesn't kill `Code.exe`, so PID liveness would falsely report "still alive"). When
    /// `is_window_alive` returns [`Error::Unsupported`] (non-Windows today) we fall back to PID liveness; that's the best we can do without a stable
    /// handle concept.
    pub async fn request_window_close_then_wait_async(
        &self,
        id: &SubSessionId,
        focuser: &dyn crate::window_focus::WindowFocuser,
    ) -> Result<PoliteCloseOutcome, Error> {
        self.request_window_close_then_wait_async_with_grace(id, focuser, APP_POLITE_CLOSE_GRACE)
            .await
    }

    /// Test seam for [`Self::request_window_close_then_wait_async`] that accepts a custom verification grace. Production code paths must use
    /// [`Self::request_window_close_then_wait_async`] so the user-visible timing stays consistent with [`APP_POLITE_CLOSE_GRACE`].
    pub(crate) async fn request_window_close_then_wait_async_with_grace(
        &self,
        id: &SubSessionId,
        focuser: &dyn crate::window_focus::WindowFocuser,
        grace: Duration,
    ) -> Result<PoliteCloseOutcome, Error> {
        // Snapshot pid + window_target under the lock, then drop it before any focuser call or .await.
        let snapshot = {
            let g = self.inner.lock().map_err(|_| Error::Internal("app pool mutex poisoned".into()))?;
            let Some(rt) = g.get(id) else {
                return Ok(PoliteCloseOutcome::Gone);
            };
            (rt.pid, rt.window_target.as_ref().map(|wt| (wt.hwnd, wt.refinder.clone())))
        };
        let pid = snapshot.0;
        let Some((mut hwnd, refinder)) = snapshot.1 else {
            return Ok(PoliteCloseOutcome::NoTarget);
        };

        // Post the close message. On stale-handle errors, try to re-find via the resolver's WindowFinder once.
        match focuser.post_close_message(hwnd) {
            Ok(()) => {}
            Err(Error::NotFound(_)) => {
                if let Some(finder) = &refinder {
                    if let Some(fresh) = finder.find_window().filter(|h| *h != 0) {
                        let _ = self.update_window_handle(id, fresh);
                        hwnd = fresh;
                        match focuser.post_close_message(fresh) {
                            Ok(()) => {}
                            Err(Error::NotFound(_)) => {
                                // Even the refreshed handle is gone — the window already disappeared. Treat as Confirmed: that's the user-visible
                                // outcome (the window they wanted closed is closed).
                                return Ok(PoliteCloseOutcome::Confirmed { pid });
                            }
                            Err(Error::Unsupported(_)) => return Ok(PoliteCloseOutcome::Unsupported),
                            Err(other) => return Err(other),
                        }
                    } else {
                        // Window already gone, no refind available — treat as Confirmed (close-of-a-gone-thing succeeded).
                        return Ok(PoliteCloseOutcome::Confirmed { pid });
                    }
                } else {
                    return Ok(PoliteCloseOutcome::Confirmed { pid });
                }
            }
            Err(Error::Unsupported(_)) => return Ok(PoliteCloseOutcome::Unsupported),
            Err(other) => return Err(other),
        }

        // Verify the window actually went away. Verification by window handle is the only correct check for multi-window apps — closing one VS Code
        // workspace doesn't kill `Code.exe`, so PID liveness would falsely report "still alive" and we'd wait the full grace for nothing. The
        // platform error contract is:
        //   - `Ok(true)`  — window still up, keep polling
        //   - `Ok(false)` — window is gone → `Confirmed`
        //   - `Err(Unsupported)` — platform doesn't expose `is_window_alive` (non-Windows today). Permanently fall back to PID liveness; that's the
        //     best we can do without a stable handle concept.
        //   - `Err(NotFound)` — the OS rejected the handle as no longer a window. That is the user-visible-correct "window gone" signal, so treat
        //     it identically to `Ok(false)` → `Confirmed`. Falling back to PID liveness here would be wrong for shared/multi-window apps (we'd wait
        //     the full grace and then report `Posted`, which suppresses the cascade's skip-kill optimisation and shows the user a "may still be
        //     prompting to save" toast for a window that demonstrably closed).
        //   - `Err(other)` — the liveness probe is broken. Don't degrade silently to PID liveness (same multi-window mis-report risk). Log loudly
        //     and return `Posted` immediately so the user gets a fast, honest "we asked, can't verify" answer instead of waiting the grace.
        let mut window_check_supported = true;
        let deadline = Instant::now() + grace;
        loop {
            if window_check_supported {
                match focuser.is_window_alive(hwnd) {
                    Ok(true) => {}
                    Ok(false) => return Ok(PoliteCloseOutcome::Confirmed { pid }),
                    Err(Error::Unsupported(_)) => {
                        window_check_supported = false;
                    }
                    Err(Error::NotFound(_)) => return Ok(PoliteCloseOutcome::Confirmed { pid }),
                    Err(other) => {
                        tracing::warn!(pid, hwnd, error = %other, "is_window_alive failed; reporting Posted without further verification");
                        return Ok(PoliteCloseOutcome::Posted { pid });
                    }
                }
            }
            if !window_check_supported && !pid_alive(pid) {
                return Ok(PoliteCloseOutcome::Confirmed { pid });
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(APP_LIVENESS_POLL).await;
        }
        Ok(PoliteCloseOutcome::Posted { pid })
    }

    /// Updates the stored HWND on the runtime under the pool lock. Returns `false` if the runtime is no longer registered.
    fn update_window_handle(&self, id: &SubSessionId, hwnd: usize) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return false;
        };
        match g.get_mut(id) {
            Some(rt) => {
                if let Some(wt) = rt.window_target.as_mut() {
                    wt.hwnd = hwnd;
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }
}

fn app_wait_loop(
    id: SubSessionId,
    waiter: Box<dyn AppWaiter>,
    sink: AppPoolSink,
    killed: Arc<AtomicBool>,
    resolver_done: Arc<AtomicBool>,
    pool_weak: std::sync::Weak<Mutex<BTreeMap<SubSessionId, AppRuntime>>>,
) {
    let result = waiter.wait();

    // The launcher has exited. If a resolver thread is still polling for the rediscovered owner (e.g. `Code.exe` is still painting its first window),
    // give it up to `RESOLVER_GRACE_DEADLINE` to publish its final result before deciding whether to emit `Exited`. Without this grace, a
    // fast-exiting launcher (`code.cmd` returns in ~1 s) races a cold-start `Code.exe` (often 3-5 s to paint), leading to a spurious sub-tab close
    // even though VS Code is up and running.
    if !resolver_done.load(Ordering::SeqCst) && !killed.load(Ordering::SeqCst) {
        let deadline = Instant::now() + RESOLVER_GRACE_DEADLINE;
        while !resolver_done.load(Ordering::SeqCst) && !killed.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(RESOLVER_GRACE_POLL);
        }
    }

    // Three possible outcomes once the launcher exits:
    //
    //   * `Retargeted` — resolver thread already swapped this entry to a long-lived
    //     owner PID. Stay silent; the liveness thread is responsible for eventually
    //     emitting `Exited`.
    //   * `Removed` — we won the race to claim emission rights for this entry.
    //     Remove ourselves from the pool and emit.
    //   * `AlreadyGone` — `kill` / `detach` removed us first; the user-facing event
    //     was the explicit close.
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

/// Resolver thread body: blocks on [`OwnerResolver::resolve`] then, under the pool lock, swaps the runtime's `pid` and `killer` to the rediscovered
/// owner. Emits a `Running` status event with the new PID and starts the liveness thread that will emit `Exited` once the rediscovered process dies.
///
/// Always sets `resolver_done` to `true` on exit (including panic unwind) via [`ResolverDoneGuard`] so [`app_wait_loop`]'s grace poll can never hang
/// waiting for a thread that is no longer running. The Drop runs *after* this function's body returns, so on the success path the guard fires only
/// after the pid/killer swap and liveness-thread attachment are committed under the pool lock — wait_loop seeing `resolver_done=true` therefore
/// implies `re_targeted` is also visible.
fn resolver_loop(
    id: SubSessionId,
    resolver: Arc<dyn OwnerResolver>,
    sink: AppPoolSink,
    pool_weak: std::sync::Weak<Mutex<BTreeMap<SubSessionId, AppRuntime>>>,
    killed: Arc<AtomicBool>,
    resolver_done: Arc<AtomicBool>,
) {
    // Drop guard: fires on every return path AND on panic unwind so `resolver_done` is always set before this thread truly exits.
    let _done_guard = ResolverDoneGuard(resolver_done);

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
        window_target: new_window_target,
    } = bundle;

    // Spawn the liveness thread first so `wait_for_death` is consumed even if the swap below loses the race. `liveness_loop` does nothing harmful
    // when the runtime is already gone.
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
                rt.window_target = new_window_target;
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
    // If !did_retarget the runtime was removed (close/relaunch) between resolve() and the lock; the liveness thread will discover the missing entry
    // on its claim-and-emit attempt and exit silently.
}

/// RAII guard that flips its inner `AtomicBool` to `true` on drop. Used by [`resolver_loop`] so `resolver_done` is set on every return path AND on
/// panic unwind. See [`resolver_loop`] for why the timing of this signal matters relative to the pid/killer swap.
struct ResolverDoneGuard(Arc<AtomicBool>);

impl Drop for ResolverDoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Liveness thread body: blocks on the [`LivenessProbe`] until the rediscovered PID dies, then claim-and-emits `Exited` provided the runtime still
/// belongs to this PID (i.e. the user didn't already `detach` or `relaunch`).
fn liveness_loop(
    id: SubSessionId,
    new_pid: u32,
    liveness: Box<dyn LivenessProbe>,
    sink: AppPoolSink,
    pool_weak: std::sync::Weak<Mutex<BTreeMap<SubSessionId, AppRuntime>>>,
    killed: Arc<AtomicBool>,
) {
    liveness.wait_for_death();

    // Claim emission rights ONLY if the runtime is still ours (matching pid). Other outcomes — entry gone (kill/detach), or entry replaced via
    // relaunch — mean someone else already owns the user-facing event.
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

// --------------------------------------------------------------------------- Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    /// Minimal fake spawner: returns sequentially-numbered PIDs and a waiter/killer pair backed by a per-spawn `FakeApp` handle the test can drive.
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
            let pid = self.next_pid.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

        let pid = pool.spawn(id, "code .".to_owned(), PathBuf::from("."), sink, None).expect("spawn");
        assert!(pid >= 2000);
        assert!(pool.contains(&id));

        spawner.child(0).signal_exit(true);

        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "pool should self-remove after natural exit",
        );

        let statuses = status_obs.lock().unwrap().clone();
        assert!(matches!(statuses.first(), Some((SubSessionStatus::Running, Some(_)))));
        assert!(statuses.iter().any(|(s, _)| matches!(s, SubSessionStatus::Exited)));
        assert!(!exit_obs.lock().unwrap().is_empty());
    }

    #[test]
    fn pool_spawn_failed_status_on_nonzero_exit() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, _) = collect_sink();
        let id = SubSessionId::default();
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None).expect("spawn");
        spawner.child(0).signal_exit(false);
        wait_until(
            || status_obs.lock().unwrap().iter().any(|(s, _)| matches!(s, SubSessionStatus::Error)),
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
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None).expect("spawn");
        // Prime: only Running observed so far.
        wait_until(|| !status_obs.lock().unwrap().is_empty(), Duration::from_secs(2), "Running");
        pool.kill(&id).expect("kill");
        wait_until(|| !pool.contains(&id), Duration::from_secs(2), "pool should drop entry on kill");
        // Give the wait thread a beat to finish (it should have aborted status emission via the killed guard).
        std::thread::sleep(Duration::from_millis(50));
        let statuses = status_obs.lock().unwrap().clone();
        assert_eq!(
            statuses.iter().filter(|(s, _)| !matches!(s, SubSessionStatus::Running)).count(),
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
        let pid = pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None).unwrap();
        assert_eq!(pool.pid(&id), Some(pid));
        spawner.child(0).signal_exit(true);
        wait_until(|| pool.pid(&id).is_none(), Duration::from_secs(2), "pid should clear after exit");
    }

    #[test]
    fn pool_detach_removes_entry_without_killing_and_suppresses_exit_event() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None).expect("spawn");
        wait_until(|| !status_obs.lock().unwrap().is_empty(), Duration::from_secs(2), "Running");
        let killed_before = spawner.child(0).was_killed();
        pool.detach(&id);
        assert!(!pool.contains(&id), "detach removes from pool");
        assert_eq!(
            spawner.child(0).was_killed(),
            killed_before,
            "detach must NOT kill the underlying process"
        );
        // Now let the (still-running, fake) child exit naturally; the killed-guard should suppress any post-detach status emission.
        spawner.child(0).signal_exit(true);
        std::thread::sleep(Duration::from_millis(80));
        let statuses = status_obs.lock().unwrap().clone();
        assert_eq!(
            statuses.iter().filter(|(s, _)| !matches!(s, SubSessionStatus::Running)).count(),
            0,
            "no post-Running status after detach, got {statuses:?}"
        );
        assert!(exit_obs.lock().unwrap().is_empty());
    }

    // ----------------------------------------------------------------- Re-target (OwnerResolver / LivenessProbe) tests
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
            // Block until either an owner is queued or 5s elapses (so a misbehaving test fails loudly rather than hanging the whole suite).
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

    /// Test liveness probe: blocks on a Condvar until `signal_dead` is called. Mirrors `FakeWaiter`.
    struct FakeLivenessProbe {
        signal: Arc<(StdMutex<bool>, std::sync::Condvar)>,
    }

    impl FakeLivenessProbe {
        fn new_pair() -> (Box<Self>, Arc<(StdMutex<bool>, std::sync::Condvar)>) {
            let signal = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
            (Box::new(FakeLivenessProbe { signal: Arc::clone(&signal) }), signal)
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

    /// Standalone fake killer keyed by an Arc<AtomicBool>, used to observe whether `pool.kill` after retarget targets the rediscovered killer (not
    /// the original).
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
        assert_eq!(pool.is_retargeted(&id), Some(false));

        // Resolver returns a NEW pid + a fresh killer + a probe we hold the death-signal for.
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
            window_target: None,
        });

        // Pool's pid for the entry should flip to new_pid.
        wait_until(
            || pool.pid(&id) == Some(new_pid),
            Duration::from_secs(2),
            "pool.pid should flip to rediscovered owner",
        );
        wait_until(
            || pool.is_retargeted(&id) == Some(true),
            Duration::from_secs(2),
            "runtime should be marked re-targeted",
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

        // Now the launcher exits. With re_targeted=true, the wait thread MUST stay silent — no Exited event.
        spawner.child(0).signal_exit(true);
        std::thread::sleep(Duration::from_millis(120));
        let post = status_obs.lock().unwrap().clone();
        assert!(
            !post.iter().any(|(s, _)| matches!(s, SubSessionStatus::Exited)),
            "launcher exit must be suppressed after retarget; got {post:?}"
        );
        assert!(exit_obs.lock().unwrap().is_empty(), "no exited event after retarget+launcher-exit");
        // Entry must still be in the pool (liveness thread owns the exit emission now).
        assert!(pool.contains(&id), "entry must remain in pool after launcher exits if retargeted");

        // pool.kill MUST hit the NEW killer, not the launcher's.
        pool.kill(&id).expect("kill");
        assert!(
            new_killed_flag.load(Ordering::SeqCst),
            "post-retarget pool.kill must invoke the rediscovered killer"
        );
        // The original launcher's killer must NOT receive the kill — FakeAppSpawner.child(0) was already exited by us above; this is just a
        // no-double-kill assertion.
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
            window_target: None,
        });
        wait_until(|| pool.pid(&id) == Some(new_pid), Duration::from_secs(2), "pool.pid should flip");

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
        assert!(!exit_obs.lock().unwrap().is_empty(), "exited event after rediscovered owner dies");
        // We did NOT call pool.kill — killer must not have been invoked.
        assert!(!killed_flag.load(Ordering::SeqCst), "natural death must not invoke killer");
    }

    #[test]
    fn pool_resolver_returning_none_falls_through_to_normal_lifecycle() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();

        // A resolver that always returns None (e.g. the resolver gave up after timeout). We use FakeOwnerResolver and call `signal_no_retarget` to
        // release the wait without queuing an owner.
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

        // Launcher exits naturally — Exited must fire (re_targeted is still false because resolver returned None).
        spawner.child(0).signal_exit(true);
        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "pool should drop entry on natural exit when resolver returned None",
        );
        let statuses = status_obs.lock().unwrap().clone();
        assert!(statuses.iter().any(|(s, _)| matches!(s, SubSessionStatus::Exited)));
        assert!(!exit_obs.lock().unwrap().is_empty());
    }

    /// **Regression**: launcher exits at t≈0 but the rediscovered owner (e.g. cold-start `Code.exe`) doesn't paint until t≈400 ms. Without the
    /// wait-thread grace window, this would emit a spurious `Exited` and the frontend would close the VS Code sub-tab even though VS Code is still
    /// up. With the grace, the wait thread sits until the resolver publishes its result and then sees `re_targeted=true` → stays silent.
    #[test]
    fn pool_launcher_exits_before_resolver_succeeds_no_premature_exited() {
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

        // Launcher exits IMMEDIATELY — before the resolver has had a chance to find the rediscovered owner. Wait thread enters its grace poll.
        spawner.child(0).signal_exit(true);

        // No Exited / Error event yet — wait thread is in grace, entry is still in pool.
        std::thread::sleep(Duration::from_millis(200));
        assert!(pool.contains(&id), "entry must remain while wait thread is in resolver grace");
        assert!(
            !status_obs
                .lock()
                .unwrap()
                .iter()
                .any(|(s, _)| matches!(s, SubSessionStatus::Exited | SubSessionStatus::Error)),
            "no Exited/Error during grace; got {:?}",
            status_obs.lock().unwrap()
        );
        assert!(exit_obs.lock().unwrap().is_empty(), "no exited callback during grace");

        // Now the resolver finally finds the rediscovered owner.
        let new_pid = 90_001;
        let new_killer: Arc<dyn AppKiller> = Arc::new(FlagKiller {
            killed: Arc::new(AtomicBool::new(false)),
        });
        let (probe, _liveness_signal) = FakeLivenessProbe::new_pair();
        resolver.signal_retarget(RetargetedOwner {
            pid: new_pid,
            killer: new_killer,
            liveness: probe,
            window_target: None,
        });

        // Pid should flip and a second Running event with new_pid should fire. Critically, NO Exited event should EVER appear for this run — the
        // launcher exit is fully suppressed.
        wait_until(
            || pool.pid(&id) == Some(new_pid),
            Duration::from_secs(2),
            "pool.pid should flip to rediscovered owner after grace",
        );
        // Give the wait thread a generous chance to wake and decide.
        std::thread::sleep(Duration::from_millis(300));

        let statuses = status_obs.lock().unwrap().clone();
        assert!(
            !statuses
                .iter()
                .any(|(s, _)| matches!(s, SubSessionStatus::Exited | SubSessionStatus::Error)),
            "launcher exit suppressed after late retarget; got {statuses:?}"
        );
        assert!(exit_obs.lock().unwrap().is_empty(), "no exited callback after late retarget");
        assert!(pool.contains(&id), "entry remains because liveness owns final exit");
        // Sanity: the Running events for both pids fired in order.
        let running_pids: Vec<_> = statuses
            .iter()
            .filter_map(|(s, p)| matches!(s, SubSessionStatus::Running).then_some(*p))
            .collect();
        assert_eq!(
            running_pids,
            vec![Some(initial_pid), Some(new_pid)],
            "Running events: launcher pid then rediscovered pid"
        );
    }

    /// **Regression**: while the wait thread is sitting in the resolver-grace poll, an explicit `kill` (or `detach`) must wake it within a few ticks
    /// AND must suppress the Exited event so the user-facing event remains the explicit close.
    #[test]
    fn pool_kill_during_resolver_grace_suppresses_exit_and_returns_promptly() {
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

        // Launcher exits → wait thread enters grace.
        spawner.child(0).signal_exit(true);
        std::thread::sleep(Duration::from_millis(80));
        assert!(pool.contains(&id), "still in grace");

        // Explicit close while we're in grace.
        let start = Instant::now();
        pool.kill(&id).expect("kill");
        // The wait thread should wake on its next poll tick (50 ms) and see `killed=true`, returning silently. Give a generous upper bound to keep
        // the test non-flaky in CI.
        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "kill must remove entry promptly even while wait thread is grace-polling",
        );
        // No Exited/Error from the wait thread — the explicit close owns the user-facing signal.
        let statuses = status_obs.lock().unwrap().clone();
        assert!(
            !statuses
                .iter()
                .any(|(s, _)| matches!(s, SubSessionStatus::Exited | SubSessionStatus::Error)),
            "kill during grace must suppress wait-thread emission; got {statuses:?}"
        );
        assert!(exit_obs.lock().unwrap().is_empty(), "no exited callback after kill during grace");
        // Don't assert a tight upper bound on kill latency — CI schedulers can stall threads. Just ensure it didn't sit for the full grace window.
        assert!(start.elapsed() < RESOLVER_GRACE_DEADLINE, "kill returned within grace window");
    }

    /// **Regression for the pre-registration race**: with a waiter that exits *immediately* (before the wait thread is even scheduled), the runtime
    /// must still be observable via the pool for the duration of the user-facing signalling. Previously the pool inserted the runtime AFTER spawning
    /// the wait thread, so a fast-exit waiter could observe an empty pool, treat the entry as `AlreadyGone`, and silently exit — leaving the runtime
    /// orphaned in the pool with nobody to emit `Exited`.
    #[test]
    fn pool_immediately_exiting_waiter_emits_exited_and_clears_pool() {
        struct InstantExitSpawner {
            next_pid: std::sync::atomic::AtomicU32,
        }
        impl AppSpawner for InstantExitSpawner {
            fn spawn(&self, _cmd: &str, _cwd: &Path) -> Result<SpawnedApp, Error> {
                struct InstantWaiter;
                impl AppWaiter for InstantWaiter {
                    fn wait(self: Box<Self>) -> Result<bool, Error> {
                        Ok(true)
                    }
                }
                struct NoopKiller;
                impl AppKiller for NoopKiller {
                    fn kill(&self) -> Result<(), Error> {
                        Ok(())
                    }
                }
                Ok(SpawnedApp {
                    pid: self.next_pid.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                    waiter: Box::new(InstantWaiter),
                    killer: Arc::new(NoopKiller),
                })
            }
        }

        let pool = AppPool::new(Arc::new(InstantExitSpawner {
            next_pid: std::sync::atomic::AtomicU32::new(70_000),
        }));
        let (sink, status_obs, exit_obs) = collect_sink();
        let id = SubSessionId::default();

        // No owner_resolver — `resolver_done` is `true` synchronously so the wait thread skips the grace window entirely.
        pool.spawn(id, "x".to_owned(), PathBuf::from("."), sink, None).expect("spawn");

        // Entry must clear and Exited must fire even though the waiter returned before the wait thread was scheduled.
        wait_until(
            || !pool.contains(&id),
            Duration::from_secs(2),
            "pool entry cleared even when waiter exits before thread scheduling",
        );
        let statuses = status_obs.lock().unwrap().clone();
        assert!(
            statuses.iter().any(|(s, _)| matches!(s, SubSessionStatus::Exited)),
            "Exited must fire after immediate-exit waiter; got {statuses:?}"
        );
        assert!(
            !exit_obs.lock().unwrap().is_empty(),
            "exited callback must fire after immediate-exit waiter"
        );
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
            window_target: None,
        });
        wait_until(|| pool.pid(&id) == Some(new_pid), Duration::from_secs(2), "retarget happened");

        // Closing the sub-tab uses detach (not kill). The rediscovered editor process must NOT be killed.
        pool.detach(&id);
        std::thread::sleep(Duration::from_millis(40));
        assert!(!killed_flag.load(Ordering::SeqCst), "detach must not invoke the rediscovered killer");
        assert!(resolver.was_called(), "resolver should have been invoked");
    }

    #[test]
    fn pid_killer_for_dead_pid_returns_ok_no_panic() {
        // PID extremely unlikely to exist. Cross-platform: this is the single-process smoke test for `PidKiller` — no actual victim is killed, but we
        // exercise the OS call paths.
        let killer = PidKiller::new(4_294_967);
        assert!(killer.kill().is_ok());
    }

    #[test]
    fn real_spawner_returns_tool_missing_for_unknown_simple_command() {
        let r = RealAppSpawner.spawn("definitely-not-a-real-binary-xyzzy", Path::new("."));
        assert!(matches!(r, Err(Error::ToolMissing(t)) if t == "definitely-not-a-real-binary-xyzzy"));
    }

    #[test]
    fn first_token_if_simple_skips_metachars() {
        assert!(first_token_if_simple("echo hi | grep h").is_none());
        assert_eq!(first_token_if_simple("code .").as_deref(), Some("code"));
    }

    /// Real-process smoke test: spawn a trivial cross-platform "exits quickly" command and observe the lifecycle. Cheap, deterministic.
    ///
    /// The wait deadline is generous (30 s) on purpose: this test spawns a real OS process and Windows process creation can stall for many seconds
    /// when the test suite runs under heavy parallel load (multiple worktrees / test binaries fighting for CPU and disk). The `wait_until` loop
    /// exits as soon as `pool.contains` returns false, so the slack is free on the happy path and only paid when the OS scheduler is overloaded.
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

        wait_until(|| !pool.contains(&id), Duration::from_secs(30), "real child should exit");
        let statuses = status_obs.lock().unwrap().clone();
        assert!(matches!(statuses.first(), Some((SubSessionStatus::Running, Some(_)))));
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

    /// Test [`WindowFinder`] backed by a queue of HWNDs.
    struct QueuedFinder {
        next: StdMutex<std::collections::VecDeque<Option<usize>>>,
        calls: AtomicU64,
    }

    impl QueuedFinder {
        fn new(values: impl IntoIterator<Item = Option<usize>>) -> Arc<Self> {
            Arc::new(Self {
                next: StdMutex::new(values.into_iter().collect()),
                calls: AtomicU64::new(0),
            })
        }
        fn call_count(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl WindowFinder for QueuedFinder {
        fn find_window(&self) -> Option<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.next.lock().unwrap().pop_front().flatten()
        }
    }

    use std::sync::atomic::AtomicU64;

    fn spawn_with_window_target(pool: &AppPool, target: WindowTarget) -> (SubSessionId, u32) {
        let id = SubSessionId::default();
        let (sink, _s, _e) = collect_sink();
        let runtime_pid = pool.spawn(id, "noop".into(), PathBuf::from("."), sink, None).expect("spawn");
        // Inject the window target directly so the test doesn't need to drive a full resolver loop.
        {
            let mut g = pool.inner.lock().unwrap();
            let rt = g.get_mut(&id).unwrap();
            rt.window_target = Some(target);
        }
        (id, runtime_pid)
    }

    #[test]
    fn focus_uses_stored_hwnd_when_present() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let target = WindowTarget {
            pid: 4242,
            hwnd: 0xABCD,
            refinder: None,
        };
        let (id, _pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_hwnd(Ok(()));
        pool.focus(&id, &focuser).expect("focus");

        assert_eq!(focuser.hwnd_calls(), vec![0xABCD]);
        assert!(focuser.calls().is_empty(), "must not fall back to PID");
    }

    #[test]
    fn focus_falls_back_to_refinder_then_pid_when_hwnd_stale() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);

        // Refinder returns a fresh handle on first call.
        let finder = QueuedFinder::new(vec![Some(0xCAFE)]);
        let target = WindowTarget {
            pid: 7777,
            hwnd: 0xDEAD,
            refinder: Some(finder.clone() as Arc<dyn WindowFinder>),
        };
        let (id, _pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_hwnd(Err(Error::NotFound("stale".into())));
        focuser.queue_hwnd(Ok(())); // refreshed handle
        pool.focus(&id, &focuser).expect("focus");

        assert_eq!(focuser.hwnd_calls(), vec![0xDEAD, 0xCAFE]);
        assert_eq!(finder.call_count(), 1);
        assert!(focuser.calls().is_empty(), "PID fallback not needed");
    }

    #[test]
    fn focus_falls_back_to_pid_when_no_window_target() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let id = SubSessionId::default();
        let (sink, _s, _e) = collect_sink();
        let pid = pool.spawn(id, "noop".into(), PathBuf::from("."), sink, None).expect("spawn");

        let focuser = crate::window_focus::RecordingFocuser::new();
        pool.focus(&id, &focuser).expect("focus");
        assert_eq!(focuser.calls(), vec![pid]);
        assert!(focuser.hwnd_calls().is_empty());
    }

    #[test]
    fn focus_falls_back_to_pid_when_hwnd_stale_and_refinder_empty() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let finder = QueuedFinder::new(vec![None]);
        let target = WindowTarget {
            pid: 5151,
            hwnd: 0xBEEF,
            refinder: Some(finder as Arc<dyn WindowFinder>),
        };
        let (id, runtime_pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_hwnd(Err(Error::NotFound("stale".into())));
        pool.focus(&id, &focuser).expect("focus");
        // Falls back to the runtime PID (NOT the WindowTarget.pid), matching production semantics: window_target tracks the resolver's identification
        // of a specific window, but the PID-based fallback always targets whatever PID the runtime currently believes the owner is.
        assert_eq!(focuser.calls(), vec![runtime_pid]);
    }

    #[test]
    fn focus_returns_not_found_when_runtime_unknown() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let focuser = crate::window_focus::RecordingFocuser::new();
        let res = pool.focus(&SubSessionId::default(), &focuser);
        assert!(matches!(res, Err(Error::NotFound(_))));
    }

    #[test]
    fn request_window_close_posts_to_stored_hwnd() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let target = WindowTarget {
            pid: 1234,
            hwnd: 0xFACE,
            refinder: None,
        };
        let (id, _pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Ok(()));
        pool.request_window_close(&id, &focuser).expect("close");
        assert_eq!(focuser.close_calls(), vec![0xFACE]);

        // Runtime is NOT removed by request_window_close.
        assert!(pool.contains(&id));
    }

    #[test]
    fn request_window_close_retries_via_refinder_on_stale_handle() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let finder = QueuedFinder::new(vec![Some(0x9999)]);
        let target = WindowTarget {
            pid: 1234,
            hwnd: 0x1111,
            refinder: Some(finder.clone() as Arc<dyn WindowFinder>),
        };
        let (id, _pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Err(Error::NotFound("stale".into())));
        focuser.queue_close(Ok(()));
        pool.request_window_close(&id, &focuser).expect("close");
        assert_eq!(focuser.close_calls(), vec![0x1111, 0x9999]);
    }

    #[test]
    fn request_window_close_returns_not_found_when_no_target() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let id = SubSessionId::default();
        let (sink, _s, _e) = collect_sink();
        let _ = pool.spawn(id, "noop".into(), PathBuf::from("."), sink, None).expect("spawn");
        let focuser = crate::window_focus::RecordingFocuser::new();
        let res = pool.request_window_close(&id, &focuser);
        assert!(matches!(res, Err(Error::NotFound(_))));
        assert!(focuser.close_calls().is_empty());
    }

    // --- Async kill/polite-close primitives (issue #132) ---------------------------------------------------------------------------------------

    /// `pid_alive` for a PID we never owned and that is extremely unlikely to be a system process should return false on every platform. Used as a
    /// stable Reaped-path PID for the async kill tests so they don't depend on the test runner's PID landscape.
    const DEAD_PID: u32 = 4_294_966;

    /// Inject a known-dead PID into a runtime entry so `kill_async` will see `pid_alive == false` immediately. Used to deterministically exercise the
    /// Reaped branch without depending on the FakeAppSpawner's PID sequence happening to be unused.
    fn override_pid(pool: &AppPool, id: &SubSessionId, pid: u32) {
        let mut g = pool.inner.lock().unwrap();
        let rt = g.get_mut(id).expect("runtime in pool");
        rt.pid = pid;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kill_async_reaped_when_pid_is_dead() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, _s, _e) = collect_sink();
        let id = SubSessionId::default();
        pool.spawn(id, "noop".into(), PathBuf::from("."), sink, None).expect("spawn");
        override_pid(&pool, &id, DEAD_PID);
        let outcome = pool.kill_async_with_grace(&id, Duration::from_millis(500)).await.expect("kill_async");
        assert_eq!(outcome, AppKillOutcome::Reaped { pid: DEAD_PID });
        assert!(
            spawner.child(0).was_killed(),
            "killer.kill() must be invoked even on the Reaped fast-path"
        );
        assert!(!pool.contains(&id), "runtime entry must be removed regardless of outcome");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kill_async_unconfirmed_when_pid_stays_alive() {
        // Use the test process's own PID — guaranteed alive for the duration of the test. FakeKiller flips a flag but never actually kills (and even
        // if it tried, the OS would not let it kill the test runner). The grace window is tiny so the test finishes quickly.
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner.clone());
        let (sink, _s, _e) = collect_sink();
        let id = SubSessionId::default();
        pool.spawn(id, "noop".into(), PathBuf::from("."), sink, None).expect("spawn");
        let own_pid = std::process::id();
        override_pid(&pool, &id, own_pid);
        let outcome = pool.kill_async_with_grace(&id, Duration::from_millis(150)).await.expect("kill_async");
        assert_eq!(outcome, AppKillOutcome::Unconfirmed { pid: own_pid });
        assert!(!pool.contains(&id), "runtime entry must be removed regardless of outcome");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kill_async_returns_not_found_for_unknown_id() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let result = pool.kill_async(&SubSessionId::default()).await;
        assert!(matches!(result, Err(Error::NotFound(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_confirmed_when_window_dies_during_grace() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let target = WindowTarget {
            pid: DEAD_PID,
            hwnd: 0xABCD,
            refinder: None,
        };
        let (id, runtime_pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Ok(()));
        focuser.queue_alive(Ok(true));
        focuser.queue_alive(Ok(false));

        let outcome = pool
            .request_window_close_then_wait_async_with_grace(&id, &focuser, Duration::from_millis(500))
            .await
            .expect("polite close");
        match outcome {
            PoliteCloseOutcome::Confirmed { pid } => assert_eq!(pid, runtime_pid),
            other => panic!("expected Confirmed, got {other:?}"),
        }
        assert_eq!(focuser.close_calls(), vec![0xABCD]);
        // Runtime is NOT removed by polite close — the caller decides the tab disposition.
        assert!(pool.contains(&id));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_posted_when_window_stays_alive() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let target = WindowTarget {
            pid: std::process::id(),
            hwnd: 0xBEEF,
            refinder: None,
        };
        let (id, runtime_pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Ok(()));
        // Default is_window_alive on RecordingFocuser without queued results returns Ok(true) — the window stays alive for the entire grace window.

        let outcome = pool
            .request_window_close_then_wait_async_with_grace(&id, &focuser, Duration::from_millis(150))
            .await
            .expect("polite close");
        match outcome {
            PoliteCloseOutcome::Posted { pid } => assert_eq!(pid, runtime_pid),
            other => panic!("expected Posted, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_unsupported_when_post_unsupported() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let target = WindowTarget {
            pid: 1234,
            hwnd: 0x1234,
            refinder: None,
        };
        let (id, _pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Err(Error::Unsupported("no polite close on this platform".into())));
        let outcome = pool
            .request_window_close_then_wait_async_with_grace(&id, &focuser, Duration::from_millis(50))
            .await
            .expect("polite close");
        assert!(matches!(outcome, PoliteCloseOutcome::Unsupported));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_no_target_when_no_window() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let id = SubSessionId::default();
        let (sink, _s, _e) = collect_sink();
        pool.spawn(id, "noop".into(), PathBuf::from("."), sink, None).expect("spawn");
        let focuser = crate::window_focus::RecordingFocuser::new();
        let outcome = pool
            .request_window_close_then_wait_async_with_grace(&id, &focuser, Duration::from_millis(50))
            .await
            .expect("polite close");
        assert!(matches!(outcome, PoliteCloseOutcome::NoTarget));
        assert!(focuser.close_calls().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_gone_when_runtime_unknown() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let focuser = crate::window_focus::RecordingFocuser::new();
        let outcome = pool
            .request_window_close_then_wait_async_with_grace(&SubSessionId::default(), &focuser, Duration::from_millis(50))
            .await
            .expect("polite close");
        assert!(matches!(outcome, PoliteCloseOutcome::Gone));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_refinds_stale_window_handle() {
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let finder = QueuedFinder::new(vec![Some(0x9999)]);
        let target = WindowTarget {
            pid: DEAD_PID,
            hwnd: 0x1111,
            refinder: Some(finder.clone() as Arc<dyn WindowFinder>),
        };
        let (id, runtime_pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Err(Error::NotFound("stale".into())));
        focuser.queue_close(Ok(()));
        focuser.queue_alive(Ok(false));

        let outcome = pool
            .request_window_close_then_wait_async_with_grace(&id, &focuser, Duration::from_millis(500))
            .await
            .expect("polite close");
        assert!(matches!(outcome, PoliteCloseOutcome::Confirmed { pid } if pid == runtime_pid));
        assert_eq!(focuser.close_calls(), vec![0x1111, 0x9999]);
        assert_eq!(finder.call_count(), 1, "refinder should be called exactly once on stale handle");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_confirmed_when_verification_reports_window_gone_via_not_found() {
        // Regression for the verification-loop fallback: if `is_window_alive` ever returns `Err(NotFound)` (e.g. a future platform that surfaces
        // stale-handle errors instead of `Ok(false)`), the user-correct interpretation is "the window is gone" → `Confirmed`. Previously we silently
        // demoted to PID liveness, which for shared/multi-window apps would have waited the full grace and returned `Posted` instead.
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let target = WindowTarget {
            pid: std::process::id(), // host PID stays alive → ensures we are not accidentally relying on PID-liveness
            hwnd: 0xDEAD,
            refinder: None,
        };
        let (id, runtime_pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Ok(()));
        focuser.queue_alive(Ok(true)); // first poll: still up
        focuser.queue_alive(Err(Error::NotFound("window vanished between polls".into())));

        let outcome = pool
            .request_window_close_then_wait_async_with_grace(&id, &focuser, Duration::from_secs(5))
            .await
            .expect("polite close");
        assert!(matches!(outcome, PoliteCloseOutcome::Confirmed { pid } if pid == runtime_pid));
        assert_eq!(focuser.alive_calls(), vec![0xDEAD, 0xDEAD]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polite_close_async_posted_immediately_when_liveness_check_is_broken() {
        // Regression for the verification-loop fallback: a non-Unsupported/NotFound error from `is_window_alive` means the probe is broken. We must
        // NOT silently degrade to PID liveness (wrong for multi-window apps) nor force the user to wait the full grace on a broken check. Returning
        // `Posted` immediately yields the fast, honest "we asked, can't verify" answer.
        let spawner = Arc::new(FakeAppSpawner::new());
        let pool = AppPool::new(spawner);
        let target = WindowTarget {
            pid: std::process::id(),
            hwnd: 0xBABE,
            refinder: None,
        };
        let (id, runtime_pid) = spawn_with_window_target(&pool, target);

        let focuser = crate::window_focus::RecordingFocuser::new();
        focuser.queue_close(Ok(()));
        focuser.queue_alive(Err(Error::Internal("probe failed".into())));

        let started = Instant::now();
        let outcome = pool
            // 5s grace would make the test slow if we wrongly waited it out.
            .request_window_close_then_wait_async_with_grace(&id, &focuser, Duration::from_secs(5))
            .await
            .expect("polite close");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "should return immediately on broken liveness probe; took {:?}",
            started.elapsed()
        );
        assert!(matches!(outcome, PoliteCloseOutcome::Posted { pid } if pid == runtime_pid));
        assert_eq!(focuser.alive_calls(), vec![0xBABE]);
    }
}
