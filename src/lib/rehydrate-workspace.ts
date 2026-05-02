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
  await useSessionStore.getState().actions.hydrate();
  if (myGen !== rehydrateGen) return;
  await frontendReady();
}
