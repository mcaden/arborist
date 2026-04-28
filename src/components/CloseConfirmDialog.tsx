// Confirmation modal shown when the user clicks a tab's close button or
// presses `Delete` on a focused tab. Uses the native <dialog> element so
// we get a focus trap and Esc-to-close for free, no extra deps.

import { useEffect, useRef } from 'react';

import { usePendingClose, useSessionActions, useSessionById } from '@/store/session-store';

export function CloseConfirmDialog(): JSX.Element | null {
  const pendingId = usePendingClose();
  const session = useSessionById(pendingId);
  const actions = useSessionActions();

  const dialogRef = useRef<HTMLDialogElement | null>(null);
  const cancelRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (pendingId !== undefined) {
      if (!dialog.open) {
        // Some test environments (jsdom) don't ship a working showModal.
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
      // Initial focus on the less destructive option.
      cancelRef.current?.focus();
    } else if (dialog.open) {
      dialog.close();
    }
  }, [pendingId]);

  if (pendingId === undefined || !session) return null;

  const onCancel = (): void => {
    actions.cancelClose();
  };

  const onConfirm = async (): Promise<void> => {
    try {
      await actions.close(pendingId);
    } finally {
      actions.cancelClose();
    }
  };

  return (
    <dialog
      ref={dialogRef}
      aria-labelledby="close-confirm-title"
      onCancel={(e) => {
        // <dialog>'s native Esc dispatches `cancel`. Route it through
        // our store action so state stays consistent.
        e.preventDefault();
        onCancel();
      }}
      className="rounded-md border border-slate-300 bg-white p-4 text-slate-900 shadow-lg backdrop:bg-black/40 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
    >
      <h2 id="close-confirm-title" className="mb-3 text-base font-semibold">
        Terminate session &ldquo;{session.label}&rdquo;?
      </h2>
      <div className="flex justify-end gap-2">
        <button
          ref={cancelRef}
          type="button"
          onClick={onCancel}
          className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm text-slate-700 hover:bg-slate-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-600 dark:bg-slate-700 dark:text-slate-100 dark:hover:bg-slate-600"
        >
          Cancel
        </button>
        <button
          type="button"
          onClick={() => {
            void onConfirm();
          }}
          className="rounded-md bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-700 focus:outline-none focus-visible:ring-2 focus-visible:ring-red-400"
        >
          Terminate
        </button>
      </div>
    </dialog>
  );
}
