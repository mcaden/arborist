// In-app settings panel — Roadmap §3.1.
//
// Reachable from the sidebar footer (and from the tab context menu's
// empty Launch submenu, which jumps straight to the Custom Processes
// tab). Two tabs:
//
//   General           — workspace root (delegates to the existing
//                       WorkspacePicker so the park-old-sessions
//                       invariant lives in one place — see
//                       `lib/workspace-switch.ts`), instruction sets
//                       directory (path picker), pre-launch commands
//                       (one shell command per line), and per-agent CLI
//                       launch overrides (claude / copilot).
//   Custom Processes  — CRUD over `AppConfig.customProcesses` (lives in
//                       a dedicated `CustomProcessesTab` component).
//
// Per-worktree pre-launch overrides remain config-file–only in v1.

import type { KeyboardEvent as ReactKeyboardEvent, RefObject } from 'react';
import { useCallback, useEffect, useId, useRef, useState } from 'react';

import { CustomProcessesTab } from './CustomProcessesTab';
import { WorkspacePicker } from './WorkspacePicker';
import { formatError, pickDirectory } from '@/lib/tauri-bridge';
import { changeWorkspace } from '@/lib/workspace-switch';
import { selectAiLaunchCommands, selectInstructionSetsDir, selectPrelaunchCommands, selectWorkspaceRoot, useConfigStore } from '@/store/config-store';

export type SettingsTab = 'general' | 'customProcesses';

export interface SettingsDialogProps {
  onClose: () => void;
  /** Which tab to show first. Defaults to `'general'`. */
  initialTab?: SettingsTab;
}

/**
 * Convert the prelaunch-commands list to/from the textarea's plain-text
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
  const instructionSetsDir = useConfigStore(selectInstructionSetsDir);
  const prelaunchCommands = useConfigStore(selectPrelaunchCommands);
  const aiLaunchCommands = useConfigStore(selectAiLaunchCommands);
  const setConfig = useConfigStore((s) => s.set);

  const [activeTab, setActiveTab] = useState<SettingsTab>(initialTab);
  const [instrInput, setInstrInput] = useState<string>(instructionSetsDir);
  const [cmdsInput, setCmdsInput] = useState<string>(commandsToText(prelaunchCommands));
  const [claudeCmdInput, setClaudeCmdInput] = useState<string>(aiLaunchCommands.claude);
  const [copilotCmdInput, setCopilotCmdInput] = useState<string>(aiLaunchCommands.copilot);
  const [saving, setSaving] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const headingId = useId();
  const generalTabId = useId();
  const customProcessesTabId = useId();
  const generalPanelId = useId();
  const customProcessesPanelId = useId();
  const closeBtnRef = useRef<HTMLButtonElement | null>(null);
  const generalTabRef = useRef<HTMLButtonElement | null>(null);
  const customProcessesTabRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    closeBtnRef.current?.focus();
  }, []);

  // WAI-ARIA tab keyboard model: ←/→/Home/End move between tabs (with
  // wrap), and the move both selects and focuses (manual activation
  // would mean two keypresses to reach a tab the user clearly wants).
  const handleTablistKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>): void => {
      const order: SettingsTab[] = ['general', 'customProcesses'];
      const refs: Record<SettingsTab, RefObject<HTMLButtonElement>> = {
        general: generalTabRef,
        customProcesses: customProcessesTabRef,
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
    setInstrInput(instructionSetsDir);
  }, [instructionSetsDir]);
  useEffect(() => {
    setCmdsInput(commandsToText(prelaunchCommands));
  }, [prelaunchCommands]);
  useEffect(() => {
    setClaudeCmdInput(aiLaunchCommands.claude);
  }, [aiLaunchCommands.claude]);
  useEffect(() => {
    setCopilotCmdInput(aiLaunchCommands.copilot);
  }, [aiLaunchCommands.copilot]);

  const parsedCmds = textToCommands(cmdsInput);
  const claudeCmdTrimmed = claudeCmdInput.trim();
  const copilotCmdTrimmed = copilotCmdInput.trim();
  const dirty =
    instrInput !== instructionSetsDir ||
    !arraysEqual(parsedCmds, prelaunchCommands) ||
    claudeCmdTrimmed !== aiLaunchCommands.claude ||
    copilotCmdTrimmed !== aiLaunchCommands.copilot;

  const handleBrowseInstructions = useCallback(async () => {
    const picked = await pickDirectory();
    if (picked) {
      setSubmitError(null);
      setInstrInput(picked);
    }
  }, []);

  const handleSave = useCallback(async () => {
    setSubmitError(null);
    setSaving(true);
    try {
      const patch: {
        instructionSetsDir?: string;
        prelaunchCommands?: string[];
        aiLaunchCommands?: { claude?: string; copilot?: string };
      } = {};
      if (instrInput !== instructionSetsDir) patch.instructionSetsDir = instrInput;
      if (!arraysEqual(parsedCmds, prelaunchCommands)) patch.prelaunchCommands = parsedCmds;
      const launchPatch: { claude?: string; copilot?: string } = {};
      if (claudeCmdTrimmed !== aiLaunchCommands.claude) launchPatch.claude = claudeCmdTrimmed;
      if (copilotCmdTrimmed !== aiLaunchCommands.copilot) launchPatch.copilot = copilotCmdTrimmed;
      if (Object.keys(launchPatch).length > 0) patch.aiLaunchCommands = launchPatch;
      if (Object.keys(patch).length > 0) await setConfig(patch);
      onClose();
    } catch (err) {
      const message = formatError(err);
      setSubmitError(message);
    } finally {
      setSaving(false);
    }
  }, [
    instrInput,
    instructionSetsDir,
    parsedCmds,
    prelaunchCommands,
    claudeCmdTrimmed,
    copilotCmdTrimmed,
    aiLaunchCommands.claude,
    aiLaunchCommands.copilot,
    setConfig,
    onClose,
  ]);

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
          </div>

          {activeTab === 'general' ? (
            <div
              role="tabpanel"
              id={generalPanelId}
              aria-labelledby={generalTabId}
              data-testid="settings-panel-general"
              className="min-h-0 flex-1 overflow-y-auto"
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
                <label
                  htmlFor="settings-instr-dir"
                  className="mb-1 block text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400"
                >
                  Instruction sets directory
                </label>
                <div className="flex items-center gap-2">
                  <input
                    id="settings-instr-dir"
                    type="text"
                    value={instrInput}
                    onChange={(e) => {
                      setSubmitError(null);
                      setInstrInput(e.target.value);
                    }}
                    placeholder="(absolute path)"
                    className="min-w-0 flex-1 rounded border border-slate-300 bg-white px-2 py-1 font-mono text-xs dark:border-slate-700 dark:bg-slate-800"
                  />
                  <button
                    type="button"
                    onClick={() => void handleBrowseInstructions()}
                    className="shrink-0 rounded border border-slate-300 bg-white px-2 py-1 text-xs hover:bg-slate-100 dark:border-slate-700 dark:bg-slate-800 dark:hover:bg-slate-700"
                  >
                    Browse…
                  </button>
                </div>
              </section>

              <section className="mb-4">
                <label
                  htmlFor="settings-prelaunch"
                  className="mb-1 block text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400"
                >
                  Pre-launch commands
                </label>
                <textarea
                  id="settings-prelaunch"
                  value={cmdsInput}
                  onChange={(e) => {
                    setSubmitError(null);
                    setCmdsInput(e.target.value);
                  }}
                  rows={5}
                  placeholder="One shell command per line, e.g.&#10;source ~/.zshenv&#10;nvm use 20"
                  className="w-full resize-y rounded border border-slate-300 bg-white px-2 py-1 font-mono text-xs dark:border-slate-700 dark:bg-slate-800"
                />
                <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">Run before every CLI session, in order. Blank lines are ignored.</p>
              </section>

              <section className="mb-4">
                <h3 className="mb-1 text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400">AI agent launch commands</h3>
                <p className="mb-2 text-xs text-slate-500 dark:text-slate-400">
                  Replace the default CLI invocation for each agent. The text is passed to the shell verbatim, so you may include arguments (e.g.{' '}
                  <code>npx claude --model sonnet</code>). Leave blank to use the default.
                </p>
                <div className="mb-2">
                  <label htmlFor="settings-launch-claude" className="mb-1 block text-xs text-slate-600 dark:text-slate-300">
                    Claude
                  </label>
                  <input
                    id="settings-launch-claude"
                    type="text"
                    value={claudeCmdInput}
                    onChange={(e) => {
                      setSubmitError(null);
                      setClaudeCmdInput(e.target.value);
                    }}
                    placeholder="claude"
                    spellCheck={false}
                    data-testid="settings-launch-claude"
                    className="w-full rounded border border-slate-300 bg-white px-2 py-1 font-mono text-xs dark:border-slate-700 dark:bg-slate-800"
                  />
                </div>
                <div>
                  <label htmlFor="settings-launch-copilot" className="mb-1 block text-xs text-slate-600 dark:text-slate-300">
                    GitHub Copilot
                  </label>
                  <input
                    id="settings-launch-copilot"
                    type="text"
                    value={copilotCmdInput}
                    onChange={(e) => {
                      setSubmitError(null);
                      setCopilotCmdInput(e.target.value);
                    }}
                    placeholder="copilot"
                    spellCheck={false}
                    data-testid="settings-launch-copilot"
                    className="w-full rounded border border-slate-300 bg-white px-2 py-1 font-mono text-xs dark:border-slate-700 dark:bg-slate-800"
                  />
                </div>
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
          ) : (
            <div
              role="tabpanel"
              id={customProcessesPanelId}
              aria-labelledby={customProcessesTabId}
              data-testid="settings-panel-custom-processes"
              className="min-h-0 flex-1 overflow-y-auto"
            >
              <CustomProcessesTab onClose={onClose} />
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
