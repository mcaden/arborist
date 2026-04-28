// In-app settings panel — Roadmap §3.1.
//
// Reachable from the sidebar footer. Exposes the three workspace-level
// configuration knobs that users would otherwise have to edit by hand:
//   * workspace root (delegates to the existing WorkspacePicker so the
//     close-all-sessions invariant lives in one place — see
//     `lib/workspace-switch.ts`),
//   * instruction sets directory (path picker),
//   * pre-launch commands (one shell command per line).
//
// Per-worktree pre-launch overrides remain config-file–only in v1.

import { useCallback, useEffect, useId, useRef, useState } from 'react';

import { WorkspacePicker } from './WorkspacePicker';
import { pickDirectory } from '@/lib/tauri-bridge';
import { changeWorkspace } from '@/lib/workspace-switch';
import {
  selectInstructionSetsDir,
  selectPrelaunchCommands,
  selectWorkspaceRoot,
  useConfigStore,
} from '@/store/config-store';

export interface SettingsDialogProps {
  onClose: () => void;
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

export function SettingsDialog({ onClose }: SettingsDialogProps): JSX.Element {
  const workspaceRoot = useConfigStore(selectWorkspaceRoot);
  const instructionSetsDir = useConfigStore(selectInstructionSetsDir);
  const prelaunchCommands = useConfigStore(selectPrelaunchCommands);
  const setConfig = useConfigStore((s) => s.set);

  const [instrInput, setInstrInput] = useState<string>(instructionSetsDir);
  const [cmdsInput, setCmdsInput] = useState<string>(commandsToText(prelaunchCommands));
  const [saving, setSaving] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const headingId = useId();
  const closeBtnRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    closeBtnRef.current?.focus();
  }, []);

  // Re-sync local edit buffers if the persisted config changes underfoot
  // (e.g. via the workspace-change flow we delegate to WorkspacePicker).
  useEffect(() => {
    setInstrInput(instructionSetsDir);
  }, [instructionSetsDir]);
  useEffect(() => {
    setCmdsInput(commandsToText(prelaunchCommands));
  }, [prelaunchCommands]);

  const parsedCmds = textToCommands(cmdsInput);
  const dirty = instrInput !== instructionSetsDir || !arraysEqual(parsedCmds, prelaunchCommands);

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
      const patch: { instructionSetsDir?: string; prelaunchCommands?: string[] } = {};
      if (instrInput !== instructionSetsDir) patch.instructionSetsDir = instrInput;
      if (!arraysEqual(parsedCmds, prelaunchCommands)) patch.prelaunchCommands = parsedCmds;
      if (Object.keys(patch).length > 0) await setConfig(patch);
      onClose();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setSubmitError(message);
    } finally {
      setSaving(false);
    }
  }, [instrInput, instructionSetsDir, parsedCmds, prelaunchCommands, setConfig, onClose]);

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
        <div className="w-full max-w-lg rounded border border-slate-300 bg-white p-5 text-sm shadow-xl dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100">
          <div className="mb-4 flex items-start justify-between gap-3">
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

          <section className="mb-4">
            <h3 className="mb-1 text-xs font-medium uppercase tracking-wide text-slate-500 dark:text-slate-400">
              Workspace
            </h3>
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
            <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">
              Changing the workspace closes every open session.
            </p>
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
            <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">
              Run before every CLI session, in order. Blank lines are ignored.
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
      </div>

      {picking ? (
        <div className="fixed inset-0 z-40 bg-black/40">
          <WorkspacePicker
            mode="change"
            initialPath={workspaceRoot}
            onConfirm={handleWorkspaceConfirm}
            onCancel={() => setPicking(false)}
          />
        </div>
      ) : null}
    </>
  );
}
