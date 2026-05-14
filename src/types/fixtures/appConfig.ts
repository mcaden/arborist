// Hoisted to `.ts` (rather than `.json`) so the discriminator literals
// inside `customProcesses[].kind` and `lastOpenSubSessions[].kind` keep
// their narrowed types — JSON imports widen `"terminal"` to `string`,
// which would silently break the `satisfies CustomProcessDef` check
// inside the union.

import type { AppConfig } from '../arborist';

export const appConfigFixture = {
  configVersion: 10,
  defaultInstructionSets: {
    claude: 'claude-default',
    copilot: 'copilot-default',
  },
  instructionSetsDir: '/cfg/instructions',
  workspaceRoot: '/repo',
  worktreeRoots: ['/repo'],
  worktreePrepCommands: ['npm install', 'cargo build'],
  aiLaunchCommands: {
    commands: {},
    iconDataUris: {},
  },
  pluginSettings: {
    ai: {
      claude: {
        enabled: true,
        settings: {
          launchCommand: 'npx claude',
        },
      },
    },
    customProcess: {},
    dashboardWidget: {},
  },
  lastOpenSessions: ['550e8400-e29b-41d4-a716-446655440000'],
  tabOrder: ['550e8400-e29b-41d4-a716-446655440000'],
  activeSessionId: '550e8400-e29b-41d4-a716-446655440000',
  customProcesses: [
    {
      id: 'shell',
      name: 'Shell',
      kind: 'terminal',
      command: 'sh -i',
      enabled: true,
    },
  ],
  lastOpenSubSessions: [
    {
      id: '11111111-1111-1111-1111-111111111111',
      parentWorktreeTabId: '550e8400-e29b-41d4-a716-446655440000',
      defId: 'shell',
      kind: 'terminal',
      label: 'Shell',
      composedCommand: 'sh -i',
    },
  ],
  worktreeTabs: [],
  worktreeTabOrder: [],
  activeWorktreeTabId: null,
  theme: 'system',
} as const satisfies AppConfig;
