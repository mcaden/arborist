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

describe('subscribeToActivity', () => {
  it('attaches onSessionActivity and routes payloads to applyActivity', () => {
    useSessionStore.setState({ sessions: [makeView('a')] });

    sessionEvents.subscribeToActivity();

    expect(bridgeMock.onSessionActivity).toHaveBeenCalledTimes(1);
    const cb = bridgeMock.onSessionActivity.mock.calls[0]![0]!;
    cb({ sessionId: 'a', kind: 'working' });

    expect(useSessionStore.getState().activity['a']).toBe('working');
  });

  it('is idempotent — a second call does not re-attach', () => {
    sessionEvents.subscribeToActivity();
    const noop = sessionEvents.subscribeToActivity();

    expect(bridgeMock.onSessionActivity).toHaveBeenCalledTimes(1);
    expect(() => noop()).not.toThrow();
  });
});

describe('subscribeToMetrics', () => {
  it('attaches onSessionMetrics and routes payloads to applyMetrics', () => {
    useSessionStore.setState({ sessions: [makeView('a')] });

    sessionEvents.subscribeToMetrics();

    expect(bridgeMock.onSessionMetrics).toHaveBeenCalledTimes(1);
    const cb = bridgeMock.onSessionMetrics.mock.calls[0]![0]!;
    cb({
      sessionId: 'a',
      contextUsedPct: 42,
      contextTokensUsed: 8400,
      contextTokensLimit: 20000,
      observedAt: 1700000000,
    });

    const m = useSessionStore.getState().metrics['a'];
    expect(m).toBeDefined();
    expect(m!.contextUsedPct).toBe(42);
  });

  it('is idempotent — a second call does not re-attach', () => {
    sessionEvents.subscribeToMetrics();
    const noop = sessionEvents.subscribeToMetrics();

    expect(bridgeMock.onSessionMetrics).toHaveBeenCalledTimes(1);
    expect(() => noop()).not.toThrow();
  });
});
