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
use std::sync::{Arc, RwLock};

use tauri::{Emitter, Manager};

use crate::config_store::{list_instructions_for, ConfigStore};
use crate::types::{
    AppConfig, AppError, InstructionSet, PartialAppConfig, SessionCloseArgs, SessionCloseResult,
    SessionCreateArgs, SessionId, SessionIdArg, SessionInputArgs, SessionOutputEvent,
    SessionResizeArgs, SessionRestartArgs, SessionStatus, SessionStatusEvent, SessionView,
    WorkspaceSwitchArgs, WorkspaceSwitchResult, WorkspaceValidateArgs, WorkspaceValidateResult,
    WorktreeCreateArgs, WorktreeCreateResult,
};
use crate::workspace_scope::WorkspaceScope;

pub use session::AppContext;

/// Smoke-test command used to verify the Tauri command/event scaffold is
/// wired correctly. Always returns `Ok("pong")`.
#[tauri::command]
pub async fn ping() -> Result<String, AppError> {
    Ok("pong".to_owned())
}

/// Resolve the [`ConfigStore`] for the current Tauri app instance.
///
/// **Legacy, no longer used in production boot.** Phase 6 replaced the
/// boot path with [`crate::boot::bind_workspace`], which opens a
/// per-(branch, workspace) store under the OS lock. This helper
/// remains as a diagnostic / fallback that opens a `ConfigStore`
/// rooted at the legacy `<app_data_dir>` (no isolation, no lock).
/// Do not use from new code — call `AppContext::store()` instead.
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
    Ok(ctx.store().load_config())
}

/// Deep-merges `partial` into the persisted [`AppConfig`].
///
/// Refused while a workspace switch is in progress — the swap relies on
/// no new writes landing in the *old* store between the
/// `switch_pending` bump and the actual `WorkspaceScope` swap. See
/// [`AppContext::switch_lock`] for the full barrier protocol.
#[tauri::command]
pub async fn config_set(app: tauri::AppHandle, partial: PartialAppConfig) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    // Take-then-check; matches `acquire_switch_read`. The guard is
    // held across `save_config` so the switch's `write().await` waits
    // for the persist to commit before swapping the workspace scope.
    let _switch = session::acquire_switch_read(&ctx)?;
    ctx.store().save_config(partial).map_err(AppError::from)?;
    Ok(())
}

/// Discovers and returns the list of [`InstructionSet`]s available under the
/// configured `instructionSetsDir`.
#[tauri::command]
pub async fn instructions_list(app: tauri::AppHandle) -> Result<Vec<InstructionSet>, AppError> {
    let ctx = ctx_of(&app)?;
    let cfg = ctx.store().load_config();
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
    // Workspace-switch rejection lives inside `session_close_impl` (it
    // takes a `try_read()` on `AppContext::switch_lock` for the full
    // body, including across `pool.kill().await`). Kept thin here so
    // the impl is the single source of truth for the gating policy.
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
pub async fn session_restart(
    app: tauri::AppHandle,
    args: SessionRestartArgs,
) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    session::session_restart_impl(&ctx, args)
}

/// Frontend signals that it has subscribed to `session://output` and
/// `session://status`. The first call triggers restore-on-launch (DESIGN
/// §5.5); subsequent calls are no-ops.
///
/// **Awaits restore registration** before resolving so the frontend can
/// safely fire its first `session_resize` (issued synchronously by
/// `attachToHost` → `refitEntry`) and trust that any restored session id
/// is already registered in `pending_spawn`. Without this, the
/// resize-arrives-before-restore race would silently drop the deferred
/// spawn (`pool.resize` → `NotFound`), leaving the session stuck in
/// `Starting` with no PTY child.
///
/// **Workspace-switch coordination.** We acquire an
/// `OwnedRwLockReadGuard` on [`AppContext::switch_lock`] and move it
/// into the `spawn_blocking` task that runs `restore_all_sessions`.
/// This bounds the entire restore loop by the same barrier that gates
/// every other workspace-mutating handler: a switch's `write().await`
/// cannot proceed until restore returns. We additionally check
/// [`AppContext::switch_pending`] both before and after taking the
/// owned guard (matching [`session::acquire_switch_read`]'s ordering)
/// because tokio's `try_read_owned` is permit-based and does NOT
/// reject when a writer is queued behind active readers — the counter
/// is what closes that gap. On a negative outcome we silently
/// `Ok(())`. (As of PR5, in-app workspace switches run their own
/// inline restore under the write guard and no longer rely on a
/// follow-up `frontend_ready`; this command remains for the boot-time
/// initial restore.)
#[tauri::command]
pub async fn frontend_ready(app: tauri::AppHandle) -> Result<(), AppError> {
    let ctx = ctx_of(&app)?;
    // Pre-check: cheap atomic load avoids touching the lock during a
    // switch.
    if ctx.switch_pending.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        return Ok(());
    }
    let switch_guard = match Arc::clone(&ctx.switch_lock).try_read_owned() {
        Ok(g) => g,
        Err(_) => return Ok(()),
    };
    // Post-check: closes the take-then-set race the same way
    // `acquire_switch_read` does. See [`AppContext::switch_lock`].
    if ctx.switch_pending.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        return Ok(());
    }
    if !session::frontend_ready_impl(&ctx) {
        // Already restored — drop guard and return.
        return Ok(());
    }
    let ctx_for_task = Arc::clone(&ctx);
    // `restore_all_sessions` no longer spawns PTYs (it only does
    // disk IO + HashMap inserts), so the work is bounded — but we
    // still run it on a blocking thread because materialise_temp_files
    // / cleanup_orphans / store IO can block. We *await* completion
    // here so the resolution of `frontend_ready` becomes a
    // happens-before edge for the frontend's first `session_resize`.
    tauri::async_runtime::spawn_blocking(move || {
        // Move the owned switch read guard into the closure so it
        // stays held for the full restore loop. Dropped when the
        // closure returns.
        let _switch = switch_guard;
        session::restore_all_sessions(&ctx_for_task);
    })
    .await
    .map_err(|join_err| {
        AppError::new(
            "Internal",
            format!("restore_all_sessions task panicked: {join_err}"),
        )
    })?;
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
    use tauri::Manager as _;
    let ctx = ctx_of(&app)?;
    let path = PathBuf::from(args.path);
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    session::workspace_validate_impl(&ctx, &path, Some(&app_data_dir), crate::BUILD_BRANCH)
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

/// Switch the active workspace in-place (Phase 7). Closes every open
/// session in the current workspace, releases its OS lock, acquires the
/// new workspace's lock, opens the new ConfigStore, runs
/// `restore_all_sessions` for the new workspace inline, and returns the
/// post-switch `{ config, sessions }` so the frontend can adopt
/// everything in one render. Returns `AppError::WorkspaceLocked` if
/// another Arborist instance holds the new workspace's lock.
#[tauri::command]
pub async fn workspace_switch(
    app: tauri::AppHandle,
    args: WorkspaceSwitchArgs,
) -> Result<WorkspaceSwitchResult, AppError> {
    let ctx = ctx_of(&app)?;
    let path = PathBuf::from(args.path);
    session::workspace_switch_impl(&ctx, &app, &path).await
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
/// Build a [`PtySink`](crate::pty_pool::PtySink) that bridges PTY events
/// from the [`PtyPool`](crate::pty_pool::PtyPool) into Tauri events
/// and the persisted session record.
///
/// Output / activity callbacks: pure event-emit, no store touch — the
/// closure only borrows `app: AppHandle`.
///
/// Status callback: persists `SessionStatus` and `pid` via
/// `ConfigStore::update_session_status`. Phase 7 (in-app workspace
/// switch): the closure resolves the *current* store via
/// `workspace.read()` on every invocation rather than capturing a
/// snapshot — so a status update that arrives after a workspace swap
/// writes into the new workspace's store. (This is generally a no-op
/// because the new store does not contain the old workspace's session
/// id, and `update_session_status` returns `NotFound` which is
/// intentionally swallowed below — but it is the right semantics:
/// never write into the abandoned old store.)
///
/// `update_session_status` may return `NotFound` if the wait thread
/// races `session_close` (or, post-switch, if the session belongs to
/// the old workspace). NotFound errors are intentionally swallowed.
#[must_use]
pub fn build_production_sink(
    app: tauri::AppHandle,
    workspace: Arc<RwLock<WorkspaceScope>>,
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
    let workspace_for_status = workspace;
    let status = Arc::new(
        move |session_id: &SessionId,
              status: SessionStatus,
              pid: Option<u32>,
              message: Option<String>| {
            // Re-resolve the current store on every callback so a
            // workspace switch in flight cannot cause a stale write
            // into the previously-bound store.
            let store = workspace_for_status
                .read()
                .expect("workspace lock poisoned")
                .store
                .clone();
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
/// Phase 7 (in-app workspace switch): the closure resolves the
/// *current* store via `workspace.read()` on every invocation rather
/// than capturing a snapshot. After a switch, callbacks from
/// not-yet-joined watchers will write into the new store; the matching
/// session id will not be present there and the resulting `NotFound`
/// is swallowed below. The Phase 7 switch path also calls
/// `metrics.stop_all_and_join()` before the swap to make this race
/// vanishingly small in practice.
///
/// Errors are intentionally swallowed (with a debug log) — discovery is
/// a best-effort signal that fires every metrics-watcher poll, and a
/// transient store error must not crash the watcher thread or surface
/// to the UI.
#[must_use]
pub fn build_production_ai_session_discover(
    workspace: Arc<RwLock<WorkspaceScope>>,
) -> crate::session_metrics::AiSessionDiscoveryCb {
    Arc::new(
        move |session_id: crate::types::SessionId, ai_session_id: String| {
            let store = workspace
                .read()
                .expect("workspace lock poisoned")
                .store
                .clone();
            match store.update_session_ai_session_id(&session_id, Some(ai_session_id.clone())) {
                Ok(true) => {
                    tracing::debug!(%session_id, %ai_session_id, "ai session id discovered");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!(%session_id, error = ?e, "failed to persist ai session id");
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_returns_pong() {
        let result = ping().await.expect("ping is infallible");
        assert_eq!(result, "pong");
    }
}
