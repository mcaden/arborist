// WorktreeDashboard — placeholder shown in MainArea when an active worktree tab has no `activeChildId` (issue #44). The real dashboard is a
// separate feature request per #44; this stub gives the user enough orientation (path, branch, child count) plus a discoverable launch path so
// the empty state isn't a dead end.

import { useMemo } from 'react';

import { ToolIcon } from './ToolIcon';
import { useSessionActions, useSessions } from '@/store/session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { Tool, WorktreeTabId } from '@/types/arborist';

interface WorktreeDashboardProps {
  tabId: WorktreeTabId;
}

export function WorktreeDashboard({ tabId }: WorktreeDashboardProps): JSX.Element | null {
  const tab = useWorktreeTabStore((s) => s.tabs.find((t) => t.id === tabId));
  const allSessions = useSessions();
  const sessionActions = useSessionActions();

  const childCount = useMemo(() => (tab ? allSessions.filter((s) => s.worktreePath === tab.path).length : 0), [allSessions, tab]);

  if (!tab) {
    // Defensive — tab was closed underneath us. Caller should have switched to a different visible-id branch already.
    return null;
  }

  const launch = (tool: Tool): void => {
    void sessionActions
      .create({
        tool,
        worktreePath: tab.path,
        cols: 80,
        rows: 24,
      })
      .catch((err: unknown) => {
        // Surface as a console warning rather than an unhandled rejection — the user has the launch buttons in front of them and a
        // toast/error UI is out of scope for the v1 dashboard placeholder. The error path is exercised in tests via mocked rejections.
        console.warn(`[WorktreeDashboard] sessionCreate(${tool}) failed: ${String(err)}`);
      });
  };

  return (
    <main
      data-testid="worktree-dashboard"
      role="region"
      aria-labelledby="worktree-dashboard-title"
      className="flex h-full min-w-0 flex-1 flex-col items-center justify-center gap-6 bg-white px-8 text-slate-700 dark:bg-slate-950 dark:text-slate-200"
    >
      <div className="flex max-w-xl flex-col items-center gap-2 text-center">
        <h1 id="worktree-dashboard-title" className="text-lg font-semibold">
          {tab.name}
        </h1>
        <p className="font-mono text-xs text-slate-500 dark:text-slate-400">{tab.path}</p>
        {tab.branch && <p className="text-xs text-slate-500 dark:text-slate-400">on branch {tab.branch}</p>}
        <p className="mt-2 text-sm text-slate-500 dark:text-slate-400">
          {childCount === 0
            ? 'No agents yet — launch one below or right-click this tab.'
            : `${childCount} agent${childCount === 1 ? '' : 's'} in this worktree.`}
        </p>
      </div>
      <div className="flex gap-3">
        <button
          type="button"
          data-testid="worktree-dashboard-launch-claude"
          onClick={() => launch('claude')}
          className="inline-flex items-center gap-2 rounded-md border border-slate-300 bg-white px-4 py-2 text-sm font-medium text-slate-700 hover:bg-slate-50 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100 dark:hover:bg-slate-800"
        >
          <ToolIcon tool="claude" className="h-4 w-4" />
          Launch Claude
        </button>
        <button
          type="button"
          data-testid="worktree-dashboard-launch-copilot"
          onClick={() => launch('copilot')}
          className="inline-flex items-center gap-2 rounded-md border border-slate-300 bg-white px-4 py-2 text-sm font-medium text-slate-700 hover:bg-slate-50 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100 dark:hover:bg-slate-800"
        >
          <ToolIcon tool="copilot" className="h-4 w-4" />
          Launch Copilot
        </button>
      </div>
    </main>
  );
}
