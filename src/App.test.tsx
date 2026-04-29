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

import { App } from './App';
import { configGet, frontendReady, resetBridgeMocks, sessionList } from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

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
    pendingClose: undefined,
    isHydrated: false,
  });
}

beforeEach(() => {
  resetBridgeMocks();
  initTerminalRouterMock.mockClear();
  subscribeToStatusMock.mockClear();
  subscribeToActivityMock.mockClear();
  subscribeToMetricsMock.mockClear();
  resetStores();
  document.documentElement.classList.remove('dark');
  installMatchMedia();
});

afterEach(() => {
  document.documentElement.classList.remove('dark');
});

describe('App boot sequence', () => {
  it('shows BootSplash before hydration completes and main UI after', async () => {
    let resolveCfg: (() => void) | null = null;
    configGet.mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveCfg = () =>
            resolve({
              configVersion: 3,
              defaultInstructionSets: { claude: '', copilot: '' },
              instructionSetsDir: '',
              workspaceRoot: '/mock/workspace',
              worktreeRoots: [],
              prelaunchCommands: [],
              worktreePrelaunchCommands: {},
              lastOpenSessions: [],
              tabOrder: [],
              activeSessionId: null,
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

  it('calls boot steps in order: configStore.hydrate -> sessionStore.hydrate -> initTerminalRouter -> subscribeToStatus -> frontendReady', async () => {
    const order: string[] = [];
    const cfgSpy = vi.spyOn(useConfigStore.getState(), 'hydrate').mockImplementation(async () => {
      order.push('config');
    });
    const sessSpy = vi
      .spyOn(useSessionStore.getState().actions, 'hydrate')
      .mockImplementation(async () => {
        order.push('session');
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

    expect(order).toEqual(['config', 'session', 'router', 'status', 'ready']);
    cfgSpy.mockRestore();
    sessSpy.mockRestore();
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
      prelaunchCommands: [],
      worktreePrelaunchCommands: {},
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
    });
    render(<App />);
    await waitFor(() => {
      expect(screen.getByRole('heading', { name: /choose your workspace/i })).toBeInTheDocument();
    });
    expect(screen.queryByTestId('main-area')).not.toBeInTheDocument();
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
