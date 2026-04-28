// Typed wrappers around Tauri's `invoke()` and `listen()` APIs. All
// frontend code MUST go through this module — never import from
// `@tauri-apps/api` directly. This is the single source of truth for the
// frontend half of the command/event contract documented in
// `dev/docs/DESIGN.md` §6.
//
// Phase 3 status: only `ping` is implemented. Every other command listed in
// DESIGN §6 is stubbed with `Promise.reject(new Error('not implemented'))`
// so callers can be written and typed against the final shape; later phases
// will flip stubs to real implementations one at a time.
//
// The mock counterpart in `./tauri-bridge.mock.ts` is structurally
// substitutable — see the `satisfies typeof import(...)` check in that
// file.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import type {
  AppConfig,
  InstructionSet,
  InstructionSetId,
  PartialAppConfig,
  Session,
  SessionId,
  SessionOutputEvent,
  SessionStatusEvent,
  SessionView,
  Tool,
} from '@/types/grove';

// ---------------------------------------------------------------------------
// Command argument shapes
//
// MIRROR: the keys here mirror the payload column of DESIGN §6. When a real
// implementation lands in a later phase, the matching Rust `#[serde(...)]`
// payload struct in `src-tauri/src/types.rs` (or a sibling) becomes the
// canonical definition and these aliases must be re-checked.
// ---------------------------------------------------------------------------

export interface SessionCreateArgs {
  tool: Tool;
  worktreePath: string;
  instructionSetId: InstructionSetId;
}

export interface SessionIdArg {
  sessionId: SessionId;
}

export interface SessionResizeArgs {
  sessionId: SessionId;
  cols: number;
  rows: number;
}

export interface SessionInputArgs {
  sessionId: SessionId;
  data: string;
}

// ---------------------------------------------------------------------------
// Smoke-test command
// ---------------------------------------------------------------------------

/**
 * Round-trips through the Tauri command boundary, returning `'pong'`.
 * Used in development and tests to verify the RPC scaffold is wired.
 */
export function ping(): Promise<string> {
  return invoke<string>('ping');
}

// ---------------------------------------------------------------------------
// Stubs for every command in DESIGN §6.
//
// Each stub returns `Promise.reject(new Error('not implemented'))` so that:
//   - calling code can be written and typed today,
//   - replacing the stub with a real `invoke(...)` call in a later phase
//     is a one-line change per command,
//   - the parametrised test in `tauri-bridge.test.ts` will start failing
//     once a real implementation lands — flagging which assertion to flip.
// ---------------------------------------------------------------------------

const NOT_IMPLEMENTED = (): Promise<never> => Promise.reject(new Error('not implemented'));

export function sessionCreate(_args: SessionCreateArgs): Promise<Session> {
  return NOT_IMPLEMENTED();
}

export function sessionList(): Promise<SessionView[]> {
  return NOT_IMPLEMENTED();
}

export function sessionClose(_args: SessionIdArg): Promise<void> {
  return NOT_IMPLEMENTED();
}

export function sessionFocus(_args: SessionIdArg): Promise<void> {
  return NOT_IMPLEMENTED();
}

export function sessionResize(_args: SessionResizeArgs): Promise<void> {
  return NOT_IMPLEMENTED();
}

export function sessionInput(_args: SessionInputArgs): Promise<void> {
  return NOT_IMPLEMENTED();
}

export function sessionRestart(_args: SessionIdArg): Promise<void> {
  return NOT_IMPLEMENTED();
}

/**
 * Returns the persisted [`AppConfig`]. Path fields are canonicalized by the
 * backend; missing instruction-set IDs are silently rewritten to the
 * discovered default for the relevant tool.
 */
export function configGet(): Promise<AppConfig> {
  return invoke<AppConfig>('config_get');
}

/**
 * Deep-merges `partial` into the persisted [`AppConfig`]. Only the fields
 * present on `partial` (i.e. not `undefined`) are touched; the rest survive
 * unchanged. Backend may reject relative paths with an `InvalidPath` error.
 */
export function configSet(partial: PartialAppConfig): Promise<void> {
  return invoke<void>('config_set', { partial });
}

/**
 * Discovers and returns the list of [`InstructionSet`]s under the configured
 * `instructionSetsDir`. Files exceeding 1 MiB or escaping the directory via
 * symlink are skipped.
 */
export function instructionsList(): Promise<InstructionSet[]> {
  return invoke<InstructionSet[]>('instructions_list');
}

// ---------------------------------------------------------------------------
// Event subscribers
//
// The returned `UnlistenFn` MUST be called by the consumer on cleanup
// (typically inside a `useEffect` cleanup function). Forgetting to do so
// leaks the listener for the lifetime of the WebView.
// ---------------------------------------------------------------------------

export function onSessionOutput(cb: (payload: SessionOutputEvent) => void): Promise<UnlistenFn> {
  return listen<SessionOutputEvent>('session://output', (event) => cb(event.payload));
}

export function onSessionStatus(cb: (payload: SessionStatusEvent) => void): Promise<UnlistenFn> {
  return listen<SessionStatusEvent>('session://status', (event) => cb(event.payload));
}
