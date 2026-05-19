//! Boot-time login-shell `PATH` recovery for macOS `.app` launches.
//!
//! `launchd` starts `.app` bundles with a minimal `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin` plus `/etc/paths.d`) and does **not** source any user shell
//! rc files. The PTY pool later spawns each session as `<$SHELL> -c <cmd>` (non-interactive, non-login), so user-installed CLIs in `~/.local/bin`,
//! `~/.npm-global/bin`, Homebrew prefixes, etc. are invisible to the spawned child — `claude: command not found` is the typical user-visible symptom.
//!
//! Fix: at boot we ask the user's login shell what its `PATH` is via `<$SHELL> -ilc 'printf '%s\n%s\n' MARKER "$PATH"'`, parse the line after the
//! marker, and apply it to the host process env so every subsequent PTY child inherits the corrected `PATH`. Same pattern as `npm:fix-path` /
//! VS Code's `resolveShellEnv`.
//!
//! Non-macOS targets get a no-op [`apply_login_path_macos`].
//!
//! Failures (timeout, missing `$SHELL`, parse error, etc.) are logged at `warn!` and leave `PATH` unchanged — the user sees the original broken
//! behavior with a log line to look at, rather than a boot abort.

use std::io;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Sentinel printed before the recovered `PATH` so rc-file noise (oh-my-zsh banners, nvm output) ahead of it can be discarded.
const MARKER: &str = "__ARBORIST_FIXPATH__";

/// Upper bound on the login shell query. Empirically, zsh `-ilc` on macOS lands at 50–500ms; 5s is conservative headroom.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Wake-up interval for the exit-status poll loop. 50ms keeps overhead negligible while bounding kill latency well below human perception.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum LoginPathError {
    #[error("$SHELL is not set and no fallback is available")]
    NoShell,
    #[error("login shell spawn failed: {0}")]
    Spawn(#[source] io::Error),
    #[error("login shell timed out after {0:?}")]
    Timeout(Duration),
    #[error("login shell exited with non-success status: {0}")]
    NonZeroExit(String),
    #[error("login shell stdout was not valid UTF-8")]
    InvalidUtf8,
    #[error("marker {MARKER:?} not found in login shell stdout")]
    MarkerNotFound,
    #[error("no PATH line followed marker in login shell stdout")]
    NoPathAfterMarker,
}

/// Seam over "ask the login shell for its `PATH`". Production uses [`RealLoginShellQuery`]; tests inject a fake that returns canned stdout so they
/// don't have to spawn `/bin/zsh -ilc`.
pub trait LoginShellQuery: Send + Sync {
    /// Return the raw stdout of the login shell after the marker-printing command. Must include the marker line plus the PATH line (or whatever the
    /// shell actually produced — `parse_marker_path` does the validation).
    fn query(&self) -> Result<String, LoginPathError>;
}

/// Production [`LoginShellQuery`] that actually spawns the user's login shell.
pub struct RealLoginShellQuery;

impl LoginShellQuery for RealLoginShellQuery {
    fn query(&self) -> Result<String, LoginPathError> {
        let shell = resolve_shell()?;
        let script = format!(r#"printf '%s\n%s\n' '{MARKER}' "$PATH""#);

        let mut child = Command::new(&shell)
            .args(["-ilc", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(LoginPathError::Spawn)?;

        // Drain stdout and stderr in dedicated threads to avoid pipe-buffer deadlock (chatty rc files writing to stderr would otherwise block the
        // child before it can `printf` its PATH line).
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| LoginPathError::Spawn(io::Error::other("stdout pipe not available")))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| LoginPathError::Spawn(io::Error::other("stderr pipe not available")))?;
        let stdout_thread = thread::Builder::new()
            .name("arborist-login-path-out".into())
            .spawn(move || read_to_end(stdout_pipe))
            .map_err(|e| LoginPathError::Spawn(io::Error::other(format!("stdout thread spawn failed: {e}"))))?;
        let stderr_thread = thread::Builder::new()
            .name("arborist-login-path-err".into())
            .spawn(move || read_to_end(stderr_pipe))
            .map_err(|e| LoginPathError::Spawn(io::Error::other(format!("stderr thread spawn failed: {e}"))))?;

        // Poll for exit; on timeout, kill the child so the reader threads can drain to EOF and join cleanly (no orphaned zsh, no leaked threads).
        let deadline = Instant::now() + QUERY_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_thread.join();
                        let _ = stderr_thread.join();
                        return Err(LoginPathError::Timeout(QUERY_TIMEOUT));
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(LoginPathError::Spawn(e));
                }
            }
        };

        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();
        if !status.success() {
            return Err(LoginPathError::NonZeroExit(String::from_utf8_lossy(&stderr).trim().to_owned()));
        }
        String::from_utf8(stdout).map_err(|_| LoginPathError::InvalidUtf8)
    }
}

fn read_to_end<R: Read>(mut r: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    buf
}

#[cfg(target_os = "macos")]
fn resolve_shell() -> Result<String, LoginPathError> {
    // launchd sets $SHELL from Directory Services, so this is reliable on macOS. Fallback to /bin/zsh (the default user shell on Catalina+).
    Ok(std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned()))
}

#[cfg(not(target_os = "macos"))]
fn resolve_shell() -> Result<String, LoginPathError> {
    std::env::var("SHELL").map_err(|_| LoginPathError::NoShell)
}

/// Run the trait, parse its stdout, and return the recovered `PATH`. Pure over `std::env` — used by [`apply_login_path_macos`] and tests.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn compute<Q: LoginShellQuery + ?Sized>(q: &Q) -> Result<String, LoginPathError> {
    let stdout = q.query()?;
    parse_marker_path(&stdout)
}

/// Apply the login shell's `PATH` to the host process env on macOS. No-op on other targets.
///
/// Idempotent and best-effort: call once at boot, before anything spawns a PTY child or probes `PATH`.
#[cfg(target_os = "macos")]
pub fn apply_login_path_macos() {
    match compute(&RealLoginShellQuery) {
        Ok(p) => {
            tracing::info!(path = %p, "applying login-shell PATH");
            std::env::set_var("PATH", p);
        }
        Err(e) => tracing::warn!(error = %e, "login-shell PATH query failed; keeping inherited PATH"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn apply_login_path_macos() {}

/// Pull the first non-empty line following the marker line out of `stdout`. Everything before the marker is treated as rc-file noise and discarded.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_marker_path(stdout: &str) -> Result<String, LoginPathError> {
    let mut lines = stdout.lines();
    let mut saw_marker = false;
    for line in lines.by_ref() {
        if line.trim() == MARKER {
            saw_marker = true;
            break;
        }
    }
    if !saw_marker {
        return Err(LoginPathError::MarkerNotFound);
    }
    for line in lines {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
    Err(LoginPathError::NoPathAfterMarker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeQuery(Mutex<Option<Result<String, LoginPathError>>>);
    impl FakeQuery {
        fn new(r: Result<String, LoginPathError>) -> Self {
            Self(Mutex::new(Some(r)))
        }
    }
    impl LoginShellQuery for FakeQuery {
        fn query(&self) -> Result<String, LoginPathError> {
            self.0.lock().unwrap().take().expect("FakeQuery::query called more than once")
        }
    }

    #[test]
    fn parse_returns_path_after_marker() {
        let stdout = format!("{MARKER}\n/usr/local/bin:/usr/bin:/bin\n");
        assert_eq!(parse_marker_path(&stdout).unwrap(), "/usr/local/bin:/usr/bin:/bin");
    }

    #[test]
    fn parse_strips_rc_file_noise_before_marker() {
        let stdout = format!("Welcome to oh-my-zsh!\nnvm: loading...\n{MARKER}\n/Users/dev/.nvm/versions/node/v20/bin:/usr/bin:/bin\n");
        assert_eq!(parse_marker_path(&stdout).unwrap(), "/Users/dev/.nvm/versions/node/v20/bin:/usr/bin:/bin");
    }

    #[test]
    fn parse_handles_crlf_line_endings() {
        let stdout = format!("{MARKER}\r\n/usr/local/bin:/usr/bin\r\n");
        assert_eq!(parse_marker_path(&stdout).unwrap(), "/usr/local/bin:/usr/bin");
    }

    #[test]
    fn parse_skips_blank_lines_after_marker() {
        let stdout = format!("{MARKER}\n\n   \n/usr/bin\n");
        assert_eq!(parse_marker_path(&stdout).unwrap(), "/usr/bin");
    }

    #[test]
    fn parse_errors_when_marker_missing() {
        let stdout = "/usr/local/bin:/usr/bin\n";
        assert!(matches!(parse_marker_path(stdout), Err(LoginPathError::MarkerNotFound)));
    }

    #[test]
    fn parse_errors_when_no_line_after_marker() {
        let stdout = format!("noise\n{MARKER}\n");
        assert!(matches!(parse_marker_path(&stdout), Err(LoginPathError::NoPathAfterMarker)));
    }

    #[test]
    fn compute_returns_path() {
        let stdout = format!("{MARKER}\n/opt/bin:/usr/bin\n");
        let q = FakeQuery::new(Ok(stdout));
        assert_eq!(compute(&q).unwrap(), "/opt/bin:/usr/bin");
    }

    #[test]
    fn compute_propagates_marker_missing() {
        let q = FakeQuery::new(Ok("/usr/bin\n".to_owned()));
        assert!(matches!(compute(&q), Err(LoginPathError::MarkerNotFound)));
    }

    #[test]
    fn compute_propagates_query_error() {
        let q = FakeQuery::new(Err(LoginPathError::NoShell));
        assert!(matches!(compute(&q), Err(LoginPathError::NoShell)));
    }
}
