import { act, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

// Avoid pulling xterm into the App test — MainArea uses TerminalView via
// useTerminal. Stub MainArea to a marker element.
vi.mock('@/components/MainArea', () => ({
  MainArea: (): JSX.Element => <div data-testid="main-area" />,
}));
vi.mock('@/components/Sidebar', () => ({
  Sidebar: (): JSX.Element => <div role="tablist" aria-label="Sessions" />,
}));
vi.mock('@/components/NewSessionDialog', () => ({
  NewSessionDialog: (): JSX.Element => <div data-testid="dlg" />,
}));

const initTerminalRouterMock = vi.fn();
vi.mock('@/hooks/use-terminal', () => ({
  initTerminalRouter: () => initTerminalRouterMock(),
}));

const subscribeToStatusMock = vi.fn(() => () => {});
const subscribeToActivityMock = vi.fn(() => () => {});
const subscribeToMetricsMock = vi.fn(() => () => {});
vi.mock('@/lib/session-events', () => ({
  subscribeToStatus: () => subscribeToStatusMock(),
  subscribeToActivity: () => subscribeToActivityMock(),
  subscribeToMetrics: () => subscribeToMetricsMock(),
}));

const subscribeToSubStatusMock = vi.fn(() => () => {});
const subscribeToSubExitedMock = vi.fn(() => () => {});
const subscribeToSubRestoredMock = vi.fn(() => () => {});
vi.mock('@/lib/sub-session-events', () => ({
  subscribeToSubStatus: () => subscribeToSubStatusMock(),
  subscribeToSubExited: () => subscribeToSubExitedMock(),
  subscribeToSubRestored: () => subscribeToSubRestoredMock(),
}));

import { App } from './App';
import { configGet, frontendReady, resetBridgeMocks, sessionList } from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import { useWorkspaceSwitchUiStore } from '@/store/workspace-switch-ui-store';

interface MediaQueryListLike {
  matches: boolean;
  media: string;
  addEventListener: (type: string, cb: (e: MediaQueryListEvent) => void) => void;
  removeEventListener: (type: string, cb: (e: MediaQueryListEvent) => void) => void;
  addListener: (cb: (e: MediaQueryListEvent) => void) => void;
  removeListener: (cb: (e: MediaQueryListEvent) => void) => void;
  dispatchEvent: (event: Event) => boolean;
  onchange: ((e: MediaQueryListEvent) => void) | null;
}

let mediaListeners: Array<(e: MediaQueryListEvent) => void> = [];
let mediaMatches = false;
function installMatchMedia(): void {
  mediaListeners = [];
  mediaMatches = false;
  Object.defineProperty(window, 'matchMedia', {
    configurable: true,
    writable: true,
    value: (query: string): MediaQueryListLike => ({
      matches: mediaMatches,
      media: query,
      addEventListener: (_t, cb) => {
        mediaListeners.push(cb);
      },
      removeEventListener: (_t, cb) => {
        mediaListeners = mediaListeners.filter((l) => l !== cb);
      },
      addListener: (cb) => {
        mediaListeners.push(cb);
      },
      removeListener: (cb) => {
        mediaListeners = mediaListeners.filter((l) => l !== cb);
      },
      dispatchEvent: () => true,
      onchange: null,
    }),
  });
}

function fireMediaChange(matches: boolean): void {
  mediaMatches = matches;
  const evt = { matches } as MediaQueryListEvent;
  for (const cb of mediaListeners) cb(evt);
}

function resetStores(): void {
  useConfigStore.setState({ status: 'idle', error: null });
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    isHydrated: false,
  });
  useWorktreeTabStore.setState({ tabs: [], activeId: null, pendingClose: undefined, isHydrated: false });
}

// `window.location` is replaced by the reload-button test below; capture
// the original descriptor in `beforeEach` and restore it in `afterEach`
// so subsequent tests (in this file or any test that imports App) get a
// pristine `location`.
let originalLocationDescriptor: PropertyDescriptor | undefined;

beforeEach(() => {
  resetBridgeMocks();
  initTerminalRouterMock.mockClear();
  subscribeToStatusMock.mockClear();
  subscribeToActivityMock.mockClear();
  subscribeToMetricsMock.mockClear();
  resetStores();
  useWorkspaceSwitchUiStore.setState({ isSwitching: false });
  document.documentElement.classList.remove('dark');
  installMatchMedia();
  originalLocationDescriptor = Object.getOwnPropertyDescriptor(window, 'location');
});

afterEach(() => {
  document.documentElement.classList.remove('dark');
  if (originalLocationDescriptor) {
    Object.defineProperty(window, 'location', originalLocationDescriptor);
  }
});

describe('App boot sequence', () => {
  it('shows BootSplash before hydration completes and main UI after', async () => {
    let resolveCfg: (() => void) | null = null;
    // Use `mockImplementationOnce` so the gated promise only governs the FIRST configGet call (configStore.hydrate). Subsequent
    // configGet calls (e.g. from worktreeTabStore.hydrate) fall back to the default mock that resolves immediately, so boot can
    // complete after we manually resolve the first one.
    configGet.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveCfg = () =>
            resolve({
              configVersion: 3,
              defaultInstructionSets: { claude: '', copilot: '' },
              instructionSetsDir: '',
              workspaceRoot: '/mock/workspace',
              worktreeRoots: [],
              worktreePrepCommands: [],
              aiLaunchCommands: { commands: {}, iconDataUris: {} },
              lastOpenSessions: [],
              tabOrder: [],
              activeSessionId: null,
              customProcesses: [],
              lastOpenSubSessions: [],
              worktreeTabs: [],
              worktreeTabOrder: [],
              activeWorktreeTabId: null,
            });
        }),
    );

    render(<App />);
    expect(screen.getByRole('status')).toHaveTextContent(/loading arborist/i);

    await act(async () => {
      resolveCfg!();
    });

    await waitFor(() => {
      expect(screen.getByTestId('main-area')).toBeInTheDocument();
    });
  });

  it('calls boot steps in order: config -> status -> session -> subsession -> worktreeTab -> router -> ready', async () => {
    const order: string[] = [];
    const cfgSpy = vi.spyOn(useConfigStore.getState(), 'hydrate').mockImplementation(async () => {
      order.push('config');
    });
    const sessSpy = vi.spyOn(useSessionStore.getState().actions, 'hydrate').mockImplementation(async () => {
      order.push('session');
    });
    const subSpy = vi.spyOn(useSubSessionStore.getState().actions, 'hydrate').mockImplementation(async () => {
      order.push('subsession');
    });
    const wttSpy = vi.spyOn(useWorktreeTabStore.getState().actions, 'hydrate').mockImplementation(async () => {
      order.push('worktreeTab');
    });
    initTerminalRouterMock.mockImplementation(() => order.push('router'));
    subscribeToStatusMock.mockImplementation(() => {
      order.push('status');
      return () => {};
    });
    frontendReady.mockImplementation(async () => {
      order.push('ready');
    });

    render(<App />);
    await waitFor(() => expect(frontendReady).toHaveBeenCalled());

    expect(order).toEqual(['config', 'status', 'session', 'subsession', 'worktreeTab', 'router', 'ready']);
    cfgSpy.mockRestore();
    sessSpy.mockRestore();
    subSpy.mockRestore();
    wttSpy.mockRestore();
  });

  it('passes the live session worktreePath set into worktreeTabStore.hydrate so orphan tabs can be self-healed', async () => {
    // Seed the session store with two sessions on different worktrees BEFORE App boots, then assert hydrate sees those paths. Use a no-op
    // implementation for sessionStore.hydrate so the seeded state survives.
    useSessionStore.setState({
      sessions: [
        { id: 's1', tool: 'claude', worktreePath: '/repo/a', worktreeName: 'a', label: 'a', tabIndex: 0, status: 'running', composedCommand: '' },
        { id: 's2', tool: 'copilot', worktreePath: '/repo/b', worktreeName: 'b', label: 'b', tabIndex: 1, status: 'running', composedCommand: '' },
        { id: 's3', tool: 'claude', worktreePath: '/repo/a', worktreeName: 'a', label: 'a 2', tabIndex: 2, status: 'running', composedCommand: '' },
      ] as never,
      activeId: undefined,
      isHydrated: true,
    });
    const sessSpy = vi.spyOn(useSessionStore.getState().actions, 'hydrate').mockImplementation(async () => undefined);
    const wttSpy = vi.spyOn(useWorktreeTabStore.getState().actions, 'hydrate').mockImplementation(async () => undefined);

    render(<App />);
    await waitFor(() => expect(wttSpy).toHaveBeenCalled());

    // Hydrate is invoked with the freshly hydrated session paths (duplicates included — the store dedupes internally).
    expect(wttSpy).toHaveBeenCalledWith(['/repo/a', '/repo/b', '/repo/a']);
    sessSpy.mockRestore();
    wttSpy.mockRestore();
  });

  it('renders the error overlay when hydrate throws and Reload calls window.location.reload', async () => {
    configGet.mockRejectedValueOnce(new Error('boom'));
    const reloadSpy = vi.fn();
    Object.defineProperty(window, 'location', {
      configurable: true,
      value: { ...window.location, reload: reloadSpy },
    });

    render(<App />);
    await waitFor(() => {
      expect(screen.getByRole('alert')).toBeInTheDocument();
    });
    expect(screen.getByText(/boom/)).toBeInTheDocument();

    act(() => {
      screen.getByRole('button', { name: /reload/i }).click();
    });
    expect(reloadSpy).toHaveBeenCalled();
  });

  it('still renders main UI when zero sessions are persisted', async () => {
    sessionList.mockResolvedValue([]);
    render(<App />);
    await waitFor(() => {
      expect(screen.getByTestId('main-area')).toBeInTheDocument();
    });
  });

  it('shows the WorkspacePicker on first boot when workspaceRoot is null', async () => {
    configGet.mockResolvedValueOnce({
      configVersion: 3,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: '',
      workspaceRoot: null,
      worktreeRoots: [],
      worktreePrepCommands: [],
      aiLaunchCommands: { commands: {}, iconDataUris: {} },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: [],
      lastOpenSubSessions: [],
      worktreeTabs: [],
      worktreeTabOrder: [],
      activeWorktreeTabId: null,
    });
    render(<App />);
    await waitFor(() => {
      expect(screen.getByRole('heading', { name: /choose your workspace/i })).toBeInTheDocument();
    });
    expect(screen.queryByTestId('main-area')).not.toBeInTheDocument();
  });
});

describe('App workspace-switch overlay', () => {
  // PR6: while the backend's transactional workspace switch is in
  // flight, App must overlay a "Switching workspace…" panel and gate
  // input so a user can't click on stale tabs that are about to be
  // replaced.
  it('does not render the overlay when isSwitching is false', async () => {
    render(<App />);
    await waitFor(() => {
      expect(screen.getByTestId('main-area')).toBeInTheDocument();
    });
    expect(screen.queryByTestId('workspace-switch-overlay')).not.toBeInTheDocument();
  });

  it('renders the overlay and marks the underlying root inert + aria-busy when isSwitching flips true', async () => {
    render(<App />);
    await waitFor(() => {
      expect(screen.getByTestId('main-area')).toBeInTheDocument();
    });

    act(() => {
      useWorkspaceSwitchUiStore.setState({ isSwitching: true });
    });

    const overlay = screen.getByTestId('workspace-switch-overlay');
    expect(overlay).toBeInTheDocument();
    // Modal semantics — `alertdialog` + `aria-modal` so AT users
    // perceive the boundary; `aria-labelledby` points at the
    // visible "Switching workspace…" copy.
    expect(overlay).toHaveAttribute('role', 'alertdialog');
    expect(overlay).toHaveAttribute('aria-modal', 'true');
    expect(overlay).toHaveAttribute('aria-labelledby', 'workspace-switch-overlay-label');
    // The MainArea + Sidebar wrapper must be inert + aria-busy so
    // input can't reach stale tabs during the switch.
    const root = screen.getByTestId('main-area').parentElement;
    expect(root).not.toBeNull();
    expect(root!.getAttribute('aria-busy')).toBe('true');
    expect(root!.hasAttribute('inert')).toBe(true);
  });

  it('moves focus into the overlay when isSwitching becomes true', async () => {
    render(<App />);
    await waitFor(() => {
      expect(screen.getByTestId('main-area')).toBeInTheDocument();
    });
    // Park focus on a focusable element outside the overlay first.
    const probe = document.createElement('button');
    probe.textContent = 'probe';
    document.body.appendChild(probe);
    probe.focus();
    expect(document.activeElement).toBe(probe);

    act(() => {
      useWorkspaceSwitchUiStore.setState({ isSwitching: true });
    });

    const overlay = screen.getByTestId('workspace-switch-overlay');
    // The overlay itself becomes the focused element so the
    // previously-focused element no longer receives keyboard input
    // (defence-in-depth on top of `inert` on the underlying root).
    expect(document.activeElement).toBe(overlay);
    document.body.removeChild(probe);
  });

  it('bounces focus back into the overlay if focus escapes while isSwitching is true', async () => {
    render(<App />);
    await waitFor(() => {
      expect(screen.getByTestId('main-area')).toBeInTheDocument();
    });
    act(() => {
      useWorkspaceSwitchUiStore.setState({ isSwitching: true });
    });
    const overlay = screen.getByTestId('workspace-switch-overlay');
    expect(document.activeElement).toBe(overlay);

    // Simulate focus escaping to an outside element (the underlying
    // root being `inert` should normally prevent this; this asserts
    // the document-level focus trap as a backstop).
    const escapee = document.createElement('button');
    escapee.textContent = 'escapee';
    document.body.appendChild(escapee);
    act(() => {
      escapee.focus();
    });

    expect(document.activeElement).toBe(overlay);
    document.body.removeChild(escapee);
  });

  it('removes the overlay and clears inert/aria-busy when isSwitching flips back to false', async () => {
    render(<App />);
    await waitFor(() => {
      expect(screen.getByTestId('main-area')).toBeInTheDocument();
    });
    act(() => {
      useWorkspaceSwitchUiStore.setState({ isSwitching: true });
    });
    expect(screen.queryByTestId('workspace-switch-overlay')).toBeInTheDocument();

    act(() => {
      useWorkspaceSwitchUiStore.setState({ isSwitching: false });
    });

    expect(screen.queryByTestId('workspace-switch-overlay')).not.toBeInTheDocument();
    const root = screen.getByTestId('main-area').parentElement;
    expect(root).not.toBeNull();
    expect(root!.getAttribute('aria-busy')).toBeNull();
    expect(root!.hasAttribute('inert')).toBe(false);
  });
});

describe('App dark mode', () => {
  it('applies the dark class when prefers-color-scheme: dark is on at boot', async () => {
    mediaMatches = true;
    render(<App />);
    await waitFor(() => {
      expect(document.documentElement.classList.contains('dark')).toBe(true);
    });
  });

  it('toggles the dark class when prefers-color-scheme changes', async () => {
    render(<App />);
    await waitFor(() => {
      expect(document.documentElement.classList.contains('dark')).toBe(false);
    });
    act(() => fireMediaChange(true));
    expect(document.documentElement.classList.contains('dark')).toBe(true);
    act(() => fireMediaChange(false));
    expect(document.documentElement.classList.contains('dark')).toBe(false);
  });
});
