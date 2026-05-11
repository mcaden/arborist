// Issue #94 — resizable left sidebar. We test the handle component in
// isolation rather than driving it through the full Sidebar so jsdom doesn't
// have to fake `pointerCapture` against the production tab tree.

import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { SidebarResizeHandle } from './SidebarResizeHandle';
import { DEFAULT_WIDTH_PX, MAX_WIDTH_PX, MIN_WIDTH_PX, clampSidebarWidth } from './sidebar-width';

// jsdom doesn't implement pointer capture; stub it on Element so the
// production code's `setPointerCapture` / `releasePointerCapture` /
// `hasPointerCapture` calls don't throw.
const proto = Element.prototype as unknown as {
  setPointerCapture?: (id: number) => void;
  releasePointerCapture?: (id: number) => void;
  hasPointerCapture?: (id: number) => boolean;
};
if (typeof proto.setPointerCapture !== 'function') proto.setPointerCapture = () => {};
if (typeof proto.releasePointerCapture !== 'function') proto.releasePointerCapture = () => {};
if (typeof proto.hasPointerCapture !== 'function') proto.hasPointerCapture = () => true;

function renderHandle(initial = DEFAULT_WIDTH_PX): {
  onWidthChange: ReturnType<typeof vi.fn>;
  onCommit: ReturnType<typeof vi.fn>;
  handle: HTMLElement;
} {
  const onWidthChange = vi.fn();
  const onCommit = vi.fn();
  render(<SidebarResizeHandle width={initial} onWidthChange={onWidthChange} onCommit={onCommit} />);
  return { onWidthChange, onCommit, handle: screen.getByTestId('sidebar-resize-handle') };
}

describe('clampSidebarWidth', () => {
  it('clamps below min and above max', () => {
    expect(clampSidebarWidth(0)).toBe(MIN_WIDTH_PX);
    expect(clampSidebarWidth(99999)).toBe(MAX_WIDTH_PX);
  });

  it('rounds non-integer inputs', () => {
    expect(clampSidebarWidth(224.4)).toBe(224);
    expect(clampSidebarWidth(224.6)).toBe(225);
  });

  it('returns the default for non-finite inputs', () => {
    expect(clampSidebarWidth(Number.NaN)).toBe(DEFAULT_WIDTH_PX);
    expect(clampSidebarWidth(Number.POSITIVE_INFINITY)).toBe(DEFAULT_WIDTH_PX);
  });
});

describe('SidebarResizeHandle', () => {
  it('exposes the WAI-ARIA separator pattern with live aria-valuenow', () => {
    const { handle } = renderHandle(260);
    expect(handle).toHaveAttribute('role', 'separator');
    expect(handle).toHaveAttribute('aria-orientation', 'vertical');
    expect(handle).toHaveAttribute('aria-valuemin', String(MIN_WIDTH_PX));
    expect(handle).toHaveAttribute('aria-valuemax', String(MAX_WIDTH_PX));
    expect(handle).toHaveAttribute('aria-valuenow', '260');
    expect(handle).toHaveAttribute('tabindex', '0');
  });

  it('drags update width live and commit the final dragged value on pointer up', () => {
    const { onWidthChange, onCommit, handle } = renderHandle(220);
    fireEvent.pointerDown(handle, { button: 0, pointerId: 1, clientX: 500 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 540 }); // +40 → 260
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 580 }); // +80 → 300
    expect(onWidthChange).toHaveBeenNthCalledWith(1, 260);
    expect(onWidthChange).toHaveBeenLastCalledWith(300);
    expect(onCommit).not.toHaveBeenCalled();

    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 580 });
    // Exactly one commit per gesture, carrying the final dragged value — NOT the pre-drag width. Regression guard for the React-18 batching race
    // where `pointermove` and `pointerup` can run in the same task without an interleaving re-render, leaving the `width` prop stale.
    expect(onCommit).toHaveBeenCalledTimes(1);
    expect(onCommit).toHaveBeenLastCalledWith(300);
  });

  it('clamps drag delta to the min/max bounds', () => {
    const { onWidthChange, handle } = renderHandle(DEFAULT_WIDTH_PX);
    fireEvent.pointerDown(handle, { button: 0, pointerId: 1, clientX: 500 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 100 }); // -400 → would be -176, clamps to MIN
    expect(onWidthChange).toHaveBeenLastCalledWith(MIN_WIDTH_PX);
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 2000 }); // +1500 → clamps to MAX
    expect(onWidthChange).toHaveBeenLastCalledWith(MAX_WIDTH_PX);
  });

  it('does not start a drag for non-primary buttons', () => {
    const { onWidthChange, onCommit, handle } = renderHandle(220);
    fireEvent.pointerDown(handle, { button: 2, pointerId: 1, clientX: 500 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 600 });
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 600 });
    expect(onWidthChange).not.toHaveBeenCalled();
    expect(onCommit).not.toHaveBeenCalled();
  });

  it('cleans up the drag on pointer cancel without leaking state', () => {
    const { onCommit, handle } = renderHandle(220);
    fireEvent.pointerDown(handle, { button: 0, pointerId: 1, clientX: 500 });
    fireEvent.pointerCancel(handle, { pointerId: 1, clientX: 500 });
    expect(onCommit).toHaveBeenCalledTimes(1);
    expect(handle).toHaveAttribute('data-dragging', 'false');
  });

  it.each([
    ['ArrowLeft', 224, 208],
    ['ArrowRight', 224, 240],
    ['Home', 224, MIN_WIDTH_PX],
    ['End', 224, MAX_WIDTH_PX],
  ])('keyboard %s nudges width and commits', (key, start, expected) => {
    const { onWidthChange, onCommit, handle } = renderHandle(start);
    handle.focus();
    fireEvent.keyDown(handle, { key });
    expect(onWidthChange).toHaveBeenCalledWith(expected);
    expect(onCommit).toHaveBeenCalledWith(expected);
  });

  it('keyboard nudge at the bound clamps and still commits (Home/End behaviour)', () => {
    const { onWidthChange, onCommit, handle } = renderHandle(MIN_WIDTH_PX);
    fireEvent.keyDown(handle, { key: 'ArrowLeft' });
    // Width was already at MIN, so no live change call; commit fires anyway so the contract is "one commit per keystroke".
    expect(onWidthChange).not.toHaveBeenCalled();
    expect(onCommit).toHaveBeenCalledWith(MIN_WIDTH_PX);
  });

  it('ignores unrelated keys', () => {
    const { onWidthChange, onCommit, handle } = renderHandle(220);
    fireEvent.keyDown(handle, { key: 'Enter' });
    fireEvent.keyDown(handle, { key: ' ' });
    fireEvent.keyDown(handle, { key: 'a' });
    expect(onWidthChange).not.toHaveBeenCalled();
    expect(onCommit).not.toHaveBeenCalled();
  });

  it('double-click resets width to the default and commits', () => {
    const { onWidthChange, onCommit, handle } = renderHandle(400);
    fireEvent.doubleClick(handle);
    expect(onWidthChange).toHaveBeenCalledWith(DEFAULT_WIDTH_PX);
    expect(onCommit).toHaveBeenCalledWith(DEFAULT_WIDTH_PX);
  });

  it('reflects in-flight drag via data-dragging for the accent line', () => {
    const { handle } = renderHandle(220);
    expect(handle).toHaveAttribute('data-dragging', 'false');
    fireEvent.pointerDown(handle, { button: 0, pointerId: 1, clientX: 500 });
    expect(handle).toHaveAttribute('data-dragging', 'true');
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 500 });
    expect(handle).toHaveAttribute('data-dragging', 'false');
  });
});
