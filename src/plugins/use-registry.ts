// `useRegistry` hook for the plugin registry context (issue #95).
//
// Kept in its own file so the provider component (`./registry-provider.tsx`)
// and the context constant (`./registry-context.ts`) each export only one
// concern apiece — that's what keeps every file in this trio Fast-Refresh
// friendly under the `react-refresh/only-export-components` lint rule.

import { useContext } from 'react';

import { PluginRegistryContext } from './registry-context';
import type { PluginRegistry } from './registry';

/**
 * Access the registry from any component beneath
 * `<PluginRegistryProvider>`. Throws if invoked outside a provider.
 */
export function useRegistry(): PluginRegistry {
  const ctx = useContext(PluginRegistryContext);
  if (ctx === null) {
    throw new Error('useRegistry() called outside <PluginRegistryProvider>');
  }
  return ctx;
}
