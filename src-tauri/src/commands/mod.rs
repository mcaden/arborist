//! Tauri command handlers.
//!
//! Phase 3 introduced `ping`; Phase 4 added `config_get`/`config_set` and
//! `instructions_list`; Phase 7 adds the full session lifecycle plus the
//! `frontend_ready` gate. The actual business logic for session commands
//! lives in the [`session`] submodule as `*_impl` free functions taking
//! [`session::AppContext`]; the `#[tauri::command]` wrappers below are
//! intentionally thin so that integration tests can drive the same code
//! paths directly.
//!
//! ## Capability model (Tauri v2)
//!
//! In Tauri v2, application-defined commands are gated by capability
//! declarations the same way plugin commands are. Each command needs a
//! permission file under `src-tauri/permissions/` referenced from
//! `src-tauri/capabilities/main.json`. Adding a new command without the
//! matching permission entry will cause the `invoke()` call to be rejected
//! at runtime with no compile-time warning.

pub mod session;
pub mod subsession;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{Emitter, Manager};

use crate::config_store::{list_instructions_for, ConfigStore};
use crate::sub_sessions::SubAppContext;
use crate::types::{
    AppConfig, AppError, InstructionSet, PartialAppConfig, SessionCloseArgs, SessionCloseResult,
    SessionCreateArgs, SessionId, SessionIdArg, SessionInputArgs, SessionOutputEvent,
    SessionResizeArgs, SessionStatus, SessionStatusEvent, SessionView, SubSession,
    SubSessionCreateArgs, SubSessionIdArg, SubSessionInputArgs, SubSessionListArgs,
    SubSessionResizeArgs, WorkspaceValidateArgs, WorkspaceValidateResult, WorktreeCreateArgs,
    WorktreeCreateResult,
};

pub use session::AppContext;

/// Smoke-test command used to verify the Tauri command/event scaffold is
/// wired correctly. Always returns `Ok("pong")`.
#[tauri::command]
pub async fn ping() -> Result<String, AppError> {
    Ok("pong".to_owned())
}

/// Resolve the [`ConfigStore`] for the current Tauri app instance.
///
/// **Avoid in command handlers** — prefer `ctx_of(&app)?.store.clone()`
/// so all writes share the managed `AppContext`'s mutex (otherwise each
/// fresh `ConfigStore::open` gets its own mutex and load-modify-write
/// races between command threads silently lose updates). This helper
/// remains for boot-time wiring in `lib.rs`, before `AppContext` is
/// constructed.
pub fn store_for(app: &tauri::AppHandle) -> Result<ConfigStore, AppError> {
    let dir: PathBuf = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    ConfigStore::open(dir).map_err(AppError::from)
}

/// Returns the persisted [`AppConfig`].
#[tauri::command]
pub async fn config_get(app: tauri::AppHandle) -> Result<AppConfig, AppError> {
    let ctx = ctx_of(&app)?;
    Ok(ctx.store.load_config())
}

/// Deep-merges `partial` into the persisted [`AppConfig`].
#[tauri::command]
pub async fn config_set(app: tauri::AppHandle, partial: PartialAppConfig) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    // Run the user's patch and the icon backfill *under the same
    // write lock* so two concurrent `config_set` calls can't lose
    // each other's updates. `save_config_with` holds the lock
    // across load → merge → mutate → write.
    let icon_cache = sub_ctx_of(&app).ok().map(|c| c.icon_cache.clone());
    ctx.store
        .save_config_with(partial, |cfg| {
            // Best-effort: walk every command string and resolve a
            // cached icon data URI. Failures are swallowed — the
            // user's patch is what matters here, the icon is a
            // cosmetic enhancement.
            let Some(cache) = &icon_cache else {
                return false;
            };
            let cwd = backfill_cwd(cfg);
            crate::icon_backfill::backfill_icons(cfg, cache, &cwd)
        })
        .map_err(AppError::from)?;
    Ok(())
}

/// Best-effort cwd for resolving relative-path commands at config-save
/// time. Defs are templates — the user's workspace root is the most
/// useful default; OS temp is the last resort. Absolute commands
/// (`C:\Program Files\...`, `/usr/bin/...`) ignore this entirely.
fn backfill_cwd(cfg: &AppConfig) -> std::path::PathBuf {
    cfg.workspace_root
        .clone()
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// Discovers and returns the list of [`InstructionSet`]s available under the
/// configured `instructionSetsDir`.
#[tauri::command]
pub async fn instructions_list(app: tauri::AppHandle) -> Result<Vec<InstructionSet>, AppError> {
    let ctx = ctx_of(&app)?;
    let cfg = ctx.store.load_config();
    list_instructions_for(&cfg)
}

// ---------------------------------------------------------------------------
// Phase 7: session commands. Each wrapper resolves the managed
// `Arc<AppContext>`, hands the typed args to the matching `*_impl`, and
// returns the result. *No* business logic in this layer.
// ---------------------------------------------------------------------------

fn ctx_of(app: &tauri::AppHandle) -> Result<Arc<AppContext>, AppError> {
    app.try_state::<Arc<AppContext>>()
        .map(|s| Arc::clone(&*s))
        .ok_or_else(|| AppError::new("Internal", "AppContext not initialised"))
}

#[tauri::command]
pub async fn session_create(
    app: tauri::AppHandle,
    args: SessionCreateArgs,
) -> Result<SessionView, AppError> {
    let ctx = ctx_of(&app)?;
    session::session_create_impl(&ctx, args)
}

#[tauri::command]
pub async fn session_list(app: tauri::AppHandle) -> Result<Vec<SessionView>, AppError> {
    let ctx = ctx_of(&app)?;
    session::session_list_impl(&ctx)
}

#[tauri::command]
pub async fn session_close(
    app: tauri::AppHandle,
    args: SessionCloseArgs,
) -> Result<SessionCloseResult, AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    // Phase 7 cascade: mark the parent as closing (RAII guard ensures
    // removal even on panic), tear down its sub-sessions, then close the
    // parent itself. The tombstone closes the door on a concurrent
    // `subsession_create` racing into the close window.
    let _guard = ctx.mark_parent_closing(args.session_id);
    subsession::close_for_parent_impl(&ctx, &sub_ctx, args.session_id).await;
    session::session_close_impl(&ctx, args.session_id, args.delete_worktree).await
}

#[tauri::command]
pub async fn session_focus(app: tauri::AppHandle, args: SessionIdArg) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_focus_impl(&ctx, args.session_id)
}

#[tauri::command]
pub async fn session_resize(
    app: tauri::AppHandle,
    args: SessionResizeArgs,
) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_resize_impl(&ctx, args)
}

#[tauri::command]
pub async fn session_input(app: tauri::AppHandle, args: SessionInputArgs) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_input_impl(&ctx, args)
}

#[tauri::command]
pub async fn session_restart(app: tauri::AppHandle, args: SessionIdArg) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_restart_impl(&ctx, args.session_id)
}

/// Frontend signals that it has subscribed to `session://output` and
/// `session://status`. The first call triggers restore-on-launch (DESIGN
/// §5.5); subsequent calls are no-ops.
#[tauri::command]
pub async fn frontend_ready(app: tauri::AppHandle) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    if session::frontend_ready_impl(&ctx) {
        let ctx_for_task = Arc::clone(&ctx);
        let sub_ctx_for_task = Arc::clone(&sub_ctx);
        // `restore_all_sessions` does blocking IO and PTY spawn — run it
        // on a blocking thread so we don't hold the executor. We
        // intentionally don't await the JoinHandle: restore is fire-and-
        // forget from the frontend's perspective.
        //
        // Phase 7: after the parent-session restore completes, kick off
        // the sub-session restore second pass on the SAME blocking
        // thread so children only attempt to spawn after their parents
        // have been re-materialised in `sessions.json`.
        std::mem::drop(tauri::async_runtime::spawn_blocking(move || {
            session::restore_all_sessions(&ctx_for_task);
            subsession::restore_all_sub_sessions_impl(&ctx_for_task, &sub_ctx_for_task);
        }));
    }
    Ok(())
}

/// Enumerate worktrees rooted at `repo_root` (DESIGN §6, Phase 10). Always
/// returns `Ok(vec![])` on discovery failures so the UI's "Browse…"
/// fallback is never blocked by an error toast.
#[tauri::command]
pub async fn worktrees_list(
    app: tauri::AppHandle,
    repo_root: String,
) -> Result<Vec<crate::types::WorktreeInfo>, AppError> {
    let ctx = ctx_of(&app)?;
    let path = PathBuf::from(repo_root);
    session::worktrees_list_impl(&ctx, &path)
}

/// Validate a candidate workspace root (Roadmap §1.1). Never errors for the
/// "invalid path" case — the picker shows inline feedback.
#[tauri::command]
pub async fn workspace_validate(
    app: tauri::AppHandle,
    args: WorkspaceValidateArgs,
) -> Result<WorkspaceValidateResult, AppError> {
    let ctx = ctx_of(&app)?;
    let path = PathBuf::from(args.path);
    session::workspace_validate_impl(&ctx, &path)
}

/// Create a new linked worktree under `<workspaceRoot>/.worktrees/<name>`
/// on a fresh branch named `<name>` (Roadmap §2.2).
#[tauri::command]
pub async fn worktree_create(
    app: tauri::AppHandle,
    args: WorktreeCreateArgs,
) -> Result<WorktreeCreateResult, AppError> {
    let ctx = ctx_of(&app)?;
    session::worktree_create_impl(&ctx, &args.name)
}

// ---------------------------------------------------------------------------
// Production PtySink builder.
// ---------------------------------------------------------------------------

/// Construct the production [`PtySink`] whose callbacks emit Tauri events
/// and persist status changes to the [`ConfigStore`].
///
/// The status callback persists the new status before emitting the event so
/// any subsequent `session_list` observes the new value. NotFound errors
/// are intentionally swallowed: the wait thread can race `session_close`
/// and report `Exited` against an already-removed record.
#[must_use]
pub fn build_production_sink(
    app: tauri::AppHandle,
    store: ConfigStore,
) -> crate::pty_pool::PtySink {
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
    let store_for_status = store;
    let status = Arc::new(
        move |session_id: &SessionId,
              status: SessionStatus,
              pid: Option<u32>,
              message: Option<String>| {
            if let Err(e) = store_for_status.update_session_status(session_id, status, pid) {
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
    let activity = Arc::new(
        move |session_id: &SessionId, event: crate::activity::ActivityEvent| {
            let payload = crate::types::SessionActivityEvent {
                session_id: *session_id,
                event,
            };
            if let Err(e) = app_for_activity.emit("session://activity", payload) {
                tracing::debug!(session_id = %session_id, error = %e, "emit session://activity failed");
            }
        },
    );

    crate::pty_pool::PtySink::new(output, status, activity)
}

/// Build the production metrics emitter (Issue #3) — fires
/// `session://metrics` Tauri events. Tests construct their own callback
/// (typically a channel sender) and pass it to [`AppContext::new`].
#[must_use]
pub fn build_production_metrics_emit(app: tauri::AppHandle) -> crate::session_metrics::MetricsCb {
    Arc::new(move |payload: crate::types::SessionMetricsEvent| {
        if let Err(e) = app.emit("session://metrics", payload) {
            tracing::debug!(error = %e, "emit session://metrics failed");
        }
    })
}

/// Production AI-session discovery callback. Persists the discovered AI
/// session id on the matching `Session` record so the next app-restart
/// restore can `--resume <id>` and continue the conversation.
///
/// Errors are intentionally swallowed (with a debug log) — discovery is
/// a best-effort signal that fires every metrics-watcher poll, and a
/// transient store error must not crash the watcher thread or surface
/// to the UI.
#[must_use]
pub fn build_production_ai_session_discover(
    store: crate::config_store::ConfigStore,
) -> crate::session_metrics::AiSessionDiscoveryCb {
    Arc::new(
        move |session_id: crate::types::SessionId, ai_session_id: String| match store
            .update_session_ai_session_id(&session_id, Some(ai_session_id.clone()))
        {
            Ok(true) => {
                tracing::debug!(%session_id, %ai_session_id, "ai session id discovered");
            }
            Ok(false) => {}
            Err(e) => {
                tracing::debug!(%session_id, error = ?e, "failed to persist ai session id");
            }
        },
    )
}

/// Build the production turn-end emitter — fires a
/// [`crate::activity::ActivityEvent::TurnEnd`] over the existing
/// `session://activity` channel so the frontend's activity reducer
/// handles it the same way as PTY-derived activity events. Tests
/// substitute a capturing closure.
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

// ---------------------------------------------------------------------------
// Phase 2: sub-session commands. Wrappers resolve the managed
// `Arc<SubAppContext>` (created in `lib.rs::run`) and forward to the
// matching `subsession::*_impl`.
// ---------------------------------------------------------------------------

fn sub_ctx_of(app: &tauri::AppHandle) -> Result<Arc<SubAppContext>, AppError> {
    app.try_state::<Arc<SubAppContext>>()
        .map(|s| Arc::clone(&*s))
        .ok_or_else(|| AppError::new("Internal", "SubAppContext not initialised"))
}

#[tauri::command]
pub async fn subsession_create(
    app: tauri::AppHandle,
    args: SubSessionCreateArgs,
) -> Result<SubSession, AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_create_impl(&ctx, &sub_ctx, args)
}

#[tauri::command]
pub async fn subsession_close(
    app: tauri::AppHandle,
    args: SubSessionIdArg,
) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_close_impl(&ctx, sub_ctx, args.id).await
}

#[tauri::command]
pub async fn subsession_focus(
    app: tauri::AppHandle,
    args: SubSessionIdArg,
) -> Result<(), AppError> {
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_focus_impl(&sub_ctx, args.id)
}

#[tauri::command]
pub async fn subsession_list(
    app: tauri::AppHandle,
    args: SubSessionListArgs,
) -> Result<Vec<SubSession>, AppError> {
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_list_impl(&sub_ctx, args.parent_session_id)
}

#[tauri::command]
pub async fn subsession_input(
    app: tauri::AppHandle,
    args: SubSessionInputArgs,
) -> Result<(), AppError> {
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_input_impl(&sub_ctx, args)
}

#[tauri::command]
pub async fn subsession_resize(
    app: tauri::AppHandle,
    args: SubSessionResizeArgs,
) -> Result<(), AppError> {
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_resize_impl(&sub_ctx, args)
}

/// Phase 7: relaunch a sub-session under the **same id**. For a greyed
/// Application sub-tab (status `exited`/`error`) this re-spawns the
/// external app; for a Terminal sub-tab it kills the old PTY and spawns
/// a fresh one. The persisted record is unchanged (id stable).
#[tauri::command]
pub async fn subsession_relaunch(
    app: tauri::AppHandle,
    args: SubSessionIdArg,
) -> Result<SubSession, AppError> {
    let ctx = ctx_of(&app)?;
    let sub_ctx = sub_ctx_of(&app)?;
    subsession::subsession_relaunch_impl(&ctx, &sub_ctx, args.id).await
}

/// Best-effort fetch of the OS application icon for an
/// `application`-kind sub-session. Returns `Some("data:image/png;base64,…")`
/// if the OS exposes an icon for the running PID's executable;
/// returns `None` (not an error) for the common cases where
/// extraction isn't possible (PID exited, terminal sub-session,
/// platform unsupported, miss). The frontend falls back to the
/// generic emoji on `None`.
///
/// Extraction runs on the blocking pool because each backend
/// (`SHGetFileInfoW`, `sips`, filesystem walks) can briefly block.
/// Returning `Ok(None)` rather than an error keeps the frontend hook
/// simple — there's no meaningful action it can take on a miss.
#[tauri::command]
pub async fn subsession_icon(
    app: tauri::AppHandle,
    args: SubSessionIdArg,
) -> Result<Option<String>, AppError> {
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

/// Build the production [`crate::sub_sessions::SubPtySink`] whose callbacks
/// emit Tauri events over `session://output` (shared UUID id space) and the
/// new `subsession://status` / `subsession://exited` channels. The status
/// callback also mutates the in-memory
/// [`crate::sub_sessions::SubSessionStore`] so `subsession_list` returns
/// the current lifecycle state without requiring the frontend to maintain
/// its own shadow copy.
#[must_use]
pub fn build_production_sub_sink(
    app: tauri::AppHandle,
    store: Arc<crate::sub_sessions::SubSessionStore>,
) -> crate::sub_sessions::SubPtySink {
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
        move |id: &crate::types::SubSessionId,
              status: crate::types::SubSessionStatus,
              pid: Option<u32>,
              message: Option<String>| {
            // Persist status into the in-memory store before emitting so
            // any `subsession_list` racing the event sees the new value.
            // NotFound is expected when the sub-session is closed before
            // its wait thread reports completion.
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
    let exited = Arc::new(
        move |id: &crate::types::SubSessionId, exit_code: Option<i32>| {
            let payload = crate::types::SubSessionExitedEvent { id: *id, exit_code };
            if let Err(e) = app_for_exit.emit("subsession://exited", payload) {
                tracing::debug!(sub_session_id = %id, error = %e, "emit subsession://exited failed");
            }
        },
    );

    let app_for_restored = app;
    let restored = Arc::new(move |sub: &crate::types::SubSession| {
        let payload = crate::types::SubSessionRestoredEvent {
            sub_session: sub.clone(),
        };
        if let Err(e) = app_for_restored.emit("subsession://restored", payload) {
            tracing::debug!(sub_session_id = %sub.id, error = %e, "emit subsession://restored failed");
        }
    });

    crate::sub_sessions::SubPtySink::new(output, status, exited, restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_returns_pong() {
        let result = ping().await.expect("ping is infallible");
        assert_eq!(result, "pong");
    }
}
