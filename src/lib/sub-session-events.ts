// App-lifetime subscription wiring for `subsession://status` and
// `subsession://exited`. Mirrors `lib/session-events.ts` exactly:
// idempotent attach, returns an unlisten that resets module state, and
// a `__resetForTests` escape hatch.

import { onSubSessionExited, onSubSessionStatus } from '@/lib/tauri-bridge';
import { useSubSessionStore } from '@/store/sub-session-store';

type Unlisten = () => void;

const NOOP_UNLISTEN: Unlisten = () => {};

let statusAttached = false;
let statusUnlistenPromise: Promise<Unlisten> | null = null;
let exitedAttached = false;
let exitedUnlistenPromise: Promise<Unlisten> | null = null;

/**
 * Attach the single app-lifetime listener for `subsession://status`.
 * Idempotent; returns a function that detaches the listener and resets
 * the module's internal state so a subsequent call can re-attach.
 */
export function subscribeToSubStatus(): Unlisten {
  if (statusAttached) return NOOP_UNLISTEN;
  statusAttached = true;

  statusUnlistenPromise = onSubSessionStatus((payload) => {
    useSubSessionStore.getState().actions.applyStatus(payload);
  });

  return () => {
    const pending = statusUnlistenPromise;
    statusAttached = false;
    statusUnlistenPromise = null;
    if (!pending) return;
    void pending.then((unlisten) => {
      unlisten();
    });
  };
}

/**
 * Attach the single app-lifetime listener for `subsession://exited`.
 * Idempotent. Same contract as {@link subscribeToSubStatus}.
 */
export function subscribeToSubExited(): Unlisten {
  if (exitedAttached) return NOOP_UNLISTEN;
  exitedAttached = true;

  exitedUnlistenPromise = onSubSessionExited((payload) => {
    useSubSessionStore.getState().actions.applyExited(payload);
  });

  return () => {
    const pending = exitedUnlistenPromise;
    exitedAttached = false;
    exitedUnlistenPromise = null;
    if (!pending) return;
    void pending.then((unlisten) => {
      unlisten();
    });
  };
}

/**
 * Test-only: forcibly detach active subscriptions and reset internal
 * state so subsequent `subscribeTo*` calls re-attach fresh.
 */
export function __resetForTests(): void {
  const pendingStatus = statusUnlistenPromise;
  const pendingExited = exitedUnlistenPromise;
  statusAttached = false;
  statusUnlistenPromise = null;
  exitedAttached = false;
  exitedUnlistenPromise = null;
  if (pendingStatus) {
    void pendingStatus.then((unlisten) => {
      unlisten();
    });
  }
  if (pendingExited) {
    void pendingExited.then((unlisten) => {
      unlisten();
    });
  }
}
