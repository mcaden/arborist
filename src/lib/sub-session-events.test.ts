// Tests for the app-lifetime `subsession://*` subscription wiring. Mirrors
// `session-events.test.ts`.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SubSession, WorktreeTabId } from '@/types/arborist';

import * as subEvents from './sub-session-events';

function makeSub(id: string, parent: WorktreeTabId, overrides: Partial<SubSession> = {}): SubSession {
  return {
    id,
    parentWorktreeTabId: parent,
    defId: 'shell',
    kind: 'terminal',
    label: id,
    status: 'running',
    composedCommand: 'bash -i',
    createdAt: 0,
    ...overrides,
  };
}

beforeEach(() => {
  subEvents.__resetForTests();
  bridgeMock.resetBridgeMocks();
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
    isHydrated: false,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('subscribeToSubStatus', () => {
  it('attaches onSubSessionStatus and routes payloads to applyStatus', () => {
    useSubSessionStore.setState({ subSessions: [makeSub('s1', 'tab-p1' as WorktreeTabId, { status: 'starting' })] });

    subEvents.subscribeToSubStatus();

    expect(bridgeMock.onSubSessionStatus).toHaveBeenCalledTimes(1);
    const cb = bridgeMock.onSubSessionStatus.mock.calls[0]![0]!;
    cb({ id: 's1', status: 'running', pid: 4242 });

    const sub = useSubSessionStore.getState().subSessions[0]!;
    expect(sub.status).toBe('running');
    expect(sub.pid).toBe(4242);
  });

  it('is idempotent — a second call does not re-attach', () => {
    subEvents.subscribeToSubStatus();
    const noop = subEvents.subscribeToSubStatus();

    expect(bridgeMock.onSubSessionStatus).toHaveBeenCalledTimes(1);
    expect(() => noop()).not.toThrow();
  });

  it('the returned unlisten resets module state so a later call re-attaches', async () => {
    const unlisten = subEvents.subscribeToSubStatus();
    expect(bridgeMock.onSubSessionStatus).toHaveBeenCalledTimes(1);
    unlisten();
    await Promise.resolve();
    subEvents.subscribeToSubStatus();
    expect(bridgeMock.onSubSessionStatus).toHaveBeenCalledTimes(2);
  });
});

describe('subscribeToSubExited', () => {
  it('attaches onSubSessionExited and routes payloads to applyExited', () => {
    useSubSessionStore.setState({ subSessions: [makeSub('s1', 'tab-p1' as WorktreeTabId, { status: 'running' })] });

    subEvents.subscribeToSubExited();

    expect(bridgeMock.onSubSessionExited).toHaveBeenCalledTimes(1);
    const cb = bridgeMock.onSubSessionExited.mock.calls[0]![0]!;
    cb({ id: 's1', exitCode: 1 });

    expect(useSubSessionStore.getState().subSessions[0]!.status).toBe('error');
  });

  it('is idempotent — a second call does not re-attach', () => {
    subEvents.subscribeToSubExited();
    const noop = subEvents.subscribeToSubExited();

    expect(bridgeMock.onSubSessionExited).toHaveBeenCalledTimes(1);
    expect(() => noop()).not.toThrow();
  });
});

describe('module-shape invariants', () => {
  it('does not export a subscribeToSubOutput (output bypasses the store)', () => {
    expect((subEvents as Record<string, unknown>).subscribeToSubOutput).toBeUndefined();
  });
});
