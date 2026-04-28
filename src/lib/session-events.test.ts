// Tests for the app-lifetime `session://status` subscription wiring.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import type { SessionView } from '@/types/arborist';

import * as sessionEvents from './session-events';

function makeView(id: string): SessionView {
  return {
    id,
    tool: 'claude',
    worktreePath: `/repo/${id}`,
    worktreeName: id,
    label: id,
    instructionSetId: 'd',
    status: 'starting',
    createdAt: 0,
    tabIndex: 0,
  };
}

beforeEach(() => {
  // Detach any leftover subscription from a prior test, then clear mocks
  // so each test starts with a fresh call count.
  sessionEvents.__resetForTests();
  bridgeMock.resetBridgeMocks();
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    pendingClose: undefined,
    isHydrated: false,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('subscribeToStatus', () => {
  it('attaches onSessionStatus and routes payloads to applyStatus', async () => {
    useSessionStore.setState({ sessions: [makeView('a')] });

    sessionEvents.subscribeToStatus();

    expect(bridgeMock.onSessionStatus).toHaveBeenCalledTimes(1);
    const cb = bridgeMock.onSessionStatus.mock.calls[0]![0]!;
    cb({ sessionId: 'a', status: 'running' });

    expect(useSessionStore.getState().sessions[0]!.status).toBe('running');
  });

  it('is idempotent — a second call does not re-attach', () => {
    sessionEvents.subscribeToStatus();
    const noop = sessionEvents.subscribeToStatus();

    expect(bridgeMock.onSessionStatus).toHaveBeenCalledTimes(1);
    // Returned unlisten from the second call is a no-op; calling it must
    // neither throw nor cause anything observable.
    expect(() => noop()).not.toThrow();
  });

  it('does not export subscribeToOutput (output bypasses the store)', () => {
    expect((sessionEvents as Record<string, unknown>).subscribeToOutput).toBeUndefined();
  });
});
