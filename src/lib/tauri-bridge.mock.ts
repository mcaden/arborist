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
import type { AppConfig } from '@/types/arborist';

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
> = vi.fn(() => Promise.resolve([]));

export const sessionClose: Mock<
  Parameters<typeof realBridge.sessionClose>,
  ReturnType<typeof realBridge.sessionClose>
> = vi.fn(() => Promise.resolve());

export const sessionFocus: Mock<
  Parameters<typeof realBridge.sessionFocus>,
  ReturnType<typeof realBridge.sessionFocus>
> = vi.fn(() => Promise.resolve());

export const sessionResize: Mock<
  Parameters<typeof realBridge.sessionResize>,
  ReturnType<typeof realBridge.sessionResize>
> = vi.fn(() => Promise.resolve());

export const sessionInput: Mock<
  Parameters<typeof realBridge.sessionInput>,
  ReturnType<typeof realBridge.sessionInput>
> = vi.fn(() => Promise.resolve());

export const sessionRestart: Mock<
  Parameters<typeof realBridge.sessionRestart>,
  ReturnType<typeof realBridge.sessionRestart>
> = vi.fn(() => Promise.resolve());

export const frontendReady: Mock<
  Parameters<typeof realBridge.frontendReady>,
  ReturnType<typeof realBridge.frontendReady>
> = vi.fn(() => Promise.resolve());

// `config_get`/`config_set`/`instructions_list` are real implementations as
// of Phase 4; their default mock behaviour returns benign empty values so
// tests don't need to wire each call individually unless they care.
const defaultAppConfig = (): AppConfig => ({
  configVersion: 3,
  defaultInstructionSets: { claude: '', copilot: '' },
  instructionSetsDir: '',
  workspaceRoot: null,
  worktreeRoots: [],
  prelaunchCommands: [],
  worktreePrelaunchCommands: {},
  lastOpenSessions: [],
  tabOrder: [],
  activeSessionId: null,
});

export const configGet: Mock<
  Parameters<typeof realBridge.configGet>,
  ReturnType<typeof realBridge.configGet>
> = vi.fn(() => Promise.resolve(defaultAppConfig()));

export const configSet: Mock<
  Parameters<typeof realBridge.configSet>,
  ReturnType<typeof realBridge.configSet>
> = vi.fn(() => Promise.resolve());

export const instructionsList: Mock<
  Parameters<typeof realBridge.instructionsList>,
  ReturnType<typeof realBridge.instructionsList>
> = vi.fn(() => Promise.resolve([]));

export const worktreesList: Mock<typeof realBridge.worktreesList> = vi.fn(() =>
  Promise.resolve([]),
);

export const workspaceValidate: Mock<typeof realBridge.workspaceValidate> = vi.fn(() =>
  Promise.resolve({ valid: true }),
);

export const worktreeCreate: Mock<typeof realBridge.worktreeCreate> = vi.fn(rejectNotImplemented);

export const pickDirectory: Mock<typeof realBridge.pickDirectory> = vi.fn(() =>
  Promise.resolve(null),
);

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
  sessionList.mockReset().mockImplementation(() => Promise.resolve([]));
  sessionClose.mockReset().mockImplementation(() => Promise.resolve());
  sessionFocus.mockReset().mockImplementation(() => Promise.resolve());
  sessionResize.mockReset().mockImplementation(() => Promise.resolve());
  sessionInput.mockReset().mockImplementation(() => Promise.resolve());
  sessionRestart.mockReset().mockImplementation(() => Promise.resolve());
  frontendReady.mockReset().mockImplementation(() => Promise.resolve());
  configGet.mockReset().mockImplementation(() => Promise.resolve(defaultAppConfig()));
  configSet.mockReset().mockImplementation(() => Promise.resolve());
  instructionsList.mockReset().mockImplementation(() => Promise.resolve([]));
  worktreesList.mockReset().mockImplementation(() => Promise.resolve([]));
  workspaceValidate.mockReset().mockImplementation(() => Promise.resolve({ valid: true }));
  worktreeCreate.mockReset().mockImplementation(rejectNotImplemented);
  pickDirectory.mockReset().mockImplementation(() => Promise.resolve(null));
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
  frontendReady,
  configGet,
  configSet,
  instructionsList,
  worktreesList,
  workspaceValidate,
  worktreeCreate,
  pickDirectory,
  onSessionOutput,
  onSessionStatus,
} satisfies typeof realBridge;
void _shapeCheck;
