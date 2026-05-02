// Regression for PR #32 review finding: when `workspaceSwitch`
// resolves Ok the backend's `workspace://changed` emit is
// best-effort (Rust side only `warn!`s on failure and the command
// still resolves Ok). Without a frontend-driven fallback an emit
// failure would leave the UI pointed at the old workspace. This
// test pins that `changeWorkspace` always re-hydrates on a real
// switch (so the fallback fires even if the listener never does)
// and *skips* rehydrate on a no-op switch (cheap fast path).

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { changeWorkspace } from './workspace-switch';
import { frontendReady, resetBridgeMocks, workspaceSwitch } from '@/lib/tauri-bridge.mock';
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

describe('changeWorkspace', () => {
  it('drives a frontend rehydrate on success so the UI converges even if the backend emit is dropped', async () => {
    workspaceSwitch.mockResolvedValue({ workspaceRoot: '/new', noOp: false });

    await changeWorkspace('/new');

    expect(workspaceSwitch).toHaveBeenCalledWith('/new');
    expect(configHydrate).toHaveBeenCalledTimes(1);
    expect(sessionHydrate).toHaveBeenCalledTimes(1);
    expect(frontendReady).toHaveBeenCalledTimes(1);
  });

  it('skips the rehydrate on a no-op switch (already on requested workspace)', async () => {
    workspaceSwitch.mockResolvedValue({ workspaceRoot: '/cur', noOp: true });

    await changeWorkspace('/cur');

    expect(configHydrate).not.toHaveBeenCalled();
    expect(sessionHydrate).not.toHaveBeenCalled();
    expect(frontendReady).not.toHaveBeenCalled();
  });

  it('translates WorkspaceLocked into a user-facing error and skips rehydrate', async () => {
    workspaceSwitch.mockRejectedValue({
      code: 'WorkspaceLocked',
      message: 'busy',
    });

    await expect(changeWorkspace('/locked')).rejects.toThrow(/already open in another/i);
    expect(configHydrate).not.toHaveBeenCalled();
  });
});
