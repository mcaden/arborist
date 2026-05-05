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
// Concurrency: a previous post-await generation guard was insufficient
// because both `configStore.hydrate()` and `sessionStore.hydrate()`
// `set(...)` their Zustand stores **as part of resolving** their
// promises. The guard ran *after* the await and so could not prevent
// an older (slower) rehydrate from overwriting the store with stale
// data after a newer rehydrate had already settled — two rapid
// workspace switches could leave the UI pointed at the previous
// workspace's config/sessions even though both rehydrates "completed
// successfully".
//
// We instead **serialize** rehydrate calls on a Promise chain and
// **coalesce** any calls that arrive while one is in flight. Each
// caller atomically claims a monotonic position; when its turn comes,
// it checks whether a newer caller has been submitted in the meantime
// and skips its own work if so (the newer caller's results would
// overwrite ours anyway, so the intermediate hydrate is pure waste +
// UI flicker). The newest submission always runs.
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

// Serial chain of rehydrate work. Every call appends to it so at most
// one rehydrate's `set(...)` calls land in the stores at a time.
let chain: Promise<void> = Promise.resolve();

// Monotonic submission counter. A queued rehydrate compares its own
// position against this when it gets dequeued: if a newer one has
// been submitted, the current run skips entirely (its data would be
// immediately overwritten by the newer run, and skipping avoids the
// intermediate `set` storm + UI flicker).
let submitted = 0;

/**
 * Re-fetch config + sessions from the backend and re-issue
 * `frontend_ready`. Safe to call concurrently from multiple sources:
 * calls are serialized, and any call superseded by a newer one before
 * its turn comes up is skipped. The returned promise still resolves
 * (or rejects with the running call's error) when the call's "slot"
 * in the chain settles — coalesced callers never see a result older
 * than what they would have produced by running themselves.
 *
 * Errors from any stage propagate to the caller — neither call site
 * treats rehydrate failure as fatal, but each handles logging on its
 * own terms. A failing run does not break the chain: subsequent runs
 * still execute.
 */
export async function rehydrateActiveWorkspace(): Promise<void> {
  submitted += 1;
  const myGen = submitted;
  const next = chain.then(async () => {
    // A newer rehydrate has been submitted while we were queued. Skip
    // ours — running it would just be undone by the newer one and
    // briefly flash the wrong workspace in the UI on the way through.
    if (myGen < submitted) return;
    await useConfigStore.getState().hydrate();
    await frontendReady();
    await useSessionStore.getState().actions.hydrate();
  });
  // Swallow errors when extending the chain so a failing run does not
  // leave the chain in a permanently-rejected state. Callers still
  // receive the original `next` promise (with its error) for logging.
  chain = next.catch(() => {});
  return next;
}
