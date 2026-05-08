// WorktreeTabContextMenu — right-click menu for a worktree tab (issue #44).
//
// Items (flat, no submenu):
//   * Launch Claude  → creates a Claude session under this worktree tab.
//   * Launch Copilot → creates a Copilot session under this worktree tab.
//   * <custom defs>  → one entry per enabled custom-process definition.
//   * Settings…      → opens the Settings dialog on the Custom Processes tab.
//   * Close          → cascades close of the worktree tab and all children (pinned to bottom).
//
// Keyboard model: ↑/↓ cycles items, Enter activates, Esc closes and
// restores focus to the trigger.

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

import { measureInitialPtyDimensions } from '@/hooks/use-terminal';
import { formatError } from '@/lib/tauri-bridge';
import { useEnabledCustomProcesses, useConfigStore } from '@/store/config-store';
import { useSessionActions } from '@/store/session-store';
import { useSubSessionActions } from '@/store/sub-session-store';
import { useWorktreeTabActions, useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { CustomProcessDefId, Tool, WorktreeTabId } from '@/types/arborist';

export interface WorktreeTabContextMenuProps {
  tabId: WorktreeTabId;
  anchor: { x: number; y: number };
  onClose: () => void;
  /** Element that should receive focus after the menu closes. Usually the worktree-tab header button. */
  restoreFocusTo?: HTMLElement | null;
  /** Open the Settings dialog (on the Custom Processes tab). */
  onOpenSettings?: () => void;
}

type Item = 'close' | 'launch-claude' | 'launch-copilot' | 'settings' | `cp:${string}`;

export function WorktreeTabContextMenu({ tabId, anchor, onClose, restoreFocusTo, onOpenSettings }: WorktreeTabContextMenuProps): JSX.Element | null {
  const tab = useWorktreeTabStore((s) => s.tabs.find((t) => t.id === tabId));
  const wttActions = useWorktreeTabActions();
  const sessionActions = useSessionActions();
  const subActions = useSubSessionActions();
  const customProcesses = useEnabledCustomProcesses();
  const claudeIconDataUri = useConfigStore((s) => s.config.aiLaunchCommands.claudeIconDataUri);
  const copilotIconDataUri = useConfigStore((s) => s.config.aiLaunchCommands.copilotIconDataUri);

  // Build the full item order: Launch Claude, Launch Copilot, custom
  // processes, Settings…, then Close pinned at the bottom.
  const itemOrder = useMemo<Item[]>(() => {
    const items: Item[] = ['launch-claude', 'launch-copilot'];
    if (customProcesses.length > 0) {
      for (const def of customProcesses) {
        items.push(`cp:${def.id}` as Item);
      }
    }
    items.push('settings', 'close');
    return items;
  }, [customProcesses]);

  const menuRef = useRef<HTMLDivElement | null>(null);
  const itemRefs = useRef<Map<Item, HTMLButtonElement | null>>(new Map());

  const [focusedItem, setFocusedItem] = useState<Item>('launch-claude');

  const closeMenu = useCallback((): void => {
    onClose();
    if (restoreFocusTo) {
      requestAnimationFrame(() => restoreFocusTo.focus());
    }
  }, [onClose, restoreFocusTo]);

  // Focus the first item when the menu mounts.
  useEffect(() => {
    itemRefs.current.get('launch-claude')?.focus();
  }, []);

  // Outside pointer-down dismisses.
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

  useEffect(() => {
    if (!tab) closeMenu();
  }, [tab, closeMenu]);

  const position = useMemo(() => {
    const margin = 4;
    const estW = 220;
    const separatorCount = 2;
    const estH = itemOrder.length * 32 + separatorCount * 9 + 8;
    const vw = typeof window !== 'undefined' ? window.innerWidth : estW * 2;
    const vh = typeof window !== 'undefined' ? window.innerHeight : estH * 2;
    return {
      left: Math.min(Math.max(margin, anchor.x), Math.max(margin, vw - estW - margin)),
      top: Math.min(Math.max(margin, anchor.y), Math.max(margin, vh - estH - margin)),
    };
  }, [anchor.x, anchor.y, itemOrder.length]);

  const focusItem = (next: Item): void => {
    setFocusedItem(next);
    itemRefs.current.get(next)?.focus();
  };

  const moveFocus = (delta: 1 | -1): void => {
    const idx = itemOrder.indexOf(focusedItem);
    const nextIdx = (idx + delta + itemOrder.length) % itemOrder.length;
    const next = itemOrder[nextIdx];
    if (next) focusItem(next);
  };

  const handleClose = (): void => {
    void wttActions.close(tabId).catch((err) => {
      console.warn(`[WorktreeTabContextMenu] close(${tabId}) failed: ${formatError(err)}`);
    });
    closeMenu();
  };

  const handleLaunch = (tool: Tool): void => {
    if (!tab) {
      closeMenu();
      return;
    }
    const dims = measureInitialPtyDimensions();
    void sessionActions
      .create({
        tool,
        worktreePath: tab.path,
        cols: dims.cols,
        rows: dims.rows,
      })
      .catch((err: unknown) => {
        console.warn(`[WorktreeTabContextMenu] session_create(${tool}) failed: ${formatError(err)}`);
      });
    closeMenu();
  };

  const handleCustomProcess = (defId: string): void => {
    void subActions.create({ parentWorktreeTabId: tabId, defId: defId as CustomProcessDefId }).catch((err: unknown) => {
      console.warn(`[WorktreeTabContextMenu] subsession_create(${defId}) failed: ${formatError(err)}`);
    });
    closeMenu();
  };

  const handleSettings = (): void => {
    closeMenu();
    onOpenSettings?.();
  };

  const activateItem = (item: Item): void => {
    if (item === 'close') handleClose();
    else if (item === 'launch-claude') handleLaunch('claude');
    else if (item === 'launch-copilot') handleLaunch('copilot');
    else if (item === 'settings') handleSettings();
    else if (item.startsWith('cp:')) handleCustomProcess(item.slice(3));
  };

  const itemForElement = (el: EventTarget | null): Item | undefined => {
    for (const item of itemOrder) {
      if (itemRefs.current.get(item) === el) return item;
    }
    return undefined;
  };

  if (!tab) return null;

  const itemBase =
    'flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm text-slate-700 hover:bg-slate-100 focus:bg-slate-100 focus:outline-none dark:text-slate-200 dark:hover:bg-slate-700 dark:focus:bg-slate-700';

  const setItemRef = (key: Item) => (el: HTMLButtonElement | null) => {
    itemRefs.current.set(key, el);
  };

  const menu = (
    <div
      ref={menuRef}
      role="menu"
      aria-label={`Worktree tab actions for ${tab.name}`}
      data-testid="worktree-tab-context-menu"
      style={{ position: 'fixed', left: position.left, top: position.top, minWidth: 200, zIndex: 60 }}
      className="rounded-md border border-slate-200 bg-white py-1 shadow-lg dark:border-slate-700 dark:bg-slate-800"
      onKeyDown={(e) => {
        if (e.key === 'Escape') {
          e.preventDefault();
          closeMenu();
          return;
        }
        if (e.key === 'Tab') {
          e.preventDefault();
          closeMenu();
          return;
        }
        switch (e.key) {
          case 'ArrowDown':
            e.preventDefault();
            moveFocus(1);
            break;
          case 'ArrowUp':
            e.preventDefault();
            moveFocus(-1);
            break;
          case 'Enter':
          case ' ':
            e.preventDefault();
            activateItem(itemForElement(e.target) ?? focusedItem);
            break;
          default:
        }
      }}
    >
      <button
        ref={setItemRef('launch-claude')}
        type="button"
        role="menuitem"
        data-testid="worktree-tab-context-menu-launch-claude"
        onClick={() => handleLaunch('claude')}
        onMouseEnter={() => setFocusedItem('launch-claude')}
        className={itemBase}
      >
        <MenuIcon src={claudeIconDataUri} fallback="🤖" />
        <span>Launch Claude</span>
      </button>
      <button
        ref={setItemRef('launch-copilot')}
        type="button"
        role="menuitem"
        data-testid="worktree-tab-context-menu-launch-copilot"
        onClick={() => handleLaunch('copilot')}
        onMouseEnter={() => setFocusedItem('launch-copilot')}
        className={itemBase}
      >
        <MenuIcon src={copilotIconDataUri} fallback="🤖" />
        <span>Launch Copilot</span>
      </button>
      {customProcesses.length > 0 &&
        customProcesses.map((def) => {
          const key: Item = `cp:${def.id}` as Item;
          return (
            <button
              key={def.id}
              ref={setItemRef(key)}
              type="button"
              role="menuitem"
              data-testid={`worktree-tab-context-menu-cp-${def.id}`}
              onClick={() => handleCustomProcess(def.id)}
              onMouseEnter={() => setFocusedItem(key)}
              className={itemBase}
            >
              <MenuIcon src={def.iconDataUri} fallback="⌗" />
              <span>{def.name}</span>
            </button>
          );
        })}
      <div role="separator" className="my-1 border-t border-slate-200 dark:border-slate-700" />
      <button
        ref={setItemRef('settings')}
        type="button"
        role="menuitem"
        data-testid="worktree-tab-context-menu-settings"
        onClick={handleSettings}
        onMouseEnter={() => setFocusedItem('settings')}
        className={itemBase}
      >
        <span>Custom Processes…</span>
      </button>
      <div role="separator" className="my-1 border-t border-slate-200 dark:border-slate-700" />
      <button
        ref={setItemRef('close')}
        type="button"
        role="menuitem"
        data-testid="worktree-tab-context-menu-close"
        onClick={handleClose}
        onMouseEnter={() => setFocusedItem('close')}
        className={itemBase}
      >
        <span>Close worktree tab</span>
      </button>
    </div>
  );

  return createPortal(menu, document.body);
}

/** Tiny icon for menu items — renders the cached data URI when available, else a text fallback. */
function MenuIcon({ src, fallback }: { src: string | undefined; fallback: string }): JSX.Element {
  if (src) {
    return <img src={src} alt="" aria-hidden="true" className="h-4 w-4 shrink-0 rounded-sm object-contain" />;
  }
  return (
    <span aria-hidden="true" className="inline-flex h-4 w-4 shrink-0 items-center justify-center text-xs text-slate-400">
      {fallback}
    </span>
  );
}
