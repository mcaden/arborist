// Plugin framework — frontend foundation (issue #95, tracking #93).
//
// TS mirror of `src-tauri/src/plugins/mod.rs`. Three plugin kinds (AI,
// Custom Process, Dashboard Widget) share a `Plugin` shape (`id` +
// `displayName`); each kind adds its own methods. The `PluginRegistry`
// returned by `createRegistry()` is append-only at startup and exposed to
// React via `PluginRegistryProvider` / `useRegistry()` (see
// `./registry-provider.tsx` / `./use-registry.ts`).
//
// This issue lands scaffolding only — the production registry is empty
// at boot. Sub-issues #96 / #97 / #98 populate it.

import type { ComponentType } from 'react';

// MIRROR: src-tauri/src/plugins/mod.rs::Plugin
export interface Plugin {
  /** Stable identifier used as a serde key on the Rust side. */
  id: string;
  /** Human-readable name surfaced in the UI. */
  displayName: string;
}

// MIRROR: src-tauri/src/plugins/ai/mod.rs::AiPlugin
export interface AiPlugin extends Plugin {
  /** Bare program token (`"claude"`, `"copilot"`). User-overridable via `AppConfig.ai_launch_commands`. */
  defaultProgram: string;
  /** Filename of the built-in instruction-set markdown under `instructions/` (e.g. `"claude-default.md"`). */
  defaultInstructionSetPath: string;
}

// MIRROR: src-tauri/src/plugins/custom_process/mod.rs::CustomProcessPlugin
export interface CustomProcessPlugin extends Plugin {
  /**
   * Returns true if this plugin should be applied to a custom-process
   * definition. The first plugin to claim a def wins.
   */
  matches: (def: { id: string; command: string }) => boolean;
  /** True if the current platform supports this plugin. */
  supportedOnPlatform: () => boolean;
}

// MIRROR: src-tauri/src/plugins/dashboard_widget/mod.rs::DashboardWidgetBackend
// (extended with frontend-only `order` and `Component`; the Rust trait is
// backend plumbing only and intentionally omits these UI fields.)
export interface DashboardWidgetPlugin extends Plugin {
  /**
   * Lower value renders first. Ties are broken by registration order.
   * Issue #98 populates this with the seed widgets (`git-status`,
   * `ai-usage`).
   */
  order: number;
  /**
   * React component rendered inside `WorktreeDashboard`. Receives the
   * worktree tab's id and path. v1 declares the prop shape only; #98
   * supplies the actual components.
   */
  Component: ComponentType<DashboardWidgetProps>;
}

export interface DashboardWidgetProps {
  tabId: string;
  tabPath: string;
}

/**
 * Reason a `register*` call failed. Mirrors `RegisterError::DuplicateId`
 * on the Rust side: the registry rejects duplicate ids rather than
 * silently overwriting so id collisions surface clearly when a future
 * out-of-tree plugin path tries to register on top of a built-in.
 */
export class PluginRegisterError extends Error {
  constructor(
    public readonly kind: 'ai' | 'customProcess' | 'widget',
    public readonly pluginId: string,
  ) {
    super(`plugin id collision: a ${kind} plugin with id "${pluginId}" is already registered`);
    this.name = 'PluginRegisterError';
  }
}

export interface PluginRegistry {
  registerAi: (plugin: AiPlugin) => void;
  registerCustomProcess: (plugin: CustomProcessPlugin) => void;
  registerWidget: (plugin: DashboardWidgetPlugin) => void;

  ai: () => readonly AiPlugin[];
  aiById: (id: string) => AiPlugin | undefined;
  customProcessForDef: (def: { id: string; command: string }) => CustomProcessPlugin | undefined;
  customProcesses: () => readonly CustomProcessPlugin[];
  widgets: () => readonly DashboardWidgetPlugin[];
}

/**
 * Construct an empty plugin registry. Production wraps this in a
 * `PluginRegistryProvider` at the App root; tests can construct a fresh
 * registry per test.
 *
 * Append-only at startup: `register*` throws `PluginRegisterError` on
 * duplicate ids. Iteration order is registration order so the UI renders
 * plugins in a stable sequence.
 */
export function createRegistry(): PluginRegistry {
  const ai: AiPlugin[] = [];
  const aiIndex = new Map<string, number>();
  const customProcess: CustomProcessPlugin[] = [];
  const customProcessIndex = new Map<string, number>();
  const widgets: DashboardWidgetPlugin[] = [];
  const widgetsIndex = new Map<string, number>();

  return {
    registerAi(plugin) {
      if (aiIndex.has(plugin.id)) {
        throw new PluginRegisterError('ai', plugin.id);
      }
      aiIndex.set(plugin.id, ai.length);
      ai.push(plugin);
    },
    registerCustomProcess(plugin) {
      if (customProcessIndex.has(plugin.id)) {
        throw new PluginRegisterError('customProcess', plugin.id);
      }
      customProcessIndex.set(plugin.id, customProcess.length);
      customProcess.push(plugin);
    },
    registerWidget(plugin) {
      if (widgetsIndex.has(plugin.id)) {
        throw new PluginRegisterError('widget', plugin.id);
      }
      widgetsIndex.set(plugin.id, widgets.length);
      widgets.push(plugin);
    },
    // Defensive copies: `readonly` is compile-time-only and callers can mutate via casts or runtime access. Returning a fresh array on every call
    // keeps the registry's internal `*Index` maps in sync with its arrays — the append-only contract is enforced even against misbehaving callers.
    ai: () => ai.slice(),
    aiById: (id) => {
      const idx = aiIndex.get(id);
      return idx === undefined ? undefined : ai[idx];
    },
    // Filter unsupported plugins before applying `matches(def)` so an unsupported plugin (e.g. Explorer on macOS in #97) cannot "win" the lookup
    // and then fail later at spawn time. Preserves registration order for tie-breaking.
    customProcessForDef: (def) => customProcess.find((p) => p.supportedOnPlatform() && p.matches(def)),
    customProcesses: () => customProcess.slice(),
    widgets: () => widgets.slice(),
  };
}

export { PluginRegistryProvider } from './registry-provider';
export { useRegistry } from './use-registry';
