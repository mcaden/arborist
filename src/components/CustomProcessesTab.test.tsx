import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import { CustomProcessesTab } from './CustomProcessesTab';
import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import type { CustomProcessDef } from '@/types/arborist';

function seedDefs(defs: CustomProcessDef[]): void {
  useConfigStore.setState({
    config: {
      configVersion: 10,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: '',
      workspaceRoot: null,
      worktreeRoots: [],
      worktreePrepCommands: [],
      aiLaunchCommands: { commands: {}, iconDataUris: {} },
      pluginSettings: { ai: {}, customProcess: {}, dashboardWidget: {} },
      repoCommandTrust: { records: {} },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: defs,
      lastOpenSubSessions: [],
      worktreeTabs: [],
      worktreeTabOrder: [],
      activeWorktreeTabId: null,
    },
    status: 'ready',
    error: null,
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
});

afterEach(() => {
  vi.clearAllMocks();
  seedDefs([]);
});

describe('CustomProcessesTab', () => {
  it('renders one row per persisted def with id locked', () => {
    seedDefs([
      { id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh -i', enabled: true },
      { id: 'vscode', name: 'VS Code', kind: 'application', command: 'code .', enabled: false },
    ]);
    render(<CustomProcessesTab onClose={() => {}} />);
    const idInputs = screen.getAllByLabelText(/^ID for/i);
    expect(idInputs).toHaveLength(2);
    for (const input of idInputs) expect(input).toHaveAttribute('readonly');
  });

  it('Save is disabled when nothing changed', () => {
    seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }]);
    render(<CustomProcessesTab onClose={() => {}} />);
    expect(screen.getByTestId('custom-processes-save')).toBeDisabled();
  });

  it('toggling enabled marks dirty and saves the full list', async () => {
    seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }]);
    const onClose = vi.fn();
    render(<CustomProcessesTab onClose={onClose} />);
    const toggle = screen.getByLabelText(/^Enabled: shell$/i);
    fireEvent.click(toggle);
    await act(async () => {
      screen.getByTestId('custom-processes-save').click();
    });
    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      customProcesses: [{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: false }],
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Add row appends a blank editable row with editable id', () => {
    seedDefs([]);
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('custom-processes-add'));
    const idInput = screen.getByLabelText(/^ID for new launcher$/i);
    expect(idInput).not.toHaveAttribute('readonly');
  });

  it('saves a newly added row with trimmed values', async () => {
    seedDefs([]);
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('custom-processes-add'));
    fireEvent.change(screen.getByLabelText(/^ID for new launcher$/i), {
      target: { value: 'lazygit' },
    });
    fireEvent.change(screen.getByLabelText(/^Name for lazygit$/i), {
      target: { value: '  Lazygit  ' },
    });
    fireEvent.change(screen.getByLabelText(/^Command for lazygit$/i), {
      target: { value: '  lazygit  ' },
    });
    await act(async () => {
      screen.getByTestId('custom-processes-save').click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      customProcesses: [{ id: 'lazygit', name: 'Lazygit', kind: 'terminal', command: 'lazygit', enabled: true }],
    });
  });

  it('blocks save and shows inline error for bad id', () => {
    seedDefs([]);
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('custom-processes-add'));
    fireEvent.change(screen.getByLabelText(/^ID for new launcher$/i), {
      target: { value: 'bad id!' },
    });
    fireEvent.change(screen.getByLabelText(/^Name for/i), { target: { value: 'X' } });
    fireEvent.change(screen.getByLabelText(/^Command for/i), { target: { value: 'x' } });
    expect(screen.getByText(/ID must match/i)).toBeInTheDocument();
    expect(screen.getByTestId('custom-processes-save')).toBeDisabled();
  });

  it('blocks save and shows inline error for blank name and command', () => {
    seedDefs([]);
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('custom-processes-add'));
    fireEvent.change(screen.getByLabelText(/^ID for new launcher$/i), {
      target: { value: 'foo' },
    });
    expect(screen.getByText(/Name is required/i)).toBeInTheDocument();
    expect(screen.getByText(/Command is required/i)).toBeInTheDocument();
    expect(screen.getByTestId('custom-processes-save')).toBeDisabled();
  });

  it('blocks save and surfaces duplicate-id error on both rows', () => {
    seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }]);
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('custom-processes-add'));
    fireEvent.change(screen.getByLabelText(/^ID for new launcher$/i), {
      target: { value: 'shell' },
    });
    expect(screen.getAllByText(/ID must be unique/i).length).toBeGreaterThanOrEqual(1);
    expect(screen.getByTestId('custom-processes-save')).toBeDisabled();
  });

  it('Delete row removes it and lets the remaining list be saved', async () => {
    seedDefs([
      { id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true },
      { id: 'vscode', name: 'VS Code', kind: 'application', command: 'code .', enabled: true },
    ]);
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.click(screen.getByLabelText(/^Delete vscode$/i));
    await act(async () => {
      screen.getByTestId('custom-processes-save').click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      customProcesses: [{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }],
    });
  });

  it('Kind select switches between terminal and application', async () => {
    seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }]);
    render(<CustomProcessesTab onClose={() => {}} />);
    const select = screen.getByLabelText(/^Kind for shell$/i);
    fireEvent.change(select, { target: { value: 'application' } });
    await act(async () => {
      screen.getByTestId('custom-processes-save').click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      customProcesses: [{ id: 'shell', name: 'Shell', kind: 'application', command: 'sh', enabled: true }],
    });
  });

  it('surfaces backend errors without closing the tab', async () => {
    seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }]);
    bridgeMock.configSet.mockRejectedValueOnce(new Error('rejected by backend'));
    const onClose = vi.fn();
    render(<CustomProcessesTab onClose={onClose} />);
    fireEvent.click(screen.getByLabelText(/^Enabled: shell$/i));
    await act(async () => {
      screen.getByTestId('custom-processes-save').click();
    });
    expect(screen.getByTestId('settings-error')).toHaveTextContent('rejected by backend');
    expect(onClose).not.toHaveBeenCalled();
  });

  it('shows the empty-state message when there are no defs', () => {
    seedDefs([]);
    render(<CustomProcessesTab onClose={() => {}} />);
    expect(screen.getByTestId('custom-processes-empty')).toBeInTheDocument();
  });

  it('preserves the optional icon hint across an unrelated edit', async () => {
    seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true, icon: '🐚' }]);
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.change(screen.getByLabelText(/^Name for shell$/i), { target: { value: 'My Shell' } });
    await act(async () => {
      screen.getByTestId('custom-processes-save').click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      customProcesses: [
        {
          id: 'shell',
          name: 'My Shell',
          kind: 'terminal',
          command: 'sh',
          enabled: true,
          icon: '🐚',
        },
      ],
    });
  });

  it('updates the in-memory config snapshot after saving (so the Launch menu reflects edits)', async () => {
    seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }]);
    // Mirror what the backend would persist+return: the toggled def
    // (icon backfill produces no URI for the bare `sh` test stub).
    bridgeMock.configSet.mockResolvedValueOnce({
      ...useConfigStore.getState().config,
      customProcesses: [{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: false }],
    });
    render(<CustomProcessesTab onClose={() => {}} />);
    fireEvent.click(screen.getByLabelText(/^Enabled: shell$/i));
    await act(async () => {
      screen.getByTestId('custom-processes-save').click();
    });
    expect(useConfigStore.getState().config.customProcesses).toEqual([
      { id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: false },
    ]);
  });

  it('hydrates from a fresh persisted snapshot when there are no unsaved edits', () => {
    seedDefs([]);
    const { rerender } = render(<CustomProcessesTab onClose={() => {}} />);
    expect(screen.getByTestId('custom-processes-empty')).toBeInTheDocument();
    act(() => {
      seedDefs([{ id: 'shell', name: 'Shell', kind: 'terminal', command: 'sh', enabled: true }]);
    });
    rerender(<CustomProcessesTab onClose={() => {}} />);
    const row = screen.getByTestId('custom-process-row-shell');
    expect(within(row).getByLabelText(/^Name for shell$/i)).toHaveValue('Shell');
  });
});
