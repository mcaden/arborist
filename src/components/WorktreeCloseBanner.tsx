// WorktreeCloseBanner — top-right overlay surfacing the lifecycle of a
// worktree-tab close (round-3 fix for PR #221). Sibling of
// `WorktreePrepBanner` and mounted in the same container.
//
// Variants:
//   * Running — neutral pill with a spinner: "Closing <name>…" and the
//     "(N)" counter when multiple closes are in flight.
//   * Success — green pill, auto-dismisses after AUTO_DISMISS_MS.
//   * Attention — amber pill, sticky. Used when the backend returned but
//     surfaced partial problems (delete refused for live apps, sub-session
//     kill unconfirmed). Carries the message verbatim.
//   * Failure — red pill, sticky. Used when the IPC call itself rejected
//     or the backend returned a `worktreeDeleteError`.
//
// Why a separate banner instead of reusing the prep one: the lifecycle
// payloads are different shapes (no log file to open, no exit code to
// report), and one banner per concern keeps each component small. The
// outer container in `MainArea` stacks both vertically.

import { useEffect } from 'react';
import { useShallow } from 'zustand/react/shallow';

import {
  selectInFlightCloses,
  selectRecentCompletedCloses,
  useWorktreeCloseStore,
  type CloseCompletedRecord,
  type CloseRunningRecord,
} from '@/store/worktree-close-store';

const AUTO_DISMISS_MS = 5_000;

function leaf(p: string): string {
  const trimmed = p.replace(/[\\/]+$/, '');
  const idx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return idx === -1 ? trimmed : trimmed.slice(idx + 1);
}

export function WorktreeCloseBanner(): JSX.Element | null {
  const inFlight = useWorktreeCloseStore(useShallow(selectInFlightCloses));
  const recent = useWorktreeCloseStore(selectRecentCompletedCloses);
  const dismissCompleted = useWorktreeCloseStore((s) => s.dismissCompleted);

  useEffect(() => {
    const timers: ReturnType<typeof setTimeout>[] = [];
    const nowMs = Date.now();
    for (const r of recent) {
      if (r.status === 'success') {
        const elapsedMs = Math.max(0, nowMs - r.finishedAt);
        const delayMs = Math.max(0, AUTO_DISMISS_MS - elapsedMs);
        timers.push(setTimeout(() => dismissCompleted(r.tabId), delayMs));
      }
    }
    return () => {
      for (const t of timers) clearTimeout(t);
    };
  }, [recent, dismissCompleted]);

  const failures = recent.filter((r) => r.status === 'failure');
  const attentions = recent.filter((r) => r.status === 'attention');
  const successes = recent.filter((r) => r.status === 'success');

  if (inFlight.length === 0 && failures.length === 0 && attentions.length === 0 && successes.length === 0) {
    return null;
  }

  return (
    <div
      className="pointer-events-none absolute right-3 top-3 z-20 flex max-w-md flex-col items-end gap-2"
      role="region"
      aria-label="Worktree close status"
      data-testid="worktree-close-banner"
    >
      {inFlight.length > 0 && <RunningBanner records={inFlight} />}
      {failures.map((rec) => (
        <ResultBanner key={rec.tabId} record={rec} variant="failure" onDismiss={() => dismissCompleted(rec.tabId)} />
      ))}
      {attentions.map((rec) => (
        <ResultBanner key={rec.tabId} record={rec} variant="attention" onDismiss={() => dismissCompleted(rec.tabId)} />
      ))}
      {successes.map((rec) => (
        <ResultBanner key={rec.tabId} record={rec} variant="success" onDismiss={() => dismissCompleted(rec.tabId)} />
      ))}
    </div>
  );
}

function RunningBanner({ records }: { records: readonly CloseRunningRecord[] }): JSX.Element {
  const first = records[0]!;
  const action = first.willDelete ? 'Closing and deleting' : 'Closing';
  const label =
    records.length === 1 ? `${action} ${leaf(first.worktreePath)}…` : `${action} ${leaf(first.worktreePath)} (+${records.length - 1} more)…`;
  return (
    <div
      className="pointer-events-auto flex items-center gap-2 rounded-md border border-sky-300 bg-sky-50 px-3 py-2 text-xs text-sky-900 shadow-sm dark:border-sky-700 dark:bg-sky-950 dark:text-sky-100"
      data-testid="worktree-close-banner-running"
      role="status"
    >
      <Spinner />
      <span>{label}</span>
    </div>
  );
}

function ResultBanner({
  record,
  variant,
  onDismiss,
}: {
  record: CloseCompletedRecord;
  variant: 'success' | 'attention' | 'failure';
  onDismiss: () => void;
}): JSX.Element {
  const palette =
    variant === 'success'
      ? 'border-emerald-300 bg-emerald-50 text-emerald-900 dark:border-emerald-700 dark:bg-emerald-950 dark:text-emerald-100'
      : variant === 'attention'
        ? 'border-amber-300 bg-amber-50 text-amber-900 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-100'
        : 'border-rose-300 bg-rose-50 text-rose-900 dark:border-rose-700 dark:bg-rose-950 dark:text-rose-100';
  const icon = variant === 'success' ? '✓' : variant === 'attention' ? '!' : '⚠';
  const title =
    variant === 'success'
      ? `Closed ${leaf(record.worktreePath)}${record.willDelete ? ' (and deleted)' : ''}`
      : variant === 'attention'
        ? `Closed ${leaf(record.worktreePath)} with warnings`
        : `Close failed for ${leaf(record.worktreePath)}`;
  return (
    <div
      className={`pointer-events-auto flex flex-col gap-1 rounded-md border px-3 py-2 text-xs shadow-sm ${palette}`}
      data-testid={`worktree-close-banner-${variant}`}
      role={variant === 'failure' ? 'alert' : 'status'}
    >
      <div className="flex items-center gap-2">
        <span aria-hidden="true">{icon}</span>
        <span>{title}</span>
      </div>
      {record.message !== '' && <div className="whitespace-pre-wrap break-words text-[11px] opacity-90">{record.message}</div>}
      <div className="flex items-center justify-end">
        <button
          type="button"
          onClick={onDismiss}
          className="text-[11px] underline opacity-80 hover:opacity-100"
          aria-label="Dismiss close notification"
        >
          Dismiss
        </button>
      </div>
    </div>
  );
}

function Spinner(): JSX.Element {
  return <span className="inline-block h-3 w-3 animate-spin rounded-full border-2 border-current border-t-transparent" aria-hidden="true" />;
}
