//! Window-focus abstraction (Phase 3 of `dev/ai/CONTEXT_MENU_PLAN.md`).
//!
//! Brings the OS window owned by `pid` to the foreground so the user sees the launched application after clicking its sub-tab. Each platform has its
//! own quirks; we hide them behind the [`WindowFocuser`] trait so frontend command handlers can branch on result without caring about the underlying
//! mechanism.
//!
//! ## Honest limitations
//!
//! * **Linux**: requires `wmctrl` on `PATH`; X11 only. Wayland compositors
//!   typically forbid programmatic focus stealing entirely — we surface that as
//!   [`Error::Unsupported`].
//! * **macOS**: uses `osascript` + System Events, which requires the user to
//!   grant Arborist Accessibility permission the first time. Permission denial
//!   is mapped to [`Error::PermissionDenied`].
//! * **Windows**: uses `EnumWindows` + `SetForegroundWindow`. Windows restricts
//!   focus stealing to the currently-foreground process; we call
//!   `AllowSetForegroundWindow(pid)` first as a best-effort workaround. May
//!   still no-op if our process is not the foreground when the user clicks
//!   (rare in practice — a click on the sub-tab *does* make us the foreground).
//!
//! ## Delegated launchers
//!
//! Many "application" defs delegate to an existing instance and exit quickly (e.g. `code .`). For those, the `pid` Arborist captured may already be
//! gone by the time the user clicks the sub-tab. The focuser returns [`Error::NotFound`] in that case — the frontend can choose to relaunch (Phase 7
//! / Phase 5 UX decision) or just leave the tab greyed.

use std::sync::Mutex;

use crate::types::Error;

/// Brings the window(s) owned by `pid` to the foreground.
pub trait WindowFocuser: Send + Sync + 'static {
    /// Best-effort focus. Returns:
    ///
    /// * `Ok(())` if focus was successfully requested. (We can't always detect
    ///   whether the OS actually gave us focus.)
    /// * `Err(Error::NotFound(...))` if no window was found for `pid` (the
    ///   process exited or owns no top-level window).
    /// * `Err(Error::ToolMissing(...))` if a required external tool isn't on
    ///   `PATH` (Linux: `wmctrl`).
    /// * `Err(Error::PermissionDenied(...))` if the OS refused (macOS
    ///   Accessibility).
    /// * `Err(Error::Unsupported(...))` if the platform fundamentally does not
    ///   support programmatic focus (Wayland in most setups).
    fn focus_pid(&self, pid: u32) -> Result<(), Error>;

    /// Best-effort focus on a specific OS window handle (HWND on Windows, cast to `usize`). Used when the caller has already identified the exact
    /// window the user expects (e.g. one specific VS Code workspace), to avoid the ambiguity of "first visible window owned by this PID".
    ///
    /// Default impl returns [`Error::Unsupported`] so platforms that don't expose a stable handle concept (or `WindowFocuser` implementations that
    /// don't care) can opt out.
    ///
    /// Returns [`Error::NotFound`] when the handle is no longer valid (window destroyed); the caller is expected to fall back to a re-find or to
    /// [`focus_pid`] on the same runtime.
    fn focus_hwnd(&self, _hwnd: usize) -> Result<(), Error> {
        Err(Error::Unsupported("focus_hwnd not implemented for this platform".into()))
    }

    /// Asks the OS to politely close a specific window (Windows: `PostMessageW(hwnd, WM_CLOSE, 0, 0)`). The target app may prompt the user (e.g.
    /// unsaved changes) before actually closing — that's intentional. Returns immediately; whether the window actually goes away is up to the app.
    ///
    /// Default impl returns [`Error::Unsupported`].
    fn post_close_message(&self, _hwnd: usize) -> Result<(), Error> {
        Err(Error::Unsupported("post_close_message not implemented for this platform".into()))
    }
}

/// Recording fake for tests. Captures the sequence of `focus_pid` arguments and returns whatever result was queued (defaulting to `Ok(())`).
pub struct RecordingFocuser {
    inner: Mutex<RecordingState>,
}

struct RecordingState {
    calls: Vec<u32>,
    next_results: std::collections::VecDeque<Result<(), Error>>,
    hwnd_calls: Vec<usize>,
    next_hwnd_results: std::collections::VecDeque<Result<(), Error>>,
    close_calls: Vec<usize>,
    next_close_results: std::collections::VecDeque<Result<(), Error>>,
}

impl RecordingFocuser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RecordingState {
                calls: Vec::new(),
                next_results: std::collections::VecDeque::new(),
                hwnd_calls: Vec::new(),
                next_hwnd_results: std::collections::VecDeque::new(),
                close_calls: Vec::new(),
                next_close_results: std::collections::VecDeque::new(),
            }),
        }
    }
    /// Queue a result to be returned on the next `focus_pid` call.
    pub fn queue(&self, result: Result<(), Error>) {
        self.inner.lock().unwrap().next_results.push_back(result);
    }
    /// Queue a result for the next `focus_hwnd` call.
    pub fn queue_hwnd(&self, result: Result<(), Error>) {
        self.inner.lock().unwrap().next_hwnd_results.push_back(result);
    }
    /// Queue a result for the next `post_close_message` call.
    pub fn queue_close(&self, result: Result<(), Error>) {
        self.inner.lock().unwrap().next_close_results.push_back(result);
    }
    pub fn calls(&self) -> Vec<u32> {
        self.inner.lock().unwrap().calls.clone()
    }
    pub fn hwnd_calls(&self) -> Vec<usize> {
        self.inner.lock().unwrap().hwnd_calls.clone()
    }
    pub fn close_calls(&self) -> Vec<usize> {
        self.inner.lock().unwrap().close_calls.clone()
    }
}

impl Default for RecordingFocuser {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowFocuser for RecordingFocuser {
    fn focus_pid(&self, pid: u32) -> Result<(), Error> {
        let mut g = self.inner.lock().unwrap();
        g.calls.push(pid);
        g.next_results.pop_front().unwrap_or(Ok(()))
    }
    fn focus_hwnd(&self, hwnd: usize) -> Result<(), Error> {
        let mut g = self.inner.lock().unwrap();
        g.hwnd_calls.push(hwnd);
        g.next_hwnd_results.pop_front().unwrap_or(Ok(()))
    }
    fn post_close_message(&self, hwnd: usize) -> Result<(), Error> {
        let mut g = self.inner.lock().unwrap();
        g.close_calls.push(hwnd);
        g.next_close_results.pop_front().unwrap_or(Ok(()))
    }
}

// --------------------------------------------------------------------------- Real platform implementation
// ---------------------------------------------------------------------------

/// Production [`WindowFocuser`]. Picks the platform-appropriate implementation at compile time.
#[derive(Default)]
pub struct RealFocuser;

#[cfg(target_os = "windows")]
mod platform {
    use crate::types::Error;
    use std::ffi::c_void;

    // Hand-rolled minimal Win32 bindings. We deliberately avoid pulling in the heavyweight `windows` crate — this is the only place the crate would
    // be used.
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

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(HWND, LPARAM) -> BOOL, lparam: LPARAM) -> BOOL;
        fn GetWindowThreadProcessId(hwnd: HWND, pid_out: *mut DWORD) -> DWORD;
        fn IsWindowVisible(hwnd: HWND) -> BOOL;
        fn IsWindow(hwnd: HWND) -> BOOL;
        fn IsIconic(hwnd: HWND) -> BOOL;
        fn SetForegroundWindow(hwnd: HWND) -> BOOL;
        fn BringWindowToTop(hwnd: HWND) -> BOOL;
        fn SwitchToThisWindow(hwnd: HWND, alt_tab: BOOL);
        fn AttachThreadInput(id_attach: DWORD, id_attach_to: DWORD, attach: BOOL) -> BOOL;
        fn GetForegroundWindow() -> HWND;
        fn AllowSetForegroundWindow(pid: DWORD) -> BOOL;
        fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL;
        fn PostMessageW(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> BOOL;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> DWORD;
    }

    const SW_RESTORE: i32 = 9;
    const SW_SHOW: i32 = 5;
    const WM_CLOSE: UINT = 0x0010;

    struct EnumState {
        target_pid: DWORD,
        found: HWND,
    }

    extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // Win32 callback panic safety: this body has no allocations today, but the same defensive guard as
        // `plugins::custom_process::vscode::owner::platform::enum_proc` applies — any future refactor that introduces a fallible Rust operation
        // would risk unwinding across the EnumWindows FFI boundary, which Rust converts to a process abort and crashes the host (the user's editor
        // under our dogfooding rules).
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> BOOL {
            // SAFETY: lparam was set by us to a `&mut EnumState`.
            let state = unsafe { &mut *(lparam as *mut EnumState) };
            let mut pid: DWORD = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid == state.target_pid && IsWindowVisible(hwnd) != 0 {
                    state.found = hwnd;
                    return 0; // stop enumeration
                }
            }
            1 // continue
        }))
        .unwrap_or(1)
    }

    pub(super) fn focus_pid(pid: u32) -> Result<(), Error> {
        let mut state = EnumState {
            target_pid: pid,
            found: std::ptr::null_mut(),
        };
        unsafe {
            EnumWindows(enum_proc, &mut state as *mut _ as LPARAM);
        }
        if state.found.is_null() {
            return Err(Error::NotFound(format!("no visible window for pid {pid}")));
        }
        focus_hwnd_raw(state.found, Some(pid))
    }

    /// Brings a specific HWND to the foreground without re-running `EnumWindows`. `pid` is optional; when present we feed it to
    /// `AllowSetForegroundWindow` to lift Windows' focus-stealing block.
    ///
    /// ## Why this is more than `SetForegroundWindow`
    ///
    /// Windows' focus-stealing prevention can silently no-op `SetForegroundWindow` (the taskbar button flashes instead of the window coming forward).
    /// The bare-minimum invocation — `ShowWindow(SW_RESTORE)` + `SetForegroundWindow` — works only when the target was minimised; for a window that's
    /// merely behind ours in z-order, the foreground call is rejected and nothing visible happens. This was reported as "clicking VS Code doesn't
    /// focus the window".
    ///
    /// The reliable Win32 idiom (used by AutoHotkey, the Win32 "ForceForegroundWindow" cookbook, etc.) is the **AttachThreadInput trick**:
    /// temporarily attach our input queue to the current foreground window's thread input queue. While attached, Windows treats us as part of the
    /// foreground process for focus-rule purposes, so `SetForegroundWindow` succeeds even when the standalone call would not. We always detach again
    /// on exit, regardless of intermediate failures.
    ///
    /// We also call `BringWindowToTop` (z-order) and `SwitchToThisWindow` (legacy Alt+Tab activator) as belt-and-suspenders for cases where
    /// individual calls are no-ops. The combined sequence is the most reliable cross-Windows-version recipe; spurious extra calls are cheap and
    /// side-effect-free.
    fn focus_hwnd_raw(hwnd: HWND, pid: Option<u32>) -> Result<(), Error> {
        unsafe {
            if IsWindow(hwnd) == 0 {
                return Err(Error::NotFound("window handle is no longer valid".into()));
            }

            if let Some(p) = pid {
                AllowSetForegroundWindow(p);
            }

            // Restore if minimised; otherwise just ensure shown so a hidden-but-not-iconic window comes back too.
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }

            // AttachThreadInput trick. Capture the foreground thread (may be us, may be a different process), our thread, and the target thread.
            // Attaching is a best-effort op — if it fails (returns 0) we still try the rest, since on some setups SetForegroundWindow succeeds
            // without the trick.
            let foreground_hwnd = GetForegroundWindow();
            let foreground_tid = if foreground_hwnd.is_null() {
                0
            } else {
                GetWindowThreadProcessId(foreground_hwnd, std::ptr::null_mut())
            };
            let target_tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
            let current_tid = GetCurrentThreadId();
            let _ = target_tid; // currently unused — we attach to the foreground thread, which is sufficient

            let attached_fg = foreground_tid != 0 && foreground_tid != current_tid && AttachThreadInput(current_tid, foreground_tid, 1) != 0;

            BringWindowToTop(hwnd);
            // SetForegroundWindow returns 0 on failure but doesn't set GetLastError reliably; treat as best-effort.
            SetForegroundWindow(hwnd);
            // SwitchToThisWindow with TRUE (alt-tab semantics) activates even when SetForegroundWindow's z-order/focus path is partially blocked.
            SwitchToThisWindow(hwnd, 1);

            if attached_fg {
                AttachThreadInput(current_tid, foreground_tid, 0);
            }
        }
        Ok(())
    }

    pub(super) fn focus_hwnd(hwnd: usize) -> Result<(), Error> {
        if hwnd == 0 {
            return Err(Error::NotFound("null window handle".into()));
        }
        // Look up the owning PID so we can allow-set-foreground on the right process. If the lookup fails the handle is stale.
        let h = hwnd as HWND;
        let pid = unsafe {
            if IsWindow(h) == 0 {
                return Err(Error::NotFound("window handle is no longer valid".into()));
            }
            let mut p: DWORD = 0;
            GetWindowThreadProcessId(h, &mut p);
            if p == 0 {
                None
            } else {
                Some(p)
            }
        };
        focus_hwnd_raw(h, pid)
    }

    pub(super) fn post_close_message(hwnd: usize) -> Result<(), Error> {
        if hwnd == 0 {
            return Err(Error::NotFound("null window handle".into()));
        }
        let h = hwnd as HWND;
        // SAFETY: PostMessageW is safe to call against any HWND value; it returns 0 (false) without side-effects when the handle isn't a real window.
        unsafe {
            if IsWindow(h) == 0 {
                return Err(Error::NotFound("window handle is no longer valid".into()));
            }
            if PostMessageW(h, WM_CLOSE, 0, 0) == 0 {
                return Err(Error::Internal("PostMessageW(WM_CLOSE) returned 0".into()));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use crate::types::Error;
    use std::process::Command;

    pub(super) fn focus_pid(pid: u32) -> Result<(), Error> {
        // Use System Events to flip frontmost on the target process by its Unix PID. Requires Accessibility permission for Arborist.
        //
        // Safe-by-typing: `pid` is `u32`, so it can only stringify as ASCII digits — there is no character `format!` could emit here that AppleScript
        // or any downstream shell would parse as a metacharacter. Defense-in-depth note for future readers: if this signature ever widens (e.g. a
        // label or process name), route the value through a separate `-e "set p to <…>"` line and reference it by variable instead of interpolating.
        let script = format!("tell application \"System Events\" to set frontmost of (first process whose unix id is {pid}) to true");
        let output = Command::new("osascript").arg("-e").arg(&script).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ToolMissing("osascript".into())
            } else {
                Error::Internal(format!("osascript spawn: {e}"))
            }
        })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        // Common cases:
        //   -1743 — accessibility permission not granted
        //   -1728 — process not found
        if stderr.contains("-1743") || stderr.to_lowercase().contains("not allowed") {
            return Err(Error::PermissionDenied(format!(
                "macOS Accessibility permission required to focus other apps: {stderr}"
            )));
        }
        if stderr.contains("-1728") || stderr.to_lowercase().contains("can\u{2019}t get") {
            return Err(Error::NotFound(format!("no process with pid {pid}: {stderr}")));
        }
        Err(Error::Internal(format!("osascript: {stderr}")))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use crate::types::Error;
    use std::process::Command;

    pub(super) fn focus_pid(pid: u32) -> Result<(), Error> {
        // Wayland session detection: WAYLAND_DISPLAY is set and there's no XWayland fallback we can usefully target via wmctrl.
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none() {
            return Err(Error::Unsupported("Wayland does not allow programmatic window focus".into()));
        }
        let listing = Command::new("wmctrl").arg("-lp").output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ToolMissing("wmctrl".into())
            } else {
                Error::Internal(format!("wmctrl spawn: {e}"))
            }
        })?;
        if !listing.status.success() {
            return Err(Error::Internal(format!("wmctrl -lp: {}", String::from_utf8_lossy(&listing.stderr))));
        }
        let text = String::from_utf8_lossy(&listing.stdout);
        // wmctrl -lp output: "<id> <desktop> <pid> <host> <title>". Anchor each line strictly: id must start with `0x` followed by hex; desktop must
        // be a decimal integer; pid must be a decimal integer. This rejects forged second lines that an attacker could splice in via a window title
        // containing an embedded newline (X11 lets clients set arbitrary _NET_WM_NAME strings, so the wmctrl output is not a trustworthy line-
        // delimited table without explicit anchoring).
        let pid_str = pid.to_string();
        let target_id = text
            .lines()
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                let id = parts.next()?;
                if !id.starts_with("0x") || id.len() < 3 || !id[2..].chars().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                let desktop = parts.next()?;
                if !desktop.chars().all(|c| c.is_ascii_digit() || c == '-') {
                    return None;
                }
                let p = parts.next()?;
                if !p.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                if p == pid_str {
                    Some(id.to_owned())
                } else {
                    None
                }
            })
            .ok_or_else(|| Error::NotFound(format!("no window for pid {pid}")))?;
        let activate = Command::new("wmctrl")
            .arg("-ia")
            .arg(&target_id)
            .output()
            .map_err(|e| Error::Internal(format!("wmctrl -ia: {e}")))?;
        if !activate.status.success() {
            return Err(Error::Internal(format!(
                "wmctrl -ia {target_id}: {}",
                String::from_utf8_lossy(&activate.stderr)
            )));
        }
        Ok(())
    }
}

impl WindowFocuser for RealFocuser {
    fn focus_pid(&self, pid: u32) -> Result<(), Error> {
        platform::focus_pid(pid)
    }

    #[cfg(target_os = "windows")]
    fn focus_hwnd(&self, hwnd: usize) -> Result<(), Error> {
        platform::focus_hwnd(hwnd)
    }

    #[cfg(target_os = "windows")]
    fn post_close_message(&self, hwnd: usize) -> Result<(), Error> {
        platform::post_close_message(hwnd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_focuser_records_calls_and_returns_queued_results() {
        let f = RecordingFocuser::new();
        f.queue(Ok(()));
        f.queue(Err(Error::NotFound("x".into())));
        assert!(f.focus_pid(101).is_ok());
        assert!(matches!(f.focus_pid(202), Err(Error::NotFound(_))));
        assert!(f.focus_pid(303).is_ok()); // default Ok when queue empty
        assert_eq!(f.calls(), vec![101, 202, 303]);
    }

    #[test]
    fn recording_focuser_records_hwnd_and_close_calls() {
        let f = RecordingFocuser::new();
        f.queue_hwnd(Err(Error::NotFound("stale".into())));
        f.queue_close(Ok(()));
        assert!(matches!(f.focus_hwnd(0xABCD), Err(Error::NotFound(_))));
        assert!(f.focus_hwnd(0x1234).is_ok());
        assert!(f.post_close_message(0xCAFE).is_ok());
        assert_eq!(f.hwnd_calls(), vec![0xABCD, 0x1234]);
        assert_eq!(f.close_calls(), vec![0xCAFE]);
    }

    #[test]
    fn focus_hwnd_default_is_unsupported() {
        struct OnlyPid;
        impl WindowFocuser for OnlyPid {
            fn focus_pid(&self, _pid: u32) -> Result<(), Error> {
                Ok(())
            }
        }
        let f = OnlyPid;
        assert!(matches!(f.focus_hwnd(0x1234), Err(Error::Unsupported(_))));
        assert!(matches!(f.post_close_message(0x1234), Err(Error::Unsupported(_))));
    }

    #[test]
    fn real_focuser_returns_error_for_nonexistent_pid() {
        // Use a PID extremely unlikely to be in use. On Windows, Linux, and macOS the OS allocates PIDs from a bounded range; 4 294 967 is high
        // enough that it should not exist. We accept either NotFound (typical) or ToolMissing (CI without wmctrl). Wayland-only sessions also
        // acceptable as Unsupported.
        let f = RealFocuser;
        match f.focus_pid(4_294_967) {
            Ok(()) => {
                // On macOS in particular, osascript may not error for an absent PID under all permission states. Don't fail.
            }
            Err(Error::NotFound(_))
            | Err(Error::ToolMissing(_))
            | Err(Error::Unsupported(_))
            | Err(Error::PermissionDenied(_))
            | Err(Error::Internal(_)) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn real_focuser_focus_hwnd_rejects_null_and_invalid_handles() {
        let f = RealFocuser;
        assert!(matches!(f.focus_hwnd(0), Err(Error::NotFound(_))));
        // 0xDEADBEEF is virtually certain not to be a real HWND.
        assert!(matches!(f.focus_hwnd(0xDEAD_BEEF), Err(Error::NotFound(_))));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn real_focuser_post_close_rejects_null_and_invalid_handles() {
        let f = RealFocuser;
        assert!(matches!(f.post_close_message(0), Err(Error::NotFound(_))));
        assert!(matches!(f.post_close_message(0xDEAD_BEEF), Err(Error::NotFound(_))));
    }
}
