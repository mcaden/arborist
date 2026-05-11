// Plugin framework barrel — re-exports the public surface.
//
// Consumers should always import from `@/plugins` (this file). The internal
// `registry-context.ts`, `registry-provider.tsx`, and `use-registry.ts`
// modules import directly from `./registry` to avoid a circular module
// dependency (`index ↔ registry-provider`) — keeping that split is what lets
// HMR and Fast Refresh behave predictably.

export * from './registry';
export { PluginRegistryProvider } from './registry-provider';
export { useRegistry } from './use-registry';
