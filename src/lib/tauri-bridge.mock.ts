// Vitest mock counterpart of `./tauri-bridge.ts`. Tests that exercise code
// importing the bridge should call:
//
//   vi.mock('@/lib/tauri-bridge', async () =>
//     await import('@/lib/tauri-bridge.mock'),
//   );
//
// in their setup and then reach for individual `vi.fn()`s on the imported
// module to set return values per test. `resetBridgeMocks()` clears every
// mock's call history and any configured implementation between tests.
//
// The `satisfies typeof realBridge` assertion at the bottom of this file
// prevents the mock from drifting away from the real bridge: adding a new
// export to `tauri-bridge.ts` without mirroring it here is a TypeScript
// compile error.

import { vi, type Mock } from 'vitest';
import type { UnlistenFn } from '@tauri-apps/api/event';

import type * as realBridge from './tauri-bridge';

// Every command stub rejects by default so a forgotten `mockResolvedValue`
// in a test surfaces the same way it would in production today.
const rejectNotImplemented = (): Promise<never> => Promise.reject(new Error('not implemented'));

// Default unlisten is a no-op so subscribers don't blow up on cleanup.
const noopUnlisten: UnlistenFn = () => {};

export const ping: Mock<
  Parameters<typeof realBridge.ping>,
  ReturnType<typeof realBridge.ping>
> = vi.fn(() => Promise.resolve('pong'));

export const sessionCreate: Mock<
  Parameters<typeof realBridge.sessionCreate>,
  ReturnType<typeof realBridge.sessionCreate>
> = vi.fn(rejectNotImplemented);

export const sessionList: Mock<
  Parameters<typeof realBridge.sessionList>,
  ReturnType<typeof realBridge.sessionList>
> = vi.fn(rejectNotImplemented);

export const sessionClose: Mock<
  Parameters<typeof realBridge.sessionClose>,
  ReturnType<typeof realBridge.sessionClose>
> = vi.fn(rejectNotImplemented);

export const sessionFocus: Mock<
  Parameters<typeof realBridge.sessionFocus>,
  ReturnType<typeof realBridge.sessionFocus>
> = vi.fn(rejectNotImplemented);

export const sessionResize: Mock<
  Parameters<typeof realBridge.sessionResize>,
  ReturnType<typeof realBridge.sessionResize>
> = vi.fn(rejectNotImplemented);

export const sessionInput: Mock<
  Parameters<typeof realBridge.sessionInput>,
  ReturnType<typeof realBridge.sessionInput>
> = vi.fn(rejectNotImplemented);

export const sessionRestart: Mock<
  Parameters<typeof realBridge.sessionRestart>,
  ReturnType<typeof realBridge.sessionRestart>
> = vi.fn(rejectNotImplemented);

export const configGet: Mock<
  Parameters<typeof realBridge.configGet>,
  ReturnType<typeof realBridge.configGet>
> = vi.fn(rejectNotImplemented);

export const configSet: Mock<
  Parameters<typeof realBridge.configSet>,
  ReturnType<typeof realBridge.configSet>
> = vi.fn(rejectNotImplemented);

export const instructionsList: Mock<
  Parameters<typeof realBridge.instructionsList>,
  ReturnType<typeof realBridge.instructionsList>
> = vi.fn(rejectNotImplemented);

export const onSessionOutput: Mock<
  Parameters<typeof realBridge.onSessionOutput>,
  ReturnType<typeof realBridge.onSessionOutput>
> = vi.fn(() => Promise.resolve(noopUnlisten));

export const onSessionStatus: Mock<
  Parameters<typeof realBridge.onSessionStatus>,
  ReturnType<typeof realBridge.onSessionStatus>
> = vi.fn(() => Promise.resolve(noopUnlisten));

// Re-export the bridge's argument-shape interfaces so consumers importing
// from the mock get identical types.
export type {
  SessionCreateArgs,
  SessionIdArg,
  SessionResizeArgs,
  SessionInputArgs,
} from './tauri-bridge';

/**
 * Reset every mock function's call history AND restore its default
 * implementation. Tests should call this in `beforeEach`.
 */
export function resetBridgeMocks(): void {
  ping.mockReset().mockImplementation(() => Promise.resolve('pong'));
  sessionCreate.mockReset().mockImplementation(rejectNotImplemented);
  sessionList.mockReset().mockImplementation(rejectNotImplemented);
  sessionClose.mockReset().mockImplementation(rejectNotImplemented);
  sessionFocus.mockReset().mockImplementation(rejectNotImplemented);
  sessionResize.mockReset().mockImplementation(rejectNotImplemented);
  sessionInput.mockReset().mockImplementation(rejectNotImplemented);
  sessionRestart.mockReset().mockImplementation(rejectNotImplemented);
  configGet.mockReset().mockImplementation(rejectNotImplemented);
  configSet.mockReset().mockImplementation(rejectNotImplemented);
  instructionsList.mockReset().mockImplementation(rejectNotImplemented);
  onSessionOutput.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  onSessionStatus.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
}

// Compile-time guard: this module must export every member of the real
// bridge with a compatible type. Adding a new export to tauri-bridge.ts
// without mirroring it here is a TypeScript error.
const _shapeCheck = {
  ping,
  sessionCreate,
  sessionList,
  sessionClose,
  sessionFocus,
  sessionResize,
  sessionInput,
  sessionRestart,
  configGet,
  configSet,
  instructionsList,
  onSessionOutput,
  onSessionStatus,
} satisfies typeof realBridge;
void _shapeCheck;
