//! Sub-session command handlers (Phases 2 + 3).
//!
//! Mirrors `commands/session.rs` in shape: each public `*_impl` is a thin
//! synchronous (or async) function taking the required contexts; the
//! `#[tauri::command]` wrappers in `commands/mod.rs` resolve the managed
//! state and forward.
//!
//! Two sub-session flavours:
//!
//! * **Terminal** (Phase 2) — owned by [`SubPtyPool`] in `sub_sessions`.
//!   PTY allocated; output streams over `session://output`.
//! * **Application** (Phase 3) — owned by
//!   [`crate::app_launcher::AppPool`]. No PTY; lifecycle limited to
//!   spawn / wait / kill. `subsession_focus` delegates to a
//!   [`crate::window_focus::WindowFocuser`].

use std::sync::Arc;

use crate::commands::AppContext;
use crate::sub_sessions::{build_sub_session, sub_session_cwd, SubAppContext};
use crate::types::{
    AppError, CustomProcessKind, Error, SubSession, SubSessionCreateArgs, SubSessionId,
    SubSessionRecord, SubSessionStatus,
};

/// Create a new sub-session under `parent_session_id` using the
/// `CustomProcessDef` identified by `def_id`. Validates:
///
/// * the def exists in `AppConfig.customProcesses`
/// * the def is `enabled`
/// * the parent session is known
/// * the parent worktree directory still exists
///
/// Persists a [`SubSessionRecord`] into `AppConfig.lastOpenSubSessions`
/// before spawning so a crash between spawn and persist doesn't lose
/// the entry.
pub fn subsession_create_impl(
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    args: SubSessionCreateArgs,
) -> Result<SubSession, AppError> {
    let cfg = ctx.store.load_config();
    let def = cfg
        .custom_processes
        .iter()
        .find(|d| d.id == args.def_id)
        .ok_or_else(|| {
            AppError::new(
                "NotFound",
                format!("custom process def {:?} not found", args.def_id),
            )
        })?;
    if !def.enabled {
        return Err(AppError::new(
            "InvalidArgument",
            format!("custom process def {:?} is disabled", args.def_id),
        ));
    }

    // Look up the parent session for its worktree path.
    let sessions = ctx.store.load_sessions();
    let parent = sessions.get(&args.parent_session_id).ok_or_else(|| {
        AppError::new(
            "NotFound",
            format!("parent session {} not found", args.parent_session_id),
        )
    })?;

    // Validate the parent worktree still exists before doing anything
    // destructive — otherwise the user gets a low-level PtySpawnFailed
    // instead of the dedicated `WorktreeMissing` error.
    if !parent.worktree_path.is_dir() {
        return Err(AppError::from(Error::WorktreeMissing(
            parent.worktree_path.clone(),
        )));
    }

    // Compose once, store-and-reuse (DESIGN §5.4 mirror).
    let composed_command = def.command.clone();
    let sub = build_sub_session(parent.id, def, composed_command.clone());

    // Insert into the in-memory store FIRST. If that fails the persist
    // step is never reached, so we can't orphan a record. If the
    // subsequent persist fails we roll back the in-memory insert.
    sub_ctx.store.insert(sub.clone()).map_err(AppError::from)?;

    let record = SubSessionRecord {
        id: sub.id,
        parent_session_id: sub.parent_session_id,
        def_id: sub.def_id.clone(),
        kind: sub.kind,
        label: sub.label.clone(),
        composed_command: sub.composed_command.clone(),
    };
    if let Err(e) = ctx.store.append_last_open_sub_session(record) {
        sub_ctx.store.remove(&sub.id);
        return Err(AppError::from(e));
    }

    let cwd = sub_session_cwd(parent).to_path_buf();

    // Branch on kind: terminal → SubPtyPool, application → AppPool.
    let spawn_result = match def.kind {
        CustomProcessKind::Terminal => {
            sub_ctx
                .pool
                .spawn_terminal(sub.id, composed_command, cwd, sub_ctx.sink.clone())
        }
        CustomProcessKind::Application => {
            sub_ctx
                .app_pool
                .spawn(sub.id, composed_command, cwd, sub_ctx.sink.clone())
        }
    };

    match spawn_result {
        Ok(pid) => {
            let snapshot = sub_ctx.store.get(&sub.id).unwrap_or(sub.clone());
            let mut returned = snapshot;
            if returned.pid.is_none() {
                returned.pid = Some(pid);
            }
            Ok(returned)
        }
        Err(e) => {
            sub_ctx.store.remove(&sub.id);
            let _ = ctx.store.remove_last_open_sub_session(&sub.id);
            Err(AppError::from(e))
        }
    }
}

/// Close a sub-session: for terminal kind, kill the PTY; for
/// application kind, drop our tracking of it (we deliberately do **not**
/// kill the external app — closing the tab should not terminate the
/// user's editor / file browser). Always removes the in-memory store
/// entry and prunes the persisted record.
pub async fn subsession_close_impl(
    ctx: &AppContext,
    sub_ctx: Arc<SubAppContext>,
    id: SubSessionId,
) -> Result<(), AppError> {
    let snapshot = sub_ctx
        .store
        .get(&id)
        .ok_or_else(|| AppError::new("NotFound", format!("sub session {id} not found")))?;
    match snapshot.kind {
        CustomProcessKind::Terminal => {
            if sub_ctx.pool.contains(&id) {
                sub_ctx.pool.kill(&id).await.map_err(AppError::from)?;
            }
        }
        CustomProcessKind::Application => {
            // Drop our tracking; do NOT kill the external app.
            // Rationale: a launcher like `code .` or `explorer .`
            // delegates to a long-lived GUI process the user is
            // actively interacting with. The "X" on the sub-tab is
            // tab-removal, not "close my editor".
            sub_ctx.app_pool.detach(&id);
        }
    }
    sub_ctx.store.remove(&id);
    let _ = ctx.store.remove_last_open_sub_session(&id);
    Ok(())
}

/// Focus handler. Terminal kind is a frontend-only tab swap (no backend
/// state to update). Application kind delegates to the configured
/// [`crate::window_focus::WindowFocuser`] using the live PID; if the
/// process has exited (no PID in the store), returns
/// `Error::NotApplicable` so the frontend can decide whether to
/// relaunch (Phase 7) or just leave the tab greyed.
pub fn subsession_focus_impl(sub_ctx: &SubAppContext, id: SubSessionId) -> Result<(), AppError> {
    let sub = sub_ctx
        .store
        .get(&id)
        .ok_or_else(|| AppError::new("NotFound", format!("sub session {id} not found")))?;
    if matches!(sub.kind, CustomProcessKind::Application) {
        let pid = sub.pid.ok_or_else(|| {
            AppError::from(Error::NotApplicable(format!(
                "sub session {id} is not running (status: {:?})",
                sub.status
            )))
        })?;
        if !matches!(sub.status, SubSessionStatus::Running) {
            return Err(AppError::from(Error::NotApplicable(format!(
                "sub session {id} status is {:?}, cannot focus",
                sub.status
            ))));
        }
        sub_ctx.focuser.focus_pid(pid).map_err(AppError::from)?;
    }
    Ok(())
}

/// List sub-sessions, optionally filtered to a parent.
pub fn subsession_list_impl(
    sub_ctx: &SubAppContext,
    parent: Option<crate::types::SessionId>,
) -> Result<Vec<SubSession>, AppError> {
    Ok(match parent {
        Some(p) => sub_ctx.store.list_for(&p),
        None => sub_ctx.store.list_all(),
    })
}

/// Send PTY input. Application sub-sessions have no PTY — return
/// `NotApplicable` so the frontend can present a clear error if it
/// accidentally routes input there.
pub fn subsession_input_impl(
    sub_ctx: &SubAppContext,
    args: crate::types::SubSessionInputArgs,
) -> Result<(), AppError> {
    if let Some(sub) = sub_ctx.store.get(&args.id) {
        if matches!(sub.kind, CustomProcessKind::Application) {
            return Err(AppError::from(Error::NotApplicable(
                "application sub-sessions do not accept PTY input".into(),
            )));
        }
    }
    sub_ctx
        .pool
        .write(&args.id, args.data.as_bytes())
        .map_err(AppError::from)
}

/// Resize a PTY. Application sub-sessions have no PTY — return
/// `NotApplicable`.
pub fn subsession_resize_impl(
    sub_ctx: &SubAppContext,
    args: crate::types::SubSessionResizeArgs,
) -> Result<(), AppError> {
    if let Some(sub) = sub_ctx.store.get(&args.id) {
        if matches!(sub.kind, CustomProcessKind::Application) {
            return Err(AppError::from(Error::NotApplicable(
                "application sub-sessions have no PTY to resize".into(),
            )));
        }
    }
    sub_ctx
        .pool
        .resize(&args.id, args.cols, args.rows)
        .map_err(AppError::from)
}
