// Shared "switch the active workspace" flow used by both the sidebar
// header (`WorkspaceIndicator`) and the in-app Settings panel
// (`SettingsDialog`). Centralised here so the park-old-sessions
// invariant is enforced in exactly one place — by the **backend**, in
// `commands/session.rs::workspace_switch_impl_inner`.
//
// PR5: the old "switch → workspace://changed event → fallback
// rehydrate (config.hydrate → frontendReady → session.hydrate)" chain
// has been replaced with a single atomic adoption of the result the
// backend now returns inline. The backend runs `restore_all_sessions`
// for the new workspace under the write guard before resolving, so by
// the time `result` lands, the new workspace's config + sessions
// (with status already advanced past `Starting`) are ready to install
// in one render — no flicker, no event round-trip.
//
// Behaviour: delegate the entire transactional swap (validate → park
// every open session of the old workspace (PTYs killed, persisted
// records preserved so a later switch-back can restore them) → bind
// new (branch, workspace) lock → swap the active store → run inline
// restore → return `{ config, sessions }`) to the backend in a single
// `workspaceSwitch` invoke. On `WorkspaceLocked` we re-throw with a
// caller-friendly message so picker UIs can surface "already open in
// another window" without parsing wire-format strings.

import { isAppErrorLike, workspaceSwitch } from '@/lib/tauri-bridge';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

/**
 * Switch the active workspace to `path`. On success the backend has
 * already swapped the live `WorkspaceScope`, run restore for the new
 * workspace, and returned its post-restore `{ config, sessions }`. We
 * adopt both atomically into the frontend stores in a single render —
 * config first (so any selectors keyed on `workspaceRoot` see the new
 * value before the session list updates), then sessions (which also
 * reconciles `activeId` from `config.activeSessionId`).
 *
 * No-op switches short-circuit adoption: the result still carries the
 * (unchanged) state, but the caller has nothing to do.
 *
 * Throws on validation or lock-contention. The caller (picker /
 * settings dialog) keeps the user on the previous workspace because
 * the backend is fully transactional: on failure no swap occurs, so
 * the in-memory state is already consistent. (Park itself is
 * best-effort and never aborts the switch — see DESIGN §5.5c
 * step 7.)
 */
export async function changeWorkspace(path: string): Promise<void> {
  let result;
  try {
    result = await workspaceSwitch(path);
  } catch (err) {
    if (isAppErrorLike(err) && err.code === 'WorkspaceLocked') {
      throw new Error(
        'That workspace is already open in another Arborist window. Close it there and try again.',
      );
    }
    throw err;
  }
  if (result.noOp) {
    return;
  }
  // Atomic adoption: install the new workspace's config + sessions in
  // one render. Order matters — config-store goes first so any
  // selectors keyed on `workspaceRoot` observe the new value before
  // the session list shifts under them. The session-store action also
  // reconciles `activeId` from the new config's `activeSessionId`,
  // closing the pre-existing UX gap where post-switch `MainArea`
  // would show a blank pane.
  useConfigStore.getState().adoptWorkspace(result.config);
  useSessionStore.getState().actions.adoptWorkspace(result.sessions, result.config.activeSessionId);
}
