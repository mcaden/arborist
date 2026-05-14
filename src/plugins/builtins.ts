import { ClaudeAiPlugin } from './ai/claude';
import { CopilotAiPlugin } from './ai/copilot';
import { ExplorerCustomProcessPlugin, VsCodeCustomProcessPlugin } from './custom-process';
import { aiUsageWidget } from './dashboard-widget/ai-usage';
import { gitStatusWidget } from './dashboard-widget/git-status';
import { createRegistry, type PluginRegistry } from './registry';

/**
 * Build the production plugin registry with all currently-shipped frontend
 * plugins.
 */
export function createBuiltinsRegistry(): PluginRegistry {
  const registry = createRegistry();
  registry.registerAi(ClaudeAiPlugin);
  registry.registerAi(CopilotAiPlugin);
  registry.registerCustomProcess(VsCodeCustomProcessPlugin);
  registry.registerCustomProcess(ExplorerCustomProcessPlugin);
  registry.registerWidget(gitStatusWidget);
  registry.registerWidget(aiUsageWidget);
  return registry;
}
