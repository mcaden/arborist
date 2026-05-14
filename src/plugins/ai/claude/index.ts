import { AI_LAUNCH_COMMAND_SETTING, type AiPlugin } from '../../registry';

import { ClaudeIcon } from './icon';

export const ClaudeAiPlugin: AiPlugin = {
  id: 'claude',
  displayName: 'Claude',
  defaultProgram: 'claude',
  contextMetricsLimitTooltipSuffix: ' (model nominal max; includes harness overhead in usage)',
  settings: [
    {
      id: AI_LAUNCH_COMMAND_SETTING,
      kind: 'text',
      label: 'Launch command',
      defaultValue: '',
      placeholder: 'claude',
      helpText: 'Shell command used when launching Claude sessions. Leave blank to use the plugin default.',
      spellCheck: false,
    },
  ],
  Icon: ClaudeIcon,
};
