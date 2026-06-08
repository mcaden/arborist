// SubTabContextMenu — ⋮-button menu for a sub-session tab (issue #49).
// Mirrors `TabContextMenu` for AI sessions: Restart + Close items, portal-
// rendered so it escapes overflow, viewport clamped, keyboard navigable
// (↑/↓, Enter/Space, Escape, Tab).
//
// Restart maps to `subActions.relaunch` (kills the underlying process and
// re-spawns it under the same id). Close mirrors the inline × button:
// running app sub-sessions go through `requestClose` so the user gets the
// confirmation dialog about leaving the OS window open; everything else
// closes immediately.

import { forwardRef, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

import { formatError } from '@/lib/tauri-bridge';
import { useSubSessionActions, useSubSessionById } from '@/store/sub-session-store';
import type { SubSessionId } from '@/types/arborist';

export interface SubTabContextMenuProps {
  subSessionId: SubSessionId;
  anchor: { x: number; y: number };
  onClose: () => void;
  restoreFocusTo?: HTMLElement | null;
}

type Item = 'restart' | 'close';
const ITEM_ORDER: Item[] = ['restart', 'close'];

export function SubTabContextMenu({ subSessionId, anchor, onClose, restoreFocusTo }: SubTabContextMenuProps): JSX.Element {
  const sub = useSubSessionById(subSessionId);
  const subActions = useSubSessionActions();

  const menuRef = useRef<HTMLDivElement | null>(null);
  const itemRefs = useRef<Record<Item, HTMLButtonElement | null>>({ restart: null, close: null });
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
    subActions.relaunch(subSessionId).catch((err: unknown) => {
      console.warn(`[SubTabContextMenu] subsession_relaunch failed: ${formatError(err)}`);
    });
    closeMenu();
  };

  const handleClose = (): void => {
    // Match the inline × button: an app that's still running needs the
    // confirm dialog (so the user decides whether to keep the OS window
    // open); everything else closes immediately.
    if (sub && sub.kind === 'application' && sub.status !== 'exited' && sub.status !== 'error') {
      subActions.requestClose(subSessionId);
    } else {
      subActions.close(subSessionId).catch((err: unknown) => {
        console.warn(`[SubTabContextMenu] subsession_close failed: ${formatError(err)}`);
      });
    }
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
      aria-label="Sub-session actions"
      data-testid="sub-tab-context-menu"
      onKeyDown={onMenuKeyDown}
      style={{ position: 'fixed', left: position.left, top: position.top, zIndex: 1000 }}
      className="min-w-[180px] rounded border border-slate-200 bg-white py-1 text-sm text-slate-800 shadow-lg dark:border-slate-700 dark:bg-slate-800 dark:text-slate-100"
    >
      <MenuItem
        ref={(el) => {
          itemRefs.current.restart = el;
        }}
        onClick={handleRestart}
        isFocused={focusedItem === 'restart'}
        onMouseEnter={() => focusItem('restart')}
      >
        Restart
      </MenuItem>
      <MenuItem
        ref={(el) => {
          itemRefs.current.close = el;
        }}
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
  children: React.ReactNode;
}

const MenuItem = forwardRef<HTMLButtonElement, MenuItemProps>(function MenuItem(props, ref): JSX.Element {
  const { onClick, isFocused, onMouseEnter, children } = props;
  return (
    <button
      ref={ref}
      type="button"
      role="menuitem"
      tabIndex={isFocused ? 0 : -1}
      onClick={onClick}
      onMouseEnter={onMouseEnter}
      className="flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left hover:bg-slate-100 focus:bg-slate-100 focus:outline-none dark:hover:bg-slate-700 dark:focus:bg-slate-700"
    >
      <span>{children}</span>
    </button>
  );
});
