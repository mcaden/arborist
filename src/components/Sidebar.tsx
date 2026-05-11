// Vertical-tab sidebar — SPEC §5.1 (S-01..S-08), NF-06.
//
// Issue #44 — sessions are now grouped under their parent **worktree tab**.
// Top-level structure:
//
//   <aside role="tablist">                    (the keyboard tablist scope)
//     [worktree header]   (li role="presentation", NOT a tab)
//       SidebarTab        (role="tab")
//       SidebarTab
//       SidebarSubTab     (role="button")
//     [worktree header]
//       SidebarTab
//       SidebarSubTab
//
// Worktree-tab headers are presentation only — they don't claim `role="tab"`,
// so the WAI-ARIA tabs pattern still walks just session tabs (matches how
// users moved between sessions before #44). They get their own click +
// right-click handlers for "make this worktree active" / "close worktree" /
// "Launch agent…" actions.
//
// Drag-to-reorder is intentionally **deferred** for the initial worktree-tab
// UI roll-out: the prior implementation reordered a flat session list with
// @dnd-kit, but in a grouped layout the visual order and the flat `ids`
// array no longer match, which makes both pointer and Alt+arrow reorder
// behave incorrectly. Re-introducing reorder (per-group, plus a separate
// worktree-tab reorder) is a follow-up task tracked in plan.md.

import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';

import { WorktreeCloseConfirmDialog } from './WorktreeCloseConfirmDialog';
import { NewSessionButton } from './NewSessionButton';
import { SettingsDialog } from './SettingsDialog';
import { SidebarResizeHandle } from './SidebarResizeHandle';
import { clampSidebarWidth, DEFAULT_WIDTH_PX as SIDEBAR_DEFAULT_WIDTH_PX } from './sidebar-width';
import { SidebarSubTab } from './SidebarSubTab';
import { SidebarTab } from './SidebarTab';
import { SidebarWorktreeTab } from './SidebarWorktreeTab';
import { SubCloseConfirmDialog } from './SubCloseConfirmDialog';
import { TabContextMenu } from './TabContextMenu';
import { WorkspaceIndicator } from './WorkspaceIndicator';
import { WorktreeTabContextMenu } from './WorktreeTabContextMenu';
import { useSessionActions, useSessions } from '@/store/session-store';
import { useSubSessionsForWorktreeTab } from '@/store/sub-session-store';
import { useActiveWorktreeTabId, useWorktreeTabs } from '@/store/worktree-tab-store';
import { useConfigStore, selectSidebarWidthPx } from '@/store/config-store';
import { formatError } from '@/lib/tauri-bridge';
import type { SessionId, WorktreeTabId } from '@/types/arborist';

interface SessionGroup {
  /** `null` for the synthetic group of orphan sessions (no matching worktree tab). */
  tabId: WorktreeTabId | null;
  sessionIds: SessionId[];
}

export function Sidebar(): JSX.Element {
  const sessions = useSessions();
  const worktreeTabs = useWorktreeTabs();
  const activeWorktreeTabId = useActiveWorktreeTabId();
  const actions = useSessionActions();
  const [settingsOpen, setSettingsOpen] = useState<boolean>(false);
  const [settingsInitialTab, setSettingsInitialTab] = useState<'general' | 'customProcesses'>('general');

  // Per-tab right-click menus. Two flavours: one for session tabs, one for worktree-tab headers.
  const [contextMenu, setContextMenu] = useState<{
    sessionId: SessionId;
    anchor: { x: number; y: number };
    trigger: HTMLElement | null;
  } | null>(null);
  const [worktreeContextMenu, setWorktreeContextMenu] = useState<{
    tabId: WorktreeTabId;
    anchor: { x: number; y: number };
    trigger: HTMLElement | null;
  } | null>(null);

  // Group sessions by worktree path so each header renders followed by its child sessions. Sessions whose path doesn't match any
  // worktree tab fall into a synthetic "unlinked" group at the bottom — the boot self-heal opens tabs for all known session paths,
  // but we render the unlinked bucket defensively so a transient drift never makes a session disappear from the sidebar.
  const activeWorktreeTab = useMemo(() => worktreeTabs.find((t) => t.id === activeWorktreeTabId) ?? null, [worktreeTabs, activeWorktreeTabId]);

  const groups = useMemo<SessionGroup[]>(() => {
    const byPath = new Map<string, SessionId[]>();
    const orphans: SessionId[] = [];
    for (const s of sessions) {
      const tab = worktreeTabs.find((t) => t.path === s.worktreePath);
      if (!tab) {
        orphans.push(s.id);
        continue;
      }
      const list = byPath.get(tab.path) ?? [];
      list.push(s.id);
      byPath.set(tab.path, list);
    }
    const out: SessionGroup[] = worktreeTabs.map((tab) => ({ tabId: tab.id, sessionIds: byPath.get(tab.path) ?? [] }));
    if (orphans.length > 0) out.push({ tabId: null, sessionIds: orphans });
    return out;
  }, [sessions, worktreeTabs]);

  // Flat list of session ids in the order they're rendered. Drives the keyboard roving-tabindex.
  const ids = useMemo(() => groups.flatMap((g) => g.sessionIds), [groups]);

  const [focusedIndex, setFocusedIndex] = useState<number>(0);

  const tabButtonRefs = useRef<Map<SessionId, HTMLButtonElement>>(new Map());
  const newSessionButtonRef = useRef<HTMLButtonElement | null>(null);

  const previousIdsRef = useRef<SessionId[]>(ids);

  const onFocusableMounted = useCallback((id: SessionId, el: HTMLButtonElement | null) => {
    if (el) tabButtonRefs.current.set(id, el);
    else tabButtonRefs.current.delete(id);
  }, []);

  const openContextMenu = useCallback((sessionId: SessionId, anchor: { x: number; y: number }, trigger: HTMLElement | null) => {
    // The trigger is whichever element the user activated (the ⋮ button
    // for mouse / touch users, the tab button itself for Shift+F10 /
    // ContextMenu key users). Restoring focus to the source preserves
    // the user's keyboard context when the menu closes.
    const fallback = tabButtonRefs.current.get(sessionId) ?? null;
    setContextMenu({ sessionId, anchor, trigger: trigger ?? fallback });
  }, []);

  const closeContextMenu = useCallback(() => {
    setContextMenu(null);
  }, []);

  const openWorktreeContextMenu = useCallback((tabId: WorktreeTabId, anchor: { x: number; y: number }, trigger: HTMLElement | null) => {
    setWorktreeContextMenu({ tabId, anchor, trigger });
  }, []);

  const closeWorktreeContextMenu = useCallback(() => {
    setWorktreeContextMenu(null);
  }, []);

  // After a session is removed, move focus to the neighbour (right, then left, then the new-session button). Also keeps focusedIndex
  // valid after a reorder via worktree close (which removes a swathe of ids at once).
  useEffect(() => {
    const previousIds = previousIdsRef.current;
    previousIdsRef.current = ids;

    if (previousIds === ids) return;

    const sameLength = previousIds.length === ids.length;
    const sameMembers = sameLength && previousIds.every((id) => ids.includes(id));

    if (sameMembers) {
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

  // Keep the keyboard cursor aligned with whatever session is currently active under the focused worktree tab. When the active tab's
  // child changes (autolink on focus / close picking a sibling), surface that on the keyboard cursor too.
  const activeChildSessionId = activeWorktreeTab?.activeChildId?.kind === 'session' ? activeWorktreeTab.activeChildId.id : undefined;
  useEffect(() => {
    if (activeChildSessionId === undefined) return;
    const idx = ids.indexOf(activeChildSessionId);
    if (idx !== -1 && idx !== focusedIndex) setFocusedIndex(idx);
    // We intentionally exclude `focusedIndex` from deps: this effect only realigns when the *active* session changes from the outside.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeChildSessionId, ids]);

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
    const target = e.target as HTMLElement | null;
    if (target) {
      const tag = target.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
      if (target.isContentEditable) return;
      if (target.closest('[role="dialog"]')) return;
      if (tag === 'BUTTON' && !target.matches('[role="tab"]')) return;
    }
    if (ids.length === 0) return;
    const current = Math.min(focusedIndex, ids.length - 1);
    const currentId = ids[current];
    if (currentId === undefined) return;

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
        // Mirror the SidebarTab mouse-click behaviour so keyboard activation also clears any sub-session viewport claim — without this,
        // arrowing onto a parent and pressing Enter while a sub still owns the viewport would feel like a no-op.
        void actions.focus(currentId);
        break;
      case 'Delete':
        e.preventDefault();
        void actions.close(currentId, false).catch((err) => {
          console.warn(`[sidebar] keyboard close failed for session ${currentId}: ${formatError(err)}`);
        });
        break;
      default:
    }
  };

  const clampedFocusedIndex = Math.min(focusedIndex, Math.max(0, ids.length - 1));

  // Persisted sidebar width (Issue #94). `undefined` until config hydrates or when the user has never resized — fall back to the legacy default.
  // Live width during a drag is kept in local state so we don't round-trip through `configSet` (and therefore disk) on every pointermove tick.
  const persistedWidth = useConfigStore(selectSidebarWidthPx);
  const configSet = useConfigStore((s) => s.set);
  const [liveWidth, setLiveWidth] = useState<number>(() => clampSidebarWidth(persistedWidth ?? SIDEBAR_DEFAULT_WIDTH_PX));

  // Adopt backend-truth width whenever the persisted value changes (hydrate, workspace switch). Skipping this when a drag is in progress would be
  // ideal, but workspace switches replace the whole sidebar tree anyway so there's no in-flight drag to disturb in practice.
  useEffect(() => {
    setLiveWidth(clampSidebarWidth(persistedWidth ?? SIDEBAR_DEFAULT_WIDTH_PX));
  }, [persistedWidth]);

  // Serialize sidebar-width persistence so rapid commits (e.g. holding ArrowLeft for auto-repeat keyboard nudges) can't issue overlapping
  // `configSet` calls that resolve out of order and leave a stale value on disk. We coalesce: the latest requested width is queued in
  // `pendingWidthRef`; while a write is in flight we just update the ref, and the drain loop picks the latest value when the in-flight call
  // resolves. Pointer-up gestures fire only one commit so the fast path is the no-op-already-equal short-circuit below.
  const pendingWidthRef = useRef<number | null>(null);
  const inFlightWriteRef = useRef<Promise<unknown> | null>(null);

  const commitWidth = useCallback(
    (next: number) => {
      const clamped = clampSidebarWidth(next);
      // Only persist when the value actually changed from what's on disk. Saves a config write on no-op drags / Home / End at the bound.
      if (clamped === (persistedWidth ?? SIDEBAR_DEFAULT_WIDTH_PX)) return;
      pendingWidthRef.current = clamped;
      if (inFlightWriteRef.current) return;
      const drain = async (): Promise<void> => {
        while (pendingWidthRef.current !== null) {
          const value = pendingWidthRef.current;
          pendingWidthRef.current = null;
          try {
            await configSet({ sidebarWidthPx: value });
          } catch (err) {
            console.warn(`[sidebar] failed to persist width ${value}: ${formatError(err)}`);
          }
        }
        inFlightWriteRef.current = null;
      };
      inFlightWriteRef.current = drain();
    },
    [configSet, persistedWidth],
  );

  return (
    <aside
      aria-label="Sessions"
      role="tablist"
      aria-orientation="vertical"
      data-testid="sidebar"
      onKeyDown={onKeyDown}
      style={{ width: `${liveWidth}px` }}
      className="relative flex h-full shrink-0 flex-col border-r border-slate-200 bg-slate-50 dark:border-slate-800 dark:bg-slate-900"
    >
      <WorkspaceIndicator />
      <NewSessionButton buttonRef={newSessionButtonRef} />
      <ul className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto py-1">
        {groups.map((group) => {
          if (group.tabId === null) {
            // Synthetic orphan group — no header; just render the sessions so they remain reachable. Boot self-heal will open a
            // proper tab on the next start. Orphans render flush (no rail) since there's no parent worktree to branch from.
            return group.sessionIds.map((id) => {
              const idx = ids.indexOf(id);
              return (
                <SidebarTab
                  key={id}
                  id={id}
                  isActive={false}
                  isFocused={idx === clampedFocusedIndex}
                  onFocusableMounted={onFocusableMounted}
                  onOpenContextMenu={openContextMenu}
                  nested={false}
                />
              );
            });
          }
          const tabId = group.tabId;
          const isActiveWorktree = tabId === activeWorktreeTabId;
          return (
            <SidebarGroupSection
              key={tabId}
              tabId={tabId}
              isActiveWorktree={isActiveWorktree}
              activeChildSessionId={isActiveWorktree ? activeChildSessionId : undefined}
              sessionIds={group.sessionIds}
              ids={ids}
              clampedFocusedIndex={clampedFocusedIndex}
              onFocusableMounted={onFocusableMounted}
              onOpenContextMenu={openContextMenu}
              onOpenWorktreeContextMenu={openWorktreeContextMenu}
            />
          );
        })}
      </ul>
      <div className="mt-auto flex items-center gap-1 border-t border-slate-200 px-2 py-2 dark:border-slate-800">
        <button
          type="button"
          data-testid="settings-button"
          onClick={() => {
            setSettingsInitialTab('general');
            setSettingsOpen(true);
          }}
          className="flex flex-1 items-center gap-2 rounded px-2 py-1.5 text-xs text-slate-600 hover:bg-slate-200 dark:text-slate-300 dark:hover:bg-slate-800"
        >
          <span aria-hidden="true">⚙</span>
          <span>Settings</span>
        </button>
      </div>
      <WorktreeCloseConfirmDialog />
      <SubCloseConfirmDialog />
      {contextMenu && (
        <TabContextMenu
          parentSessionId={contextMenu.sessionId}
          anchor={contextMenu.anchor}
          onClose={closeContextMenu}
          restoreFocusTo={contextMenu.trigger}
        />
      )}
      {worktreeContextMenu && (
        <WorktreeTabContextMenu
          tabId={worktreeContextMenu.tabId}
          anchor={worktreeContextMenu.anchor}
          onClose={closeWorktreeContextMenu}
          restoreFocusTo={worktreeContextMenu.trigger}
          onOpenSettings={() => {
            setSettingsInitialTab('customProcesses');
            setSettingsOpen(true);
          }}
        />
      )}
      {settingsOpen ? <SettingsDialog onClose={() => setSettingsOpen(false)} initialTab={settingsInitialTab} /> : null}
      <SidebarResizeHandle width={liveWidth} onWidthChange={setLiveWidth} onCommit={commitWidth} />
    </aside>
  );
}

interface SidebarGroupSectionProps {
  tabId: WorktreeTabId;
  isActiveWorktree: boolean;
  activeChildSessionId: SessionId | undefined;
  sessionIds: SessionId[];
  ids: SessionId[];
  clampedFocusedIndex: number;
  onFocusableMounted: (id: SessionId, el: HTMLButtonElement | null) => void;
  onOpenContextMenu: (sessionId: SessionId, anchor: { x: number; y: number }, trigger: HTMLElement | null) => void;
  onOpenWorktreeContextMenu: (tabId: WorktreeTabId, anchor: { x: number; y: number }, trigger: HTMLElement | null) => void;
}

function SidebarGroupSection({
  tabId,
  isActiveWorktree,
  activeChildSessionId,
  sessionIds,
  ids,
  clampedFocusedIndex,
  onFocusableMounted,
  onOpenContextMenu,
  onOpenWorktreeContextMenu,
}: SidebarGroupSectionProps): JSX.Element {
  const subSessions = useSubSessionsForWorktreeTab(tabId);
  // The "last child" of the group is the last sub-session if any are present, otherwise the last AI-session tab. That row gets
  // the "└" elbow on its branch decoration so the rail visibly terminates before the next worktree header.
  const lastSessionIndex = sessionIds.length - 1;
  const lastSubIndex = subSessions.length - 1;
  return (
    <>
      <SidebarWorktreeTab tabId={tabId} isActive={isActiveWorktree} onOpenContextMenu={onOpenWorktreeContextMenu} />
      {sessionIds.map((id, i) => {
        const idx = ids.indexOf(id);
        const isLastInGroup = subSessions.length === 0 && i === lastSessionIndex;
        return (
          <SidebarTab
            key={id}
            id={id}
            isActive={isActiveWorktree && id === activeChildSessionId}
            isFocused={idx === clampedFocusedIndex}
            onFocusableMounted={onFocusableMounted}
            onOpenContextMenu={onOpenContextMenu}
            isLastInGroup={isLastInGroup}
          />
        );
      })}
      {subSessions.map((sub, i) => (
        <SidebarSubTab key={sub.id} subSessionId={sub.id} isLastInGroup={i === lastSubIndex} />
      ))}
    </>
  );
}
