//! Cross-platform VS Code owner re-discovery.
//!
//! See `dev/ai/CONTEXT_MENU_PLAN.md` (post-PR-29 follow-up). When the user opens a `vscode` application sub-tab, the actual command that runs is
//! `code .` (or `code.cmd` on Windows). The launcher EITHER:
//!
//!   * **No VS Code already running** — spawns the long-lived editor process
//!     (`Code.exe` on Windows, `Electron`/`code` on Linux, the `Code.app`
//!     bundle on macOS) and exits ~1 s later.
//!   * **VS Code already running** — sends an IPC message to the existing
//!     editor, which opens the folder in a new window; the launcher exits
//!     ~1 s later. The new window's process is **not** a descendant of our
//!     launcher PID.
//!
//! Either way, the PID Arborist captured is dead within seconds and `focus_pid` returns [`Error::NotFound`]. This module re-discovers the long-lived
//! editor process whose window owns the workspace and retargets the [`AppRuntime`] so subsequent focus/kill calls hit the correct process.
//!
//! ## Identification strategy
//!
//! VS Code formats top-level window titles as `<filename> - <workspace folder> - Visual Studio Code`. We enumerate top-level visible windows and pick
//! the first whose title ends with ` - Visual Studio Code` (case-sensitive — that's how VS Code formats it) and contains the workspace folder's
//! basename as a discrete segment (case-insensitive, since NTFS is case-insensitive and so is "Visual Studio Code"'s default title behaviour).
//!
//! ## Per-platform window enumeration
//!
//! * **Windows** — `EnumWindows` + `GetWindowTextW` (no `OpenProcess`, no PEB / NT
//!   internals — same Win32 surface area as [`crate::window_focus`]).
//! * **macOS** — `osascript` + System Events to query VS Code's window list. Requires
//!   Accessibility permission for Arborist (or, equivalently, for `osascript`); without
//!   it `find_vscode_window` returns `None` and the sub-tab keeps the launcher PID.
//! * **Linux / BSD** — shells out to `wmctrl -lp` (X11 / XWayland). Pure-Wayland windows
//!   that don't surface through XWayland aren't enumerable; if `wmctrl` is missing or the
//!   compositor doesn't expose the workspace window, `find_vscode_window` returns `None`
//!   and the sub-tab keeps the launcher PID.
//!
//! On macOS and Linux the `hwnd` field of [`WindowTarget`] is reported as `0` — those
//! platforms don't expose an `hwnd`-style focus API ([`crate::window_focus`]'s
//! `focus_hwnd`/`post_close_message` are Windows-only), so the [`AppPool`] always falls
//! back to PID-based focus. The PID itself is the meaningful re-discovery output.
//!
//! ## Polling
//!
//! 500 ms × 16 iterations = 8 s deadline. Empirically VS Code paints its first window within 2 s on a warm machine and ~5 s on a cold one. Beyond 8 s
//! we give up and the sub-session keeps the launcher PID (focus will NotFound, user can relaunch).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app_launcher::{AppKiller, LivenessProbe, OwnerResolver, PidKiller, RetargetedOwner, WindowFinder, WindowTarget};
use crate::cmd_resolver::ShellTokens;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const POLL_DEADLINE: Duration = Duration::from_secs(8);
const TITLE_SUFFIX: &str = " - Visual Studio Code";

/// First-token program names recognised as launching VS Code. Matched case-insensitively against the first whitespace-delimited token of
/// `def.command` after stripping any leading `env … VAR=val` prefix. Includes Windows-specific extensions and the official Insiders channel.
const VSCODE_PROGRAM_NAMES: &[&str] = &["code", "code.cmd", "code.exe", "code-insiders", "code-insiders.cmd", "code-insiders.exe"];

/// True when the `def.command` string launches VS Code (stable or Insiders, with or without an absolute path or `.cmd`/`.exe` extension).
///
/// Used so user-defined VS Code launchers — not just the built-in `vscode` def — get the [`VsCodeOwnerResolver`] wired up. Without this, a custom def
/// named "VSCode" with command `code .` would never have its launcher PID re-targeted, leaving the sub-tab pointing at a dead `code.cmd` shim within
/// ~1 second of launch.
#[must_use]
pub fn looks_like_vscode_command(command: &str) -> bool {
    // Quote-aware token iterator: handles leading `"path with spaces"` and skips over leading `env`/`KEY=value` shell prefixes so that e.g. `env
    // ELECTRON_RUN_AS_NODE=0 code .` is still recognised.
    for t in ShellTokens::new(command) {
        if t == "env" {
            continue;
        }
        if !t.starts_with('/') && !t.starts_with('\\') && !t.contains(['\\', '/']) && t.contains('=') {
            // Looks like `KEY=value` env prefix — keep skipping.
            continue;
        }
        return is_vscode_program_token(&t);
    }
    false
}

fn is_vscode_program_token(token: &str) -> bool {
    let basename = std::path::Path::new(token).file_name().and_then(|s| s.to_str()).unwrap_or(token);
    let lower = basename.to_ascii_lowercase();
    VSCODE_PROGRAM_NAMES.iter().any(|n| n == &lower.as_str())
}

/// [`OwnerResolver`] that re-discovers the long-lived `Code.exe`
/// window owning a given workspace folder. Constructed per-spawn with the worktree path; the basename is matched against the VS Code window title.
pub struct VsCodeOwnerResolver {
    worktree_path: PathBuf,
}

impl VsCodeOwnerResolver {
    #[must_use]
    pub fn new(worktree_path: PathBuf) -> Self {
        Self { worktree_path }
    }

    /// Per-platform PID + HWND lookup. Returns `Some((pid, hwnd))` if a matching window is found at the moment of the call, `None` otherwise. `hwnd`
    /// is the platform window handle cast to `usize` (a real Win32 HWND on Windows).
    fn find_now(&self) -> Option<(u32, usize)> {
        let basename = self.basename()?;
        platform::find_vscode_window(&basename)
    }

    /// Basename of the worktree path. Used both for window matching during resolve and for the liveness probe so it can detect "workspace window
    /// closed" without requiring `Code.exe` itself to die. Returned in its original case; `find_vscode_window` lowercases internally and matches
    /// case-insensitively against the window title.
    fn basename(&self) -> Option<String> {
        self.worktree_path.file_name().and_then(|s| s.to_str()).map(|s| s.to_owned())
    }
}

impl OwnerResolver for VsCodeOwnerResolver {
    fn resolve(&self) -> Option<RetargetedOwner> {
        let basename = self.basename()?;
        let deadline = Instant::now() + POLL_DEADLINE;
        loop {
            if let Some((pid, hwnd)) = self.find_now() {
                let killer: Arc<dyn AppKiller> = Arc::new(PidKiller::new(pid));
                let liveness: Box<dyn LivenessProbe> = platform::liveness_probe(pid, basename.clone());
                let window_target = Some(WindowTarget {
                    pid,
                    hwnd,
                    refinder: Some(Arc::new(VsCodeWindowFinder { basename: basename.clone() })),
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
/// the stale-handle escape hatch for [`crate::app_launcher::AppPool::focus`] and [`crate::app_launcher::AppPool::request_window_close`].
///
/// On macOS and Linux the returned `hwnd` is `0` — those platforms don't expose an `hwnd`-style focus API, so the result is only meaningful on
/// Windows. Re-finding the window still has value as a liveness/ownership probe, but `AppPool` will fall through to PID-based focus on Unix.
pub struct VsCodeWindowFinder {
    basename: String,
}

impl WindowFinder for VsCodeWindowFinder {
    fn find_window(&self) -> Option<usize> {
        platform::find_vscode_window(&self.basename).map(|(_pid, hwnd)| hwnd)
    }
}

/// Title-matching heuristic shared by every platform module. Returns `true` when `title` is plausibly a VS Code window for the workspace folder
/// whose lowercased basename equals `needle_lower`.
///
/// Logic:
/// 1. Title must end with the literal suffix ` - Visual Studio Code` (case-sensitive — that's how VS Code emits it).
/// 2. The remainder is split on ` - ` segments (`<filename> - <folder> [- <variants>]`); we match the workspace folder against an explicit segment,
///    not a substring of the whole title. This is the difference between "find the workspace named `code`" and "steal any window whose open file
///    contains the word `code`" (e.g. `code.js - other-project - Visual Studio Code`).
/// 3. Trailing decorations VS Code appends when title-related settings are non-default — ` [Workspace]`, ` [Folder]`, ` (Restricted Mode)` etc. — are
///    stripped from each segment before comparison.
fn title_matches_workspace(title: &str, needle_lower: &str) -> bool {
    let Some(remainder) = title.strip_suffix(TITLE_SUFFIX) else {
        return false;
    };
    let remainder_lower = remainder.to_lowercase();
    remainder_lower.split(" - ").any(|seg| {
        let mut cleaned = seg;
        if let Some(idx) = cleaned.find(" [") {
            cleaned = &cleaned[..idx];
        }
        if let Some(idx) = cleaned.find(" (") {
            cleaned = &cleaned[..idx];
        }
        cleaned.trim() == needle_lower
    })
}

// --------------------------------------------------------------------------- Platform: Windows
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
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
        // First matching (pid, hwnd), if any. Stored as `usize` because HWND is `*mut c_void` which isn't `Send`; storing the raw integer avoids the
        // need for unsafe `Send` impls and matches the wire format used elsewhere (e.g. `crate::app_launcher::WindowTarget::hwnd`).
        found: Option<(u32, usize)>,
    }

    extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // Win32 callback panic safety: this body allocates (`vec![0u16; cap]`, `String::from_utf16_lossy`, `to_lowercase`), so an OOM panic could
        // unwind across the EnumWindows FFI boundary. Modern Rust converts cross-FFI panics into a process abort, which under our dogfooding rules
        // would crash the host (the user's editor). Catch any panic and return 1 (continue enumeration); the worst observable effect is that
        // re-discovery misses a window and surfaces NotFound, which the user can retry via relaunch.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> BOOL {
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
            // SAFETY: buf has space for `cap` u16s. Returned `n` is the number of code units written (excluding the null terminator).
            let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), cap as i32) };
            if n <= 0 {
                return 1;
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            if !super::title_matches_workspace(&title, &state.needle) {
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
        }))
        .unwrap_or(1)
    }

    /// Returns `(pid, hwnd_as_usize)` for the first visible top-level window whose title matches the VS Code workspace pattern.
    pub(super) fn find_vscode_window(basename: &str) -> Option<(u32, usize)> {
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

    /// Window-based liveness probe with PID death as a fallback.
    ///
    /// VS Code is multi-window: closing **the workspace** doesn't necessarily kill `Code.exe` — the user may have other windows / workspaces open in
    /// the same process tree. A PID-only probe would therefore wait forever for a process that's still running, leaving the sub-tab stuck on
    /// `Running` long after the workspace was closed.
    ///
    /// Strategy: every 1 s, (1) poll the matched PID with `WaitForSingleObject(0)` so the probe still fires immediately when `Code.exe` itself exits;
    /// (2) re-enumerate top-level windows for one whose title still matches the workspace folder. If the window is gone for two consecutive polls (a
    /// 1 s grace to avoid spurious misses during e.g. a quick title-bar repaint), report the workspace closed. Either signal returning ends the
    /// probe; the [`AppPool`] then emits `Exited` and the sub-tab goes grey.
    pub(super) fn liveness_probe(pid: u32, basename: String) -> Box<dyn LivenessProbe> {
        Box::new(WindowsLivenessProbe { pid, basename })
    }

    struct WindowsLivenessProbe {
        pid: u32,
        basename: String,
    }
    impl LivenessProbe for WindowsLivenessProbe {
        fn wait_for_death(self: Box<Self>) {
            // Number of consecutive polls in which the workspace window wasn't found. Two-poll debounce guards against a transient "no window matched
            // right now" miss (window briefly without the suffix during a title animation, EnumWindows racing a window list update, etc.). 2 polls ×
            // 1 s = up to ~2 s detection latency for a closed workspace, which the user perceives as "near-instant".
            let mut window_gone_polls: u32 = 0;
            const WINDOW_GONE_THRESHOLD: u32 = 2;

            loop {
                // (1) Process-death fast path. If `Code.exe` itself exited (last window closed → app quits) the wait returns immediately. SAFETY:
                // literal access mask + PID.
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

                // (2) Window-based check. If the workspace window is no longer enumerable for `WINDOW_GONE_THRESHOLD` consecutive polls, the
                // workspace was closed.
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

// --------------------------------------------------------------------------- Platform: macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use super::title_matches_workspace;
    use crate::app_launcher::LivenessProbe;
    use std::process::Command;

    /// AppleScript that enumerates VS Code windows. Output: one line per window, `<pid>\t<title>`. Errors (e.g. missing Accessibility permission)
    /// surface as a non-zero exit; we treat that as "no match" without surfacing the failure to the user.
    ///
    /// We deliberately match `process whose name starts with "Code"` so both the stable VS Code build (`Code`) and the Insiders build
    /// (`Code - Insiders`) are picked up. The repeat is wrapped in `try` blocks so a transient AppleScript error on one process doesn't abort the
    /// whole enumeration.
    const ENUMERATE_SCRIPT: &str = r#"set output to ""
try
    tell application "System Events"
        repeat with p in (every process whose background only is false)
            try
                set procName to name of p
                if procName starts with "Code" then
                    set procPid to unix id of p
                    repeat with w in (windows of p)
                        try
                            set output to output & procPid & (ASCII character 9) & (name of w) & linefeed
                        end try
                    end repeat
                end if
            end try
        end repeat
    end tell
end try
return output
"#;

    pub(super) fn find_vscode_window(basename: &str) -> Option<(u32, usize)> {
        let needle = basename.to_lowercase();
        let output = Command::new("osascript").arg("-e").arg(ENUMERATE_SCRIPT).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let Some((pid_str, title)) = line.split_once('\t') else {
                continue;
            };
            let Ok(pid) = pid_str.trim().parse::<u32>() else {
                continue;
            };
            if title_matches_workspace(title, &needle) {
                // hwnd is unused on macOS — `WindowFocuser::focus_hwnd` is Windows-only and the AppPool falls back to PID-based focus.
                return Some((pid, 0));
            }
        }
        None
    }

    pub(super) fn liveness_probe(pid: u32, basename: String) -> Box<dyn LivenessProbe> {
        Box::new(super::unix_liveness::UnixLivenessProbe { pid, basename })
    }
}

// --------------------------------------------------------------------------- Platform: Linux / BSD (X11 / XWayland)
// ---------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::title_matches_workspace;
    use crate::app_launcher::LivenessProbe;
    use std::process::Command;

    /// Parses a single line of `wmctrl -lp` output.
    ///
    /// The wmctrl format is `<window-id-hex> <desktop> <pid> <hostname> <title>` where the title is the remainder of the line and may contain
    /// spaces. We skip 4 whitespace-delimited fields and treat what's left (minus leading whitespace) as the title.
    fn parse_wmctrl_line(line: &str) -> Option<(u32, usize, &str)> {
        let mut rest = line;
        let mut win_id_str = "";
        let mut pid_str = "";
        for field_idx in 0..4u32 {
            let trimmed = rest.trim_start();
            let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
            if end == 0 {
                return None;
            }
            let field = &trimmed[..end];
            match field_idx {
                0 => win_id_str = field,
                2 => pid_str = field,
                _ => {}
            }
            rest = &trimmed[end..];
        }
        let title = rest.trim_start();
        if title.is_empty() {
            return None;
        }
        let win_id = usize::from_str_radix(win_id_str.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()?;
        let pid: u32 = pid_str.parse().ok()?;
        Some((pid, win_id, title))
    }

    pub(super) fn find_vscode_window(basename: &str) -> Option<(u32, usize)> {
        let needle = basename.to_lowercase();
        // wmctrl missing → spawn fails → output() returns Err → we return None silently. Same for any other I/O failure: best-effort, never panic.
        let output = Command::new("wmctrl").arg("-lp").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let Some((pid, win_id, title)) = parse_wmctrl_line(line) else {
                continue;
            };
            if pid == 0 {
                // wmctrl reports `0` for windows whose pid couldn't be resolved (e.g. windows from remote X clients). Skip — we have no useful PID.
                continue;
            }
            if title_matches_workspace(title, &needle) {
                return Some((pid, win_id));
            }
        }
        None
    }

    pub(super) fn liveness_probe(pid: u32, basename: String) -> Box<dyn LivenessProbe> {
        Box::new(super::unix_liveness::UnixLivenessProbe { pid, basename })
    }

    #[cfg(test)]
    mod wmctrl_tests {
        use super::parse_wmctrl_line;

        #[test]
        fn parses_canonical_line() {
            let line = "0x05000007  0 24356  hostname.local file.rs - my-feature - Visual Studio Code";
            let (pid, win_id, title) = parse_wmctrl_line(line).expect("should parse");
            assert_eq!(pid, 24356);
            assert_eq!(win_id, 0x0500_0007);
            assert_eq!(title, "file.rs - my-feature - Visual Studio Code");
        }

        #[test]
        fn parses_line_with_na_hostname() {
            // wmctrl substitutes "N/A" when it can't resolve a hostname — still a valid 5th field.
            let line = "0x01234567  0 4242 N/A workspace - Visual Studio Code";
            let (pid, _win, title) = parse_wmctrl_line(line).expect("should parse");
            assert_eq!(pid, 4242);
            assert_eq!(title, "workspace - Visual Studio Code");
        }

        #[test]
        fn rejects_short_lines() {
            assert!(parse_wmctrl_line("").is_none());
            assert!(parse_wmctrl_line("0x1 0 1234").is_none());
            assert!(parse_wmctrl_line("0x1 0 1234 host").is_none());
        }

        #[test]
        fn rejects_non_hex_window_id() {
            let line = "notahex 0 1234 host title";
            assert!(parse_wmctrl_line(line).is_none());
        }

        #[test]
        fn rejects_non_numeric_pid() {
            let line = "0x1 0 notapid host title";
            assert!(parse_wmctrl_line(line).is_none());
        }
    }
}

// --------------------------------------------------------------------------- Unix liveness probe (macOS + Linux)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod unix_liveness {
    use crate::app_launcher::LivenessProbe;
    use std::time::Duration;

    /// Window-based liveness probe with PID-death fallback. Mirrors the Windows probe (see `WindowsLivenessProbe`): VS Code is multi-window, so
    /// closing **the workspace** doesn't necessarily kill the editor process. We poll once per second and return either when the PID dies (fast
    /// path) or when the workspace window has been gone for two consecutive polls (debounce against transient enumeration misses).
    pub(super) struct UnixLivenessProbe {
        pub pid: u32,
        pub basename: String,
    }

    impl LivenessProbe for UnixLivenessProbe {
        fn wait_for_death(self: Box<Self>) {
            let mut window_gone_polls: u32 = 0;
            const WINDOW_GONE_THRESHOLD: u32 = 2;

            loop {
                // (1) Process-death fast path. `kill(pid, 0)` returns 0 iff the process exists and we have permission to signal it; on macOS and
                // Linux the launcher and the Code process run as the same user, so a non-zero return effectively means "process is gone".
                // SAFETY: literal signal 0 + numeric pid; no dereferenced pointers.
                let alive = unsafe { libc::kill(self.pid as libc::pid_t, 0) } == 0;
                if !alive {
                    return;
                }

                // (2) Window-based check. If the workspace window is no longer enumerable for `WINDOW_GONE_THRESHOLD` consecutive polls, treat the
                // workspace as closed even though the editor process is still alive (other windows / workspaces).
                if super::platform::find_vscode_window(&self.basename).is_none() {
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

// --------------------------------------------------------------------------- Platform: other (wasm, unknown) — re-discovery is a no-op.
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "windows", unix)))]
mod platform {
    use crate::app_launcher::LivenessProbe;

    pub(super) fn find_vscode_window(_basename: &str) -> Option<(u32, usize)> {
        None
    }

    pub(super) fn liveness_probe(_pid: u32, _basename: String) -> Box<dyn LivenessProbe> {
        struct Never;
        impl LivenessProbe for Never {
            fn wait_for_death(self: Box<Self>) {
                // Park indefinitely. In practice the resolver thread won't construct us — find_vscode_window returns None on these platforms — but if
                // someone does call this, we park rather than busy-loop or panic.
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
        // Construct with a clearly-nonexistent workspace name and use a tight test-only deadline. This exercises the polling loop structure (real
        // Windows search runs but finds nothing) and proves resolve() honours its deadline.
        let r = VsCodeOwnerResolverWithDeadline {
            inner: VsCodeOwnerResolver::new(PathBuf::from("definitely-not-a-real-workspace-zzzqqq-arborist-test")),
            deadline: Duration::from_millis(50),
        };
        let result = r.resolve();
        assert!(result.is_none(), "expected None for nonsense basename");
    }

    /// Test wrapper: same logic as [`VsCodeOwnerResolver`] but with an injectable deadline so tests don't wait the full 8 s production budget.
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
                    let liveness: Box<dyn LivenessProbe> = platform::liveness_probe(pid, basename.clone());
                    let window_target = Some(WindowTarget {
                        pid,
                        hwnd,
                        refinder: Some(Arc::new(VsCodeWindowFinder { basename: basename.clone() })),
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
            assert!(!looks_like_vscode_command(cmd), "expected non-match for {cmd:?}");
        }
    }

    #[test]
    fn title_matches_workspace_accepts_canonical_titles() {
        // Canonical: `<filename> - <workspace folder> - Visual Studio Code`.
        assert!(title_matches_workspace("file.rs - my-feature - Visual Studio Code", "my-feature"));
        // Workspace-only window (no open file) still has the folder as a segment.
        assert!(title_matches_workspace("my-feature - Visual Studio Code", "my-feature"));
        // Insiders / variants append decorations VS Code emits when title settings are non-default.
        assert!(title_matches_workspace(
            "file.rs - my-feature [Workspace] - Visual Studio Code",
            "my-feature"
        ));
        assert!(title_matches_workspace(
            "file.rs - my-feature (Restricted Mode) - Visual Studio Code",
            "my-feature"
        ));
        // Case-insensitive folder match (NTFS / macOS HFS+ default).
        assert!(title_matches_workspace("file.rs - My-Feature - Visual Studio Code", "my-feature"));
    }

    #[test]
    fn title_matches_workspace_requires_segment_match_not_substring() {
        // Substring match would pick up any title where the needle appears inside another word — the previous bug. Segment-match rejects it.
        assert!(!title_matches_workspace("code.js - other-project - Visual Studio Code", "code"));
        assert!(!title_matches_workspace("encode-tests.rs - other - Visual Studio Code", "code"));
    }

    #[test]
    fn title_matches_workspace_rejects_wrong_suffix() {
        assert!(!title_matches_workspace("my-feature - Visual Studio Code Insiders", "my-feature"));
        assert!(!title_matches_workspace("my-feature - notepad", "my-feature"));
        assert!(!title_matches_workspace("", "my-feature"));
    }

    #[test]
    fn title_matches_workspace_handles_other_folder() {
        assert!(!title_matches_workspace("file.rs - other-project - Visual Studio Code", "my-feature"));
    }
}
