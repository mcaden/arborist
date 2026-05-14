import type { CustomProcessPlugin } from '../../registry';
import { looksLikeVsCodeCommand } from '../match-command';

export const VsCodeCustomProcessPlugin: CustomProcessPlugin = {
  id: 'vscode',
  displayName: 'VS Code',
  matches: (def) => looksLikeVsCodeCommand(def.command),
  supportedOnPlatform: () => true,
};
