//! Worktree prep commands (issue #63).
//!
//! When a new linked worktree is created via `worktree_create`, we run the
//! user-configured `worktree_prep_commands` once in the new worktree's `cwd`,
//! capture combined stdout+stderr into a per-prep log file under
//! `<app_data_dir>/worktree-prep-logs/<prep-id>.log`, and emit lifecycle
//! events on the `worktree://prep` Tauri channel so the UI can show a banner.
//!
//! Design notes:
//!
//! - We deliberately use `tokio::process::Command` (not `portable-pty`) — preps
//!   are batch shell scripts, not TTY agents. Pipes are sufficient and avoid
//!   spawning a full PTY for `npm install`.
//! - The registry stores `oneshot::Sender<()>` kill handles, **not** the
//!   `Child` — the watcher task owns the child and drives a
//!   `tokio::select! { wait, kill_rx }`. Nothing holds a mutex across `.await`.
//! - The script is composed with `compose::platform_shell()` to reuse the same
//!   per-OS shell program/flag as session command composition. Commands are
//!   joined with ` && ` after blank-line filtering — matching the UI's
//!   `commandsToText` semantics.
//! - Spawn failures (log dir not writeable, child cannot start) do **not** fail
//!   `worktree_create`. They surface to the UI via the same `worktree://prep`
//!   channel: a `Started` event followed immediately by an `Exited` event with
//!   `exit_code = None` and a populated `error_message`.
//! - The worktree path itself is passed as `Command::current_dir`; it is
//!   never interpolated into the joined script.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, Manager};
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::compose;
use crate::types::{AppConfig, WorktreePrepEvent, WorktreePrepId, WorktreePrepInfo};

/// Tauri event channel for prep lifecycle events.
pub const PREP_EVENT: &str = "worktree://prep";

/// Subdirectory under `<app_data_dir>` where per-prep log files are written.
pub const LOG_SUBDIR: &str = "worktree-prep-logs";
const MAX_PREP_LOG_FILES: usize = 500;

/// In-flight prep registry. The watcher task owns each child; the registry only keeps kill handles for shutdown and workspace switches.
#[derive(Default)]
pub struct WorktreePrepRegistry {
    inner: Mutex<HashMap<WorktreePrepId, oneshot::Sender<()>>>,
}

impl WorktreePrepRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_inner(&self) -> MutexGuard<'_, HashMap<WorktreePrepId, oneshot::Sender<()>>> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("worktree prep registry mutex was poisoned; recovering inner state");
                poisoned.into_inner()
            }
        }
    }

    fn insert(&self, id: WorktreePrepId, tx: oneshot::Sender<()>) {
        self.lock_inner().insert(id, tx);
    }

    fn remove(&self, id: &WorktreePrepId) {
        self.lock_inner().remove(id);
    }

    /// Best-effort kill of every in-flight prep. Used on app shutdown.
    pub fn kill_all(&self) {
        let drained: Vec<_> = self.lock_inner().drain().collect();
        for (_, tx) in drained {
            // Send may fail if the watcher already finished; that's fine.
            let _ = tx.send(());
        }
    }
}

/// Strip blank/whitespace-only lines and trim each non-blank command.
///
/// Matches the UI's `commandsToText` cleanup so the user sees `npm install`, not the raw textarea whitespace.
pub(crate) fn clean_commands(cmds: &[String]) -> Vec<String> {
    cmds.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// Kick off configured prep commands in the background and return the [`WorktreePrepInfo`] handle.
///
/// Returns `None` when no commands are configured, so `WorktreeCreateResult.prep` serializes as `null`.
///
/// **Never returns an error**: log-dir or child-spawn failures are reported via `worktree://prep`, and `worktree_create` stays successful.
pub fn maybe_spawn(app: &AppHandle, registry: Arc<WorktreePrepRegistry>, cfg: &AppConfig, worktree_path: &Path) -> Option<WorktreePrepInfo> {
    let cleaned = clean_commands(&cfg.worktree_prep_commands);
    if cleaned.is_empty() {
        return None;
    }

    let prep_id = WorktreePrepId::new_v4();
    let log_path = match log_path_without_creating(app, prep_id) {
        Ok(p) => p,
        Err(err) => {
            let placeholder = temp_fallback_log_path(prep_id);
            emit_synthetic_failure(
                app,
                prep_id,
                worktree_path.to_path_buf(),
                placeholder.clone(),
                &cleaned,
                format!("log_dir: {err}"),
            );
            return Some(WorktreePrepInfo {
                prep_id,
                worktree_path: worktree_path.to_path_buf(),
                log_path: placeholder,
            });
        }
    };
    let info = WorktreePrepInfo {
        prep_id,
        worktree_path: worktree_path.to_path_buf(),
        log_path: log_path.clone(),
    };

    if let Err(err) = ensure_log_dir(&log_path) {
        emit_synthetic_failure(
            app,
            prep_id,
            worktree_path.to_path_buf(),
            log_path.clone(),
            &cleaned,
            format!("log_dir: {err}"),
        );
        return Some(info);
    }

    let script = cleaned.join(" && ");
    let started_at = unix_now();

    // Spawn synchronously so we can report spawn failure immediately and insert the registry handle before any exit event can race it.
    let shell = compose::platform_shell();

    // Open the log file for stdout+stderr; we clone the file handle for stderr.
    let stdout_file = match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f,
        Err(err) => {
            emit_synthetic_failure(
                app,
                prep_id,
                worktree_path.to_path_buf(),
                log_path.clone(),
                &cleaned,
                format!("open log: {err}"),
            );
            return Some(info);
        }
    };
    let stderr_file = match stdout_file.try_clone() {
        Ok(f) => f,
        Err(err) => {
            emit_synthetic_failure(
                app,
                prep_id,
                worktree_path.to_path_buf(),
                log_path.clone(),
                &cleaned,
                format!("dup log fd: {err}"),
            );
            return Some(info);
        }
    };

    // Write a header line so the user can correlate the log to the worktree even if the prep is empty / instant.
    write_log_header(&log_path, worktree_path, &script, started_at);

    let mut cmd = Command::new(&shell.program);
    cmd.arg(shell.flag)
        .arg(&script)
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout_file))
        .stderr(std::process::Stdio::from(stderr_file));

    // Inherit env. We do not set `kill_on_drop(true)` because the watcher owns the Child and drives the explicit cancellation flow.
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            append_log_line(&log_path, &format!("\n[arborist] spawn failed: {err}\n"));
            emit(
                app,
                &WorktreePrepEvent::Started {
                    prep_id,
                    worktree_path: worktree_path.to_path_buf(),
                    log_path: log_path.clone(),
                    command: script.clone(),
                    started_at,
                },
            );
            emit(
                app,
                &WorktreePrepEvent::Exited {
                    prep_id,
                    worktree_path: worktree_path.to_path_buf(),
                    log_path: log_path.clone(),
                    exit_code: None,
                    error_message: Some(format!("spawn: {err}")),
                    started_at,
                    finished_at: unix_now(),
                },
            );
            return Some(info);
        }
    };

    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    registry.insert(prep_id, kill_tx);

    // Emit `Started` *before* spawning the watcher so even an extremely fast prep cannot have its `Exited` arrive before its `Started`.
    emit(
        app,
        &WorktreePrepEvent::Started {
            prep_id,
            worktree_path: worktree_path.to_path_buf(),
            log_path: log_path.clone(),
            command: script.clone(),
            started_at,
        },
    );

    // The watcher owns the Child; on kill_rx it kills then awaits exit so we always emit `Exited` exactly once.
    let app_clone = app.clone();
    let registry_clone = registry.clone();
    let log_path_clone = log_path.clone();
    let worktree_path_clone = worktree_path.to_path_buf();
    tauri::async_runtime::spawn(async move {
        watch_child(
            child,
            kill_rx,
            app_clone,
            registry_clone,
            prep_id,
            worktree_path_clone,
            log_path_clone,
            started_at,
        )
        .await;
    });

    Some(info)
}

#[allow(clippy::too_many_arguments)]
async fn watch_child(
    mut child: tokio::process::Child,
    kill_rx: oneshot::Receiver<()>,
    app: AppHandle,
    registry: Arc<WorktreePrepRegistry>,
    prep_id: WorktreePrepId,
    worktree_path: PathBuf,
    log_path: PathBuf,
    started_at: i64,
) {
    let (exit_code, error_message) = tokio::select! {
        // Natural exit: bubble up its status.
        wait_result = child.wait() => match wait_result {
            Ok(status) => {
                let code = status.code();
                let err = if code.is_none() {
                    Some("terminated by signal".to_owned())
                } else {
                    None
                };
                (code, err)
            }
            Err(err) => (None, Some(format!("wait: {err}"))),
        },
        // Cancellation: kill the child and await its exit so we don't double-emit.
        _ = kill_rx => {
            // start_kill posts SIGKILL / TerminateProcess; subsequent wait drains the exit.
            let _ = child.start_kill();
            let _ = child.wait().await;
            (None, Some("cancelled".to_owned()))
        }
    };

    let finished_at = unix_now();
    append_log_line(&log_path, &format_exit_footer(exit_code, error_message.as_deref(), finished_at));

    registry.remove(&prep_id);

    emit(
        &app,
        &WorktreePrepEvent::Exited {
            prep_id,
            worktree_path,
            log_path,
            exit_code,
            error_message,
            started_at,
            finished_at,
        },
    );
}

fn emit(app: &AppHandle, event: &WorktreePrepEvent) {
    if let Err(err) = app.emit(PREP_EVENT, event) {
        tracing::warn!(%err, "failed to emit worktree prep event");
    }
}

fn emit_synthetic_failure(app: &AppHandle, prep_id: WorktreePrepId, worktree_path: PathBuf, log_path: PathBuf, cleaned: &[String], err: String) {
    let started_at = unix_now();
    let command = cleaned.join(" && ");
    emit(
        app,
        &WorktreePrepEvent::Started {
            prep_id,
            worktree_path: worktree_path.clone(),
            log_path: log_path.clone(),
            command,
            started_at,
        },
    );
    emit(
        app,
        &WorktreePrepEvent::Exited {
            prep_id,
            worktree_path,
            log_path,
            exit_code: None,
            error_message: Some(err),
            started_at,
            finished_at: unix_now(),
        },
    );
}

/// Build the absolute log path under `<app_data_dir>/worktree-prep-logs/<prep-id>.log` and ensure the parent directory exists.
pub fn log_path_for(app: &AppHandle, prep_id: WorktreePrepId) -> std::io::Result<PathBuf> {
    let path = log_path_without_creating(app, prep_id)?;
    ensure_log_dir(&path)?;
    Ok(path)
}

fn log_path_without_creating(app: &AppHandle, prep_id: WorktreePrepId) -> std::io::Result<PathBuf> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| std::io::Error::other(format!("app_data_dir: {e}")))?;
    Ok(log_path_from_base(&base, prep_id))
}

fn log_path_from_base(base: &Path, prep_id: WorktreePrepId) -> PathBuf {
    base.join(LOG_SUBDIR).join(format!("{prep_id}.log"))
}

fn temp_fallback_log_path(prep_id: WorktreePrepId) -> PathBuf {
    std::env::temp_dir().join(LOG_SUBDIR).join(format!("{prep_id}.log"))
}

fn ensure_log_dir(log_path: &Path) -> std::io::Result<()> {
    let dir = log_path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "log path has no parent"))?;
    reject_existing_redirected_log_dir(dir)?;
    std::fs::create_dir_all(dir)?;
    reject_existing_redirected_log_dir(dir)?;
    reject_canonical_log_dir_redirect(dir)?;
    prune_logs_dir_best_effort(dir, MAX_PREP_LOG_FILES);
    Ok(())
}

fn reject_existing_redirected_log_dir(dir: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("worktree-prep logs root is a symlink: {}", dir.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("worktree-prep logs root is not a directory: {}", dir.display()),
        ));
    }
    Ok(())
}

fn reject_canonical_log_dir_redirect(dir: &Path) -> std::io::Result<()> {
    let app_data_dir = dir
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "logs root has no parent"))?;
    let canon_app_data = dunce::canonicalize(app_data_dir)?;
    let canon_dir = dunce::canonicalize(dir)?;
    let expected = canon_app_data.join(LOG_SUBDIR);
    if canon_dir == expected {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("worktree-prep logs root resolves outside app data: {}", canon_dir.display()),
    ))
}

fn prune_logs_dir_best_effort(dir: &Path, keep: usize) {
    let mut files: Vec<(PathBuf, SystemTime)> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_file() {
                    return None;
                }
                let modified = entry.metadata().ok()?.modified().unwrap_or(UNIX_EPOCH);
                Some((path, modified))
            })
            .collect(),
        Err(_) => return,
    };
    if files.len() <= keep {
        return;
    }

    files.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let to_remove = files.len().saturating_sub(keep);
    for (path, _) in files.into_iter().take(to_remove) {
        let _ = std::fs::remove_file(path);
    }
}

fn write_log_header(log_path: &Path, worktree_path: &Path, script: &str, started_at: i64) {
    let header = format!(
        "[arborist] worktree-prep started at {started_at}\n[arborist] worktree: {}\n[arborist] script: {script}\n\n",
        worktree_path.display()
    );
    append_log_line(log_path, &header);
}

fn append_log_line(log_path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn format_exit_footer(exit_code: Option<i32>, error_message: Option<&str>, finished_at: i64) -> String {
    match (exit_code, error_message) {
        (Some(code), _) => format!("\n[arborist] exited with code {code} at {finished_at}\n"),
        (None, Some(err)) => format!("\n[arborist] exited at {finished_at}: {err}\n"),
        (None, None) => format!("\n[arborist] exited at {finished_at}\n"),
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_commands_strips_blank_and_trims() {
        let input = vec![
            "  npm install  ".to_owned(),
            "".to_owned(),
            "   ".to_owned(),
            "cargo build".to_owned(),
            "\n".to_owned(),
        ];
        assert_eq!(clean_commands(&input), vec!["npm install".to_owned(), "cargo build".to_owned()]);
    }

    #[test]
    fn clean_commands_empty_when_only_blanks() {
        let input = vec!["".to_owned(), "  ".to_owned(), "\t".to_owned()];
        assert!(clean_commands(&input).is_empty());
    }

    #[test]
    fn registry_kill_all_drains_handles() {
        let reg = WorktreePrepRegistry::new();
        let id1 = WorktreePrepId::new_v4();
        let id2 = WorktreePrepId::new_v4();
        let (tx1, mut rx1) = oneshot::channel::<()>();
        let (tx2, mut rx2) = oneshot::channel::<()>();
        reg.insert(id1, tx1);
        reg.insert(id2, tx2);
        reg.kill_all();
        // After kill_all, both senders have been dropped/sent; receivers should resolve (Ok if sent, Err if dropped).
        assert!(matches!(rx1.try_recv(), Ok(()) | Err(oneshot::error::TryRecvError::Closed)));
        assert!(matches!(rx2.try_recv(), Ok(()) | Err(oneshot::error::TryRecvError::Closed)));
        // Internal map is empty.
        assert!(reg.inner.lock().unwrap().is_empty());
    }

    #[test]
    fn registry_remove_drops_handle_without_signal() {
        let reg = WorktreePrepRegistry::new();
        let id = WorktreePrepId::new_v4();
        let (tx, mut rx) = oneshot::channel::<()>();
        reg.insert(id, tx);
        reg.remove(&id);
        // Sender dropped without sending → receiver sees Closed.
        assert!(matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
    }

    #[test]
    fn registry_recovers_after_poisoned_lock() {
        let reg = Arc::new(WorktreePrepRegistry::new());
        let poison_reg = Arc::clone(&reg);
        let _ = std::thread::spawn(move || {
            let _guard = poison_reg.inner.lock().unwrap();
            panic!("poison registry lock");
        })
        .join();

        let id = WorktreePrepId::new_v4();
        let (tx, mut rx) = oneshot::channel::<()>();
        reg.insert(id, tx);
        reg.kill_all();

        assert!(matches!(rx.try_recv(), Ok(()) | Err(oneshot::error::TryRecvError::Closed)));
    }

    #[test]
    fn format_exit_footer_with_code() {
        let s = format_exit_footer(Some(0), None, 1700000000);
        assert!(s.contains("exited with code 0"));
        assert!(s.contains("1700000000"));
    }

    #[test]
    fn format_exit_footer_with_error() {
        let s = format_exit_footer(None, Some("cancelled"), 1700000000);
        assert!(s.contains("cancelled"));
        assert!(s.contains("1700000000"));
    }

    #[test]
    fn log_path_from_base_stays_under_logs_root_when_root_cannot_be_created() {
        let temp = tempfile::tempdir().expect("tempdir");
        let prep_id = WorktreePrepId::new_v4();
        let logs_root = temp.path().join(LOG_SUBDIR);
        std::fs::write(&logs_root, b"not a directory").expect("logs root file");

        let log_path = log_path_from_base(temp.path(), prep_id);
        let err = ensure_log_dir(&log_path).expect_err("logs root file should prevent directory creation");

        assert_eq!(log_path, logs_root.join(format!("{prep_id}.log")));
        assert!(log_path.starts_with(&logs_root));
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn reject_canonical_log_dir_redirect_requires_expected_logs_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside dir");

        let err = reject_canonical_log_dir_redirect(&outside).expect_err("redirected logs root must fail");

        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn prune_logs_dir_caps_file_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.log")), b"x").expect("write");
        }

        prune_logs_dir_best_effort(dir.path(), 2);
        let remaining = std::fs::read_dir(dir.path())
            .expect("read")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_file())
            .count();

        assert!(remaining <= 2);
    }
}
