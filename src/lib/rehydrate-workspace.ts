// Shared "re-fetch all workspace-derived state" routine used by both
// the `workspace://changed` event handler in `App.tsx` and the
// `changeWorkspace` flow in `lib/workspace-switch.ts`.
//
// Why a shared module: the backend's `workspace_switch` command emits
// `workspace://changed` on success, but the emit is best-effort (the
// Rust side only logs on failure and the API still resolves Ok).
// Without a frontend-driven fallback, an emit failure would leave the
// UI pointed at the old workspace even though the backend has swapped.
// Both call sites must drive the same rehydrate to converge.
//
// Concurrency: a monotonic generation counter lives at module scope
// (so both call sites share it). Each invocation bumps the gen, copies
// it locally, and bails after every `await` if a newer rehydrate has
// superseded it. This prevents a slow rehydrate (e.g. backend hung on
// the first `configGet`) from overwriting Zustand state with stale
// data after a faster, later rehydrate has already settled — which
// would silently leave the UI showing one workspace's sessions while
// the backend was bound to another.
//
// Step ordering — IMPORTANT (regression: parked sessions never
// resumed after switch-back):
//   1. configStore.hydrate()  — frontend-only state; no UI
//      session-list mount happens here.
//   2. frontendReady()        — backend awaits `restore_all_sessions`
//      to completion, which **populates `pending_spawn`** for every
//      session that should come back. We MUST do this before step 3.
//   3. sessionStore.hydrate() — pulls the new workspace's session
//      list into Zustand. React renders `MainArea`, mounts the new
//      `TerminalView`s, which `attach` → `refit` → fire the first
//      `session_resize`. Because `pending_spawn` is already populated
//      by step 2, the backend's `session_resize_impl` finds the
//      session waiting and triggers the deferred PTY spawn.
//
// If step 3 ran before step 2, the first `session_resize` would race
// `restore_all_sessions`: `pending_spawn` is empty at that moment, the
// session isn't in the pool either, so `pool.resize` returns
// `NotFound` and the spawn is never triggered. The session sits at
// `Starting` forever — the symptom users saw as "tabs appear with the
// starting spinner but the AI never resumes". (At app boot the same
// sequence is safe because `MainArea` is hidden behind `BootSplash`
// until `setStatus('ready')` runs after `frontendReady`; only the
// runtime workspace-switch path hits this race because the UI is
// already `ready`.)

import { frontendReady } from '@/lib/tauri-bridge';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

let rehydrateGen = 0;

/**
 * Re-fetch config + sessions from the backend and re-issue
 * `frontend_ready`. Safe to call concurrently from multiple sources:
 * a stale (slower) call bails after each await once a newer call has
 * bumped the generation counter, leaving only the freshest result in
 * the Zustand stores.
 *
 * Awaits to completion. Errors from any stage propagate to the
 * caller — neither call site treats rehydrate failure as fatal, but
 * each handles logging on its own terms.
 */
export async function rehydrateActiveWorkspace(): Promise<void> {
  rehydrateGen += 1;
  const myGen = rehydrateGen;
  await useConfigStore.getState().hydrate();
  if (myGen !== rehydrateGen) return;
  // Drive `restore_all_sessions` BEFORE updating the session-store so
  // `pending_spawn` is populated before React mounts any new
  // TerminalView. See the step-ordering note in the module header.
  await frontendReady();
  if (myGen !== rehydrateGen) return;
  await useSessionStore.getState().actions.hydrate();
}
