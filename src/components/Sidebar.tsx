// Vertical-tab sidebar — SPEC §5.1 (S-01..S-08), NF-06.
//
// Owns the visual layout, drag-to-reorder via @dnd-kit, full keyboard
// navigation (arrow / Home / End / Enter / Delete / Alt+arrow), and focus
// management after a tab is closed. Per-session state lives in the Zustand
// session store; the *focused* tab index is purely local UI state and lives
// here.

import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  sortableKeyboardCoordinates,
  verticalListSortingStrategy,
  arrayMove,
} from '@dnd-kit/sortable';
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';

import { CloseConfirmDialog } from './CloseConfirmDialog';
import { NewSessionButton } from './NewSessionButton';
import { SettingsDialog } from './SettingsDialog';
import { SidebarSubTab } from './SidebarSubTab';
import { SidebarTab } from './SidebarTab';
import { TabContextMenu } from './TabContextMenu';
import { WorkspaceIndicator } from './WorkspaceIndicator';
import { useActiveSessionId, useSessionActions, useSessions } from '@/store/session-store';
import { useSubSessionsForParent } from '@/store/sub-session-store';
import type { SessionId } from '@/types/arborist';

export function Sidebar(): JSX.Element {
  const sessions = useSessions();
  const activeId = useActiveSessionId();
  const actions = useSessionActions();
  const [settingsOpen, setSettingsOpen] = useState<boolean>(false);
  const [settingsInitialTab, setSettingsInitialTab] = useState<'general' | 'customProcesses'>(
    'general',
  );

  // Single-menu invariant: at most one TabContextMenu open across all
  // tabs. The triggering button is captured so we can restore focus on
  // close (Esc / outside-click / activation).
  const [contextMenu, setContextMenu] = useState<{
    sessionId: SessionId;
    anchor: { x: number; y: number };
    trigger: HTMLElement | null;
  } | null>(null);

  const ids = useMemo(() => sessions.map((s) => s.id), [sessions]);

  // Local roving-tabindex state. Tracks which tab the keyboard cursor is
  // on (separate from the active/visible session — Tab nav and clicking
  // are distinct interactions in the WAI-ARIA tabs pattern).
  const [focusedIndex, setFocusedIndex] = useState<number>(0);

  // Refs to each tab's <button> so we can imperatively move DOM focus.
  const tabButtonRefs = useRef<Map<SessionId, HTMLButtonElement>>(new Map());
  const newSessionButtonRef = useRef<HTMLButtonElement | null>(null);

  // Snapshot of `ids` on the previous render. Used by the post-render
  // effect to detect removals and focus the right neighbour.
  const previousIdsRef = useRef<SessionId[]>(ids);

  const onFocusableMounted = useCallback((id: SessionId, el: HTMLButtonElement | null) => {
    if (el) tabButtonRefs.current.set(id, el);
    else tabButtonRefs.current.delete(id);
  }, []);

  const openContextMenu = useCallback((sessionId: SessionId, anchor: { x: number; y: number }) => {
    const trigger = tabButtonRefs.current.get(sessionId) ?? null;
    setContextMenu({ sessionId, anchor, trigger });
  }, []);

  const closeContextMenu = useCallback(() => {
    setContextMenu(null);
  }, []);

  // After a session is removed, move focus to the neighbour (right, then
  // left, then the new-session button). Also keeps `focusedIndex` valid
  // after a reorder.
  useEffect(() => {
    const previousIds = previousIdsRef.current;
    previousIdsRef.current = ids;

    if (previousIds === ids) return;

    const sameLength = previousIds.length === ids.length;
    const sameMembers = sameLength && previousIds.every((id) => ids.includes(id));

    if (sameMembers) {
      // Pure reorder — sync focusedIndex to the moved id's new position.
      const focusedId = previousIds[focusedIndex];
      if (focusedId !== undefined) {
        const newPos = ids.indexOf(focusedId);
        if (newPos !== -1 && newPos !== focusedIndex) setFocusedIndex(newPos);
      }
      return;
    }

    const removed = previousIds.filter((id) => !ids.includes(id));
    if (removed.length === 0) return;

    if (ids.length === 0) {
      setFocusedIndex(0);
      newSessionButtonRef.current?.focus();
      return;
    }

    // Find the right neighbour of the first removed id (in the previous
    // list), falling back to the left, falling back to the tail.
    const removedIdx = previousIds.indexOf(removed[0]!);
    let nextId: SessionId | undefined;
    for (let i = removedIdx + 1; i < previousIds.length; i++) {
      const candidate = previousIds[i]!;
      if (ids.includes(candidate)) {
        nextId = candidate;
        break;
      }
    }
    if (nextId === undefined) {
      for (let i = removedIdx - 1; i >= 0; i--) {
        const candidate = previousIds[i]!;
        if (ids.includes(candidate)) {
          nextId = candidate;
          break;
        }
      }
    }
    if (nextId === undefined) nextId = ids[ids.length - 1];
    if (nextId !== undefined) {
      setFocusedIndex(ids.indexOf(nextId));
      tabButtonRefs.current.get(nextId)?.focus();
    }
  }, [ids, focusedIndex]);

  // Keep the keyboard cursor aligned with the active session when the
  // user activates via mouse click / programmatic focus.
  useEffect(() => {
    if (activeId === undefined) return;
    const idx = ids.indexOf(activeId);
    if (idx !== -1 && idx !== focusedIndex) setFocusedIndex(idx);
    // We intentionally exclude `focusedIndex` from deps: this effect only
    // realigns when the *active* session changes from the outside.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeId, ids]);

  const focusTabAt = useCallback(
    (index: number) => {
      const id = ids[index];
      if (id === undefined) return;
      setFocusedIndex(index);
      tabButtonRefs.current.get(id)?.focus();
    },
    [ids],
  );

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>): void => {
    // The sidebar implements the WAI-ARIA tabs keyboard pattern. It
    // must not intercept events that originate from descendant inputs
    // or modal dialogs (e.g. the Settings dialog or NewSession dialog
    // rendered inside the aside): otherwise Space/Arrow/Delete typed
    // into a textbox would move tab focus or trigger close-confirm.
    const target = e.target as HTMLElement | null;
    if (target) {
      const tag = target.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
      if (target.isContentEditable) return;
      if (target.closest('[role="dialog"]')) return;
    }
    if (ids.length === 0) return;
    const current = Math.min(focusedIndex, ids.length - 1);
    const currentId = ids[current];
    if (currentId === undefined) return;

    if (e.altKey && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
      const delta = e.key === 'ArrowUp' ? -1 : 1;
      const target = current + delta;
      if (target < 0 || target >= ids.length) {
        e.preventDefault();
        return;
      }
      e.preventDefault();
      const next = arrayMove(ids, current, target);
      void actions.reorder(next);
      // The post-reorder effect will sync `focusedIndex` to currentId's
      // new position; re-focus the DOM node on the next paint.
      requestAnimationFrame(() => {
        tabButtonRefs.current.get(currentId)?.focus();
      });
      return;
    }

    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        focusTabAt(Math.min(current + 1, ids.length - 1));
        break;
      case 'ArrowUp':
        e.preventDefault();
        focusTabAt(Math.max(current - 1, 0));
        break;
      case 'Home':
        e.preventDefault();
        focusTabAt(0);
        break;
      case 'End':
        e.preventDefault();
        focusTabAt(ids.length - 1);
        break;
      case 'Enter':
      case ' ':
        e.preventDefault();
        void actions.focus(currentId);
        break;
      case 'Delete':
        e.preventDefault();
        actions.requestClose(currentId);
        break;
      default:
    }
  };

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const handleDragEnd = (event: DragEndEvent): void => {
    const { active, over } = event;
    if (!over || active.id === over.id) return;
    const fromIndex = ids.indexOf(String(active.id));
    const toIndex = ids.indexOf(String(over.id));
    if (fromIndex === -1 || toIndex === -1) return;
    const next = arrayMove(ids, fromIndex, toIndex);
    void actions.reorder(next);
  };

  const clampedFocusedIndex = Math.min(focusedIndex, Math.max(0, ids.length - 1));

  return (
    <aside
      aria-label="Sessions"
      role="tablist"
      aria-orientation="vertical"
      onKeyDown={onKeyDown}
      className="flex h-full w-56 shrink-0 flex-col border-r border-slate-200 bg-slate-50 dark:border-slate-800 dark:bg-slate-900"
    >
      <WorkspaceIndicator />
      <NewSessionButton buttonRef={newSessionButtonRef} />
      <DndContext sensors={sensors} onDragEnd={handleDragEnd}>
        <SortableContext items={ids} strategy={verticalListSortingStrategy}>
          <ul className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto py-1">
            {sessions.map((session, idx) => (
              <ParentTabGroup
                key={session.id}
                id={session.id}
                isActive={session.id === activeId}
                isFocused={idx === clampedFocusedIndex}
                onFocusableMounted={onFocusableMounted}
                onOpenContextMenu={openContextMenu}
              />
            ))}
          </ul>
        </SortableContext>
      </DndContext>
      <div className="mt-auto border-t border-slate-200 px-2 py-2 dark:border-slate-800">
        <button
          type="button"
          data-testid="settings-button"
          onClick={() => {
            setSettingsInitialTab('general');
            setSettingsOpen(true);
          }}
          className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-xs text-slate-600 hover:bg-slate-200 dark:text-slate-300 dark:hover:bg-slate-800"
        >
          <span aria-hidden="true">⚙</span>
          <span>Settings</span>
        </button>
      </div>
      <CloseConfirmDialog />
      {contextMenu && (
        <TabContextMenu
          parentSessionId={contextMenu.sessionId}
          anchor={contextMenu.anchor}
          onClose={closeContextMenu}
          restoreFocusTo={contextMenu.trigger}
          onOpenSettings={() => {
            setSettingsInitialTab('customProcesses');
            setSettingsOpen(true);
          }}
        />
      )}
      {settingsOpen ? (
        <SettingsDialog onClose={() => setSettingsOpen(false)} initialTab={settingsInitialTab} />
      ) : null}
    </aside>
  );
}

// ParentTabGroup — renders a parent SidebarTab plus all its sub-tab rows
// indented underneath. Sub-tabs are intentionally outside the @dnd-kit
// SortableContext (no drag-reorder for sub-tabs in v1) but live inside
// the sidebar's tablist for keyboard / focus purposes.
interface ParentTabGroupProps {
  id: SessionId;
  isActive: boolean;
  isFocused: boolean;
  onFocusableMounted: (id: SessionId, el: HTMLButtonElement | null) => void;
  onOpenContextMenu: (sessionId: SessionId, anchor: { x: number; y: number }) => void;
}

function ParentTabGroup({
  id,
  isActive,
  isFocused,
  onFocusableMounted,
  onOpenContextMenu,
}: ParentTabGroupProps): JSX.Element {
  const subSessions = useSubSessionsForParent(id);
  return (
    <>
      <SidebarTab
        id={id}
        isActive={isActive}
        isFocused={isFocused}
        onFocusableMounted={onFocusableMounted}
        onOpenContextMenu={onOpenContextMenu}
      />
      {subSessions.length > 0 && (
        <li role="presentation">
          <ul role="group" aria-label="Sub-sessions" className="flex flex-col gap-0.5">
            {subSessions.map((sub) => (
              <SidebarSubTab
                key={sub.id}
                parentId={id}
                subSessionId={sub.id}
                parentIsActive={isActive}
              />
            ))}
          </ul>
        </li>
      )}
    </>
  );
}
