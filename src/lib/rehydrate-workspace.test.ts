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
//
// The later "coalesces a superseded rehydrate" / "serialises calls"
// tests cover a separate concurrency regression: the original
// post-await generation guard let an older rehydrate's `set(...)`
// calls inside `hydrate()` land in the store before its gen check
// ran. Two rapid switches could leave the UI showing stale workspace
// data even though both rehydrates "completed successfully". The fix
// serialises rehydrate calls on a Promise chain and skips any call
// that has been superseded by a newer submission before its turn.

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

  it('coalesces a superseded rehydrate so its hydrate calls do not run', async () => {
    // Regression: the previous post-await generation guard let an older
    // rehydrate's `set(...)` calls inside `hydrate()` land in the store
    // before the gen check ran, so two rapid switches could leave the
    // UI showing stale workspace data. Serialise + skip-when-superseded
    // ensures the older call's hydrate never runs at all when a newer
    // one is queued behind it.
    let resolveFirstConfig: (() => void) | undefined;
    const firstConfig = new Promise<void>((resolve) => {
      resolveFirstConfig = resolve;
    });
    configHydrate.mockImplementationOnce(() => firstConfig);

    const firstCall = rehydrateActiveWorkspace();
    // Second call queues behind the first while it is still awaiting
    // `configStore.hydrate`. Its mere submission must cause the first
    // call to skip its work entirely.
    const secondCall = rehydrateActiveWorkspace();

    resolveFirstConfig?.();
    await firstCall;
    await secondCall;

    // Only the winner (second submission) drives all three stages.
    // The first call returns without touching any of them.
    expect(configHydrate).toHaveBeenCalledTimes(1);
    expect(frontendReady).toHaveBeenCalledTimes(1);
    expect(sessionHydrate).toHaveBeenCalledTimes(1);
  });

  it('serialises calls so an older rehydrate cannot overwrite a newer one', async () => {
    // Regression for the race the round-9 reviewer flagged: even if
    // both `hydrate()` calls `set(...)` synchronously inside their
    // bodies, the chain ensures call A runs to completion before
    // call B starts, and the skip-when-superseded check ensures only
    // the newest run actually mutates the stores. Final order seen
    // by the stores must be the newest workspace's data.
    const order: string[] = [];
    configHydrate.mockImplementation(async () => {
      order.push(`config@${configHydrate.mock.calls.length}`);
    });
    frontendReady.mockImplementation(async () => {
      order.push(`ready@${frontendReady.mock.calls.length}`);
    });
    sessionHydrate.mockImplementation(async () => {
      order.push(`session@${sessionHydrate.mock.calls.length}`);
    });

    const a = rehydrateActiveWorkspace();
    const b = rehydrateActiveWorkspace();
    await Promise.all([a, b]);

    // Older call (A) is superseded and skipped; only B's stages run,
    // and they run in the canonical order.
    expect(order).toEqual(['config@1', 'ready@1', 'session@1']);
  });

  it('keeps the chain alive across a failing rehydrate', async () => {
    // A failing run must not poison the chain — a subsequent rehydrate
    // submitted after the failure has settled still has to execute.
    configHydrate.mockImplementationOnce(async () => {
      throw new Error('boom');
    });

    await expect(rehydrateActiveWorkspace()).rejects.toThrow('boom');

    await rehydrateActiveWorkspace();
    expect(sessionHydrate).toHaveBeenCalledTimes(1);
  });

  it('bails an in-flight rehydrate as soon as a newer call is submitted mid-run', async () => {
    // Regression for PR #32 round-12 review finding: the existing
    // dequeue-time gen check only handles calls queued behind us
    // BEFORE we started. Both call sites in the workspace-switch flow
    // (App-level `workspace://changed` listener AND the explicit
    // fallback rehydrate inside `changeWorkspace`) are triggered by
    // the same backend state change but the second one fires AFTER
    // the listener-driven run has already passed its initial guard.
    // Without mid-run gen checks, both runs would do all three
    // backend round-trips on every successful workspace switch.
    //
    // Trace:
    //   1. Call A is submitted → starts → awaits configStore.hydrate.
    //   2. Call B is submitted (held in queue behind A's chain).
    //   3. A's configStore.hydrate resolves → mid-run check sees
    //      `myGenA(1) < submitted(2)` → A bails (no frontendReady,
    //      no sessionHydrate).
    //   4. B dequeues → runs the full pipeline (1 of each call).
    //
    // Net: configHydrate twice (1 wasted), frontendReady once,
    // sessionHydrate once. Without the mid-run check we'd see 2/2/2.
    let resolveFirstConfig: (() => void) | undefined;
    const firstConfig = new Promise<void>((resolve) => {
      resolveFirstConfig = resolve;
    });
    configHydrate.mockImplementationOnce(() => firstConfig);

    const firstCall = rehydrateActiveWorkspace();
    // Yield once so call A actually starts running and is awaiting
    // configHydrate. Otherwise B is queued before A starts and the
    // existing dequeue-time check (already covered by the
    // "coalesces a superseded rehydrate" test) handles it.
    await Promise.resolve();
    await Promise.resolve();
    expect(configHydrate).toHaveBeenCalledTimes(1);

    // Submit call B while A is mid-flight, parked at configHydrate.
    const secondCall = rehydrateActiveWorkspace();

    // Resolve A's configHydrate so it reaches the mid-run check.
    resolveFirstConfig?.();

    await firstCall;
    await secondCall;

    // A's configHydrate ran (the wasted one we can't avoid). After
    // its mid-run check, A bailed → no frontendReady, no
    // sessionHydrate from A. B then ran fully.
    expect(configHydrate).toHaveBeenCalledTimes(2);
    expect(frontendReady).toHaveBeenCalledTimes(1);
    expect(sessionHydrate).toHaveBeenCalledTimes(1);
  });
});
