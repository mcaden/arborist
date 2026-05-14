// In-app settings panel — Roadmap §3.1.
//
// Reachable from the sidebar footer (and from the tab context menu's
// empty Launch submenu, which jumps straight to the Custom Processes
// tab). Three tabs:
//
//   General           — workspace root (delegates to the existing
//                       WorkspacePicker so the park-old-sessions
//                       invariant lives in one place — see
//                       `lib/workspace-switch.ts`), worktree prep commands
//                       (one shell command per line).
//   Plugins           — enable/disable plugins and edit plugin-owned
//                       settings such as AI launch commands.
//   Custom Processes  — CRUD over `AppConfig.customProcesses` (lives in
//                       a dedicated `CustomProcessesTab` component).
//   About             — lightweight project attribution and context.

import type { KeyboardEvent as ReactKeyboardEvent, RefObject } from 'react';
import { useCallback, useEffect, useId, useRef, useState } from 'react';

import type { ThemeMode } from '@/types/arborist';

import { CustomProcessesTab } from './CustomProcessesTab';
import { PluginsTab } from './PluginsTab';
import { WorkspacePicker } from './WorkspacePicker';
import { formatError } from '@/lib/tauri-bridge';
import { changeWorkspace } from '@/lib/workspace-switch';
import { selectTheme, selectWorkspaceRoot, selectWorktreePrepCommands, useConfigStore } from '@/store/config-store';

export type SettingsTab = 'general' | 'plugins' | 'customProcesses' | 'about';

export interface SettingsDialogProps {
  onClose: () => void;
  /** Which tab to show first. Defaults to `'general'`. */
  initialTab?: SettingsTab;
}

/**
 * Convert the worktree-prep-commands list to/from the textarea's plain-text
 * value. We intentionally use a textarea (one command per line) instead
 * of a row-per-command editor: the v1 spec only needs ordered editing
 * and a textarea is naturally good at that — copy/paste, drag, undo all
 * work out of the box.
 */
function commandsToText(cmds: readonly string[]): string {
  return cmds.join('\n');
}

function textToCommands(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

function arraysEqual(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

export function SettingsDialog({ onClose, initialTab = 'general' }: SettingsDialogProps): JSX.Element {
  const workspaceRoot = useConfigStore(selectWorkspaceRoot);
  const worktreePrepCommands = useConfigStore(selectWorktreePrepCommands);
  const theme = useConfigStore(selectTheme);
  const setConfig = useConfigStore((s) => s.set);

  const [activeTab, setActiveTab] = useState<SettingsTab>(initialTab);
  const [cmdsInput, setCmdsInput] = useState<string>(commandsToText(worktreePrepCommands));
  const [themeInput, setThemeInput] = useState<ThemeMode>(theme);
  const [saving, setSaving] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const headingId = useId();
  const generalTabId = useId();
  const pluginsTabId = useId();
  const customProcessesTabId = useId();
  const aboutTabId = useId();
  const generalPanelId = useId();
  const pluginsPanelId = useId();
  const customProcessesPanelId = useId();
  const aboutPanelId = useId();
  const closeBtnRef = useRef<HTMLButtonElement | null>(null);
  const generalTabRef = useRef<HTMLButtonElement | null>(null);
  const pluginsTabRef = useRef<HTMLButtonElement | null>(null);
  const customProcessesTabRef = useRef<HTMLButtonElement | null>(null);
  const aboutTabRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    closeBtnRef.current?.focus();
  }, []);

  // WAI-ARIA tab keyboard model: ←/→/Home/End move between tabs (with
  // wrap), and the move both selects and focuses (manual activation
  // would mean two keypresses to reach a tab the user clearly wants).
  const handleTablistKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>): void => {
      const order: SettingsTab[] = ['general', 'plugins', 'customProcesses', 'about'];
      const refs: Record<SettingsTab, RefObject<HTMLButtonElement | null>> = {
        general: generalTabRef,
        plugins: pluginsTabRef,
        customProcesses: customProcessesTabRef,
        about: aboutTabRef,
      };
      const idx = order.indexOf(activeTab);
      let next: SettingsTab | null = null;
      switch (e.key) {
        case 'ArrowLeft':
          next = order[(idx - 1 + order.length) % order.length]!;
          break;
        case 'ArrowRight':
          next = order[(idx + 1) % order.length]!;
          break;
        case 'Home':
          next = order[0]!;
          break;
        case 'End':
          next = order[order.length - 1]!;
          break;
        default:
          return;
      }
      e.preventDefault();
      setActiveTab(next);
      // Move DOM focus too so the roving-tabindex contract holds.
      requestAnimationFrame(() => refs[next!].current?.focus());
    },
    [activeTab],
  );

  // Re-sync local edit buffers if the persisted config changes underfoot
  // (e.g. via the workspace-change flow we delegate to WorkspacePicker).
  useEffect(() => {
    setCmdsInput(commandsToText(worktreePrepCommands));
  }, [worktreePrepCommands]);
  useEffect(() => {
    setThemeInput(theme);
  }, [theme]);

  const parsedCmds = textToCommands(cmdsInput);
  const dirty = !arraysEqual(parsedCmds, worktreePrepCommands) || themeInput !== theme;

  const handleSave = useCallback(async () => {
    setSubmitError(null);
    setSaving(true);
    try {
      const patch: {
        worktreePrepCommands?: string[];
        theme?: ThemeMode;
      } = {};
      if (!arraysEqual(parsedCmds, worktreePrepCommands)) patch.worktreePrepCommands = parsedCmds;
      if (themeInput !== theme) patch.theme = themeInput;
      if (Object.keys(patch).length > 0) await setConfig(patch);
      onClose();
    } catch (err) {
      const message = formatError(err);
      setSubmitError(message);
    } finally {
      setSaving(false);
    }
  }, [parsedCmds, worktreePrepCommands, themeInput, theme, setConfig, onClose]);

  const handleWorkspaceConfirm = useCallback(async (path: string) => {
    await changeWorkspace(path);
    setPicking(false);
  }, []);

  return (
    <>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={headingId}
        data-testid="settings-dialog"
        className="fixed inset-0 z-30 flex items-center justify-center bg-black/40 p-4"
        onClick={(e) => {
          if (e.target === e.currentTarget) onClose();
        }}
      >
        <div className="flex max-h-full w-full max-w-lg flex-col overflow-hidden rounded border border-slate-300 bg-white p-5 text-sm shadow-xl dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100">
          <div className="mb-4 flex shrink-0 items-start justify-between gap-3">
            <h2 id={headingId} className="text-base font-semibold">
              Settings
            </h2>
            <button
              ref={closeBtnRef}
              type="button"
              onClick={onClose}
              aria-label="Close settings"
              className="rounded px-2 py-0.5 text-slate-500 hover:bg-slate-100 dark:text-slate-400 dark:hover:bg-slate-800"
            >
              <span aria-hidden="true">✕</span>
            </button>
          </div>

          <div
            role="tablist"
            aria-label="Settings sections"
            onKeyDown={handleTablistKeyDown}
            className="mb-4 flex shrink-0 gap-1 border-b border-slate-200 dark:border-slate-700"
          >
            <button
              ref={generalTabRef}
              type="button"
              role="tab"
              id={generalTabId}
              aria-selected={activeTab === 'general'}
              aria-controls={generalPanelId}
              tabIndex={activeTab === 'general' ? 0 : -1}
              onClick={() => setActiveTab('general')}
              data-testid="settings-tab-general"
              className={`-mb-px rounded-t border-b-2 px-3 py-1 text-xs ${
                activeTab === 'general'
                  ? 'border-blue-600 font-medium text-blue-600 dark:text-blue-400'
                  : 'border-transparent text-slate-500 hover:text-slate-700 dark:text-slate-400 dark:hover:text-slate-200'
              }`}
            >
              General
            </button>
            <button
              ref={pluginsTabRef}
              type="button"
              role="tab"
              id={pluginsTabId}
              aria-selected={activeTab === 'plugins'}
              aria-controls={pluginsPanelId}
              tabIndex={activeTab === 'plugins' ? 0 : -1}
              onClick={() => setActiveTab('plugins')}
              data-testid="settings-tab-plugins"
              className={`-mb-px rounded-t border-b-2 px-3 py-1 text-xs ${
                activeTab === 'plugins'
                  ? 'border-blue-600 font-medium text-blue-600 dark:text-blue-400'
                  : 'border-transparent text-slate-500 hover:text-slate-700 dark:text-slate-400 dark:hover:text-slate-200'
              }`}
            >
              Plugins
            </button>
            <button
              ref={customProcessesTabRef}
              type="button"
              role="tab"
              id={customProcessesTabId}
              aria-selected={activeTab === 'customProcesses'}
              aria-controls={customProcessesPanelId}
              tabIndex={activeTab === 'customProcesses' ? 0 : -1}
              onClick={() => setActiveTab('customProcesses')}
              data-testid="settings-tab-custom-processes"
              className={`-mb-px rounded-t border-b-2 px-3 py-1 text-xs ${
                activeTab === 'customProcesses'
                  ? 'border-blue-600 font-medium text-blue-600 dark:text-blue-400'
                  : 'border-transparent text-slate-500 hover:text-slate-700 dark:text-slate-400 dark:hover:text-slate-200'
              }`}
            >
              Custom Processes
            </button>
            <button
              ref={aboutTabRef}
              type="button"
              role="tab"
              id={aboutTabId}
              aria-selected={activeTab === 'about'}
              aria-controls={aboutPanelId}
              tabIndex={activeTab === 'about' ? 0 : -1}
              onClick={() => setActiveTab('about')}
              data-testid="settings-tab-about"
              className={`-mb-px rounded-t border-b-2 px-3 py-1 text-xs ${
                activeTab === 'about'
                  ? 'border-blue-600 font-medium text-blue-600 dark:text-blue-400'
                  : 'border-transparent text-slate-500 hover:text-slate-700 dark:text-slate-400 dark:hover:text-slate-200'
              }`}
            >
              About
            </button>
          </div>

          {activeTab === 'general' ? (
            <div
              role="tabpanel"
              id={generalPanelId}
              aria-labelledby={generalTabId}
              data-testid="settings-panel-general"
              className="themed-scrollbar min-h-0 flex-1 overflow-y-auto pr-2"
            >
              <section className="mb-4">
                <h3 className="mb-1 text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400">Workspace</h3>
                <div className="flex items-center gap-2">
                  <p
                    className="min-w-0 flex-1 truncate rounded border border-slate-200 bg-slate-50 px-2 py-1 font-mono text-xs dark:border-slate-700 dark:bg-slate-800"
                    title={workspaceRoot ?? ''}
                    data-testid="settings-workspace-path"
                  >
                    {workspaceRoot ?? '(none)'}
                  </p>
                  <button
                    type="button"
                    onClick={() => setPicking(true)}
                    className="shrink-0 rounded border border-slate-300 bg-white px-2 py-1 text-xs hover:bg-slate-100 dark:border-slate-700 dark:bg-slate-800 dark:hover:bg-slate-700"
                  >
                    Change…
                  </button>
                </div>
                <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">Changing the workspace closes every open session.</p>
              </section>

              <section className="mb-4">
                <fieldset>
                  <legend className="mb-1 text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400">Appearance</legend>
                  <div className="flex items-center gap-3" data-testid="settings-theme-picker">
                    {(['system', 'light', 'dark'] as const).map((mode) => (
                      <label key={mode} className="flex items-center gap-1.5 text-xs">
                        <input
                          type="radio"
                          name="theme"
                          value={mode}
                          checked={themeInput === mode}
                          onChange={() => {
                            setSubmitError(null);
                            setThemeInput(mode);
                          }}
                          className="accent-blue-600"
                        />
                        {mode === 'system' ? 'System' : mode === 'light' ? 'Light' : 'Dark'}
                      </label>
                    ))}
                  </div>
                  <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">
                    Choose your preferred colour scheme. &ldquo;System&rdquo; follows the OS preference.
                  </p>
                </fieldset>
              </section>

              <section className="mb-4">
                <label
                  htmlFor="settings-worktree-prep"
                  className="mb-1 block text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400"
                >
                  Worktree Prep Commands
                </label>
                <textarea
                  id="settings-worktree-prep"
                  value={cmdsInput}
                  onChange={(e) => {
                    setSubmitError(null);
                    setCmdsInput(e.target.value);
                  }}
                  rows={5}
                  placeholder="One shell command per line, e.g.&#10;npm install&#10;cargo build"
                  className="w-full resize-y rounded border border-slate-300 bg-white px-2 py-1 font-mono text-xs dark:border-slate-700 dark:bg-slate-800"
                />
                <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">
                  Run once when a new worktree is created, in the worktree&apos;s directory. Combined output is captured to a log file. Blank lines
                  are ignored.
                </p>
              </section>

              {submitError && (
                <p
                  role="alert"
                  data-testid="settings-error"
                  className="mb-3 rounded border border-red-300 bg-red-50 px-2 py-1 text-xs text-red-800 dark:border-red-800 dark:bg-red-950 dark:text-red-200"
                >
                  {submitError}
                </p>
              )}

              <div className="flex justify-end gap-2">
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
                  onClick={() => void handleSave()}
                  disabled={!dirty || saving}
                  className="rounded bg-blue-600 px-3 py-1 text-xs font-medium text-white hover:bg-blue-500 disabled:opacity-50"
                >
                  {saving ? 'Saving…' : 'Save'}
                </button>
              </div>
            </div>
          ) : activeTab === 'plugins' ? (
            <div
              role="tabpanel"
              id={pluginsPanelId}
              aria-labelledby={pluginsTabId}
              data-testid="settings-panel-plugins"
              className="themed-scrollbar min-h-0 flex-1 overflow-x-hidden overflow-y-auto pr-2"
            >
              <PluginsTab onClose={onClose} />
            </div>
          ) : activeTab === 'customProcesses' ? (
            <div
              role="tabpanel"
              id={customProcessesPanelId}
              aria-labelledby={customProcessesTabId}
              data-testid="settings-panel-custom-processes"
              className="themed-scrollbar min-h-0 flex-1 overflow-x-hidden overflow-y-auto pr-2"
            >
              <CustomProcessesTab onClose={onClose} />
            </div>
          ) : (
            <div
              role="tabpanel"
              id={aboutPanelId}
              aria-labelledby={aboutTabId}
              data-testid="settings-panel-about"
              className="themed-scrollbar min-h-0 flex-1 overflow-y-auto pr-2"
            >
              <section className="space-y-3 text-xs leading-5 text-slate-600 dark:text-slate-300">
                <p data-testid="settings-about-attribution">
                  Arborist is created by <strong>mcaden</strong>.
                </p>
                <p>
                  Arborist is a cross-platform desktop app for managing multiple Claude CLI / GitHub Copilot CLI sessions, each bound to a Git
                  worktree.
                </p>
                <p>
                  It keeps each session&apos;s terminal persistent in the background so you can switch tabs quickly without interrupting running tasks
                  or losing output.
                </p>
              </section>
            </div>
          )}
        </div>
      </div>

      {picking ? (
        <div className="fixed inset-0 z-40 bg-black/40">
          <WorkspacePicker mode="change" initialPath={workspaceRoot} onConfirm={handleWorkspaceConfirm} onCancel={() => setPicking(false)} />
        </div>
      ) : null}
    </>
  );
}
