// Zustand-backed cache of the persisted [`AppConfig`]. The Rust backend
// owns the on-disk file (`config.json` via `ConfigStore`); this store just
// keeps a fast, reactive in-memory copy that components can subscribe to.
//
// Conventions:
// * Components subscribe via granular selectors (one field per `useStore`
//   call) — never `useStore(s => s)`.
// * Callers mutating config should hand `set()` only the *diff* of fields
//   they actually changed. The bridge round-trips that diff to `config_set`
//   so backend canonicalization warnings only fire for the touched fields.
// * `hydrate()` is idempotent: it always re-reads from the backend and
//   replaces the cached snapshot. Call it once at app start (and again
//   after a backend write that may have produced backend-side fallbacks
//   the frontend should observe).

import { create } from 'zustand';

import { configGet, configSet } from '@/lib/tauri-bridge';
import type { AppConfig, PartialAppConfig } from '@/types/grove';

const EMPTY_CONFIG: AppConfig = {
  configVersion: 1,
  defaultInstructionSets: { claude: '', copilot: '' },
  instructionSetsDir: '',
  worktreeRoots: [],
  prelaunchCommands: [],
  worktreePrelaunchCommands: {},
  lastOpenSessions: [],
  tabOrder: [],
};

export type HydrationStatus = 'idle' | 'loading' | 'ready' | 'error';

export interface ConfigStoreState {
  /** Last-known snapshot from the backend. `EMPTY_CONFIG` until hydrated. */
  config: AppConfig;
  status: HydrationStatus;
  /** Last error message, if `status === 'error'`. */
  error: string | null;
  /** Re-read the persisted config and replace the cached snapshot. */
  hydrate: () => Promise<void>;
  /**
   * Push a diff to the backend via `configSet` and, on success, mirror the
   * diff into the local cache. Only fields explicitly present on `patch`
   * are forwarded — `undefined` values are stripped first so the backend's
   * deep-merge sees a true patch.
   */
  set: (patch: PartialAppConfig) => Promise<void>;
}

function stripUndefined(patch: PartialAppConfig): PartialAppConfig {
  const out: PartialAppConfig = {};
  for (const [key, value] of Object.entries(patch) as [
    keyof PartialAppConfig,
    PartialAppConfig[keyof PartialAppConfig],
  ][]) {
    if (value !== undefined) {
      // Safety: we're rebuilding the same shape we just destructured, so
      // the cast preserves type fidelity without re-checking each field.
      (out as Record<string, unknown>)[key] = value;
    }
  }
  return out;
}

function applyPatch(config: AppConfig, patch: PartialAppConfig): AppConfig {
  const next: AppConfig = { ...config };
  if (patch.configVersion !== undefined) next.configVersion = patch.configVersion;
  if (patch.defaultInstructionSets !== undefined) {
    next.defaultInstructionSets = {
      ...next.defaultInstructionSets,
      ...patch.defaultInstructionSets,
    };
  }
  if (patch.instructionSetsDir !== undefined) next.instructionSetsDir = patch.instructionSetsDir;
  if (patch.worktreeRoots !== undefined) next.worktreeRoots = patch.worktreeRoots;
  if (patch.prelaunchCommands !== undefined) next.prelaunchCommands = patch.prelaunchCommands;
  if (patch.worktreePrelaunchCommands !== undefined) {
    next.worktreePrelaunchCommands = patch.worktreePrelaunchCommands;
  }
  if (patch.lastOpenSessions !== undefined) next.lastOpenSessions = patch.lastOpenSessions;
  if (patch.tabOrder !== undefined) next.tabOrder = patch.tabOrder;
  return next;
}

export const useConfigStore = create<ConfigStoreState>((set, get) => ({
  config: EMPTY_CONFIG,
  status: 'idle',
  error: null,

  hydrate: async () => {
    set({ status: 'loading', error: null });
    try {
      const config = await configGet();
      set({ config, status: 'ready', error: null });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      set({ status: 'error', error: message });
      throw err;
    }
  },

  set: async (patch) => {
    const diff = stripUndefined(patch);
    await configSet(diff);
    set({ config: applyPatch(get().config, diff) });
  },
}));

// ---------------------------------------------------------------------------
// Granular selectors. Components should reach for these instead of pulling
// the whole store; doing so keeps re-renders tight.
// ---------------------------------------------------------------------------

export const selectConfig = (s: ConfigStoreState): AppConfig => s.config;
export const selectInstructionSetsDir = (s: ConfigStoreState): string =>
  s.config.instructionSetsDir;
export const selectWorktreeRoots = (s: ConfigStoreState): readonly string[] =>
  s.config.worktreeRoots;
export const selectPrelaunchCommands = (s: ConfigStoreState): readonly string[] =>
  s.config.prelaunchCommands;
export const selectDefaultInstructionSets = (
  s: ConfigStoreState,
): AppConfig['defaultInstructionSets'] => s.config.defaultInstructionSets;
export const selectTabOrder = (s: ConfigStoreState): AppConfig['tabOrder'] => s.config.tabOrder;
export const selectLastOpenSessions = (s: ConfigStoreState): AppConfig['lastOpenSessions'] =>
  s.config.lastOpenSessions;
export const selectStatus = (s: ConfigStoreState): HydrationStatus => s.status;
export const selectError = (s: ConfigStoreState): string | null => s.error;
