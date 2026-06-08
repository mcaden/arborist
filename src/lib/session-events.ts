// App-lifetime subscription wiring for `session://status` and
// `session://activity`.
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

import { onSessionActivity, onSessionMetrics, onSessionStatus } from '@/lib/tauri-bridge';
import { useSessionStore } from '@/store/session-store';

type Unlisten = () => void;

const NOOP_UNLISTEN: Unlisten = () => {};

let statusAttached = false;
let statusUnlistenPromise: Promise<Unlisten> | null = null;
let activityAttached = false;
let activityUnlistenPromise: Promise<Unlisten> | null = null;
let metricsAttached = false;
let metricsUnlistenPromise: Promise<Unlisten> | null = null;

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
  if (statusAttached) return NOOP_UNLISTEN;
  statusAttached = true;

  statusUnlistenPromise = onSessionStatus((payload) => {
    useSessionStore.getState().actions.applyStatus(payload);
  });

  return () => {
    const pending = statusUnlistenPromise;
    statusAttached = false;
    statusUnlistenPromise = null;
    if (!pending) return;
    pending.then((unlisten) => {
      unlisten();
    });
  };
}

/**
 * Attach the single app-lifetime listener for `session://activity` and
 * route each event into the session store's `applyActivity` action. Same
 * idempotency contract as {@link subscribeToStatus}.
 */
export function subscribeToActivity(): Unlisten {
  if (activityAttached) return NOOP_UNLISTEN;
  activityAttached = true;

  activityUnlistenPromise = onSessionActivity((payload) => {
    useSessionStore.getState().actions.applyActivity(payload);
  });

  return () => {
    const pending = activityUnlistenPromise;
    activityAttached = false;
    activityUnlistenPromise = null;
    if (!pending) return;
    pending.then((unlisten) => {
      unlisten();
    });
  };
}

/**
 * Attach the single app-lifetime listener for `session://metrics` and route
 * each event into the session store's `applyMetrics` action. Same idempotency
 * contract as {@link subscribeToStatus}.
 */
export function subscribeToMetrics(): Unlisten {
  if (metricsAttached) return NOOP_UNLISTEN;
  metricsAttached = true;

  metricsUnlistenPromise = onSessionMetrics((payload) => {
    useSessionStore.getState().actions.applyMetrics(payload);
  });

  return () => {
    const pending = metricsUnlistenPromise;
    metricsAttached = false;
    metricsUnlistenPromise = null;
    if (!pending) return;
    pending.then((unlisten) => {
      unlisten();
    });
  };
}

/**
 * Test-only: forcibly detach the active subscription (if any) and reset the
 * module's internal state so subsequent `subscribeTo*` calls attach fresh.
 * Production code must use the unlisten returned by `subscribeToStatus`
 * / `subscribeToActivity` instead.
 */
export function __resetForTests(): void {
  const pendingStatus = statusUnlistenPromise;
  const pendingActivity = activityUnlistenPromise;
  const pendingMetrics = metricsUnlistenPromise;
  statusAttached = false;
  statusUnlistenPromise = null;
  activityAttached = false;
  activityUnlistenPromise = null;
  metricsAttached = false;
  metricsUnlistenPromise = null;
  if (pendingStatus) {
    pendingStatus.then((unlisten) => {
      unlisten();
    });
  }
  if (pendingActivity) {
    pendingActivity.then((unlisten) => {
      unlisten();
    });
  }
  if (pendingMetrics) {
    pendingMetrics.then((unlisten) => {
      unlisten();
    });
  }
}
