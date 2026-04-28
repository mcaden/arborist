// App-lifetime subscription wiring for `session://status`.
//
// Phase 8 owns the metadata/status side of the bridge → store glue. PTY
// output is *not* handled here; `session://output` is consumed directly by
// `use-terminal` (Phase 11) so byte streams never round-trip through
// Zustand.
//
// **Do not add a `subscribeToOutput` here.** That bypass is load-bearing —
// re-rendering the React tree on every keystroke would tank performance and
// defeat xterm's own buffering. If you find yourself wanting one, you almost
// certainly want a hook closer to the terminal instead.

import { onSessionStatus } from '@/lib/tauri-bridge';
import { useSessionStore } from '@/store/session-store';

type Unlisten = () => void;

const NOOP_UNLISTEN: Unlisten = () => {};

let attached = false;
let unlistenPromise: Promise<Unlisten> | null = null;

/**
 * Attach the single app-lifetime listener for `session://status` and route
 * each event into the session store's `applyStatus` action.
 *
 * Idempotent: a second call returns a no-op unlisten without re-attaching,
 * so callers can invoke this from React strict-mode double-mounts or any
 * other re-entry without leaking listeners.
 *
 * Returns a function that, when invoked, detaches the listener (and resets
 * the module's internal state so a subsequent call can re-attach — useful
 * for tests).
 */
export function subscribeToStatus(): Unlisten {
  if (attached) return NOOP_UNLISTEN;
  attached = true;

  unlistenPromise = onSessionStatus((payload) => {
    useSessionStore.getState().actions.applyStatus(payload);
  });

  return () => {
    const pending = unlistenPromise;
    attached = false;
    unlistenPromise = null;
    if (!pending) return;
    void pending.then((unlisten) => {
      unlisten();
    });
  };
}

/**
 * Test-only: forcibly detach the active subscription (if any) and reset the
 * module's internal state so a subsequent `subscribeToStatus` call attaches
 * fresh. Production code must use the unlisten returned by `subscribeToStatus`
 * instead.
 */
export function __resetForTests(): void {
  const pending = unlistenPromise;
  attached = false;
  unlistenPromise = null;
  if (!pending) return;
  void pending.then((unlisten) => {
    unlisten();
  });
}
