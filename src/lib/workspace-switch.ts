// Shared "switch the active workspace" flow used by both the sidebar
// header (`WorkspaceIndicator`) and the in-app Settings panel
// (`SettingsDialog`). Centralised here so the park-old-sessions
// invariant is enforced in exactly one place — by the **backend**, in
// `commands/session.rs::workspace_switch_impl_inner`.
//
// Behaviour: delegate the entire transactional swap (validate → park
// every open session of the old workspace (PTYs killed, persisted
// records preserved so a later switch-back can restore them) → bind
// new (branch, workspace) lock → swap the active store → emit
// `workspace://changed`) to the backend in a single `workspaceSwitch`
// invoke. On `WorkspaceLocked` we re-throw with a caller-friendly
// message so picker UIs can surface "already open in another window"
// without parsing wire-format strings.

import { isAppErrorLike, workspaceSwitch } from '@/lib/tauri-bridge';
import { rehydrateActiveWorkspace } from '@/lib/rehydrate-workspace';

/**
 * Switch the active workspace to `path`. On success the backend has
 * already swapped the live `WorkspaceScope` and emitted
 * `workspace://changed`; the top-level `App` listener picks that up
 * and re-hydrates. We **also** drive `rehydrateActiveWorkspace()`
 * here as a defensive fallback: the backend only **logs** if
 * emitting the event fails (the Rust handler still resolves with
 * `Ok`), so without this fallback an emit failure would leave the
 * UI pointed at the old workspace even though the backend had
 * already rebound to the new one. The shared generation counter in
 * `rehydrate-workspace.ts` makes the duplicate work race-safe — the
 * losing call simply bails after its first await.
 *
 * Rehydrate failures after a successful `workspaceSwitch` are
 * swallowed (logged only): the backend has already rebound, so
 * surfacing the error would tell the user the operation failed when
 * it actually succeeded — the picker modal would stay open showing
 * "error" for a completed switch and a retry would hit the new
 * workspace as a no-op. The `workspace://changed` listener (or the
 * next user-initiated event that triggers a hydrate) is the
 * recovery path.
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
  if (!result.noOp) {
    try {
      await rehydrateActiveWorkspace();
    } catch (err) {
      // The switch already succeeded backend-side; do not propagate
      // the rehydrate failure to the caller. The App-level
      // `workspace://changed` listener will retry rehydrate, and the
      // shared generation counter makes the duplicate work safe.
      console.warn(
        'changeWorkspace: post-switch rehydrate failed; relying on event-driven rehydrate',
        err,
      );
    }
  }
}
