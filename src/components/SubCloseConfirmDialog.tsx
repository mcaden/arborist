// Confirmation modal shown when the user clicks the close (×) on a
// running application sub-tab. The user has three options:
//
//   * **Cancel** — leave both the tab and the external app alone.
//   * **Close tab only** — detach our tracking; the external window
//     keeps running. (The default for sub-tabs whose underlying
//     window we couldn't identify; bypassed entirely for terminal
//     sub-tabs since the tab IS the process.)
//   * **Close tab & app window** — detach AND politely ask the OS
//     to close the matched window (Windows: WM_CLOSE). The app may
//     show a save-changes prompt and decline; our tab is removed
//     regardless.
//
// Mirrors `CloseConfirmDialog`'s native-`<dialog>` pattern (focus
// trap, Esc-to-cancel) and jsdom-safe `showModal()` fallback.
//
// Mounted from the same place as `CloseConfirmDialog` (Sidebar) so a
// pending close stays open across viewport swaps.

import { useEffect, useRef } from 'react';

import {
  usePendingSubClose,
  useSubSessionActions,
  useSubSessionById,
} from '@/store/sub-session-store';

export function SubCloseConfirmDialog(): JSX.Element | null {
  const pendingId = usePendingSubClose();
  const sub = useSubSessionById(pendingId);
  const actions = useSubSessionActions();

  const dialogRef = useRef<HTMLDialogElement | null>(null);
  const cancelRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (pendingId !== undefined) {
      if (!dialog.open) {
        // jsdom in some versions ships a no-op showModal; fall back to
        // the `open` attribute so tests can still see the dialog mount.
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
      // Initial focus on the least destructive option.
      cancelRef.current?.focus();
    } else if (dialog.open) {
      dialog.close();
    }
  }, [pendingId]);

  if (pendingId === undefined || !sub) return null;

  const onCancel = (): void => {
    actions.cancelClose();
  };

  const closeWith = async (intent: 'tabOnly' | 'requestAppClose'): Promise<void> => {
    let alertMessage: string | null = null;
    try {
      await actions.close(pendingId, intent);
    } catch (error: unknown) {
      const detail =
        error instanceof Error && error.message.length > 0 ? error.message : String(error);
      alertMessage = `Close request failed:\n\n${detail}`;
    } finally {
      // close() auto-clears pendingClose on the success path; ensure
      // it's also cleared on rollback so the dialog disappears.
      actions.cancelClose();
    }
    if (
      alertMessage !== null &&
      typeof window !== 'undefined' &&
      typeof window.alert === 'function'
    ) {
      window.alert(alertMessage);
    }
  };

  return (
    <dialog
      ref={dialogRef}
      aria-labelledby="sub-close-confirm-title"
      onCancel={(e) => {
        e.preventDefault();
        onCancel();
      }}
      className="rounded-md border border-slate-300 bg-white p-4 text-slate-900 shadow-lg backdrop:bg-black/40 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
    >
      <h2 id="sub-close-confirm-title" className="mb-3 text-base font-semibold">
        Close sub-session &ldquo;{sub.label}&rdquo;?
      </h2>
      <p className="mb-4 max-w-md text-sm text-slate-600 dark:text-slate-300">
        You can close just the Arborist tab and leave the application running, or also ask the
        application to close its window.
      </p>
      <div className="flex flex-wrap justify-end gap-2">
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
            void closeWith('tabOnly');
          }}
          className="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm text-slate-700 hover:bg-slate-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-600 dark:bg-slate-700 dark:text-slate-100 dark:hover:bg-slate-600"
        >
          Close tab only
        </button>
        <button
          type="button"
          onClick={() => {
            void closeWith('requestAppClose');
          }}
          className="rounded-md bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-700 focus:outline-none focus-visible:ring-2 focus-visible:ring-red-400"
        >
          Close tab &amp; app window
        </button>
      </div>
    </dialog>
  );
}
