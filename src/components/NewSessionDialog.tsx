// Single-step modal for opening a worktree tab — the "+" button in the
// sidebar. After the worktree-as-parent-tab restructure (issue #44), the
// "+" button no longer creates an AI session directly; it opens (or
// creates) a worktree tab. The user then right-clicks the worktree tab
// and uses Launch Claude / Launch Copilot to start a session.
//
// The user can choose an existing worktree from the workspace's
// `.arborist/.worktrees/` directory, or create a new one by name.
//
// Renders via the native `<dialog>` element using `showModal()`/`close()`,
// matching the pattern set by `WorktreeCloseConfirmDialog` (jsdom shim installed
// in tests covers `showModal`/`close`).

import { useEffect, useMemo, useRef, useState } from 'react';

import { isInsideWorktreesDir } from '@/lib/worktree-paths';
import { formatError, pickDirectory, worktreeCreate, worktreesList } from '@/lib/tauri-bridge';
import { validateWorktreeName } from '@/lib/worktree-validation';
import { selectWorkspaceRoot, useConfigStore } from '@/store/config-store';
import { useNewSessionDialog } from '@/store/new-session-dialog-store';
import { useWorktreeTabActions } from '@/store/worktree-tab-store';
import type { WorktreeInfo } from '@/types/arborist';

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
  const wttActions = useWorktreeTabActions();
  const workspaceRoot = useConfigStore(selectWorkspaceRoot);

  const dialogRef = useRef<HTMLDialogElement | null>(null);
  const firstFocusRef = useRef<HTMLInputElement | null>(null);
  const existingConfirmRef = useRef<HTMLButtonElement | null>(null);
  const isMountedRef = useRef<boolean>(false);
  // Monotonic request counter for worktree-list loads.
  const worktreesRequestIdRef = useRef<number>(0);
  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

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

  // Reset state whenever the dialog opens.
  useEffect(() => {
    if (!isOpen) return;
    setWorktreeMode('new');
    setWorktree(null);
    setWorktrees([]);
    setSubmitting(false);
    setSubmitError(null);
    setNewName('');
    setCreating(false);
    setCreateError(null);
  }, [isOpen]);

  // Show/hide the native <dialog>.
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
      firstFocusRef.current?.focus();
    } else if (dialog.open) {
      dialog.close();
    }
  }, [isOpen]);

  // List worktrees when the dialog opens.
  useEffect(() => {
    const requestId = ++worktreesRequestIdRef.current;
    if (!isOpen) return;
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
  }, [isOpen, workspaceRoot]);

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
    if (trimmed.length === 0 || newNameError !== null || creating) return;
    setCreating(true);
    setCreateError(null);
    setSubmitError(null);
    try {
      const result = await worktreeCreate(trimmed);
      setWorktree({ path: result.path, branch: trimmed, isMain: false });
      setNewName('');
      // Open the worktree tab and close the dialog.
      try {
        await wttActions.open(result.path);
        close();
      } catch (openErr) {
        setSubmitError(formatError(openErr));
        setWorktreeMode('existing');
        // Refresh the list in the background so the new worktree shows up.
        if (workspaceRoot !== null && workspaceRoot.length > 0) {
          const root = workspaceRoot;
          const requestId = ++worktreesRequestIdRef.current;
          setWorktreesLoading(true);
          worktreesList(root)
            .catch(() => [] as WorktreeInfo[])
            .then((list) => {
              if (!isMountedRef.current) return;
              if (requestId !== worktreesRequestIdRef.current) return;
              setWorktrees(list.filter((w) => isInsideWorktreesDir(root, w.path)));
            })
            .finally(() => {
              if (!isMountedRef.current) return;
              if (requestId === worktreesRequestIdRef.current) setWorktreesLoading(false);
            });
        }
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

  const onConfirmExisting = async (): Promise<void> => {
    if (!worktree) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      await wttActions.open(worktree.path);
      close();
    } catch (err) {
      setSubmitError(formatError(err));
    } finally {
      setSubmitting(false);
    }
  };

  if (!isOpen) return null;

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
      className="fixed inset-0 m-auto h-fit w-[28rem] rounded-md border border-slate-300 bg-white p-4 text-slate-900 shadow-lg backdrop:bg-black/40 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
    >
      <h2 id="new-session-title" className="mb-3 text-base font-semibold">
        Add worktree
      </h2>

      <div className="mb-4">
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
            ref={(el) => {
              if (worktreeMode === 'new') (firstFocusRef as React.MutableRefObject<HTMLElement | null>).current = el;
            }}
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
            ref={(el) => {
              if (worktreeMode === 'existing') (firstFocusRef as React.MutableRefObject<HTMLElement | null>).current = el;
            }}
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

        <div role="tabpanel" id="worktree-panel-existing" aria-labelledby="worktree-tab-existing" hidden={worktreeMode !== 'existing'}>
          {worktreeMode === 'existing' && (
            <>
              {worktreesLoading ? (
                <p className="text-sm text-slate-500">Loading...</p>
              ) : worktrees.length === 0 ? (
                <p className="mb-2 text-sm text-slate-500">
                  No worktrees found in <span className="font-mono">.arborist/.worktrees/</span> — create one in the New tab, or use Browse for a path
                  elsewhere.
                </p>
              ) : (
                <ul className="mb-2 max-h-48 overflow-y-auto rounded border border-slate-200 dark:border-slate-700">
                  {worktrees.map((w) => (
                    <li key={w.path}>
                      <button
                        type="button"
                        onClick={() => {
                          setWorktree({
                            path: w.path,
                            ...(w.branch !== undefined ? { branch: w.branch } : {}),
                            isMain: w.isMain,
                          });
                          // Move focus to the confirm button so Enter opens the worktree.
                          requestAnimationFrame(() => existingConfirmRef.current?.focus());
                        }}
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
          <form
            id="new-worktree-form"
            onSubmit={(e) => {
              e.preventDefault();
              void onCreateWorktree();
            }}
          >
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
                  git worktree add .arborist/.worktrees/{newName.trim() || 'NAME'} -b {newName.trim() || 'NAME'}
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
          </form>
        </div>

        {worktree && <p className="mt-2 truncate text-xs text-slate-500">Selected: {worktree.path}</p>}

        {worktree && (
          <p className="mt-2 truncate text-xs text-slate-500">
            Label will be: <span className="font-mono">{deriveLabel(worktree.path)}</span>
          </p>
        )}
      </div>

      {submitError && (
        <p role="alert" aria-live="polite" className="mb-2 rounded bg-red-100 px-2 py-1 text-xs text-red-800 dark:bg-red-900 dark:text-red-100">
          {submitError}
        </p>
      )}

      <div className="flex justify-end gap-2">
        <button
          type="button"
          onClick={onCancel}
          disabled={creating || submitting}
          className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-600 dark:bg-slate-700 dark:hover:bg-slate-600"
        >
          Cancel
        </button>
        {worktreeMode === 'new' && (
          <button
            type="submit"
            form="new-worktree-form"
            disabled={creating || submitting || newName.trim().length === 0 || newNameError !== null}
            className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 disabled:cursor-not-allowed disabled:bg-slate-400"
          >
            {creating ? 'Creating…' : 'Create & open'}
          </button>
        )}
        {worktreeMode === 'existing' && (
          <button
            ref={existingConfirmRef}
            type="button"
            onClick={() => {
              void onConfirmExisting();
            }}
            disabled={submitting || creating || !worktree}
            className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-700 disabled:cursor-not-allowed disabled:bg-slate-400"
          >
            {submitting ? 'Opening…' : 'Open worktree'}
          </button>
        )}
      </div>
    </dialog>
  );
}
