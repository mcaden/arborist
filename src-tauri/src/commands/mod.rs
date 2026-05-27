//! Tauri command handlers.
//!
//! Phase 3 introduced `ping`; Phase 4 added `config_get`/`config_set`; Phase 7 adds the full session lifecycle plus the
//! `frontend_ready` gate. The actual business logic for session commands lives in the [`session`] submodule as `*_impl` free functions taking
//! [`session::AppContext`]; the `#[tauri::command]` wrappers below are
//! intentionally thin so that integration tests can drive the same code paths directly.
//!
//! ## Capability model (Tauri v2)
//!
//! In Tauri v2, application-defined commands are gated by capability declarations the same way plugin commands are. Each command needs a permission
//! file under `src-tauri/permissions/` referenced from `src-tauri/capabilities/main.json`. Adding a new command without the matching permission entry
//! will cause the `invoke()` call to be rejected at runtime with no compile-time warning.

pub mod session;
pub mod subsession;
pub mod worktree_tab;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tauri::{Emitter, Manager};

use crate::config_store::ConfigStore;
use crate::sub_sessions::SubAppContext;
use crate::types::{
    AppConfig, AppError, PartialAppConfig, RepoCommandTrustArgs, SessionCloseArgs, SessionCloseResult, SessionCreateArgs, SessionId, SessionIdArg,
    SessionInputArgs, SessionOutputEvent, SessionResizeArgs, SessionRestartArgs, SessionStatus, SessionStatusEvent, SessionView, ShellCommandPreview,
    ShellCommandPreviewArgs, SubSession, SubSessionCloseArgs, SubSessionCreateArgs, SubSessionIdArg, SubSessionInputArgs, SubSessionListArgs,
    SubSessionResizeArgs, WorkspaceSwitchArgs, WorkspaceSwitchResult, WorkspaceValidateArgs, WorkspaceValidateResult, WorktreeCreateArgs,
    WorktreeCreateResult, WorktreeTab, WorktreeTabCloseArgs, WorktreeTabCloseResult, WorktreeTabFocusArgs, WorktreeTabOpenArgs,
    WorktreeTabReorderArgs, WorktreeTabSetActiveChildArgs,
};
use crate::workspace_scope::WorkspaceScope;

pub use session::AppContext;

/// Smoke-test command used to verify the Tauri command/event scaffold is wired correctly. Always returns `Ok("pong")`.
#[tauri::command]
pub async fn ping() -> Result<String, AppError> {
    Ok("pong".to_owned())
}

/// Resolve the [`ConfigStore`] for the current Tauri app instance.
///
/// **Legacy, no longer used in production boot.** Phase 6 replaced the boot path with [`crate::boot::bind_workspace`], which opens a per-(branch,
/// workspace) store under the OS lock. This helper remains as a diagnostic / fallback that opens a `ConfigStore` rooted at the legacy
/// `<app_data_dir>` (no isolation, no lock). Do not use from new code — call `AppContext::store()` instead.
pub fn store_for(app: &tauri::AppHandle) -> Result<ConfigStore, AppError> {
    let dir: PathBuf = app.path().app_data_dir().map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    ConfigStore::open(dir).map_err(AppError::from)
}

/// Returns the persisted [`AppConfig`], or a default config with `workspaceRoot: null` when unbound (no workspace selected yet).
#[tauri::command]
pub async fn config_get(app: tauri::AppHandle) -> Result<AppConfig, AppError> {
    let ctx = ctx_of(&app)?;
    let ws = ctx.workspace.read().expect("workspace lock poisoned");
    if ws.is_unbound() {
        return Ok(AppConfig::default());
    }
    let store = ws.store.clone().expect("bound scope always has a store");
    drop(ws);
    Ok(store.load_config())
}

/// Deep-merges `partial` into the persisted [`AppConfig`] and returns the resulting config so the frontend can replace its in-memory snapshot in a
/// single round trip. Returning the merged config (vs. `()`) is load-bearing for backend-derived fields like `icon_data_uri`: the frontend never
/// sends them, but the backfill pass below populates them under the same write lock — without the returned value the user would have to restart the
/// app to see freshly-resolved icons.
///
/// Refused while a workspace switch is in progress — the swap relies on no new writes landing in the *old* store between the `switch_pending` bump
/// and the actual `WorkspaceScope` swap. See
/// [`session::acquire_switch_read`] for the full barrier protocol.
#[tauri::command]
pub async fn config_set(app: tauri::AppHandle, partial: PartialAppConfig) -> Result<AppConfig, AppError> {
    let ctx = ctx_of(&app)?;
    // Workspace-switch barrier: refuse new writes against the old store while a swap is queued. The read guard is held across `save_config_with` so
    // the switch's `write().await` waits for our persist + icon backfill to commit before swapping the `WorkspaceScope`.
    let _switch = session::acquire_switch_read(&ctx)?;
    // Run the user's patch and the icon backfill *under the same store-internal write lock* so two concurrent `config_set` calls can't lose each
    // other's updates. `save_config_with` holds the lock across load → merge → mutate → write.
    let icon_cache = sub_ctx_of(&app).ok().map(|c| c.icon_cache.clone());
    let merged = ctx
        .store()
        .save_config_with(partial, |cfg| {
            // Best-effort: walk every command string and resolve a cached icon data URI. Failures are swallowed — the user's patch is what matters
            // here, the icon is a cosmetic enhancement.
            let Some(cache) = &icon_cache else {
                return false;
            };
            let cwd = backfill_cwd(cfg);
            crate::icon_backfill::backfill_icons(cfg, cache, &cwd)
        })
        .map_err(AppError::from)?;
    Ok(merged)
}

#[tauri::command]
pub async fn shell_command_preview(app: tauri::AppHandle, args: ShellCommandPreviewArgs) -> Result<ShellCommandPreview, AppError> {
    let ctx = ctx_of(&app)?;
    session::shell_command_preview_impl(&ctx, args)
}

#[tauri::command]
pub async fn repo_command_trust(app: tauri::AppHandle, args: RepoCommandTrustArgs) -> Result<AppConfig, AppError> {
    let ctx = ctx_of(&app)?;
    session::repo_command_trust_impl(&ctx, args)
}

#[tauri::command]
pub async fn repo_command_allow_once(app: tauri::AppHandle, args: RepoCommandTrustArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::repo_command_allow_once_impl(&ctx, args)
}

/// Best-effort cwd for resolving relative-path commands at config-save time. Defs are templates — the user's workspace root is the most useful
/// default; OS temp is the last resort. Absolute commands (`C:\Program Files\...`, `/usr/bin/...`) ignore this entirely.
fn backfill_cwd(cfg: &AppConfig) -> std::path::PathBuf {
    cfg.workspace_root.clone().filter(|p| p.is_dir()).unwrap_or_else(std::env::temp_dir)
}

fn pick_directory_native() -> Option<String> {
    rfd::FileDialog::new().pick_folder().map(|p| p.to_string_lossy().into_owned())
}

/// Open the native OS directory picker and return the selected path (or `None`
/// on cancel). The picker is dispatched to Tauri's main thread because GTK's
/// sync dialog APIs must run there on Linux.
#[tauri::command]
pub async fn dialog_pick_directory(app: tauri::AppHandle) -> Result<Option<String>, AppError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
    app.run_on_main_thread(move || {
        let _ = tx.send(pick_directory_native());
    })
    .map_err(|e| AppError::new("Internal", format!("dispatch directory picker: {e}")))?;
    rx.await.map_err(|_| AppError::new("Internal", "directory picker channel closed"))
}

// --------------------------------------------------------------------------- Phase 7: session commands. Each wrapper resolves the managed
// `Arc<AppContext>`, hands the typed args to the matching `*_impl`, and returns the result. *No* business logic in this layer.
// ---------------------------------------------------------------------------

fn ctx_of(app: &tauri::AppHandle) -> Result<Arc<AppContext>, AppError> {
    app.try_state::<Arc<AppContext>>()
        .map(|s| Arc::clone(&*s))
        .ok_or_else(|| AppError::new("Internal", "AppContext not initialised"))
}

#[tauri::command]
pub async fn session_create(app: tauri::AppHandle, args: SessionCreateArgs) -> Result<SessionView, AppError> {
    let ctx = ctx_of(&app)?;
    session::session_create_impl(&ctx, args)
}

#[tauri::command]
pub async fn session_list(app: tauri::AppHandle) -> Result<Vec<SessionView>, AppError> {
    let ctx = ctx_of(&app)?;
    session::session_list_impl(&ctx)
}

#[tauri::command]
pub async fn session_close(app: tauri::AppHandle, args: SessionCloseArgs) -> Result<SessionCloseResult, AppError> {
    let ctx = ctx_of(&app)?;
    // Refuse close while a workspace switch is in progress.
    let _switch = session::acquire_switch_read(&ctx)?;
    // Mark the parent as closing (RAII guard) then close. Sub-sessions are NOT cascaded here — they belong to the worktree tab, not the agent
    // session. The worktree-tab close path handles sub-session teardown.
    let _guard = ctx.mark_parent_closing(args.session_id);
    session::session_close_locked(&ctx, args.session_id, args.delete_worktree).await
}

#[tauri::command]
pub async fn session_focus(app: tauri::AppHandle, args: SessionIdArg) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_focus_impl(&ctx, args.session_id)
}

#[tauri::command]
pub async fn session_resize(app: tauri::AppHandle, args: SessionResizeArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_resize_impl(&ctx, args)
}

#[tauri::command]
pub async fn session_input(app: tauri::AppHandle, args: SessionInputArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_input_impl(&ctx, args)
}

#[tauri::command]
pub async fn session_restart(app: tauri::AppHandle, args: SessionRestartArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_restart_impl(&ctx, args)
}

/// Frontend signals that it has subscribed to `session://output` and `session://status`. The first call triggers restore-on-launch;
/// subsequent calls are no-ops.
///
/// **Awaits restore registration** before resolving so the frontend can safely fire its first `session_resize` (issued synchronously by
/// `attachToHost` → `refitEntry`) and trust that any restored session id is already registered in `pending_spawn`. Without this, the
/// resize-arrives-before-restore race would silently drop the deferred spawn (`pool.resize` → `NotFound`), leaving the session stuck in `Starting`
/// with no PTY child.
///
/// **Workspace-switch coordination.** We acquire an `OwnedRwLockReadGuard` on [`AppContext::switch_lock`] and move it into the `spawn_blocking` task
/// that runs `restore_all_sessions`. This bounds the entire restore loop by the same barrier that gates every other workspace-mutating handler: a
/// switch's `write().await` cannot proceed until restore returns. We additionally check
/// [`AppContext::switch_pending`] both before and after taking the
/// owned guard (matching [`session::acquire_switch_read`]'s ordering) because tokio's `try_read_owned` is permit-based and does NOT reject when a
/// writer is queued behind active readers — the counter is what closes that gap. On a negative outcome we silently `Ok(())`. (As of PR5, in-app
/// workspace switches run their own inline restore under the write guard and no longer rely on a follow-up `frontend_ready`; this command remains for
/// the boot-time initial restore.)
#[tauri::command]
pub async fn frontend_ready(app: tauri::AppHandle) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    // Pre-check: cheap atomic load avoids touching the lock during a switch.
    if ctx.switch_pending.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        return Ok(());
    }
    let switch_guard = match Arc::clone(&ctx.switch_lock).try_read_owned() {
        Ok(g) => g,
        Err(_) => return Ok(()),
    };
    // Post-check: closes the take-then-set race the same way `acquire_switch_read` does. See [`AppContext::switch_lock`].
    if ctx.switch_pending.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        return Ok(());
    }
    if !session::frontend_ready_impl(&ctx) {
        // Already restored — drop guard and return.
        return Ok(());
    }
    let ctx_for_task = Arc::clone(&ctx);
    let sub_ctx_for_task = Arc::clone(&sub_ctx);
    // `restore_all_sessions` no longer spawns PTYs (it only does disk IO + HashMap inserts), so the work is bounded — but we still run it on a
    // blocking thread because materialise_temp_files / cleanup_orphans / store IO can block. We *await* completion here so the resolution of
    // `frontend_ready` becomes a happens-before edge for the frontend's first `session_resize`.
    //
    // Phase 7: after the parent-session restore completes, run the sub-session restore second pass on the SAME blocking thread so children only
    // attempt to spawn after their parents have been re-materialised in `sessions.json`. Both restores must be done before we return — same
    // happens-before reasoning.
    tauri::async_runtime::spawn_blocking(move || {
        // Move the owned switch read guard into the closure so it stays held for the full restore loop. Dropped when the closure returns.
        let _switch = switch_guard;
        session::restore_all_sessions(&ctx_for_task);
        subsession::restore_all_sub_sessions_impl(&ctx_for_task, &sub_ctx_for_task);
    })
    .await
    .map_err(|join_err| AppError::new("Internal", format!("restore_all_sessions task panicked: {join_err}")))?;
    Ok(())
}

/// Enumerate worktrees rooted at `repo_root`. Always returns `Ok(vec![])` on discovery failures so the UI's "Browse…" fallback
/// is never blocked by an error toast.
#[tauri::command]
pub async fn worktrees_list(app: tauri::AppHandle, repo_root: String) -> Result<Vec<crate::types::WorktreeInfo>, AppError> {
    let ctx = ctx_of(&app)?;
    let path = PathBuf::from(repo_root);
    session::worktrees_list_impl(&ctx, &path)
}

/// Snapshot `git status` for a worktree (Issue #55). Always returns `Ok(...)`; on any discovery failure (invalid/missing path, non-repo, `git`
/// binary missing, non-zero `git` exit) the result is a default-valued [`crate::types::WorktreeGitStatus`] with `error: Some(message)` populated
/// so the dashboard can distinguish a clean tree from an unreadable one and surface "unable to read git status" rather than blocking.
#[tauri::command]
pub async fn worktree_git_status(
    app: tauri::AppHandle,
    args: crate::types::WorktreeGitStatusArgs,
) -> Result<crate::types::WorktreeGitStatus, AppError> {
    let ctx = ctx_of(&app)?;
    crate::plugins::dashboard_widget::git_status::worktree_git_status_impl(&ctx, &args.path)
}

/// Validate a candidate workspace root (Roadmap §1.1). Never errors for the "invalid path" case — the picker shows inline feedback.
#[tauri::command]
pub async fn workspace_validate(app: tauri::AppHandle, args: WorkspaceValidateArgs) -> Result<WorkspaceValidateResult, AppError> {
    use tauri::Manager as _;
    let ctx = ctx_of(&app)?;
    let path = PathBuf::from(args.path);
    let app_data_dir = app.path().app_data_dir().map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    session::workspace_validate_impl(&ctx, &path, Some(&app_data_dir), crate::BUILD_BRANCH)
}

/// Create a new linked worktree under `<workspaceRoot>/.arborist/.worktrees/<name>` on a fresh branch named `<name>` (Roadmap §2.2, issue #71).
///
/// After the worktree is created, kicks off `worktree_prep_commands` (issue #63) in the background. Repo-stored overrides from
/// `<workspaceRoot>/.arborist/settings.json` (issue #71) override the user-level prep command list when present.
///
/// The returned `prep` field lets the frontend correlate `worktree://prep` events. The workspace-switch barrier covers both creation and prep spawn,
/// so the prep is bound to the same active workspace as the creation.
#[tauri::command]
pub async fn worktree_create(app: tauri::AppHandle, args: WorktreeCreateArgs) -> Result<WorktreeCreateResult, AppError> {
    let ctx = ctx_of(&app)?;
    let _switch = session::acquire_switch_read(&ctx)?;
    let cfg = session::trusted_worktree_create_config(&ctx, &args.name)?;
    let mut result = session::worktree_create_impl(&ctx, &args.name)?;
    result.prep = crate::worktree_prep::maybe_spawn(&app, ctx.prep_registry.clone(), &cfg, &result.path);
    Ok(result)
}

/// Open a worktree-prep log file in the user's default OS handler.
///
/// The canonical path must live under `<app_data_dir>/worktree-prep-logs/`, so this cannot be abused as a generic file opener.
#[tauri::command]
pub async fn worktree_prep_open_log(app: tauri::AppHandle, args: crate::types::WorktreePrepOpenLogArgs) -> Result<(), AppError> {
    use tauri::Manager as _;
    let app_data_dir = app.path().app_data_dir().map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    let logs_root = app_data_dir.join(crate::worktree_prep::LOG_SUBDIR);
    let canon_path = validate_prep_log_path(&app_data_dir, &logs_root, &args.log_path)?;
    open_path_with_os(&canon_path).map_err(|e| AppError::new("Io", format!("open log: {e}")))
}

fn validate_prep_log_path(app_data_dir: &Path, logs_root: &Path, log_path: &Path) -> Result<PathBuf, AppError> {
    let root_meta = std::fs::symlink_metadata(logs_root).map_err(|e| AppError::new("InvalidPath", format!("stat logs root: {e}")))?;
    if !root_meta.is_dir() {
        return Err(AppError::new(
            "InvalidPath",
            format!("logs root is not a directory: {}", logs_root.display()),
        ));
    }
    if root_meta.file_type().is_symlink() {
        return Err(AppError::new(
            "PermissionDenied",
            format!("logs root is a symlink: {}", logs_root.display()),
        ));
    }

    let canon_app_data = dunce::canonicalize(app_data_dir).map_err(|e| AppError::new("InvalidPath", format!("canonicalize app data: {e}")))?;
    let canon_root = dunce::canonicalize(logs_root).map_err(|e| AppError::new("InvalidPath", format!("canonicalize logs root: {e}")))?;
    reject_redirected_logs_root(&canon_app_data, &canon_root).map_err(|message| AppError::new("PermissionDenied", message))?;

    let canon_path = dunce::canonicalize(log_path).map_err(|e| AppError::new("InvalidPath", format!("canonicalize log path: {e}")))?;
    if !canon_path.starts_with(&canon_root) {
        return Err(AppError::new(
            "PermissionDenied",
            format!("log path is outside the worktree-prep-logs root: {}", canon_path.display()),
        ));
    }
    Ok(canon_path)
}

fn reject_redirected_logs_root(canon_app_data: &Path, canon_root: &Path) -> Result<(), String> {
    let expected = canon_app_data.join(crate::worktree_prep::LOG_SUBDIR);
    if canon_root == expected {
        return Ok(());
    }
    Err(format!(
        "worktree-prep logs root resolves outside the expected app-data location: {}",
        canon_root.display()
    ))
}

fn spawn_opener(mut command: std::process::Command) -> std::io::Result<()> {
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_path_with_os(path: &std::path::Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new("explorer.exe");
    command.arg(path);
    spawn_opener(command)
}

#[cfg(target_os = "macos")]
fn open_path_with_os(path: &std::path::Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new("open");
    command.arg(path);
    spawn_opener(command)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_path_with_os(path: &std::path::Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new("xdg-open");
    command.arg(path);
    spawn_opener(command)
}

/// Switch the active workspace in-place (Phase 7). Closes every open session in the current workspace, releases its OS lock, acquires the new
/// workspace's lock, opens the new ConfigStore, runs `restore_all_sessions` for the new workspace inline, and returns the post-switch `{ config,
/// sessions }` so the frontend can adopt everything in one render. Returns `AppError::WorkspaceLocked` if another Arborist instance holds the new
/// workspace's lock.
#[tauri::command]
pub async fn workspace_switch(app: tauri::AppHandle, args: WorkspaceSwitchArgs) -> Result<WorkspaceSwitchResult, AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    let path = PathBuf::from(args.path);
    session::workspace_switch_impl(&ctx, Some(sub_ctx), &app, &path).await
}

// --------------------------------------------------------------------------- Production PtySink builder.
// ---------------------------------------------------------------------------

/// Construct the production [`PtySink`] whose callbacks emit Tauri events and persist status changes to the [`ConfigStore`].
///
/// The status callback persists the new status before emitting the event so any subsequent `session_list` observes the new value. NotFound errors are
/// intentionally swallowed: the wait thread can race `session_close` and report `Exited` against an already-removed record. Build a
/// [`PtySink`](crate::pty_pool::PtySink) that bridges PTY events from the [`PtyPool`](crate::pty_pool::PtyPool) into Tauri events and the persisted
/// session record.
///
/// Output / activity callbacks: pure event-emit, no store touch — the closure only borrows `app: AppHandle`.
///
/// Status callback: persists `SessionStatus` and `pid` via `ConfigStore::update_session_status`. Phase 7 (in-app workspace switch): the closure
/// resolves the *current* store via `workspace.read()` on every invocation rather than capturing a snapshot — so a status update that arrives after a
/// workspace swap writes into the new workspace's store. (This is generally a no-op because the new store does not contain the old workspace's
/// session id, and `update_session_status` returns `NotFound` which is intentionally swallowed below — but it is the right semantics: never write
/// into the abandoned old store.)
///
/// `update_session_status` may return `NotFound` if the wait thread races `session_close` (or, post-switch, if the session belongs to the old
/// workspace). NotFound errors are intentionally swallowed.
#[must_use]
pub fn build_production_sink(app: tauri::AppHandle, workspace: Arc<RwLock<WorkspaceScope>>) -> crate::pty_pool::PtySink {
    let app_for_output = app.clone();
    let output = Arc::new(move |session_id: &SessionId, data: String| {
        let payload = SessionOutputEvent {
            session_id: *session_id,
            data,
        };
        if let Err(e) = app_for_output.emit("session://output", payload) {
            tracing::debug!(session_id = %session_id, error = %e, "emit session://output failed");
        }
    });

    let app_for_status = app.clone();
    let workspace_for_status = workspace;
    let status = Arc::new(
        move |session_id: &SessionId, status: SessionStatus, pid: Option<u32>, message: Option<String>| {
            // Re-resolve the current store on every callback so a workspace switch in flight cannot cause a stale write into the previously-bound
            // store.
            let store = match workspace_for_status.read() {
                Ok(guard) => match guard.store.clone() {
                    Some(s) => s,
                    None => {
                        tracing::debug!(session_id = %session_id, "workspace unbound; skipping status persist");
                        let payload = SessionStatusEvent {
                            session_id: *session_id,
                            status,
                            message,
                        };
                        let _ = app_for_status.emit("session://status", &payload);
                        return;
                    }
                },
                Err(_) => {
                    tracing::error!(session_id = %session_id, "workspace lock poisoned; skipping status persist");
                    // Still emit the event so the frontend sees the transition.
                    let payload = SessionStatusEvent {
                        session_id: *session_id,
                        status,
                        message,
                    };
                    let _ = app_for_status.emit("session://status", payload);
                    return;
                }
            };
            if let Err(e) = store.update_session_status(session_id, status, pid) {
                use crate::types::Error as E;
                if !matches!(e, E::NotFound(_)) {
                    tracing::warn!(session_id = %session_id, error = ?e, "persist status failed");
                }
            }
            let payload = SessionStatusEvent {
                session_id: *session_id,
                status,
                message,
            };
            if let Err(e) = app_for_status.emit("session://status", payload) {
                tracing::debug!(session_id = %session_id, error = %e, "emit session://status failed");
            }
        },
    );

    let app_for_activity = app;
    let activity = Arc::new(move |session_id: &SessionId, event: crate::activity::ActivityEvent| {
        let payload = crate::types::SessionActivityEvent {
            session_id: *session_id,
            event,
        };
        if let Err(e) = app_for_activity.emit("session://activity", payload) {
            tracing::debug!(session_id = %session_id, error = %e, "emit session://activity failed");
        }
    });

    crate::pty_pool::PtySink::new(output, status, activity)
}

/// Build the production metrics emitter (Issue #3) — fires `session://metrics` Tauri events and persists the snapshot on the session record so
/// restore can seed the frontend dashboard (Issue #140). Tests construct their own callback (typically a channel sender) and pass it to
/// [`AppContext::new`].
#[must_use]
pub fn build_production_metrics_emit(app: tauri::AppHandle, workspace: Arc<RwLock<WorkspaceScope>>) -> crate::session_metrics::MetricsCb {
    Arc::new(move |payload: crate::types::SessionMetricsEvent| {
        if let Err(e) = app.emit("session://metrics", &payload) {
            tracing::debug!(error = %e, "emit session://metrics failed");
        }
        // Best-effort persist — errors are swallowed so a transient store issue doesn't crash the watcher thread.
        let store = match workspace.read() {
            Ok(guard) => match guard.store.clone() {
                Some(s) => s,
                None => return, // unbound — no sessions exist yet
            },
            Err(_) => {
                tracing::warn!("workspace lock poisoned; skipping metrics persist");
                return;
            }
        };
        let session_id = payload.session_id;
        if let Err(e) = store.update_session_metrics(&session_id, payload) {
            match &e {
                crate::types::Error::Internal(_) => {
                    tracing::warn!(error = ?e, "metrics persist bug: id/payload mismatch");
                }
                crate::types::Error::NotFound(_) => {
                    tracing::trace!(error = ?e, "metrics persist skipped (session gone — expected during teardown)");
                }
                _ => {
                    tracing::debug!(error = ?e, "failed to persist session metrics");
                }
            }
        }
    })
}

/// Production AI-session discovery callback. Persists the discovered AI session id on the matching `Session` record so the next app-restart restore
/// can resume the conversation using the tool's resume token (`--resume` for Claude, `--session-id` for Copilot, `resume` subcommand for Codex).
///
/// Phase 7 (in-app workspace switch): the closure resolves the *current* store via `workspace.read()` on every invocation rather than capturing a
/// snapshot. After a switch, callbacks from not-yet-joined watchers will write into the new store; the matching session id will not be present there
/// and the resulting `NotFound` is swallowed below. The Phase 7 switch path also calls `metrics.stop_all_and_join()` before the swap to make this
/// race vanishingly small in practice.
///
/// Errors are intentionally swallowed (with a debug log) — discovery is a best-effort signal that fires every metrics-watcher poll, and a transient
/// store error must not crash the watcher thread or surface to the UI.
#[must_use]
pub fn build_production_ai_session_discover(workspace: Arc<RwLock<WorkspaceScope>>) -> crate::session_metrics::AiSessionDiscoveryCb {
    Arc::new(move |session_id: crate::types::SessionId, ai_session_id: String| {
        let store = match workspace.read() {
            Ok(guard) => match guard.store.clone() {
                Some(s) => s,
                None => return, // unbound — no sessions exist yet
            },
            Err(_) => {
                tracing::error!(%session_id, "workspace lock poisoned; skipping ai session id persist");
                return;
            }
        };
        match store.update_session_ai_session_id(&session_id, Some(ai_session_id.clone())) {
            Ok(true) => {
                tracing::debug!(%session_id, %ai_session_id, "ai session id discovered");
            }
            Ok(false) => {}
            Err(e) => {
                tracing::debug!(%session_id, error = ?e, "failed to persist ai session id");
            }
        }
    })
}

/// Build the production turn-end emitter — fires a
/// [`crate::activity::ActivityEvent::TurnEnd`] over the existing
/// `session://activity` channel so the frontend's activity reducer handles it the same way as PTY-derived activity events. Tests substitute a
/// capturing closure.
#[must_use]
pub fn build_production_turn_emit(app: tauri::AppHandle) -> crate::session_metrics::TurnCb {
    Arc::new(move |session_id: SessionId, duration_ms: Option<u64>| {
        let payload = crate::types::SessionActivityEvent {
            session_id,
            event: crate::activity::ActivityEvent::TurnEnd { duration_ms },
        };
        if let Err(e) = app.emit("session://activity", payload) {
            tracing::debug!(session_id = %session_id, error = %e, "emit session://activity (turnEnd) failed");
        }
    })
}

// --------------------------------------------------------------------------- Phase 2: sub-session commands. Wrappers resolve the managed
// `Arc<SubAppContext>` (created in `lib.rs::run`) and forward to the matching `subsession::*_impl`.
// ---------------------------------------------------------------------------

fn sub_ctx_of(app: &tauri::AppHandle) -> Result<Arc<SubAppContext>, AppError> {
    app.try_state::<Arc<SubAppContext>>()
        .map(|s| Arc::clone(&*s))
        .ok_or_else(|| AppError::new("Internal", "SubAppContext not initialised"))
}

#[tauri::command]
pub async fn subsession_create(app: tauri::AppHandle, args: SubSessionCreateArgs) -> Result<SubSession, AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_create_impl(&ctx, &sub_ctx, args)
}

#[tauri::command]
pub async fn subsession_close(app: tauri::AppHandle, args: SubSessionCloseArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_close_impl(&ctx, sub_ctx, args.id, args.intent).await
}

#[tauri::command]
pub async fn subsession_focus(app: tauri::AppHandle, args: SubSessionIdArg) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_focus_impl(&ctx, &sub_ctx, args.id)
}

#[tauri::command]
pub async fn subsession_list(app: tauri::AppHandle, args: SubSessionListArgs) -> Result<Vec<SubSession>, AppError> {
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_list_impl(&sub_ctx, args.parent_worktree_tab_id)
}

#[tauri::command]
pub async fn subsession_input(app: tauri::AppHandle, args: SubSessionInputArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_input_impl(&ctx, &sub_ctx, args)
}

#[tauri::command]
pub async fn subsession_resize(app: tauri::AppHandle, args: SubSessionResizeArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_resize_impl(&ctx, &sub_ctx, args)
}

/// Phase 7: relaunch a sub-session under the **same id**. For a greyed Application sub-tab (status `exited`/`error`) this re-spawns the external app;
/// for a Terminal sub-tab it kills the old PTY and spawns a fresh one. The persisted record is unchanged (id stable).
#[tauri::command]
pub async fn subsession_relaunch(app: tauri::AppHandle, args: SubSessionIdArg) -> Result<SubSession, AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_relaunch_impl(&ctx, &sub_ctx, args.id).await
}

/// Best-effort fetch of the OS application icon for an `application`-kind sub-session. Returns `Some("data:image/png;base64,…")` if the OS exposes an
/// icon for the running PID's executable; returns `None` (not an error) for the common cases where extraction isn't possible (PID exited, terminal
/// sub-session, platform unsupported, miss). The frontend falls back to the generic emoji on `None`.
///
/// Extraction runs on the blocking pool because each backend (`SHGetFileInfoW`, `sips`, filesystem walks) can briefly block. Returning `Ok(None)`
/// rather than an error keeps the frontend hook simple — there's no meaningful action it can take on a miss.
#[tauri::command]
pub async fn subsession_icon(app: tauri::AppHandle, args: SubSessionIdArg) -> Result<Option<String>, AppError> {
    let sub_ctx = sub_ctx_of(&app)?;
    let pid = match sub_ctx.store.get(&args.id) {
        Some(s) => s.pid,
        None => return Ok(None),
    };
    let Some(pid) = pid else {
        return Ok(None);
    };
    let cache = sub_ctx.icon_cache.clone();
    let result = tokio::task::spawn_blocking(move || cache.data_uri_for(pid))
        .await
        .map_err(|err| AppError::new("Internal", format!("icon extraction join failed: {err}")))?;
    Ok(result)
}

/// Build the production [`crate::sub_sessions::SubPtySink`] whose callbacks emit Tauri events over `session://output` (shared UUID id space) and the
/// new `subsession://status` / `subsession://exited` channels. The status callback also mutates the in-memory
/// [`crate::sub_sessions::SubSessionStore`] so `subsession_list` returns
/// the current lifecycle state without requiring the frontend to maintain its own shadow copy.
#[must_use]
pub fn build_production_sub_sink(app: tauri::AppHandle, store: Arc<crate::sub_sessions::SubSessionStore>) -> crate::sub_sessions::SubPtySink {
    let app_for_output = app.clone();
    let output = Arc::new(move |id: &crate::types::SubSessionId, data: String| {
        let payload = SessionOutputEvent {
            session_id: SessionId(id.0),
            data,
        };
        if let Err(e) = app_for_output.emit("session://output", payload) {
            tracing::debug!(sub_session_id = %id, error = %e, "emit session://output (sub) failed");
        }
    });

    let app_for_status = app.clone();
    let store_for_status = store;
    let status = Arc::new(
        move |id: &crate::types::SubSessionId, status: crate::types::SubSessionStatus, pid: Option<u32>, message: Option<String>| {
            // Persist status into the in-memory store before emitting so any `subsession_list` racing the event sees the new value. NotFound is
            // expected when the sub-session is closed before its wait thread reports completion.
            if let Err(e) = store_for_status.set_status(id, status, pid) {
                use crate::types::Error as E;
                if !matches!(e, E::NotFound(_)) {
                    tracing::warn!(sub_session_id = %id, error = ?e, "persist sub status failed");
                }
            }
            let payload = crate::types::SubSessionStatusEvent {
                id: *id,
                status,
                pid,
                message,
            };
            if let Err(e) = app_for_status.emit("subsession://status", payload) {
                tracing::debug!(sub_session_id = %id, error = %e, "emit subsession://status failed");
            }
        },
    );

    let app_for_exit = app.clone();
    let exited = Arc::new(move |id: &crate::types::SubSessionId, exit_code: Option<i32>| {
        let payload = crate::types::SubSessionExitedEvent { id: *id, exit_code };
        if let Err(e) = app_for_exit.emit("subsession://exited", payload) {
            tracing::debug!(sub_session_id = %id, error = %e, "emit subsession://exited failed");
        }
    });

    let app_for_restored = app;
    let restored = Arc::new(move |sub: &crate::types::SubSession| {
        let payload = crate::types::SubSessionRestoredEvent { sub_session: sub.clone() };
        if let Err(e) = app_for_restored.emit("subsession://restored", payload) {
            tracing::debug!(sub_session_id = %sub.id, error = %e, "emit subsession://restored failed");
        }
    });

    crate::sub_sessions::SubPtySink::new(output, status, exited, restored)
}

// --------------------------------------------------------------------------- Worktree tab commands (Issue #44)

#[tauri::command]
pub async fn worktree_tab_open(app: tauri::AppHandle, args: WorktreeTabOpenArgs) -> Result<WorktreeTab, AppError> {
    let ctx = ctx_of(&app)?;
    worktree_tab::worktree_tab_open_impl(&ctx, args)
}

#[tauri::command]
pub async fn worktree_tab_close(app: tauri::AppHandle, args: WorktreeTabCloseArgs) -> Result<WorktreeTabCloseResult, AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    worktree_tab::worktree_tab_close_impl(&ctx, sub_ctx, args.id, args.delete_worktree, args.app_close_policy).await
}

#[tauri::command]
pub async fn worktree_tab_focus(app: tauri::AppHandle, args: WorktreeTabFocusArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    worktree_tab::worktree_tab_focus_impl(&ctx, args.id)
}

#[tauri::command]
pub async fn worktree_tab_list(app: tauri::AppHandle) -> Result<Vec<WorktreeTab>, AppError> {
    let ctx = ctx_of(&app)?;
    worktree_tab::worktree_tab_list_impl(&ctx)
}

#[tauri::command]
pub async fn worktree_tab_reorder(app: tauri::AppHandle, args: WorktreeTabReorderArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    worktree_tab::worktree_tab_reorder_impl(&ctx, args.ids)
}

#[tauri::command]
pub async fn worktree_tab_set_active_child(app: tauri::AppHandle, args: WorktreeTabSetActiveChildArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    worktree_tab::worktree_tab_set_active_child_impl(&ctx, sub_ctx, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_returns_pong() {
        let result = ping().await.expect("ping is infallible");
        assert_eq!(result, "pong");
    }

    #[test]
    fn validate_prep_log_path_accepts_file_inside_logs_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logs_root = temp.path().join(crate::worktree_prep::LOG_SUBDIR);
        std::fs::create_dir_all(&logs_root).expect("logs root");
        let log_path = logs_root.join("prep.log");
        std::fs::write(&log_path, b"log").expect("log file");

        let validated = validate_prep_log_path(temp.path(), &logs_root, &log_path).expect("valid path");

        assert_eq!(validated, dunce::canonicalize(log_path).expect("canonical log"));
        #[cfg(windows)]
        assert!(!validated.as_os_str().to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn validate_prep_log_path_rejects_file_outside_logs_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logs_root = temp.path().join(crate::worktree_prep::LOG_SUBDIR);
        std::fs::create_dir_all(&logs_root).expect("logs root");
        let outside = temp.path().join("outside.log");
        std::fs::write(&outside, b"log").expect("outside file");

        let err = validate_prep_log_path(temp.path(), &logs_root, &outside).expect_err("outside path must fail");

        assert_eq!(err.code, "PermissionDenied");
    }

    #[test]
    fn validate_prep_log_path_maps_missing_log_to_invalid_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logs_root = temp.path().join(crate::worktree_prep::LOG_SUBDIR);
        std::fs::create_dir_all(&logs_root).expect("logs root");

        let err = validate_prep_log_path(temp.path(), &logs_root, &logs_root.join("missing.log")).expect_err("missing path must fail");

        assert_eq!(err.code, "InvalidPath");
    }

    #[test]
    fn validate_prep_log_path_maps_missing_logs_root_to_invalid_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logs_root = temp.path().join(crate::worktree_prep::LOG_SUBDIR);
        let log_path = temp.path().join("prep.log");
        std::fs::write(&log_path, b"log").expect("log file");

        let err = validate_prep_log_path(temp.path(), &logs_root, &log_path).expect_err("missing root must fail");

        assert_eq!(err.code, "InvalidPath");
    }

    #[test]
    fn validate_prep_log_path_rejects_redirected_logs_root() {
        let app_data = tempfile::tempdir().expect("app data");
        let redirected_root = tempfile::tempdir().expect("redirected root");
        let log_path = redirected_root.path().join("prep.log");
        std::fs::write(&log_path, b"log").expect("log file");

        let err = validate_prep_log_path(app_data.path(), redirected_root.path(), &log_path).expect_err("redirected root must fail");

        assert_eq!(err.code, "PermissionDenied");
    }
}
