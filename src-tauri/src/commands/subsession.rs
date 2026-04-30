//! Sub-session command handlers (Phase 2).
//!
//! Mirrors `commands/session.rs` in shape: each public `*_impl` is a thin
//! synchronous (or async) function taking the required contexts; the
//! `#[tauri::command]` wrappers in `commands/mod.rs` resolve the managed
//! state and forward.
//!
//! Phase 2 ships **terminal sub-tabs only**. Application-kind defs are
//! rejected with `AppError::new("NotImplemented", ...)` until Phase 3.

use std::sync::Arc;

use crate::commands::AppContext;
use crate::sub_sessions::{build_sub_session, sub_session_cwd, SubAppContext};
use crate::types::{
    AppError, CustomProcessKind, SubSession, SubSessionCreateArgs, SubSessionId, SubSessionRecord,
};

/// Create a new sub-session under `parent_session_id` using the
/// `CustomProcessDef` identified by `def_id`. Validates:
///
/// * the def exists in `AppConfig.customProcesses`
/// * the def is `enabled`
/// * the parent session is known
/// * (Phase 2) the def is `terminal` kind — application returns
///   `NotImplemented`
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
    if matches!(def.kind, CustomProcessKind::Application) {
        return Err(AppError::new(
            "NotImplemented",
            "application-kind sub-tabs land in Phase 3",
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
        return Err(AppError::from(crate::types::Error::WorktreeMissing(
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
    };
    if let Err(e) = ctx.store.append_last_open_sub_session(record) {
        sub_ctx.store.remove(&sub.id);
        return Err(AppError::from(e));
    }

    let cwd = sub_session_cwd(parent).to_path_buf();
    match sub_ctx
        .pool
        .spawn_terminal(sub.id, composed_command, cwd, sub_ctx.sink.clone())
    {
        Ok(pid) => {
            // The pool fires `Running` via the sink (which updates the
            // store in production); the in-memory record was inserted
            // above with `Starting`. Return the up-to-date snapshot so
            // the caller doesn't see a stale Starting.
            // TODO(phase-7): when the parent session is closed, cascade
            // close to all of its sub-sessions.
            let snapshot = sub_ctx.store.get(&sub.id).unwrap_or(sub.clone());
            let mut returned = snapshot;
            if returned.pid.is_none() {
                returned.pid = Some(pid);
            }
            Ok(returned)
        }
        Err(e) => {
            // Roll back both projections.
            sub_ctx.store.remove(&sub.id);
            let _ = ctx.store.remove_last_open_sub_session(&sub.id);
            Err(AppError::from(e))
        }
    }
}

/// Close a sub-session: kill the underlying child (terminal kind),
/// remove from the in-memory store, and prune the persisted record.
pub async fn subsession_close_impl(
    ctx: &AppContext,
    sub_ctx: Arc<SubAppContext>,
    id: SubSessionId,
) -> Result<(), AppError> {
    let existed = sub_ctx.store.get(&id).is_some();
    if !existed {
        return Err(AppError::new(
            "NotFound",
            format!("sub session {id} not found"),
        ));
    }
    if sub_ctx.pool.contains(&id) {
        sub_ctx.pool.kill(&id).await.map_err(AppError::from)?;
    }
    sub_ctx.store.remove(&id);
    let _ = ctx.store.remove_last_open_sub_session(&id);
    Ok(())
}

/// Focus handler — terminal kind is a frontend-only swap (no backend
/// state to update); application kind will window-focus in Phase 3. For
/// Phase 2 we just verify the id exists.
pub fn subsession_focus_impl(sub_ctx: &SubAppContext, id: SubSessionId) -> Result<(), AppError> {
    sub_ctx
        .store
        .get(&id)
        .ok_or_else(|| AppError::new("NotFound", format!("sub session {} not found", id)))?;
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

pub fn subsession_input_impl(
    sub_ctx: &SubAppContext,
    args: crate::types::SubSessionInputArgs,
) -> Result<(), AppError> {
    sub_ctx
        .pool
        .write(&args.id, args.data.as_bytes())
        .map_err(AppError::from)
}

pub fn subsession_resize_impl(
    sub_ctx: &SubAppContext,
    args: crate::types::SubSessionResizeArgs,
) -> Result<(), AppError> {
    sub_ctx
        .pool
        .resize(&args.id, args.cols, args.rows)
        .map_err(AppError::from)
}
