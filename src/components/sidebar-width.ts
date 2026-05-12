// Shared bounds and helpers for the resizable left sidebar (Issue #94).
// Hoisted out of `SidebarResizeHandle.tsx` so the constants and pure
// `clampSidebarWidth` helper can be imported from `Sidebar.tsx` without
// breaking the `react-refresh/only-export-components` rule on the handle
// component file.

export const MIN_WIDTH_PX = 180;
export const MAX_WIDTH_PX = 480;
export const DEFAULT_WIDTH_PX = 224;
export const KEYBOARD_STEP_PX = 16;

export function clampSidebarWidth(value: number): number {
  if (!Number.isFinite(value)) return DEFAULT_WIDTH_PX;
  return Math.min(MAX_WIDTH_PX, Math.max(MIN_WIDTH_PX, Math.round(value)));
}
