// SidebarSubTab — child row under a worktree tab representing a single
// sub-session (terminal or application). It is visually aligned with
// AI-session child tabs, but stays simpler: no drag-reorder, no metrics line,
// and a single status dot. Click forwards to:
//   * `subSessionStore.relaunch` when an *application* sub-tab is greyed
//     (status `exited` or `error`) — the user clicked a launcher chrome
//     for a process that died and should re-spawn under the same id;
//   * `subSessionStore.focus` (which navigates the sub-tab pane into
//     view) when a *terminal* sub-tab is greyed — the user gets to see
//     the relaunch / close pane rendered by `SubTerminalView` instead
//     of an automatic reset of their scrollback;
//   * otherwise `subSessionStore.focus`, which:
//     * for terminal sub-sessions, swaps the MainArea viewport to this
//       sub and brings the owning worktree tab into view;
//     * for application sub-sessions, raises the OS window without
//       touching the viewport (the parent terminal stays visible).
//
// Close (×) handler:
//   * Terminal kind — immediate close (the tab IS the process).
//   * Application kind, currently running — opens
//     `SubCloseConfirmDialog` so the user can decide whether to keep
//     the underlying window open.
//   * Application kind, already exited — immediate close (no window
//     to address).
//
// A vertical-ellipsis (⋮) button next to the close × opens
// `SubTabContextMenu` (Restart + Close), mirroring the AI session-tab
// affordance added in issue #49.
//
// Accessibility: the row is a plain `<button>` (implicit `role="button"`),
// not `role="tab"`, so it stays out of the sidebar's roving-tabindex model.

import { useRef, useState } from 'react';

import { SubTabContextMenu } from './SubTabContextMenu';
import { useSubSessionActions, useSubSessionById } from '@/store/sub-session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import { useSubSessionIcon } from '@/hooks/use-sub-session-icon';
import type { SubSessionId, SubSessionStatus } from '@/types/arborist';

interface SidebarSubTabProps {
  subSessionId: SubSessionId;
}

export function SidebarSubTab({ subSessionId }: SidebarSubTabProps): JSX.Element | null {
  const sub = useSubSessionById(subSessionId);
  const subActions = useSubSessionActions();
  const iconDataUri = useSubSessionIcon(subSessionId);
  const isActive = useWorktreeTabStore((s) => {
    if (!sub) return false;
    const tab = s.tabs.find((t) => t.id === sub.parentWorktreeTabId);
    return s.activeId === sub.parentWorktreeTabId && tab?.activeChildId?.kind === 'subSession' && tab.activeChildId.id === subSessionId;
  });
  const rowButtonRef = useRef<HTMLButtonElement | null>(null);
  const [menu, setMenu] = useState<{ anchor: { x: number; y: number } } | null>(null);

  if (!sub) return null;

  const isExited = sub.status === 'exited' || sub.status === 'error';

  const handleClick = (): void => {
    if (isExited && sub.kind === 'application') {
      void subActions.relaunch(subSessionId);
      return;
    }
    void subActions.focus(subSessionId);
  };

  const handleClose = (e: React.MouseEvent): void => {
    e.stopPropagation();
    if (sub.kind === 'application' && !isExited) {
      subActions.requestClose(subSessionId);
      return;
    }
    void subActions.close(subSessionId);
  };

  const stateClasses = isActive
    ? 'bg-sky-100 text-sky-900 dark:bg-sky-900/40 dark:text-sky-100'
    : 'text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-slate-800';

  return (
    <li className="group relative px-2">
      <button
        ref={rowButtonRef}
        type="button"
        aria-current={isActive ? 'page' : undefined}
        onClick={handleClick}
        className={`flex w-full items-center gap-2 rounded-md py-1 pl-5 pr-12 text-left text-xs transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 ${stateClasses}`}
      >
        <SubTabIcon kind={sub.kind} iconDataUri={iconDataUri} label={sub.label} />
        <span className="min-w-0 flex-1 truncate">{sub.label}</span>
        <SubStatusDot status={sub.status} />
      </button>
      <button
        type="button"
        aria-label={`More actions for sub-session ${sub.label}`}
        aria-haspopup="menu"
        data-testid={`sub-tab-menu-${subSessionId}`}
        onClick={(e) => {
          e.stopPropagation();
          const rect = e.currentTarget.getBoundingClientRect();
          setMenu({ anchor: { x: rect.left, y: rect.bottom + 2 } });
        }}
        className="absolute right-7 top-1/2 inline-flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded text-xs leading-none text-slate-400 opacity-0 transition-opacity hover:bg-slate-200 hover:text-slate-900 focus:opacity-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 group-hover:opacity-100 dark:text-slate-500 dark:hover:bg-slate-700 dark:hover:text-slate-100"
      >
        <span aria-hidden="true">⋮</span>
      </button>
      <button
        type="button"
        aria-label={`Close sub-session ${sub.label}`}
        onClick={handleClose}
        className="absolute right-2 top-1/2 inline-flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded text-xs leading-none text-slate-400 opacity-0 transition-opacity hover:bg-slate-200 hover:text-slate-900 focus:opacity-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 group-hover:opacity-100 dark:text-slate-500 dark:hover:bg-slate-700 dark:hover:text-slate-100"
      >
        <span aria-hidden="true">×</span>
      </button>
      {menu && (
        <SubTabContextMenu subSessionId={subSessionId} anchor={menu.anchor} onClose={() => setMenu(null)} restoreFocusTo={rowButtonRef.current} />
      )}
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
  return <span aria-hidden="true" data-testid={`sub-status-${status}`} className={`h-2 w-2 shrink-0 rounded-full ${colour}`} />;
}

/**
 * Sub-tab leading icon. Renders the OS application icon (PNG data
 * URI) when available; otherwise falls back to the kind-specific
 * emoji. Decorative — `aria-hidden` because the visible label
 * already conveys the sub-session identity for assistive tech.
 */
function SubTabIcon({ kind, iconDataUri, label }: { kind: 'terminal' | 'application'; iconDataUri: string | undefined; label: string }): JSX.Element {
  if (iconDataUri) {
    return (
      <img src={iconDataUri} alt="" aria-hidden="true" data-testid={`sub-tab-icon-${label}`} className="h-4 w-4 shrink-0 rounded-sm object-contain" />
    );
  }
  return (
    <span aria-hidden="true" className="text-xs text-slate-400">
      {kind === 'application' ? '🪟' : '⌗'}
    </span>
  );
}
