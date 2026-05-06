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

use tracing::{info, warn};

use crate::commands::session::acquire_switch_read;
use crate::commands::AppContext;
use crate::sub_sessions::{build_sub_session, sub_session_cwd, SubAppContext};
use crate::types::{
    AppError, CustomProcessKind, Error, SubSession, SubSessionCloseIntent, SubSessionCreateArgs,
    SubSessionId, SubSessionRecord, SubSessionStatus,
};

/// Create a new sub-session under `parent_session_id` using the
/// `CustomProcessDef` identified by `def_id`. Validates:
///
/// * the def exists in `AppConfig.customProcesses`
/// * the def is `enabled`
/// * the parent session is known
/// * the parent worktree directory still exists
/// * Phase 7: the parent is not currently mid-`session_close` cascade
///
/// Persists a [`SubSessionRecord`] into `AppConfig.lastOpenSubSessions`
/// before spawning so a crash between spawn and persist doesn't lose
/// the entry.
pub fn subsession_create_impl(
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    args: SubSessionCreateArgs,
) -> Result<SubSession, AppError> {
    // Reject while a workspace switch is queued or active. Held for the
    // entire body so the switch can't see a half-spawned sub-session.
    let _switch = acquire_switch_read(ctx)?;

    // Phase 7 race guard: refuse new children under a closing parent.
    // The tombstone is set synchronously by the `session_close` wrapper
    // before cascade, and removed via RAII guard once the parent record
    // is gone — see `commands::mod::session_close`.
    if ctx.is_parent_closing(&args.parent_session_id) {
        return Err(AppError::new(
            "InvalidArgument",
            format!(
                "parent session {} is closing; refusing to create sub-session",
                args.parent_session_id
            ),
        ));
    }

    let cfg = ctx.store().load_config();
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
    let sessions = ctx.store().load_sessions();
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
    if let Err(e) = ctx.store().append_last_open_sub_session(record) {
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
        CustomProcessKind::Application => sub_ctx.app_pool.spawn(
            sub.id,
            composed_command,
            cwd.clone(),
            sub_ctx.sink.clone(),
            owner_resolver_for(def, &cwd),
        ),
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
            let _ = ctx.store().remove_last_open_sub_session(&sub.id);
            Err(AppError::from(e))
        }
    }
}

/// Close a sub-session. Behaviour depends on `intent`:
///
/// * **Terminal kind** — `intent` is ignored; we always kill the
///   underlying PTY (the tab IS the process) and remove the record.
/// * **Application + `TabOnly`** — detach our tracking; leave the
///   external app running.
/// * **Application + `RequestAppClose`** — best-effort: post
///   `WM_CLOSE` (or platform equivalent) to the resolver-matched
///   window via [`crate::app_launcher::AppPool::request_window_close`],
///   then detach. The app may show a save-changes prompt and decline
///   to actually close — Arborist's tab is removed regardless.
/// * **Application + `ForceKill`** — `pool.kill` the underlying
///   process and remove the record. Use sparingly.
///
/// The store entry and persisted `lastOpenSubSessions` slot are
/// removed in all cases.
pub async fn subsession_close_impl(
    ctx: &AppContext,
    sub_ctx: Arc<SubAppContext>,
    id: SubSessionId,
    intent: SubSessionCloseIntent,
) -> Result<(), AppError> {
    // Reject while a workspace switch is queued or active. Held for the
    // full body (including across `pool.kill().await`) so the switch's
    // `write().await` cannot proceed until our teardown completes
    // against the old store. NOTE: the parent-cascade path uses
    // `close_for_parent_impl` directly (not this function), so adding
    // the gate here does not block parent-close cascades.
    let _switch = acquire_switch_read(ctx)?;

    let snapshot = sub_ctx
        .store
        .get(&id)
        .ok_or_else(|| AppError::new("NotFound", format!("sub session {id} not found")))?;
    match snapshot.kind {
        CustomProcessKind::Terminal => {
            if sub_ctx.pool.contains(&id) {
                match sub_ctx.pool.kill(&id).await {
                    Ok(crate::pty_pool::KillOutcome::Reaped) => {}
                    Ok(crate::pty_pool::KillOutcome::Unconfirmed { pid }) => {
                        // User clicked close — proceed with prune. Log
                        // loudly so a human can find and clean up the
                        // orphan PID. Mirrors the session-park policy:
                        // an Unconfirmed kill still issued the signal,
                        // so the failure mode is "child may linger",
                        // not "child is definitely alive".
                        tracing::warn!(
                            sub_session_id = %id,
                            pid,
                            "subsession_close: PTY kill issued but reap unconfirmed; pruning record anyway (orphan PID may need manual cleanup)"
                        );
                    }
                    Err(e) => return Err(AppError::from(e)),
                }
            }
        }
        CustomProcessKind::Application => match intent {
            SubSessionCloseIntent::TabOnly => {
                // Drop our tracking; do NOT kill the external app.
                // Rationale: a launcher like `code .` or `explorer .`
                // delegates to a long-lived GUI process the user is
                // actively interacting with. The "X" on the sub-tab is
                // tab-removal, not "close my editor".
                sub_ctx.app_pool.detach(&id);
            }
            SubSessionCloseIntent::RequestAppClose => {
                // Best-effort polite close. Errors are logged but
                // swallowed — we still want to detach the tab.
                if let Err(e) = sub_ctx
                    .app_pool
                    .request_window_close(&id, &*sub_ctx.focuser)
                {
                    tracing::warn!(
                        sub_session_id = %id,
                        error = %e,
                        "request_window_close failed; detaching tab anyway",
                    );
                }
                sub_ctx.app_pool.detach(&id);
            }
            SubSessionCloseIntent::ForceKill => {
                if sub_ctx.app_pool.contains(&id) {
                    sub_ctx.app_pool.kill(&id).map_err(AppError::from)?;
                } else {
                    sub_ctx.app_pool.detach(&id);
                }
            }
        },
    }
    sub_ctx.store.remove(&id);
    let _ = ctx.store().remove_last_open_sub_session(&id);
    Ok(())
}

/// Focus handler. Terminal kind is a frontend-only tab swap (no backend
/// state to update). Application kind delegates to
/// [`crate::app_launcher::AppPool::focus`], which prefers the
/// resolver-matched HWND (via [`crate::window_focus::WindowFocuser::focus_hwnd`])
/// before falling back to the runtime PID. If the process has exited
/// (no PID in the store), returns `Error::NotApplicable` so the
/// frontend can decide whether to relaunch (Phase 7) or just leave
/// the tab greyed.
pub fn subsession_focus_impl(
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    id: SubSessionId,
) -> Result<(), AppError> {
    // Reject while a workspace switch is queued or active. Focus can
    // race a swap of the underlying app_pool tracking.
    let _switch = acquire_switch_read(ctx)?;

    let sub = sub_ctx
        .store
        .get(&id)
        .ok_or_else(|| AppError::new("NotFound", format!("sub session {id} not found")))?;
    if matches!(sub.kind, CustomProcessKind::Application) {
        if sub.pid.is_none() {
            return Err(AppError::from(Error::NotApplicable(format!(
                "sub session {id} is not running (status: {:?})",
                sub.status
            ))));
        }
        if !matches!(sub.status, SubSessionStatus::Running) {
            return Err(AppError::from(Error::NotApplicable(format!(
                "sub session {id} status is {:?}, cannot focus",
                sub.status
            ))));
        }
        sub_ctx
            .app_pool
            .focus(&id, &*sub_ctx.focuser)
            .map_err(AppError::from)?;
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
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    args: crate::types::SubSessionInputArgs,
) -> Result<(), AppError> {
    // Reject while a workspace switch is queued or active. Writing to a
    // PTY that's about to be drained for swap would be silently lost.
    let _switch = acquire_switch_read(ctx)?;

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
/// `NotApplicable`. Mirrors `subsession_input_impl`'s switch-read
/// guard so that a resize aimed at a PTY about to be drained for a
/// workspace swap is rejected with `WorkspaceSwitchInProgress`
/// rather than silently lost — keeps the UI's reconciliation logic
/// in sync with input/relaunch.
pub fn subsession_resize_impl(
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    args: crate::types::SubSessionResizeArgs,
) -> Result<(), AppError> {
    let _switch = acquire_switch_read(ctx)?;

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

// ---------------------------------------------------------------------------
// Phase 7: parent-close cascade
// ---------------------------------------------------------------------------

/// Tear down every sub-session belonging to `parent_id`. Called from the
/// `session_close` wrapper BEFORE `session_close_impl` so the sidebar
/// can converge on "parent and its children gone" in a single round
/// trip.
///
/// Cascade rules (mirror `subsession_close_impl`):
///
/// * **Terminal**: best-effort `pool.kill()`. On a *real* PTY-kill
///   failure we keep the in-memory record + persistence + flip status to
///   `Error` — better a visible orphan than a silently-leaked PTY child.
///   `NotFound` from the pool (sub-session already exited on its own)
///   counts as success.
/// * **Application**: `app_pool.detach()` — never kill. The user's
///   editor / file browser must survive its parent session being
///   closed; same rule as the explicit `subsession_close` path.
///
/// Returns `()` — cascade is best-effort and never blocks the parent
/// close. Failures are logged via `tracing::warn`.
pub async fn close_for_parent_impl(
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    parent_id: crate::types::SessionId,
) {
    let subs = sub_ctx.store.list_for(&parent_id);
    if subs.is_empty() {
        return;
    }
    info!(
        parent_session_id = %parent_id,
        sub_count = subs.len(),
        "cascade: tearing down sub-sessions for closing parent"
    );
    for sub in subs {
        match sub.kind {
            CustomProcessKind::Terminal => {
                if sub_ctx.pool.contains(&sub.id) {
                    match sub_ctx.pool.kill(&sub.id).await {
                        Ok(crate::pty_pool::KillOutcome::Reaped) => {}
                        Err(Error::NotFound(_)) => {
                            // already exited — drop through to prune
                        }
                        Ok(crate::pty_pool::KillOutcome::Unconfirmed { pid }) => {
                            warn!(
                                parent_session_id = %parent_id,
                                sub_session_id = %sub.id,
                                pid,
                                "cascade: PTY kill issued but reap unconfirmed within grace; \
                                 keeping orphan record visible (CP-07)"
                            );
                            (sub_ctx.sink.status)(
                                &sub.id,
                                SubSessionStatus::Error,
                                Some(pid),
                                Some(format!(
                                    "PTY kill unconfirmed during parent close (pid {pid} may still be alive)"
                                )),
                            );
                            // Skip the prune — leave the orphan visible.
                            continue;
                        }
                        Err(e) => {
                            warn!(
                                parent_session_id = %parent_id,
                                sub_session_id = %sub.id,
                                error = ?e,
                                "cascade: PTY kill failed; keeping orphan record visible"
                            );
                            (sub_ctx.sink.status)(
                                &sub.id,
                                SubSessionStatus::Error,
                                None,
                                Some(format!("PTY kill failed during parent close: {e}")),
                            );
                            // Skip the prune — leave the orphan visible.
                            continue;
                        }
                    }
                }
            }
            CustomProcessKind::Application => {
                // Detach only — never kill. Closing the parent must NOT
                // terminate the user's editor / file manager.
                sub_ctx.app_pool.detach(&sub.id);
            }
        }
        sub_ctx.store.remove(&sub.id);
        if let Err(e) = ctx.store().remove_last_open_sub_session(&sub.id) {
            warn!(
                parent_session_id = %parent_id,
                sub_session_id = %sub.id,
                error = ?e,
                "cascade: persistence prune failed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 7: restore-on-launch second pass
// ---------------------------------------------------------------------------

/// Re-materialise every sub-session persisted in
/// `AppConfig.lastOpenSubSessions`. Called from the `frontend_ready`
/// wrapper AFTER `session::restore_all_sessions` so parent sessions are
/// already present in `sessions.json` (we look up worktree paths from
/// there).
///
/// Per-record handling:
///
/// * **Orphan** (parent gone OR parent currently mid-close): drop from
///   persistence and skip. The frontend will not see the row at all.
/// * **Terminal**: insert into the in-memory store (Starting), emit
///   `subsession://restored` so the frontend store inserts the row,
///   then `pool.spawn_terminal(record.composed_command)`. The pool
///   emits `Running` (or `Exited`/`Error` on failure) via the same sink
///   the production wiring uses, so the frontend status path is
///   identical to the create path.
/// * **Application**: insert into the in-memory store with status
///   `Exited` (greyed) and no PID, emit `subsession://restored`. The
///   frontend renders the tab greyed; clicking it triggers
///   `subsession_relaunch` which spawns under the same id.
///
/// On a Terminal *spawn* failure we keep the persistence record so the
/// next app launch can retry; the row is already visible via the
/// `restored` event with status `Error`.
pub fn restore_all_sub_sessions_impl(ctx: &AppContext, sub_ctx: &SubAppContext) {
    let cfg = ctx.store().load_config();
    let records = cfg.last_open_sub_sessions.clone();
    if records.is_empty() {
        return;
    }
    let sessions = ctx.store().load_sessions();

    info!(
        sub_record_count = records.len(),
        "restore: second pass for sub-sessions"
    );

    for record in records {
        // Orphan check: parent gone OR currently mid-close (the latter is
        // unlikely during cold-start restore but defensive).
        let parent = match sessions.get(&record.parent_session_id) {
            Some(p) => p,
            None => {
                warn!(
                    sub_session_id = %record.id,
                    parent_session_id = %record.parent_session_id,
                    "restore: dropping orphan sub-session (parent gone)"
                );
                let _ = ctx.store().remove_last_open_sub_session(&record.id);
                continue;
            }
        };
        if ctx.is_parent_closing(&record.parent_session_id) {
            warn!(
                sub_session_id = %record.id,
                parent_session_id = %record.parent_session_id,
                "restore: dropping sub-session under closing parent"
            );
            let _ = ctx.store().remove_last_open_sub_session(&record.id);
            continue;
        }

        // Build the in-memory entry. Application kind starts greyed
        // because we never auto-relaunch external editors at startup —
        // the user clicks to bring them back.
        let initial_status = match record.kind {
            CustomProcessKind::Terminal => SubSessionStatus::Starting,
            CustomProcessKind::Application => SubSessionStatus::Exited,
        };
        let sub = SubSession {
            id: record.id,
            parent_session_id: record.parent_session_id,
            def_id: record.def_id.clone(),
            kind: record.kind,
            label: record.label.clone(),
            status: initial_status,
            pid: None,
            composed_command: record.composed_command.clone(),
            created_at: now_unix_seconds(),
        };

        if let Err(e) = sub_ctx.store.insert(sub.clone()) {
            warn!(
                sub_session_id = %record.id,
                error = ?e,
                "restore: store insert failed; skipping"
            );
            continue;
        }
        // Emit `restored` BEFORE any subsequent status event so the
        // frontend store has the row when status arrives.
        (sub_ctx.sink.restored)(&sub);

        match record.kind {
            CustomProcessKind::Terminal => {
                let cwd = sub_session_cwd(parent).to_path_buf();
                match sub_ctx.pool.spawn_terminal(
                    record.id,
                    record.composed_command.clone(),
                    cwd,
                    sub_ctx.sink.clone(),
                ) {
                    Ok(pid) => {
                        info!(sub_session_id = %record.id, pid, "restore: terminal sub-session respawned");
                    }
                    Err(e) => {
                        warn!(
                            sub_session_id = %record.id,
                            error = ?e,
                            "restore: terminal sub-session respawn failed; keeping persistence record so user can retry next launch"
                        );
                        // Surface failure to the frontend store. The
                        // `restored` event already arrived with status
                        // Starting; this Error transition flips the row
                        // to the visible failure state.
                        (sub_ctx.sink.status)(
                            &record.id,
                            SubSessionStatus::Error,
                            None,
                            Some(format!("respawn failed: {e}")),
                        );
                    }
                }
            }
            CustomProcessKind::Application => {
                // Greyed: nothing to spawn. Click → relaunch.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 7: relaunch
// ---------------------------------------------------------------------------

/// Re-spawn a sub-session under its existing id. Used by the frontend
/// when the user clicks a greyed Application sub-tab (status `exited`
/// or `error`); also valid for Terminal sub-tabs whose PTY died for any
/// reason. Persisted record is unchanged (id stable; composedCommand is
/// **re-derived** from the current def so user edits to the
/// `customProcesses` list take effect on relaunch).
pub async fn subsession_relaunch_impl(
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    id: SubSessionId,
) -> Result<SubSession, AppError> {
    // Reject while a workspace switch is queued or active. Held for the
    // full body (including across `pool.kill().await` + spawn) so the
    // new child can't end up bound to the old workspace's CWD after a
    // mid-flight swap.
    let _switch = acquire_switch_read(ctx)?;

    let existing = sub_ctx
        .store
        .get(&id)
        .ok_or_else(|| AppError::new("NotFound", format!("sub session {id} not found")))?;

    // Look up the def fresh — relaunch picks up any post-creation edits
    // the user made via the Settings tab. If the def has been deleted
    // we refuse rather than spawning an empty command.
    let cfg = ctx.store().load_config();
    let def = cfg
        .custom_processes
        .iter()
        .find(|d| d.id == existing.def_id)
        .ok_or_else(|| {
            AppError::new(
                "NotFound",
                format!(
                    "custom process def {:?} no longer exists; cannot relaunch",
                    existing.def_id
                ),
            )
        })?
        .clone();
    if !def.enabled {
        return Err(AppError::new(
            "InvalidArgument",
            format!("custom process def {:?} is disabled", existing.def_id),
        ));
    }

    // Look up the parent session for its worktree path.
    let sessions = ctx.store().load_sessions();
    let parent = sessions.get(&existing.parent_session_id).ok_or_else(|| {
        AppError::new(
            "NotFound",
            format!("parent session {} not found", existing.parent_session_id),
        )
    })?;
    if !parent.worktree_path.is_dir() {
        return Err(AppError::from(Error::WorktreeMissing(
            parent.worktree_path.clone(),
        )));
    }
    if ctx.is_parent_closing(&existing.parent_session_id) {
        return Err(AppError::new(
            "InvalidArgument",
            format!(
                "parent session {} is closing; refusing to relaunch sub-session",
                existing.parent_session_id
            ),
        ));
    }

    // Tear down the old child (best-effort — Application has already
    // exited if status is exited/error; Terminal may still have a PTY
    // process around).
    match existing.kind {
        CustomProcessKind::Terminal => {
            if sub_ctx.pool.contains(&id) {
                match sub_ctx.pool.kill(&id).await {
                    Ok(crate::pty_pool::KillOutcome::Reaped) | Err(Error::NotFound(_)) => {}
                    Ok(crate::pty_pool::KillOutcome::Unconfirmed { pid }) => {
                        warn!(
                            sub_session_id = %id,
                            pid,
                            "relaunch: pre-kill issued but reap unconfirmed; continuing (orphan PID may need manual cleanup)"
                        );
                    }
                    Err(e) => {
                        warn!(sub_session_id = %id, error = ?e, "relaunch: pre-kill failed (continuing)");
                    }
                }
            }
        }
        CustomProcessKind::Application => {
            sub_ctx.app_pool.detach(&id);
        }
    }

    // Persisted composed_command is refreshed from the current def so
    // edits to the def (rename, command change) take effect.
    let composed_command = def.command.clone();

    // Reset status synchronously and update persistence so a crash
    // between here and spawn doesn't leave a stale exited row.
    let mut refreshed = existing.clone();
    refreshed.status = SubSessionStatus::Starting;
    refreshed.pid = None;
    refreshed.composed_command = composed_command.clone();
    refreshed.label = def.name.clone();
    sub_ctx.store.remove(&id);
    sub_ctx
        .store
        .insert(refreshed.clone())
        .map_err(AppError::from)?;
    let updated_record = SubSessionRecord {
        id: refreshed.id,
        parent_session_id: refreshed.parent_session_id,
        def_id: refreshed.def_id.clone(),
        kind: refreshed.kind,
        label: refreshed.label.clone(),
        composed_command: refreshed.composed_command.clone(),
    };
    if let Err(e) = ctx.store().append_last_open_sub_session(updated_record) {
        // The on-disk record is unchanged (write_atomic is atomic), but
        // we already tore down the prior child above. Roll the in-memory
        // entry back to `existing` with status=Error so the row stays
        // visible (CP-07 spirit: a visible orphan beats a silent leak)
        // and the user can retry via relaunch. Mirrors the create-path
        // rollback at `subsession_create_impl`.
        warn!(sub_session_id = %id, error = ?e, "relaunch: persistence refresh failed; rolling back in-memory state");
        sub_ctx.store.remove(&id);
        let mut rollback = existing.clone();
        rollback.status = SubSessionStatus::Error;
        rollback.pid = None;
        let _ = sub_ctx.store.insert(rollback);
        (sub_ctx.sink.status)(
            &id,
            SubSessionStatus::Error,
            None,
            Some(format!("relaunch persistence failed: {e}")),
        );
        return Err(AppError::from(e));
    }
    (sub_ctx.sink.status)(&id, SubSessionStatus::Starting, None, None);

    let cwd = sub_session_cwd(parent).to_path_buf();
    let spawn_result = match def.kind {
        CustomProcessKind::Terminal => {
            sub_ctx
                .pool
                .spawn_terminal(id, composed_command, cwd, sub_ctx.sink.clone())
        }
        CustomProcessKind::Application => sub_ctx.app_pool.spawn(
            id,
            composed_command,
            cwd.clone(),
            sub_ctx.sink.clone(),
            owner_resolver_for(&def, &cwd),
        ),
    };

    match spawn_result {
        Ok(pid) => {
            let snapshot = sub_ctx.store.get(&id).unwrap_or(refreshed);
            let mut returned = snapshot;
            if returned.pid.is_none() {
                returned.pid = Some(pid);
            }
            Ok(returned)
        }
        Err(e) => {
            // Surface as Error but KEEP the row + persistence so user can
            // retry. Mirrors `restore_all_sub_sessions_impl` failure path.
            (sub_ctx.sink.status)(
                &id,
                SubSessionStatus::Error,
                None,
                Some(format!("relaunch failed: {e}")),
            );
            Err(AppError::from(e))
        }
    }
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|_| {
            tracing::warn!("system clock before UNIX epoch; using 0");
            0
        })
}

/// Build the [`crate::app_launcher::OwnerResolver`] (if any) appropriate
/// for the given def. Detection is by **command shape**, not def id —
/// a user-defined "VSCode" entry with command `code .` gets the same
/// re-discovery treatment as the built-in `vscode` def. See
/// [`crate::vscode_owner::looks_like_vscode_command`] for the matching
/// rules and `vscode_owner.rs` for the re-discovery strategy itself.
///
/// Returns `None` for every other def: most app launchers spawn a
/// child the user identifies with directly (`open`, `explorer`,
/// `gimp`, etc.) so the launcher PID IS the long-lived process.
fn owner_resolver_for(
    def: &crate::types::CustomProcessDef,
    cwd: &std::path::Path,
) -> Option<Arc<dyn crate::app_launcher::OwnerResolver>> {
    if crate::vscode_owner::looks_like_vscode_command(&def.command) {
        return Some(Arc::new(crate::vscode_owner::VsCodeOwnerResolver::new(
            cwd.to_path_buf(),
        )));
    }
    None
}
