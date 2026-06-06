import type { DashboardWidgetProps } from '@/plugins/registry';
import { openExternalUrl } from '@/lib/tauri-bridge';
import type { GitStatusFileKind, PrChecksStatus, PrState, WorktreePrInfo } from '@/types/arborist';

import { useGitStatus } from './use-git-status';
import { usePrInfo } from './use-pr-info';

const KINDS: GitStatusFileKind[] = ['staged', 'unstaged', 'untracked', 'conflicted'];

const KIND_LABELS: Record<GitStatusFileKind, string> = {
  staged: 'Staged',
  unstaged: 'Unstaged',
  untracked: 'Untracked',
  conflicted: 'Conflicted',
};

const PR_STATE_LABELS: Record<PrState, string> = {
  open: 'Open',
  draft: 'Draft',
  merged: 'Merged',
  closed: 'Closed',
  unknown: 'Unknown',
};

const PR_STATE_CLASSES: Record<PrState, string> = {
  open: 'bg-emerald-100 text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300',
  draft: 'bg-slate-200 text-slate-700 dark:bg-slate-700 dark:text-slate-200',
  merged: 'bg-purple-100 text-purple-700 dark:bg-purple-900/40 dark:text-purple-300',
  closed: 'bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300',
  unknown: 'bg-slate-200 text-slate-600 dark:bg-slate-700 dark:text-slate-300',
};

const PR_CHECKS_META: Record<PrChecksStatus, { label: string; glyph: string; className: string } | null> = {
  passing: { label: 'Checks passing', glyph: '✓', className: 'text-emerald-600 dark:text-emerald-400' },
  failing: { label: 'Checks failing', glyph: '✗', className: 'text-red-600 dark:text-red-400' },
  pending: { label: 'Checks pending', glyph: '•', className: 'text-amber-600 dark:text-amber-400' },
  none: null,
  unknown: null,
};

function PullRequestSection({
  prInfo,
  prError,
  prLoading,
}: Readonly<{
  prInfo: WorktreePrInfo | null;
  prError: string | null;
  prLoading: boolean;
}>): JSX.Element | null {
  // `prError` is a hook-level rejection; `prInfo.error` is the backend's structured always-Ok failure (e.g. invalid worktree path, which also
  // defaults `provider` to `unknown`). Surface both the same way so a structured error is never hidden by the unrecognised-host short-circuit below.
  const failure = prError ?? prInfo?.error ?? null;
  if (failure) {
    return (
      <p data-testid="worktree-dashboard-pr-error" className="text-xs text-red-600 dark:text-red-400">
        Unable to read pull request: {failure}
      </p>
    );
  }
  if (!prInfo) {
    return prLoading ? <p className="text-xs text-slate-500 dark:text-slate-400">Loading pull request…</p> : null;
  }
  // Nothing useful to show for unrecognised hosts (no error, no PR) — keep the widget uncluttered.
  if (prInfo.provider === 'unknown' && !prInfo.pr) {
    return null;
  }

  const checksMeta = prInfo.pr ? PR_CHECKS_META[prInfo.pr.checks] : null;

  return (
    <div
      data-testid="worktree-dashboard-pr"
      className="flex flex-col gap-1 rounded-md border border-slate-200 bg-white p-2 text-xs dark:border-slate-700 dark:bg-slate-950"
    >
      <span className="text-[10px] font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400">Pull Request</span>
      {prInfo.pr ? (
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-2">
            <button
              type="button"
              data-testid="worktree-dashboard-pr-link"
              onClick={() => {
                void openExternalUrl(prInfo.pr!.url);
              }}
              className="cursor-pointer font-mono font-semibold text-sky-600 hover:underline dark:text-sky-400"
            >
              #{prInfo.pr.number}
            </button>
            <span
              data-testid="worktree-dashboard-pr-state"
              className={`rounded px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide ${PR_STATE_CLASSES[prInfo.pr.state]}`}
            >
              {PR_STATE_LABELS[prInfo.pr.state]}
            </span>
            {checksMeta && (
              <span data-testid="worktree-dashboard-pr-checks" title={checksMeta.label} className={`text-xs font-semibold ${checksMeta.className}`}>
                {checksMeta.glyph} {checksMeta.label}
              </span>
            )}
          </div>
          {prInfo.pr.title && <span className="truncate text-slate-600 dark:text-slate-300">{prInfo.pr.title}</span>}
        </div>
      ) : (
        <div className="flex flex-col gap-1">
          {prInfo.note && (
            <span data-testid="worktree-dashboard-pr-note" className="text-slate-500 dark:text-slate-400">
              {prInfo.note}
            </span>
          )}
          {prInfo.repoWebUrl && (
            <button
              type="button"
              data-testid="worktree-dashboard-pr-repo-link"
              onClick={() => {
                void openExternalUrl(prInfo.repoWebUrl!);
              }}
              className="cursor-pointer self-start font-mono text-sky-600 hover:underline dark:text-sky-400"
            >
              Open repository
            </button>
          )}
        </div>
      )}
    </div>
  );
}

export function GitStatusWidget({ tabPath }: DashboardWidgetProps): JSX.Element {
  const { status, statusError, statusLoading, refreshStatus } = useGitStatus(tabPath);
  const { prInfo, prError, prLoading, refreshPrInfo } = usePrInfo(tabPath);
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
            void refreshPrInfo();
          }}
          disabled={statusLoading}
          className="rounded-md border border-slate-300 bg-white px-2 py-1 text-xs font-medium text-slate-700 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-700 dark:bg-slate-950 dark:text-slate-200 dark:hover:bg-slate-800"
        >
          {statusLoading ? 'Refreshing…' : 'Refresh'}
        </button>
      </div>

      <PullRequestSection prInfo={prInfo} prError={prError} prLoading={prLoading} />

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
            {status.sourceBranch && status.sourceAhead != null && status.sourceBehind != null && (
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
              className="themed-scrollbar max-h-40 overflow-y-auto rounded-md border border-slate-200 bg-white p-2 font-mono text-[11px] dark:border-slate-700 dark:bg-slate-950"
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
