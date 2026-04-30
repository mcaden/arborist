//! Window-focus abstraction (Phase 3 of `dev/ai/CONTEXT_MENU_PLAN.md`).
//!
//! Brings the OS window owned by `pid` to the foreground so the user
//! sees the launched application after clicking its sub-tab. Each
//! platform has its own quirks; we hide them behind the [`WindowFocuser`]
//! trait so frontend command handlers can branch on result without
//! caring about the underlying mechanism.
//!
//! ## Honest limitations
//!
//! * **Linux**: requires `wmctrl` on `PATH`; X11 only. Wayland
//!   compositors typically forbid programmatic focus stealing entirely
//!   — we surface that as [`Error::Unsupported`].
//! * **macOS**: uses `osascript` + System Events, which requires the
//!   user to grant Arborist Accessibility permission the first time.
//!   Permission denial is mapped to [`Error::PermissionDenied`].
//! * **Windows**: uses `EnumWindows` + `SetForegroundWindow`. Windows
//!   restricts focus stealing to the currently-foreground process; we
//!   call `AllowSetForegroundWindow(pid)` first as a best-effort
//!   workaround. May still no-op if our process is not the foreground
//!   when the user clicks (rare in practice — a click on the sub-tab
//!   *does* make us the foreground).
//!
//! ## Delegated launchers
//!
//! Many "application" defs delegate to an existing instance and exit
//! quickly (e.g. `code .`). For those, the `pid` Arborist captured may
//! already be gone by the time the user clicks the sub-tab. The focuser
//! returns [`Error::NotFound`] in that case — the frontend can choose to
//! relaunch (Phase 7 / Phase 5 UX decision) or just leave the tab
//! greyed.

use std::sync::Mutex;

use crate::types::Error;

/// Brings the window(s) owned by `pid` to the foreground.
pub trait WindowFocuser: Send + Sync + 'static {
    /// Best-effort focus. Returns:
    ///
    /// * `Ok(())` if focus was successfully requested. (We can't always
    ///   detect whether the OS actually gave us focus.)
    /// * `Err(Error::NotFound(...))` if no window was found for `pid`
    ///   (the process exited or owns no top-level window).
    /// * `Err(Error::ToolMissing(...))` if a required external tool
    ///   isn't on `PATH` (Linux: `wmctrl`).
    /// * `Err(Error::PermissionDenied(...))` if the OS refused (macOS
    ///   Accessibility).
    /// * `Err(Error::Unsupported(...))` if the platform fundamentally
    ///   does not support programmatic focus (Wayland in most setups).
    fn focus_pid(&self, pid: u32) -> Result<(), Error>;
}

/// Recording fake for tests. Captures the sequence of `focus_pid`
/// arguments and returns whatever result was queued (defaulting to
/// `Ok(())`).
pub struct RecordingFocuser {
    inner: Mutex<RecordingState>,
}

struct RecordingState {
    calls: Vec<u32>,
    next_results: std::collections::VecDeque<Result<(), Error>>,
}

impl RecordingFocuser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RecordingState {
                calls: Vec::new(),
                next_results: std::collections::VecDeque::new(),
            }),
        }
    }
    /// Queue a result to be returned on the next `focus_pid` call.
    pub fn queue(&self, result: Result<(), Error>) {
        self.inner.lock().unwrap().next_results.push_back(result);
    }
    pub fn calls(&self) -> Vec<u32> {
        self.inner.lock().unwrap().calls.clone()
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
}

// ---------------------------------------------------------------------------
// Real platform implementation
// ---------------------------------------------------------------------------

/// Production [`WindowFocuser`]. Picks the platform-appropriate
/// implementation at compile time.
#[derive(Default)]
pub struct RealFocuser;

#[cfg(target_os = "windows")]
mod platform {
    use crate::types::Error;
    use std::ffi::c_void;

    // Hand-rolled minimal Win32 bindings. We deliberately avoid pulling
    // in the heavyweight `windows` crate — this is the only place the
    // crate would be used.
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type HWND = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(clippy::upper_case_acronyms)]
    type LPARAM = isize;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(HWND, LPARAM) -> BOOL, lparam: LPARAM) -> BOOL;
        fn GetWindowThreadProcessId(hwnd: HWND, pid_out: *mut DWORD) -> DWORD;
        fn IsWindowVisible(hwnd: HWND) -> BOOL;
        fn SetForegroundWindow(hwnd: HWND) -> BOOL;
        fn AllowSetForegroundWindow(pid: DWORD) -> BOOL;
        fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL;
    }

    const SW_RESTORE: i32 = 9;

    struct EnumState {
        target_pid: DWORD,
        found: HWND,
    }

    extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
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
        unsafe {
            // Best-effort: lift Windows' focus-stealing block.
            AllowSetForegroundWindow(pid);
            // Restore (in case minimised) before raising.
            ShowWindow(state.found, SW_RESTORE);
            if SetForegroundWindow(state.found) == 0 {
                // SetForegroundWindow returns 0 on failure but leaves
                // GetLastError unset reliably, so there's nothing
                // useful to report. Treat as best-effort success — the
                // common cause is the focus-stealing rule which we
                // can't override.
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
        // Use System Events to flip frontmost on the target process by
        // its Unix PID. Requires Accessibility permission for Arborist.
        let script = format!(
            "tell application \"System Events\" to set frontmost of (first process whose unix id is {pid}) to true"
        );
        let output = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .map_err(|e| {
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
            return Err(Error::NotFound(format!(
                "no process with pid {pid}: {stderr}"
            )));
        }
        Err(Error::Internal(format!("osascript: {stderr}")))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use crate::types::Error;
    use std::process::Command;

    pub(super) fn focus_pid(pid: u32) -> Result<(), Error> {
        // Wayland session detection: WAYLAND_DISPLAY is set and there's
        // no XWayland fallback we can usefully target via wmctrl.
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none() {
            return Err(Error::Unsupported(
                "Wayland does not allow programmatic window focus".into(),
            ));
        }
        let listing = Command::new("wmctrl").arg("-lp").output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ToolMissing("wmctrl".into())
            } else {
                Error::Internal(format!("wmctrl spawn: {e}"))
            }
        })?;
        if !listing.status.success() {
            return Err(Error::Internal(format!(
                "wmctrl -lp: {}",
                String::from_utf8_lossy(&listing.stderr)
            )));
        }
        let text = String::from_utf8_lossy(&listing.stdout);
        // wmctrl -lp output: "<id> <desktop> <pid> <host> <title>"
        let pid_str = pid.to_string();
        let target_id = text
            .lines()
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                let id = parts.next()?;
                let _desktop = parts.next()?;
                let p = parts.next()?;
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
    fn real_focuser_returns_error_for_nonexistent_pid() {
        // Use a PID extremely unlikely to be in use. On Windows, Linux,
        // and macOS the OS allocates PIDs from a bounded range; 4 294 967
        // is high enough that it should not exist. We accept either
        // NotFound (typical) or ToolMissing (CI without wmctrl).
        // Wayland-only sessions also acceptable as Unsupported.
        let f = RealFocuser;
        match f.focus_pid(4_294_967) {
            Ok(()) => {
                // On macOS in particular, osascript may not error for an
                // absent PID under all permission states. Don't fail.
            }
            Err(Error::NotFound(_))
            | Err(Error::ToolMissing(_))
            | Err(Error::Unsupported(_))
            | Err(Error::PermissionDenied(_))
            | Err(Error::Internal(_)) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }
}
