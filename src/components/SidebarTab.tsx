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
import { useHasUnread, useSessionActions, useSessionById } from '@/store/session-store';
import type { SessionId } from '@/types/arborist';

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
    'flex w-full items-center gap-2 rounded-md py-2 pl-2 pr-7 text-left text-sm transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500';
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
        <ToolIcon
          tool={session.tool}
          className={
            isActive
              ? 'h-5 w-5 shrink-0 text-sky-700 dark:text-sky-300'
              : 'h-5 w-5 shrink-0 text-slate-500 dark:text-slate-400'
          }
        />
        <span className="min-w-0 flex-1 truncate">{session.label}</span>
        {hasUnread && !isActive && session.status !== 'error' && (
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
        className="absolute right-3 top-1/2 -translate-y-1/2 rounded p-1 text-slate-500 opacity-0 transition-opacity hover:bg-slate-200 hover:text-slate-900 focus:opacity-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 group-hover:opacity-100 dark:text-slate-400 dark:hover:bg-slate-700 dark:hover:text-slate-100"
      >
        <span aria-hidden="true">×</span>
      </button>
    </li>
  );
}
