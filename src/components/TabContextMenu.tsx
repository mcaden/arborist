// TabContextMenu — right-click / Shift+F10 / ContextMenu key menu for a
// session tab. Renders through `createPortal` so the menu can escape any
// `overflow: hidden` ancestors in the sidebar.
//
// Items:
//   * Restart        → invokes `session_restart` for the parent.
//   * Close          → invokes the existing close-confirm flow.
//   * Launch ▸       → submenu listing enabled custom processes; clicking
//                      one creates a sub-session under this parent. Empty
//                      state offers a "Manage in Settings…" link.
//
// Keyboard model:
//   * ↑/↓             move between items (wrapping).
//   * Enter / Space   activate the focused item.
//   * → / Enter       open submenu when focused on Launch.
//   * ←               close submenu (restoring focus to Launch).
//   * Escape          close the menu (and restore focus to the tab).
//   * Tab             closes the menu (don't let focus wander silently).
//
// Closes on: outside pointer-down, Escape, Tab (don't let focus wander
// silently), and after activating any item. Restores DOM focus to the
// triggering tab on close, except when handing off to the Settings
// dialog (Settings will take focus management itself).

import { forwardRef, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

import { getTerminalDimensions, measureInitialPtyDimensions } from '@/hooks/use-terminal';
import { formatError, sessionRestart } from '@/lib/tauri-bridge';
import { useEnabledCustomProcesses } from '@/store/config-store';
import { useSessionActions } from '@/store/session-store';
import { useSubSessionActions } from '@/store/sub-session-store';
import type { CustomProcessDef, SessionId } from '@/types/arborist';

export interface TabContextMenuProps {
  /** The tab the menu was opened from. */
  parentSessionId: SessionId;
  /** Pixel coordinates (viewport-relative) where the menu should open. */
  anchor: { x: number; y: number };
  /** Called when the menu wants to close itself. */
  onClose: () => void;
  /**
   * Element that should receive focus after the menu closes. Usually the
   * triggering tab button. Skipped when `onOpenSettings` is invoked so
   * the Settings dialog can manage focus itself.
   */
  restoreFocusTo?: HTMLElement | null;
  /**
   * Invoked when the user picks "Manage in Settings…" from the empty
   * Launch submenu. The parent component is responsible for opening the
   * Settings dialog.
   */
  onOpenSettings?: () => void;
}

type Item = 'restart' | 'close' | 'launch';
const ITEM_ORDER: Item[] = ['restart', 'close', 'launch'];

export function TabContextMenu({ parentSessionId, anchor, onClose, restoreFocusTo, onOpenSettings }: TabContextMenuProps): JSX.Element {
  const sessionActions = useSessionActions();
  const subActions = useSubSessionActions();
  const customProcesses = useEnabledCustomProcesses();

  const menuRef = useRef<HTMLDivElement | null>(null);
  const submenuRef = useRef<HTMLDivElement | null>(null);
  const itemRefs = useRef<Record<Item, HTMLButtonElement | null>>({
    restart: null,
    close: null,
    launch: null,
  });
  const submenuItemRefs = useRef<Array<HTMLButtonElement | null>>([]);

  const [focusedItem, setFocusedItem] = useState<Item>('restart');
  const [submenuOpen, setSubmenuOpen] = useState<boolean>(false);
  const [submenuFocusedIdx, setSubmenuFocusedIdx] = useState<number>(0);

  // If the menu activates a destination that wants focus (e.g. Settings),
  // the closing path skips `restoreFocusTo`. We carry that decision here
  // so any close path (Esc, outside-click) defaults to restoring focus.
  const skipFocusRestoreRef = useRef<boolean>(false);

  const closeMenu = useCallback((): void => {
    const skipRestore = skipFocusRestoreRef.current;
    onClose();
    if (!skipRestore && restoreFocusTo) {
      // Defer so the menu's portal teardown completes first, otherwise
      // focus() can race against React unmounting and not stick.
      requestAnimationFrame(() => {
        restoreFocusTo.focus();
      });
    }
  }, [onClose, restoreFocusTo]);

  // Focus the first item when the menu mounts.
  useEffect(() => {
    itemRefs.current.restart?.focus();
  }, []);

  // Outside pointer-down dismisses the menu. We listen on `mousedown` so
  // dismissal precedes any click on a sidebar tab — otherwise the tab's
  // click handler would re-focus the parent before our `onClose` runs.
  useEffect(() => {
    function onPointerDown(e: MouseEvent): void {
      const target = e.target as Node | null;
      if (!target) return;
      if (menuRef.current?.contains(target)) return;
      if (submenuRef.current?.contains(target)) return;
      closeMenu();
    }
    document.addEventListener('mousedown', onPointerDown);
    return () => document.removeEventListener('mousedown', onPointerDown);
  }, [closeMenu]);

  // Position guard — clamp inside the viewport so the menu never opens
  // off-screen on right-click near the edges.
  const position = useMemo(() => {
    const margin = 4;
    const estW = 220;
    const estH = 140;
    const vw = typeof window !== 'undefined' ? window.innerWidth : estW * 2;
    const vh = typeof window !== 'undefined' ? window.innerHeight : estH * 2;
    return {
      left: Math.min(Math.max(margin, anchor.x), Math.max(margin, vw - estW - margin)),
      top: Math.min(Math.max(margin, anchor.y), Math.max(margin, vh - estH - margin)),
    };
  }, [anchor.x, anchor.y]);

  const focusItem = (next: Item): void => {
    setFocusedItem(next);
    itemRefs.current[next]?.focus();
  };

  const moveFocus = (delta: 1 | -1): void => {
    const idx = ITEM_ORDER.indexOf(focusedItem);
    const nextIdx = (idx + delta + ITEM_ORDER.length) % ITEM_ORDER.length;
    const next = ITEM_ORDER[nextIdx];
    if (next) focusItem(next);
  };

  const openSubmenu = (): void => {
    setSubmenuOpen(true);
    setSubmenuFocusedIdx(0);
    requestAnimationFrame(() => {
      submenuItemRefs.current[0]?.focus();
    });
  };

  const closeSubmenu = (): void => {
    setSubmenuOpen(false);
    focusItem('launch');
  };

  const handleRestart = (): void => {
    // Reuse the active terminal's measured cols/rows so the restarted PTY
    // child paints at the size xterm currently shows. Falls back to a
    // fresh measurement if the terminal isn't attached.
    const dims = getTerminalDimensions(parentSessionId) ?? measureInitialPtyDimensions();
    void sessionRestart({ sessionId: parentSessionId, cols: dims.cols, rows: dims.rows }).catch((err: unknown) => {
      const message = formatError(err);
      console.warn(`[TabContextMenu] session_restart failed: ${message}`);
    });
    closeMenu();
  };

  const handleClose = (): void => {
    sessionActions.requestClose(parentSessionId);
    closeMenu();
  };

  const handleLaunch = (def: CustomProcessDef): void => {
    void subActions
      .create({
        parentSessionId,
        defId: def.id,
      })
      .then((sub) => {
        // For terminal kind, make the freshly-created sub-tab the
        // visible viewport. That requires both:
        //   1. activating the parent session (so MainArea picks its
        //      pane to render — without this, if no parent or another
        //      parent is active, the new sub stays hidden); and
        //   2. focusing the sub itself so it becomes the viewport
        //      owner under that parent.
        // Mirrors `SidebarSubTab.handleClick`'s pattern of "focus
        // parent if not already active, then focus the sub".
        // Application kind is intentionally left alone — launching an
        // app pops its own OS window; we don't yank in-app focus.
        if (def.kind === 'terminal') {
          void sessionActions.focus(parentSessionId);
          void subActions.focus(sub.id);
        }
      })
      .catch((err: unknown) => {
        const message = formatError(err);
        console.warn(`[TabContextMenu] subsession_create(${def.id}) failed: ${message}`);
      });
    closeMenu();
  };

  const handleOpenSettings = (): void => {
    if (!onOpenSettings) {
      closeMenu();
      return;
    }
    // Settings will manage its own focus; skip the tab restore so we
    // don't pull focus back to the sidebar. Defer the call so the menu
    // unmounts cleanly before Settings opens.
    skipFocusRestoreRef.current = true;
    closeMenu();
    requestAnimationFrame(() => {
      onOpenSettings();
    });
  };

  const onMenuKeyDown = (e: React.KeyboardEvent<HTMLDivElement>): void => {
    if (submenuOpen) return; // Submenu owns its own keystrokes.
    switch (e.key) {
      case 'Escape':
        e.preventDefault();
        closeMenu();
        break;
      case 'Tab':
        // Don't let focus escape silently — close the menu and restore.
        e.preventDefault();
        closeMenu();
        break;
      case 'ArrowDown':
        e.preventDefault();
        moveFocus(1);
        break;
      case 'ArrowUp':
        e.preventDefault();
        moveFocus(-1);
        break;
      case 'ArrowRight':
        if (focusedItem === 'launch') {
          e.preventDefault();
          openSubmenu();
        }
        break;
      case 'Enter':
      case ' ':
        if (focusedItem === 'launch') {
          e.preventDefault();
          openSubmenu();
        }
        break;
      default:
    }
  };

  const onSubmenuKeyDown = (e: React.KeyboardEvent<HTMLDivElement>): void => {
    const total = customProcesses.length === 0 ? 1 : customProcesses.length;
    switch (e.key) {
      case 'Escape':
        e.preventDefault();
        closeMenu();
        break;
      case 'Tab':
        e.preventDefault();
        closeMenu();
        break;
      case 'ArrowLeft':
        e.preventDefault();
        closeSubmenu();
        break;
      case 'ArrowDown': {
        e.preventDefault();
        const next = (submenuFocusedIdx + 1) % total;
        setSubmenuFocusedIdx(next);
        submenuItemRefs.current[next]?.focus();
        break;
      }
      case 'ArrowUp': {
        e.preventDefault();
        const next = (submenuFocusedIdx - 1 + total) % total;
        setSubmenuFocusedIdx(next);
        submenuItemRefs.current[next]?.focus();
        break;
      }
      default:
    }
  };

  const portalTarget = typeof document !== 'undefined' ? document.body : null;
  if (!portalTarget) return <></>;

  return createPortal(
    <>
      <div
        ref={menuRef}
        role="menu"
        aria-label="Session actions"
        data-testid="tab-context-menu"
        onKeyDown={onMenuKeyDown}
        style={{ position: 'fixed', left: position.left, top: position.top, zIndex: 1000 }}
        className="min-w-[180px] rounded border border-slate-200 bg-white py-1 text-sm text-slate-800 shadow-lg dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
      >
        <MenuItem
          ref={(el) => (itemRefs.current.restart = el)}
          onClick={handleRestart}
          isFocused={focusedItem === 'restart'}
          onMouseEnter={() => focusItem('restart')}
        >
          Restart
        </MenuItem>
        <MenuItem
          ref={(el) => (itemRefs.current.close = el)}
          onClick={handleClose}
          isFocused={focusedItem === 'close'}
          onMouseEnter={() => focusItem('close')}
        >
          Close…
        </MenuItem>
        <MenuItem
          ref={(el) => (itemRefs.current.launch = el)}
          onClick={openSubmenu}
          isFocused={focusedItem === 'launch'}
          onMouseEnter={() => {
            focusItem('launch');
          }}
          aria-haspopup="menu"
          aria-expanded={submenuOpen}
          rightAdornment="▸"
        >
          Launch
        </MenuItem>
      </div>
      {submenuOpen && (
        <div
          ref={submenuRef}
          role="menu"
          aria-label="Launch process"
          data-testid="tab-context-menu-launch"
          onKeyDown={onSubmenuKeyDown}
          style={{
            position: 'fixed',
            // Open to the right of the parent menu; clamp similarly.
            left: Math.min(position.left + 200, (typeof window !== 'undefined' ? window.innerWidth : 1024) - 200),
            top: position.top + 60,
            zIndex: 1001,
          }}
          className="min-w-[200px] rounded border border-slate-200 bg-white py-1 text-sm text-slate-800 shadow-lg dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
        >
          {customProcesses.length === 0 ? (
            <button
              ref={(el) => {
                submenuItemRefs.current[0] = el;
              }}
              type="button"
              role="menuitem"
              data-testid="tab-context-menu-empty"
              onClick={handleOpenSettings}
              className="block w-full px-3 py-1.5 text-left text-xs text-slate-500 hover:bg-slate-100 focus:bg-slate-100 focus:outline-none dark:text-slate-400 dark:hover:bg-slate-700 dark:focus:bg-slate-700"
            >
              No custom processes — open Settings…
            </button>
          ) : (
            customProcesses.map((def, idx) => (
              <button
                key={def.id}
                ref={(el) => {
                  submenuItemRefs.current[idx] = el;
                }}
                type="button"
                role="menuitem"
                data-testid={`tab-context-menu-launch-${def.id}`}
                onClick={() => handleLaunch(def)}
                onMouseEnter={() => setSubmenuFocusedIdx(idx)}
                className="flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left hover:bg-slate-100 focus:bg-slate-100 focus:outline-none dark:hover:bg-slate-700 dark:focus:bg-slate-700"
              >
                <span className="truncate">{def.name}</span>
                <span aria-hidden="true" className="text-xs uppercase tracking-wide text-slate-400 dark:text-slate-500">
                  {def.kind === 'application' ? 'app' : 'term'}
                </span>
              </button>
            ))
          )}
        </div>
      )}
    </>,
    portalTarget,
  );
}

interface MenuItemProps {
  onClick: () => void;
  isFocused: boolean;
  onMouseEnter?: () => void;
  rightAdornment?: string;
  children: React.ReactNode;
  'aria-haspopup'?: 'menu';
  'aria-expanded'?: boolean;
}

const MenuItem = forwardRef<HTMLButtonElement, MenuItemProps>(function MenuItem(props, ref): JSX.Element {
  const { onClick, isFocused, onMouseEnter, rightAdornment, children, ...aria } = props;
  return (
    <button
      ref={ref}
      type="button"
      role="menuitem"
      tabIndex={isFocused ? 0 : -1}
      onClick={onClick}
      onMouseEnter={onMouseEnter}
      className="flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left hover:bg-slate-100 focus:bg-slate-100 focus:outline-none dark:hover:bg-slate-700 dark:focus:bg-slate-700"
      {...aria}
    >
      <span>{children}</span>
      {rightAdornment !== undefined && (
        <span aria-hidden="true" className="text-slate-400">
          {rightAdornment}
        </span>
      )}
    </button>
  );
});
