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
import {
  configGet,
  frontendReady,
  onWorkspaceChanged,
  resetBridgeMocks,
  sessionList,
} from '@/lib/tauri-bridge.mock';
import type { AppConfig, WorkspaceChangedEvent } from '@/types/arborist';
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

function defaultConfig(overrides: { workspaceRoot?: string | null } = {}): AppConfig {
  return {
    configVersion: 4,
    defaultInstructionSets: { claude: '', copilot: '' },
    instructionSetsDir: '',
    workspaceRoot: overrides.workspaceRoot ?? '/mock/workspace',
    worktreeRoots: [],
    prelaunchCommands: [],
    worktreePrelaunchCommands: {},
    aiLaunchCommands: { claude: '', copilot: '' },
    lastOpenSessions: [],
    tabOrder: [],
    activeSessionId: null,
  };
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
              aiLaunchCommands: { claude: '', copilot: '' },
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
      aiLaunchCommands: { claude: '', copilot: '' },
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

  // Regression for PR #32 review finding: a slow `workspace://changed`
  // rehydrate must not overwrite Zustand state after a newer event has
  // already settled. With `rehydrateActiveWorkspace`'s serialise +
  // skip-when-superseded helper, the *older* emit's body is skipped
  // entirely before any IPC fires — so its slow `configGet` mock is
  // never consumed.
  it('discards a stale workspace://changed rehydrate when a newer event arrives', async () => {
    let emit: ((payload: WorkspaceChangedEvent) => void) | null = null;
    onWorkspaceChanged.mockImplementation((cb) => {
      emit = cb;
      return Promise.resolve(() => {});
    });

    // Boot: first configGet/sessionList resolves immediately so the app
    // reaches the ready state before we start firing workspace events.
    render(<App />);
    await waitFor(() => expect(emit).not.toBeNull());
    await waitFor(() => expect(frontendReady).toHaveBeenCalledTimes(1));

    const sessionListBefore = sessionList.mock.calls.length;
    const configGetBefore = configGet.mock.calls.length;
    const frontendReadyBefore = frontendReady.mock.calls.length;

    // Queue the newer emit's mocks first so they are consumed by the
    // *winning* (second) submission. The older (skipped) submission
    // never invokes any of these mocks.
    configGet.mockResolvedValueOnce(defaultConfig({ workspaceRoot: '/ws/b' }));
    sessionList.mockResolvedValueOnce([]);

    await act(async () => {
      // Fire two emits back-to-back, synchronously. The first is queued
      // on `rehydrateActiveWorkspace`'s serial chain; the second's
      // mere submission supersedes it. By the time the chain reaches
      // the first slot, it sees `myGen < submitted` and skips.
      emit!({ workspaceRoot: '/ws/a' });
      emit!({ workspaceRoot: '/ws/b' });
      // Yield repeatedly so every queued microtask completes.
      for (let i = 0; i < 6; i++) await Promise.resolve();
    });
    await waitFor(() => expect(frontendReady).toHaveBeenCalledTimes(2));

    // Exactly one extra hydrate executed (the winner). The superseded
    // call did not call configGet, frontendReady, or sessionList.
    expect(configGet.mock.calls.length - configGetBefore).toBe(1);
    expect(frontendReady.mock.calls.length - frontendReadyBefore).toBe(1);
    expect(sessionList.mock.calls.length - sessionListBefore).toBe(1);
  });
  // Regression for PR #32 round-9 review finding: when the cleanup of
  // App's boot effect runs BEFORE `await onWorkspaceChanged()`
  // resolves, the listener registration is in flight in the backend
  // but the local `unlisten` is still null. The OLD code's cleanup
  // checked `unlistenWorkspaceChanged` synchronously, found null, and
  // dropped the listener on the floor — every subsequent mount
  // registered a duplicate handler that fired on every workspace
  // switch. The fix holds the in-flight `Promise<Unlisten>` and
  // chains the unlisten call off it in cleanup so the listener is
  // detached as soon as registration resolves, regardless of when
  // cleanup ran.
  //
  // We simulate the race by deferring the registration promise. The
  // listener call itself happens synchronously when boot reaches that
  // line, so we can wait for `onWorkspaceChanged` to have been called
  // and then unmount before fulfilling its promise.
  it('detaches a workspace://changed listener even when the effect cleanup runs before registration resolves', async () => {
    let unlistenCalled = false;
    let resolveRegistration: (() => void) | null = null;
    onWorkspaceChanged.mockImplementation(
      () =>
        new Promise<() => void>((resolve) => {
          resolveRegistration = () => {
            resolve(() => {
              unlistenCalled = true;
            });
          };
        }),
    );

    const { unmount } = render(<App />);

    // Wait until boot reaches the listener registration. The call is
    // synchronous; only its promise is deferred.
    await waitFor(() => expect(onWorkspaceChanged).toHaveBeenCalledTimes(1));
    expect(resolveRegistration).not.toBeNull();
    expect(unlistenCalled).toBe(false);

    // Tear down the effect BEFORE the registration promise resolves.
    // With the old code, cleanup would see `unlistenWorkspaceChanged ===
    // null` and bail — leaking the listener.
    unmount();
    expect(unlistenCalled).toBe(false);

    // Now resolve the registration. Cleanup chained off the promise,
    // so the unlisten must fire as soon as the promise settles.
    await act(async () => {
      resolveRegistration!();
      // Yield to flush the microtask queue (Promise.then callbacks).
      for (let i = 0; i < 6; i++) await Promise.resolve();
    });

    expect(unlistenCalled).toBe(true);
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
