// Tests for the 2-step NewSessionDialog. Bridge mocked wholesale.

import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useNewSessionDialog } from '@/store/new-session-dialog-store';
import { useSessionStore } from '@/store/session-store';
import type { AppConfig, InstructionSet, SessionView, WorktreeInfo } from '@/types/arborist';

import { NewSessionDialog } from './NewSessionDialog';

const REPO_ROOT = '/repos/arborist';

function defaultConfig(overrides: Partial<AppConfig> = {}): AppConfig {
  return {
    configVersion: 3,
    defaultInstructionSets: { claude: '', copilot: '' },
    instructionSetsDir: '/sets',
    workspaceRoot: REPO_ROOT,
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

    expect(await screen.findByText(/new session — step 1 of 2/i)).toBeInTheDocument();
    const next = screen.getByRole('button', { name: /^next$/i });
    expect(next).toBeDisabled();

    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    expect(next).toBeEnabled();
  });

  it('moves focus to the first interactive control on open', async () => {
    render(<NewSessionDialog />);
    openDialog();
    await screen.findByText(/step 1 of 2/i);
    expect(screen.getByRole('radio', { name: /claude/i })).toHaveFocus();
  });

  it('Step 2 lists worktrees from `.worktrees/` and supports manual Browse', async () => {
    bridgeMock.worktreesList.mockResolvedValue([
      // Main checkout — filtered out (not under .worktrees/).
      makeWt(REPO_ROOT, 'main', true),
      // Linked worktree — kept.
      makeWt(`${REPO_ROOT}/.worktrees/feature`, 'feature'),
    ]);
    bridgeMock.pickDirectory.mockResolvedValue('/somewhere/else');

    render(<NewSessionDialog />);
    openDialog();

    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));

    await screen.findByText(/step 2 of 2/i);
    fireEvent.click(screen.getByRole('tab', { name: /^existing$/i }));

    // Only the linked worktree shows up; the main checkout (REPO_ROOT,
    // outside .worktrees/) is filtered out.
    const featureBtn = await screen.findByRole('button', {
      name: /\.worktrees\/feature.*feature/i,
    });
    expect(screen.queryByText(new RegExp(`^${REPO_ROOT}$`))).not.toBeInTheDocument();

    // Selecting one enables Create.
    fireEvent.click(featureBtn);
    expect(screen.getByRole('button', { name: /create session/i })).toBeEnabled();

    // Browse calls the bridge and replaces the selection.
    fireEvent.click(screen.getByRole('button', { name: /browse/i }));
    await waitFor(() => expect(bridgeMock.pickDirectory).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/Selected: \/somewhere\/else/i)).toBeInTheDocument();
  });

  it('Step 2 shows the .worktrees/ empty-state and still allows Browse', async () => {
    bridgeMock.worktreesList.mockResolvedValue([
      // Only the main checkout — filtered out, list ends up empty.
      makeWt(REPO_ROOT, 'main', true),
    ]);
    bridgeMock.pickDirectory.mockResolvedValue('/manual/pick');

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /copilot/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    fireEvent.click(await screen.findByRole('tab', { name: /^existing$/i }));

    expect(await screen.findByText(/no worktrees found in/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /create session/i })).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: /browse/i }));
    await waitFor(() => expect(bridgeMock.pickDirectory).toHaveBeenCalled());
    expect(await screen.findByText(/Selected: \/manual\/pick/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /create session/i })).toBeEnabled();
  });

  it('Step 2 New tab validates the name and creates worktree+session on submit', async () => {
    bridgeMock.worktreesList.mockResolvedValue([]);
    bridgeMock.worktreeCreate.mockResolvedValue({
      path: `${REPO_ROOT}/.worktrees/my-feature`,
    });
    bridgeMock.sessionCreate.mockResolvedValue({
      id: 'new-id',
      tool: 'claude',
      worktreePath: `${REPO_ROOT}/.worktrees/my-feature`,
      worktreeName: 'my-feature',
      label: 'my-feature',
      status: 'running',
      createdAt: 1,
      tabIndex: 0,
    } satisfies SessionView);

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);

    // New tab is the default landing tab on Step 2.
    const input = await screen.findByLabelText(/branch \/ worktree name/i);

    // Invalid name: contains a space.
    fireEvent.change(input, { target: { value: 'bad name' } });
    expect(await screen.findByRole('alert')).toHaveTextContent(/space/i);
    expect(screen.getByRole('button', { name: /^create worktree & session$/i })).toBeDisabled();

    // Valid name enables the Create button.
    fireEvent.change(input, { target: { value: 'my-feature' } });
    const createBtn = screen.getByRole('button', { name: /^create worktree & session$/i });
    expect(createBtn).toBeEnabled();

    // Create — both bridge calls happen and the dialog closes.
    fireEvent.click(createBtn);
    await waitFor(() => expect(bridgeMock.worktreeCreate).toHaveBeenCalledWith('my-feature'));
    await waitFor(() =>
      expect(bridgeMock.sessionCreate).toHaveBeenCalledWith({
        tool: 'claude',
        worktreePath: `${REPO_ROOT}/.worktrees/my-feature`,
      }),
    );
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('Step 2 New tab surfaces backend worktree-create errors', async () => {
    bridgeMock.worktreesList.mockResolvedValue([]);
    bridgeMock.worktreeCreate.mockRejectedValue(new Error('branch already exists'));

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);

    const input = await screen.findByLabelText(/branch \/ worktree name/i);
    fireEvent.change(input, { target: { value: 'already-there' } });
    fireEvent.click(screen.getByRole('button', { name: /^create worktree & session$/i }));

    expect(await screen.findByText(/branch already exists/i)).toBeInTheDocument();
    expect(bridgeMock.sessionCreate).not.toHaveBeenCalled();
    expect(useNewSessionDialog.getState().isOpen).toBe(true);
  });

  it('Step 2 New tab surfaces session-create failures while preserving the worktree', async () => {
    bridgeMock.worktreesList.mockResolvedValue([]);
    bridgeMock.worktreeCreate.mockResolvedValue({
      path: `${REPO_ROOT}/.worktrees/my-feature`,
    });
    bridgeMock.sessionCreate.mockRejectedValue(new Error('spawn failed'));

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);

    const input = await screen.findByLabelText(/branch \/ worktree name/i);
    fireEvent.change(input, { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create worktree & session$/i }));

    expect(await screen.findByText(/spawn failed/i)).toBeInTheDocument();
    // Dialog stays open; user can retry from the Existing tab.
    expect(useNewSessionDialog.getState().isOpen).toBe(true);
    expect(await screen.findByText(/Label will be:/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /create session/i })).toBeEnabled();
  });

  it('does not expose the Existing-mode "Create session" button while the chained session-create is in flight', async () => {
    bridgeMock.worktreesList.mockResolvedValue([]);
    bridgeMock.worktreeCreate.mockResolvedValue({
      path: `${REPO_ROOT}/.worktrees/my-feature`,
    });
    // Hold session creation pending so we can observe the in-flight UI.
    let resolveSession: (value: SessionView) => void = () => {};
    bridgeMock.sessionCreate.mockImplementation(
      () =>
        new Promise<SessionView>((resolve) => {
          resolveSession = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);

    const input = await screen.findByLabelText(/branch \/ worktree name/i);
    fireEvent.change(input, { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create worktree & session$/i }));

    // While the chained session-create is pending, the New-mode footer button
    // is the only primary action visible (showing its loading label) — the
    // Existing-mode "Create session" button must NOT be exposed yet, since
    // clicking it would spawn a second concurrent session for the same worktree.
    await waitFor(() => expect(bridgeMock.sessionCreate).toHaveBeenCalled());
    expect(screen.queryByRole('button', { name: /^create session$/i })).toBeNull();
    expect(screen.getByRole('button', { name: /creating/i })).toBeDisabled();

    // Resolve the pending session so React can flush the close.
    act(() => {
      resolveSession({
        id: 'new-id',
        tool: 'claude',
        worktreePath: `${REPO_ROOT}/.worktrees/my-feature`,
        worktreeName: 'my-feature',
        label: 'my-feature',
        status: 'running',
        createdAt: 1,
        tabIndex: 0,
      } satisfies SessionView);
    });
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('disables Cancel and Back while the chained worktree+session create is in flight', async () => {
    bridgeMock.worktreesList.mockResolvedValue([]);
    bridgeMock.worktreeCreate.mockResolvedValue({
      path: `${REPO_ROOT}/.worktrees/my-feature`,
    });
    let resolveSession: (value: SessionView) => void = () => {};
    bridgeMock.sessionCreate.mockImplementation(
      () =>
        new Promise<SessionView>((resolve) => {
          resolveSession = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);

    const input = await screen.findByLabelText(/branch \/ worktree name/i);
    fireEvent.change(input, { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create worktree & session$/i }));

    // While the chained call is in flight, neither Cancel nor Back may be
    // clickable: closing the dialog now would orphan the in-flight session
    // spawn (no AbortSignal plumbed to the backend).
    await waitFor(() => expect(bridgeMock.sessionCreate).toHaveBeenCalled());
    expect(screen.getByRole('button', { name: /^cancel$/i })).toBeDisabled();
    expect(screen.getByRole('button', { name: /^back$/i })).toBeDisabled();

    // Resolve the pending session so the dialog closes cleanly.
    act(() => {
      resolveSession({
        id: 'new-id',
        tool: 'claude',
        worktreePath: `${REPO_ROOT}/.worktrees/my-feature`,
        worktreeName: 'my-feature',
        label: 'my-feature',
        status: 'running',
        createdAt: 1,
        tabIndex: 0,
      } satisfies SessionView);
    });
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('Confirm submits without an instructionSetId so the backend launches the CLI from the worktree cwd', async () => {
    bridgeMock.worktreesList.mockResolvedValue([
      makeWt(`${REPO_ROOT}/.worktrees/main`, 'main', false),
    ]);
    bridgeMock.sessionCreate.mockResolvedValue({
      id: 'new-id',
      tool: 'claude',
      worktreePath: `${REPO_ROOT}/.worktrees/main`,
      worktreeName: 'main',
      label: 'main',
      status: 'running',
      createdAt: 1,
      tabIndex: 0,
    } satisfies SessionView);

    render(<NewSessionDialog />);
    openDialog();

    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    fireEvent.click(await screen.findByRole('tab', { name: /^existing$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /\.worktrees\/main/i }));
    fireEvent.click(screen.getByRole('button', { name: /create session/i }));

    await waitFor(() =>
      expect(bridgeMock.sessionCreate).toHaveBeenCalledWith({
        tool: 'claude',
        worktreePath: `${REPO_ROOT}/.worktrees/main`,
      }),
    );
    // Defensive: assert the field was not silently included as undefined
    // either, so the contract change is unambiguous on the wire.
    expect(bridgeMock.sessionCreate.mock.calls[0]?.[0]).not.toHaveProperty('instructionSetId');
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('Confirm surfaces backend AppError objects as readable text (not [object Object])', async () => {
    bridgeMock.worktreesList.mockResolvedValue([
      makeWt(`${REPO_ROOT}/.worktrees/main`, 'main', false),
    ]);
    bridgeMock.instructionsList.mockResolvedValue([makeInstr('copilot-default', 'copilot', true)]);
    // Tauri serialises Rust `AppError` as `{ code, message }` and rejects
    // the invoke promise with that bare object — not an `Error`. Without
    // `formatError`, this would render as "[object Object]".
    bridgeMock.sessionCreate.mockRejectedValue({
      code: 'PtySpawnFailed',
      message: 'failed to spawn copilot: program not found',
    });

    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /copilot/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    fireEvent.click(await screen.findByRole('tab', { name: /^existing$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /\.worktrees\/main/i }));
    fireEvent.click(await screen.findByRole('button', { name: /create session/i }));

    expect(await screen.findByText(/PtySpawnFailed.*program not found/i)).toBeInTheDocument();
    expect(screen.queryByText(/\[object Object\]/i)).not.toBeInTheDocument();
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
    expect(screen.getByText(/step 1 of 2/i)).toBeInTheDocument();
  });

  it('moves focus to the first interactive control of the new step on advance/back (#8.1)', async () => {
    bridgeMock.worktreesList.mockResolvedValue([
      makeWt(`${REPO_ROOT}/.worktrees/feature`, 'feature'),
    ]);
    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);
    // The first focusable in step 2's body is the "New" tab button (default mode).
    const newTab = screen.getByRole('tab', { name: /^new$/i });
    expect(newTab).toHaveFocus();
  });

  it('focuses the currently-selected tab on Step 2 re-entry after Back/Next (#8.1)', async () => {
    bridgeMock.worktreesList.mockResolvedValue([
      makeWt(`${REPO_ROOT}/.worktrees/feature`, 'feature'),
    ]);
    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);
    // Switch to Existing.
    fireEvent.click(screen.getByRole('tab', { name: /^existing$/i }));
    // Back to Step 1.
    fireEvent.click(screen.getByRole('button', { name: /^back$/i }));
    await screen.findByText(/step 1 of 2/i);
    // Forward again — focus should land on the still-selected Existing tab,
    // not the first focusable (New).
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);
    const existingTab = screen.getByRole('tab', { name: /^existing$/i });
    expect(existingTab).toHaveFocus();
  });

  it('typing in the Step 2 New tab does not steal focus back to the tab strip (#8.1 regression)', async () => {
    bridgeMock.worktreesList.mockResolvedValue([]);
    render(<NewSessionDialog />);
    openDialog();
    fireEvent.click(screen.getByRole('radio', { name: /claude/i }));
    fireEvent.click(screen.getByRole('button', { name: /^next$/i }));
    await screen.findByText(/step 2 of 2/i);
    // Switch to the New sub-mode and focus the name input.
    fireEvent.click(screen.getByRole('tab', { name: /^new$/i }));
    const nameInput = await screen.findByLabelText(/branch \/ worktree name/i);
    nameInput.focus();
    expect(nameInput).toHaveFocus();
    // Typing must not cause the step-transition focus effect to refire
    // and pull focus back to the first focusable in the step body.
    fireEvent.change(nameInput, { target: { value: 'feature-x' } });
    expect(nameInput).toHaveFocus();
  });
});
