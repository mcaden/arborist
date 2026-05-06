import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { WorkspaceIndicator } from './WorkspaceIndicator';
import {
  configSet,
  resetBridgeMocks,
  workspaceSwitch,
  workspaceValidate,
} from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

vi.mock('@/lib/tauri-bridge', () => import('@/lib/tauri-bridge.mock'));

function seedStores(workspaceRoot: string | null): void {
  useConfigStore.setState({
    config: {
      configVersion: 3,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: '',
      workspaceRoot,
      worktreeRoots: [],
      prelaunchCommands: [],
      worktreePrelaunchCommands: {},
      aiLaunchCommands: { claude: '', copilot: '' },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
    },
    status: 'ready',
    error: null,
  });
  useSessionStore.setState({ sessions: [], activeId: undefined, isHydrated: true });
}

beforeEach(() => {
  resetBridgeMocks();
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  vi.useRealTimers();
});

describe('WorkspaceIndicator', () => {
  it('renders nothing when workspaceRoot is null', () => {
    seedStores(null);
    const { container } = render(<WorkspaceIndicator />);
    expect(container.firstChild).toBeNull();
  });

  it('shows the basename of the workspace root and a tooltip with the full path', () => {
    seedStores('/Users/dev/projects/grove');
    render(<WorkspaceIndicator />);
    expect(screen.getByText('grove')).toBeInTheDocument();
    expect(screen.getByText('grove')).toHaveAttribute('title', '/Users/dev/projects/grove');
  });

  it('handles Windows-style separators in the workspace root', () => {
    seedStores('C:\\Users\\dev\\projects\\grove');
    render(<WorkspaceIndicator />);
    expect(screen.getByText('grove')).toBeInTheDocument();
  });

  it('Change… opens the picker and delegates the switch to the backend', async () => {
    seedStores('/old/workspace');
    const closeMock = vi.fn().mockResolvedValue(undefined);
    useSessionStore.setState({
      sessions: [{ id: 's1' as never } as never, { id: 's2' as never } as never],
    });
    // Replace the session actions to record close() calls so we can
    // assert the frontend does NOT close sessions itself any more —
    // the backend's `workspace_switch` does that transactionally.
    useSessionStore.setState((s) => ({
      actions: { ...s.actions, close: closeMock },
    }));
    workspaceValidate.mockResolvedValue({ valid: true });
    workspaceSwitch.mockResolvedValue({
      workspaceRoot: '/new',
      noOp: false,
      config: {
        configVersion: 4,
        defaultInstructionSets: { claude: '', copilot: '' },
        instructionSetsDir: '',
        workspaceRoot: '/new',
        worktreeRoots: [],
        prelaunchCommands: [],
        worktreePrelaunchCommands: {},
        aiLaunchCommands: { claude: '', copilot: '' },
        lastOpenSessions: [],
        tabOrder: [],
        activeSessionId: null,
      },
      sessions: [],
    });

    render(<WorkspaceIndicator />);
    fireEvent.click(screen.getByRole('button', { name: /change/i }));
    expect(await screen.findByRole('heading', { name: /change workspace/i })).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText(/workspace path/i), { target: { value: '/new' } });
    await act(async () => {
      vi.advanceTimersByTime(300);
      await Promise.resolve();
    });

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /switch workspace/i }));
      await Promise.resolve();
      await Promise.resolve();
    });

    await waitFor(() => {
      expect(workspaceSwitch).toHaveBeenCalledWith('/new');
    });
    // The frontend must not directly close sessions or patch the
    // config store; both are the backend's responsibility now.
    expect(closeMock).not.toHaveBeenCalled();
    expect(configSet).not.toHaveBeenCalled();
  });
});
