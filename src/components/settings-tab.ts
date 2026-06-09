import type { PartialAppConfig } from '@/types/arborist';

/**
 * Imperative handle a settings tab exposes (via `forwardRef`) when embedded
 * in `SettingsDialog`. The dialog's single Save button collects each tab's
 * patch and merges them into one `config_set` round-trip.
 */
export interface SettingsTabHandle {
  /** Build the config patch for this tab's unsaved edits, or `undefined` when nothing changed. */
  buildPatch: () => PartialAppConfig | undefined;
}

/** Dirty/validity snapshot a tab reports upward so the dialog can drive per-tab dirty dots and enable Save. */
export interface SettingsTabStateChange {
  /** True when the tab has unsaved edits relative to the persisted config. */
  dirty: boolean;
  /** False when the tab has a field-level validation error that must block saving. */
  valid: boolean;
}
