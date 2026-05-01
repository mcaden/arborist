// useSubSessionIcon — best-effort fetch of the OS application icon
// for an `application`-kind sub-session, returned as a data URI for
// direct use in an `<img src=…>`.
//
// ## Why this isn't part of `sub-session-store`
//
// Icons are derived state (PID → bytes), not part of the persisted
// sub-session record, and the fetch is async. Putting them in the
// store would either bloat every status update with image data or
// require parallel state plumbing. A self-contained hook with local
// `useState` keeps the concern isolated and avoids re-fetching on
// every store change unrelated to the icon.
//
// ## Late-response protection
//
// The VS Code retarget flow can change a sub-session's PID under
// our feet: the launcher reports `Running(pid=launcher)` and a
// second `Running(pid=Code.exe)` arrives ~1-8s later. If we naively
// kept whichever response resolves last, a slow lookup against the
// (now-dead) launcher PID could overwrite the correct Code.exe icon.
//
// Two guards:
//   1. Per-effect `cancelled` flag — set on cleanup so unmount /
//      re-render with new deps discards in-flight responses.
//   2. After the response arrives, we re-read the store and ignore
//      the result if `sub.pid` no longer matches the PID we queried.
//
// Combined, these make the hook eventually consistent with the
// current PID even under concurrent retargets.

import { useEffect, useState } from 'react';
import { subSessionIcon } from '@/lib/tauri-bridge';
import { useSubSessionById, useSubSessionStore } from '@/store/sub-session-store';
import type { SubSessionId } from '@/types/arborist';

/**
 * Returns a data URI for the application icon, or `undefined` while
 * loading / when no icon is available. Safe for terminal sub-sessions
 * (returns `undefined` and skips the fetch).
 */
export function useSubSessionIcon(id: SubSessionId | undefined): string | undefined {
  const sub = useSubSessionById(id);
  const [icon, setIcon] = useState<string | undefined>(undefined);
  const pid = sub?.pid;
  const status = sub?.status;
  const kind = sub?.kind;

  useEffect(() => {
    // Reset whenever the underlying pid disappears / changes so the
    // user never sees a stale icon during transitions.
    setIcon(undefined);
    if (!id || kind !== 'application' || !pid || status !== 'running') {
      return;
    }
    const targetPid = pid;
    let cancelled = false;
    void subSessionIcon(id)
      .then((result) => {
        if (cancelled) return;
        // Re-read store: if the pid changed mid-flight (e.g. VS Code
        // retarget), discard. The next effect run will refetch
        // against the new pid.
        const current = useSubSessionStore.getState().subSessions.find((s) => s.id === id);
        if (!current || current.pid !== targetPid) return;
        if (result) setIcon(result);
      })
      .catch(() => {
        // Best-effort: swallow errors and keep the emoji fallback.
        // Logging here would spam the console for the common
        // platform-unsupported case.
      });
    return () => {
      cancelled = true;
    };
  }, [id, pid, status, kind]);

  return icon;
}
