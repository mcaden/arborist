// Stub for the "create new session" affordance at the top of the sidebar.
// Phase 10 will wire this to the NewSessionDialog. Until then, clicking it
// logs a warning so the placeholder behaviour is obvious in dev.

interface NewSessionButtonProps {
  /** When set, the parent's keyboard nav considers this the focusable
   *  fallback (e.g. after closing the last session). */
  buttonRef?: React.Ref<HTMLButtonElement>;
}

export function NewSessionButton({ buttonRef }: NewSessionButtonProps): JSX.Element {
  return (
    <button
      ref={buttonRef}
      type="button"
      aria-label="New session"
      onClick={() => {
        console.warn('new-session dialog not implemented yet');
      }}
      className="mx-2 mb-2 mt-2 flex h-9 items-center justify-center rounded-md border border-slate-300 bg-white text-lg font-semibold text-slate-700 transition-colors hover:bg-slate-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
    >
      <span aria-hidden="true">+</span>
    </button>
  );
}
