//! Windows-only File Explorer owner re-discovery (issue #74).
//!
//! When a user opens an `explorer .` application sub-tab, the actual command that runs is `cmd /c explorer .`. The captured launcher PID dies within
//! ~1 s because `explorer.exe` typically delegates to the existing desktop shell `explorer.exe` and exits immediately. The window the user sees is
//! owned by a long-lived `explorer.exe` process that the launcher never spawned.
//!
//! Without re-discovery, Arborist's sub-tab would jump from `Running` to `Exited` almost immediately, focus would `NotFound` because no window is
//! owned by the dead launcher PID, and a `ForceKill` close intent would (catastrophically) call `TerminateProcess` on the long-lived shell, taking
//! down the user's taskbar / desktop.
//!
//! This module mirrors [`crate::plugins::custom_process::vscode::owner`]:
//!
//! * [`looks_like_explorer_command`] — command-shape detection.
//! * [`ExplorerOwnerResolver`] — re-discovers the explorer.exe window owning
//!   the worktree folder.
//!
//! ## Identification strategy
//!
//! Standard File Explorer windows on Windows have window class `CabinetWClass` (modern shell, single-pane and dual-pane both). Title is typically the
//! folder basename (default Folder Options) or the full path (when "Display the full path in the title bar" is enabled). We enumerate visible
//! top-level windows of class `CabinetWClass` and pick the first whose title equals the worktree basename (case-insensitive — NTFS/Windows are
//! case-insensitive) or ends with the worktree basename as a path segment.
//!
//! ## Killer choice
//!
//! Unlike VS Code, where [`crate::app_launcher::PidKiller`] is safe (each `Code.exe` is per-window-set), `explorer.exe` is the Windows shell host —
//! `TerminateProcess` on it kills the desktop and taskbar. We therefore install [`WindowCloseKiller`] which posts `WM_CLOSE` to the matched HWND and
//! is a no-op if the window is gone. `ForceKill` close intent on an Explorer sub-tab thus closes the window politely; the shell process is never
//! touched.
//!
//! ## Polling
//!
//! Same 500 ms × 16 = 8 s deadline as VS Code. Explorer windows usually paint within ~500 ms; the extra budget covers cold delegation handoffs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app_launcher::{AppKiller, LivenessProbe, OwnerResolver, RetargetedOwner, WindowFinder, WindowTarget};
use crate::cmd_resolver::ShellTokens;
use crate::types::Error;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const POLL_DEADLINE: Duration = Duration::from_secs(8);

/// Window class for modern File Explorer windows.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const EXPLORER_WINDOW_CLASS: &str = "CabinetWClass";

/// First-token program names recognised as launching File Explorer. Matched case-insensitively against the first whitespace-delimited token of
/// `def.command` after stripping any leading `env … VAR=val` prefix.
const EXPLORER_PROGRAM_NAMES: &[&str] = &["explorer", "explorer.exe"];

/// True when the `def.command` string launches Windows File Explorer.
///
/// Used so user-defined Explorer launchers — not just the built-in `explorer .` default — get the [`ExplorerOwnerResolver`] wired up.
#[must_use]
pub fn looks_like_explorer_command(command: &str) -> bool {
    for t in ShellTokens::new(command) {
        if t == "env" {
            continue;
        }
        if !t.starts_with('/') && !t.starts_with('\\') && !t.contains(['\\', '/']) && t.contains('=') {
            // `KEY=value` env prefix — keep skipping.
            continue;
        }
        return is_explorer_program_token(&t);
    }
    false
}

fn is_explorer_program_token(token: &str) -> bool {
    let basename = std::path::Path::new(token).file_name().and_then(|s| s.to_str()).unwrap_or(token);
    let lower = basename.to_ascii_lowercase();
    EXPLORER_PROGRAM_NAMES.iter().any(|n| n == &lower.as_str())
}

/// [`OwnerResolver`] that re-discovers the long-lived `explorer.exe` window opened on a given worktree folder.
pub struct ExplorerOwnerResolver {
    worktree_path: PathBuf,
}

impl ExplorerOwnerResolver {
    #[must_use]
    pub fn new(worktree_path: PathBuf) -> Self {
        Self { worktree_path }
    }

    /// Per-platform PID + HWND lookup. `hwnd` is the platform window handle cast to `usize` (a real Win32 HWND on Windows).
    fn find_now(&self) -> Option<(u32, usize)> {
        let basename = self.basename()?;
        platform::find_explorer_window(&basename)
    }

    /// Basename of the worktree path. Matched case-insensitively against the Explorer window title.
    fn basename(&self) -> Option<String> {
        self.worktree_path.file_name().and_then(|s| s.to_str()).map(|s| s.to_owned())
    }
}

impl OwnerResolver for ExplorerOwnerResolver {
    fn resolve(&self) -> Option<RetargetedOwner> {
        let basename = self.basename()?;
        let deadline = Instant::now() + POLL_DEADLINE;
        loop {
            if let Some((pid, hwnd)) = self.find_now() {
                // CRITICAL: never use PidKiller for explorer.exe — terminating the shell process kills the user's taskbar and desktop. WindowCloseKiller
                // posts WM_CLOSE to the specific HWND so ForceKill closes only that Explorer window.
                let killer: Arc<dyn AppKiller> = Arc::new(WindowCloseKiller::new(hwnd));
                let liveness: Box<dyn LivenessProbe> = platform::liveness_probe(basename.clone());
                let window_target = Some(WindowTarget {
                    pid,
                    hwnd,
                    refinder: Some(Arc::new(ExplorerWindowFinder { basename: basename.clone() })),
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

/// [`WindowFinder`] that re-runs the Explorer title heuristic. On non-Windows platforms returns `None`.
pub struct ExplorerWindowFinder {
    basename: String,
}

impl WindowFinder for ExplorerWindowFinder {
    fn find_window(&self) -> Option<usize> {
        platform::find_explorer_window(&self.basename).map(|(_pid, hwnd)| hwnd)
    }
}

/// [`AppKiller`] that posts `WM_CLOSE` to a specific HWND instead of terminating the owning process. Used for Explorer because the owning
/// `explorer.exe` is the Windows shell host — `TerminateProcess` would kill the taskbar and desktop. Best-effort and idempotent: if the window has
/// already been closed, [`AppKiller::kill`] returns `Ok(())`.
pub struct WindowCloseKiller {
    hwnd: usize,
}

impl WindowCloseKiller {
    #[must_use]
    pub fn new(hwnd: usize) -> Self {
        Self { hwnd }
    }
}

impl AppKiller for WindowCloseKiller {
    fn kill(&self) -> Result<(), Error> {
        platform::post_close(self.hwnd)
    }
}

// --------------------------------------------------------------------------- Platform: Windows
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
    use super::EXPLORER_WINDOW_CLASS;
    use crate::app_launcher::LivenessProbe;
    use crate::types::Error;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type HWND = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(clippy::upper_case_acronyms)]
    type LPARAM = isize;
    #[allow(clippy::upper_case_acronyms)]
    type WPARAM = usize;
    #[allow(clippy::upper_case_acronyms)]
    type UINT = u32;

    const WM_CLOSE: UINT = 0x0010;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(HWND, LPARAM) -> BOOL, lparam: LPARAM) -> BOOL;
        fn GetWindowThreadProcessId(hwnd: HWND, pid_out: *mut DWORD) -> DWORD;
        fn IsWindowVisible(hwnd: HWND) -> BOOL;
        fn IsWindow(hwnd: HWND) -> BOOL;
        fn GetClassNameW(hwnd: HWND, buf: *mut u16, max_count: i32) -> i32;
        fn GetWindowTextLengthW(hwnd: HWND) -> i32;
        fn GetWindowTextW(hwnd: HWND, buf: *mut u16, max_count: i32) -> i32;
        fn PostMessageW(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> BOOL;
    }

    struct EnumState {
        // Lowercased basename to match against (case-insensitive).
        needle: String,
        // First matching (pid, hwnd), if any. Stored as `usize` because HWND isn't `Send`.
        found: Option<(u32, usize)>,
    }

    extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // Win32 callback panic safety: this body allocates (`vec![0u16; …]`, `String::from_utf16_lossy`, `to_lowercase`). An OOM panic unwinding
        // across the EnumWindows FFI boundary aborts the process under modern Rust, which under our dogfooding rules would crash the host editor.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> BOOL {
            let state = unsafe { &mut *(lparam as *mut EnumState) };
            // SAFETY: hwnd comes from EnumWindows and is valid.
            if unsafe { IsWindowVisible(hwnd) } == 0 {
                return 1;
            }

            // Class filter. CabinetWClass is the modern File Explorer file-list window.
            let mut cls_buf: [u16; 64] = [0; 64];
            let cls_len = unsafe { GetClassNameW(hwnd, cls_buf.as_mut_ptr(), cls_buf.len() as i32) };
            if cls_len <= 0 {
                return 1;
            }
            let class_name = String::from_utf16_lossy(&cls_buf[..cls_len as usize]);
            if class_name != EXPLORER_WINDOW_CLASS {
                return 1;
            }

            // Title fetch.
            let len = unsafe { GetWindowTextLengthW(hwnd) };
            if len <= 0 {
                return 1;
            }
            let cap = (len as usize) + 1;
            let mut buf: Vec<u16> = vec![0u16; cap];
            let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), cap as i32) };
            if n <= 0 {
                return 1;
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            let title_lower = title.to_lowercase();
            let needle = state.needle.as_str();

            // Title may be the basename alone (default Folder Options) or a full path / partial path. We accept exact match OR the basename appearing
            // as the final path segment of the title (so `C:\repos\arborist` matches needle `arborist`). Plain substring match is rejected to avoid
            // selecting an unrelated window whose title happens to contain the basename as a sub-segment.
            let matches = if title_lower == needle {
                true
            } else {
                let last_segment = title_lower.rsplit(['\\', '/']).next().unwrap_or(title_lower.as_str());
                last_segment == needle
            };
            if !matches {
                return 1;
            }

            let mut pid: DWORD = 0;
            unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
            if pid != 0 {
                state.found = Some((pid, hwnd as usize));
                return 0; // stop enumeration
            }
            1
        }))
        .unwrap_or(1)
    }

    /// Returns `(pid, hwnd_as_usize)` for the first visible top-level Explorer window whose title matches `basename`.
    pub(super) fn find_explorer_window(basename: &str) -> Option<(u32, usize)> {
        let mut state = EnumState {
            needle: basename.to_lowercase(),
            found: None,
        };
        // SAFETY: enum_proc reads lparam as &mut EnumState; we pass exactly that.
        unsafe {
            EnumWindows(enum_proc, &mut state as *mut _ as LPARAM);
        }
        state.found
    }

    /// Window-only liveness probe.
    ///
    /// `explorer.exe` is the Windows shell host — it (essentially) never exits while the user is logged in. So polling the PID with
    /// `WaitForSingleObject` would block forever even after the user closed the matched Explorer window, leaving the sub-tab stuck on `Running`.
    ///
    /// Strategy: every 1 s, re-enumerate. If no Explorer window matching the workspace basename is visible for two consecutive polls (a 1 s grace
    /// against transient EnumWindows races), report exited.
    pub(super) fn liveness_probe(basename: String) -> Box<dyn LivenessProbe> {
        Box::new(WindowsLivenessProbe { basename })
    }

    struct WindowsLivenessProbe {
        basename: String,
    }
    impl LivenessProbe for WindowsLivenessProbe {
        fn wait_for_death(self: Box<Self>, cancel: &AtomicBool) {
            let mut window_gone_polls: u32 = 0;
            const WINDOW_GONE_THRESHOLD: u32 = 2;
            loop {
                // Cooperative cancellation: the pool sets `killed` (wired through `cancel`) on `detach` / `kill_async`. Polled at the top of every
                // iteration so torn-down runtimes free this OS thread within ~1 s instead of leaking until Explorer itself exits.
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                if find_explorer_window(&self.basename).is_none() {
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

    pub(super) fn post_close(hwnd: usize) -> Result<(), Error> {
        if hwnd == 0 {
            return Ok(());
        }
        let h = hwnd as HWND;
        // SAFETY: PostMessageW is safe to call against any HWND. IsWindow guards against stale handles. We swallow PostMessageW failure as benign:
        // the window is gone, which is the same end state.
        unsafe {
            if IsWindow(h) == 0 {
                return Ok(());
            }
            let _ = PostMessageW(h, WM_CLOSE, 0, 0);
        }
        Ok(())
    }
}

// --------------------------------------------------------------------------- Platform: non-Windows — Explorer is Windows-only.
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
mod platform {
    use crate::app_launcher::LivenessProbe;
    use crate::types::Error;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    pub(super) fn find_explorer_window(_basename: &str) -> Option<(u32, usize)> {
        None
    }

    pub(super) fn liveness_probe(_basename: String) -> Box<dyn LivenessProbe> {
        struct Never;
        impl LivenessProbe for Never {
            fn wait_for_death(self: Box<Self>, cancel: &AtomicBool) {
                // Park with a 1 s timeout so cooperative cancellation (via `cancel`) lands in bounded time. In practice this constructor is unused on
                // non-Windows (Explorer is Windows-only), but the [`LivenessProbe`] cancellation contract still applies.
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::park_timeout(Duration::from_secs(1));
                }
            }
        }
        Box::new(Never)
    }

    pub(super) fn post_close(_hwnd: usize) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn looks_like_explorer_command_matches_canonical_invocations() {
        for cmd in [
            "explorer .",
            "explorer",
            "explorer.exe .",
            "EXPLORER.EXE .",
            "Explorer .",
            "explorer \"C:\\path with spaces\"",
            "'C:\\Windows\\explorer.exe' .",
            "\"C:\\Windows\\explorer.exe\" .",
            "C:\\Windows\\explorer.exe .",
            "C:/Windows/explorer.exe .",
            "env FOO=bar explorer .",
            "FOO=bar explorer .",
        ] {
            assert!(looks_like_explorer_command(cmd), "expected match for {cmd:?}");
        }
    }

    #[test]
    fn looks_like_explorer_command_rejects_non_explorer() {
        for cmd in [
            "",
            "   ",
            "code .",
            "explorerpatcher",
            "myexplorer .",
            "echo explorer",
            "open .",
            "xdg-open .",
            "pwsh -c explorer",
        ] {
            assert!(!looks_like_explorer_command(cmd), "expected non-match for {cmd:?}");
        }
    }

    #[test]
    fn explorer_owner_resolver_for_nonexistent_workspace_returns_none() {
        // Use a tight test-only deadline. On Windows this exercises the polling loop structure (the Win32 search runs and finds nothing); on
        // non-Windows `find_explorer_window` is a stub returning None.
        let r = ExplorerOwnerResolverWithDeadline {
            inner: ExplorerOwnerResolver::new(PathBuf::from("definitely-not-a-real-folder-zzzqqq-arborist-test")),
            deadline: Duration::from_millis(50),
        };
        let result = r.resolve();
        assert!(result.is_none(), "expected None for nonsense basename");
    }

    /// Test wrapper: same logic as [`ExplorerOwnerResolver`] but with an injectable deadline so tests don't wait the full 8 s production budget.
    struct ExplorerOwnerResolverWithDeadline {
        inner: ExplorerOwnerResolver,
        deadline: Duration,
    }

    impl OwnerResolver for ExplorerOwnerResolverWithDeadline {
        fn resolve(&self) -> Option<RetargetedOwner> {
            let basename = self.inner.basename()?;
            let stop_at = Instant::now() + self.deadline;
            loop {
                if let Some((pid, hwnd)) = self.inner.find_now() {
                    let killer: Arc<dyn AppKiller> = Arc::new(WindowCloseKiller::new(hwnd));
                    let liveness: Box<dyn LivenessProbe> = platform::liveness_probe(basename.clone());
                    let window_target = Some(WindowTarget {
                        pid,
                        hwnd,
                        refinder: Some(Arc::new(ExplorerWindowFinder { basename: basename.clone() })),
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
    fn window_close_killer_returns_ok_on_zero_handle() {
        let k = WindowCloseKiller::new(0);
        assert!(k.kill().is_ok());
    }

    #[test]
    fn window_close_killer_returns_ok_on_stale_handle() {
        // 0xDEAD_BEEF is almost certainly not a real HWND. On Windows, IsWindow returns 0 and we early-return Ok. On non-Windows the stub returns Ok
        // unconditionally.
        let k = WindowCloseKiller::new(0xDEAD_BEEF);
        assert!(k.kill().is_ok());
    }
}
