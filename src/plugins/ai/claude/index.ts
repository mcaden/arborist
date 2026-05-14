import type { AiPlugin } from '../../registry';

import { ClaudeIcon } from './icon';

export const ClaudeAiPlugin: AiPlugin = {
  id: 'claude',
  displayName: 'Claude',
  defaultProgram: 'claude',
  defaultInstructionSetPath: 'claude-default.md',
  contextMetricsLimitTooltipSuffix: ' (model nominal max; includes harness overhead in usage)',
  Icon: ClaudeIcon,
};
