// Plugin registry React context (issue #95).
//
// Production wraps `<App />` in `<PluginRegistryProvider>` at the top
// of `App.tsx`. v1 the provider value is an empty registry; sub-issues
// #96 / #97 / #98 populate it during construction.
//
// The context itself is exported so `useRegistry` (in
// `use-registry.ts`) can subscribe to it from a separate file — that
// split keeps this file Fast-Refresh-friendly under the
// `react-refresh/only-export-components` lint rule.

import { createContext } from 'react';

import type { PluginRegistry } from './index';

export const PluginRegistryContext = createContext<PluginRegistry | null>(null);
