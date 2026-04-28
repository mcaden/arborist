// Tests for the 3-step NewSessionDialog. Bridge mocked wholesale.

import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useNewSessionDialog } from '@/store/new-session-dialog-store';
import { useSessionStore } from '@/store/session-store';
import type { AppConfig, InstructionSet, SessionView, WorktreeInfo } from '@/types/grove';

import { NewSessionDialog } from './NewSessionDialog';

const REPO_ROOT = '/repos/grove';

function defaultConfig(overrides: Partial<AppConfig> = {}): AppConfig {
  return {
    configVersion: 2,
    defaultInstructionSets: { claude: '', copilot: '' },
    instructionSetsDir: '/sets',
    worktreeRoots: [REPO_ROOT],
    prelaunchCommands: ['nvm use 20'],
    worktreePrelaunchCommands: {},
    lastOpenSessions: [],
    tabOrder: [],
    activeSessionId: null,
    ...overrides,
  };
}

function makeWt(path: string, branch?: string, isMain = false): WorktreeInfo {
  return { path, isMain, isLocked: false, ...(branch !== undefined ? { branch } : {}) };
}

function makeInstr(id: string, tool: 'claude' | 'copilot', isDefault = false): InstructionSet {
  return { id, name: id, tool, filePath: `/sets/${id}.md`, isDefault };
}

function openDialog(): void {
  act(() => {
    useNewSessionDialog.setState({ isOpen: true });
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useNewSessionDialog.setState({ isOpen: false });
  useConfigStore.setState({ config: defaultConfig(), status: 'ready', error: null });
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    pendingClose: undefined,
    isHydrated: true,
  });

  // jsdom <dialog> shim — same pattern as Sidebar.test.tsx.
  const proto = HTMLDialogElement.prototype as unknown as {
    showModal?: () => void;
    close?: () => void;
  };
  if (typeof proto.showModal !== 'function') {
    proto.showModal = function showModal(this: HTMLDialogElement) {
      this.setAttribute('open', '');
    };
  }
  if (typeof proto.close !== 'function') {
    proto.close = function close(this: HTMLDialogElement) {
      this.removeAttribute('open');
    };
  }
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('NewSessionDialog', () => {
  it('renders nothing when closed', () => {
    const { container } = render(<NewSessionDialog />);
    expect(container.firstChild).toBeNull();
  });

  it('opens on Step 1 with Next disabled until a tool is chosen', async () => {
    render(<NewSessionDialog />);
    openDialog();

    expect(await screen.findByText(/new session — step 1 of 3/i)).toBeInTheDocument();
    const next = screen.getByRole('button', { name: /^next$/i });
    expect(next).toBeDisabled();

    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    expect(next).toBeEnabled();
  });

  it('moves focus to the first interactive control on open', async () => {
    render(<NewSessionDialog />);
    openDialog();
    await screen.findByText(/step 1 of 3/i);
    expect(screen.getByRole('radio', { name: /claude/i })).toHaveFocus();
  });

  it('Step 2 lists worktrees from the bridge and supports manual Browse', async () => {
    bridgeMock.worktreesList.mockResolvedValue([
      makeWt('/repos/grove', 'main', true),
      makeWt('/repos/grove-feature', 'feature'),
    ]);
    bridgeMock.pickDirectory.mockResolvedValue('/somewhere/else');

    render(<NewSessionDialog />);
    openDialog();

    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));

    await screen.findByText(/step 2 of 3/i);

    // Quick-pick rendered both worktrees with branch + main badges.
    const mainBtn = await screen.findByRole('button', { name: /\/repos\/grove\b.*main/i });
    expect(within(mainBtn).getAllByText(/main/i).length).toBeGreaterThanOrEqual(2);
    expect(
      screen.getByRole('button', { name: /\/repos\/grove-feature.*feature/i }),
    ).toBeInTheDocument();

    // Selecting one enables Next.
    fireEvent.click(mainBtn);
    expect(screen.getByRole('button', { name: /^next$/i })).toBeEnabled();

    // Browse calls the bridge and replaces the selection.
    fireEvent.click(screen.getByRole('button', { name: /browse/i }));
    await waitFor(() => expect(bridgeMock.pickDirectory).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/Selected: \/somewhere\/else/i)).toBeInTheDocument();
  });

  it('Step 2 shows the manual fallback when no worktrees are detected', async () => {
    // Empty config means no roots queried; bridge would otherwise return [].
    useConfigStore.setState({
      config: defaultConfig({ worktreeRoots: [] }),
      status: 'ready',
      error: null,
    });
    bridgeMock.pickDirectory.mockResolvedValue('/manual/pick');

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /copilot/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));

    expect(await screen.findByText(/could not detect git worktrees/i)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /^next$/i })).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: /browse/i }));
    await waitFor(() => expect(bridgeMock.pickDirectory).toHaveBeenCalled());
    expect(await screen.findByText(/Selected: \/manual\/pick/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^next$/i })).toBeEnabled();
  });

  it('Step 3 filters instruction sets by tool, includes (none), and re-filters when tool changes', async () => {
    bridgeMock.worktreesList.mockResolvedValue([makeWt('/repos/grove', 'main', true)]);
    bridgeMock.instructionsList.mockResolvedValue([
      makeInstr('claude-default', 'claude', true),
      makeInstr('claude-strict', 'claude'),
      makeInstr('copilot-default', 'copilot', true),
    ]);

    render(<NewSessionDialog />);
    openDialog();

    // Step 1 → Claude
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    // Step 2 → pick worktree
    fireEvent.click(await screen.findByRole('button', { name: /\/repos\/grove\b.*main/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));

    // Step 3 visible
    await screen.findByText(/step 3 of 3/i);
    expect(screen.getByRole('radio', { name: /^\(none\)$/i })).toBeInTheDocument();
    expect(screen.getByRole('radio', { name: /claude-default/i })).toBeInTheDocument();
    expect(screen.getByRole('radio', { name: /claude-strict/i })).toBeInTheDocument();
    expect(screen.queryByRole('radio', { name: /copilot-default/i })).toBeNull();

    // Pre-launch commands preview is visible.
    expect(screen.getByText(/nvm use 20/)).toBeInTheDocument();

    // Pick claude-default, then back twice to flip the tool.
    fireEvent.click(screen.getByRole('radio', { name: /claude-default/i }));
    fireEvent.click(screen.getByRole('button', { name: /back/i }));
    fireEvent.click(screen.getByRole('button', { name: /back/i }));
    fireEvent.click(screen.getByRole('radio', { name: /copilot/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /\/repos\/grove\b.*main/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));

    // Now copilot-only should be visible and the previously-selected
    // claude-default selection should have been cleared.
    expect(await screen.findByRole('radio', { name: /copilot-default/i })).toBeInTheDocument();
    expect(screen.queryByRole('radio', { name: /claude-default/i })).toBeNull();
    expect(screen.getByRole('radio', { name: /^\(none\)$/i })).toBeChecked();
  });

  it('Confirm calls actions.create with the right payload and closes the dialog', async () => {
    bridgeMock.worktreesList.mockResolvedValue([makeWt('/repos/grove', 'main', true)]);
    bridgeMock.instructionsList.mockResolvedValue([makeInstr('claude-default', 'claude', true)]);
    bridgeMock.sessionCreate.mockResolvedValue({
      id: 'new-id',
      tool: 'claude',
      worktreePath: '/repos/grove',
      worktreeName: 'grove',
      label: 'grove',
      instructionSetId: 'claude-default',
      status: 'running',
      createdAt: 1,
      tabIndex: 0,
    } satisfies SessionView);

    render(<NewSessionDialog />);
    openDialog();

    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /\/repos\/grove\b.*main/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    fireEvent.click(await screen.findByRole('radio', { name: /claude-default/i }));
    fireEvent.click(screen.getByRole('button', { name: /create session/i }));

    await waitFor(() =>
      expect(bridgeMock.sessionCreate).toHaveBeenCalledWith({
        tool: 'claude',
        worktreePath: '/repos/grove',
        instructionSetId: 'claude-default',
      }),
    );
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('Confirm with the (none) instruction set submits the per-tool default id', async () => {
    useConfigStore.setState({
      config: defaultConfig({
        defaultInstructionSets: { claude: 'claude-default', copilot: 'copilot-default' },
      }),
      status: 'ready',
      error: null,
    });
    bridgeMock.worktreesList.mockResolvedValue([makeWt('/repos/grove', 'main', true)]);
    bridgeMock.sessionCreate.mockResolvedValue({
      id: 'x',
      tool: 'claude',
      worktreePath: '/repos/grove',
      worktreeName: 'grove',
      label: 'grove',
      instructionSetId: 'claude-default',
      status: 'running',
      createdAt: 1,
      tabIndex: 0,
    });

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /\/repos\/grove\b/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    // Default selection is (none); just confirm.
    fireEvent.click(await screen.findByRole('button', { name: /create session/i }));

    await waitFor(() => expect(bridgeMock.sessionCreate).toHaveBeenCalled());
    expect(bridgeMock.sessionCreate.mock.calls[0]?.[0].instructionSetId).toBe('claude-default');
  });

  it('Esc (native dialog cancel) closes the dialog', async () => {
    render(<NewSessionDialog />);
    openDialog();
    const dialog = (await screen.findByRole('dialog')) as HTMLDialogElement;
    fireEvent(dialog, new Event('cancel', { bubbles: false, cancelable: true }));
    expect(useNewSessionDialog.getState().isOpen).toBe(false);
  });

  it('opening twice in a row does not throw', async () => {
    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('dialog');
    act(() => {
      useNewSessionDialog.setState({ isOpen: false });
    });
    // Re-open immediately.
    act(() => {
      useNewSessionDialog.setState({ isOpen: true });
    });
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    // And state was reset to Step 1.
    expect(screen.getByText(/step 1 of 3/i)).toBeInTheDocument();
  });
});
