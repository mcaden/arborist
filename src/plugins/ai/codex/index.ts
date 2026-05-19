import { AI_LAUNCH_COMMAND_SETTING, type AiPlugin } from '../../registry';

import { CodexIcon } from './icon';

export const CodexAiPlugin: AiPlugin = {
  id: 'codex',
  displayName: 'Codex',
  defaultProgram: 'codex',
  // Codex reports context metrics via rollout-file tailing; no tooling-specific tooltip caveat needed.
  contextMetricsLimitTooltipSuffix: '',
  settings: [
    {
      id: AI_LAUNCH_COMMAND_SETTING,
      kind: 'text',
      label: 'Launch command',
      defaultValue: '',
      placeholder: 'codex',
      helpText: 'Shell command used when launching Codex sessions. Leave blank to use the plugin default.',
      spellCheck: false,
    },
  ],
  Icon: CodexIcon,
};
