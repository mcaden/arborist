// Three-step modal for creating a new session — SPEC §5.2 (C-01..C-08).
//
// Step 1: choose tool (Claude / Copilot)
// Step 2: choose worktree (quick-pick from `worktrees_list` for each
//         configured root, plus a "Browse…" fallback that opens the OS
//         directory picker)
// Step 3: choose instruction set (filtered by tool; `(none)` is allowed —
//         backend uses the per-tool default in that case)
//
// Renders via the native `<dialog>` element using `showModal()`/`close()`,
// matching the pattern set by `CloseConfirmDialog` in Phase 9 (jsdom shim
// installed in tests covers `showModal`/`close` so the same code path runs
// in unit tests). Focus is moved to the first interactive element on open;
// Esc cancels via the native `cancel` event.

import { useEffect, useMemo, useRef, useState } from 'react';

import {
  instructionsList as fetchInstructions,
  pickDirectory,
  worktreesList,
} from '@/lib/tauri-bridge';
import {
  selectDefaultInstructionSets,
  selectPrelaunchCommands,
  selectWorktreeRoots,
  useConfigStore,
} from '@/store/config-store';
import { useNewSessionDialog } from '@/store/new-session-dialog-store';
import { useSessionActions } from '@/store/session-store';
import type { InstructionSet, Tool, WorktreeInfo } from '@/types/grove';

type Step = 1 | 2 | 3;

interface ChosenWorktree {
  path: string;
  branch?: string;
  isMain: boolean;
}

function deriveLabel(path: string): string {
  // Strip both `/` and `\` so this works on either platform without
  // pulling in a path lib.
  const trimmed = path.replace(/[\\/]+$/, '');
  const idx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}

export function NewSessionDialog(): JSX.Element | null {
  const isOpen = useNewSessionDialog((s) => s.isOpen);
  const close = useNewSessionDialog((s) => s.close);
  const actions = useSessionActions();
  const worktreeRoots = useConfigStore(selectWorktreeRoots);
  const prelaunchCommands = useConfigStore(selectPrelaunchCommands);
  const defaultSets = useConfigStore(selectDefaultInstructionSets);

  const dialogRef = useRef<HTMLDialogElement | null>(null);
  const firstFocusRef = useRef<HTMLInputElement | null>(null);

  const [step, setStep] = useState<Step>(1);
  const [tool, setTool] = useState<Tool | null>(null);
  const [worktree, setWorktree] = useState<ChosenWorktree | null>(null);
  const [instructionSetId, setInstructionSetId] = useState<string | null>(null);
  const [worktrees, setWorktrees] = useState<WorktreeInfo[]>([]);
  const [worktreesLoading, setWorktreesLoading] = useState(false);
  const [allInstructions, setAllInstructions] = useState<InstructionSet[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  // Reset all wizard state whenever the dialog transitions to open. We do
  // this on the *open* edge so the dialog is fresh each time it appears
  // (SPEC C-01: closing mid-flow discards the in-progress selection).
  useEffect(() => {
    if (!isOpen) return;
    setStep(1);
    setTool(null);
    setWorktree(null);
    setInstructionSetId(null);
    setWorktrees([]);
    setAllInstructions([]);
    setSubmitting(false);
    setSubmitError(null);
  }, [isOpen]);

  // Show/hide the native <dialog>. Mirrors CloseConfirmDialog's jsdom-safe
  // dance.
  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (isOpen) {
      if (!dialog.open) {
        if (typeof dialog.showModal === 'function') {
          try {
            dialog.showModal();
          } catch {
            dialog.setAttribute('open', '');
          }
        } else {
          dialog.setAttribute('open', '');
        }
      }
      // Move focus into the dialog so keyboard users land somewhere
      // sensible. The first radio is the natural target on Step 1.
      firstFocusRef.current?.focus();
    } else if (dialog.open) {
      dialog.close();
    }
  }, [isOpen]);

  // Fetch instruction sets when the dialog opens (cheap; the backend
  // already memoises discovery effectively by filesystem cache).
  useEffect(() => {
    if (!isOpen) return;
    let cancelled = false;
    fetchInstructions()
      .then((sets) => {
        if (!cancelled) setAllInstructions(sets);
      })
      .catch(() => {
        // Discovery failure is non-fatal for the create flow — the user
        // can still pick "(none)" and let the backend use defaults.
        if (!cancelled) setAllInstructions([]);
      });
    return () => {
      cancelled = true;
    };
  }, [isOpen]);

  // When the user lands on Step 2, fan out a `worktrees_list` per
  // configured root and concatenate. Any per-root failure is silently
  // dropped (the backend already swallows them and returns `[]`).
  useEffect(() => {
    if (!isOpen || step !== 2) return;
    let cancelled = false;
    setWorktreesLoading(true);
    const roots = worktreeRoots;
    if (roots.length === 0) {
      setWorktrees([]);
      setWorktreesLoading(false);
      return;
    }
    Promise.all(roots.map((r) => worktreesList(r).catch(() => [] as WorktreeInfo[])))
      .then((lists) => {
        if (cancelled) return;
        // Deduplicate by absolute path — a worktree might be reachable
        // from multiple configured roots.
        const seen = new Set<string>();
        const merged: WorktreeInfo[] = [];
        for (const list of lists) {
          for (const w of list) {
            if (seen.has(w.path)) continue;
            seen.add(w.path);
            merged.push(w);
          }
        }
        setWorktrees(merged);
      })
      .finally(() => {
        if (!cancelled) setWorktreesLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [isOpen, step, worktreeRoots]);

  // Filter instruction sets by the selected tool so changing the tool
  // after Step 1 always re-derives the visible options.
  const filteredInstructions = useMemo<InstructionSet[]>(
    () => (tool ? allInstructions.filter((s) => s.tool === tool) : []),
    [allInstructions, tool],
  );

  // If the previously-selected instruction set is no longer in the
  // filtered list (e.g. the user backed up to Step 1 and switched tool),
  // drop the stale selection so the form doesn't submit a mismatched id.
  useEffect(() => {
    if (instructionSetId === null) return;
    if (!filteredInstructions.some((s) => s.id === instructionSetId)) {
      setInstructionSetId(null);
    }
  }, [filteredInstructions, instructionSetId]);

  // Resolve the prelaunchCommands the backend would actually run for the
  // chosen worktree (DESIGN §5.6 / §8.1): per-worktree override (if set)
  // wins, else the global list. We don't have a TS-side override map per
  // worktree yet (config-store exposes only the global list), so we
  // surface the global list as the preview.
  const previewPrelaunch = prelaunchCommands;

  const next = (): void => {
    if (step === 1 && tool) setStep(2);
    else if (step === 2 && worktree) setStep(3);
  };

  const back = (): void => {
    if (step === 3) setStep(2);
    else if (step === 2) setStep(1);
  };

  const onCancel = (): void => {
    close();
  };

  const onPickDirectory = async (): Promise<void> => {
    const picked = await pickDirectory();
    if (picked) {
      setWorktree({ path: picked, isMain: false });
    }
  };

  const onConfirm = async (): Promise<void> => {
    if (!tool || !worktree) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      await actions.create({
        tool,
        worktreePath: worktree.path,
        // "(none)" maps to the per-tool default the backend has already
        // canonicalised on disk. The backend rejects empty ids with a
        // NotFound error, so we resolve the fallback here rather than
        // relying on a sentinel.
        instructionSetId: instructionSetId ?? defaultSets[tool],
      });
      close();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setSubmitError(message);
    } finally {
      setSubmitting(false);
    }
  };

  if (!isOpen) {
    // Still render the <dialog> shell when *closed* would mean we lose
    // the ref; instead we render nothing and let the open-edge effect
    // re-create the element next time. This keeps the DOM clean.
    return null;
  }

  return (
    <dialog
      ref={dialogRef}
      role="dialog"
      aria-labelledby="new-session-title"
      onCancel={(e) => {
        e.preventDefault();
        onCancel();
      }}
      className="w-[28rem] rounded-md border border-slate-300 bg-white p-4 text-slate-900 shadow-lg backdrop:bg-black/40 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
    >
      <h2 id="new-session-title" className="mb-3 text-base font-semibold">
        New session — Step {step} of 3
      </h2>

      {step === 1 && (
        <fieldset className="mb-4">
          <legend className="mb-2 text-sm font-medium">Choose a tool</legend>
          <label className="mb-1 flex items-center gap-2 text-sm">
            <input
              ref={firstFocusRef}
              type="radio"
              name="tool"
              value="claude"
              checked={tool === 'claude'}
              onChange={() => setTool('claude')}
            />
            Claude
          </label>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="radio"
              name="tool"
              value="copilot"
              checked={tool === 'copilot'}
              onChange={() => setTool('copilot')}
            />
            Copilot
          </label>
        </fieldset>
      )}

      {step === 2 && (
        <div className="mb-4">
          <p className="mb-2 text-sm font-medium">Choose a worktree</p>
          {worktreesLoading ? (
            <p className="text-sm text-slate-500">Loading...</p>
          ) : worktrees.length === 0 ? (
            <p className="mb-2 text-sm text-slate-500">
              Could not detect git worktrees - pick a directory manually.
            </p>
          ) : (
            <ul className="mb-2 max-h-48 overflow-y-auto rounded border border-slate-200 dark:border-slate-700">
              {worktrees.map((w) => (
                <li key={w.path}>
                  <button
                    type="button"
                    onClick={() =>
                      setWorktree({
                        path: w.path,
                        ...(w.branch !== undefined ? { branch: w.branch } : {}),
                        isMain: w.isMain,
                      })
                    }
                    className={`flex w-full items-center justify-between gap-2 px-3 py-2 text-left text-sm hover:bg-slate-100 dark:hover:bg-slate-700 ${
                      worktree?.path === w.path ? 'bg-sky-100 dark:bg-sky-900' : ''
                    }`}
                  >
                    <span className="truncate font-mono">{w.path}</span>
                    <span className="flex shrink-0 items-center gap-1">
                      {w.branch && (
                        <span className="rounded bg-slate-200 px-1.5 py-0.5 text-xs dark:bg-slate-600">
                          {w.branch}
                        </span>
                      )}
                      {w.isMain && (
                        <span className="rounded bg-emerald-200 px-1.5 py-0.5 text-xs text-emerald-900 dark:bg-emerald-700 dark:text-emerald-50">
                          main
                        </span>
                      )}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
          <button
            type="button"
            onClick={() => {
              void onPickDirectory();
            }}
            className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm hover:bg-slate-100 dark:border-slate-600 dark:bg-slate-700 dark:hover:bg-slate-600"
          >
            Browse...
          </button>
          {worktree && (
            <p className="mt-2 truncate text-xs text-slate-500">Selected: {worktree.path}</p>
          )}
        </div>
      )}

      {step === 3 && (
        <div className="mb-4">
          <p className="mb-2 text-sm font-medium">Choose an instruction set</p>
          <ul className="mb-3 max-h-48 overflow-y-auto rounded border border-slate-200 dark:border-slate-700">
            <li>
              <label className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-sm hover:bg-slate-100 dark:hover:bg-slate-700">
                <input
                  type="radio"
                  name="instruction-set"
                  value=""
                  checked={instructionSetId === null}
                  onChange={() => setInstructionSetId(null)}
                />
                <span>(none)</span>
              </label>
            </li>
            {filteredInstructions.map((s) => (
              <li key={s.id}>
                <label className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-sm hover:bg-slate-100 dark:hover:bg-slate-700">
                  <input
                    type="radio"
                    name="instruction-set"
                    value={s.id}
                    checked={instructionSetId === s.id}
                    onChange={() => setInstructionSetId(s.id)}
                  />
                  <span>
                    {s.name}
                    {s.isDefault && (
                      <span className="ml-2 rounded bg-slate-200 px-1.5 py-0.5 text-xs dark:bg-slate-600">
                        default
                      </span>
                    )}
                  </span>
                </label>
              </li>
            ))}
          </ul>

          <details className="rounded border border-slate-200 px-2 py-1 text-xs dark:border-slate-700">
            <summary className="cursor-pointer">
              Pre-launch commands ({previewPrelaunch.length})
            </summary>
            {previewPrelaunch.length === 0 ? (
              <p className="px-1 py-1 text-slate-500">(none)</p>
            ) : (
              <ol className="list-decimal space-y-0.5 px-5 py-1 font-mono">
                {previewPrelaunch.map((cmd, idx) => (
                  <li key={`${idx}-${cmd}`}>{cmd}</li>
                ))}
              </ol>
            )}
          </details>

          {worktree && (
            <p className="mt-2 truncate text-xs text-slate-500">
              Label will be: <span className="font-mono">{deriveLabel(worktree.path)}</span>
            </p>
          )}
        </div>
      )}

      {submitError && (
        <p className="mb-2 rounded bg-red-100 px-2 py-1 text-xs text-red-800 dark:bg-red-900 dark:text-red-100">
          {submitError}
        </p>
      )}

      <div className="flex justify-between gap-2">
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm hover:bg-slate-100 dark:border-slate-600 dark:bg-slate-700 dark:hover:bg-slate-600"
        >
          Cancel
        </button>
        <div className="flex gap-2">
          {step > 1 && (
            <button
              type="button"
              onClick={back}
              className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm hover:bg-slate-100 dark:border-slate-600 dark:bg-slate-700 dark:hover:bg-slate-600"
            >
              Back
            </button>
          )}
          {step < 3 && (
            <button
              type="button"
              onClick={next}
              disabled={(step === 1 && !tool) || (step === 2 && !worktree)}
              className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 disabled:cursor-not-allowed disabled:bg-slate-300 dark:disabled:bg-slate-600"
            >
              Next
            </button>
          )}
          {step === 3 && (
            <button
              type="button"
              onClick={() => {
                void onConfirm();
              }}
              disabled={submitting}
              className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 disabled:cursor-not-allowed disabled:bg-slate-400"
            >
              {submitting ? 'Creating...' : 'Create session'}
            </button>
          )}
        </div>
      </div>
    </dialog>
  );
}
