// SidebarSubTab — indented row beneath a parent SidebarTab representing a
// single sub-session (terminal or application). Sub-tabs are deliberately
// simpler than parent tabs: no drag-reorder, no metrics line, and a single
// status dot. Click forwards to `subSessionStore.focus`, which:
//   * for terminal sub-sessions, swaps the MainArea viewport to this sub
//     and brings the parent into view;
//   * for application sub-sessions, raises the OS window without touching
//     the viewport (the parent terminal stays visible).

import { useSessionActions } from '@/store/session-store';
import {
  useActiveSubSessionId,
  useSubSessionActions,
  useSubSessionById,
} from '@/store/sub-session-store';
import type { SessionId, SubSessionId, SubSessionStatus } from '@/types/arborist';

interface SidebarSubTabProps {
  parentId: SessionId;
  subSessionId: SubSessionId;
  /** True when the parent tab is the active session in the parent layer. */
  parentIsActive: boolean;
}

export function SidebarSubTab({
  parentId,
  subSessionId,
  parentIsActive,
}: SidebarSubTabProps): JSX.Element | null {
  const sub = useSubSessionById(subSessionId);
  const activeSubId = useActiveSubSessionId(parentId);
  const subActions = useSubSessionActions();
  const sessionActions = useSessionActions();

  if (!sub) return null;

  // A terminal sub-tab is "selected" when it owns the viewport for its
  // parent AND its parent is the active session. Application sub-tabs
  // never get the viewport, so they're never visually selected by the
  // viewport-swap rule (we just dim the row).
  const isViewportOwner = sub.kind === 'terminal' && activeSubId === subSessionId && parentIsActive;

  const handleClick = (): void => {
    // Phase 7: clicking a greyed-out application sub-tab triggers a
    // relaunch (re-spawn under the same id). Status flows back via
    // `subsession://status`; the row visually transitions starting →
    // running. Per-id dedupe in the store action prevents double-clicks
    // from spawning two processes.
    if (sub.kind === 'application' && (sub.status === 'exited' || sub.status === 'error')) {
      void subActions.relaunch(subSessionId);
      return;
    }
    // For terminal sub-sessions, also bring the parent into view if the
    // user clicked from another parent (otherwise activeByParent[parent]
    // is set but `activeId` still points elsewhere — the viewport
    // wouldn't update). Done here rather than inside the store action
    // to avoid creating a cross-store import dependency cycle.
    if (sub.kind === 'terminal' && !parentIsActive) {
      void sessionActions.focus(parentId);
    }
    void subActions.focus(subSessionId);
  };

  const handleClose = (e: React.MouseEvent): void => {
    e.stopPropagation();
    void subActions.close(subSessionId);
  };

  const stateClasses = isViewportOwner
    ? 'bg-sky-100 text-sky-900 dark:bg-sky-900/40 dark:text-sky-100'
    : 'text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-slate-800';

  return (
    <li className="group relative px-2">
      <button
        type="button"
        role="tab"
        aria-selected={isViewportOwner}
        onClick={handleClick}
        className={`flex w-full items-center gap-2 rounded-md py-1 pl-7 pr-7 text-left text-xs transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 ${stateClasses}`}
      >
        <span aria-hidden="true" className="text-xs text-slate-400">
          {sub.kind === 'application' ? '🪟' : '⌗'}
        </span>
        <span className="min-w-0 flex-1 truncate">{sub.label}</span>
        <SubStatusDot status={sub.status} />
      </button>
      <button
        type="button"
        aria-label={`Close sub-session ${sub.label}`}
        onClick={handleClose}
        className="absolute right-3 top-1 rounded p-0.5 text-slate-400 opacity-0 transition-opacity hover:bg-slate-200 hover:text-slate-900 focus:opacity-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 group-hover:opacity-100 dark:text-slate-500 dark:hover:bg-slate-700 dark:hover:text-slate-100"
      >
        <span aria-hidden="true">×</span>
      </button>
    </li>
  );
}

function SubStatusDot({ status }: { status: SubSessionStatus }): JSX.Element {
  const colour = (() => {
    switch (status) {
      case 'starting':
        return 'bg-sky-400 animate-pulse';
      case 'running':
        return 'bg-emerald-500';
      case 'exited':
        return 'bg-slate-400';
      case 'error':
        return 'bg-red-500';
    }
  })();
  return (
    <span
      aria-hidden="true"
      data-testid={`sub-status-${status}`}
      className={`h-2 w-2 shrink-0 rounded-full ${colour}`}
    />
  );
}
