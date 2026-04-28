// Tauri error formatting utilities — kept in a separate module so that
// `tauri-bridge.mock.ts` can import these pure helpers WITHOUT importing
// `tauri-bridge.ts` itself. Importing the real bridge from inside the mock
// factory creates a circular dependency that deadlocks Vitest's module
// resolver when `vi.mock('@/lib/tauri-bridge', async () => ...)` is used.
//
// MIRROR: src-tauri/src/types.rs::AppError

export interface AppErrorLike {
  code: string;
  message: string;
}

export function isAppErrorLike(value: unknown): value is AppErrorLike {
  if (value === null || typeof value !== 'object') return false;
  const v = value as Record<string, unknown>;
  return typeof v.code === 'string' && typeof v.message === 'string';
}

/**
 * Normalises any thrown value (Error, AppError payload, string, unknown) to a
 * human-readable string suitable for inline UI display.
 *
 * Tauri commands reject with the serialised `AppError` payload defined in
 * `src-tauri/src/types.rs` — a plain `{ code, message }` object. Calling
 * `String(err)` on such an object yields `"[object Object]"`, which is what
 * users would otherwise see when e.g. a `copilot` session failed to spawn.
 */
export function formatError(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (isAppErrorLike(err)) {
    return err.code.length > 0 ? `${err.code}: ${err.message}` : err.message;
  }
  if (typeof err === 'string') return err;
  if (err === null || err === undefined) return 'Unknown error';
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}
