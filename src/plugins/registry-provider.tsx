// Provider component for the plugin registry context.
//
// Split from `registry-context.ts` so the context constant and the
// hook live in their own files; this file exports only runtime
// components (the `PluginRegistryProviderProps` interface is
// type-only and is erased at build time), which keeps React Fast
// Refresh happy (`react-refresh/only-export-components` lint rule).

import { useMemo, type ReactNode } from 'react';

import { createBuiltinsRegistry } from './builtins';
import type { PluginRegistry } from './registry';
import { PluginRegistryContext } from './registry-context';

export interface PluginRegistryProviderProps {
  /**
   * Optional pre-built registry. When omitted, a built-ins registry is
   * constructed once per provider instance and kept stable across re-renders.
   */
  registry?: PluginRegistry;
  children: ReactNode;
}

export function PluginRegistryProvider({ registry, children }: PluginRegistryProviderProps): JSX.Element {
  // `useMemo` with an empty dep list gives us a per-provider-instance
  // built-ins registry that survives re-renders without leaking across
  // tests (each test renders a fresh provider).
  const fallback = useMemo(() => createBuiltinsRegistry(), []);
  const value = registry ?? fallback;
  return <PluginRegistryContext.Provider value={value}>{children}</PluginRegistryContext.Provider>;
}
