import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import { SettingsDialog } from './SettingsDialog';
import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

function seedConfig(
  overrides: Partial<{
    workspaceRoot: string | null;
    instructionSetsDir: string;
    prelaunchCommands: string[];
    aiLaunchCommands: { claude: string; copilot: string };
    worktreesDir: string;
  }> = {},
): void {
  useConfigStore.setState({
    config: {
      configVersion: 5,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: overrides.instructionSetsDir ?? '/cfg/instr',
      workspaceRoot: overrides.workspaceRoot ?? '/work',
      worktreeRoots: [],
      prelaunchCommands: overrides.prelaunchCommands ?? [],
      worktreePrelaunchCommands: {},
      aiLaunchCommands: overrides.aiLaunchCommands ?? { claude: '', copilot: '' },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: [],
      lastOpenSubSessions: [],
      worktreesDir: overrides.worktreesDir ?? '.worktrees',
    },
    status: 'ready',
    error: null,
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    pendingClose: undefined,
    isHydrated: true,
    statusMessages: {},
  });
});

afterEach(() => {
  // Reset config store between tests by re-seeding the empty default.
  useConfigStore.setState({
    config: {
      configVersion: 5,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: '',
      workspaceRoot: null,
      worktreeRoots: [],
      prelaunchCommands: [],
      worktreePrelaunchCommands: {},
      aiLaunchCommands: { claude: '', copilot: '' },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: [],
      lastOpenSubSessions: [],
      worktreesDir: '.worktrees',
    },
    status: 'idle',
    error: null,
  });
});

describe('SettingsDialog', () => {
  it('shows the current workspace root, instructions dir, and prelaunch commands', () => {
    seedConfig({
      workspaceRoot: '/repos/grove',
      instructionSetsDir: '/cfg/instr',
      prelaunchCommands: ['source ~/.zshenv', 'nvm use 20'],
    });
    render(<SettingsDialog onClose={() => {}} />);
    expect(screen.getByTestId('settings-workspace-path')).toHaveTextContent('/repos/grove');
    expect(screen.getByLabelText(/instruction sets directory/i)).toHaveValue('/cfg/instr');
    expect(screen.getByLabelText(/pre-launch commands/i)).toHaveValue('source ~/.zshenv\nnvm use 20');
  });

  it('Save button is disabled until something changes', () => {
    seedConfig();
    render(<SettingsDialog onClose={() => {}} />);
    const save = screen.getByRole('button', { name: /^save$/i });
    expect(save).toBeDisabled();
    fireEvent.change(screen.getByLabelText(/instruction sets directory/i), {
      target: { value: '/new/instr' },
    });
    expect(save).toBeEnabled();
  });

  it('persists only the changed fields and closes on success', async () => {
    seedConfig({
      instructionSetsDir: '/old',
      prelaunchCommands: ['echo a'],
    });
    const onClose = vi.fn();
    render(<SettingsDialog onClose={onClose} />);
    fireEvent.change(screen.getByLabelText(/pre-launch commands/i), {
      target: { value: 'echo a\necho b\n' },
    });
    await act(async () => {
      screen.getByRole('button', { name: /^save$/i }).click();
    });
    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      prelaunchCommands: ['echo a', 'echo b'],
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('parses the prelaunch textarea by trimming and dropping blank lines', async () => {
    seedConfig({ prelaunchCommands: [] });
    render(<SettingsDialog onClose={() => {}} />);
    fireEvent.change(screen.getByLabelText(/pre-launch commands/i), {
      target: { value: '  source ~/.zshenv  \n\n  nvm use 20\n' },
    });
    await act(async () => {
      screen.getByRole('button', { name: /^save$/i }).click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      prelaunchCommands: ['source ~/.zshenv', 'nvm use 20'],
    });
  });

  it('surfaces backend errors without closing', async () => {
    seedConfig({ instructionSetsDir: '/old' });
    bridgeMock.configSet.mockRejectedValueOnce(new Error('bad path'));
    const onClose = vi.fn();
    render(<SettingsDialog onClose={onClose} />);
    fireEvent.change(screen.getByLabelText(/instruction sets directory/i), {
      target: { value: 'rel/path' },
    });
    await act(async () => {
      screen.getByRole('button', { name: /^save$/i }).click();
    });
    expect(screen.getByTestId('settings-error')).toHaveTextContent('bad path');
    expect(onClose).not.toHaveBeenCalled();
  });

  it('Browse… button populates the instructions input from the directory picker', async () => {
    seedConfig({ instructionSetsDir: '' });
    bridgeMock.pickDirectory.mockResolvedValueOnce('/picked/dir');
    render(<SettingsDialog onClose={() => {}} />);
    await act(async () => {
      screen.getByRole('button', { name: /browse/i }).click();
    });
    expect(screen.getByLabelText(/instruction sets directory/i)).toHaveValue('/picked/dir');
  });

  it('clicking the backdrop closes the dialog', () => {
    seedConfig();
    const onClose = vi.fn();
    render(<SettingsDialog onClose={onClose} />);
    fireEvent.click(screen.getByTestId('settings-dialog'));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Cancel button closes the dialog without saving', () => {
    seedConfig();
    const onClose = vi.fn();
    render(<SettingsDialog onClose={onClose} />);
    fireEvent.change(screen.getByLabelText(/instruction sets directory/i), {
      target: { value: '/something/new' },
    });
    fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(bridgeMock.configSet).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('shows General tab by default and switches panels when the tab is clicked', () => {
    seedConfig();
    render(<SettingsDialog onClose={() => {}} />);
    expect(screen.getByTestId('settings-panel-general')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-custom-processes')).toBeNull();
    expect(screen.getByTestId('settings-tab-general')).toHaveAttribute('aria-selected', 'true');
    fireEvent.click(screen.getByTestId('settings-tab-custom-processes'));
    expect(screen.getByTestId('settings-panel-custom-processes')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-general')).toBeNull();
    expect(screen.getByTestId('settings-tab-custom-processes')).toHaveAttribute('aria-selected', 'true');
  });

  it('honours initialTab="customProcesses" so the empty-launch handoff lands on the right tab', () => {
    seedConfig();
    render(<SettingsDialog onClose={() => {}} initialTab="customProcesses" />);
    expect(screen.getByTestId('settings-panel-custom-processes')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-general')).toBeNull();
  });

  it('Arrow keys move between tabs (WAI-ARIA tab keyboard model)', () => {
    seedConfig();
    render(<SettingsDialog onClose={() => {}} />);
    const generalTab = screen.getByTestId('settings-tab-general');
    const customTab = screen.getByTestId('settings-tab-custom-processes');
    generalTab.focus();
    fireEvent.keyDown(generalTab, { key: 'ArrowRight' });
    expect(customTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-custom-processes')).toBeInTheDocument();
    fireEvent.keyDown(customTab, { key: 'ArrowLeft' });
    expect(generalTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-general')).toBeInTheDocument();
  });

  it('shows the configured AI agent launch commands and persists edits', async () => {
    seedConfig({ aiLaunchCommands: { claude: 'npx claude', copilot: '' } });
    const onClose = vi.fn();
    render(<SettingsDialog onClose={onClose} />);

    const claudeInput = screen.getByTestId('settings-launch-claude') as HTMLInputElement;
    const copilotInput = screen.getByTestId('settings-launch-copilot') as HTMLInputElement;
    expect(claudeInput.value).toBe('npx claude');
    expect(copilotInput.value).toBe('');
    expect(copilotInput.placeholder).toBe('copilot');

    fireEvent.change(claudeInput, { target: { value: 'claude --model sonnet' } });
    fireEvent.change(copilotInput, { target: { value: 'gh copilot' } });
    await act(async () => {
      screen.getByRole('button', { name: /^save$/i }).click();
    });

    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      aiLaunchCommands: { claude: 'claude --model sonnet', copilot: 'gh copilot' },
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('clearing an AI launch command persists empty string (revert to default)', async () => {
    seedConfig({ aiLaunchCommands: { claude: 'npx claude', copilot: '' } });
    render(<SettingsDialog onClose={() => {}} />);
    const claudeInput = screen.getByTestId('settings-launch-claude') as HTMLInputElement;
    fireEvent.change(claudeInput, { target: { value: '   ' } });
    await act(async () => {
      screen.getByRole('button', { name: /^save$/i }).click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      aiLaunchCommands: { claude: '' },
    });
  });

  describe('worktrees folder section (Issue #53)', () => {
    beforeEach(() => {
      vi.useFakeTimers();
    });
    afterEach(() => {
      vi.runOnlyPendingTimers();
      vi.useRealTimers();
    });

    it('shows the configured worktrees folder', () => {
      seedConfig({ worktreesDir: 'wt' });
      render(<SettingsDialog onClose={() => {}} />);
      expect(screen.getByTestId('settings-worktrees-dir')).toHaveValue('wt');
    });

    it('debounces the live check to ~250ms and uses latest-wins ordering', async () => {
      seedConfig();
      bridgeMock.worktreesDirCheck.mockResolvedValue({
        resolvedPath: '/work/.worktrees',
        insideRepo: true,
        gitIgnored: true,
      });
      render(<SettingsDialog onClose={() => {}} />);
      // Drain the initial mount-driven check.
      await act(async () => {
        vi.advanceTimersByTime(260);
      });
      bridgeMock.worktreesDirCheck.mockClear();

      const input = screen.getByTestId('settings-worktrees-dir');
      fireEvent.change(input, { target: { value: 'a' } });
      await act(async () => {
        vi.advanceTimersByTime(100);
      });
      // Still within debounce window — no call yet.
      expect(bridgeMock.worktreesDirCheck).not.toHaveBeenCalled();
      // A new keystroke resets the timer.
      fireEvent.change(input, { target: { value: 'ab' } });
      await act(async () => {
        vi.advanceTimersByTime(100);
      });
      expect(bridgeMock.worktreesDirCheck).not.toHaveBeenCalled();
      await act(async () => {
        vi.advanceTimersByTime(160);
      });
      // Only the latest value should fire — exactly one call with `ab`.
      expect(bridgeMock.worktreesDirCheck).toHaveBeenCalledTimes(1);
      expect(bridgeMock.worktreesDirCheck).toHaveBeenLastCalledWith('ab');
    });

    it('shows the warning when the resolved path is inside the repo and not gitignored', async () => {
      seedConfig();
      bridgeMock.worktreesDirCheck.mockResolvedValue({
        resolvedPath: '/work/inside',
        insideRepo: true,
        gitIgnored: false,
      });
      render(<SettingsDialog onClose={() => {}} />);
      fireEvent.change(screen.getByTestId('settings-worktrees-dir'), { target: { value: 'inside' } });
      await act(async () => {
        vi.advanceTimersByTime(260);
      });
      expect(screen.getByTestId('settings-worktrees-dir-warning')).toBeInTheDocument();
    });

    it('hides the warning when the path is gitignored', async () => {
      seedConfig();
      bridgeMock.worktreesDirCheck.mockResolvedValue({
        resolvedPath: '/work/.worktrees',
        insideRepo: true,
        gitIgnored: true,
      });
      render(<SettingsDialog onClose={() => {}} />);
      fireEvent.change(screen.getByTestId('settings-worktrees-dir'), { target: { value: '.worktrees' } });
      await act(async () => {
        vi.advanceTimersByTime(260);
      });
      expect(screen.queryByTestId('settings-worktrees-dir-warning')).toBeNull();
    });

    it('hides the warning when the path is outside the workspace', async () => {
      seedConfig();
      bridgeMock.worktreesDirCheck.mockResolvedValue({
        resolvedPath: '/var/wt',
        insideRepo: false,
        gitIgnored: false,
      });
      render(<SettingsDialog onClose={() => {}} />);
      fireEvent.change(screen.getByTestId('settings-worktrees-dir'), { target: { value: '/var/wt' } });
      await act(async () => {
        vi.advanceTimersByTime(260);
      });
      expect(screen.queryByTestId('settings-worktrees-dir-warning')).toBeNull();
    });

    it('persists the new worktrees dir on save', async () => {
      seedConfig({ worktreesDir: '.worktrees' });
      bridgeMock.worktreesDirCheck.mockResolvedValue({
        resolvedPath: '/work/wt',
        insideRepo: true,
        gitIgnored: true,
      });
      render(<SettingsDialog onClose={() => {}} />);
      fireEvent.change(screen.getByTestId('settings-worktrees-dir'), { target: { value: 'wt' } });
      await act(async () => {
        vi.advanceTimersByTime(260);
      });
      await act(async () => {
        screen.getByRole('button', { name: /^save$/i }).click();
      });
      expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({ worktreesDir: 'wt' });
    });

    it('whitespace-only input collapses to the default and does not mark dirty', async () => {
      seedConfig({ worktreesDir: '.worktrees' });
      render(<SettingsDialog onClose={() => {}} />);
      fireEvent.change(screen.getByTestId('settings-worktrees-dir'), { target: { value: '   ' } });
      await act(async () => {
        vi.advanceTimersByTime(260);
      });
      expect(screen.getByRole('button', { name: /^save$/i })).toBeDisabled();
    });
  });
});
