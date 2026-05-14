import type { DashboardWidgetProps } from '@/plugins/registry';
import type { GitStatusFileKind } from '@/types/arborist';

import { useGitStatus } from './use-git-status';

const KINDS: GitStatusFileKind[] = ['staged', 'unstaged', 'untracked', 'conflicted'];

const KIND_LABELS: Record<GitStatusFileKind, string> = {
  staged: 'Staged',
  unstaged: 'Unstaged',
  untracked: 'Untracked',
  conflicted: 'Conflicted',
};

export function GitStatusWidget({ tabPath }: DashboardWidgetProps): JSX.Element {
  const { status, statusError, statusLoading, refreshStatus } = useGitStatus(tabPath);
  const counts: Record<GitStatusFileKind, number> = {
    staged: status?.staged ?? 0,
    unstaged: status?.unstaged ?? 0,
    untracked: status?.untracked ?? 0,
    conflicted: status?.conflicted ?? 0,
  };
  const totalChanges = counts.staged + counts.unstaged + counts.untracked + counts.conflicted;

  return (
    <article
      data-testid="worktree-dashboard-git-status"
      className="flex flex-col gap-3 rounded-md border border-slate-200 bg-slate-50 p-4 dark:border-slate-800 dark:bg-slate-900"
    >
      <div className="flex items-center justify-between gap-2">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400">Git Status</h2>
        <button
          type="button"
          data-testid="worktree-dashboard-git-refresh"
          onClick={() => {
            void refreshStatus();
          }}
          disabled={statusLoading}
          className="rounded-md border border-slate-300 bg-white px-2 py-1 text-xs font-medium text-slate-700 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-700 dark:bg-slate-950 dark:text-slate-200 dark:hover:bg-slate-800"
        >
          {statusLoading ? 'Refreshing…' : 'Refresh'}
        </button>
      </div>

      {statusError ? (
        <p data-testid="worktree-dashboard-git-error" className="text-xs text-red-600 dark:text-red-400">
          Unable to read git status: {statusError}
        </p>
      ) : !status ? (
        <p className="text-xs text-slate-500 dark:text-slate-400">Loading…</p>
      ) : status.error ? (
        <p data-testid="worktree-dashboard-git-error" className="text-xs text-red-600 dark:text-red-400">
          Unable to read git status: {status.error}
        </p>
      ) : (
        <>
          <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
            <dt className="text-slate-500 dark:text-slate-400">Branch</dt>
            <dd className="font-mono">{status.branch ?? '(detached)'}</dd>
            {status.upstream && (
              <>
                <dt className="text-slate-500 dark:text-slate-400">Upstream</dt>
                <dd className="font-mono">{status.upstream}</dd>
                <dt className="text-slate-500 dark:text-slate-400">Ahead / behind</dt>
                <dd data-testid="worktree-dashboard-ahead-behind">
                  {status.ahead === 0 && status.behind === 0 ? (
                    <span className="text-slate-500 dark:text-slate-400">In sync</span>
                  ) : (
                    <>
                      <span className="text-emerald-600 dark:text-emerald-400">↑{status.ahead}</span>
                      <span className="mx-1 text-slate-400">·</span>
                      <span className="text-amber-600 dark:text-amber-400">↓{status.behind}</span>
                    </>
                  )}
                </dd>
              </>
            )}
            {status.sourceBranch && (
              <>
                <dt className="text-slate-500 dark:text-slate-400">Source</dt>
                <dd className="font-mono">{status.sourceBranch}</dd>
                <dt className="text-slate-500 dark:text-slate-400">Divergence</dt>
                <dd data-testid="worktree-dashboard-source-divergence">
                  {status.sourceAhead === 0 && status.sourceBehind === 0 ? (
                    <span className="text-slate-500 dark:text-slate-400">In sync</span>
                  ) : (
                    <>
                      <span className="text-emerald-600 dark:text-emerald-400">↑{status.sourceAhead}</span>
                      <span className="mx-1 text-slate-400">·</span>
                      <span className="text-amber-600 dark:text-amber-400">↓{status.sourceBehind}</span>
                    </>
                  )}
                </dd>
              </>
            )}
          </dl>

          <div className="grid grid-cols-4 gap-2" data-testid="worktree-dashboard-git-counts">
            {KINDS.map((kind) => (
              <div
                key={kind}
                className="flex flex-col items-center rounded-md border border-slate-200 bg-white px-2 py-1 dark:border-slate-700 dark:bg-slate-950"
                data-testid={`worktree-dashboard-count-${kind}`}
              >
                <span className="text-base font-semibold tabular-nums">{counts[kind]}</span>
                <span className="text-[10px] uppercase tracking-wide text-slate-500 dark:text-slate-400">{KIND_LABELS[kind]}</span>
              </div>
            ))}
          </div>

          {totalChanges === 0 ? (
            <p className="text-xs text-slate-500 dark:text-slate-400">Working tree clean.</p>
          ) : (
            <ul
              data-testid="worktree-dashboard-git-files"
              className="max-h-40 overflow-y-auto rounded-md border border-slate-200 bg-white p-2 font-mono text-[11px] dark:border-slate-700 dark:bg-slate-950"
            >
              {status.files.map((f) => (
                <li key={`${f.path}-${f.kind}`} className="flex items-center gap-2 truncate">
                  <span className="w-16 shrink-0 text-slate-500 dark:text-slate-400">{KIND_LABELS[f.kind]}</span>
                  <span className="truncate">{f.path}</span>
                </li>
              ))}
              {status.filesTruncated && <li className="mt-1 text-slate-500 dark:text-slate-400">…and more (truncated).</li>}
            </ul>
          )}
        </>
      )}
    </article>
  );
}
