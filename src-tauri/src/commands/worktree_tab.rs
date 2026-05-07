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
    AppConfig, AppError, ChildId, PartialAppConfig, SessionId, WorktreeTab, WorktreeTabCloseResult, WorktreeTabId, WorktreeTabOpenArgs,
    WorktreeTabSetActiveChildArgs,
};

use super::session::{self, AppContext};

/// Renumber every tab's `tab_index` to match its slot in `worktree_tab_order`. Pure helper called from any code path that mutates the order
/// (close, future merge/split flows). `reorder` does the same renumbering inline; centralising the logic here means the close path can no longer
/// drift out of sync with the order list and produce stale `tab_index` values that subsequently collide with newly-opened tabs (see issue #44 PR
/// #65 review feedback).
pub(crate) fn renormalize_worktree_tab_indices(cfg: &mut AppConfig) {
    for (idx, tid) in cfg.worktree_tab_order.iter().enumerate() {
        if let Some(tab) = cfg.worktree_tabs.iter_mut().find(|t| t.id == *tid) {
            tab.tab_index = idx;
        }
    }
}

/// Clear any `WorktreeTab.active_child_id` in `cfg` that matches `target`. Called from the close paths
/// ([`crate::commands::session::session_close_locked`], [`crate::commands::subsession::subsession_close_impl`],
/// [`crate::commands::subsession::close_for_parent_impl`]) and from the restore-prune paths that drop sessions/sub-sessions whose backing record is no
/// longer valid ([`crate::commands::session::trim_unknown_session_refs_with_store`], the worktree-missing branch of
/// [`crate::commands::session::restore_all_sessions`], and the orphan/closing-parent branches of `subsession::restore_*`). Without this, a tab's
/// last-focused-child pointer can dangle past the child's removal — surfacing as incorrect restore/focus when the tab is next visited (PR #65 review).
///
/// Returns `true` if anything was cleared. Always safe to call; a no-op when no tab references `target`.
pub(crate) fn clear_active_child_in_config(cfg: &mut AppConfig, target: ChildId) -> bool {
    let mut changed = false;
    for tab in cfg.worktree_tabs.iter_mut() {
        if tab.active_child_id == Some(target) {
            tab.active_child_id = None;
            changed = true;
        }
    }
    changed
}

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
    // Delegate canonicalisation to `compose::validate_worktree` so missing-vs-not-a-directory failures map to the same stable error codes the
    // session-create path returns (`WorktreeMissing` vs `InvalidPath`). Without this, the frontend would see two different code values for the
    // "directory doesn't exist" case depending on whether the user picked the worktree via the picker (session path) or the upcoming worktree-tab
    // dialog (this path), and any error-routing it does (e.g. "show a friendlier message for missing worktrees") would silently miss this entry.
    let canonical = compose::validate_worktree(&path).map_err(AppError::from)?;

    // Hot-path early return: if a tab already exists at this canonical path AND it is already the active worktree tab, the call is a true no-op.
    // Returning before `save_config_with` avoids rewriting `config.json` (and bumping its mtime) on every idempotent re-open from the UI — without
    // this, a tight loop of "open same path" calls would produce a disk write each time even though nothing changed. The race window between this
    // load and the `save_config_with` below is benign: if the tab is removed by another caller in the gap, the closure below re-checks under the
    // config write lock and falls through to the create-new path. The create-new path produces a fresh tab at the requested path, which is the
    // correct post-condition for `worktree_tab_open` regardless of whether the previous tab was a re-use or a fresh create.
    {
        let cfg = ctx.store().load_config();
        if let Some(existing) = cfg.worktree_tabs.iter().find(|t| t.path == canonical) {
            if cfg.active_worktree_tab_id == Some(existing.id) {
                return Ok(existing.clone());
            }
        }
    }

    // Atomic read-modify-write: duplicate check + insert (or focus-existing) under the config write lock. "Open" also acts as "focus" — the
    // backend's persisted `active_worktree_tab_id` is updated to point at the (existing or new) tab so restore-on-launch lands on it.
    let mut result_tab: Option<WorktreeTab> = None;
    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            // Check for an existing tab at the same canonical path.
            if let Some(existing) = cfg.worktree_tabs.iter().find(|t| t.path == canonical) {
                let existing_id = existing.id;
                result_tab = Some(existing.clone());
                if cfg.active_worktree_tab_id == Some(existing_id) {
                    // Reachable on the rare race where the tab was *not* active at the pre-check above but became active in the gap before we took
                    // the write lock. The bool is currently informational (the framework writes unconditionally), but signalling "no mutation" keeps
                    // the callsite honest and lets a future variant of `save_config_with` skip the write generically.
                    return false;
                }
                cfg.active_worktree_tab_id = Some(existing_id);
                return true;
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

    // The closure always populates `result_tab` on both branches; this `ok_or_else` exists only to remove the production `expect()` panic path so
    // future refactors that introduce an early return inside the closure surface as a regular AppError instead of a panic.
    let tab = result_tab.ok_or_else(|| AppError::new("Internal", "worktree_tab_open: result_tab not set after save_config_with"))?;
    info!(worktree_tab_id = %tab.id, path = %tab.path.display(), "worktree tab opened");
    Ok(tab)
}

/// Close a worktree tab and cascade-close all child sessions and sub-sessions. Returns a result with any per-child errors that occurred (the tab
/// itself is always removed). The switch-read guard is held for the full body so the workspace cannot be swapped mid-cascade — which would otherwise
/// leave the wrong workspace's config mutated. Holding the guard across awaits is correct: tokio guards are `Send`, and the cascade calls
/// `session_close_locked` (the unguarded inner helper) so the inner close cannot self-reject mid-cascade if a workspace switch queues in the gap (see
/// the doc on `session_close_locked` for why calling `session_close_impl` here would orphan child records).
pub async fn worktree_tab_close_impl(
    ctx: &AppContext,
    sub_ctx: std::sync::Arc<crate::sub_sessions::SubAppContext>,
    id: WorktreeTabId,
) -> Result<WorktreeTabCloseResult, AppError> {
    let _switch = session::acquire_switch_read(ctx)?;

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
    for sid in &child_session_ids {
        let _guard = ctx.mark_parent_closing(*sid);
        super::subsession::close_for_parent_impl(ctx, &sub_ctx, *sid).await;
        // `session_close_locked` (NOT `_impl`) — we already hold the switch read guard for the entire cascade. Calling `_impl` would re-enter
        // `acquire_switch_read`, which checks `AppContext::switch_pending` independently of guard ownership and would reject mid-cascade if a
        // workspace switch were queued in the gap, leaving the parent worktree tab removed below but the child session record still present.
        match super::session::session_close_locked(ctx, *sid, false).await {
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
            // Re-normalize `tab_index` on every surviving tab so the value matches the tab's slot in `worktree_tab_order`. Without this, a subsequent
            // `worktree_tab_open` (which assigns the new tab `tab_index = worktree_tab_order.len()`) can collide with the stale `tab_index` carried by
            // a tab that was originally past the just-closed slot — e.g. open A, B, C → close B → C still carries `tab_index = 2`; opening D then
            // also assigns `tab_index = 2`. `reorder` already does this same renumbering pass.
            renormalize_worktree_tab_indices(cfg);
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

    // Cold-path early return: if the tab does not exist, fail fast WITHOUT calling `save_config_with` (which would otherwise rewrite `config.json`
    // on every NotFound — `save_config_with` writes unconditionally regardless of the closure's bool return). Repeated calls with stale ids (e.g.
    // racing a close) used to produce one disk write per call. The race window between this load and the closure below is benign: if the tab
    // disappears in the gap the closure sets `found = false` and we still report NotFound.
    {
        let cfg = ctx.store().load_config();
        if !cfg.worktree_tabs.iter().any(|t| t.id == id) {
            return Err(AppError::new("NotFound", format!("worktree tab {id} not found")));
        }
    }

    // Validate before mutation. Holding `_switch` and the config write lock inside `save_config_with` makes this check-then-write effectively atomic
    // wrt other commands in this process.
    let mut found = false;
    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            if cfg.worktree_tabs.iter().any(|t| t.id == id) {
                cfg.active_worktree_tab_id = Some(id);
                found = true;
                true
            } else {
                false
            }
        })
        .map_err(AppError::from)?;
    if !found {
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

/// Reorder worktree tabs. The provided IDs list becomes the new `worktree_tab_order`; each tab's `tab_index` is updated to match. The list must
/// contain every existing tab exactly once (no partial reorder → silent tab loss). Validation runs *inside* the config write lock so a malformed
/// call cannot leave `tab_index` and `worktree_tab_order` in disagreement.
pub fn worktree_tab_reorder_impl(ctx: &AppContext, ids: Vec<WorktreeTabId>) -> Result<(), AppError> {
    let _switch = session::acquire_switch_read(ctx)?;

    // ValidationError captured inside the closure so we can return a descriptive error after the save_config_with call.
    let mut validation_error: Option<AppError> = None;

    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            let known: HashSet<WorktreeTabId> = cfg.worktree_tabs.iter().map(|t| t.id).collect();
            if ids.len() != known.len() {
                validation_error = Some(AppError::new(
                    "InvalidArgument",
                    format!("reorder list must contain all {} tabs, got {}", known.len(), ids.len()),
                ));
                return false;
            }
            // Reject duplicates explicitly. Without this, `[A, A, B]` against `{A, B, C}` would pass len-equality and per-id "is known" but silently
            // drop tab `C` from the persisted order, violating the "exactly once" contract.
            let mut seen: HashSet<WorktreeTabId> = HashSet::with_capacity(ids.len());
            for id in &ids {
                if !seen.insert(*id) {
                    validation_error = Some(AppError::new(
                        "InvalidArgument",
                        format!("reorder list contains duplicate worktree tab id {id}"),
                    ));
                    return false;
                }
            }
            for id in &ids {
                if !known.contains(id) {
                    validation_error = Some(AppError::new("NotFound", format!("worktree tab {id} not found in reorder list")));
                    return false;
                }
            }
            // All checks passed — apply mutation.
            for (idx, id) in ids.iter().enumerate() {
                if let Some(tab) = cfg.worktree_tabs.iter_mut().find(|t| &t.id == id) {
                    tab.tab_index = idx;
                }
            }
            cfg.worktree_tab_order = ids.clone();
            true
        })
        .map_err(AppError::from)?;

    if let Some(err) = validation_error {
        return Err(err);
    }
    Ok(())
}

/// Set (or clear) the active child for a worktree tab. When `child_id` is `None`, the worktree dashboard shows; when `Some`, the matching session or
/// sub-session terminal is shown. Validates that the child belongs to this worktree:
/// - `ChildId::Session(sid)` — the session's `worktree_path` must match the tab's path.
/// - `ChildId::SubSession(ssid)` — the sub-session's parent session's `worktree_path` must match the tab's path.
pub fn worktree_tab_set_active_child_impl(
    ctx: &AppContext,
    sub_ctx: std::sync::Arc<crate::sub_sessions::SubAppContext>,
    args: WorktreeTabSetActiveChildArgs,
) -> Result<(), AppError> {
    let _switch = session::acquire_switch_read(ctx)?;

    // Validate the child belongs to this worktree tab (if applicable). Done before the write lock so we can return descriptive errors.
    if let Some(ref child) = args.child_id {
        let cfg = ctx.store().load_config();
        let tab = cfg
            .worktree_tabs
            .iter()
            .find(|t| t.id == args.id)
            .ok_or_else(|| AppError::new("NotFound", format!("worktree tab {} not found", args.id)))?;
        let tab_path = tab.path.clone();
        let sessions = ctx.store().load_sessions();
        match child {
            ChildId::Session(sid) => {
                let session = sessions
                    .get(sid)
                    .ok_or_else(|| AppError::new("NotFound", format!("session {sid} not found")))?;
                if session.worktree_path != tab_path {
                    return Err(AppError::new(
                        "InvalidArgument",
                        format!("session {sid} worktree path does not match worktree tab {}", args.id),
                    ));
                }
            }
            ChildId::SubSession(ssid) => {
                // Resolve the sub-session in the live in-memory store, then verify its parent session belongs to this worktree.
                let sub = sub_ctx
                    .store
                    .get(ssid)
                    .ok_or_else(|| AppError::new("NotFound", format!("sub-session {ssid} not found")))?;
                let parent = sessions
                    .get(&sub.parent_session_id)
                    .ok_or_else(|| AppError::new("NotFound", format!("sub-session {ssid} parent session not found")))?;
                if parent.worktree_path != tab_path {
                    return Err(AppError::new(
                        "InvalidArgument",
                        format!("sub-session {ssid} parent worktree path does not match worktree tab {}", args.id),
                    ));
                }
            }
        }
    }

    let mut found = false;
    ctx.store()
        .save_config_with(PartialAppConfig::default(), |cfg| {
            if let Some(tab) = cfg.worktree_tabs.iter_mut().find(|t| t.id == args.id) {
                tab.active_child_id = args.child_id;
                found = true;
                true
            } else {
                false
            }
        })
        .map_err(AppError::from)?;
    if !found {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tab(id: WorktreeTabId, idx: usize) -> WorktreeTab {
        WorktreeTab {
            id,
            path: PathBuf::from(format!("/repo/{id}")),
            name: id.to_string(),
            branch: None,
            label: id.to_string(),
            tab_index: idx,
            active_child_id: None,
        }
    }

    #[test]
    fn renormalize_tab_indices_assigns_each_tab_its_slot_in_the_order() {
        // Simulate the post-close state: A, B, C were originally at indices 0, 1, 2; B was just removed from `worktree_tab_order` but C still
        // carries `tab_index = 2` (stale). The helper must rewrite C → 1 so subsequent `worktree_tab_open` (which uses `worktree_tab_order.len()` for
        // the new tab's `tab_index`) doesn't collide with the stale value.
        let id_a = WorktreeTabId::new();
        let id_c = WorktreeTabId::new();
        let mut cfg = AppConfig {
            worktree_tabs: vec![tab(id_a, 0), tab(id_c, 2)],
            worktree_tab_order: vec![id_a, id_c],
            ..AppConfig::default()
        };

        renormalize_worktree_tab_indices(&mut cfg);

        let by_id: std::collections::HashMap<_, _> = cfg.worktree_tabs.iter().map(|t| (t.id, t.tab_index)).collect();
        assert_eq!(by_id[&id_a], 0);
        assert_eq!(by_id[&id_c], 1, "C must be renumbered to slot 1, not left at the stale 2");
    }

    #[test]
    fn renormalize_tab_indices_is_a_noop_when_indices_already_match() {
        let id_a = WorktreeTabId::new();
        let id_b = WorktreeTabId::new();
        let mut cfg = AppConfig {
            worktree_tabs: vec![tab(id_a, 0), tab(id_b, 1)],
            worktree_tab_order: vec![id_a, id_b],
            ..AppConfig::default()
        };

        renormalize_worktree_tab_indices(&mut cfg);

        let by_id: std::collections::HashMap<_, _> = cfg.worktree_tabs.iter().map(|t| (t.id, t.tab_index)).collect();
        assert_eq!(by_id[&id_a], 0);
        assert_eq!(by_id[&id_b], 1);
    }

    #[test]
    fn renormalize_tab_indices_skips_tabs_missing_from_order_list() {
        // Defensive: if `worktree_tabs` and `worktree_tab_order` have ever drifted (shouldn't happen on the close path, which retains both in sync),
        // tabs not in the order list keep their existing `tab_index` rather than panicking or re-using a slot value.
        let id_a = WorktreeTabId::new();
        let id_orphan = WorktreeTabId::new();
        let mut cfg = AppConfig {
            worktree_tabs: vec![tab(id_a, 5), tab(id_orphan, 99)],
            worktree_tab_order: vec![id_a],
            ..AppConfig::default()
        };

        renormalize_worktree_tab_indices(&mut cfg);

        let by_id: std::collections::HashMap<_, _> = cfg.worktree_tabs.iter().map(|t| (t.id, t.tab_index)).collect();
        assert_eq!(by_id[&id_a], 0);
        assert_eq!(
            by_id[&id_orphan], 99,
            "orphan tab keeps its existing index — helper does not invent slots"
        );
    }

    // ---- clear_active_child_in_config (PR #65 review-7) ----

    fn tab_with_child(id: WorktreeTabId, child: Option<ChildId>) -> WorktreeTab {
        let mut t = tab(id, 0);
        t.active_child_id = child;
        t
    }

    #[test]
    fn clear_active_child_clears_only_matching_session_pointer() {
        let id_target = crate::types::SessionId::new();
        let id_other = crate::types::SessionId::new();
        let tab_a = WorktreeTabId::new();
        let tab_b = WorktreeTabId::new();
        let tab_c = WorktreeTabId::new();
        let mut cfg = AppConfig {
            worktree_tabs: vec![
                tab_with_child(tab_a, Some(ChildId::Session(id_target))),
                tab_with_child(tab_b, Some(ChildId::Session(id_other))),
                tab_with_child(tab_c, None),
            ],
            ..AppConfig::default()
        };

        let changed = clear_active_child_in_config(&mut cfg, ChildId::Session(id_target));

        assert!(changed, "must report changed");
        assert_eq!(cfg.worktree_tabs[0].active_child_id, None, "matching session pointer must be cleared");
        assert_eq!(
            cfg.worktree_tabs[1].active_child_id,
            Some(ChildId::Session(id_other)),
            "non-matching session pointer must be left alone"
        );
        assert_eq!(cfg.worktree_tabs[2].active_child_id, None, "already-None must remain None");
    }

    #[test]
    fn clear_active_child_clears_only_matching_subsession_pointer_not_the_session_kind() {
        // Sub-session ids and session ids both wrap UUIDs but are different types in the discriminated `ChildId` enum. A clear request for one kind
        // must NOT match a tab whose pointer is the other kind, even if (hypothetically) the underlying UUIDs happened to coincide.
        let shared_uuid = uuid::Uuid::new_v4();
        let sid = crate::types::SessionId(shared_uuid);
        let ssid = crate::types::SubSessionId(shared_uuid);
        let tab_a = WorktreeTabId::new();
        let tab_b = WorktreeTabId::new();
        let mut cfg = AppConfig {
            worktree_tabs: vec![
                tab_with_child(tab_a, Some(ChildId::Session(sid))),
                tab_with_child(tab_b, Some(ChildId::SubSession(ssid))),
            ],
            ..AppConfig::default()
        };

        let changed = clear_active_child_in_config(&mut cfg, ChildId::SubSession(ssid));

        assert!(changed);
        assert_eq!(
            cfg.worktree_tabs[0].active_child_id,
            Some(ChildId::Session(sid)),
            "ChildId::Session pointer must NOT be cleared by a SubSession clear request even on coincident UUIDs"
        );
        assert_eq!(cfg.worktree_tabs[1].active_child_id, None, "matching SubSession pointer must be cleared");
    }

    #[test]
    fn clear_active_child_returns_false_when_no_tab_matches() {
        let bystander = crate::types::SessionId::new();
        let target = crate::types::SessionId::new();
        let tab_a = WorktreeTabId::new();
        let mut cfg = AppConfig {
            worktree_tabs: vec![tab_with_child(tab_a, Some(ChildId::Session(bystander)))],
            ..AppConfig::default()
        };

        let changed = clear_active_child_in_config(&mut cfg, ChildId::Session(target));

        assert!(
            !changed,
            "no match → returns false → callers can elide a write if combined with a no-op short-circuit"
        );
        assert_eq!(
            cfg.worktree_tabs[0].active_child_id,
            Some(ChildId::Session(bystander)),
            "bystander untouched"
        );
    }

    #[test]
    fn clear_active_child_clears_multiple_tabs_pointing_at_same_target() {
        // Pathological but persistable: two tabs simultaneously pointing at the same child (e.g. via a buggy migration). The helper must clear ALL of
        // them in a single pass, not just the first.
        let target = crate::types::SessionId::new();
        let tab_a = WorktreeTabId::new();
        let tab_b = WorktreeTabId::new();
        let tab_c = WorktreeTabId::new();
        let mut cfg = AppConfig {
            worktree_tabs: vec![
                tab_with_child(tab_a, Some(ChildId::Session(target))),
                tab_with_child(tab_b, Some(ChildId::Session(target))),
                tab_with_child(tab_c, None),
            ],
            ..AppConfig::default()
        };

        let changed = clear_active_child_in_config(&mut cfg, ChildId::Session(target));

        assert!(changed);
        assert_eq!(cfg.worktree_tabs[0].active_child_id, None);
        assert_eq!(cfg.worktree_tabs[1].active_child_id, None);
        assert_eq!(cfg.worktree_tabs[2].active_child_id, None);
    }
}
