import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import { SettingsDialog } from './SettingsDialog';
import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { PluginRegistryProvider } from '@/plugins';
import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';
import type { PluginSettings } from '@/types/arborist';

function seedConfig(
  overrides: Partial<{
    workspaceRoot: string | null;
    instructionSetsDir: string;
    worktreePrepCommands: string[];
    aiLaunchCommands: { commands: Record<string, string>; iconDataUris: Record<string, string | null> };
    pluginSettings: PluginSettings;
  }> = {},
): void {
  useConfigStore.setState({
    config: {
      configVersion: 10,
      defaultInstructionSets: { claude: '', copilot: '' },
      instructionSetsDir: overrides.instructionSetsDir ?? '/cfg/instr',
      workspaceRoot: overrides.workspaceRoot ?? '/work',
      worktreeRoots: [],
      worktreePrepCommands: overrides.worktreePrepCommands ?? [],
      aiLaunchCommands: overrides.aiLaunchCommands ?? { commands: {}, iconDataUris: {} },
      pluginSettings: overrides.pluginSettings ?? { ai: {}, customProcess: {}, dashboardWidget: {} },
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: [],
      lastOpenSubSessions: [],
      worktreeTabs: [],
      worktreeTabOrder: [],
      activeWorktreeTabId: null,
    },
    status: 'ready',
    error: null,
  });
}

function renderWithPlugins(ui: JSX.Element): ReturnType<typeof render> {
  return render(<PluginRegistryProvider>{ui}</PluginRegistryProvider>);
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    isHydrated: true,
    statusMessages: {},
  });
});

afterEach(() => {
  // Reset config store between tests by re-seeding the empty default.
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
      lastOpenSessions: [],
      tabOrder: [],
      activeSessionId: null,
      customProcesses: [],
      lastOpenSubSessions: [],
      worktreeTabs: [],
      worktreeTabOrder: [],
      activeWorktreeTabId: null,
    },
    status: 'idle',
    error: null,
  });
});

describe('SettingsDialog', () => {
  it('shows the current workspace root, instructions dir, and worktree prep commands', () => {
    seedConfig({
      workspaceRoot: '/repos/grove',
      instructionSetsDir: '/cfg/instr',
      worktreePrepCommands: ['source ~/.zshenv', 'nvm use 20'],
    });
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
    expect(screen.getByTestId('settings-workspace-path')).toHaveTextContent('/repos/grove');
    expect(screen.getByLabelText(/instruction sets directory/i)).toHaveValue('/cfg/instr');
    expect(screen.getByLabelText(/worktree prep commands/i)).toHaveValue('source ~/.zshenv\nnvm use 20');
  });

  it('Save button is disabled until something changes', () => {
    seedConfig();
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
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
      worktreePrepCommands: ['echo a'],
    });
    const onClose = vi.fn();
    renderWithPlugins(<SettingsDialog onClose={onClose} />);
    fireEvent.change(screen.getByLabelText(/worktree prep commands/i), {
      target: { value: 'echo a\necho b\n' },
    });
    await act(async () => {
      screen.getByRole('button', { name: /^save$/i }).click();
    });
    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      worktreePrepCommands: ['echo a', 'echo b'],
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('parses the worktree prep textarea by trimming and dropping blank lines', async () => {
    seedConfig({ worktreePrepCommands: [] });
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
    fireEvent.change(screen.getByLabelText(/worktree prep commands/i), {
      target: { value: '  source ~/.zshenv  \n\n  nvm use 20\n' },
    });
    await act(async () => {
      screen.getByRole('button', { name: /^save$/i }).click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      worktreePrepCommands: ['source ~/.zshenv', 'nvm use 20'],
    });
  });

  it('surfaces backend errors without closing', async () => {
    seedConfig({ instructionSetsDir: '/old' });
    bridgeMock.configSet.mockRejectedValueOnce(new Error('bad path'));
    const onClose = vi.fn();
    renderWithPlugins(<SettingsDialog onClose={onClose} />);
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
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
    await act(async () => {
      screen.getByRole('button', { name: /browse/i }).click();
    });
    expect(screen.getByLabelText(/instruction sets directory/i)).toHaveValue('/picked/dir');
  });

  it('clicking the backdrop closes the dialog', () => {
    seedConfig();
    const onClose = vi.fn();
    renderWithPlugins(<SettingsDialog onClose={onClose} />);
    fireEvent.click(screen.getByTestId('settings-dialog'));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Cancel button closes the dialog without saving', () => {
    seedConfig();
    const onClose = vi.fn();
    renderWithPlugins(<SettingsDialog onClose={onClose} />);
    fireEvent.change(screen.getByLabelText(/instruction sets directory/i), {
      target: { value: '/something/new' },
    });
    fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(bridgeMock.configSet).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('shows General tab by default and switches panels when the tab is clicked', () => {
    seedConfig();
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
    expect(screen.getByTestId('settings-panel-general')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-plugins')).toBeNull();
    expect(screen.queryByTestId('settings-panel-custom-processes')).toBeNull();
    expect(screen.queryByTestId('settings-panel-about')).toBeNull();
    expect(screen.getByTestId('settings-tab-general')).toHaveAttribute('aria-selected', 'true');
    fireEvent.click(screen.getByTestId('settings-tab-plugins'));
    expect(screen.getByTestId('settings-panel-plugins')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-general')).toBeNull();
    expect(screen.queryByTestId('settings-panel-custom-processes')).toBeNull();
    expect(screen.queryByTestId('settings-panel-about')).toBeNull();
    expect(screen.getByTestId('settings-tab-plugins')).toHaveAttribute('aria-selected', 'true');
    fireEvent.click(screen.getByTestId('settings-tab-custom-processes'));
    expect(screen.getByTestId('settings-panel-custom-processes')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-general')).toBeNull();
    expect(screen.queryByTestId('settings-panel-plugins')).toBeNull();
    expect(screen.queryByTestId('settings-panel-about')).toBeNull();
    expect(screen.getByTestId('settings-tab-custom-processes')).toHaveAttribute('aria-selected', 'true');
    fireEvent.click(screen.getByTestId('settings-tab-about'));
    expect(screen.getByTestId('settings-panel-about')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-general')).toBeNull();
    expect(screen.queryByTestId('settings-panel-plugins')).toBeNull();
    expect(screen.queryByTestId('settings-panel-custom-processes')).toBeNull();
    expect(screen.getByTestId('settings-tab-about')).toHaveAttribute('aria-selected', 'true');
  });

  it('honours initialTab="customProcesses" so the empty-launch handoff lands on the right tab', () => {
    seedConfig();
    renderWithPlugins(<SettingsDialog onClose={() => {}} initialTab="customProcesses" />);
    expect(screen.getByTestId('settings-panel-custom-processes')).toBeInTheDocument();
    expect(screen.queryByTestId('settings-panel-general')).toBeNull();
  });

  it('Arrow keys move between tabs (WAI-ARIA tab keyboard model)', () => {
    seedConfig();
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
    const generalTab = screen.getByTestId('settings-tab-general');
    const pluginsTab = screen.getByTestId('settings-tab-plugins');
    const customTab = screen.getByTestId('settings-tab-custom-processes');
    const aboutTab = screen.getByTestId('settings-tab-about');
    generalTab.focus();
    fireEvent.keyDown(generalTab, { key: 'ArrowRight' });
    expect(pluginsTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-plugins')).toBeInTheDocument();
    fireEvent.keyDown(pluginsTab, { key: 'ArrowRight' });
    expect(customTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-custom-processes')).toBeInTheDocument();
    fireEvent.keyDown(customTab, { key: 'ArrowRight' });
    expect(aboutTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-about')).toBeInTheDocument();
    fireEvent.keyDown(aboutTab, { key: 'ArrowLeft' });
    expect(customTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-custom-processes')).toBeInTheDocument();
    fireEvent.keyDown(customTab, { key: 'ArrowLeft' });
    expect(pluginsTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-plugins')).toBeInTheDocument();
    fireEvent.keyDown(pluginsTab, { key: 'ArrowLeft' });
    expect(generalTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('settings-panel-general')).toBeInTheDocument();
  });

  it('shows attribution and project description in the About tab', () => {
    seedConfig();
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('settings-tab-about'));
    expect(screen.getByTestId('settings-about-attribution')).toHaveTextContent('mcaden');
    expect(screen.getByText(/cross-platform desktop app/i)).toBeInTheDocument();
    expect(screen.getByText(/terminal persistent in the background/i)).toBeInTheDocument();
  });

  it('shows the configured AI agent launch commands and persists edits', async () => {
    seedConfig({ aiLaunchCommands: { commands: { claude: 'npx claude', copilot: '' }, iconDataUris: {} } });
    const onClose = vi.fn();
    renderWithPlugins(<SettingsDialog onClose={onClose} />);
    fireEvent.click(screen.getByTestId('settings-tab-plugins'));

    const claudeInput = screen.getByTestId('plugin-ai-claude-launch-command') as HTMLInputElement;
    const copilotInput = screen.getByTestId('plugin-ai-copilot-launch-command') as HTMLInputElement;
    expect(claudeInput.value).toBe('npx claude');
    expect(copilotInput.value).toBe('');
    expect(copilotInput.placeholder).toBe('copilot');

    fireEvent.change(claudeInput, { target: { value: 'claude --model sonnet' } });
    fireEvent.change(copilotInput, { target: { value: 'gh copilot' } });
    await act(async () => {
      screen.getByTestId('plugins-save').click();
    });

    expect(bridgeMock.configSet).toHaveBeenCalledTimes(1);
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      pluginSettings: {
        ai: {
          claude: { settings: { launchCommand: 'claude --model sonnet' } },
          copilot: { settings: { launchCommand: 'gh copilot' } },
        },
      },
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('clearing an AI launch command persists empty string (revert to default)', async () => {
    seedConfig({
      pluginSettings: {
        ai: { claude: { settings: { launchCommand: 'npx claude' } } },
        customProcess: {},
        dashboardWidget: {},
      },
    });
    renderWithPlugins(<SettingsDialog onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('settings-tab-plugins'));
    const claudeInput = screen.getByTestId('plugin-ai-claude-launch-command') as HTMLInputElement;
    fireEvent.change(claudeInput, { target: { value: '   ' } });
    await act(async () => {
      screen.getByTestId('plugins-save').click();
    });
    expect(bridgeMock.configSet.mock.calls[0]![0]).toEqual({
      pluginSettings: { ai: { claude: { settings: { launchCommand: '' } } } },
    });
  });
});
