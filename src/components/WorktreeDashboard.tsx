// WorktreeDashboard — rendered in MainArea when an active worktree tab has no
// `activeChildId` (issues #44, #55). Shows a Git Status snapshot and an
// aggregate AI-usage summary for every session bound to this worktree, plus
// the header and launch buttons that were the original placeholder.
//
// Backend contract: `worktreeGitStatus(path)` always resolves. A successful
// snapshot leaves `result.error` undefined; on git discovery failures (path
// missing, not a repo, git binary unavailable, non-zero status exit) the
// backend populates `result.error` with a human-readable message and zeroes
// the counts. The panel shows that message inline so users can distinguish
// "clean tree" from "unable to read git status". A rejected `invoke()` (rare
// — e.g. capability denied) is treated the same way via the catch handler.
//
// The git panel polls every 15s while mounted; a manual "Refresh" button is
// also exposed. An in-flight guard ensures only one snapshot is dispatched at
// a time even if a poll tick arrives before the previous request resolves,
// so a slow `git status` on a large repo can't pile up concurrent `git`
// processes. Polling is intentionally simple — a notify-based file
// watcher is a follow-up (#93 widget refactor will host it).

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';

import { ToolIcon } from './ToolIcon';
import { measureInitialPtyDimensions } from '@/hooks/use-terminal';
import { formatError, worktreeGitStatus } from '@/lib/tauri-bridge';
import { useRegistry } from '@/plugins';
import { useConfigStore } from '@/store/config-store';
import { useSessionActions, useSessionStore, useSessions } from '@/store/session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { GitStatusFileKind, SessionStatus, Tool, WorktreeGitStatus, WorktreeTabId } from '@/types/arborist';

interface WorktreeDashboardProps {
  tabId: WorktreeTabId;
}

const POLL_INTERVAL_MS = 15_000;

// All four kinds the backend can produce, in the order we display badges.
const KINDS: GitStatusFileKind[] = ['staged', 'unstaged', 'untracked', 'conflicted'];

const KIND_LABELS: Record<GitStatusFileKind, string> = {
  staged: 'Staged',
  unstaged: 'Unstaged',
  untracked: 'Untracked',
  conflicted: 'Conflicted',
};

const STATUS_LABELS: Record<SessionStatus, string> = {
  starting: 'Starting',
  running: 'Running',
  exited: 'Exited',
  error: 'Error',
};

export function WorktreeDashboard({ tabId }: WorktreeDashboardProps): JSX.Element | null {
  const tab = useWorktreeTabStore((s) => s.tabs.find((t) => t.id === tabId));
  const registry = useRegistry();
  const allSessions = useSessions();
  const metrics = useSessionStore((s) => s.metrics);
  const aiIconDataUris = useConfigStore((s) => s.config.aiLaunchCommands.iconDataUris);
  const sessionActions = useSessionActions();
  const aiPlugins = useMemo(() => registry.ai(), [registry]);

  const tabPath = tab?.path;

  const sessionsForWorktree = useMemo(() => (tabPath ? allSessions.filter((s) => s.worktreePath === tabPath) : []), [allSessions, tabPath]);

  // Aggregate AI usage by summing cumulative inputTokens/outputTokens across
  // every session. `latestModel` picks the most recently-observed metrics
  // record's model so a single agent's name still surfaces; ties are broken
  // by observedAt order (last write wins).
  const usage = useMemo(() => {
    let inputTokens = 0;
    let outputTokens = 0;
    let latestObservedAt = -Infinity;
    let latestModel: string | undefined;
    const statusCounts: Partial<Record<SessionStatus, number>> = {};
    for (const s of sessionsForWorktree) {
      statusCounts[s.status] = (statusCounts[s.status] ?? 0) + 1;
      const m = metrics[s.id];
      if (!m) continue;
      inputTokens += m.inputTokens ?? 0;
      outputTokens += m.outputTokens ?? 0;
      if (m.model && m.observedAt > latestObservedAt) {
        latestObservedAt = m.observedAt;
        latestModel = m.model;
      }
    }
    return { inputTokens, outputTokens, latestModel, statusCounts };
  }, [sessionsForWorktree, metrics]);

  const [status, setStatus] = useState<WorktreeGitStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [statusLoading, setStatusLoading] = useState(false);
  const reqIdRef = useRef(0);
  // In-flight guard: prevents a poll tick from launching a second
  // `worktreeGitStatus` call while the previous one is still pending. Without
  // this, a slow `git status` on a large repo would queue overlapping requests
  // that each spawn a child `git` process — wasting work and (despite `reqId`
  // dropping their state writes) still racing each other in flight.
  const inFlightRef = useRef(false);

  const refreshStatus = useCallback(async () => {
    if (!tabPath) return;
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    const reqId = ++reqIdRef.current;
    setStatusLoading(true);
    try {
      const result = await worktreeGitStatus(tabPath);
      if (reqIdRef.current !== reqId) return;
      setStatus(result);
      setStatusError(null);
    } catch (err) {
      if (reqIdRef.current !== reqId) return;
      setStatusError(formatError(err));
    } finally {
      // Only clear the in-flight guard and loading flag if WE are still the
      // active request. A tab switch (or another call started by the
      // tabPath-change reset) bumps `reqIdRef.current`, in which case the
      // newer request now owns those flags and we must not stomp them.
      if (reqIdRef.current === reqId) {
        inFlightRef.current = false;
        setStatusLoading(false);
      }
    }
  }, [tabPath]);

  useEffect(() => {
    if (!tabPath) return;
    // Reset stale state from a previous worktree tab so the panel doesn't briefly
    // show the prior tab's status / error during the cross-tab transition. The
    // first refresh below repopulates immediately for the new tabPath. Crucially
    // we also clear `inFlightRef` and `statusLoading` so a slow request still
    // pending against the *previous* tabPath can't block the new tab's initial
    // fetch (the in-flight guard would otherwise short-circuit it and the
    // Refresh button would stay disabled until the prior call resolved).
    setStatus(null);
    setStatusError(null);
    inFlightRef.current = false;
    setStatusLoading(false);
    void refreshStatus();
    const handle = window.setInterval(() => {
      void refreshStatus();
    }, POLL_INTERVAL_MS);
    // Capture the ref so the cleanup function reads from the same closure.
    const refSnapshot = reqIdRef;
    return () => {
      window.clearInterval(handle);
      // Invalidate any in-flight request so a late resolve from the previous
      // path doesn't overwrite state for a newly-focused worktree tab.
      refSnapshot.current++;
    };
  }, [tabPath, refreshStatus]);

  if (!tab) {
    // Defensive — tab was closed underneath us.
    return null;
  }

  const launch = (tool: Tool): void => {
    const dims = measureInitialPtyDimensions();
    void sessionActions
      .create({
        tool,
        worktreePath: tab.path,
        cols: dims.cols,
        rows: dims.rows,
      })
      .catch((err: unknown) => {
        console.warn(`[WorktreeDashboard] sessionCreate(${tool}) failed: ${formatError(err)}`);
      });
  };

  const childCount = sessionsForWorktree.length;
  const counts: Record<GitStatusFileKind, number> = {
    staged: status?.staged ?? 0,
    unstaged: status?.unstaged ?? 0,
    untracked: status?.untracked ?? 0,
    conflicted: status?.conflicted ?? 0,
  };
  const totalChanges = counts.staged + counts.unstaged + counts.untracked + counts.conflicted;

  return (
    <section
      data-testid="worktree-dashboard"
      role="region"
      aria-labelledby="worktree-dashboard-title"
      className="flex h-full min-w-0 flex-1 flex-col gap-6 overflow-y-auto bg-white px-8 py-6 text-slate-700 dark:bg-slate-950 dark:text-slate-200"
    >
      <header className="flex flex-col gap-1">
        <h1 id="worktree-dashboard-title" className="text-lg font-semibold">
          {tab.name}
        </h1>
        <p className="font-mono text-xs text-slate-500 dark:text-slate-400">{tab.path}</p>
        {tab.branch && <p className="text-xs text-slate-500 dark:text-slate-400">on branch {tab.branch}</p>}
      </header>

      <div className="grid gap-4 md:grid-cols-2">
        {/* ---------- Git Status ---------- */}
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
                  </>
                )}
                {(status.ahead > 0 || status.behind > 0) && (
                  <>
                    <dt className="text-slate-500 dark:text-slate-400">Ahead / behind</dt>
                    <dd data-testid="worktree-dashboard-ahead-behind">
                      <span className="text-emerald-600 dark:text-emerald-400">↑{status.ahead}</span>
                      <span className="mx-1 text-slate-400">·</span>
                      <span className="text-amber-600 dark:text-amber-400">↓{status.behind}</span>
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

        {/* ---------- AI Usage ---------- */}
        <article
          data-testid="worktree-dashboard-ai-usage"
          className="flex flex-col gap-3 rounded-md border border-slate-200 bg-slate-50 p-4 dark:border-slate-800 dark:bg-slate-900"
        >
          <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400">AI Usage</h2>
          {childCount === 0 ? (
            <p className="text-xs text-slate-500 dark:text-slate-400">No agents yet — launch one below or right-click this tab.</p>
          ) : (
            <>
              <p className="text-xs text-slate-500 dark:text-slate-400">
                {childCount} agent{childCount === 1 ? '' : 's'} in this worktree.
              </p>
              <div className="flex flex-wrap gap-1.5" data-testid="worktree-dashboard-status-breakdown">
                {(Object.keys(STATUS_LABELS) as SessionStatus[]).map((st) => {
                  const n = usage.statusCounts[st] ?? 0;
                  if (n === 0) return null;
                  return (
                    <span
                      key={st}
                      data-testid={`worktree-dashboard-status-${st}`}
                      className="rounded-full bg-slate-200 px-2 py-0.5 text-[11px] font-medium text-slate-700 dark:bg-slate-800 dark:text-slate-200"
                    >
                      {STATUS_LABELS[st]}: {n}
                    </span>
                  );
                })}
              </div>
              <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
                <dt className="text-slate-500 dark:text-slate-400">Input tokens</dt>
                <dd data-testid="worktree-dashboard-input-tokens" className="tabular-nums">
                  {usage.inputTokens.toLocaleString()}
                </dd>
                <dt className="text-slate-500 dark:text-slate-400">Output tokens</dt>
                <dd data-testid="worktree-dashboard-output-tokens" className="tabular-nums">
                  {usage.outputTokens.toLocaleString()}
                </dd>
                {usage.latestModel && (
                  <>
                    <dt className="text-slate-500 dark:text-slate-400">Latest model</dt>
                    <dd className="font-mono">{usage.latestModel}</dd>
                  </>
                )}
              </dl>
            </>
          )}
        </article>
      </div>

      <div className="flex gap-3">
        {aiPlugins.map((plugin) => {
          const iconDataUri = aiIconDataUris[plugin.id];
          return (
            <button
              key={plugin.id}
              type="button"
              data-testid={`worktree-dashboard-launch-${plugin.id}`}
              onClick={() => launch(plugin.id as Tool)}
              className="inline-flex items-center gap-2 rounded-md border border-slate-300 bg-white px-4 py-2 text-sm font-medium text-slate-700 hover:bg-slate-50 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100 dark:hover:bg-slate-800"
            >
              <ToolIcon tool={plugin.id as Tool} className="h-4 w-4" {...(iconDataUri ? { iconDataUri } : {})} />
              <span>{`Launch ${plugin.displayName}`}</span>
            </button>
          );
        })}
      </div>
    </section>
  );
}
