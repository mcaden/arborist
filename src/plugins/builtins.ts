import { aiUsageWidget } from './dashboard-widget/ai-usage';
import { gitStatusWidget } from './dashboard-widget/git-status';
import { createBuiltinRegistry, type PluginRegistry } from './registry';

/**
 * Build the production plugin registry with all currently-shipped frontend
 * plugins.
 */
export function createBuiltinsRegistry(): PluginRegistry {
  const registry = createBuiltinRegistry();
  registry.registerWidget(gitStatusWidget);
  registry.registerWidget(aiUsageWidget);
  return registry;
}
