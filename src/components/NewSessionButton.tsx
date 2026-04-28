// Stub for the "create new session" affordance at the top of the sidebar.
// Phase 10 wires the click to opening the NewSessionDialog via a
// dedicated dialog store so the button stays a leaf component.

import { useNewSessionDialog } from '@/store/new-session-dialog-store';

interface NewSessionButtonProps {
  /** When set, the parent's keyboard nav considers this the focusable
   *  fallback (e.g. after closing the last session). */
  buttonRef?: React.Ref<HTMLButtonElement>;
}

export function NewSessionButton({ buttonRef }: NewSessionButtonProps): JSX.Element {
  const open = useNewSessionDialog((s) => s.open);
  return (
    <button
      ref={buttonRef}
      type="button"
      aria-label="New session"
      onClick={() => {
        open();
      }}
      className="mx-2 mb-2 mt-2 flex h-9 items-center justify-center rounded-md border border-slate-300 bg-white text-lg font-semibold text-slate-700 transition-colors hover:bg-slate-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
    >
      <span aria-hidden="true">+</span>
    </button>
  );
}
