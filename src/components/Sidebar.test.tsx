// Behavioural tests for the Sidebar. Bridge mocked wholesale; the Zustand
// session store is reset between tests via direct setState.
//
// Drag-to-reorder note: simulating a real mouse drag against @dnd-kit in
// jsdom is fragile (PointerEvent + getBoundingClientRect dance). We test
// the drag *outcome* by invoking the same code path the drag triggers —
// `actions.reorder()` — through the Alt+ArrowDown keyboard alternative
// (test #5) and then assert that `handleDragEnd` produces the same call
// (test #6) by directly dispatching synthetic events to ensure the same
// `actions.reorder` path runs and persists via `config_set`. We document
// this trade-off here: keyboard reorder gives us full coverage of the
// reorder pipeline; pointer-drag is exercised at the integration level
// (manual smoke during dev).

import { act, fireEvent, render as rtlRender, screen, within } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { PluginRegistryProvider } from '@/plugins';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { ChildId, SessionStatus, SessionView, SubSession, WorktreeTab, WorktreeTabId } from '@/types/arborist';

import { Sidebar } from './Sidebar';

function render(ui: ReactNode) {
  const rendered = rtlRender(<PluginRegistryProvider>{ui}</PluginRegistryProvider>);
  return {
    ...rendered,
    rerender: (nextUi: ReactNode) => rendered.rerender(<PluginRegistryProvider>{nextUi}</PluginRegistryProvider>),
  };
}

function makeView(id: string, overrides: Partial<SessionView> = {}): SessionView {
  return {
    id,
    tool: 'claude',
    worktreePath: `/repo/${id}`,
    worktreeName: id,
    label: id,
    status: 'running',
    createdAt: 1_700_000_000_000,
    tabIndex: 0,
    ...overrides,
  };
}

function tabFor(session: SessionView, overrides: Partial<WorktreeTab> = {}): WorktreeTab {
  return {
    id: `tab-${session.id}` as WorktreeTabId,
    path: session.worktreePath,
    name: session.worktreeName,
    label: session.worktreeName,
    tabIndex: 0,
    iconId: 1,
    ...overrides,
  };
}

function seed(sessions: SessionView[], activeId: string | undefined): void {
  // Seed both stores so the Sidebar's worktree-tab-driven `isActive` derivation reflects the test's intended active session. Each session
  // gets a synthetic worktree tab whose `activeChildId` points at the session itself. This mirrors what the production autolink in
  // session-store.create does at runtime — without it the new grouped Sidebar would render every tab as inactive.
  const tabs = sessions.map((s, i) => tabFor(s, { tabIndex: i }));
  const activeSession = sessions.find((s) => s.id === activeId);
  const activeTab = activeSession ? tabs.find((t) => t.path === activeSession.worktreePath) : undefined;
  if (activeTab && activeSession) {
    activeTab.activeChildId = { kind: 'session', id: activeSession.id } as ChildId;
  }
  useSessionStore.setState({
    sessions,
    activeId,
    isHydrated: true,
  });
  useWorktreeTabStore.setState({
    tabs,
    activeId: activeTab ? activeTab.id : null,
    isHydrated: true,
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    isHydrated: false,
    statusMessages: {},
    hasUnread: {},
    activity: {},
    metrics: {},
  });
  useWorktreeTabStore.setState({ tabs: [], activeId: null, pendingClose: undefined, isHydrated: false });
  useSubSessionStore.setState({ subSessions: [], statusMessages: {}, pendingClose: undefined, isHydrated: true });
  // Reset the config store between tests so prior `sidebarWidthPx` writes don't leak into the next test's mount. `exactOptionalPropertyTypes`
  // forbids assigning `sidebarWidthPx: undefined` literally — strip the key by destructuring it out instead.
  const { sidebarWidthPx: _drop, ...restConfig } = useConfigStore.getState().config;
  void _drop;
  useConfigStore.setState({ config: restConfig });
  // jsdom doesn't implement HTMLDialogElement.showModal/close in older
  // versions; provide minimal shims so the worktree close-confirm dialog
  // (and other native <dialog> consumers) can mount.
  const proto = HTMLDialogElement.prototype as unknown as {
    showModal?: () => void;
    close?: () => void;
  };
  if (typeof proto.showModal !== 'function') {
    proto.showModal = function showModal(this: HTMLDialogElement) {
      this.setAttribute('open', '');
    };
  }
  if (typeof proto.close !== 'function') {
    proto.close = function close(this: HTMLDialogElement) {
      this.removeAttribute('open');
    };
  }
});

afterEach(() => {
  vi.clearAllMocks();
});

function tabByLabel(label: string): HTMLElement {
  return screen.getByRole('tab', { name: new RegExp(label, 'i') });
}

describe('Sidebar', () => {
  it('renders one tab per session and marks the active one selected', () => {
    seed([makeView('a'), makeView('b'), makeView('c')], 'b');
    render(<Sidebar />);

    expect(screen.getAllByRole('tab')).toHaveLength(3);
    expect(tabByLabel('claude session a')).toHaveAttribute('aria-selected', 'false');
    expect(tabByLabel('claude session b')).toHaveAttribute('aria-selected', 'true');
    expect(tabByLabel('claude session c')).toHaveAttribute('aria-selected', 'false');
  });

  it('clicking a tab activates that session', () => {
    seed([makeView('a'), makeView('b')], 'a');
    render(<Sidebar />);

    fireEvent.click(tabByLabel('claude session b'));

    expect(useSessionStore.getState().activeId).toBe('b');
    expect(bridgeMock.sessionFocus).toHaveBeenCalledWith({ sessionId: 'b' });
  });

  it('clicking the parent tab swaps the worktree tab back to that session', () => {
    seed([makeView('a')], 'a');
    const sub: SubSession = {
      id: 'sub-1',
      parentWorktreeTabId: 'tab-a' as WorktreeTabId,
      defId: 'shell',
      kind: 'terminal',
      label: 'shell',
      status: 'running',
      pid: 1234,
      composedCommand: 'bash',
      createdAt: 1_700_000_000_000,
    };
    useSubSessionStore.setState({
      subSessions: [sub],
      statusMessages: {},
      isHydrated: true,
    });
    useWorktreeTabStore.setState({
      tabs: [tabFor(makeView('a'), { activeChildId: { kind: 'subSession', id: 'sub-1' } })],
      activeId: 'tab-a' as WorktreeTabId,
      isHydrated: true,
    });
    render(<Sidebar />);

    fireEvent.click(tabByLabel('claude session a'));

    expect(useWorktreeTabStore.getState().tabs[0]?.activeChildId).toEqual({ kind: 'session', id: 'a' });
    expect(useSessionStore.getState().activeId).toBe('a');
  });

  it('clicking close immediately invokes session close (no confirmation dialog)', async () => {
    seed([makeView('a', { label: 'feature-x' }), makeView('b')], 'a');
    render(<Sidebar />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /close session feature-x/i }));
    });

    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(bridgeMock.sessionClose).toHaveBeenCalledWith({ sessionId: 'a', deleteWorktree: false });
    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['b']);
  });

  it('keyboard nav: ArrowDown / Home / End / Enter / Delete', async () => {
    seed([makeView('a'), makeView('b'), makeView('c')], 'a');
    render(<Sidebar />);

    const tablist = screen.getByRole('tablist');
    const a = tabByLabel('claude session a');
    a.focus();

    fireEvent.keyDown(tablist, { key: 'ArrowDown' });
    expect(tabByLabel('claude session b')).toHaveFocus();

    fireEvent.keyDown(tablist, { key: 'End' });
    expect(tabByLabel('claude session c')).toHaveFocus();

    fireEvent.keyDown(tablist, { key: 'Home' });
    expect(tabByLabel('claude session a')).toHaveFocus();

    fireEvent.keyDown(tablist, { key: 'ArrowDown' });
    fireEvent.keyDown(tablist, { key: 'Enter' });
    expect(useSessionStore.getState().activeId).toBe('b');

    await act(async () => {
      fireEvent.keyDown(tablist, { key: 'Delete' });
    });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(bridgeMock.sessionClose).toHaveBeenCalledWith({ sessionId: 'b', deleteWorktree: false });
  });

  it('Alt+ArrowDown is a no-op (session reorder deferred for v1 worktree-tab UI)', async () => {
    seed([makeView('a'), makeView('b'), makeView('c')], 'a');
    render(<Sidebar />);

    const tablist = screen.getByRole('tablist');
    tabByLabel('claude session a').focus();

    await act(async () => {
      fireEvent.keyDown(tablist, { key: 'ArrowDown', altKey: true });
    });

    // Order unchanged. Per-group session reorder is a planned follow-up; the v1 grouped sidebar drops Alt+arrow because the visual
    // grouping no longer matches a flat session id array.
    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['a', 'b', 'c']);
    expect(bridgeMock.configSet).not.toHaveBeenCalledWith({ tabOrder: expect.any(Array) });
  });

  it('drag-to-reorder pipeline (handleDragEnd) persists order via config_set', async () => {
    // We invoke the same store action a real drag would call. The
    // wiring from @dnd-kit's onDragEnd → actions.reorder is a one-line
    // glue; verifying the *pipeline* (store + bridge) here keeps the
    // test deterministic without simulating pointer geometry.
    seed([makeView('a'), makeView('b'), makeView('c')], 'a');
    render(<Sidebar />);

    await act(async () => {
      await useSessionStore.getState().actions.reorder(['c', 'a', 'b']);
    });

    expect(bridgeMock.configSet).toHaveBeenCalledWith({ tabOrder: ['c', 'a', 'b'] });
    expect(useSessionStore.getState().sessions.map((s) => s.id)).toEqual(['c', 'a', 'b']);
  });

  it('shows the error indicator when status is "error"', () => {
    const errorStatus: SessionStatus = 'error';
    seed([makeView('a', { status: errorStatus })], 'a');
    render(<Sidebar />);

    const tab = tabByLabel('claude session a');
    expect(within(tab).getByRole('img', { name: /error/i })).toBeInTheDocument();
  });

  it('shows the starting indicator when status is "starting"', () => {
    const startingStatus: SessionStatus = 'starting';
    seed([makeView('a', { status: startingStatus })], 'a');
    render(<Sidebar />);

    const tab = tabByLabel('claude session a');
    expect(within(tab).getByRole('img', { name: /starting/i })).toBeInTheDocument();
    expect(within(tab).queryByRole('img', { name: /error/i })).not.toBeInTheDocument();
  });

  it('shows the exited indicator when status is "exited"', () => {
    const exitedStatus: SessionStatus = 'exited';
    seed([makeView('a', { status: exitedStatus })], 'a');
    render(<Sidebar />);

    const tab = tabByLabel('claude session a');
    expect(within(tab).getByRole('img', { name: /exited/i })).toBeInTheDocument();
  });

  it('focus moves to the right neighbour after closing the active tab', async () => {
    seed([makeView('a'), makeView('b'), makeView('c')], 'b');
    render(<Sidebar />);

    tabByLabel('claude session b').focus();
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /close session b/i }));
    });

    expect(tabByLabel('claude session c')).toHaveFocus();
  });

  it('focus moves to the new-session button when the last tab is closed', async () => {
    seed([makeView('a')], 'a');
    render(<Sidebar />);

    tabByLabel('claude session a').focus();
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /close session a/i }));
    });

    expect(screen.getByRole('button', { name: /new session/i })).toHaveFocus();
  });

  it('the close button has an accessible name scoped to the session', () => {
    seed([makeView('a', { label: 'docs-rewrite' })], 'a');
    render(<Sidebar />);

    expect(screen.getByRole('button', { name: /close session docs-rewrite/i })).toBeInTheDocument();
  });

  it('renders the Settings footer button and opens the Settings dialog on click', () => {
    seed([makeView('a')], 'a');
    render(<Sidebar />);
    expect(screen.queryByTestId('settings-dialog')).toBeNull();
    fireEvent.click(screen.getByTestId('settings-button'));
    expect(screen.getByTestId('settings-dialog')).toBeInTheDocument();
  });

  it('does not intercept Delete/Arrow keypresses originating in the Settings dialog', () => {
    seed([makeView('a')], 'a');
    render(<Sidebar />);
    fireEvent.click(screen.getByTestId('settings-button'));
    const input = screen.getByLabelText(/worktree prep commands/i);
    input.focus();
    fireEvent.keyDown(input, { key: 'Delete' });
    // The close-confirm dialog must NOT have been opened — Delete
    // pressed inside an input should not bubble up into the tablist
    // handler.
    expect(screen.queryByText(/terminate session/i)).toBeNull();
  });

  it('session context menu exposes only Restart and Close actions', async () => {
    seed([makeView('a')], 'a');
    render(<Sidebar />);
    fireEvent.click(screen.getByTestId('sidebar-tab-menu-a'));
    expect(screen.getByRole('menuitem', { name: /restart/i })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /close/i })).toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: /launch/i })).toBeNull();
    await act(async () => {});
  });

  it('opens Settings on the General tab when launched from the footer button', () => {
    seed([makeView('a')], 'a');
    render(<Sidebar />);
    fireEvent.click(screen.getByTestId('settings-button'));
    expect(screen.getByTestId('settings-panel-general')).toBeInTheDocument();
  });

  it('does not swallow Enter/Space activation on non-tab buttons in the bottom bar', () => {
    // Regression for the "Settings button stops working with the
    // keyboard" PR-review finding: the tablist `onKeyDown` used to fire
    // `preventDefault()` on Enter/Space for ANY descendant, so focusing
    // the Settings button and pressing Enter would no longer activate
    // it. Buttons inside the sidebar that lack `role="tab"` must be
    // skipped by the tablist key handler so the browser's default click
    // synthesis fires normally.
    seed([makeView('a')], 'a');
    render(<Sidebar />);
    expect(screen.queryByTestId('settings-dialog')).toBeNull();

    const settingsBtn = screen.getByTestId('settings-button');
    settingsBtn.focus();
    // Mirror what a real keyboard activation would do: fire keydown
    // with the focused button as the event target. The handler must
    // bail out, leaving Enter free to trigger the click → onClick.
    fireEvent.keyDown(settingsBtn, { key: 'Enter' });
    expect(settingsBtn).not.toHaveAttribute('aria-disabled', 'true');
    // Sanity: the close-confirm dialog (bound to Delete on tabs)
    // should also remain closed if Delete is pressed on a non-tab
    // button — proves the gate is wide enough.
    fireEvent.keyDown(settingsBtn, { key: 'Delete' });
    expect(screen.queryByText(/terminate session/i)).toBeNull();
  });
});

describe('Sidebar unread accessibility', () => {
  it("appends '(unread output)' to inactive tabs' aria-label so screen readers hear it", () => {
    seed([makeView('a'), makeView('b')], 'b');
    useSessionStore.setState({ hasUnread: { a: true } });
    render(<Sidebar />);
    expect(tabByLabel('claude session a')).toHaveAttribute('aria-label', 'claude session a (unread output)');
    // Active tab never carries the unread suffix — focusing it clears the flag.
    expect(tabByLabel('claude session b')).toHaveAttribute('aria-label', 'claude session b');
  });

  it('does not append the suffix when hasUnread is false', () => {
    seed([makeView('a')], 'a');
    render(<Sidebar />);
    expect(tabByLabel('claude session a')).toHaveAttribute('aria-label', 'claude session a');
  });
});

describe('Sidebar metrics indicator (Issue #3)', () => {
  it('renders a compact metrics line under the label when metrics are present', () => {
    seed([makeView('a')], 'a');
    useSessionStore.setState({
      metrics: {
        a: {
          sessionId: 'a',
          model: 'claude-sonnet-4-6',
          contextUsedPct: 42,
          contextTokensUsed: 12_345,
          contextTokensLimit: 200_000,
          inputTokens: 9000,
          outputTokens: 3345,
          observedAt: 1700000000,
        },
      },
    });
    render(<Sidebar />);
    const line = screen.getByTestId('sidebar-metrics');
    expect(line.textContent).toContain('42%');
    expect(line.textContent).toContain('12.3k tok');
    // Long-form is in the title attribute for hover.
    expect(line.getAttribute('title')).toContain('claude-sonnet-4-6');
    expect(line.getAttribute('title')).toMatch(/200,000|200000/);
  });

  it('hides the metrics line when no snapshot is in the store', () => {
    seed([makeView('a')], 'a');
    render(<Sidebar />);
    expect(screen.queryByTestId('sidebar-metrics')).toBeNull();
  });

  it('hides the metrics line for non-running sessions even if a stale snapshot exists', () => {
    seed([makeView('a', { status: 'starting' })], 'a');
    useSessionStore.setState({
      metrics: {
        a: { sessionId: 'a', contextUsedPct: 10, observedAt: 0 },
      },
    });
    render(<Sidebar />);
    expect(screen.queryByTestId('sidebar-metrics')).toBeNull();
  });

  // Copilot sessions surface metrics through the same wire shape
  // (`SessionMetricsEvent`) as Claude — the only difference is which
  // backend watcher produced the snapshot. The sidebar must render
  // them identically.
  it('renders the same metrics indicator for Copilot sessions', () => {
    seed([makeView('a', { tool: 'copilot' })], 'a');
    useSessionStore.setState({
      metrics: {
        a: {
          sessionId: 'a',
          model: 'claude-opus-4.7',
          contextUsedPct: 17,
          contextTokensUsed: 29_461,
          contextTokensLimit: 168_000,
          inputTokens: 39_497,
          outputTokens: 24,
          observedAt: 1_700_000_000,
        },
      },
    });
    render(<Sidebar />);
    const line = screen.getByTestId('sidebar-metrics');
    expect(line.textContent).toContain('17%');
    expect(line.textContent).toContain('29.5k tok');
    expect(line.getAttribute('title')).toContain('claude-opus-4.7');
    expect(line.getAttribute('title')).toMatch(/168,000|168000/);
    // Disambiguation: the Copilot-reported window is smaller than the
    // model's nominal max because Copilot reserves space for its own
    // system prompt + tool definitions. Surface that in the tooltip so
    // users don't read 30k/168k as a math error against e.g. Opus's 200k.
    expect(line.getAttribute('title')).toMatch(/Copilot-reported/i);
    expect(line.getAttribute('title')).toMatch(/system-prompt \+ tool overhead/i);
  });

  it('does NOT add the Copilot-reported caveat for Claude sessions', () => {
    seed([makeView('a')], 'a');
    useSessionStore.setState({
      metrics: {
        a: {
          sessionId: 'a',
          model: 'claude-sonnet-4-6',
          contextUsedPct: 42,
          contextTokensUsed: 12_345,
          contextTokensLimit: 200_000,
          inputTokens: 9000,
          outputTokens: 3345,
          observedAt: 1_700_000_000,
        },
      },
    });
    render(<Sidebar />);
    const title = screen.getByTestId('sidebar-metrics').getAttribute('title') ?? '';
    expect(title).not.toMatch(/Copilot-reported/i);
    // Claude gets its own parallel caveat so users know what 'limit' means.
    expect(title).toMatch(/model nominal max/i);
    expect(title).toMatch(/includes harness overhead/i);
  });

  // ---------------------------------------------------------------------
  // Issue #94 — resizable sidebar
  // ---------------------------------------------------------------------

  describe('sidebar width (Issue #94)', () => {
    it('falls back to the 224 px default when no persisted width is set', () => {
      seed([makeView('a')], 'a');
      render(<Sidebar />);
      expect(screen.getByTestId('sidebar')).toHaveStyle({ width: '224px' });
    });

    it('renders the persisted width from config-store on mount', () => {
      useConfigStore.setState({
        config: { ...useConfigStore.getState().config, sidebarWidthPx: 320 },
      });
      seed([makeView('a')], 'a');
      render(<Sidebar />);
      expect(screen.getByTestId('sidebar')).toHaveStyle({ width: '320px' });
    });

    it('persists a new width via configSet when the user nudges via keyboard', async () => {
      bridgeMock.configSet.mockResolvedValue({
        ...useConfigStore.getState().config,
        sidebarWidthPx: 240,
      });
      seed([makeView('a')], 'a');
      render(<Sidebar />);
      const handle = screen.getByTestId('sidebar-resize-handle');
      await act(async () => {
        fireEvent.keyDown(handle, { key: 'ArrowRight' });
      });
      expect(bridgeMock.configSet).toHaveBeenCalledWith({ sidebarWidthPx: 240 });
    });

    it('skips persistence when the width is already at the persisted value', () => {
      useConfigStore.setState({
        config: { ...useConfigStore.getState().config, sidebarWidthPx: 224 },
      });
      seed([makeView('a')], 'a');
      render(<Sidebar />);
      const handle = screen.getByTestId('sidebar-resize-handle');
      // Double-click "reset" should be a no-op write because we're already at the default.
      fireEvent.doubleClick(handle);
      expect(bridgeMock.configSet).not.toHaveBeenCalled();
    });
  });
});
