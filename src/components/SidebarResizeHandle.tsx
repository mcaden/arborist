// Hover-revealed drag handle for the left sidebar's right edge (Issue #94).
//
// Renders an 8 px-wide invisible hit zone overlaid on the sidebar's right
// edge, with a 1-2 px accent line that fades in on hover or while a drag is
// in progress. Live width updates go through the `onWidthChange` callback so
// the parent can mutate its rendered width without re-rendering the whole
// sidebar tree on every `pointermove`. Persistence (and clamping) happens
// once on pointer-up via `onCommit`.
//
// Accessibility: `role="separator"` + `aria-orientation="vertical"` matches
// the WAI-ARIA window-splitter pattern. The handle is focusable; ArrowLeft /
// ArrowRight nudge by `KEYBOARD_STEP_PX` (16 px), Home / End snap to the min
// / max bounds. Double-click resets to `DEFAULT_WIDTH_PX` (224 px).

import { useCallback, useRef, useState, type CSSProperties, type KeyboardEvent, type PointerEvent } from 'react';

import { clampSidebarWidth, DEFAULT_WIDTH_PX, KEYBOARD_STEP_PX, MAX_WIDTH_PX, MIN_WIDTH_PX } from './sidebar-width';

function computeKeyboardWidth(key: string, current: number): number | null {
  switch (key) {
    case 'ArrowLeft':
      return clampSidebarWidth(current - KEYBOARD_STEP_PX);
    case 'ArrowRight':
      return clampSidebarWidth(current + KEYBOARD_STEP_PX);
    case 'Home':
      return MIN_WIDTH_PX;
    case 'End':
      return MAX_WIDTH_PX;
    default:
      return null;
  }
}

interface SidebarResizeHandleProps {
  /** Current rendered width (px). Drives the `aria-valuenow` so screen readers track live drags. */
  width: number;
  /** Live update while dragging or after a keyboard nudge — caller mutates its rendered width. */
  onWidthChange: (next: number) => void;
  /**
   * Called when the user finishes a gesture (pointer up, keyboard release, double-click reset). Caller persists the value. Skipped during in-flight
   * pointer move so we don't hit disk on every tick.
   */
  onCommit: (next: number) => void;
}

export function SidebarResizeHandle({ width, onWidthChange, onCommit }: SidebarResizeHandleProps): JSX.Element {
  // Drag bookkeeping. `pointerId` doubles as "is a drag in progress?" — we capture the pointer so the user can drag past the handle
  // without losing it under fast movement, and release on pointer up / cancel.
  const dragRef = useRef<{ pointerId: number; startX: number; startWidth: number } | null>(null);
  // Mirror dragging state into React state so the `data-dragging` attribute and the accent-line opacity update on `pointerdown` without
  // waiting for an unrelated re-render. We still keep the `dragRef` for the start coordinates so move handlers don't re-bind on every render.
  const [isDragging, setIsDragging] = useState<boolean>(false);
  // Mirror the latest *prop-driven* width into a ref so pointermove can read the freshest committed value without re-binding on every render.
  const widthRef = useRef<number>(width);
  widthRef.current = width;
  // Track the last width we computed during the current gesture. We can't read this from `widthRef` because the parent re-render hasn't necessarily
  // flushed by the time `pointerup` fires — React 18 batches state updates across pointermove/pointerup in the same task. Committing
  // `widthRef.current` from `finishDrag` would therefore risk snapping back to the *pre-drag* width on fast releases.
  const lastComputedRef = useRef<number>(width);
  lastComputedRef.current = width;

  const onPointerDown = useCallback((e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    dragRef.current = { pointerId: e.pointerId, startX: e.clientX, startWidth: widthRef.current };
    setIsDragging(true);
  }, []);

  const onPointerMove = useCallback(
    (e: PointerEvent<HTMLDivElement>) => {
      const drag = dragRef.current;
      if (!drag || drag.pointerId !== e.pointerId) return;
      const next = clampSidebarWidth(drag.startWidth + (e.clientX - drag.startX));
      // Capture the freshly-computed width *before* notifying the parent. `finishDrag` reads this on pointerup; reading `widthRef.current` there
      // would race React 18's batched re-renders — pointermove + pointerup can land in the same task with no flush in between, leaving the prop
      // (and therefore `widthRef`) at the pre-drag value.
      lastComputedRef.current = next;
      if (next !== widthRef.current) onWidthChange(next);
    },
    [onWidthChange],
  );

  const finishDrag = useCallback(
    (e: PointerEvent<HTMLDivElement>) => {
      const drag = dragRef.current;
      if (!drag || drag.pointerId !== e.pointerId) return;
      dragRef.current = null;
      setIsDragging(false);
      if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
      // Commit even when width is unchanged from drag start — keeps the contract simple (one commit per gesture) and the no-op write is cheap.
      onCommit(lastComputedRef.current);
    },
    [onCommit],
  );

  const onDoubleClick = useCallback(() => {
    // Defensive: cancel any in-flight gesture so a trailing pointerup can't overwrite the reset with the last dragged width. In normal DOM event
    // ordering `dblclick` only fires after two completed pointerup cycles, so `dragRef` should already be null — but releasing pointer capture and
    // resetting the computed-width ref keeps the invariant unconditional.
    dragRef.current = null;
    setIsDragging(false);
    lastComputedRef.current = DEFAULT_WIDTH_PX;
    onWidthChange(DEFAULT_WIDTH_PX);
    onCommit(DEFAULT_WIDTH_PX);
  }, [onCommit, onWidthChange]);

  const onKeyDown = useCallback(
    (e: KeyboardEvent<HTMLDivElement>) => {
      const next = computeKeyboardWidth(e.key, widthRef.current);
      if (next === null) return;
      e.preventDefault();
      if (next !== widthRef.current) onWidthChange(next);
      onCommit(next);
    },
    [onCommit, onWidthChange],
  );

  const isDraggingNow = isDragging;

  const accentStyle: CSSProperties = {
    // Width of the visible accent line; the hit zone (parent) stays 8 px wide so the click target is comfortable even when the line is 1 px.
    width: '1px',
  };

  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize sidebar"
      aria-valuemin={MIN_WIDTH_PX}
      aria-valuemax={MAX_WIDTH_PX}
      aria-valuenow={width}
      tabIndex={0}
      data-testid="sidebar-resize-handle"
      data-dragging={isDraggingNow ? 'true' : 'false'}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={finishDrag}
      onPointerCancel={finishDrag}
      onDoubleClick={onDoubleClick}
      onKeyDown={onKeyDown}
      // `group` lets the inner accent line react to hover on the larger hit zone. `touch-none` keeps mobile pointer drags from
      // turning into scrolls. The 8 px width gives a forgiving grab target while the inner line stays visually quiet.
      className="group absolute right-0 top-0 z-10 flex h-full w-2 translate-x-1/2 cursor-col-resize touch-none items-stretch justify-center focus:outline-none focus-visible:ring-2 focus-visible:ring-blue-400 dark:focus-visible:ring-blue-500"
    >
      <div
        aria-hidden="true"
        style={accentStyle}
        // `data-dragging` on the parent forces the accent visible during an active drag so the user keeps a clear reference even
        // when the pointer wanders away from the handle.
        className="h-full bg-slate-300 opacity-0 transition-opacity group-hover:opacity-100 group-data-[dragging=true]:opacity-100 dark:bg-slate-700"
      />
    </div>
  );
}
