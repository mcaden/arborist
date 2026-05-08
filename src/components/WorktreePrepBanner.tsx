// WorktreePrepBanner — floating top-right overlay surfacing the lifecycle of
// `worktree_prep` runs (issue #63).
//
// Variants:
//   * Running — info-coloured pill with a spinner and "(N)" if multiple.
//   * Success — green pill, auto-dismisses after AUTO_DISMISS_MS.
//   * Failure — red pill, sticky; **View log** button calls
//     `worktreePrepOpenLog` to open the captured log in the OS-default
//     handler, and a **Dismiss** button removes it from the queue.
//
// State source: `useWorktreePrepStore`. The store is fed by the global
// `worktree://prep` listener wired up in `App.tsx` boot — the banner is a
// pure subscriber and owns no events of its own.
//
// Positioning: absolute top-right inside `MainArea`'s panel. We keep the
// terminal underneath full-bleed (it does not reflow when the banner
// appears) — this matches the dogfooding constraint that PTY dimensions
// must not change while a session is running.

import { useEffect } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { formatError, worktreePrepOpenLog } from '@/lib/tauri-bridge';
import { selectInFlightPreps, selectRecentCompletedPreps, useWorktreePrepStore, type PrepCompletedRecord } from '@/store/worktree-prep-store';

/** Successful preps fade away after this long. Failures stay until dismissed. */
const AUTO_DISMISS_MS = 5_000;

/** Return the leaf segment of a Windows-or-POSIX path. */
function leaf(p: string): string {
  // Strip trailing slashes, then take everything after the last separator.
  const trimmed = p.replace(/[\\/]+$/, '');
  const idx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return idx === -1 ? trimmed : trimmed.slice(idx + 1);
}

export function WorktreePrepBanner(): JSX.Element | null {
  // `selectInFlightPreps` derives an array via `Object.values`, so a
  // referential-equality compare would treat every render as a change and
  // trigger an infinite loop in React; `useShallow` does an element-wise
  // equality check on the array so we only re-render on actual state changes.
  const inFlight = useWorktreePrepStore(useShallow(selectInFlightPreps));
  const recent = useWorktreePrepStore(selectRecentCompletedPreps);
  const dismissCompleted = useWorktreePrepStore((s) => s.dismissCompleted);

  // Auto-dismiss successful completions. Failures must be acknowledged
  // explicitly so the user notices them — we never silently hide a
  // failed prep.
  useEffect(() => {
    const timers: ReturnType<typeof setTimeout>[] = [];
    for (const r of recent) {
      if (r.ok) {
        timers.push(setTimeout(() => dismissCompleted(r.prepId), AUTO_DISMISS_MS));
      }
    }
    return () => {
      for (const t of timers) clearTimeout(t);
    };
  }, [recent, dismissCompleted]);

  const failures = recent.filter((r) => !r.ok);
  const successes = recent.filter((r) => r.ok);

  if (inFlight.length === 0 && failures.length === 0 && successes.length === 0) {
    return null;
  }

  return (
    <div
      className="pointer-events-none absolute right-3 top-3 z-20 flex max-w-md flex-col items-end gap-2"
      role="region"
      aria-label="Worktree prep status"
      data-testid="worktree-prep-banner"
    >
      {inFlight.length > 0 && <RunningBanner count={inFlight.length} firstWorktree={inFlight[0]!.worktreePath} />}
      {failures.map((rec) => (
        <FailureBanner key={rec.prepId} record={rec} onDismiss={() => dismissCompleted(rec.prepId)} />
      ))}
      {successes.map((rec) => (
        <SuccessBanner key={rec.prepId} record={rec} onDismiss={() => dismissCompleted(rec.prepId)} />
      ))}
    </div>
  );
}

function RunningBanner({ count, firstWorktree }: { count: number; firstWorktree: string }): JSX.Element {
  const label = count === 1 ? `Worktree prep running for ${leaf(firstWorktree)}…` : `Worktree prep running… (${count})`;
  return (
    <div
      className="pointer-events-auto flex items-center gap-2 rounded-md border border-sky-300 bg-sky-50 px-3 py-2 text-xs text-sky-900 shadow-sm dark:border-sky-700 dark:bg-sky-950 dark:text-sky-100"
      data-testid="worktree-prep-banner-running"
      role="status"
    >
      <Spinner />
      <span>{label}</span>
    </div>
  );
}

function SuccessBanner({ record, onDismiss }: { record: PrepCompletedRecord; onDismiss: () => void }): JSX.Element {
  return (
    <div
      className="pointer-events-auto flex items-center gap-2 rounded-md border border-emerald-300 bg-emerald-50 px-3 py-2 text-xs text-emerald-900 shadow-sm dark:border-emerald-700 dark:bg-emerald-950 dark:text-emerald-100"
      data-testid="worktree-prep-banner-success"
      role="status"
    >
      <span aria-hidden="true">✓</span>
      <span>Prep complete for {leaf(record.worktreePath)}</span>
      <button
        type="button"
        onClick={onDismiss}
        className="ml-2 text-[11px] underline opacity-70 hover:opacity-100"
        aria-label="Dismiss prep notification"
      >
        Dismiss
      </button>
    </div>
  );
}

function FailureBanner({ record, onDismiss }: { record: PrepCompletedRecord; onDismiss: () => void }): JSX.Element {
  const reason = record.errorMessage ?? (record.exitCode === null ? 'process was signalled' : `exit code ${record.exitCode}`);
  return (
    <div
      className="pointer-events-auto flex flex-col gap-1 rounded-md border border-rose-300 bg-rose-50 px-3 py-2 text-xs text-rose-900 shadow-sm dark:border-rose-700 dark:bg-rose-950 dark:text-rose-100"
      data-testid="worktree-prep-banner-failure"
      role="alert"
    >
      <div className="flex items-center gap-2">
        <span aria-hidden="true">⚠</span>
        <span>
          Prep failed for {leaf(record.worktreePath)} ({reason})
        </span>
      </div>
      <div className="flex items-center justify-end gap-2">
        <button
          type="button"
          onClick={() => {
            void worktreePrepOpenLog({ logPath: record.logPath }).catch((err: unknown) => {
              // The most useful surface for an open-log failure is the
              // existing banner — re-use the failure copy by mutating the
              // record's errorMessage. We deliberately do not throw.
              // eslint-disable-next-line no-console
              console.warn('worktree_prep_open_log failed:', formatError(err));
            });
          }}
          className="rounded border border-rose-400 px-2 py-0.5 text-[11px] hover:bg-rose-100 dark:border-rose-600 dark:hover:bg-rose-900"
        >
          View log
        </button>
        <button type="button" onClick={onDismiss} className="text-[11px] underline opacity-80 hover:opacity-100" aria-label="Dismiss prep failure">
          Dismiss
        </button>
      </div>
    </div>
  );
}

function Spinner(): JSX.Element {
  return <span className="inline-block h-3 w-3 animate-spin rounded-full border-2 border-current border-t-transparent" aria-hidden="true" />;
}
