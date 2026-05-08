// TabContextMenu — right-click / Shift+F10 / ContextMenu key menu for a
// session tab. Renders through `createPortal` so the menu can escape any
// `overflow: hidden` ancestors in the sidebar.
//
// Items:
//   * Restart        → invokes `session_restart` for the parent.
//   * Close          → invokes the existing close-confirm flow.
//
// Custom-process launching moved to WorktreeTabContextMenu (sub-sessions
// are now owned by the worktree tab, not by agent sessions).
//
// Keyboard model:
//   * ↑/↓             move between items (wrapping).
//   * Enter / Space   activate the focused item.
//   * Escape          close the menu (and restore focus to the tab).
//   * Tab             closes the menu (don't let focus wander silently).

import { forwardRef, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

import { getTerminalDimensions, measureInitialPtyDimensions } from '@/hooks/use-terminal';
import { formatError, sessionRestart } from '@/lib/tauri-bridge';
import { useSessionActions } from '@/store/session-store';
import type { SessionId } from '@/types/arborist';

export interface TabContextMenuProps {
  /** The tab the menu was opened from. */
  parentSessionId: SessionId;
  /** Pixel coordinates (viewport-relative) where the menu should open. */
  anchor: { x: number; y: number };
  /** Called when the menu wants to close itself. */
  onClose: () => void;
  /**
   * Element that should receive focus after the menu closes. Usually the
   * triggering tab button.
   */
  restoreFocusTo?: HTMLElement | null;
}

type Item = 'restart' | 'close';
const ITEM_ORDER: Item[] = ['restart', 'close'];

export function TabContextMenu({ parentSessionId, anchor, onClose, restoreFocusTo }: TabContextMenuProps): JSX.Element {
  const sessionActions = useSessionActions();

  const menuRef = useRef<HTMLDivElement | null>(null);
  const itemRefs = useRef<Record<Item, HTMLButtonElement | null>>({
    restart: null,
    close: null,
  });

  const [focusedItem, setFocusedItem] = useState<Item>('restart');

  const closeMenu = useCallback((): void => {
    onClose();
    if (restoreFocusTo) {
      requestAnimationFrame(() => {
        restoreFocusTo.focus();
      });
    }
  }, [onClose, restoreFocusTo]);

  useEffect(() => {
    itemRefs.current.restart?.focus();
  }, []);

  useEffect(() => {
    function onPointerDown(e: MouseEvent): void {
      const target = e.target as Node | null;
      if (!target) return;
      if (menuRef.current?.contains(target)) return;
      closeMenu();
    }
    document.addEventListener('mousedown', onPointerDown);
    return () => document.removeEventListener('mousedown', onPointerDown);
  }, [closeMenu]);

  const position = useMemo(() => {
    const margin = 4;
    const estW = 220;
    const estH = 100;
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

  const handleRestart = (): void => {
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

  const onMenuKeyDown = (e: React.KeyboardEvent<HTMLDivElement>): void => {
    switch (e.key) {
      case 'Escape':
      case 'Tab':
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
      default:
    }
  };

  const portalTarget = typeof document !== 'undefined' ? document.body : null;
  if (!portalTarget) return <></>;

  return createPortal(
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
    </div>,
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
