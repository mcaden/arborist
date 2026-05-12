import type { AiPlugin } from '../../registry';

import { CopilotIcon } from './icon';

export const CopilotAiPlugin: AiPlugin = {
  id: 'copilot',
  displayName: 'Copilot',
  defaultProgram: 'copilot',
  defaultInstructionSetPath: 'copilot-default.md',
  Icon: CopilotIcon,
};
