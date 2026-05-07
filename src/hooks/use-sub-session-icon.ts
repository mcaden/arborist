// useSubSessionIcon — returns the cached app-icon data URI for a
// sub-session by looking up the backing `CustomProcessDef.iconDataUri`
// in `useConfigStore`.
//
// The icon is resolved & cached at config-save time and at app
// startup by the backend's `backfill_icons` pass (see
// `src-tauri/src/icon_backfill.rs`). That keeps the render path
// synchronous and avoids the wrapper-PID flicker the runtime
// PID-based extractor used to suffer from.
//
// Returns `undefined` when:
//   * the sub-session id is unknown,
//   * the def has been deleted (rare but possible — sub-sessions
//     can outlive their def while running),
//   * resolution failed at backfill time (e.g. shell built-ins like
//     `cd`, or generic interpreter wrappers like `node.exe` —
//     callers fall back to an emoji glyph or bundled SVG).

import { useConfigStore } from '@/store/config-store';
import { useSubSessionById } from '@/store/sub-session-store';
import type { SubSessionId } from '@/types/arborist';

/** Returns the resolved icon data URI for the sub-session's def, if any. */
export function useSubSessionIcon(id: SubSessionId | undefined): string | undefined {
  const sub = useSubSessionById(id);
  const defId = sub?.defId;
  // Subscribe to a single field so we don't re-render on every
  // unrelated config update.
  return useConfigStore((s) => (defId ? s.config.customProcesses.find((d) => d.id === defId)?.iconDataUri : undefined));
}
