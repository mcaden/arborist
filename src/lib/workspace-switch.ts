// Shared "switch the active workspace" flow used by both the sidebar
// header (`WorkspaceIndicator`) and the in-app Settings panel
// (`SettingsDialog`). Centralised here so the close-all-sessions
// invariant is enforced in exactly one place — by the **backend**, in
// `commands/session.rs::workspace_switch_impl_inner`.
//
// Behaviour: delegate the entire transactional swap (validate → close
// every open session → bind new (branch, workspace) lock → swap the
// active store → emit `workspace://changed`) to the backend in a single
// `workspaceSwitch` invoke. On `WorkspaceLocked` we re-throw with a
// caller-friendly message so picker UIs can surface "already open in
// another window" without parsing wire-format strings.

import { isAppErrorLike, workspaceSwitch } from '@/lib/tauri-bridge';

/**
 * Switch the active workspace to `path`. On success the backend has
 * already swapped the live `WorkspaceScope` and emitted
 * `workspace://changed`, so this function does **not** touch the
 * frontend `useConfigStore` directly — the top-level `App` listener
 * picks up the event and re-fetches.
 *
 * Throws on validation, lock-contention, or close-all failure. The
 * caller (picker / settings dialog) keeps the user on the previous
 * workspace because the backend is fully transactional: on failure no
 * swap occurs, so the in-memory state is already consistent.
 */
export async function changeWorkspace(path: string): Promise<void> {
  try {
    await workspaceSwitch(path);
  } catch (err) {
    if (isAppErrorLike(err) && err.code === 'WorkspaceLocked') {
      throw new Error(
        'That workspace is already open in another Arborist window. Close it there and try again.',
      );
    }
    throw err;
  }
}
