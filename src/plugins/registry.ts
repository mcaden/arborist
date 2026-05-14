// Plugin framework — core types, registry, and factory (issue #95, tracking #93).
//
// TS mirror of `src-tauri/src/plugins/mod.rs`. Three plugin kinds (AI, Custom
// Process, Dashboard Widget) share a `Plugin` shape (`id` + `displayName`);
// each kind adds its own methods. The `PluginRegistry` returned by
// `createRegistry()` is append-only at startup and exposed to React via
// `PluginRegistryProvider` / `useRegistry()` (see `./registry-provider.tsx`
// and `./use-registry.ts`).
//
// This module is the canonical source for the types and factory. `./index.ts`
// is a thin barrel that re-exports from here and from the provider/hook
// files; consumers should always import from `@/plugins` (the barrel) rather
// than this file, but the internal provider/hook files import directly from
// here to avoid an `index ↔ provider` circular module dependency.
//
// Frontend plugin capabilities are kind-driven. Custom-process plugins expose
// matching/platform predicates only (no UI contract), while dashboard widgets
// carry a React `Component`. The production app builds its registry via
// `createBuiltinsRegistry()` (`src/plugins/builtins.ts`).

import type { ComponentType } from 'react';

import type { CustomProcessDef, Tool, WorktreeTabId } from '@/types/arborist';

// MIRROR: src-tauri/src/plugins/mod.rs::Plugin
export interface Plugin {
  /** Stable identifier used as a serde key on the Rust side. */
  id: string;
  /** Human-readable name surfaced in the UI. */
  displayName: string;
}

// MIRROR: src-tauri/src/plugins/ai/mod.rs::AiPlugin
export interface AiPlugin extends Plugin {
  /** Stable AI tool discriminator mirrored from persisted `Tool` (`"claude" | "copilot"`). */
  id: Tool;
  /** Bare program token (`"claude"`, `"copilot"`). User-overridable via `AppConfig.ai_launch_commands`. */
  defaultProgram: string;
  /** Filename of the built-in instruction-set markdown under `instructions/` (e.g. `"claude-default.md"`). */
  defaultInstructionSetPath: string;
  /** Tooltip copy explaining how this plugin reports context-window limits. */
  contextMetricsLimitTooltipSuffix: string;
  /** SVG icon component for the plugin's launcher / tab chrome. */
  Icon: ComponentType<{ className?: string }>;
}

// MIRROR: src-tauri/src/plugins/custom_process/mod.rs::CustomProcessPlugin
// (frontend-facing subset; backend-only owner_resolver(cwd) is intentionally omitted)
export interface CustomProcessPlugin extends Plugin {
  /**
   * Returns true if this plugin should be applied to a custom-process
   * definition. The first plugin to claim a def wins. Receives the full
   * `CustomProcessDef` so plugins can inspect any field (e.g. `kind`) —
   * mirrors the Rust signature `fn matches(&self, def: &CustomProcessDef)`.
   */
  matches: (def: CustomProcessDef) => boolean;
  /** True if the current platform supports this plugin. */
  supportedOnPlatform: () => boolean;
}

// MIRROR: src-tauri/src/plugins/dashboard_widget/mod.rs::DashboardWidgetBackend
// (extended with frontend-only `order` and `Component`; the Rust trait is
// backend plumbing only and intentionally omits these UI fields.)
export interface DashboardWidgetPlugin extends Plugin {
  /**
   * Lower value renders first. Ties are broken by registration order.
   * Built-ins currently use this for `git-status` and `ai-usage`.
   */
  order: number;
  /**
   * React component rendered inside `WorktreeDashboard`. Receives the
   * worktree tab's id and path.
   */
  Component: ComponentType<DashboardWidgetProps>;
}

export interface DashboardWidgetProps {
  tabId: WorktreeTabId;
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
    public readonly kind: 'ai' | 'custom_process' | 'dashboard_widget',
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
  aiById: (id: Tool) => AiPlugin | undefined;
  customProcessForDef: (def: CustomProcessDef) => CustomProcessPlugin | undefined;
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
  const aiIndex = new Map<Tool, number>();
  const customProcess: CustomProcessPlugin[] = [];
  const customProcessIndex = new Map<string, number>();
  const widgets: DashboardWidgetPlugin[] = [];
  const widgetsIndex = new Map<string, number>();

  return {
    registerAi(plugin) {
      // Capture `id` once: a class-based plugin may implement `id` as a getter, and re-reading it across the duplicate check, error construction,
      // and index insert could (for a misbehaving getter) produce inconsistent values that desync the `*Index` map from the backing array.
      const id = plugin.id;
      if (aiIndex.has(id)) {
        throw new PluginRegisterError('ai', id);
      }
      // Freeze the caller's object directly (don't spread): a class-based plugin would otherwise lose prototype methods/getters (e.g. `Component`
      // implemented as a getter, or `matches` defined on a prototype). Freezing in-place preserves the prototype chain while still preventing
      // post-registration mutation of `id` (or other top-level fields) that would desync the *Index maps from the backing arrays.
      Object.freeze(plugin);
      aiIndex.set(id, ai.length);
      ai.push(plugin);
    },
    registerCustomProcess(plugin) {
      const id = plugin.id;
      if (customProcessIndex.has(id)) {
        throw new PluginRegisterError('custom_process', id);
      }
      Object.freeze(plugin);
      customProcessIndex.set(id, customProcess.length);
      customProcess.push(plugin);
    },
    registerWidget(plugin) {
      const id = plugin.id;
      if (widgetsIndex.has(id)) {
        throw new PluginRegisterError('dashboard_widget', id);
      }
      Object.freeze(plugin);
      widgetsIndex.set(id, widgets.length);
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
    // Sort by `order` (lower first) so the documented contract on `DashboardWidgetPlugin.order` holds.
    // Array.prototype.sort is stable since ES2019, so equal `order` values retain registration order.
    widgets: () => widgets.slice().sort((a, b) => a.order - b.order),
  };
}
