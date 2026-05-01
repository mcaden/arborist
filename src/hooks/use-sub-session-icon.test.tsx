import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SessionId, SubSession, SubSessionId } from '@/types/arborist';

import { useSubSessionIcon } from './use-sub-session-icon';

const PARENT: SessionId = '00000000-0000-0000-0000-000000000a01' as SessionId;
const SUB_ID: SubSessionId = '11111111-1111-1111-1111-111111111101' as SubSessionId;

function makeApp(overrides: Partial<SubSession> = {}): SubSession {
  return {
    id: SUB_ID,
    parentSessionId: PARENT,
    defId: 'vscode',
    kind: 'application',
    label: 'VS Code',
    status: 'running',
    pid: 1234,
    composedCommand: 'code .',
    createdAt: 0,
    ...overrides,
  } as SubSession;
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSubSessionStore.setState({
    subSessions: [],
    activeByParent: {},
    statusMessages: {},
    isHydrated: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('useSubSessionIcon', () => {
  it('returns the resolved data URI for a running application sub-session', async () => {
    bridgeMock.subSessionIcon.mockResolvedValueOnce('data:image/png;base64,AAA=');
    useSubSessionStore.setState({ subSessions: [makeApp()] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    await waitFor(() => expect(result.current).toBe('data:image/png;base64,AAA='));
    expect(bridgeMock.subSessionIcon).toHaveBeenCalledWith(SUB_ID);
  });

  it('returns undefined and does not query for terminal sub-sessions', async () => {
    useSubSessionStore.setState({ subSessions: [makeApp({ kind: 'terminal' })] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    // Give the effect a chance to (incorrectly) run.
    await new Promise((r) => setTimeout(r, 10));
    expect(result.current).toBeUndefined();
    expect(bridgeMock.subSessionIcon).not.toHaveBeenCalled();
  });

  it('does not query when the sub-session has no pid', async () => {
    useSubSessionStore.setState({ subSessions: [makeApp({ pid: undefined })] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    await new Promise((r) => setTimeout(r, 10));
    expect(result.current).toBeUndefined();
    expect(bridgeMock.subSessionIcon).not.toHaveBeenCalled();
  });

  it('does not query when status is not running', async () => {
    useSubSessionStore.setState({ subSessions: [makeApp({ status: 'starting' })] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    await new Promise((r) => setTimeout(r, 10));
    expect(result.current).toBeUndefined();
    expect(bridgeMock.subSessionIcon).not.toHaveBeenCalled();
  });

  it('discards a late response after the pid changed mid-flight (vscode retarget)', async () => {
    // First lookup against pid=1000 will resolve to a stale icon AFTER
    // the pid has already been retargeted to 2000.
    let resolveFirst: (val: string | null) => void;
    const firstPromise = new Promise<string | null>((res) => {
      resolveFirst = res;
    });
    bridgeMock.subSessionIcon.mockImplementationOnce(() => firstPromise);

    useSubSessionStore.setState({ subSessions: [makeApp({ pid: 1000 })] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    // Effect fires for pid=1000.
    await waitFor(() => expect(bridgeMock.subSessionIcon).toHaveBeenCalledTimes(1));

    // Pid changes — second effect fires AND gets a fresh result.
    bridgeMock.subSessionIcon.mockResolvedValueOnce('data:image/png;base64,FRESH=');
    act(() => {
      useSubSessionStore.setState({ subSessions: [makeApp({ pid: 2000 })] });
    });

    await waitFor(() => expect(bridgeMock.subSessionIcon).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current).toBe('data:image/png;base64,FRESH='));

    // Now the stale (pid=1000) lookup finally resolves — must NOT
    // overwrite the fresh icon, because pid no longer matches.
    act(() => {
      resolveFirst!('data:image/png;base64,STALE=');
    });
    // Allow any microtasks to drain.
    await new Promise((r) => setTimeout(r, 10));
    expect(result.current).toBe('data:image/png;base64,FRESH=');
  });

  it('keeps showing the fallback when extraction returns null', async () => {
    bridgeMock.subSessionIcon.mockResolvedValueOnce(null);
    useSubSessionStore.setState({ subSessions: [makeApp()] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    await waitFor(() => expect(bridgeMock.subSessionIcon).toHaveBeenCalled());
    // Give the .then a tick.
    await new Promise((r) => setTimeout(r, 10));
    expect(result.current).toBeUndefined();
  });

  it('swallows errors and keeps the fallback rather than crashing the tab', async () => {
    bridgeMock.subSessionIcon.mockRejectedValueOnce(new Error('boom'));
    useSubSessionStore.setState({ subSessions: [makeApp()] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    await waitFor(() => expect(bridgeMock.subSessionIcon).toHaveBeenCalled());
    await new Promise((r) => setTimeout(r, 10));
    expect(result.current).toBeUndefined();
  });
});
