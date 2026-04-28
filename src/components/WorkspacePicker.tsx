// First-boot workspace picker — Roadmap §1.1.
//
// Shown when `config.workspaceRoot` is null. The user enters or browses
// for a path, the bridge validates it, and on confirm we persist the
// canonical path via `config.set({ workspaceRoot })`.
//
// The picker is also reachable mid-session via the "Change workspace…"
// button in the sidebar header; in that mode `mode === 'change'` and a
// Cancel button is exposed.
//
// Validation is async (one round-trip per debounced change), so the
// component surfaces three textual states in addition to the inline
// error: idle / validating / validated. The Confirm button is enabled
// only when the most recent validation succeeded for the *current* input.

import { useCallback, useEffect, useId, useRef, useState } from 'react';

import { pickDirectory, workspaceValidate } from '@/lib/tauri-bridge';

export type WorkspacePickerMode = 'first-boot' | 'change';

export interface WorkspacePickerProps {
  mode: WorkspacePickerMode;
  /** Currently-persisted workspace root (used to seed the input on 'change'). */
  initialPath?: string | null;
  onConfirm: (path: string) => void | Promise<void>;
  onCancel?: () => void;
}

type ValidationState =
  | { kind: 'idle' }
  | { kind: 'validating' }
  | { kind: 'valid' }
  | { kind: 'invalid'; error: string };

const DEBOUNCE_MS = 250;

export function WorkspacePicker({
  mode,
  initialPath,
  onConfirm,
  onCancel,
}: WorkspacePickerProps): JSX.Element {
  const [path, setPath] = useState<string>(initialPath ?? '');
  const [validation, setValidation] = useState<ValidationState>({ kind: 'idle' });
  const [submitting, setSubmitting] = useState<boolean>(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const inputId = useId();
  const errorId = useId();
  const inputRef = useRef<HTMLInputElement | null>(null);

  // Track the latest in-flight validation request so a slow earlier
  // response cannot overwrite the result for a newer input.
  const requestSeq = useRef(0);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    // Editing the path invalidates any prior submission error — keep the
    // UI in sync with the user's current intent.
    setSubmitError(null);
    const trimmed = path.trim();
    if (trimmed.length === 0) {
      setValidation({ kind: 'idle' });
      return;
    }
    setValidation({ kind: 'validating' });
    const seq = ++requestSeq.current;
    const handle = window.setTimeout(() => {
      void (async () => {
        try {
          const result = await workspaceValidate(trimmed);
          if (seq !== requestSeq.current) return;
          if (result.valid) {
            setValidation({ kind: 'valid' });
          } else {
            setValidation({ kind: 'invalid', error: result.error ?? 'invalid path' });
          }
        } catch (err) {
          if (seq !== requestSeq.current) return;
          const message = err instanceof Error ? err.message : String(err);
          setValidation({ kind: 'invalid', error: message });
        }
      })();
    }, DEBOUNCE_MS);
    return () => window.clearTimeout(handle);
  }, [path]);

  const handleBrowse = useCallback(async () => {
    try {
      const picked = await pickDirectory();
      if (picked) setPath(picked);
    } catch {
      // pickDirectory swallows cancellation; surface other failures via
      // the existing validation state on the next debounce cycle.
    }
  }, []);

  const canSubmit = validation.kind === 'valid' && !submitting;

  const handleSubmit = useCallback(async () => {
    if (!canSubmit) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      await onConfirm(path.trim());
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setSubmitError(message);
      setSubmitting(false);
    }
  }, [canSubmit, onConfirm, path]);

  const heading = mode === 'first-boot' ? 'Choose your workspace' : 'Change workspace';
  const sub =
    mode === 'first-boot'
      ? 'Arborist needs the path to a git repository it can manage worktrees in. This is the directory that contains the .git folder.'
      : 'Switching workspaces will close every open session. The new workspace must be the root of a git repository.';

  const errorText = validation.kind === 'invalid' ? validation.error : (submitError ?? null);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby={`${inputId}-heading`}
      className="flex h-full w-full items-center justify-center bg-white p-8 text-slate-900 dark:bg-slate-900 dark:text-slate-100"
    >
      <div className="w-full max-w-lg">
        <h1 id={`${inputId}-heading`} className="text-xl font-semibold">
          {heading}
        </h1>
        <p className="mt-2 text-sm text-slate-600 dark:text-slate-300">{sub}</p>

        <label
          htmlFor={inputId}
          className="mt-6 block text-sm font-medium text-slate-700 dark:text-slate-200"
        >
          Workspace path
        </label>
        <div className="mt-2 flex gap-2">
          <input
            ref={inputRef}
            id={inputId}
            type="text"
            value={path}
            onChange={(e) => setPath(e.target.value)}
            aria-invalid={validation.kind === 'invalid'}
            aria-describedby={errorText ? errorId : undefined}
            placeholder="/path/to/repo"
            className="min-w-0 flex-1 rounded border border-slate-300 bg-white px-3 py-2 text-sm focus:border-blue-500 focus:outline-none focus:ring-1 focus:ring-blue-500 dark:border-slate-700 dark:bg-slate-800"
          />
          <button
            type="button"
            onClick={() => void handleBrowse()}
            className="rounded border border-slate-300 bg-slate-100 px-3 py-2 text-sm hover:bg-slate-200 dark:border-slate-700 dark:bg-slate-800 dark:hover:bg-slate-700"
          >
            Browse…
          </button>
        </div>

        <p aria-live="polite" className="mt-2 text-xs" data-testid="picker-status">
          {validation.kind === 'validating' ? (
            <span className="text-slate-500 dark:text-slate-400">Validating…</span>
          ) : validation.kind === 'valid' ? (
            <span className="text-emerald-600 dark:text-emerald-400">
              Looks good — git repository detected.
            </span>
          ) : null}
        </p>

        {errorText !== null ? (
          <p id={errorId} role="alert" className="mt-2 text-sm text-red-600 dark:text-red-400">
            {errorText}
          </p>
        ) : null}

        <div className="mt-6 flex justify-end gap-2">
          {mode === 'change' && onCancel ? (
            <button
              type="button"
              onClick={onCancel}
              disabled={submitting}
              className="rounded border border-slate-300 bg-white px-4 py-2 text-sm hover:bg-slate-100 disabled:opacity-50 dark:border-slate-700 dark:bg-slate-800 dark:hover:bg-slate-700"
            >
              Cancel
            </button>
          ) : null}
          <button
            type="button"
            onClick={() => void handleSubmit()}
            disabled={!canSubmit}
            className="rounded bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-700 disabled:opacity-50"
          >
            {submitting ? 'Saving…' : mode === 'first-boot' ? 'Continue' : 'Switch workspace'}
          </button>
        </div>
      </div>
    </div>
  );
}
