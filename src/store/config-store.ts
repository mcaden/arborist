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
import { useShallow } from 'zustand/react/shallow';

import { configGet, configSet, formatError } from '@/lib/tauri-bridge';
import type { AppConfig, CustomProcessDef, PartialAppConfig, SubSessionRecord } from '@/types/arborist';

const EMPTY_CONFIG: AppConfig = {
  configVersion: 5,
  defaultInstructionSets: { claude: '', copilot: '' },
  instructionSetsDir: '',
  workspaceRoot: null,
  worktreeRoots: [],
  prelaunchCommands: [],
  worktreePrelaunchCommands: {},
  aiLaunchCommands: { claude: '', copilot: '' },
  lastOpenSessions: [],
  tabOrder: [],
  activeSessionId: null,
  customProcesses: [],
  lastOpenSubSessions: [],
  worktreesDir: '.worktrees',
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
   * Atomically replace the cached config with a server-truth snapshot
   * (no diff merge). Used by `lib/workspace-switch.ts` after a
   * successful `workspaceSwitch` to install the **new** workspace's
   * config in one render — combined with `sessionStore.adoptWorkspace`,
   * this collapses the old "config-store.hydrate → frontendReady →
   * session-store.hydrate" round-trip chain into a single paint.
   * Marks the store as `ready` and clears any prior error.
   */
  adoptWorkspace: (config: AppConfig) => void;
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
  for (const [key, value] of Object.entries(patch) as [keyof PartialAppConfig, PartialAppConfig[keyof PartialAppConfig]][]) {
    if (value !== undefined) {
      // Safety: we're rebuilding the same shape we just destructured, so
      // the cast preserves type fidelity without re-checking each field.
      (out as Record<string, unknown>)[key] = value;
    }
  }
  return out;
}

export const useConfigStore = create<ConfigStoreState>((set) => ({
  config: EMPTY_CONFIG,
  status: 'idle',
  error: null,

  hydrate: async () => {
    set({ status: 'loading', error: null });
    try {
      const config = await configGet();
      set({ config, status: 'ready', error: null });
    } catch (err) {
      const message = formatError(err);
      set({ status: 'error', error: message });
      throw err;
    }
  },

  adoptWorkspace: (config) => {
    set({ config, status: 'ready', error: null });
  },

  set: async (patch) => {
    const diff = stripUndefined(patch);
    // The backend returns the merged config — including backend-derived
    // fields (e.g. `customProcesses[].iconDataUri` populated by the
    // icon backfill pass) that the original `diff` doesn't carry.
    // Trust the returned snapshot wholesale.
    const config = await configSet(diff);
    set({ config });
  },
}));

// ---------------------------------------------------------------------------
// Granular selectors. Components should reach for these instead of pulling
// the whole store; doing so keeps re-renders tight.
// ---------------------------------------------------------------------------

export const selectConfig = (s: ConfigStoreState): AppConfig => s.config;
export const selectInstructionSetsDir = (s: ConfigStoreState): string => s.config.instructionSetsDir;
export const selectWorkspaceRoot = (s: ConfigStoreState): string | null => s.config.workspaceRoot;
export const selectWorktreeRoots = (s: ConfigStoreState): readonly string[] => s.config.worktreeRoots;
export const selectPrelaunchCommands = (s: ConfigStoreState): readonly string[] => s.config.prelaunchCommands;
export const selectAiLaunchCommands = (s: ConfigStoreState): AppConfig['aiLaunchCommands'] => s.config.aiLaunchCommands;
export const selectDefaultInstructionSets = (s: ConfigStoreState): AppConfig['defaultInstructionSets'] => s.config.defaultInstructionSets;
export const selectTabOrder = (s: ConfigStoreState): AppConfig['tabOrder'] => s.config.tabOrder;
export const selectLastOpenSessions = (s: ConfigStoreState): AppConfig['lastOpenSessions'] => s.config.lastOpenSessions;
export const selectCustomProcesses = (s: ConfigStoreState): readonly CustomProcessDef[] => s.config.customProcesses;
export const selectLastOpenSubSessions = (s: ConfigStoreState): readonly SubSessionRecord[] => s.config.lastOpenSubSessions;
export const selectWorktreesDir = (s: ConfigStoreState): string => s.config.worktreesDir;
export const selectStatus = (s: ConfigStoreState): HydrationStatus => s.status;
export const selectError = (s: ConfigStoreState): string | null => s.error;

/**
 * Convenience hook for the enabled subset of `customProcesses`, used by the
 * tab context menu's "Launch…" submenu. Returns a stable reference per
 * underlying-array identity (Zustand handles equality on the slice itself).
 */
export const useEnabledCustomProcesses = (): readonly CustomProcessDef[] =>
  useConfigStore(useShallow((s) => s.config.customProcesses.filter((d) => d.enabled)));
