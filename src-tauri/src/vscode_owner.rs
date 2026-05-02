//! Windows-only VS Code owner re-discovery.
//!
//! See `dev/ai/CONTEXT_MENU_PLAN.md` (post-PR-29 follow-up). When the
//! user opens a `vscode` application sub-tab, the actual command that
//! runs is `code .`. On Windows this resolves to `code.cmd`, which
//! launches a Node.js helper that EITHER:
//!
//!   * **No VS Code already running** — spawns the long-lived
//!     `Code.exe` (the editor) and exits ~1 s later.
//!   * **VS Code already running** — sends an IPC message to the
//!     existing `Code.exe`, which opens the folder in a new window;
//!     the launcher exits ~1 s later. The new window's `Code.exe` is
//!     **not** a descendant of our launcher PID.
//!
//! Either way, the PID Arborist captured is dead within seconds and
//! `focus_pid` returns [`Error::NotFound`]. This module re-discovers
//! the long-lived `Code.exe` whose window owns the workspace and
//! retargets the [`AppRuntime`] so subsequent focus/kill calls hit the
//! correct process.
//!
//! ## Identification strategy
//!
//! VS Code formats top-level window titles as
//! `<filename> - <workspace folder> - Visual Studio Code`. We
//! enumerate top-level visible windows and pick the first whose title
//! ends with ` - Visual Studio Code` (case-sensitive — that's how VS
//! Code formats it) and contains the workspace folder's basename
//! (case-insensitive, since NTFS is case-insensitive).
//!
//! No `OpenProcess`, no PEB / NT internals — same Win32 surface area
//! as [`crate::window_focus`].
//!
//! ## Polling
//!
//! 500 ms × 16 iterations = 8 s deadline. Empirically VS Code paints
//! its first window within 2 s on a warm machine and ~5 s on a cold
//! one. Beyond 8 s we give up and the sub-session keeps the launcher
//! PID (focus will NotFound, user can relaunch).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app_launcher::{
    AppKiller, LivenessProbe, OwnerResolver, PidKiller, RetargetedOwner, WindowFinder, WindowTarget,
};
use crate::cmd_resolver::ShellTokens;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const POLL_DEADLINE: Duration = Duration::from_secs(8);
const TITLE_SUFFIX: &str = " - Visual Studio Code";

/// First-token program names recognised as launching VS Code. Matched
/// case-insensitively against the first whitespace-delimited token of
/// `def.command` after stripping any leading `env … VAR=val` prefix.
/// Includes Windows-specific extensions and the official Insiders
/// channel.
const VSCODE_PROGRAM_NAMES: &[&str] = &[
    "code",
    "code.cmd",
    "code.exe",
    "code-insiders",
    "code-insiders.cmd",
    "code-insiders.exe",
];

/// True when the `def.command` string launches VS Code (stable or
/// Insiders, with or without an absolute path or `.cmd`/`.exe`
/// extension).
///
/// Used so user-defined VS Code launchers — not just the built-in
/// `vscode` def — get the [`VsCodeOwnerResolver`] wired up. Without
/// this, a custom def named "VSCode" with command `code .` would
/// never have its launcher PID re-targeted, leaving the sub-tab
/// pointing at a dead `code.cmd` shim within ~1 second of launch.
#[must_use]
pub fn looks_like_vscode_command(command: &str) -> bool {
    // Quote-aware token iterator: handles leading `"path with spaces"`
    // and skips over leading `env`/`KEY=value` shell prefixes so that
    // e.g. `env ELECTRON_RUN_AS_NODE=0 code .` is still recognised.
    for t in ShellTokens::new(command) {
        if t == "env" {
            continue;
        }
        if !t.starts_with('/')
            && !t.starts_with('\\')
            && !t.contains(['\\', '/'])
            && t.contains('=')
        {
            // Looks like `KEY=value` env prefix — keep skipping.
            continue;
        }
        return is_vscode_program_token(&t);
    }
    false
}

fn is_vscode_program_token(token: &str) -> bool {
    let basename = std::path::Path::new(token)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(token);
    let lower = basename.to_ascii_lowercase();
    VSCODE_PROGRAM_NAMES.iter().any(|n| n == &lower.as_str())
}

/// [`OwnerResolver`] that re-discovers the long-lived `Code.exe`
/// window owning a given workspace folder. Constructed per-spawn with
/// the worktree path; the basename is matched against the VS Code
/// window title.
pub struct VsCodeOwnerResolver {
    worktree_path: PathBuf,
}

impl VsCodeOwnerResolver {
    #[must_use]
    pub fn new(worktree_path: PathBuf) -> Self {
        Self { worktree_path }
    }

    /// Per-platform PID + HWND lookup. Returns `Some((pid, hwnd))`
    /// if a matching window is found at the moment of the call,
    /// `None` otherwise. `hwnd` is the platform window handle cast to
    /// `usize` (a real Win32 HWND on Windows).
    fn find_now(&self) -> Option<(u32, usize)> {
        let basename = self.basename()?;
        platform::find_vscode_window(&basename)
    }

    /// Basename of the worktree path. Used both for window matching
    /// during resolve and for the liveness probe so it can detect
    /// "workspace window closed" without requiring `Code.exe` itself
    /// to die. Returned in its original case; `find_vscode_window`
    /// lowercases internally and matches case-insensitively against
    /// the window title.
    fn basename(&self) -> Option<String> {
        self.worktree_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_owned())
    }
}

impl OwnerResolver for VsCodeOwnerResolver {
    fn resolve(&self) -> Option<RetargetedOwner> {
        let basename = self.basename()?;
        let deadline = Instant::now() + POLL_DEADLINE;
        loop {
            if let Some((pid, hwnd)) = self.find_now() {
                let killer: Arc<dyn AppKiller> = Arc::new(PidKiller::new(pid));
                let liveness: Box<dyn LivenessProbe> =
                    platform::liveness_probe(pid, basename.clone());
                let window_target = Some(WindowTarget {
                    pid,
                    hwnd,
                    refinder: Some(Arc::new(VsCodeWindowFinder {
                        basename: basename.clone(),
                    })),
                });
                return Some(RetargetedOwner {
                    pid,
                    killer,
                    liveness,
                    window_target,
                });
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// [`WindowFinder`] that re-runs the VS Code title heuristic, used as
/// the stale-handle escape hatch for [`crate::app_launcher::AppPool::focus`]
/// and [`crate::app_launcher::AppPool::request_window_close`].
///
/// On non-Windows platforms `find_window` returns `None` (the
/// platform module's `find_vscode_window` is a stub).
pub struct VsCodeWindowFinder {
    basename: String,
}

impl WindowFinder for VsCodeWindowFinder {
    fn find_window(&self) -> Option<usize> {
        platform::find_vscode_window(&self.basename).map(|(_pid, hwnd)| hwnd)
    }
}

// ---------------------------------------------------------------------------
// Platform: Windows
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
    use super::TITLE_SUFFIX;
    use crate::app_launcher::LivenessProbe;
    use std::ffi::c_void;
    use std::time::Duration;

    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type HWND = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type HANDLE = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(clippy::upper_case_acronyms)]
    type LPARAM = isize;

    const SYNCHRONIZE: DWORD = 0x0010_0000;
    const WAIT_OBJECT_0: DWORD = 0;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(HWND, LPARAM) -> BOOL, lparam: LPARAM) -> BOOL;
        fn GetWindowThreadProcessId(hwnd: HWND, pid_out: *mut DWORD) -> DWORD;
        fn IsWindowVisible(hwnd: HWND) -> BOOL;
        fn GetWindowTextLengthW(hwnd: HWND) -> i32;
        fn GetWindowTextW(hwnd: HWND, buf: *mut u16, max_count: i32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn CloseHandle(handle: HANDLE) -> BOOL;
        fn WaitForSingleObject(handle: HANDLE, timeout_ms: DWORD) -> DWORD;
    }

    struct EnumState {
        // Lowercased basename to match against (case-insensitive).
        needle: String,
        // First matching (pid, hwnd), if any. Stored as `usize`
        // because HWND is `*mut c_void` which isn't `Send`; storing
        // the raw integer avoids the need for unsafe `Send` impls and
        // matches the wire format used elsewhere
        // (e.g. `crate::app_launcher::WindowTarget::hwnd`).
        found: Option<(u32, usize)>,
    }

    extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is a `&mut EnumState` we set in `find_vscode_window`.
        let state = unsafe { &mut *(lparam as *mut EnumState) };
        // SAFETY: hwnd comes from EnumWindows and is valid.
        let visible = unsafe { IsWindowVisible(hwnd) } != 0;
        if !visible {
            return 1;
        }
        // SAFETY: see above.
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return 1;
        }
        let cap = (len as usize) + 1;
        let mut buf: Vec<u16> = vec![0u16; cap];
        // SAFETY: buf has space for `cap` u16s. Returned `n` is the
        // number of code units written (excluding the null terminator).
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), cap as i32) };
        if n <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if !title.ends_with(TITLE_SUFFIX) {
            return 1;
        }
        if !title.to_lowercase().contains(&state.needle) {
            return 1;
        }
        let mut pid: DWORD = 0;
        // SAFETY: pid is a valid &mut DWORD; hwnd is valid.
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != 0 {
            state.found = Some((pid, hwnd as usize));
            return 0; // stop enumeration
        }
        1
    }

    /// Returns `(pid, hwnd_as_usize)` for the first visible top-level
    /// window whose title matches the VS Code workspace pattern.
    pub(super) fn find_vscode_window(basename: &str) -> Option<(u32, usize)> {
        let mut state = EnumState {
            needle: basename.to_lowercase(),
            found: None,
        };
        // SAFETY: enum_proc reads lparam as &mut EnumState; we pass
        // exactly that.
        unsafe {
            EnumWindows(enum_proc, &mut state as *mut _ as LPARAM);
        }
        state.found
    }

    /// Window-based liveness probe with PID death as a fallback.
    ///
    /// VS Code is multi-window: closing **the workspace** doesn't
    /// necessarily kill `Code.exe` — the user may have other
    /// windows / workspaces open in the same process tree. A
    /// PID-only probe would therefore wait forever for a process
    /// that's still running, leaving the sub-tab stuck on `Running`
    /// long after the workspace was closed.
    ///
    /// Strategy: every 1 s, (1) poll the matched PID with
    /// `WaitForSingleObject(0)` so the probe still fires immediately
    /// when `Code.exe` itself exits; (2) re-enumerate top-level
    /// windows for one whose title still matches the workspace
    /// folder. If the window is gone for two consecutive polls (a
    /// 1 s grace to avoid spurious misses during e.g. a quick
    /// title-bar repaint), report the workspace closed. Either
    /// signal returning ends the probe; the [`AppPool`] then emits
    /// `Exited` and the sub-tab goes grey.
    pub(super) fn liveness_probe(pid: u32, basename: String) -> Box<dyn LivenessProbe> {
        Box::new(WindowsLivenessProbe { pid, basename })
    }

    struct WindowsLivenessProbe {
        pid: u32,
        basename: String,
    }
    impl LivenessProbe for WindowsLivenessProbe {
        fn wait_for_death(self: Box<Self>) {
            // Number of consecutive polls in which the workspace
            // window wasn't found. Two-poll debounce guards against
            // a transient "no window matched right now" miss
            // (window briefly without the suffix during a title
            // animation, EnumWindows racing a window list update,
            // etc.). 2 polls × 1 s = up to ~2 s detection latency
            // for a closed workspace, which the user perceives as
            // "near-instant".
            let mut window_gone_polls: u32 = 0;
            const WINDOW_GONE_THRESHOLD: u32 = 2;

            loop {
                // (1) Process-death fast path. If `Code.exe` itself
                // exited (last window closed → app quits) the
                // wait returns immediately.
                // SAFETY: literal access mask + PID.
                let h = unsafe { OpenProcess(SYNCHRONIZE, 0, self.pid) };
                if !h.is_null() {
                    // SAFETY: h is a valid handle returned just above.
                    let r = unsafe { WaitForSingleObject(h, 0) };
                    // SAFETY: h is valid.
                    unsafe { CloseHandle(h) };
                    if r == WAIT_OBJECT_0 {
                        return;
                    }
                } else {
                    // Process gone or we lost rights — treat as exited.
                    return;
                }

                // (2) Window-based check. If the workspace window
                // is no longer enumerable for `WINDOW_GONE_THRESHOLD`
                // consecutive polls, the workspace was closed.
                if find_vscode_window(&self.basename).is_none() {
                    window_gone_polls = window_gone_polls.saturating_add(1);
                    if window_gone_polls >= WINDOW_GONE_THRESHOLD {
                        return;
                    }
                } else {
                    window_gone_polls = 0;
                }

                std::thread::sleep(Duration::from_millis(1_000));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Platform: non-Windows — VS Code re-discovery is a no-op.
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
mod platform {
    use crate::app_launcher::LivenessProbe;

    pub(super) fn find_vscode_window(_basename: &str) -> Option<(u32, usize)> {
        None
    }

    pub(super) fn liveness_probe(_pid: u32, _basename: String) -> Box<dyn LivenessProbe> {
        // Should never be called on non-Windows because find_vscode_window
        // returns None, but provide a dummy in case the resolver is
        // wired up for tests / future extension.
        struct Never;
        impl LivenessProbe for Never {
            fn wait_for_death(self: Box<Self>) {
                // Park indefinitely. In practice the resolver thread
                // won't construct us — find_vscode_window returns None
                // on these platforms — but if someone does call this,
                // we park rather than busy-loop or panic.
                loop {
                    std::thread::park();
                }
            }
        }
        Box::new(Never)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_for_nonexistent_workspace_returns_none() {
        // Construct with a clearly-nonexistent workspace name and use
        // a tight test-only deadline. This exercises the polling loop
        // structure (real Windows search runs but finds nothing) and
        // proves resolve() honours its deadline.
        let r = VsCodeOwnerResolverWithDeadline {
            inner: VsCodeOwnerResolver::new(PathBuf::from(
                "definitely-not-a-real-workspace-zzzqqq-arborist-test",
            )),
            deadline: Duration::from_millis(50),
        };
        let result = r.resolve();
        assert!(result.is_none(), "expected None for nonsense basename");
    }

    /// Test wrapper: same logic as [`VsCodeOwnerResolver`] but with an
    /// injectable deadline so tests don't wait the full 8 s production
    /// budget.
    struct VsCodeOwnerResolverWithDeadline {
        inner: VsCodeOwnerResolver,
        deadline: Duration,
    }

    impl OwnerResolver for VsCodeOwnerResolverWithDeadline {
        fn resolve(&self) -> Option<RetargetedOwner> {
            let basename = self.inner.basename()?;
            let stop_at = Instant::now() + self.deadline;
            loop {
                if let Some((pid, hwnd)) = self.inner.find_now() {
                    let killer: Arc<dyn AppKiller> = Arc::new(PidKiller::new(pid));
                    let liveness: Box<dyn LivenessProbe> =
                        platform::liveness_probe(pid, basename.clone());
                    let window_target = Some(WindowTarget {
                        pid,
                        hwnd,
                        refinder: Some(Arc::new(VsCodeWindowFinder {
                            basename: basename.clone(),
                        })),
                    });
                    return Some(RetargetedOwner {
                        pid,
                        killer,
                        liveness,
                        window_target,
                    });
                }
                if Instant::now() >= stop_at {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn looks_like_vscode_command_matches_canonical_invocations() {
        for cmd in [
            "code .",
            "code",
            "code.cmd .",
            "code.exe .",
            "Code .",
            "CODE.EXE .",
            "code-insiders .",
            "code-insiders.cmd .",
            "code .  --new-window",
            "/usr/local/bin/code .",
            "/usr/local/bin/code-insiders .",
            "C:\\Users\\me\\AppData\\Local\\Programs\\code.cmd .",
            "C:/tools/code/bin/code.cmd .",
            "\"C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd\" .",
            "'C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd' .",
            "env ELECTRON_RUN_AS_NODE=0 code .",
            "FOO=bar code .",
            "env code .",
        ] {
            assert!(looks_like_vscode_command(cmd), "expected match for {cmd:?}");
        }
    }

    #[test]
    fn looks_like_vscode_command_rejects_non_vscode() {
        for cmd in [
            "",
            "   ",
            "vscode .",
            "codium .",
            "vscodium .",
            "cursor .",
            "code-runner",
            "xdg-open .",
            "echo code",
            "pwsh -c code",
            "decode file",
            "encode file",
            "/usr/bin/codium .",
        ] {
            assert!(
                !looks_like_vscode_command(cmd),
                "expected non-match for {cmd:?}"
            );
        }
    }
}
