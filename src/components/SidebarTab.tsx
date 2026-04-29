// A single vertical tab in the sidebar. Renders the tool icon, the label,
// an error indicator dot when the session has crashed, and a small close
// button that opens the close-confirmation dialog.
//
// The whole tab is the @dnd-kit drag handle. The close button stops the
// pointer-down event so dragging from the close glyph doesn't accidentally
// initiate a reorder.

import { useSortable } from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';

import { ToolIcon } from './ToolIcon';
import {
  useActivity,
  useHasUnread,
  useMetrics,
  useSessionActions,
  useSessionById,
} from '@/store/session-store';
import type { SessionId, SessionMetrics, Tool } from '@/types/arborist';

interface SidebarTabProps {
  id: SessionId;
  isActive: boolean;
  isFocused: boolean;
  onFocusableMounted: (id: SessionId, el: HTMLButtonElement | null) => void;
}

export function SidebarTab({
  id,
  isActive,
  isFocused,
  onFocusableMounted,
}: SidebarTabProps): JSX.Element | null {
  const session = useSessionById(id);
  const hasUnread = useHasUnread(id);
  const activity = useActivity(id);
  const metrics = useMetrics(id);
  const actions = useSessionActions();

  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id,
  });

  if (!session) return null;

  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    opacity: isDragging ? 0.6 : 1,
  };

  const baseClasses =
    'flex w-full flex-col items-stretch gap-0.5 rounded-md py-2 pl-2 pr-7 text-left text-sm transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500';
  const stateClasses = isActive
    ? 'bg-sky-100 text-sky-900 dark:bg-sky-900/40 dark:text-sky-100'
    : 'text-slate-700 hover:bg-slate-100 dark:text-slate-200 dark:hover:bg-slate-800';

  return (
    <li ref={setNodeRef} style={style} className="group relative px-2">
      <button
        ref={(el) => onFocusableMounted(id, el)}
        {...attributes}
        {...listeners}
        type="button"
        role="tab"
        id={`session-tab-${id}`}
        aria-selected={isActive}
        aria-label={`${session.tool} session ${session.label}`}
        tabIndex={isFocused ? 0 : -1}
        onClick={() => {
          void actions.focus(id);
        }}
        className={`${baseClasses} ${stateClasses}`}
      >
        <span className="flex w-full items-center gap-2">
          <ToolIcon
            tool={session.tool}
            className={
              isActive
                ? 'h-5 w-5 shrink-0 text-sky-700 dark:text-sky-300'
                : 'h-5 w-5 shrink-0 text-slate-500 dark:text-slate-400'
            }
          />
          <span className="min-w-0 flex-1 truncate">{session.label}</span>
          {activity === 'attention' && session.status !== 'error' && (
            <span
              role="img"
              aria-label="Attention required"
              data-testid="status-attention"
              className="h-2 w-2 shrink-0 rounded-full bg-amber-500"
            />
          )}
          {activity === 'working' && session.status === 'running' && (
            <span
              role="img"
              aria-label="Working"
              data-testid="status-working"
              className="h-2 w-2 shrink-0 animate-pulse rounded-full bg-emerald-500"
            />
          )}
          {hasUnread && !isActive && session.status !== 'error' && activity !== 'attention' && (
            <span
              role="img"
              aria-label="Unread output"
              data-testid="status-unread"
              className="h-2 w-2 shrink-0 rounded-full bg-sky-500"
            />
          )}
          {session.status === 'starting' && (
            <span
              role="img"
              aria-label="Starting"
              data-testid="status-starting"
              className="h-2.5 w-2.5 shrink-0 animate-pulse rounded-full border-2 border-sky-500 border-t-transparent"
            />
          )}
          {session.status === 'exited' && (
            <span
              role="img"
              aria-label="Exited"
              data-testid="status-exited"
              className="h-2 w-2 shrink-0 rounded-full bg-slate-400 dark:bg-slate-500"
            />
          )}
          {session.status === 'error' && (
            <span
              role="img"
              aria-label="Error"
              className="h-2 w-2 shrink-0 rounded-full bg-red-500"
            />
          )}
        </span>
        <MetricsLine
          metrics={session.status === 'running' ? metrics : undefined}
          tool={session.tool}
          isActive={isActive}
        />
      </button>
      <button
        type="button"
        aria-label={`Close session ${session.label}`}
        onPointerDown={(e) => {
          // Don't let the drag listeners on the parent treat this as a drag.
          e.stopPropagation();
        }}
        onClick={(e) => {
          e.stopPropagation();
          actions.requestClose(id);
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
// whole tab remains the single keyboard/DnD target.
// ---------------------------------------------------------------------------

interface MetricsLineProps {
  metrics: SessionMetrics | undefined;
  tool: Tool;
  isActive: boolean;
}

function MetricsLine({ metrics, tool, isActive }: MetricsLineProps): JSX.Element {
  const colour = isActive
    ? 'text-sky-800/80 dark:text-sky-200/80'
    : 'text-slate-500 dark:text-slate-400';

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
  if (
    typeof metrics.contextTokensUsed === 'number' &&
    typeof metrics.contextTokensLimit === 'number'
  ) {
    const suffix =
      tool === 'copilot' ? ' (Copilot-reported; excludes its system-prompt + tool overhead)' : '';
    longParts.push(
      `Context ${metrics.contextTokensUsed.toLocaleString()} / ` +
        `${metrics.contextTokensLimit.toLocaleString()} tokens${suffix}`,
    );
  }
  if (typeof metrics.inputTokens === 'number' && typeof metrics.outputTokens === 'number') {
    longParts.push(
      `Session totals: ${metrics.inputTokens.toLocaleString()} in, ` +
        `${metrics.outputTokens.toLocaleString()} out`,
    );
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

/** Format a token count as `12.3k` for >= 1000, else as the raw number. */
function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  const k = n / 1000;
  return k >= 100 ? `${Math.round(k)}k` : `${k.toFixed(1).replace(/\.0$/, '')}k`;
}
