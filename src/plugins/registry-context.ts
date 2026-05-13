// Plugin registry React context (issue #95).
//
// Production wraps `<App />` in `<PluginRegistryProvider>` at the top of
// `App.tsx`. The app passes a pre-built registry from
// `createBuiltinsRegistry()` so built-in plugins are available without
// waiting for runtime registration side effects.
//
// This file owns only the context constant. The provider component lives in
// `./registry-provider.tsx` and the `useRegistry` hook in `./use-registry.ts`;
// splitting the three across separate files keeps each one Fast-Refresh
// friendly under the `react-refresh/only-export-components` lint rule.

import { createContext } from 'react';

import type { PluginRegistry } from './registry';

export const PluginRegistryContext = createContext<PluginRegistry | null>(null);
