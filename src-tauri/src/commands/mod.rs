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

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{Emitter, Manager};

use crate::config_store::{list_instructions_for, ConfigStore};
use crate::types::{
    AppConfig, AppError, InstructionSet, PartialAppConfig, SessionCloseArgs, SessionCloseResult,
    SessionCreateArgs, SessionId, SessionIdArg, SessionInputArgs, SessionOutputEvent,
    SessionResizeArgs, SessionStatus, SessionStatusEvent, SessionView, WorkspaceValidateArgs,
    WorkspaceValidateResult, WorktreeCreateArgs, WorktreeCreateResult,
};

pub use session::AppContext;

/// Smoke-test command used to verify the Tauri command/event scaffold is
/// wired correctly. Always returns `Ok("pong")`.
#[tauri::command]
pub async fn ping() -> Result<String, AppError> {
    Ok("pong".to_owned())
}

/// Resolve the [`ConfigStore`] for the current Tauri app instance.
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
    let store = store_for(&app)?;
    Ok(store.load_config())
}

/// Deep-merges `partial` into the persisted [`AppConfig`].
#[tauri::command]
pub async fn config_set(app: tauri::AppHandle, partial: PartialAppConfig) -> Result<(), AppError> {
    let store = store_for(&app)?;
    store.save_config(partial).map_err(AppError::from)?;
    Ok(())
}

/// Discovers and returns the list of [`InstructionSet`]s available under the
/// configured `instructionSetsDir`.
#[tauri::command]
pub async fn instructions_list(app: tauri::AppHandle) -> Result<Vec<InstructionSet>, AppError> {
    let store = store_for(&app)?;
    let cfg = store.load_config();
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
    if session::frontend_ready_impl(&ctx) {
        let ctx_for_task = Arc::clone(&ctx);
        // `restore_all_sessions` does blocking IO and PTY spawn — run it
        // on a blocking thread so we don't hold the executor. We
        // intentionally don't await the JoinHandle: restore is fire-and-
        // forget from the frontend's perspective.
        std::mem::drop(tauri::async_runtime::spawn_blocking(move || {
            session::restore_all_sessions(&ctx_for_task);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_returns_pong() {
        let result = ping().await.expect("ping is infallible");
        assert_eq!(result, "pong");
    }
}
