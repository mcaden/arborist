// See `session.ts` for why this fixture is `.ts` rather than `.json`
// (TypeScript widens JSON-import literal types, defeating `satisfies`
// against the nested `AppConfig` and `SessionView` shapes which carry
// tagged-union `tool` / `status` literals). PR5 added the nested
// `config` + `sessions` fields when the switch became atomic — the
// migration was forced at that point.

import type { WorkspaceSwitchResult } from '../arborist';

export const workspaceSwitchResultFixture = {
  workspaceRoot: '/tmp/repo',
  noOp: false,
  config: {
    configVersion: 10,
    defaultInstructionSets: {
      claude: 'claude-default',
      copilot: 'copilot-default',
    },
    instructionSetsDir: '/cfg/instructions',
    workspaceRoot: '/tmp/repo',
    worktreeRoots: ['/tmp/repo'],
    worktreePrepCommands: [],
    aiLaunchCommands: { commands: {}, iconDataUris: {} },
    pluginSettings: { ai: {}, customProcess: {}, dashboardWidget: {} },
    repoCommandTrust: { records: {} },
    lastOpenSessions: [],
    tabOrder: [],
    activeSessionId: null,
    customProcesses: [],
    lastOpenSubSessions: [],
    worktreeTabs: [],
    worktreeTabOrder: [],
    activeWorktreeTabId: null,
  },
  sessions: [],
} as const satisfies WorkspaceSwitchResult;
