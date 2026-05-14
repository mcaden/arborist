import type { CustomProcessPlugin } from '../../registry';
import { looksLikeExplorerCommand } from '../match-command';

function isWindowsFrontend(): boolean {
  return typeof navigator !== 'undefined' && /Windows/i.test(navigator.userAgent);
}

export const ExplorerCustomProcessPlugin: CustomProcessPlugin = {
  id: 'explorer',
  displayName: 'Windows Explorer',
  matches: (def) => looksLikeExplorerCommand(def.command),
  supportedOnPlatform: isWindowsFrontend,
};
