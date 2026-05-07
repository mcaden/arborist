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
// the time `result` lands, the new workspace's config + sessions are
// ready to install in one render — no flicker, no event round-trip.
// Note that restored sessions arrive in `Starting` (PTY spawn is
// deferred until the frontend's first `session_resize` measures the
// host — see `restore_all_sessions` in `commands/session.rs`); a few
// may also arrive as `Error` if their previous spawn failed. The
// status events that flip them to `Running` are emitted by the
// pty-pool wait threads after spawn fires, which happens AFTER this
// adoption.
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
import { useWorkspaceSwitchUiStore } from '@/store/workspace-switch-ui-store';

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
 * The `isSwitching` UI flag is set synchronously **before** the invoke
 * and cleared in `finally` after adoption lands. Pairing the flag-off
 * with adoption in a single React render means the user never sees a
 * "no workspace" flash. While `isSwitching` is true, `App.tsx`
 * overlays the UI with a "Switching workspace…" panel and sets
 * `aria-busy` + `inert` on the root so click / keyboard input can't
 * reach stale tabs (see DESIGN §5.5c — switches are transactional and
 * inputs received during the switch would be against ambiguous
 * state).
 *
 * **Reentrancy contract**: if a switch is already in flight when this
 * is called, the new call is silently dropped — it returns a resolved
 * `Promise<void>` without invoking the bridge or touching the stores.
 * This means an awaited `changeWorkspace(...)` may resolve as a no-op,
 * and callers cannot distinguish "switched" from "dropped" from the
 * return value. In practice the overlay's `inert` root + each picker's
 * `submitting` state prevent overlapping calls from the existing UI,
 * but any future caller that needs to know the call actually ran (e.g.
 * to show user-facing feedback) must gate at its own layer (e.g. by
 * subscribing to `useWorkspaceSwitchUiStore.isSwitching`).
 *
 * Throws on validation or lock-contention. The caller (picker /
 * settings dialog) keeps the user on the previous workspace because
 * the backend is fully transactional: on failure no swap occurs, so
 * the in-memory state is already consistent. (Park itself is
 * best-effort and never aborts the switch — see DESIGN §5.5c
 * step 7.)
 */
export async function changeWorkspace(path: string): Promise<void> {
  const { isSwitching, setSwitching } = useWorkspaceSwitchUiStore.getState();
  // Reentrancy guard: if a switch is already in flight, drop this call.
  // The other call owns the flag and will clear it in its own `finally`;
  // clearing here would lower the overlay while the in-flight invoke is
  // still pending, exposing stale tabs to input. Other UI layers (the
  // overlay's `inert` root, the picker's `submitting` state, the
  // backend's `switch_lock`) already make double-fires unlikely, but
  // making this function reentrant-safe in isolation removes the last
  // race window.
  if (isSwitching) {
    return;
  }
  setSwitching(true);
  try {
    let result;
    try {
      result = await workspaceSwitch(path);
    } catch (err) {
      if (isAppErrorLike(err) && err.code === 'WorkspaceLocked') {
        throw new Error('That workspace is already open in another Arborist window. Close it there and try again.');
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
  } finally {
    // Cleared even on throw / WorkspaceLocked so the picker / settings
    // dialog isn't permanently locked behind a stuck overlay. Adoption
    // (above) and the flag-off coalesce into a single React render in
    // the success path, so the new workspace's tabs become interactive
    // in the same paint that hides the overlay.
    setSwitching(false);
  }
}
