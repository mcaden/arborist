import { useMemo } from 'react';

import type { DashboardWidgetProps } from '@/plugins/registry';
import { useSessionStore, useSessions } from '@/store/session-store';
import type { SessionStatus } from '@/types/arborist';

const STATUS_LABELS: Record<SessionStatus, string> = {
  starting: 'Starting',
  running: 'Running',
  exited: 'Exited',
  error: 'Error',
};

export function AiUsageWidget({ tabPath }: DashboardWidgetProps): JSX.Element {
  const allSessions = useSessions();
  const metrics = useSessionStore((s) => s.metrics);

  const sessionsForWorktree = useMemo(() => allSessions.filter((s) => s.worktreePath === tabPath), [allSessions, tabPath]);
  const usage = useMemo(() => {
    let inputTokens = 0;
    let outputTokens = 0;
    let latestObservedAt = -Infinity;
    let latestModel: string | undefined;
    const statusCounts: Partial<Record<SessionStatus, number>> = {};
    for (const session of sessionsForWorktree) {
      statusCounts[session.status] = (statusCounts[session.status] ?? 0) + 1;
      const metricsForSession = metrics[session.id];
      if (!metricsForSession) continue;
      inputTokens += metricsForSession.inputTokens ?? 0;
      outputTokens += metricsForSession.outputTokens ?? 0;
      if (metricsForSession.model && metricsForSession.observedAt > latestObservedAt) {
        latestObservedAt = metricsForSession.observedAt;
        latestModel = metricsForSession.model;
      }
    }
    return { inputTokens, outputTokens, latestModel, statusCounts };
  }, [sessionsForWorktree, metrics]);

  const childCount = sessionsForWorktree.length;

  return (
    <article
      data-testid="worktree-dashboard-ai-usage"
      className="flex flex-col gap-3 rounded-md border border-slate-200 bg-slate-50 p-4 dark:border-slate-800 dark:bg-slate-900"
    >
      <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400">AI Usage</h2>
      {childCount === 0 ? (
        <p className="text-xs text-slate-500 dark:text-slate-400">No agents yet — use the Launch buttons above to start one.</p>
      ) : (
        <>
          <p className="text-xs text-slate-500 dark:text-slate-400">
            {childCount} agent{childCount === 1 ? '' : 's'} in this worktree.
          </p>
          <div className="flex flex-wrap gap-1.5" data-testid="worktree-dashboard-status-breakdown">
            {(Object.keys(STATUS_LABELS) as SessionStatus[]).map((status) => {
              const n = usage.statusCounts[status] ?? 0;
              if (n === 0) return null;
              return (
                <span
                  key={status}
                  data-testid={`worktree-dashboard-status-${status}`}
                  className="rounded-full bg-slate-200 px-2 py-0.5 text-[11px] font-medium text-slate-700 dark:bg-slate-800 dark:text-slate-200"
                >
                  {STATUS_LABELS[status]}: {n}
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
  );
}
