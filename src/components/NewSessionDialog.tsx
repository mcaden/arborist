// Two-step modal for creating a new session — SPEC §5.2 (C-01..C-08).
//
// Step 1: choose tool (Claude / Copilot)
// Step 2: choose worktree (quick-pick from `worktrees_list` for each
//         configured root, plus a "Browse…" fallback that opens the OS
//         directory picker), then Create the session.
//
// The instruction set is resolved silently at submit time:
//   configured default for the tool
//     → discovered `is_default` set for the tool
//     → first available set for the tool.
// The CLI inherits the worktree path as its `cwd`, so repository-level
// instructions (`CLAUDE.md`, `.github/copilot-instructions.md`) are
// auto-discovered regardless. SPEC I-03 ("SHOULD be selectable at
// session-creation time") is satisfied via the per-tool default in
// Settings rather than a wizard step (RFC 2119 SHOULD, not MUST).
//
// Renders via the native `<dialog>` element using `showModal()`/`close()`,
// matching the pattern set by `CloseConfirmDialog` in Phase 9 (jsdom shim
// installed in tests covers `showModal`/`close` so the same code path runs
// in unit tests). Focus is moved to the first interactive element on open;
// Esc cancels via the native `cancel` event.

import { useEffect, useMemo, useRef, useState } from 'react';

import { isInsideWorktreesDir } from '@/lib/worktree-paths';
import { formatError, pickDirectory, worktreeCreate, worktreesList } from '@/lib/tauri-bridge';
import { validateWorktreeName } from '@/lib/worktree-validation';
import { measureInitialPtyDimensions } from '@/hooks/use-terminal';
import { selectWorkspaceRoot, useConfigStore } from '@/store/config-store';
import { useNewSessionDialog } from '@/store/new-session-dialog-store';
import { useSessionActions } from '@/store/session-store';
import type { Tool, WorktreeInfo } from '@/types/arborist';

type Step = 1 | 2;
type WorktreeMode = 'existing' | 'new';

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
  const workspaceRoot = useConfigStore(selectWorkspaceRoot);

  const dialogRef = useRef<HTMLDialogElement | null>(null);
  const firstFocusRef = useRef<HTMLInputElement | null>(null);
  const stepBodyRef = useRef<HTMLDivElement | null>(null);
  const existingConfirmRef = useRef<HTMLButtonElement | null>(null);
  const isMountedRef = useRef<boolean>(false);
  // Monotonic request counter for worktree-list loads. Both the Step-2
  // useEffect and the post-create failure refresh kick off `worktreesList`
  // calls; the latest call wins so a slow earlier response can never
  // overwrite a fresher one with stale data.
  const worktreesRequestIdRef = useRef<number>(0);
  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);
  // Track the previous step so the focus-management effect only fires on
  // an actual transition, not on every step-body re-render (which would
  // steal focus away from the user as they typed).
  const previousStepRef = useRef<Step | null>(null);

  const [step, setStep] = useState<Step>(1);
  const [tool, setTool] = useState<Tool | null>(null);
  const [worktreeMode, setWorktreeMode] = useState<WorktreeMode>('new');
  const [worktree, setWorktree] = useState<ChosenWorktree | null>(null);
  const [worktrees, setWorktrees] = useState<WorktreeInfo[]>([]);
  const [worktreesLoading, setWorktreesLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  // "New worktree" sub-form state.
  const [newName, setNewName] = useState<string>('');
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);

  // Reset all wizard state whenever the dialog transitions to open. We do
  // this on the *open* edge so the dialog is fresh each time it appears
  // (SPEC C-01: closing mid-flow discards the in-progress selection).
  useEffect(() => {
    if (!isOpen) return;
    setStep(1);
    setTool(null);
    setWorktreeMode('new');
    setWorktree(null);
    setWorktrees([]);
    setSubmitting(false);
    setSubmitError(null);
    setNewName('');
    setCreating(false);
    setCreateError(null);
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

  // When the user lands on Step 2, list worktrees for the configured
  // workspace and filter to those under `<workspaceRoot>/.worktrees/`.
  // Worktrees outside that directory (the main checkout, ad-hoc paths)
  // are still reachable via "Browse…".
  useEffect(() => {
    // Invalidate any in-flight worktreesList from a prior render of this
    // effect first thing — including when the new render bails on the
    // isOpen/step/workspaceRoot guards below — so a stale earlier request
    // can never win the latest-id check after any dependency changes.
    const requestId = ++worktreesRequestIdRef.current;
    if (!isOpen || step !== 2) return;
    setWorktreesLoading(true);
    if (workspaceRoot === null || workspaceRoot.length === 0) {
      setWorktrees([]);
      setWorktreesLoading(false);
      return;
    }
    const root = workspaceRoot;
    worktreesList(root)
      .catch(() => [] as WorktreeInfo[])
      .then((list) => {
        if (!isMountedRef.current) return;
        if (requestId !== worktreesRequestIdRef.current) return;
        const filtered = list.filter((w) => isInsideWorktreesDir(root, w.path));
        setWorktrees(filtered);
      })
      .finally(() => {
        if (!isMountedRef.current) return;
        if (requestId === worktreesRequestIdRef.current) setWorktreesLoading(false);
      });
  }, [isOpen, step, workspaceRoot]);

  // Issue #63 retired the per-session prelaunch preview: prep commands now run
  // once on worktree creation (kicked off by `worktree_create` in the backend)
  // rather than ahead of every session shell. The post-create `worktree://prep`
  // event channel surfaces progress via `WorktreePrepBanner`.

  // Focus the first interactive element of the new step whenever the
  // user advances/goes back. Skip the very first render after open
  // (Step 1 focus is handled by the showModal effect, which targets
  // `firstFocusRef` so the test seam is preserved). Roadmap §8.1.
  useEffect(() => {
    if (!isOpen) {
      previousStepRef.current = null;
      return;
    }
    const previousStep = previousStepRef.current;
    previousStepRef.current = step;
    if (previousStep === null) return; // initial open — handled elsewhere
    if (previousStep === step) return;
    // On Step 2, focus the currently-selected tab so focus and
    // `aria-selected` stay consistent across Back/Next round-trips.
    if (step === 2) {
      const id = worktreeMode === 'new' ? 'worktree-tab-new' : 'worktree-tab-existing';
      const tab = document.getElementById(id);
      if (tab) {
        tab.focus();
        return;
      }
    }
    const body = stepBodyRef.current;
    if (!body) return;
    const candidate = body.querySelector<HTMLElement>(
      'input:not([disabled]), select:not([disabled]), textarea:not([disabled]), button:not([disabled]), [tabindex]:not([tabindex="-1"])',
    );
    candidate?.focus();
  }, [step, isOpen, worktreeMode]);

  const next = (): void => {
    if (step === 1 && tool) setStep(2);
  };

  const back = (): void => {
    if (step === 2) setStep(1);
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

  const newNameError = useMemo<string | null>(() => {
    const trimmed = newName.trim();
    if (trimmed.length === 0) return null;
    return validateWorktreeName(trimmed);
  }, [newName]);

  const onCreateWorktree = async (): Promise<void> => {
    const trimmed = newName.trim();
    if (trimmed.length === 0 || newNameError !== null || creating || !tool) return;
    setCreating(true);
    setCreateError(null);
    setSubmitError(null);
    try {
      const result = await worktreeCreate(trimmed);
      // Pre-select the new worktree so it's ready to use if session creation
      // fails and we fall back to the Existing tab. We deliberately do NOT
      // switch `worktreeMode` to 'existing' yet — doing so would swap the
      // footer to the Existing-mode "Create session" button while the chained
      // session-create is still in flight, allowing the user to trigger a
      // second concurrent session for the same worktree.
      setWorktree({ path: result.path, branch: trimmed, isMain: false });
      setNewName('');
      // The "New worktree" flow is one-shot: creating the worktree
      // immediately starts the session and closes the dialog. If session
      // creation fails, surface the error and switch to the Existing tab so
      // the user can retry via "Create session" without losing the worktree.
      // We refresh the worktree list lazily only on the failure path — on
      // success the dialog closes and the listing is never shown.
      try {
        const initialDims = measureInitialPtyDimensions();
        await actions.create({
          tool,
          worktreePath: result.path,
          cols: initialDims.cols,
          rows: initialDims.rows,
        });
        close();
      } catch (sessionErr) {
        setSubmitError(formatError(sessionErr));
        // Switch to the Existing tab and surface the retry button immediately
        // — don't make the user wait on a worktree listing refresh before they
        // can react to the failure. The list refresh runs in the background.
        setWorktreeMode('existing');
        if (workspaceRoot !== null && workspaceRoot.length > 0) {
          const root = workspaceRoot;
          const requestId = ++worktreesRequestIdRef.current;
          setWorktreesLoading(true);
          worktreesList(root)
            .catch(() => [] as WorktreeInfo[])
            .then((list) => {
              // Dialog may have been closed and the component unmounted
              // (e.g. by a fresh open + close) by the time this resolves.
              if (!isMountedRef.current) return;
              // A newer list request has been issued in the meantime; let
              // the latest one win to avoid stale data overwriting fresh.
              if (requestId !== worktreesRequestIdRef.current) return;
              setWorktrees(list.filter((w) => isInsideWorktreesDir(root, w.path)));
            })
            .finally(() => {
              if (!isMountedRef.current) return;
              if (requestId === worktreesRequestIdRef.current) setWorktreesLoading(false);
            });
        }
        // Move focus to the now-visible "Create session" button so keyboard
        // and screen-reader users land on the retry action rather than on
        // the body/document after the New-mode button unmounts.
        requestAnimationFrame(() => {
          existingConfirmRef.current?.focus();
        });
      }
    } catch (err) {
      setCreateError(formatError(err));
    } finally {
      setCreating(false);
    }
  };

  const onConfirm = async (): Promise<void> => {
    if (!tool || !worktree) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      // Instruction sets are intentionally not selected here. The CLI
      // tools auto-discover repository instructions from `cwd` (the
      // worktree): Claude reads `CLAUDE.md`, Copilot reads
      // `.github/copilot-instructions.md`. Power users can attach an
      // additional instruction-set overlay through Settings; this wizard
      // keeps the per-session create flow opinionated.
      const initialDims = measureInitialPtyDimensions();
      await actions.create({
        tool,
        worktreePath: worktree.path,
        cols: initialDims.cols,
        rows: initialDims.rows,
      });
      close();
    } catch (err) {
      setSubmitError(formatError(err));
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
        if (creating || submitting) return;
        onCancel();
      }}
      className="w-[28rem] rounded-md border border-slate-300 bg-white p-4 text-slate-900 shadow-lg backdrop:bg-black/40 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
    >
      <h2 id="new-session-title" className="mb-3 text-base font-semibold">
        New session — Step {step} of 2
      </h2>

      <div ref={stepBodyRef}>
        {step === 1 && (
          <fieldset className="mb-4">
            <legend className="mb-2 text-sm font-medium">Choose a tool</legend>
            <label className="mb-1 flex items-center gap-2 text-sm">
              <input ref={firstFocusRef} type="radio" name="tool" value="claude" checked={tool === 'claude'} onChange={() => setTool('claude')} />
              Claude
            </label>
            <label className="flex items-center gap-2 text-sm">
              <input type="radio" name="tool" value="copilot" checked={tool === 'copilot'} onChange={() => setTool('copilot')} />
              Copilot
            </label>
          </fieldset>
        )}

        {step === 2 && (
          <div className="mb-4">
            <p className="mb-2 text-sm font-medium">Choose a worktree</p>

            <div
              role="tablist"
              aria-label="Worktree source"
              className="mb-3 flex gap-1"
              onKeyDown={(e) => {
                if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight' && e.key !== 'Home' && e.key !== 'End') return;
                if (creating || submitting) return;
                e.preventDefault();
                const nextMode: WorktreeMode = e.key === 'Home' ? 'new' : e.key === 'End' ? 'existing' : worktreeMode === 'new' ? 'existing' : 'new';
                setWorktreeMode(nextMode);
                const id = nextMode === 'new' ? 'worktree-tab-new' : 'worktree-tab-existing';
                document.getElementById(id)?.focus();
              }}
            >
              <button
                type="button"
                role="tab"
                id="worktree-tab-new"
                aria-selected={worktreeMode === 'new'}
                aria-controls="worktree-panel-new"
                tabIndex={worktreeMode === 'new' ? 0 : -1}
                onClick={() => {
                  if (creating || submitting) return;
                  setWorktreeMode('new');
                }}
                disabled={creating || submitting}
                className={`rounded-t border-b-2 px-3 py-1.5 text-sm ${
                  worktreeMode === 'new'
                    ? 'border-sky-600 text-sky-700 dark:text-sky-300'
                    : 'border-transparent text-slate-500 hover:text-slate-700 dark:hover:text-slate-200'
                }`}
              >
                New
              </button>
              <button
                type="button"
                role="tab"
                id="worktree-tab-existing"
                aria-selected={worktreeMode === 'existing'}
                aria-controls="worktree-panel-existing"
                tabIndex={worktreeMode === 'existing' ? 0 : -1}
                onClick={() => {
                  if (creating || submitting) return;
                  setWorktreeMode('existing');
                }}
                disabled={creating || submitting}
                className={`rounded-t border-b-2 px-3 py-1.5 text-sm ${
                  worktreeMode === 'existing'
                    ? 'border-sky-600 text-sky-700 dark:text-sky-300'
                    : 'border-transparent text-slate-500 hover:text-slate-700 dark:hover:text-slate-200'
                }`}
              >
                Existing
              </button>
            </div>

            {/* Both tabpanels are always rendered so `aria-controls` on the tabs
                always points at a node in the DOM. The inactive panel is hidden
                via the native `hidden` attribute. */}
            <div role="tabpanel" id="worktree-panel-existing" aria-labelledby="worktree-tab-existing" hidden={worktreeMode !== 'existing'}>
              {worktreeMode === 'existing' && (
                <>
                  {worktreesLoading ? (
                    <p className="text-sm text-slate-500">Loading...</p>
                  ) : worktrees.length === 0 ? (
                    <p className="mb-2 text-sm text-slate-500">
                      No worktrees found in <span className="font-mono">.worktrees/</span> — create one in the New tab, or use Browse for a path
                      elsewhere.
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
                              {w.branch && <span className="rounded bg-slate-200 px-1.5 py-0.5 text-xs dark:bg-slate-600">{w.branch}</span>}
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
                </>
              )}
            </div>
            <div role="tabpanel" id="worktree-panel-new" aria-labelledby="worktree-tab-new" hidden={worktreeMode !== 'new'}>
              <label htmlFor="new-worktree-name" className="block text-sm font-medium">
                Branch / worktree name
              </label>
              <input
                id="new-worktree-name"
                type="text"
                value={newName}
                onChange={(e) => {
                  setNewName(e.target.value);
                  setCreateError(null);
                }}
                aria-invalid={newNameError !== null}
                aria-describedby={
                  newNameError !== null ? 'new-worktree-name-error' : createError !== null ? 'new-worktree-create-error' : 'new-worktree-name-help'
                }
                placeholder="my-feature"
                className="mt-1 w-full rounded border border-slate-300 bg-white px-3 py-2 text-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-700 dark:bg-slate-800"
              />
              {newNameError !== null ? (
                <p id="new-worktree-name-error" role="alert" className="mt-1 text-xs text-red-600 dark:text-red-400">
                  {newNameError}
                </p>
              ) : (
                <p id="new-worktree-name-help" className="mt-1 text-xs text-slate-500">
                  Will run{' '}
                  <span className="font-mono">
                    git worktree add .worktrees/{newName.trim() || 'NAME'} -b {newName.trim() || 'NAME'}
                  </span>
                </p>
              )}
              {createError !== null && (
                <p
                  id="new-worktree-create-error"
                  role="alert"
                  className="mt-2 rounded bg-red-100 px-2 py-1 text-xs text-red-800 dark:bg-red-900 dark:text-red-100"
                >
                  {createError}
                </p>
              )}
            </div>

            {worktree && <p className="mt-2 truncate text-xs text-slate-500">Selected: {worktree.path}</p>}

            {worktree && (
              <p className="mt-2 truncate text-xs text-slate-500">
                Label will be: <span className="font-mono">{deriveLabel(worktree.path)}</span>
              </p>
            )}
          </div>
        )}
      </div>

      {submitError && (
        <p role="alert" aria-live="polite" className="mb-2 rounded bg-red-100 px-2 py-1 text-xs text-red-800 dark:bg-red-900 dark:text-red-100">
          {submitError}
        </p>
      )}

      <div className="flex justify-between gap-2">
        <button
          type="button"
          onClick={onCancel}
          disabled={creating || submitting}
          className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-600 dark:bg-slate-700 dark:hover:bg-slate-600"
        >
          Cancel
        </button>
        <div className="flex gap-2">
          {step > 1 && (
            <button
              type="button"
              onClick={back}
              disabled={creating || submitting}
              className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-600 dark:bg-slate-700 dark:hover:bg-slate-600"
            >
              Back
            </button>
          )}
          {step < 2 && (
            <button
              type="button"
              onClick={next}
              disabled={step === 1 && !tool}
              className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 disabled:cursor-not-allowed disabled:bg-slate-300 dark:disabled:bg-slate-600"
            >
              Next
            </button>
          )}
          {step === 2 && worktreeMode === 'new' && (
            <button
              type="button"
              onClick={() => void onCreateWorktree()}
              disabled={creating || submitting || newName.trim().length === 0 || newNameError !== null}
              className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 disabled:cursor-not-allowed disabled:bg-slate-400"
            >
              {creating ? 'Creating…' : 'Create worktree & session'}
            </button>
          )}
          {step === 2 && worktreeMode === 'existing' && (
            <button
              ref={existingConfirmRef}
              type="button"
              onClick={() => {
                void onConfirm();
              }}
              disabled={submitting || creating || !worktree}
              className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 disabled:cursor-not-allowed disabled:bg-slate-400"
            >
              {submitting ? 'Creating…' : 'Create session'}
            </button>
          )}
        </div>
      </div>
    </dialog>
  );
}
