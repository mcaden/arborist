//! Sub-session command handlers (Phases 2 + 3).
//!
//! Mirrors `commands/session.rs` in shape: each public `*_impl` is a thin synchronous (or async) function taking the required contexts; the
//! `#[tauri::command]` wrappers in `commands/mod.rs` resolve the managed state and forward.
//!
//! Two sub-session flavours:
//!
//! * **Terminal** (Phase 2) — owned by [`SubPtyPool`] in `sub_sessions`. PTY
//!   allocated; output streams over `session://output`.
//! * **Application** (Phase 3) — owned by [`crate::app_launcher::AppPool`]. No
//!   PTY; lifecycle limited to spawn / wait / kill. `subsession_focus`
//!   delegates to a [`crate::window_focus::WindowFocuser`].

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{sleep, Instant};
use tracing::{info, warn};

use crate::commands::session::acquire_switch_read;
use crate::commands::worktree_tab::clear_active_child_in_config;
use crate::commands::AppContext;
use crate::sub_sessions::{build_sub_session, sub_session_cwd, SubAppContext};
use crate::types::{
    AppError, ChildId, CustomProcessKind, Error, PartialAppConfig, SubSession, SubSessionCloseIntent, SubSessionCreateArgs, SubSessionId,
    SubSessionRecord, SubSessionStatus, WorktreeTabAppClosePolicy, WorktreeTabId,
};

/// Create a new sub-session under `parent_worktree_tab_id` using the `CustomProcessDef` identified by `def_id`. Validates:
///
/// * the def exists in `AppConfig.customProcesses`
/// * the def is `enabled`
/// * the parent worktree tab is known and its directory still exists
/// * the worktree tab is not currently mid-close
///
/// Persists a [`SubSessionRecord`] into `AppConfig.lastOpenSubSessions` before spawning so a crash between spawn and persist doesn't lose the entry.
pub fn subsession_create_impl(ctx: &AppContext, sub_ctx: &SubAppContext, args: SubSessionCreateArgs) -> Result<SubSession, AppError> {
    // Reject while a workspace switch is queued or active. Held for the entire body so the switch can't see a half-spawned sub-session.
    let _switch = acquire_switch_read(ctx)?;

    // Race guard: refuse new children under a closing worktree tab. The tombstone is set synchronously by the `worktree_tab_close` wrapper before
    // cascade, and removed via RAII guard once the tab record is gone.
    if ctx.is_worktree_tab_closing(&args.parent_worktree_tab_id) {
        return Err(AppError::new(
            "InvalidArgument",
            format!("worktree tab {} is closing; refusing to create sub-session", args.parent_worktree_tab_id),
        ));
    }

    let cfg = ctx.store().load_config();
    let def = cfg
        .custom_processes
        .iter()
        .find(|d| d.id == args.def_id)
        .ok_or_else(|| AppError::new("NotFound", format!("custom process def {:?} not found", args.def_id)))?;
    if !def.enabled {
        return Err(AppError::new(
            "InvalidArgument",
            format!("custom process def {:?} is disabled", args.def_id),
        ));
    }

    // Look up the worktree tab for its path.
    let tab = cfg
        .worktree_tabs
        .iter()
        .find(|t| t.id == args.parent_worktree_tab_id)
        .ok_or_else(|| AppError::new("NotFound", format!("worktree tab {} not found", args.parent_worktree_tab_id)))?;

    // Validate the worktree directory still exists before doing anything destructive — otherwise the user gets a low-level PtySpawnFailed instead of
    // the dedicated `WorktreeMissing` error.
    if !tab.path.is_dir() {
        return Err(AppError::from(Error::WorktreeMissing(tab.path.clone())));
    }

    // Compose once, store-and-reuse.
    let composed_command = def.command.clone();
    let sub = build_sub_session(args.parent_worktree_tab_id, def, composed_command.clone());

    // Insert into the in-memory store FIRST. If that fails the persist step is never reached, so we can't orphan a record. If the subsequent persist
    // fails we roll back the in-memory insert.
    sub_ctx.store.insert(sub.clone()).map_err(AppError::from)?;

    let record = SubSessionRecord {
        id: sub.id,
        parent_session_id: None,
        parent_worktree_tab_id: Some(sub.parent_worktree_tab_id),
        def_id: sub.def_id.clone(),
        kind: sub.kind,
        label: sub.label.clone(),
        composed_command: sub.composed_command.clone(),
    };
    if let Err(e) = ctx.store().append_last_open_sub_session(record) {
        sub_ctx.store.remove(&sub.id);
        return Err(AppError::from(e));
    }

    let cwd = sub_session_cwd(&tab.path).to_path_buf();

    // Branch on kind: terminal → SubPtyPool, application → AppPool.
    let spawn_result = match def.kind {
        CustomProcessKind::Terminal => sub_ctx.pool.spawn_terminal(sub.id, composed_command, cwd, sub_ctx.sink.clone()),
        CustomProcessKind::Application => sub_ctx.app_pool.spawn(
            sub.id,
            composed_command,
            cwd.clone(),
            sub_ctx.sink.clone(),
            owner_resolver_for(sub_ctx.registry.as_ref(), def, &cwd),
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
/// * **Terminal kind** — `intent` is ignored; we always kill the underlying PTY
///   (the tab IS the process) and remove the record.
/// * **Application + `TabOnly`** — detach our tracking; leave the external app
///   running.
/// * **Application + `RequestAppClose`** — best-effort: post `WM_CLOSE` (or
///   platform equivalent) to the resolver-matched window via
///   [`crate::app_launcher::AppPool::request_window_close`], then detach. The
///   app may show a save-changes prompt and decline to actually close —
///   Arborist's tab is removed regardless.
/// * **Application + `ForceKill`** — `pool.kill` the underlying process and
///   remove the record. Use sparingly.
///
/// The store entry and persisted `lastOpenSubSessions` slot are removed in all cases.
pub async fn subsession_close_impl(
    ctx: &AppContext,
    sub_ctx: Arc<SubAppContext>,
    id: SubSessionId,
    intent: SubSessionCloseIntent,
) -> Result<(), AppError> {
    // Reject while a workspace switch is queued or active. Held for the full body (including across `pool.kill().await`) so the switch's
    // `write().await` cannot proceed until our teardown completes against the old store. NOTE: the parent-cascade path uses `close_for_parent_impl`
    // directly (not this function), so adding the gate here does not block parent-close cascades.
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
                        // User clicked close — proceed with prune. Log loudly so a human can find and clean up the orphan PID. Mirrors the
                        // session-park policy: an Unconfirmed kill still issued the signal, so the failure mode is "child may linger", not "child is
                        // definitely alive".
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
                // Drop our tracking; do NOT kill the external app. Rationale: a launcher like `code .` or `explorer .` delegates to a long-lived GUI
                // process the user is actively interacting with. The "X" on the sub-tab is tab-removal, not "close my editor".
                sub_ctx.app_pool.detach(&id);
            }
            SubSessionCloseIntent::RequestAppClose => {
                // Best-effort polite close. Errors are logged but swallowed — we still want to detach the tab.
                if let Err(e) = sub_ctx.app_pool.request_window_close(&id, &*sub_ctx.focuser) {
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
    // Best-effort config cleanup AFTER the irreversible kill + in-memory removal above. We *log and continue* instead of propagating, by design:
    //
    //   * Returning `Err` here would surface "close failed" to the frontend even though the sub-session is already gone (PTY killed, runtime
    //     dropped). Worse, the close is non-retryable from the user's perspective — re-issuing `subsession_close` would now hit a NotFound because
    //     the in-memory store no longer contains the record.
    //   * Round-7's `?` propagation tried to prevent a "closed in memory but visible after restart" anomaly. That reasoning was incomplete:
    //     `restore_all_sub_sessions_impl`'s `forget_sub_session` only fires for *orphan* / *closing-parent* records, not for sub-sessions whose
    //     parent session is still present. So if this write fails AND the parent is alive, the sub *can* genuinely re-spawn on next launch.
    //     That's a real failure mode of best-effort persistence — we accept it because the alternative (Err on a completed close) is worse UX
    //     and equally non-recoverable.
    //   * Stale `WorktreeTab.active_child_id` pointers can likewise survive on write failure; they're cleaned up the next time the tab's active
    //     child changes (or by future restore-time reconciliation, which is not implemented today).
    if let Err(e) = ctx.store().save_config_with(PartialAppConfig::default(), |cfg| {
        cfg.last_open_sub_sessions.retain(|r| r.id != id);
        clear_active_child_in_config(cfg, ChildId::SubSession(id));
        true
    }) {
        warn!(
            sub_session_id = %id,
            error = %e,
            "subsession_close: best-effort config cleanup failed; close still completed (in-memory state cleared, kill complete). \
             Stale `last_open_sub_sessions` row may re-spawn on next launch if parent session is still alive; stale `active_child_id` \
             may persist on a worktree tab until the next focus change.",
        );
    }
    Ok(())
}

/// Focus handler. Terminal kind is a frontend-only tab swap (no backend state to update). Application kind delegates to
/// [`crate::app_launcher::AppPool::focus`], which prefers the
/// resolver-matched HWND (via
/// [`crate::window_focus::WindowFocuser::focus_hwnd`]) before falling back to
/// the runtime PID. If the process has exited (no PID in the store), returns `Error::NotApplicable` so the frontend can decide whether to relaunch
/// (Phase 7) or just leave the tab greyed.
pub fn subsession_focus_impl(ctx: &AppContext, sub_ctx: &SubAppContext, id: SubSessionId) -> Result<(), AppError> {
    // Reject while a workspace switch is queued or active. Focus can race a swap of the underlying app_pool tracking.
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
        sub_ctx.app_pool.focus(&id, &*sub_ctx.focuser).map_err(AppError::from)?;
    }
    Ok(())
}

/// List sub-sessions, optionally filtered to a worktree tab.
pub fn subsession_list_impl(sub_ctx: &SubAppContext, parent_worktree_tab_id: Option<WorktreeTabId>) -> Result<Vec<SubSession>, AppError> {
    Ok(match parent_worktree_tab_id {
        Some(tab_id) => sub_ctx.store.list_for_worktree_tab(&tab_id),
        None => sub_ctx.store.list_all(),
    })
}

/// Send PTY input. Application sub-sessions have no PTY — return `NotApplicable` so the frontend can present a clear error if it accidentally routes
/// input there.
pub fn subsession_input_impl(ctx: &AppContext, sub_ctx: &SubAppContext, args: crate::types::SubSessionInputArgs) -> Result<(), AppError> {
    // Reject while a workspace switch is queued or active. Writing to a PTY that's about to be drained for swap would be silently lost.
    let _switch = acquire_switch_read(ctx)?;

    if let Some(sub) = sub_ctx.store.get(&args.id) {
        if matches!(sub.kind, CustomProcessKind::Application) {
            return Err(AppError::from(Error::NotApplicable(
                "application sub-sessions do not accept PTY input".into(),
            )));
        }
    }
    sub_ctx.pool.write(&args.id, args.data.as_bytes()).map_err(AppError::from)
}

/// Resize a PTY. Application sub-sessions have no PTY — return `NotApplicable`. Mirrors `subsession_input_impl`'s switch-read guard so that a resize
/// aimed at a PTY about to be drained for a workspace swap is rejected with `WorkspaceSwitchInProgress` rather than silently lost — keeps the UI's
/// reconciliation logic in sync with input/relaunch.
pub fn subsession_resize_impl(ctx: &AppContext, sub_ctx: &SubAppContext, args: crate::types::SubSessionResizeArgs) -> Result<(), AppError> {
    let _switch = acquire_switch_read(ctx)?;

    if let Some(sub) = sub_ctx.store.get(&args.id) {
        if matches!(sub.kind, CustomProcessKind::Application) {
            return Err(AppError::from(Error::NotApplicable(
                "application sub-sessions have no PTY to resize".into(),
            )));
        }
    }
    sub_ctx.pool.resize(&args.id, args.cols, args.rows).map_err(AppError::from)
}

// --------------------------------------------------------------------------- Worktree-tab-close cascade
// ---------------------------------------------------------------------------

const APP_CLOSE_GRACE: Duration = Duration::from_millis(600);
const APP_CLOSE_POLL: Duration = Duration::from_millis(25);

/// Tear down every sub-session belonging to `tab_id`. Called from the `worktree_tab_close` wrapper BEFORE closing child agent sessions so the
/// sidebar can converge on "tab and all its children gone" in a single round trip.
///
/// Cascade rules (mirror `subsession_close_impl`):
///
/// * **Terminal**: best-effort `pool.kill()`. On a *real* PTY-kill failure we
///   keep the in-memory record + persistence + flip status to `Error` — better
///   a visible orphan than a silently-leaked PTY child. `NotFound` from the
///   pool (sub-session already exited on its own) counts as success.
/// * **Application**: obey `app_close_policy` (`Detach` vs `Terminate`).
///
/// Returns a best-effort list of per-child failures to surface via
/// `WorktreeTabCloseResult.childErrors`. The cascade always continues.
pub async fn close_for_worktree_tab_impl(
    ctx: &AppContext,
    sub_ctx: &SubAppContext,
    tab_id: crate::types::WorktreeTabId,
    app_close_policy: WorktreeTabAppClosePolicy,
) -> Vec<String> {
    let subs = sub_ctx.store.list_for_worktree_tab(&tab_id);
    if subs.is_empty() {
        return Vec::new();
    }
    info!(
        worktree_tab_id = %tab_id,
        sub_count = subs.len(),
        "cascade: tearing down sub-sessions for closing worktree tab"
    );

    // Pre-pass: clear any worktree-tab `active_child_id` referencing a sub of the closing tab. Done once up-front so we take
    // the config write lock at most once for this concern.
    let sub_ids: Vec<SubSessionId> = subs.iter().map(|s| s.id).collect();
    let cascade_set: HashSet<SubSessionId> = sub_ids.iter().copied().collect();
    let needs_cleanup = ctx
        .store()
        .load_config()
        .worktree_tabs
        .iter()
        .any(|t| matches!(t.active_child_id, Some(ChildId::SubSession(sid)) if cascade_set.contains(&sid)));
    if needs_cleanup {
        if let Err(e) = ctx.store().save_config_with(PartialAppConfig::default(), |cfg| {
            let mut changed = false;
            for sid in &sub_ids {
                changed |= clear_active_child_in_config(cfg, ChildId::SubSession(*sid));
            }
            changed
        }) {
            warn!(
                worktree_tab_id = %tab_id,
                error = ?e,
                "cascade: active_child_id cleanup failed (continuing — orphan tab pointers may persist)"
            );
        }
    }

    let mut child_errors: Vec<String> = Vec::new();
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
                                worktree_tab_id = %tab_id,
                                sub_session_id = %sub.id,
                                pid,
                                "cascade: PTY kill issued but reap unconfirmed within grace; \
                                 keeping orphan record visible (CP-07)"
                            );
                            (sub_ctx.sink.status)(
                                &sub.id,
                                SubSessionStatus::Error,
                                Some(pid),
                                Some(format!("PTY kill unconfirmed during worktree tab close (pid {pid} may still be alive)")),
                            );
                            child_errors.push(format!("sub-session {}: PTY kill unconfirmed (pid {pid} may still be alive)", sub.id));
                            continue;
                        }
                        Err(e) => {
                            warn!(
                                worktree_tab_id = %tab_id,
                                sub_session_id = %sub.id,
                                error = ?e,
                                "cascade: PTY kill failed; keeping orphan record visible"
                            );
                            (sub_ctx.sink.status)(
                                &sub.id,
                                SubSessionStatus::Error,
                                None,
                                Some(format!("PTY kill failed during worktree tab close: {e}")),
                            );
                            child_errors.push(format!("sub-session {}: PTY kill failed: {e}", sub.id));
                            continue;
                        }
                    }
                }
            }
            CustomProcessKind::Application => {
                match app_close_policy {
                    WorktreeTabAppClosePolicy::Detach => {
                        sub_ctx.app_pool.detach(&sub.id);
                    }
                    WorktreeTabAppClosePolicy::Terminate => {
                        if sub_ctx.app_pool.contains(&sub.id) {
                            match sub_ctx.app_pool.request_window_close(&sub.id, &*sub_ctx.focuser) {
                                Ok(()) => {
                                    let deadline = Instant::now() + APP_CLOSE_GRACE;
                                    while sub_ctx.app_pool.contains(&sub.id) && Instant::now() < deadline {
                                        sleep(APP_CLOSE_POLL).await;
                                    }
                                }
                                Err(Error::NotFound(_)) | Err(Error::Unsupported(_)) => {
                                    // No close-capable window target on this runtime/platform; may still be killable below.
                                }
                                Err(e) => {
                                    warn!(
                                        worktree_tab_id = %tab_id,
                                        sub_session_id = %sub.id,
                                        error = ?e,
                                        "cascade: request_window_close failed for app sub-session"
                                    );
                                    child_errors.push(format!("sub-session {}: graceful app-close request failed: {e}", sub.id));
                                }
                            }
                        }

                        if sub_ctx.app_pool.contains(&sub.id) {
                            if matches!(sub_ctx.app_pool.is_retargeted(&sub.id), Some(true)) {
                                sub_ctx.app_pool.detach(&sub.id);
                                child_errors.push(format!(
                                    "sub-session {}: detached instead of force-killing a re-targeted/shared app owner",
                                    sub.id
                                ));
                            } else if let Err(e) = sub_ctx.app_pool.kill(&sub.id) {
                                warn!(
                                    worktree_tab_id = %tab_id,
                                    sub_session_id = %sub.id,
                                    error = ?e,
                                    "cascade: app terminate failed"
                                );
                                child_errors.push(format!("sub-session {}: app terminate failed: {e}", sub.id));
                            }
                        }
                    }
                }
            }
        }
        sub_ctx.store.remove(&sub.id);
        if let Err(e) = ctx.store().remove_last_open_sub_session(&sub.id) {
            warn!(
                worktree_tab_id = %tab_id,
                sub_session_id = %sub.id,
                error = ?e,
                "cascade: persistence prune failed"
            );
        }
    }
    child_errors
}

// --------------------------------------------------------------------------- Phase 7: restore-on-launch second pass
// ---------------------------------------------------------------------------

/// Drop a sub-session id from `AppConfig.last_open_sub_sessions` AND clear any worktree-tab `active_child_id` referencing it. Single atomic write
/// under the config write lock. Errors are logged at `warn` (with `tag` for source attribution) but **not** propagated — the restore prune paths and
/// other best-effort cleanup callers must always make forward progress; a config-write hiccup at restore must not strand the rest of the records.
fn forget_sub_session(ctx: &AppContext, id: SubSessionId, tag: &'static str) {
    if let Err(e) = ctx.store().save_config_with(PartialAppConfig::default(), |cfg| {
        cfg.last_open_sub_sessions.retain(|r| r.id != id);
        clear_active_child_in_config(cfg, ChildId::SubSession(id));
        true
    }) {
        warn!(sub_session_id = %id, error = ?e, tag, "forget_sub_session: persistence cleanup failed");
    }
}

/// Re-materialise every sub-session persisted in `AppConfig.lastOpenSubSessions`. Called from the `frontend_ready` wrapper AFTER
/// `session::restore_all_sessions` so parent sessions are already present in `sessions.json` (we look up worktree paths from there).
///
/// Per-record handling:
///
/// * **Orphan** (parent gone OR parent currently mid-close): drop from
///   persistence and skip. The frontend will not see the row at all.
/// * **Terminal**: insert into the in-memory store (Starting), emit
///   `subsession://restored` so the frontend store inserts the row, then
///   `pool.spawn_terminal(record.composed_command)`. The pool emits `Running`
///   (or `Exited`/`Error` on failure) via the same sink the production wiring
///   uses, so the frontend status path is identical to the create path.
/// * **Application**: insert into the in-memory store with status `Exited`
///   (greyed) and no PID, emit `subsession://restored`. The frontend renders
///   the tab greyed; clicking it triggers `subsession_relaunch` which spawns
///   under the same id.
///
/// Reconstitute sub-sessions from `AppConfig.lastOpenSubSessions` after launch or workspace switch.
/// Each record's `parent_worktree_tab_id` must reference a `WorktreeTab` that is currently present
/// in config (the worktree tab was created during the v5→v6 migration or at normal creation time).
/// Records whose parent worktree tab is missing are dropped (orphan prune). Records whose worktree
/// tab is mid-close are also dropped defensively.
///
/// On a Terminal *spawn* failure we keep the persistence record so the next app launch can retry; the row is already visible via the `restored` event
/// with status `Error`.
pub fn restore_all_sub_sessions_impl(ctx: &AppContext, sub_ctx: &SubAppContext) {
    let cfg = ctx.store().load_config();
    let records = cfg.last_open_sub_sessions.clone();
    if records.is_empty() {
        return;
    }

    // Build a lookup of tab id → path for cwd derivation.
    let tab_paths: BTreeMap<WorktreeTabId, PathBuf> = cfg.worktree_tabs.iter().map(|t| (t.id, t.path.clone())).collect();

    info!(sub_record_count = records.len(), "restore: second pass for sub-sessions");

    for record in records {
        let tab_id = match record.parent_worktree_tab_id {
            Some(tid) => tid,
            None => {
                warn!(
                    sub_session_id = %record.id,
                    "restore: dropping sub-session record with no parent_worktree_tab_id"
                );
                forget_sub_session(ctx, record.id, "restore-no-tab-id");
                continue;
            }
        };

        // Orphan check: worktree tab gone OR currently mid-close.
        let tab_path = match tab_paths.get(&tab_id) {
            Some(p) => p.clone(),
            None => {
                warn!(
                    sub_session_id = %record.id,
                    worktree_tab_id = %tab_id,
                    "restore: dropping orphan sub-session (worktree tab gone)"
                );
                forget_sub_session(ctx, record.id, "restore-orphan");
                continue;
            }
        };
        if ctx.is_worktree_tab_closing(&tab_id) {
            warn!(
                sub_session_id = %record.id,
                worktree_tab_id = %tab_id,
                "restore: dropping sub-session under closing worktree tab"
            );
            forget_sub_session(ctx, record.id, "restore-tab-closing");
            continue;
        }

        // Build the in-memory entry. Application kind starts greyed because we never auto-relaunch external editors at startup — the user clicks to
        // bring them back.
        let initial_status = match record.kind {
            CustomProcessKind::Terminal => SubSessionStatus::Starting,
            CustomProcessKind::Application => SubSessionStatus::Exited,
        };
        let sub = SubSession {
            id: record.id,
            parent_worktree_tab_id: tab_id,
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
        // Emit `restored` BEFORE any subsequent status event so the frontend store has the row when status arrives.
        (sub_ctx.sink.restored)(&sub);

        match record.kind {
            CustomProcessKind::Terminal => {
                let cwd = sub_session_cwd(&tab_path).to_path_buf();
                match sub_ctx
                    .pool
                    .spawn_terminal(record.id, record.composed_command.clone(), cwd, sub_ctx.sink.clone())
                {
                    Ok(pid) => {
                        info!(sub_session_id = %record.id, pid, "restore: terminal sub-session respawned");
                    }
                    Err(e) => {
                        warn!(
                            sub_session_id = %record.id,
                            error = ?e,
                            "restore: terminal sub-session respawn failed; keeping persistence record so user can retry next launch"
                        );
                        // Surface failure to the frontend store. The `restored` event already arrived with status Starting; this Error transition
                        // flips the row to the visible failure state.
                        (sub_ctx.sink.status)(&record.id, SubSessionStatus::Error, None, Some(format!("respawn failed: {e}")));
                    }
                }
            }
            CustomProcessKind::Application => {
                // Greyed: nothing to spawn. Click → relaunch.
            }
        }
    }
}

// --------------------------------------------------------------------------- Phase 7: relaunch
// ---------------------------------------------------------------------------

/// Re-spawn a sub-session under its existing id. Used by the frontend when the user clicks a greyed Application sub-tab (status `exited` or `error`);
/// also valid for Terminal sub-tabs whose PTY died for any reason. Persisted record is unchanged (id stable; composedCommand is **re-derived** from
/// the current def so user edits to the `customProcesses` list take effect on relaunch).
pub async fn subsession_relaunch_impl(ctx: &AppContext, sub_ctx: &SubAppContext, id: SubSessionId) -> Result<SubSession, AppError> {
    // Reject while a workspace switch is queued or active. Held for the full body (including across `pool.kill().await` + spawn) so the new child
    // can't end up bound to the old workspace's CWD after a mid-flight swap.
    let _switch = acquire_switch_read(ctx)?;

    let existing = sub_ctx
        .store
        .get(&id)
        .ok_or_else(|| AppError::new("NotFound", format!("sub session {id} not found")))?;

    // Look up the def fresh — relaunch picks up any post-creation edits the user made via the Settings tab. If the def has been deleted we refuse
    // rather than spawning an empty command.
    let cfg = ctx.store().load_config();
    let def = cfg
        .custom_processes
        .iter()
        .find(|d| d.id == existing.def_id)
        .ok_or_else(|| {
            AppError::new(
                "NotFound",
                format!("custom process def {:?} no longer exists; cannot relaunch", existing.def_id),
            )
        })?
        .clone();
    if !def.enabled {
        return Err(AppError::new(
            "InvalidArgument",
            format!("custom process def {:?} is disabled", existing.def_id),
        ));
    }

    // Look up the worktree tab for its path.
    let cfg = ctx.store().load_config();
    let tab = cfg
        .worktree_tabs
        .iter()
        .find(|t| t.id == existing.parent_worktree_tab_id)
        .ok_or_else(|| AppError::new("NotFound", format!("worktree tab {} not found", existing.parent_worktree_tab_id)))?;
    if !tab.path.is_dir() {
        return Err(AppError::from(Error::WorktreeMissing(tab.path.clone())));
    }
    if ctx.is_worktree_tab_closing(&existing.parent_worktree_tab_id) {
        return Err(AppError::new(
            "InvalidArgument",
            format!(
                "worktree tab {} is closing; refusing to relaunch sub-session",
                existing.parent_worktree_tab_id
            ),
        ));
    }

    // Tear down the old child (best-effort — Application has already exited if status is exited/error; Terminal may still have a PTY process around).
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

    // Persisted composed_command is refreshed from the current def so edits to the def (rename, command change) take effect.
    let composed_command = def.command.clone();

    // Reset status synchronously and update persistence so a crash between here and spawn doesn't leave a stale exited row.
    let mut refreshed = existing.clone();
    refreshed.status = SubSessionStatus::Starting;
    refreshed.pid = None;
    refreshed.composed_command = composed_command.clone();
    refreshed.label = def.name.clone();
    sub_ctx.store.remove(&id);
    sub_ctx.store.insert(refreshed.clone()).map_err(AppError::from)?;
    let updated_record = SubSessionRecord {
        id: refreshed.id,
        parent_session_id: None,
        parent_worktree_tab_id: Some(refreshed.parent_worktree_tab_id),
        def_id: refreshed.def_id.clone(),
        kind: refreshed.kind,
        label: refreshed.label.clone(),
        composed_command: refreshed.composed_command.clone(),
    };
    if let Err(e) = ctx.store().append_last_open_sub_session(updated_record) {
        // The on-disk record is unchanged (write_atomic is atomic), but we already tore down the prior child above. Roll the in-memory entry back to
        // `existing` with status=Error so the row stays visible (CP-07 spirit: a visible orphan beats a silent leak) and the user can retry via
        // relaunch. Mirrors the create-path rollback at `subsession_create_impl`.
        warn!(sub_session_id = %id, error = ?e, "relaunch: persistence refresh failed; rolling back in-memory state");
        sub_ctx.store.remove(&id);
        let mut rollback = existing.clone();
        rollback.status = SubSessionStatus::Error;
        rollback.pid = None;
        let _ = sub_ctx.store.insert(rollback);
        (sub_ctx.sink.status)(&id, SubSessionStatus::Error, None, Some(format!("relaunch persistence failed: {e}")));
        return Err(AppError::from(e));
    }
    (sub_ctx.sink.status)(&id, SubSessionStatus::Starting, None, None);

    let cwd = sub_session_cwd(&tab.path).to_path_buf();
    let spawn_result = match def.kind {
        CustomProcessKind::Terminal => sub_ctx.pool.spawn_terminal(id, composed_command, cwd, sub_ctx.sink.clone()),
        CustomProcessKind::Application => sub_ctx.app_pool.spawn(
            id,
            composed_command,
            cwd.clone(),
            sub_ctx.sink.clone(),
            owner_resolver_for(sub_ctx.registry.as_ref(), &def, &cwd),
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
            // Surface as Error but KEEP the row + persistence so user can retry. Mirrors `restore_all_sub_sessions_impl` failure path.
            (sub_ctx.sink.status)(&id, SubSessionStatus::Error, None, Some(format!("relaunch failed: {e}")));
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

/// Build the [`crate::app_launcher::OwnerResolver`] (if any) by delegating to the first matching custom-process plugin in the registry.
fn owner_resolver_for(
    registry: &crate::plugins::PluginRegistry,
    def: &crate::types::CustomProcessDef,
    cwd: &std::path::Path,
) -> Option<Arc<dyn crate::app_launcher::OwnerResolver>> {
    registry.custom_process_for_def(def).and_then(|plugin| plugin.owner_resolver(cwd))
}
