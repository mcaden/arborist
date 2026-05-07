// SidebarWorktreeTab — top-level header row for one worktree tab (issue #44).
//
// Visible information:
//   * Folder icon + worktree name (basename)
//   * Branch (if known) on a sub-line
//   * Status roll-up icon (max-priority across child sessions)
//   * Close button (×) — cascades close to all children via worktree_tab_close
//
// The header is rendered as a plain button inside an `<li role="presentation">`
// — it is **not** a `role="tab"` participant. The sidebar's tablist scope
// continues to enumerate session tabs only, matching the pre-#44 keyboard
// pattern. Right-click opens `WorktreeTabContextMenu` (Close + Launch ▸).
//
// Click activates the worktree tab and clears its `activeChildId` so the
// MainArea swaps to the dashboard placeholder. Users who want to land on
// a specific child should click that child directly.

import { useState } from 'react';

import { StatusIcon } from './StatusIcon';
import { formatError } from '@/lib/tauri-bridge';
import { useSessionStore } from '@/store/session-store';
import { selectWorktreeTabRollupStatus, useWorktreeTabActions, useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { WorktreeTabId } from '@/types/arborist';

interface SidebarWorktreeTabProps {
  tabId: WorktreeTabId;
  isActive: boolean;
  onOpenContextMenu: (tabId: WorktreeTabId, anchor: { x: number; y: number }, trigger: HTMLElement | null) => void;
}

export function SidebarWorktreeTab({ tabId, isActive, onOpenContextMenu }: SidebarWorktreeTabProps): JSX.Element | null {
  const tab = useWorktreeTabStore((s) => s.tabs.find((t) => t.id === tabId));
  // Subscribe to the rollup so the icon re-renders when any child changes status. Selector returns a primitive string so equality
  // gating works out of the box.
  const tabPath = tab?.path ?? '';
  const rollupStatus = useSessionStore((s) => selectWorktreeTabRollupStatus(tabPath)(s));
  const wttActions = useWorktreeTabActions();
  const [buttonEl, setButtonEl] = useState<HTMLButtonElement | null>(null);

  if (!tab) return null;

  const baseClasses =
    'flex w-full items-center gap-2 rounded-md py-1.5 pl-2 pr-7 text-left text-xs font-semibold uppercase tracking-wide transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500';
  const stateClasses = isActive
    ? 'bg-slate-200 text-slate-900 dark:bg-slate-800 dark:text-slate-100'
    : 'text-slate-600 hover:bg-slate-100 dark:text-slate-400 dark:hover:bg-slate-800';

  return (
    <li role="presentation" className="group relative px-2 pt-2">
      <button
        ref={setButtonEl}
        type="button"
        data-testid={`worktree-tab-${tab.id}`}
        aria-label={`Worktree ${tab.name}${tab.branch ? ` on branch ${tab.branch}` : ''}`}
        onClick={() => {
          // Activating a worktree header is a deliberate "show me the dashboard" gesture — clear the activeChildId so MainArea swaps
          // to <WorktreeDashboard> instead of staying on whichever session was previously visible.
          wttActions.patchActiveChild(tab.id, null);
          void wttActions.focus(tab.id);
          void wttActions.setActiveChild(tab.id, null).catch((err) => {
            console.warn(`[SidebarWorktreeTab] setActiveChild(${tab.id}, null) failed: ${formatError(err)}`);
          });
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          onOpenContextMenu(tab.id, { x: e.clientX, y: e.clientY }, buttonEl);
        }}
        onKeyDown={(e) => {
          if ((e.shiftKey && e.key === 'F10') || e.key === 'ContextMenu') {
            e.preventDefault();
            const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
            onOpenContextMenu(tab.id, { x: rect.left + 8, y: rect.bottom }, buttonEl);
          }
        }}
        className={`${baseClasses} ${stateClasses}`}
      >
        <span aria-hidden="true" className="text-sm">
          ▸
        </span>
        <span className="min-w-0 flex-1 truncate normal-case font-semibold">{tab.name}</span>
        {rollupStatus !== 'idle' && (
          <span className="shrink-0">
            <StatusIcon status={rollupStatus} title={`Children: ${rollupStatus}`} className="text-sm shrink-0" />
          </span>
        )}
      </button>
      <button
        type="button"
        aria-label={`Close worktree tab ${tab.name}`}
        data-testid={`worktree-tab-close-${tab.id}`}
        onClick={(e) => {
          e.stopPropagation();
          void wttActions.close(tab.id).catch((err) => {
            console.warn(`[SidebarWorktreeTab] close(${tab.id}) failed: ${formatError(err)}`);
          });
        }}
        className="absolute right-3 top-3 rounded p-0.5 text-slate-500 opacity-0 transition-opacity hover:bg-slate-200 hover:text-slate-900 focus:opacity-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 group-hover:opacity-100 dark:text-slate-400 dark:hover:bg-slate-700 dark:hover:text-slate-100"
      >
        <span aria-hidden="true">×</span>
      </button>
      {tab.branch && <p className="px-2 pb-1 pt-0.5 font-mono text-[10px] text-slate-500 dark:text-slate-500">{tab.branch}</p>}
    </li>
  );
}
