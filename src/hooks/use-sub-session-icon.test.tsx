import { renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import { useConfigStore } from '@/store/config-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { CustomProcessDef, SubSession, SubSessionId, WorktreeTabId } from '@/types/arborist';

import { useSubSessionIcon } from './use-sub-session-icon';

const PARENT = 'tab-parent' as WorktreeTabId;
const SUB_ID: SubSessionId = '11111111-1111-1111-1111-111111111101' as SubSessionId;
const ICON_DATA_URI = 'data:image/png;base64,AAA=';

function makeApp(overrides: Partial<SubSession> = {}): SubSession {
  return {
    id: SUB_ID,
    parentWorktreeTabId: PARENT,
    defId: 'vscode',
    kind: 'application',
    label: 'VS Code',
    status: 'running',
    pid: 1234,
    composedCommand: 'code .',
    createdAt: 0,
    ...overrides,
  };
}

function seedDef(overrides: Partial<CustomProcessDef> = {}) {
  const def: CustomProcessDef = {
    id: 'vscode',
    name: 'VS Code',
    kind: 'application',
    command: 'code .',
    enabled: true,
    iconDataUri: ICON_DATA_URI,
    ...overrides,
  };
  useConfigStore.setState((s) => ({
    config: { ...s.config, customProcesses: [def] },
  }));
}

function seedDefWithoutIcon(): void {
  const def: CustomProcessDef = {
    id: 'vscode',
    name: 'VS Code',
    kind: 'application',
    command: 'code .',
    enabled: true,
  };
  useConfigStore.setState((s) => ({
    config: { ...s.config, customProcesses: [def] },
  }));
}

beforeEach(() => {
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
    isHydrated: true,
  });
  useConfigStore.setState((s) => ({
    config: { ...s.config, customProcesses: [] },
  }));
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('useSubSessionIcon', () => {
  it('returns the cached iconDataUri from the backing def', () => {
    seedDef();
    useSubSessionStore.setState({ subSessions: [makeApp()] });

    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));

    expect(result.current).toBe(ICON_DATA_URI);
  });

  it('returns undefined when the sub-session is unknown', () => {
    seedDef();
    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));
    expect(result.current).toBeUndefined();
  });

  it('returns undefined when the def has been deleted (orphan sub-session)', () => {
    useSubSessionStore.setState({ subSessions: [makeApp()] });
    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));
    expect(result.current).toBeUndefined();
  });

  it('returns undefined when the def has no cached iconDataUri', () => {
    seedDefWithoutIcon();
    useSubSessionStore.setState({ subSessions: [makeApp()] });
    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));
    expect(result.current).toBeUndefined();
  });

  it('works the same way for terminal kind', () => {
    seedDef({ kind: 'terminal' });
    useSubSessionStore.setState({ subSessions: [makeApp({ kind: 'terminal' })] });
    const { result } = renderHook(() => useSubSessionIcon(SUB_ID));
    expect(result.current).toBe(ICON_DATA_URI);
  });
});
