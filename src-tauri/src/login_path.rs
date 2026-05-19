//! Boot-time login-shell `PATH` recovery for macOS `.app` launches.
//!
//! `launchd` starts `.app` bundles with a minimal `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin` plus `/etc/paths.d`) and does **not** source any user shell
//! rc files. The PTY pool later spawns each session as `<$SHELL> -c <cmd>` (non-interactive, non-login), so user-installed CLIs in `~/.local/bin`,
//! `~/.npm-global/bin`, Homebrew prefixes, etc. are invisible to the spawned child — `claude: command not found` is the typical user-visible symptom.
//!
//! Fix: at boot we ask the user's login shell what its `PATH` is via `<$SHELL> -ilc 'printf MARKER\n%s\n "$PATH"'`, parse the line after the marker,
//! and apply it to the host process env so every subsequent PTY child inherits the corrected `PATH`. Same pattern as `npm:fix-path` /
//! VS Code's `resolveShellEnv`.
//!
//! Non-macOS targets get a no-op [`apply_login_path_macos`].
//!
//! Failures (timeout, missing `$SHELL`, parse error, etc.) are logged at `warn!` and leave `PATH` unchanged — the user sees the original broken
//! behavior with a log line to look at, rather than a boot abort.

use std::io;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Sentinel line printed before the actual `PATH` so we can ignore arbitrary noise written by the user's rc files (oh-my-zsh banners, nvm output,
/// `echo "welcome"` in `.zshrc`, etc.). Must not collide with anything a real PATH could match.
const MARKER: &str = "__ARBORIST_FIXPATH__";

/// Upper bound on how long we'll wait for the login shell to produce its `PATH`. Login zsh with heavy plugins (oh-my-zsh, p10k instant prompt) routinely
/// takes 200-500ms; pathological setups can hit 2-3s. 5s leaves headroom without freezing the boot UI noticeably.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

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

        let (tx, rx) = mpsc::channel();
        let shell_for_thread = shell.clone();
        thread::Builder::new()
            .name("arborist-login-path".into())
            .spawn(move || {
                let out = Command::new(&shell_for_thread)
                    .args(["-ilc", &script])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output();
                let _ = tx.send(out);
            })
            .map_err(|e| LoginPathError::Spawn(io::Error::other(format!("thread spawn failed: {e}"))))?;

        let output = match rx.recv_timeout(QUERY_TIMEOUT) {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(LoginPathError::Spawn(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(LoginPathError::Timeout(QUERY_TIMEOUT)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(LoginPathError::Spawn(io::Error::other("shell thread disconnected")));
            }
        };

        if !output.status.success() {
            return Err(LoginPathError::NonZeroExit(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
        }
        String::from_utf8(output.stdout).map_err(|_| LoginPathError::InvalidUtf8)
    }
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

/// Compute the `PATH` value to apply, without touching `std::env`. Pure over the trait — used by [`apply_login_path_macos`] and by tests.
///
/// Returns:
/// * `Ok(Some(path))` when the shell returned a non-empty `PATH`.
/// * `Ok(None)` when the shell returned an empty `PATH` (refuse to clobber).
/// * `Err(_)` when the query or parse failed.
pub fn compute<Q: LoginShellQuery + ?Sized>(q: &Q) -> Result<Option<String>, LoginPathError> {
    let stdout = q.query()?;
    let path = parse_marker_path(&stdout)?;
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(path))
    }
}

/// Apply the login shell's `PATH` to the host process env on macOS. No-op everywhere else.
///
/// Idempotent and best-effort: callers should invoke once per process at boot, before anything spawns a PTY child or probes `PATH`.
#[cfg(target_os = "macos")]
pub fn apply_login_path_macos() {
    apply_from(&RealLoginShellQuery);
}

#[cfg(not(target_os = "macos"))]
pub fn apply_login_path_macos() {}

/// Inner step of [`apply_login_path_macos`] split out so tests can drive it without depending on the `cfg`-gated entry point.
#[allow(dead_code)] // Used on macOS and by unit tests; unused on Windows/Linux production builds.
pub(crate) fn apply_from<Q: LoginShellQuery + ?Sized>(q: &Q) {
    match compute(q) {
        Ok(Some(p)) => {
            tracing::info!(path = %p, "applying login-shell PATH");
            std::env::set_var("PATH", p);
        }
        Ok(None) => {
            tracing::warn!("login-shell PATH was empty; keeping inherited PATH");
        }
        Err(e) => {
            tracing::warn!(error = %e, "login-shell PATH query failed; keeping inherited PATH");
        }
    }
}

/// Pull the first non-empty line following the marker line out of `stdout`. Everything before the marker is treated as rc-file noise and discarded.
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

    struct FakeQuery(Result<String, LoginPathError>);
    impl LoginShellQuery for FakeQuery {
        fn query(&self) -> Result<String, LoginPathError> {
            match &self.0 {
                Ok(s) => Ok(s.clone()),
                Err(LoginPathError::NoShell) => Err(LoginPathError::NoShell),
                Err(LoginPathError::Timeout(d)) => Err(LoginPathError::Timeout(*d)),
                Err(LoginPathError::MarkerNotFound) => Err(LoginPathError::MarkerNotFound),
                Err(e) => Err(LoginPathError::NonZeroExit(format!("{e}"))),
            }
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
    fn compute_returns_some_for_non_empty_path() {
        let stdout = format!("{MARKER}\n/opt/bin:/usr/bin\n");
        let q = FakeQuery(Ok(stdout));
        assert_eq!(compute(&q).unwrap(), Some("/opt/bin:/usr/bin".to_owned()));
    }

    #[test]
    fn compute_propagates_marker_missing() {
        let q = FakeQuery(Ok("/usr/bin\n".to_owned()));
        assert!(matches!(compute(&q), Err(LoginPathError::MarkerNotFound)));
    }

    #[test]
    fn compute_propagates_query_error() {
        let q = FakeQuery(Err(LoginPathError::NoShell));
        assert!(matches!(compute(&q), Err(LoginPathError::NoShell)));
    }
}
