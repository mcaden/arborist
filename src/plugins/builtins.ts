import { aiUsageWidget } from './dashboard-widget/ai-usage';
import { gitStatusWidget } from './dashboard-widget/git-status';
import { createRegistry, type PluginRegistry } from './registry';

/**
 * Build the production plugin registry with all currently-shipped frontend
 * plugins. Future plugin migrations (AI / custom-process) append registrations
 * here.
 */
export function createBuiltinsRegistry(): PluginRegistry {
  const registry = createRegistry();
  registry.registerWidget(gitStatusWidget);
  registry.registerWidget(aiUsageWidget);
  return registry;
}
