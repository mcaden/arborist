//! Worktree tab command implementations (Issue #44).
//!
//! A worktree tab is the first-class sidebar parent introduced in `configVersion = 5`. Child sessions and sub-sessions are grouped underneath by
//! matching `WorktreeTab.path == Session.worktree_path`. These implementations are driven by the thin `#[tauri::command]` wrappers in `mod.rs`.

use std::collections::HashSet;
use std::path::PathBuf;

use tracing::{info, warn};

use crate::compose;
use crate::config_store::ConfigStore;
use crate::types::{
    AppError, ChildId, PartialAppConfig, SessionId, WorktreeTab, WorktreeTabCloseResult, WorktreeTabId, WorktreeTabOpenArgs,
    WorktreeTabSetActiveChildArgs,
};

use super::session::{self, AppContext};

/// Open (or return an existing) worktree tab for the given path. Idempotent on canonical path — if a tab already exists for the same directory, it is
/// returned without creating a duplicate. Atomicity: the duplicate check and insert run under `save_config_with`'s write lock.
pub fn worktree_tab_open_impl(ctx: &AppContext, args: WorktreeTabOpenArgs) -> Result<WorktreeTab, AppError> {
    let _switch = session::acquire_switch_read(ctx)?;

    let path = PathBuf::from(&args.path);
    if path.is_relative() {
        return Err(AppError::new(
            "InvalidArgument",
            format!("worktree tab path must be absolute, got {}", path.display()),
        ));
    }
    let canonical = dunce::canonicalize(&path).map_err(|e| AppError::new("InvalidPath", format!("cannot canonicalize {}: {e}", path.display())))?;
    if !canonical.is_dir() {
        return Err(AppError::new(
            "InvalidPath",
            format!("worktree tab path is not a directory: {}", canonical.display()),
        ));
    }

    // Atomic read-modify-write: duplicate check + insert under the config write lock.
    let mut result_tab: Option<WorktreeTab> = None;
    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            // Check for an existing tab at the same canonical path.
            if let Some(existing) = cfg.worktree_tabs.iter().find(|t| t.path == canonical) {
                result_tab = Some(existing.clone());
                return false; // no mutation needed
            }

            let name = canonical
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "worktree".into());
            let existing_labels: Vec<&str> = cfg.worktree_tabs.iter().map(|t| t.label.as_str()).collect();
            let label = compose::dedupe_label(&existing_labels, &name);
            let tab_index = cfg.worktree_tab_order.len();

            let tab = WorktreeTab {
                id: WorktreeTabId::new(),
                path: canonical.clone(),
                name,
                branch: None,
                label,
                tab_index,
                active_child_id: None,
            };

            cfg.worktree_tabs.push(tab.clone());
            cfg.worktree_tab_order.push(tab.id);
            cfg.active_worktree_tab_id = Some(tab.id);
            result_tab = Some(tab);
            true
        })
        .map_err(AppError::from)?;

    let tab = result_tab.expect("result_tab set in closure");
    info!(worktree_tab_id = %tab.id, path = %tab.path.display(), "worktree tab opened");
    Ok(tab)
}

/// Close a worktree tab and cascade-close all child sessions and sub-sessions. Returns a result with any per-child errors that occurred (the tab
/// itself is always removed). The workspace-switch read guard is acquired by individual `session_close_impl` calls.
pub async fn worktree_tab_close_impl(
    ctx: &AppContext,
    sub_ctx: std::sync::Arc<crate::sub_sessions::SubAppContext>,
    id: WorktreeTabId,
) -> Result<WorktreeTabCloseResult, AppError> {
    // Validate tab exists and grab its path before starting the cascade.
    let tab_path = {
        let cfg = ctx.store().load_config();
        let tab = cfg
            .worktree_tabs
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| AppError::new("NotFound", format!("worktree tab {id} not found")))?;
        tab.path.clone()
    };

    // Find all child sessions under this worktree tab (by matching path).
    let sessions = ctx.store().load_sessions();
    let child_session_ids: Vec<SessionId> = sessions.values().filter(|s| s.worktree_path == tab_path).map(|s| s.id).collect();

    let mut child_errors: Vec<String> = Vec::new();

    // Close each child session (this cascades to their sub-sessions automatically).
    // Each session_close_impl call acquires its own switch-read guard internally.
    for sid in &child_session_ids {
        let _guard = ctx.mark_parent_closing(*sid);
        super::subsession::close_for_parent_impl(ctx, &sub_ctx, *sid).await;
        match super::session::session_close_impl(ctx, *sid, false).await {
            Ok(_) => {}
            Err(e) => {
                warn!(session_id = %sid, error = %e.message, "worktree tab close: child session close failed");
                child_errors.push(format!("session {sid}: {}", e.message));
            }
        }
    }

    // Atomically remove the worktree tab from config.
    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            cfg.worktree_tabs.retain(|t| t.id != id);
            cfg.worktree_tab_order.retain(|tid| *tid != id);
            if cfg.active_worktree_tab_id == Some(id) {
                cfg.active_worktree_tab_id = cfg.worktree_tab_order.first().copied();
            }
            true
        })
        .map_err(AppError::from)?;

    info!(worktree_tab_id = %id, child_sessions = child_session_ids.len(), "worktree tab closed");
    Ok(WorktreeTabCloseResult { child_errors })
}

/// Set the active worktree tab. Persists the selection to config.
pub fn worktree_tab_focus_impl(ctx: &AppContext, id: WorktreeTabId) -> Result<(), AppError> {
    let _switch = session::acquire_switch_read(ctx)?;

    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            if !cfg.worktree_tabs.iter().any(|t| t.id == id) {
                return false;
            }
            cfg.active_worktree_tab_id = Some(id);
            true
        })
        .map_err(AppError::from)?;

    // Validate the tab existed (the closure above silently no-ops on miss — check post-hoc).
    let cfg = ctx.store().load_config();
    if !cfg.worktree_tabs.iter().any(|t| t.id == id) {
        return Err(AppError::new("NotFound", format!("worktree tab {id} not found")));
    }
    Ok(())
}

/// List all worktree tabs, ordered by the authoritative `worktree_tab_order`.
pub fn worktree_tab_list_impl(ctx: &AppContext) -> Result<Vec<WorktreeTab>, AppError> {
    let cfg = ctx.store().load_config();
    let order: Vec<WorktreeTabId> = cfg.worktree_tab_order.clone();
    let mut by_id: std::collections::HashMap<WorktreeTabId, WorktreeTab> = cfg.worktree_tabs.into_iter().map(|t| (t.id, t)).collect();

    // Return tabs in authoritative order, then append any stragglers not in the order list.
    let mut result: Vec<WorktreeTab> = Vec::with_capacity(by_id.len());
    for id in &order {
        if let Some(tab) = by_id.remove(id) {
            result.push(tab);
        }
    }
    // Append stragglers (defensive — shouldn't happen in normal operation).
    let mut stragglers: Vec<WorktreeTab> = by_id.into_values().collect();
    stragglers.sort_by_key(|t| t.tab_index);
    result.extend(stragglers);
    Ok(result)
}

/// Reorder worktree tabs. The provided IDs list becomes the new `worktree_tab_order`; each tab's `tab_index` is updated to match.
pub fn worktree_tab_reorder_impl(ctx: &AppContext, ids: Vec<WorktreeTabId>) -> Result<(), AppError> {
    let _switch = session::acquire_switch_read(ctx)?;

    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            let known: HashSet<WorktreeTabId> = cfg.worktree_tabs.iter().map(|t| t.id).collect();
            // Completeness + existence are validated; any mismatch is a caller bug, but we update what we can.
            for (idx, id) in ids.iter().enumerate() {
                if let Some(tab) = cfg.worktree_tabs.iter_mut().find(|t| &t.id == id) {
                    tab.tab_index = idx;
                }
            }
            // Only update order if it covers all tabs (prevent silent tab loss).
            if ids.len() == known.len() && ids.iter().all(|id| known.contains(id)) {
                cfg.worktree_tab_order = ids.clone();
            }
            true
        })
        .map_err(AppError::from)?;

    // Post-hoc validation for the caller.
    let cfg = ctx.store().load_config();
    let known: HashSet<WorktreeTabId> = cfg.worktree_tabs.iter().map(|t| t.id).collect();
    if ids.len() != known.len() {
        return Err(AppError::new(
            "InvalidArgument",
            format!("reorder list must contain all {} tabs, got {}", known.len(), ids.len()),
        ));
    }
    for id in &ids {
        if !known.contains(id) {
            return Err(AppError::new("NotFound", format!("worktree tab {id} not found in reorder list")));
        }
    }
    Ok(())
}

/// Set (or clear) the active child for a worktree tab. When `child_id` is `None`, the worktree dashboard shows; when `Some`, the matching session or
/// sub-session terminal is shown.
pub fn worktree_tab_set_active_child_impl(ctx: &AppContext, args: WorktreeTabSetActiveChildArgs) -> Result<(), AppError> {
    let _switch = session::acquire_switch_read(ctx)?;

    // Validate session child belongs to this worktree tab (if applicable).
    if let Some(ChildId::Session(ref sid)) = args.child_id {
        let cfg = ctx.store().load_config();
        let tab = cfg
            .worktree_tabs
            .iter()
            .find(|t| t.id == args.id)
            .ok_or_else(|| AppError::new("NotFound", format!("worktree tab {} not found", args.id)))?;
        let sessions = ctx.store().load_sessions();
        let session = sessions
            .get(sid)
            .ok_or_else(|| AppError::new("NotFound", format!("session {sid} not found")))?;
        if session.worktree_path != tab.path {
            return Err(AppError::new(
                "InvalidArgument",
                format!("session {sid} worktree path does not match worktree tab {}", args.id),
            ));
        }
    }

    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            if let Some(tab) = cfg.worktree_tabs.iter_mut().find(|t| t.id == args.id) {
                tab.active_child_id = args.child_id;
                true
            } else {
                false
            }
        })
        .map_err(AppError::from)?;

    // Post-hoc check that tab existed.
    let cfg = ctx.store().load_config();
    if !cfg.worktree_tabs.iter().any(|t| t.id == args.id) {
        return Err(AppError::new("NotFound", format!("worktree tab {} not found", args.id)));
    }
    Ok(())
}

/// Resolve the worktree path from a [`WorktreeTabId`] by looking it up in the current config. Helper used by other command handlers.
pub fn resolve_worktree_path(store: &ConfigStore, id: WorktreeTabId) -> Result<PathBuf, AppError> {
    let cfg = store.load_config();
    cfg.worktree_tabs
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.path.clone())
        .ok_or_else(|| AppError::new("NotFound", format!("worktree tab {id} not found")))
}
