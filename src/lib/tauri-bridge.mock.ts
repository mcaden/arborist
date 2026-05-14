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

// Import pure error helpers from the utility module — NOT from `./tauri-bridge`
// itself. Importing the real bridge here would create a circular dependency:
// the vi.mock factory loads this file, which would try to load `./tauri-bridge`,
// which is the module being mocked, deadlocking Vitest's module resolver.
import * as tauriError from '@/lib/tauri-error';
import type * as realBridge from './tauri-bridge';
import type { AppConfig, WorktreeGitStatus } from '@/types/arborist';

// Pure helpers (no Tauri side effects) — re-export so tests get the same
// formatting as production. Indirected through `tauriError.*` instead of named
// re-export so vitest 4 + vite 8 cannot tree-shake / strip the bindings before
// the mocked bridge is consumed by the test subject.
export const formatError: typeof tauriError.formatError = (err) => tauriError.formatError(err);
export const isAppErrorLike: typeof tauriError.isAppErrorLike = (v): v is tauriError.AppErrorLike => tauriError.isAppErrorLike(v);
export type { AppErrorLike } from '@/lib/tauri-error';

// Every command stub rejects by default so a forgotten `mockResolvedValue`
// in a test surfaces the same way it would in production today.
const rejectNotImplemented = (): Promise<never> => Promise.reject(new Error('not implemented'));

// Default unlisten is a no-op so subscribers don't blow up on cleanup.
const noopUnlisten: UnlistenFn = () => {};

export const ping: Mock<typeof realBridge.ping> = vi.fn(() => Promise.resolve('pong'));

export const sessionCreate: Mock<typeof realBridge.sessionCreate> = vi.fn(rejectNotImplemented);

export const sessionList: Mock<typeof realBridge.sessionList> = vi.fn(() => Promise.resolve([]));

export const sessionClose: Mock<typeof realBridge.sessionClose> = vi.fn(() => Promise.resolve({ worktreeDeleteError: null }));

export const sessionFocus: Mock<typeof realBridge.sessionFocus> = vi.fn(() => Promise.resolve());

export const sessionResize: Mock<typeof realBridge.sessionResize> = vi.fn(() => Promise.resolve());

export const sessionInput: Mock<typeof realBridge.sessionInput> = vi.fn(() => Promise.resolve());

export const sessionRestart: Mock<typeof realBridge.sessionRestart> = vi.fn(() => Promise.resolve());

export const frontendReady: Mock<typeof realBridge.frontendReady> = vi.fn(() => Promise.resolve());

// `config_get`/`config_set` are real implementations; their default mock behaviour returns benign empty values so tests don't need to wire each call
// individually unless they care.
const defaultAppConfig = (): AppConfig => ({
  configVersion: 11,
  // Tests assume the main UI is reachable by default. The first-boot
  // picker is exercised explicitly when a test overrides this to `null`.
  workspaceRoot: '/mock/workspace',
  worktreeRoots: [],
  worktreePrepCommands: [],
  aiLaunchCommands: { commands: {}, iconDataUris: {} },
  pluginSettings: { ai: {}, customProcess: {}, dashboardWidget: {} },
  lastOpenSessions: [],
  tabOrder: [],
  activeSessionId: null,
  customProcesses: [],
  lastOpenSubSessions: [],
  worktreeTabs: [],
  worktreeTabOrder: [],
  activeWorktreeTabId: null,
  theme: 'system',
});

export const configGet: Mock<typeof realBridge.configGet> = vi.fn(() => Promise.resolve(defaultAppConfig()));

export const configSet: Mock<typeof realBridge.configSet> = vi.fn(() => Promise.resolve(defaultAppConfig()));

export const worktreesList: Mock<typeof realBridge.worktreesList> = vi.fn(() => Promise.resolve([]));

const defaultGitStatus = (): WorktreeGitStatus => ({
  ahead: 0,
  behind: 0,
  staged: 0,
  unstaged: 0,
  untracked: 0,
  conflicted: 0,
  files: [],
  filesTruncated: false,
});

export const worktreeGitStatus: Mock<typeof realBridge.worktreeGitStatus> = vi.fn(() => Promise.resolve(defaultGitStatus()));

export const workspaceValidate: Mock<typeof realBridge.workspaceValidate> = vi.fn(() => Promise.resolve({ valid: true }));

export const worktreeCreate: Mock<typeof realBridge.worktreeCreate> = vi.fn(rejectNotImplemented);

export const workspaceSwitch: Mock<typeof realBridge.workspaceSwitch> = vi.fn(rejectNotImplemented);

export const pickDirectory: Mock<typeof realBridge.pickDirectory> = vi.fn(() => Promise.resolve(null));

export const onSessionOutput: Mock<typeof realBridge.onSessionOutput> = vi.fn(() => Promise.resolve(noopUnlisten));

export const onSessionStatus: Mock<typeof realBridge.onSessionStatus> = vi.fn(() => Promise.resolve(noopUnlisten));

export const onSessionActivity: Mock<typeof realBridge.onSessionActivity> = vi.fn(() => Promise.resolve(noopUnlisten));

export const onSessionMetrics: Mock<typeof realBridge.onSessionMetrics> = vi.fn(() => Promise.resolve(noopUnlisten));

// Phase 2: sub-session command/event mocks.
export const subSessionCreate: Mock<typeof realBridge.subSessionCreate> = vi.fn(rejectNotImplemented);

export const subSessionClose: Mock<typeof realBridge.subSessionClose> = vi.fn(() => Promise.resolve());

export const subSessionFocus: Mock<typeof realBridge.subSessionFocus> = vi.fn(() => Promise.resolve());

export const subSessionList: Mock<typeof realBridge.subSessionList> = vi.fn(() => Promise.resolve([]));

export const subSessionInput: Mock<typeof realBridge.subSessionInput> = vi.fn(() => Promise.resolve());

export const subSessionResize: Mock<typeof realBridge.subSessionResize> = vi.fn(() => Promise.resolve());

export const subSessionRelaunch: Mock<typeof realBridge.subSessionRelaunch> = vi.fn(rejectNotImplemented);

export const subSessionIcon: Mock<typeof realBridge.subSessionIcon> = vi.fn(() => Promise.resolve(null));

export const onSubSessionStatus: Mock<typeof realBridge.onSubSessionStatus> = vi.fn(() => Promise.resolve(noopUnlisten));

export const onSubSessionExited: Mock<typeof realBridge.onSubSessionExited> = vi.fn(() => Promise.resolve(noopUnlisten));

export const onSubSessionRestored: Mock<typeof realBridge.onSubSessionRestored> = vi.fn(() => Promise.resolve(noopUnlisten));

// ---- Worktree tab commands (Issue #44) ----

export const worktreeTabOpen: Mock<typeof realBridge.worktreeTabOpen> = vi.fn(rejectNotImplemented);
export const worktreeTabClose: Mock<typeof realBridge.worktreeTabClose> = vi.fn(() => Promise.resolve({ childErrors: [] }));
export const worktreeTabFocus: Mock<typeof realBridge.worktreeTabFocus> = vi.fn(() => Promise.resolve());
export const worktreeTabList: Mock<typeof realBridge.worktreeTabList> = vi.fn(() => Promise.resolve([]));
export const worktreeTabReorder: Mock<typeof realBridge.worktreeTabReorder> = vi.fn(() => Promise.resolve());
export const worktreeTabSetActiveChild: Mock<typeof realBridge.worktreeTabSetActiveChild> = vi.fn(() => Promise.resolve());

export const worktreePrepOpenLog: Mock<typeof realBridge.worktreePrepOpenLog> = vi.fn(() => Promise.resolve());

export const onWorktreePrep: Mock<typeof realBridge.onWorktreePrep> = vi.fn(() => Promise.resolve(noopUnlisten));

// Re-export the bridge's argument-shape interfaces so consumers importing
// from the mock get identical types.
export type { SessionCreateArgs, SessionIdArg, SessionCloseArgs, SessionCloseResult, SessionResizeArgs, SessionInputArgs } from './tauri-bridge';

/**
 * Reset every mock function's call history AND restore its default
 * implementation. Tests should call this in `beforeEach`.
 */
export function resetBridgeMocks(): void {
  ping.mockReset().mockImplementation(() => Promise.resolve('pong'));
  sessionCreate.mockReset().mockImplementation(rejectNotImplemented);
  sessionList.mockReset().mockImplementation(() => Promise.resolve([]));
  sessionClose.mockReset().mockImplementation(() => Promise.resolve({ worktreeDeleteError: null }));
  sessionFocus.mockReset().mockImplementation(() => Promise.resolve());
  sessionResize.mockReset().mockImplementation(() => Promise.resolve());
  sessionInput.mockReset().mockImplementation(() => Promise.resolve());
  sessionRestart.mockReset().mockImplementation(() => Promise.resolve());
  frontendReady.mockReset().mockImplementation(() => Promise.resolve());
  configGet.mockReset().mockImplementation(() => Promise.resolve(defaultAppConfig()));
  configSet.mockReset().mockImplementation(() => Promise.resolve(defaultAppConfig()));
  worktreesList.mockReset().mockImplementation(() => Promise.resolve([]));
  worktreeGitStatus.mockReset().mockImplementation(() => Promise.resolve(defaultGitStatus()));
  workspaceValidate.mockReset().mockImplementation(() => Promise.resolve({ valid: true }));
  worktreeCreate.mockReset().mockImplementation(rejectNotImplemented);
  workspaceSwitch.mockReset().mockImplementation(rejectNotImplemented);
  pickDirectory.mockReset().mockImplementation(() => Promise.resolve(null));
  onSessionOutput.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  onSessionStatus.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  onSessionActivity.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  onSessionMetrics.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  subSessionCreate.mockReset().mockImplementation(rejectNotImplemented);
  subSessionClose.mockReset().mockImplementation(() => Promise.resolve());
  subSessionFocus.mockReset().mockImplementation(() => Promise.resolve());
  subSessionList.mockReset().mockImplementation(() => Promise.resolve([]));
  subSessionInput.mockReset().mockImplementation(() => Promise.resolve());
  subSessionResize.mockReset().mockImplementation(() => Promise.resolve());
  subSessionRelaunch.mockReset().mockImplementation(rejectNotImplemented);
  subSessionIcon.mockReset().mockImplementation(() => Promise.resolve(null));
  onSubSessionStatus.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  onSubSessionExited.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  onSubSessionRestored.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
  worktreeTabOpen.mockReset().mockImplementation(rejectNotImplemented);
  worktreeTabClose.mockReset().mockImplementation(() => Promise.resolve({ childErrors: [] }));
  worktreeTabFocus.mockReset().mockImplementation(() => Promise.resolve());
  worktreeTabList.mockReset().mockImplementation(() => Promise.resolve([]));
  worktreeTabReorder.mockReset().mockImplementation(() => Promise.resolve());
  worktreeTabSetActiveChild.mockReset().mockImplementation(() => Promise.resolve());
  worktreePrepOpenLog.mockReset().mockImplementation(() => Promise.resolve());
  onWorktreePrep.mockReset().mockImplementation(() => Promise.resolve(noopUnlisten));
}

// Compile-time guard: this module must export every member of the real
// bridge with a compatible type. Adding a new export to tauri-bridge.ts
// without mirroring it here is a TypeScript error.
const _shapeCheck = {
  formatError,
  isAppErrorLike,
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
  worktreesList,
  worktreeGitStatus,
  workspaceValidate,
  worktreeCreate,
  workspaceSwitch,
  pickDirectory,
  onSessionOutput,
  onSessionStatus,
  onSessionActivity,
  onSessionMetrics,
  subSessionCreate,
  subSessionClose,
  subSessionFocus,
  subSessionList,
  subSessionInput,
  subSessionResize,
  subSessionRelaunch,
  subSessionIcon,
  onSubSessionStatus,
  onSubSessionExited,
  onSubSessionRestored,
  worktreeTabOpen,
  worktreeTabClose,
  worktreeTabFocus,
  worktreeTabList,
  worktreeTabReorder,
  worktreeTabSetActiveChild,
  worktreePrepOpenLog,
  onWorktreePrep,
} satisfies typeof realBridge;
void _shapeCheck;
