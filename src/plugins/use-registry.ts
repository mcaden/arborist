// `useRegistry` hook split from `registry-context.tsx` so the context
// file only exports a component (keeps Fast Refresh happy under the
// `react-refresh/only-export-components` lint rule).

import { useContext } from 'react';

import { PluginRegistryContext } from './registry-context';
import type { PluginRegistry } from './index';

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
