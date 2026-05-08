// =============================================================================
// Selectors — central registry of data-testid values used across e2e specs.
//
// Every selector used by the spec suite is defined here so that when a testid
// changes in the frontend, only this file needs updating.
// =============================================================================

/** Helper: build a CSS selector for a data-testid attribute. */
export function byTestId(id: string): string {
  return `[data-testid="${id}"]`;
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------
export const SIDEBAR = byTestId("sidebar");
export const MAIN_AREA = byTestId("main-area");

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------
export const WORKSPACE_INDICATOR = byTestId("workspace-indicator");
export const WORKSPACE_PICKER = byTestId("workspace-picker");

// ---------------------------------------------------------------------------
// Session tabs
// ---------------------------------------------------------------------------
export const SIDEBAR_TAB = byTestId("sidebar-tab");
export const SIDEBAR_TAB_LABEL = byTestId("sidebar-tab-label");
export const SIDEBAR_TAB_CLOSE = byTestId("sidebar-tab-close");

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------
export const TERMINAL_VIEW = byTestId("terminal-view");
export const XTERM_ROWS = ".xterm-rows";

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------
export const SETTINGS_DIALOG = byTestId("settings-dialog");
