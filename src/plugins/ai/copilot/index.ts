import { AI_LAUNCH_COMMAND_SETTING, type AiPlugin } from '../../registry';

import { CopilotIcon } from './icon';

export const CopilotAiPlugin: AiPlugin = {
  id: 'copilot',
  displayName: 'Copilot',
  defaultProgram: 'copilot',
  contextMetricsLimitTooltipSuffix: ' (Copilot-reported; excludes its system-prompt + tool overhead)',
  settings: [
    {
      id: AI_LAUNCH_COMMAND_SETTING,
      kind: 'text',
      label: 'Launch command',
      defaultValue: '',
      placeholder: 'copilot',
      helpText: 'Shell command used when launching Copilot sessions. Leave blank to use the plugin default.',
      spellCheck: false,
    },
  ],
  Icon: CopilotIcon,
};
