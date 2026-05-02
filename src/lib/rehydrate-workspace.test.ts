// Regression for the "tabs appear with starting spinner but the AI
// never resumes" symptom users hit after switching workspaces back to
// one with parked sessions.
//
// Root cause: `rehydrateActiveWorkspace` previously called
// `sessionStore.hydrate()` BEFORE `frontendReady()`. That order races
// the backend's deferred-spawn registration:
//
//   1. sessionStore.hydrate() resolves → React re-renders with the
//      restored sessions → `MainArea` mounts the new `TerminalView`s
//      → `attach` → `refit` → first `session_resize` IPC fires.
//   2. ...meanwhile `frontendReady()` is still in flight, awaiting
//      `restore_all_sessions` to populate `pending_spawn`.
//
// The first `session_resize` arrived before `pending_spawn` was
// populated, so the backend's `pool.resize` returned `NotFound` and
// the deferred spawn never triggered — the session sat at `Starting`
// forever.
//
// The fix flips the order so `frontendReady` (which awaits
// `restore_all_sessions` to completion server-side) settles BEFORE
// the session-store update that drives the React mount → resize.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { rehydrateActiveWorkspace } from './rehydrate-workspace';
import { frontendReady, resetBridgeMocks } from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

vi.mock('@/lib/tauri-bridge', () => import('@/lib/tauri-bridge.mock'));

let configHydrate: ReturnType<typeof vi.fn>;
let sessionHydrate: ReturnType<typeof vi.fn>;

beforeEach(() => {
  resetBridgeMocks();
  configHydrate = vi.fn().mockResolvedValue(undefined);
  sessionHydrate = vi.fn().mockResolvedValue(undefined);
  useConfigStore.setState({ hydrate: configHydrate } as never);
  useSessionStore.setState((s) => ({
    actions: { ...s.actions, hydrate: sessionHydrate },
  }));
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('rehydrateActiveWorkspace', () => {
  it('calls frontendReady before sessionStore.hydrate so pending_spawn is populated before TerminalView mounts and fires session_resize', async () => {
    const order: string[] = [];
    configHydrate.mockImplementation(async () => {
      order.push('config');
    });
    frontendReady.mockImplementation(async () => {
      order.push('frontendReady');
    });
    sessionHydrate.mockImplementation(async () => {
      order.push('session');
    });

    await rehydrateActiveWorkspace();

    expect(order).toEqual(['config', 'frontendReady', 'session']);
  });

  it('does not start sessionStore.hydrate until frontendReady has fully resolved', async () => {
    // Hold frontendReady open and assert sessionStore.hydrate is not
    // called while it's pending. This is the load-bearing invariant —
    // restore_all_sessions runs server-side as part of frontend_ready,
    // so the React-driven session_resize MUST NOT race it.
    let resolveReady: (() => void) | undefined;
    const readyPromise = new Promise<void>((resolve) => {
      resolveReady = resolve;
    });
    frontendReady.mockImplementation(() => readyPromise);

    const rehydratePromise = rehydrateActiveWorkspace();

    // Yield once to let the configStore.hydrate await resolve.
    await Promise.resolve();
    await Promise.resolve();
    expect(configHydrate).toHaveBeenCalledTimes(1);
    expect(frontendReady).toHaveBeenCalledTimes(1);
    expect(sessionHydrate).not.toHaveBeenCalled();

    resolveReady?.();
    await rehydratePromise;

    expect(sessionHydrate).toHaveBeenCalledTimes(1);
  });

  it('bails after the configStore.hydrate await when a newer rehydrate has superseded it', async () => {
    let resolveFirstConfig: (() => void) | undefined;
    const firstConfig = new Promise<void>((resolve) => {
      resolveFirstConfig = resolve;
    });
    configHydrate.mockImplementationOnce(() => firstConfig);

    const firstCall = rehydrateActiveWorkspace();
    // Second call bumps the generation counter while the first is
    // still awaiting configStore.hydrate. The second resolves
    // immediately (default mock).
    await rehydrateActiveWorkspace();

    resolveFirstConfig?.();
    await firstCall;

    // Both calls hydrate config, but only the second (winner) drives
    // frontendReady + sessionStore.hydrate to completion.
    expect(configHydrate).toHaveBeenCalledTimes(2);
    expect(frontendReady).toHaveBeenCalledTimes(1);
    expect(sessionHydrate).toHaveBeenCalledTimes(1);
  });
});
