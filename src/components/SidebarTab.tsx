// A single vertical tab in the sidebar. Renders the tool icon, the label,
// an error indicator dot when the session has crashed, and a small close
// button that immediately closes the AI-agent session (no confirmation —
// the worktree-deletion confirmation lives on the worktree parent tab,
// see WorktreeCloseConfirmDialog).
//
// Drag-to-reorder of session tabs was removed in the worktree-tab UI
// roll-out (issue #44). The grouped layout invalidated the previous flat
// reorder model; per-group reorder is a planned follow-up.

import { StatusIcon } from './StatusIcon';
import { ToolIcon } from './ToolIcon';
import { BranchDecoration } from './BranchDecoration';
import { formatError } from '@/lib/tauri-bridge';
import { useConfigStore } from '@/store/config-store';
import {
  useDisplayStatus,
  useHasUnread,
  useLastTurnDurationMs,
  useLastTurnEndAt,
  useMetrics,
  useOpenPermissions,
  useOpenTools,
  useSessionActions,
  useSessionById,
  useToolSiblingOrdinal,
  type DisplayStatus,
  type OpenPermission,
  type OpenTool,
} from '@/store/session-store';
import type { SessionId, SessionMetrics, Tool } from '@/types/arborist';

interface SidebarTabProps {
  id: SessionId;
  isActive: boolean;
  isFocused: boolean;
  onFocusableMounted: (id: SessionId, el: HTMLButtonElement | null) => void;
  /**
   * Open the context menu anchored at viewport coordinates. The Sidebar
   * owns the menu state so only one menu is open at a time across all
   * tabs.
   */
  onOpenContextMenu: (sessionId: SessionId, anchor: { x: number; y: number }) => void;
  /**
   * Render the git-branch style "trunk + branch + node" decoration on
   * the left edge so this tab visually nests under its parent worktree
   * header. Defaults to `true`; orphan sessions (no matching worktree)
   * pass `false` so they render flush without a phantom rail.
   */
  nested?: boolean;
  /**
   * When `nested`, marks the last child of its worktree group so the
   * rail terminates with an "└" elbow at the node instead of carrying
   * on into the next group.
   */
  isLastInGroup?: boolean;
}

export function SidebarTab({
  id,
  isActive,
  isFocused,
  onFocusableMounted,
  onOpenContextMenu,
  nested = true,
  isLastInGroup = false,
}: SidebarTabProps): JSX.Element | null {
  const session = useSessionById(id);
  const hasUnread = useHasUnread(id);
  const displayStatus = useDisplayStatus(id);
  const lastTurnEndAt = useLastTurnEndAt(id);
  const lastTurnDurationMs = useLastTurnDurationMs(id);
  const openTools = useOpenTools(id);
  const openPermissions = useOpenPermissions(id);
  const metrics = useMetrics(id);
  const actions = useSessionActions();
  // Pull the cached AI-tool icon URI from config. The selector is a
  // narrow string-or-undefined so unrelated config changes don't
  // re-render this row.
  const tool = session?.tool;
  const toolIconDataUri = useConfigStore((s) =>
    tool === 'claude' ? s.config.aiLaunchCommands.claudeIconDataUri : tool === 'copilot' ? s.config.aiLaunchCommands.copilotIconDataUri : undefined,
  );
  // `useToolSiblingOrdinal` builds a single (tool, worktreePath) ->
  // ordinal map per `sessions` array reference and shares it across
  // every SidebarTab — each tab's lookup is O(1) and the hook returns
  // a primitive so unrelated session changes don't re-render this row.
  const siblingOrdinal = useToolSiblingOrdinal(id);

  if (!session) return null;

  const baseClasses =
    'flex w-full flex-col items-stretch gap-0.5 rounded-md py-2 pl-5 pr-7 text-left text-sm transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500';
  const stateClasses = isActive
    ? 'bg-sky-100 text-sky-900 dark:bg-sky-900/40 dark:text-sky-100'
    : 'text-slate-700 hover:bg-slate-100 dark:text-slate-200 dark:hover:bg-slate-800';

  return (
    <li className={`group relative pr-2 ${nested ? 'ml-6' : 'ml-2'}`}>
      {nested && <BranchDecoration isLastInGroup={isLastInGroup} anchorTop="18px" />}
      <button
        ref={(el) => onFocusableMounted(id, el)}
        type="button"
        role="tab"
        id={`session-tab-${id}`}
        aria-selected={isActive}
        aria-label={`${session.tool} session ${session.label}${hasUnread && !isActive ? ' (unread output)' : ''}`}
        tabIndex={isFocused ? 0 : -1}
        onClick={() => {
          // Clicking a session tab focuses it via the worktree-tab activeChildId
          // mechanism (actions.focus sets activeChildId to this session).
          void actions.focus(id);
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          onOpenContextMenu(id, { x: e.clientX, y: e.clientY });
        }}
        onKeyDown={(e) => {
          // Shift+F10 and the Apps / ContextMenu key are the standard
          // keyboard shortcuts for the context menu (matches OS menus
          // and browsers). Anchor the menu to the tab's bounding rect
          // so it appears near the focused element. Don't preventDefault
          // for other keys — Sidebar's tablist handler still owns Arrow
          // / Home / End / Delete / Enter / Space.
          if ((e.shiftKey && e.key === 'F10') || e.key === 'ContextMenu') {
            e.preventDefault();
            const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
            onOpenContextMenu(id, { x: rect.left + 8, y: rect.bottom });
          }
        }}
        className={`${baseClasses} ${stateClasses}`}
      >
        <span className="flex w-full items-center gap-2">
          <ToolIcon
            tool={session.tool}
            {...(toolIconDataUri !== undefined ? { iconDataUri: toolIconDataUri } : {})}
            className={isActive ? 'h-5 w-5 shrink-0 text-sky-700 dark:text-sky-300' : 'h-5 w-5 shrink-0 text-slate-500 dark:text-slate-400'}
          />
          <span className="min-w-0 flex-1 truncate">
            {toolDisplayName(session.tool) + (siblingOrdinal !== undefined ? ` ${siblingOrdinal}` : '')}
          </span>
          <SessionStatusIndicator
            status={displayStatus}
            hasUnread={hasUnread && !isActive}
            lastTurnEndAt={lastTurnEndAt}
            lastTurnDurationMs={lastTurnDurationMs}
            openTools={openTools}
            openPermissions={openPermissions}
          />
        </span>
        <MetricsLine metrics={session.status === 'running' ? metrics : undefined} tool={session.tool} isActive={isActive} />
      </button>
      <button
        type="button"
        aria-label={`Close session ${session.label}`}
        onClick={(e) => {
          e.stopPropagation();
          void actions.close(id, false).catch((err) => {
            console.warn(`[sidebar-tab] close failed for session ${id}: ${formatError(err)}`);
          });
        }}
        className="absolute right-3 top-1.5 rounded p-1 text-slate-500 opacity-0 transition-opacity hover:bg-slate-200 hover:text-slate-900 focus:opacity-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 group-hover:opacity-100 dark:text-slate-400 dark:hover:bg-slate-700 dark:hover:text-slate-100"
      >
        <span aria-hidden="true">×</span>
      </button>
    </li>
  );
}

// ---------------------------------------------------------------------------
// MetricsLine — compact second line under the label showing context-window
// usage and total token count. Non-interactive (no nested focusables): the
// whole tab remains the single keyboard target.
// ---------------------------------------------------------------------------

interface MetricsLineProps {
  metrics: SessionMetrics | undefined;
  tool: Tool;
  isActive: boolean;
}

function MetricsLine({ metrics, tool, isActive }: MetricsLineProps): JSX.Element {
  const colour = isActive ? 'text-sky-800/80 dark:text-sky-200/80' : 'text-slate-500 dark:text-slate-400';

  if (!metrics) {
    // Same height as a populated metrics line — preserves uniform tab
    // height while we wait for the first snapshot.
    return (
      <span aria-hidden="true" className={`block pl-7 text-xs leading-tight ${colour}`}>
        &nbsp;
      </span>
    );
  }

  const parts: string[] = [];
  if (typeof metrics.contextUsedPct === 'number') {
    parts.push(`${metrics.contextUsedPct}%`);
  }
  if (typeof metrics.contextTokensUsed === 'number') {
    parts.push(`${formatTokens(metrics.contextTokensUsed)} tok`);
  }
  if (parts.length === 0) {
    return (
      <span aria-hidden="true" className={`block pl-7 text-xs leading-tight ${colour}`}>
        &nbsp;
      </span>
    );
  }
  const text = parts.join(' · ');

  const longParts: string[] = [];
  if (typeof metrics.contextTokensUsed === 'number' && typeof metrics.contextTokensLimit === 'number') {
    const suffix =
      tool === 'copilot'
        ? ' (Copilot-reported; excludes its system-prompt + tool overhead)'
        : ' (model nominal max; includes harness overhead in usage)';
    longParts.push(`Context ${metrics.contextTokensUsed.toLocaleString()} / ` + `${metrics.contextTokensLimit.toLocaleString()} tokens${suffix}`);
  }
  if (typeof metrics.inputTokens === 'number' && typeof metrics.outputTokens === 'number') {
    longParts.push(`Session totals: ${metrics.inputTokens.toLocaleString()} in, ` + `${metrics.outputTokens.toLocaleString()} out`);
  }
  if (metrics.model) {
    longParts.push(`Model: ${metrics.model}`);
  }

  return (
    <span
      data-testid="sidebar-metrics"
      title={longParts.join('\n') || undefined}
      className={`block pl-7 text-xs leading-tight tabular-nums ${colour}`}
    >
      {text}
    </span>
  );
}

// AI session tabs sit *under* a worktree tab that already shows the
// worktree name, so the row itself just identifies which CLI it hosts.
function toolDisplayName(tool: Tool): string {
  switch (tool) {
    case 'claude':
      return 'Claude CLI';
    case 'copilot':
      return 'Copilot CLI';
  }
}

/** Format a token count as `12.3k` for >= 1000, else as the raw number. */
function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  const k = n / 1000;
  return k >= 100 ? `${Math.round(k)}k` : `${k.toFixed(1).replace(/\.0$/, '')}k`;
}

// ---------------------------------------------------------------------------
// SessionStatusIndicator — single icon glyph for the derived display state,
// plus a small unread-overlay dot when the tab has unseen output. The icon
// owns the per-state colour and animation; the unread dot is a separate
// element so the two stack naturally.
// ---------------------------------------------------------------------------

interface SessionStatusIndicatorProps {
  status: DisplayStatus;
  hasUnread: boolean;
  lastTurnEndAt: number | undefined;
  lastTurnDurationMs: number | undefined;
  openTools: Record<string, OpenTool> | undefined;
  openPermissions: Record<string, OpenPermission> | undefined;
}

function SessionStatusIndicator({
  status,
  hasUnread,
  lastTurnEndAt,
  lastTurnDurationMs,
  openTools,
  openPermissions,
}: SessionStatusIndicatorProps): JSX.Element | null {
  // When the icon collapses to nothing (idle), still show the unread dot
  // on its own so the user has *some* signal that output arrived. The
  // dot is decorative — the parent tab's aria-label already conveys
  // unread state to assistive tech, so no role/aria-label here.
  if (status === 'idle') {
    return hasUnread ? <span aria-hidden="true" data-testid="status-unread" className="h-2 w-2 shrink-0 rounded-full bg-sky-500" /> : null;
  }

  const iconClasses = statusIconClasses(status);
  const tooltip = statusTooltip(status, lastTurnEndAt, lastTurnDurationMs, openTools, openPermissions);

  return (
    <span className="relative inline-flex shrink-0">
      <StatusIcon status={status} title={tooltip} className={iconClasses} />
      {hasUnread && status !== 'attention' && status !== 'awaitingPermission' && status !== 'error' && (
        <span
          aria-hidden="true"
          data-testid="status-unread"
          className="absolute -right-0.5 -top-0.5 h-1.5 w-1.5 rounded-full bg-sky-500 ring-1 ring-white dark:ring-slate-900"
        />
      )}
    </span>
  );
}

function statusIconClasses(status: DisplayStatus): string {
  // Uniform geometry; only color and animation vary by state. The icon
  // is now a Nerd Font codicon glyph (see `StatusIcon.tsx`), so size is
  // controlled by `text-*` (font-size) rather than `h-* w-*` — the
  // glyph's box is pinned to 1em × 1em inside the component.
  const base = 'text-base shrink-0';
  switch (status) {
    case 'starting':
      return `${base} animate-spin text-sky-500`;
    case 'working':
      return `${base} animate-pulse text-emerald-500`;
    case 'awaiting':
      return `${base} text-sky-500 dark:text-sky-400`;
    case 'attention':
      return `${base} text-amber-500`;
    case 'awaitingPermission':
      return `${base} animate-pulse text-amber-500`;
    case 'runningTool':
      return `${base} animate-pulse text-emerald-500`;
    case 'thinking':
      return `${base} animate-pulse text-emerald-500`;
    case 'exited':
      return `${base} text-slate-400 dark:text-slate-500`;
    case 'error':
      return `${base} text-red-500`;
    case 'idle':
      return base;
  }
}

function statusTooltip(
  status: DisplayStatus,
  lastTurnEndAt: number | undefined,
  lastTurnDurationMs: number | undefined,
  openTools: Record<string, OpenTool> | undefined,
  openPermissions: Record<string, OpenPermission> | undefined,
): string {
  const headline = (() => {
    switch (status) {
      case 'starting':
        return 'Starting';
      case 'working':
        return 'Working';
      case 'thinking':
        return 'Thinking';
      case 'runningTool': {
        // Surface the in-flight tool name(s) — the user often wants to
        // know whether the agent is doing something benign (`view`) or
        // something they may want to inspect (`shell`, `edit`).
        const tools = openTools ? Object.values(openTools) : [];
        if (tools.length === 1) return `Running tool: ${tools[0]!.toolName}`;
        if (tools.length > 1) return `Running ${tools.length} tools: ${tools.map((t) => t.toolName).join(', ')}`;
        return 'Running tool';
      }
      case 'awaitingPermission': {
        // The single most actionable state — make sure the tooltip
        // identifies the kind so the user knows whether to bring focus
        // here or let it sit (e.g. an `mcp` prompt vs a `shell` prompt).
        const perms = openPermissions ? Object.values(openPermissions) : [];
        if (perms.length === 1) {
          const p = perms[0]!;
          const summary = p.summary && p.summary.length > 0 ? `: ${p.summary}` : '';
          return `Awaiting approval (${p.permissionKind})${summary}`;
        }
        if (perms.length > 1) return `Awaiting ${perms.length} approvals`;
        return 'Awaiting approval';
      }
      case 'awaiting':
        return 'Awaiting input';
      case 'attention':
        return 'Attention required';
      case 'exited':
        return 'Exited';
      case 'error':
        return 'Error';
      case 'idle':
        return 'Idle';
    }
  })();

  const parts: string[] = [headline];
  if (status === 'awaiting' && lastTurnEndAt !== undefined) {
    const ageSec = Math.max(0, Math.floor(Date.now() / 1000) - lastTurnEndAt);
    parts.push(`Last turn ${formatDuration(ageSec)} ago`);
  }
  if (typeof lastTurnDurationMs === 'number' && status !== 'starting' && status !== 'error') {
    parts.push(`Took ${formatDurationMs(lastTurnDurationMs)}`);
  }
  return parts.join(' · ');
}

function formatDuration(sec: number): string {
  if (sec < 60) return `${sec}s`;
  const m = Math.floor(sec / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h`;
}

function formatDurationMs(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  return formatDuration(Math.round(s));
}
