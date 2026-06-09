import { forwardRef, useCallback, useEffect, useImperativeHandle, useMemo, useState } from 'react';

import { ExperimentalIcon } from './ExperimentalIcon';
import type { SettingsTabHandle, SettingsTabStateChange } from './settings-tab';
import { formatError } from '@/lib/tauri-bridge';
import { AI_LAUNCH_COMMAND_SETTING, aiLaunchCommand, pluginEnabled, useRegistry } from '@/plugins';
import { selectConfig, useConfigStore } from '@/store/config-store';
import type { PartialAppConfig, PartialPluginSettingState, PartialPluginSettings, PluginSettingValue } from '@/types/arborist';

export interface PluginsTabProps {
  onClose: () => void;
  /** When true, the tab hides its own Save/Cancel footer; the parent SettingsDialog coordinates saving across tabs. */
  embedded?: boolean;
  /** Report dirty/validity upward so the parent can drive cross-tab dirty indicators and the unified Save button. */
  onStateChange?: (state: SettingsTabStateChange) => void;
}

interface PluginDraft {
  enabled: boolean;
  settings: Record<string, PluginSettingValue>;
}

interface PluginDrafts {
  ai: Record<string, PluginDraft>;
  customProcess: Record<string, PluginDraft>;
  dashboardWidget: Record<string, PluginDraft>;
}

function makeDraft(enabled: boolean, settings: Record<string, PluginSettingValue> = {}): PluginDraft {
  return { enabled, settings };
}

function draftsEqual(a: PluginDrafts, b: PluginDrafts): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

function draftPatch(current: PluginDraft, persisted: PluginDraft): PartialPluginSettingState | undefined {
  const patch: PartialPluginSettingState = {};
  if (current.enabled !== persisted.enabled) patch.enabled = current.enabled;
  const settings: Record<string, PluginSettingValue> = {};
  for (const [key, value] of Object.entries(current.settings)) {
    if (persisted.settings[key] !== value) settings[key] = value;
  }
  if (Object.keys(settings).length > 0) patch.settings = settings;
  return patch.enabled !== undefined || patch.settings !== undefined ? patch : undefined;
}

export const PluginsTab = forwardRef<SettingsTabHandle, PluginsTabProps>(function PluginsTab(
  { onClose, embedded = false, onStateChange },
  ref,
): JSX.Element {
  const registry = useRegistry();
  const aiPlugins = useMemo(() => registry.ai(), [registry]);
  const customProcessPlugins = useMemo(() => registry.customProcesses(), [registry]);
  const dashboardWidgets = useMemo(() => registry.widgets(), [registry]);
  const config = useConfigStore(selectConfig);
  const setConfig = useConfigStore((s) => s.set);

  const persistedDrafts = useMemo<PluginDrafts>(() => {
    const ai: Record<string, PluginDraft> = {};
    for (const plugin of aiPlugins) {
      ai[plugin.id] = makeDraft(pluginEnabled(config.pluginSettings, 'ai', plugin.id, plugin.defaultEnabled ?? true), {
        [AI_LAUNCH_COMMAND_SETTING]: aiLaunchCommand(config, plugin.id),
      });
    }

    const customProcess: Record<string, PluginDraft> = {};
    for (const plugin of customProcessPlugins) {
      customProcess[plugin.id] = makeDraft(pluginEnabled(config.pluginSettings, 'customProcess', plugin.id, plugin.defaultEnabled ?? true));
    }

    const dashboardWidget: Record<string, PluginDraft> = {};
    for (const plugin of dashboardWidgets) {
      dashboardWidget[plugin.id] = makeDraft(pluginEnabled(config.pluginSettings, 'dashboardWidget', plugin.id, plugin.defaultEnabled ?? true));
    }

    return { ai, customProcess, dashboardWidget };
  }, [aiPlugins, config, customProcessPlugins, dashboardWidgets]);

  const [drafts, setDrafts] = useState<PluginDrafts>(persistedDrafts);
  const [lastSynced, setLastSynced] = useState<PluginDrafts>(persistedDrafts);
  const [saving, setSaving] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  useEffect(() => {
    if (draftsEqual(drafts, lastSynced)) {
      setDrafts(persistedDrafts);
      setLastSynced(persistedDrafts);
    }
  }, [drafts, lastSynced, persistedDrafts]);

  const updateEnabled = useCallback((kind: keyof PluginDrafts, pluginId: string, enabled: boolean): void => {
    setSubmitError(null);
    setDrafts((prev) => ({
      ...prev,
      [kind]: {
        ...prev[kind],
        [pluginId]: makeDraft(enabled, prev[kind][pluginId]?.settings ?? {}),
      },
    }));
  }, []);

  const updateSetting = useCallback((kind: keyof PluginDrafts, pluginId: string, settingId: string, value: PluginSettingValue): void => {
    setSubmitError(null);
    setDrafts((prev) => {
      const existing = prev[kind][pluginId] ?? makeDraft(true);
      return {
        ...prev,
        [kind]: {
          ...prev[kind],
          [pluginId]: makeDraft(existing.enabled, { ...existing.settings, [settingId]: value }),
        },
      };
    });
  }, []);

  const buildPatch = useCallback((): PartialAppConfig | undefined => {
    const pluginSettings: PartialPluginSettings = {};
    for (const kind of ['ai', 'customProcess', 'dashboardWidget'] as const) {
      const byPlugin: Record<string, PartialPluginSettingState> = {};
      for (const [pluginId, current] of Object.entries(drafts[kind])) {
        const persisted = lastSynced[kind][pluginId] ?? makeDraft(true);
        const patch = draftPatch(current, persisted);
        if (patch) byPlugin[pluginId] = patch;
      }
      if (Object.keys(byPlugin).length > 0) pluginSettings[kind] = byPlugin;
    }
    return Object.keys(pluginSettings).length > 0 ? { pluginSettings } : undefined;
  }, [drafts, lastSynced]);

  const handleSave = useCallback(async (): Promise<void> => {
    // Standalone-only entry point: in embedded mode the parent SettingsDialog
    // drives saving via buildPatch(), and the Save/Cancel footer below (which is
    // the only consumer of `saving`/`submitError`/`handleSave`) is not rendered.
    setSubmitError(null);
    setSaving(true);
    try {
      const patch = buildPatch();
      if (patch) await setConfig(patch);
      onClose();
    } catch (err) {
      setSubmitError(formatError(err));
    } finally {
      setSaving(false);
    }
  }, [buildPatch, onClose, setConfig]);

  const dirty = !draftsEqual(drafts, lastSynced);

  // Plugin edits have no field-level validation, so the tab is always "valid".
  useEffect(() => {
    onStateChange?.({ dirty, valid: true });
  }, [dirty, onStateChange]);

  useImperativeHandle(ref, () => ({ buildPatch }), [buildPatch]);

  return (
    <div data-testid="plugins-tab" className="flex min-h-0 flex-1 flex-col">
      <p className="mb-3 text-xs text-slate-500 dark:text-slate-400">
        Enable or disable plugins and edit settings that belong to each plugin. Existing running sessions are not stopped when an AI plugin is
        disabled.
      </p>

      <div className="themed-scrollbar -mx-1 flex-1 space-y-4 overflow-y-auto px-1">
        <section>
          <h3 className="mb-2 text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400">AI agents</h3>
          <div className="space-y-3">
            {aiPlugins.map((plugin) => {
              const draft = drafts.ai[plugin.id] ?? makeDraft(true);
              const launchCommand = String(draft.settings[AI_LAUNCH_COMMAND_SETTING] ?? '');
              const launchSetting = plugin.settings?.find((setting) => setting.id === AI_LAUNCH_COMMAND_SETTING && setting.kind === 'text');
              return (
                <fieldset
                  key={plugin.id}
                  data-testid={`plugin-row-ai-${plugin.id}`}
                  className="rounded border border-slate-200 bg-slate-50 p-3 dark:border-slate-700 dark:bg-slate-800"
                >
                  <legend className="px-1 text-xs font-medium text-slate-700 dark:text-slate-200">
                    <span className="inline-flex items-center gap-1">
                      <span>{plugin.displayName}</span>
                      {plugin.experimental ? (
                        <span
                          data-testid={`plugin-ai-${plugin.id}-experimental`}
                          className="inline-flex items-center gap-0.5 rounded bg-amber-100 px-1 py-px text-[10px] font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-200"
                          title="This plugin is experimental and may change or break without notice."
                        >
                          <ExperimentalIcon className="h-3 w-3" />
                          <span>(experimental)</span>
                        </span>
                      ) : null}
                    </span>
                  </legend>
                  <label className="mb-2 inline-flex items-center gap-2 text-xs">
                    <input
                      type="checkbox"
                      checked={draft.enabled}
                      onChange={(e) => updateEnabled('ai', plugin.id, e.target.checked)}
                      aria-label={`Enabled: ${plugin.displayName}`}
                    />
                    Enabled
                  </label>
                  <label className="block text-xs">
                    <span className="mb-0.5 block text-slate-500 dark:text-slate-400">{launchSetting?.label ?? 'Launch command'}</span>
                    <input
                      type="text"
                      value={launchCommand}
                      placeholder={launchSetting?.placeholder ?? plugin.defaultProgram}
                      spellCheck={launchSetting?.spellCheck ?? false}
                      data-testid={`plugin-ai-${plugin.id}-launch-command`}
                      onChange={(e) => updateSetting('ai', plugin.id, AI_LAUNCH_COMMAND_SETTING, e.target.value.trimStart())}
                      className="w-full rounded border border-slate-300 bg-white px-2 py-1 font-mono text-xs dark:border-slate-700 dark:bg-slate-900"
                    />
                  </label>
                  <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">
                    {launchSetting?.helpText ?? 'Leave blank to use the plugin default command.'}
                  </p>
                </fieldset>
              );
            })}
          </div>
        </section>

        <section>
          <h3 className="mb-2 text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400">Custom process integrations</h3>
          <p className="mb-2 text-xs text-slate-500 dark:text-slate-400">
            These toggles control built-in integrations such as application window ownership. Launcher definitions are managed in Custom Processes.
          </p>
          <div className="space-y-2">
            {customProcessPlugins.map((plugin) => {
              const draft = drafts.customProcess[plugin.id] ?? makeDraft(true);
              return (
                <label
                  key={plugin.id}
                  data-testid={`plugin-row-custom-process-${plugin.id}`}
                  className="flex items-center justify-between rounded border border-slate-200 bg-slate-50 px-3 py-2 text-xs dark:border-slate-700 dark:bg-slate-800"
                >
                  <span>{plugin.displayName}</span>
                  <input
                    type="checkbox"
                    checked={draft.enabled}
                    onChange={(e) => updateEnabled('customProcess', plugin.id, e.target.checked)}
                    aria-label={`Enabled: ${plugin.displayName}`}
                  />
                </label>
              );
            })}
          </div>
        </section>

        <section>
          <h3 className="mb-2 text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400">Dashboard widgets</h3>
          <div className="space-y-2">
            {dashboardWidgets.map((plugin) => {
              const draft = drafts.dashboardWidget[plugin.id] ?? makeDraft(true);
              return (
                <label
                  key={plugin.id}
                  data-testid={`plugin-row-dashboard-widget-${plugin.id}`}
                  className="flex items-center justify-between rounded border border-slate-200 bg-slate-50 px-3 py-2 text-xs dark:border-slate-700 dark:bg-slate-800"
                >
                  <span>{plugin.displayName}</span>
                  <input
                    type="checkbox"
                    checked={draft.enabled}
                    onChange={(e) => updateEnabled('dashboardWidget', plugin.id, e.target.checked)}
                    aria-label={`Enabled: ${plugin.displayName}`}
                  />
                </label>
              );
            })}
          </div>
        </section>
      </div>

      {!embedded && submitError && (
        <p
          role="alert"
          data-testid="settings-error"
          className="mt-3 rounded border border-red-300 bg-red-50 px-2 py-1 text-xs text-red-800 dark:border-red-800 dark:bg-red-950 dark:text-red-200"
        >
          {submitError}
        </p>
      )}

      {!embedded && (
        <div className="mt-3 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            disabled={saving}
            className="rounded border border-slate-300 bg-white px-3 py-1 text-xs hover:bg-slate-100 disabled:opacity-50 dark:border-slate-700 dark:bg-slate-800 dark:hover:bg-slate-700"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => handleSave()}
            disabled={!dirty || saving}
            data-testid="plugins-save"
            className="rounded bg-blue-600 px-3 py-1 text-xs font-medium text-white hover:bg-blue-500 disabled:opacity-50"
          >
            {saving ? 'Saving...' : 'Save'}
          </button>
        </div>
      )}
    </div>
  );
});
