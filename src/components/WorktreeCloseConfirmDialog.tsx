// Confirmation modal shown when the user closes a worktree parent tab
// (clicking ✕ on the SidebarWorktreeTab header or "Close worktree tab" in
// `WorktreeTabContextMenu`). Uses the native <dialog> element so we get a
// focus trap and Esc-to-close for free, no extra deps.
//
// This dialog replaces the per-AI-agent `CloseConfirmDialog` for the
// "delete worktree directory" affordance: worktrees are owned by the
// worktree parent tab, not by individual AI agent sessions, so the
// destructive deletion choice belongs here.

import { useEffect, useMemo, useRef, useState } from 'react';

import { formatError } from '@/lib/tauri-bridge';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import { usePendingWorktreeTabClose, useWorktreeTabActions, useWorktreeTabStore } from '@/store/worktree-tab-store';

export function WorktreeCloseConfirmDialog(): JSX.Element | null {
  const pendingId = usePendingWorktreeTabClose();
  const tab = useWorktreeTabStore((s) => (pendingId ? s.tabs.find((t) => t.id === pendingId) : undefined));
  const actions = useWorktreeTabActions();

  const sessionCount = useSessionStore((s) => (tab ? s.sessions.filter((sess) => sess.worktreePath === tab.path).length : 0));
  const subSessionCount = useSubSessionStore((s) => (tab ? s.subSessions.filter((sub) => sub.parentWorktreeTabId === tab.id).length : 0));

  const dialogRef = useRef<HTMLDialogElement | null>(null);
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const [deleteWorktree, setDeleteWorktree] = useState<boolean>(false);
  const [busy, setBusy] = useState<boolean>(false);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (pendingId !== undefined) {
      // Reset transient state every time the dialog opens — deletion
      // is destructive and should never be sticky across tabs.
      setDeleteWorktree(false);
      setBusy(false);
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
      cancelRef.current?.focus();
    } else if (dialog.open) {
      dialog.close();
    }
  }, [pendingId]);

  const childSummary = useMemo<string>(() => {
    const parts: string[] = [];
    if (sessionCount > 0) parts.push(`${sessionCount} AI ${sessionCount === 1 ? 'agent' : 'agents'}`);
    if (subSessionCount > 0) parts.push(`${subSessionCount} sub-${subSessionCount === 1 ? 'process' : 'processes'}`);
    return parts.join(' and ');
  }, [sessionCount, subSessionCount]);

  if (pendingId === undefined || !tab) return null;

  const onCancel = (): void => {
    if (busy) return;
    actions.cancelClose();
  };

  const onConfirm = async (): Promise<void> => {
    if (busy) return;
    setBusy(true);
    let alertMessage: string | null = null;
    try {
      const result = await actions.close(pendingId, deleteWorktree);
      if (result.worktreeDeleteError) {
        alertMessage = `Worktree tab closed, but deleting the worktree failed:\n\n${result.worktreeDeleteError}`;
      }
    } catch (error: unknown) {
      alertMessage = `Close request failed (the tab may already be gone):\n\n${formatError(error)}`;
    } finally {
      actions.cancelClose();
    }
    if (alertMessage !== null && typeof window !== 'undefined' && typeof window.alert === 'function') {
      window.alert(alertMessage);
    }
  };

  return (
    <dialog
      ref={dialogRef}
      aria-labelledby="worktree-close-confirm-title"
      aria-busy={busy}
      data-testid="worktree-close-confirm-dialog"
      onCancel={(e) => {
        // <dialog>'s native Esc dispatches `cancel`. Block it while busy;
        // otherwise route through our store action so state stays consistent.
        e.preventDefault();
        if (!busy) onCancel();
      }}
      className="rounded-md border border-slate-300 bg-white p-4 text-slate-900 shadow-lg backdrop:bg-black/40 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
    >
      <h2 id="worktree-close-confirm-title" className="mb-3 text-base font-semibold">
        Close worktree tab &ldquo;{tab.name}&rdquo;?
      </h2>
      {childSummary.length > 0 ? (
        <p className="mb-3 text-sm text-slate-600 dark:text-slate-300">
          This will terminate <span className="font-medium">{childSummary}</span> running under this worktree.
        </p>
      ) : null}
      <label className="mb-4 flex items-start gap-2 text-sm">
        <input
          type="checkbox"
          checked={deleteWorktree}
          disabled={busy}
          onChange={(e) => setDeleteWorktree(e.target.checked)}
          className="mt-0.5 h-4 w-4 cursor-pointer accent-red-600 disabled:cursor-not-allowed disabled:opacity-50"
        />
        <span className="flex flex-col">
          <span>
            Also delete the worktree directory
            {deleteWorktree ? <span className="ml-1 font-medium text-red-700 dark:text-red-400">(cannot be undone)</span> : null}
          </span>
          <span className="mt-0.5 break-all font-mono text-xs text-slate-500 dark:text-slate-400" title={tab.path}>
            {tab.path}
          </span>
        </span>
      </label>
      <div className="flex items-center justify-end gap-2">
        {busy ? (
          <svg aria-label="Closing…" role="status" className="mr-auto h-5 w-5 animate-spin text-slate-400" viewBox="0 0 24 24" fill="none">
            <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
            <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z" />
          </svg>
        ) : null}
        <button
          ref={cancelRef}
          type="button"
          disabled={busy}
          onClick={onCancel}
          className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm text-slate-700 hover:bg-slate-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 disabled:cursor-not-allowed disabled:opacity-50 dark:border-slate-600 dark:bg-slate-700 dark:text-slate-100 dark:hover:bg-slate-600"
        >
          Cancel
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={() => {
            void onConfirm();
          }}
          className="rounded-md bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-700 focus:outline-none focus-visible:ring-2 focus-visible:ring-red-400 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {deleteWorktree ? 'Close & delete worktree' : 'Close tab'}
        </button>
      </div>
    </dialog>
  );
}
