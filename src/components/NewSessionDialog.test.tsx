// Tests for the single-step NewSessionDialog. Bridge mocked wholesale.

import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useConfigStore } from '@/store/config-store';
import { useNewSessionDialog } from '@/store/new-session-dialog-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { AppConfig, WorktreeInfo, WorktreeTab } from '@/types/arborist';

import { NewSessionDialog } from './NewSessionDialog';

const REPO_ROOT = '/repos/arborist';

function defaultConfig(overrides: Partial<AppConfig> = {}): AppConfig {
  return {
    configVersion: 11,
    workspaceRoot: REPO_ROOT,
    worktreeRoots: [REPO_ROOT],
    worktreePrepCommands: ['nvm use 20'],
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
    theme: 'system',
    ...overrides,
  };
}

function makeWt(path: string, branch?: string, isMain = false): WorktreeInfo {
  return { path, isMain, isLocked: false, ...(branch !== undefined ? { branch } : {}) };
}

function makeTab(path: string): WorktreeTab {
  const name = path.split('/').at(-1) ?? path;
  return {
    id: `tab-${name}`,
    path,
    name,
    label: name,
    tabIndex: 0,
    iconId: 1,
  } satisfies WorktreeTab;
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
  useWorktreeTabStore.setState({ tabs: [], activeId: null, isHydrated: true });

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

  it('opens with New tab active and Create & open disabled until name entered', async () => {
    render(<NewSessionDialog />);
    openDialog();

    expect(await screen.findByRole('heading', { name: /add worktree/i })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: /^new$/i })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tab', { name: /^existing$/i })).toHaveAttribute('aria-selected', 'false');
    expect(screen.getByRole('button', { name: /^create & open$/i })).toBeDisabled();

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    expect(screen.getByRole('button', { name: /^create & open$/i })).toBeEnabled();
  });

  it('moves focus to the first interactive control on open', async () => {
    render(<NewSessionDialog />);
    openDialog();

    await screen.findByRole('heading', { name: /add worktree/i });
    expect(screen.getByRole('tab', { name: /^new$/i })).toHaveFocus();
  });

  it('lists worktrees and supports Browse', async () => {
    bridgeMock.worktreesList.mockResolvedValue([makeWt(REPO_ROOT, 'main', true), makeWt(`${REPO_ROOT}/.arborist/.worktrees/feature`, 'feature')]);
    bridgeMock.pickDirectory.mockResolvedValue('/somewhere/else');

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.click(screen.getByRole('tab', { name: /^existing$/i }));

    const featureBtn = await screen.findByRole('button', { name: /\.arborist\/\.worktrees\/feature.*feature/i });
    expect(featureBtn).toBeInTheDocument();
    expect(screen.queryByText(new RegExp(`^${REPO_ROOT}$`))).not.toBeInTheDocument();

    fireEvent.click(featureBtn);
    expect(screen.getByRole('button', { name: /^open worktree$/i })).toBeEnabled();

    fireEvent.click(screen.getByRole('button', { name: /^browse\.\.\.$/i }));
    await waitFor(() => expect(bridgeMock.pickDirectory).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/selected: \/somewhere\/else/i)).toBeInTheDocument();
    expect(screen.getByText(/label will be:/i)).toHaveTextContent(/else/i);
  });

  it('shows empty state and allows Browse', async () => {
    bridgeMock.worktreesList.mockResolvedValue([makeWt(REPO_ROOT, 'main', true)]);
    bridgeMock.pickDirectory.mockResolvedValue('/manual/pick');

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.click(screen.getByRole('tab', { name: /^existing$/i }));

    expect(await screen.findByText(/no worktrees found in/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^open worktree$/i })).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: /^browse\.\.\.$/i }));
    await waitFor(() => expect(bridgeMock.pickDirectory).toHaveBeenCalledTimes(1));
    expect(await screen.findByText(/selected: \/manual\/pick/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^open worktree$/i })).toBeEnabled();
  });

  it('New tab validates name and creates worktree on submit', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockResolvedValue(makeTab(path));

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    const input = screen.getByLabelText(/branch \/ worktree name/i);

    fireEvent.change(input, { target: { value: 'bad name' } });
    expect(await screen.findByRole('alert')).toHaveTextContent(/space/i);
    expect(screen.getByRole('button', { name: /^create & open$/i })).toBeDisabled();

    fireEvent.change(input, { target: { value: 'my-feature' } });
    const createBtn = screen.getByRole('button', { name: /^create & open$/i });
    expect(createBtn).toBeEnabled();

    fireEvent.click(createBtn);

    await waitFor(() => expect(bridgeMock.worktreeCreate).toHaveBeenCalledWith('my-feature'));
    await waitFor(() => expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path }));
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('validates trimmed name', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockResolvedValue(makeTab(path));

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    const input = screen.getByLabelText(/branch \/ worktree name/i);

    fireEvent.change(input, { target: { value: '   ' } });
    expect(screen.queryByRole('alert')).toBeNull();
    expect(screen.getByRole('button', { name: /^create & open$/i })).toBeDisabled();

    fireEvent.change(input, { target: { value: '  my-feature  ' } });
    expect(screen.queryByRole('alert')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    await waitFor(() => expect(bridgeMock.worktreeCreate).toHaveBeenCalledWith('my-feature'));
    await waitFor(() => expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path }));
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('surfaces backend worktree-create errors', async () => {
    bridgeMock.worktreeCreate.mockRejectedValue(new Error('branch already exists'));

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'already-there' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    expect(await screen.findByText(/branch already exists/i)).toBeInTheDocument();
    expect(bridgeMock.worktreeTabOpen).not.toHaveBeenCalled();
    expect(useNewSessionDialog.getState().isOpen).toBe(true);
  });

  it('surfaces open-tab failures while preserving the worktree', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockRejectedValue(new Error('open failed'));
    bridgeMock.worktreesList.mockResolvedValueOnce([]).mockResolvedValueOnce([makeWt(path, 'my-feature')]);

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    expect(await screen.findByText(/open failed/i)).toBeInTheDocument();
    await waitFor(() => expect(bridgeMock.worktreesList).toHaveBeenCalledTimes(2));

    const existingTab = screen.getByRole('tab', { name: /^existing$/i });
    const retry = await screen.findByRole('button', { name: /^open worktree$/i });
    expect(existingTab).toHaveAttribute('aria-selected', 'true');
    expect(await screen.findByText(new RegExp(`Selected:\\s*${path.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`))).toBeInTheDocument();
    expect(retry).toBeEnabled();
    await waitFor(() => expect(retry).toHaveFocus());
  });

  it('retry button immediately after open failure', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    let resolveRefresh: (value: WorktreeInfo[]) => void = () => {};

    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockRejectedValue(new Error('open failed'));
    bridgeMock.worktreesList.mockResolvedValueOnce([]).mockImplementationOnce(
      () =>
        new Promise<WorktreeInfo[]>((resolve) => {
          resolveRefresh = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    const retry = await screen.findByRole('button', { name: /^open worktree$/i });
    expect(retry).toBeEnabled();
    expect(screen.queryByRole('button', { name: /^create & open$/i })).toBeNull();

    await act(async () => {
      resolveRefresh([]);
    });
  });

  it('does not expose retry while open is in flight', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    let resolveOpen: (value: WorktreeTab) => void = () => {};

    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockImplementation(
      () =>
        new Promise<WorktreeTab>((resolve) => {
          resolveOpen = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    await waitFor(() => expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path }));
    expect(screen.queryByRole('button', { name: /^open worktree$/i })).toBeNull();
    expect(screen.getByRole('button', { name: /creating…/i })).toBeDisabled();

    await act(async () => {
      resolveOpen(makeTab(path));
    });
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('disables Cancel while create is in flight', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    let resolveOpen: (value: WorktreeTab) => void = () => {};

    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockImplementation(
      () =>
        new Promise<WorktreeTab>((resolve) => {
          resolveOpen = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    await waitFor(() => expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path }));
    expect(screen.getByRole('button', { name: /^cancel$/i })).toBeDisabled();

    await act(async () => {
      resolveOpen(makeTab(path));
    });
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('disables tabs while create is in flight', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    let resolveOpen: (value: WorktreeTab) => void = () => {};

    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockImplementation(
      () =>
        new Promise<WorktreeTab>((resolve) => {
          resolveOpen = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    await waitFor(() => expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path }));

    const newTab = screen.getByRole('tab', { name: /^new$/i });
    const existingTab = screen.getByRole('tab', { name: /^existing$/i });
    expect(newTab).toBeDisabled();
    expect(existingTab).toBeDisabled();

    fireEvent.click(existingTab);
    expect(screen.getByRole('button', { name: /creating…/i })).toBeInTheDocument();
    fireEvent.keyDown(newTab.parentElement!, { key: 'ArrowRight' });
    expect(screen.getByRole('button', { name: /creating…/i })).toBeInTheDocument();

    await act(async () => {
      resolveOpen(makeTab(path));
    });
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('ignores stale worktreesList when workspaceRoot flips to null', async () => {
    let resolveList: (value: WorktreeInfo[]) => void = () => {};
    bridgeMock.worktreesList.mockImplementation(
      () =>
        new Promise<WorktreeInfo[]>((resolve) => {
          resolveList = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    await waitFor(() => expect(bridgeMock.worktreesList).toHaveBeenCalledTimes(1));

    act(() => {
      useConfigStore.setState({ config: defaultConfig({ workspaceRoot: null }) });
    });

    fireEvent.click(screen.getByRole('tab', { name: /^existing$/i }));

    await act(async () => {
      resolveList([makeWt(`${REPO_ROOT}/.arborist/.worktrees/stale`, 'stale')]);
    });

    expect(screen.queryByRole('button', { name: /\.arborist\/\.worktrees\/stale/i })).not.toBeInTheDocument();
    expect(screen.queryByText(/^loading\.\.\.$/i)).not.toBeInTheDocument();
  });

  it('ignores worktreesList after unmount', async () => {
    let resolveList: (value: WorktreeInfo[]) => void = () => {};
    bridgeMock.worktreesList.mockImplementation(
      () =>
        new Promise<WorktreeInfo[]>((resolve) => {
          resolveList = resolve;
        }),
    );

    const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});

    try {
      const { unmount } = render(<NewSessionDialog />);
      openDialog();
      await waitFor(() => expect(bridgeMock.worktreesList).toHaveBeenCalledTimes(1));

      unmount();

      await act(async () => {
        resolveList([makeWt(`${REPO_ROOT}/.arborist/.worktrees/late`, 'late')]);
      });

      const offending = errorSpy.mock.calls.find((args) => {
        const msg = String(args[0] ?? '');
        return msg.includes('not wrapped in act') || msg.includes('unmounted component');
      });
      expect(offending).toBeUndefined();
    } finally {
      errorSpy.mockRestore();
    }
  });

  it("stale list doesn't overwrite post-failure refresh", async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    const stale = [makeWt(`${REPO_ROOT}/.arborist/.worktrees/old-feature`, 'old-feature')];
    const fresh = [makeWt(`${REPO_ROOT}/.arborist/.worktrees/old-feature`, 'old-feature'), makeWt(path, 'my-feature')];
    let resolveStaleList: (value: WorktreeInfo[]) => void = () => {};
    let resolveFreshList: (value: WorktreeInfo[]) => void = () => {};
    let listCallCount = 0;

    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockRejectedValue(new Error('open failed'));
    bridgeMock.worktreesList.mockImplementation(
      () =>
        new Promise<WorktreeInfo[]>((resolve) => {
          listCallCount += 1;
          if (listCallCount === 1) resolveStaleList = resolve;
          else resolveFreshList = resolve;
        }),
    );

    render(<NewSessionDialog />);
    openDialog();
    await waitFor(() => expect(listCallCount).toBe(1));

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    await waitFor(() => expect(listCallCount).toBe(2));

    await act(async () => {
      resolveFreshList(fresh);
    });
    expect(await screen.findByRole('button', { name: /\.arborist\/\.worktrees\/my-feature.*my-feature/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /\.arborist\/\.worktrees\/old-feature.*old-feature/i })).toBeInTheDocument();

    await act(async () => {
      resolveStaleList(stale);
    });
    expect(screen.getByRole('button', { name: /\.arborist\/\.worktrees\/my-feature.*my-feature/i })).toBeInTheDocument();
  });

  it('Existing tab open calls worktreeTabOpen with selected path', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/main`;
    bridgeMock.worktreesList.mockResolvedValue([makeWt(path, 'main')]);
    bridgeMock.worktreeTabOpen.mockResolvedValue(makeTab(path));

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.click(screen.getByRole('tab', { name: /^existing$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /\.arborist\/\.worktrees\/main.*main/i }));
    fireEvent.click(screen.getByRole('button', { name: /^open worktree$/i }));

    await waitFor(() => expect(bridgeMock.worktreeTabOpen).toHaveBeenCalledWith({ path }));
    await waitFor(() => expect(useNewSessionDialog.getState().isOpen).toBe(false));
  });

  it('Confirm surfaces backend AppError objects as readable text', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/main`;
    bridgeMock.worktreesList.mockResolvedValue([makeWt(path, 'main')]);
    bridgeMock.worktreeTabOpen.mockRejectedValue({
      code: 'PtySpawnFailed',
      message: 'failed to open worktree tab: program not found',
    });

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.click(screen.getByRole('tab', { name: /^existing$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /\.arborist\/\.worktrees\/main.*main/i }));
    fireEvent.click(screen.getByRole('button', { name: /^open worktree$/i }));

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
    act(() => {
      useNewSessionDialog.setState({ isOpen: true });
    });

    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: /add worktree/i })).toBeInTheDocument();
  });

  it('focuses the retry button after create succeeds but open fails', async () => {
    const path = `${REPO_ROOT}/.arborist/.worktrees/my-feature`;
    bridgeMock.worktreeCreate.mockResolvedValue({ path, prep: null });
    bridgeMock.worktreeTabOpen.mockRejectedValue(new Error('open failed'));
    bridgeMock.worktreesList.mockResolvedValue([]);

    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    fireEvent.change(screen.getByLabelText(/branch \/ worktree name/i), { target: { value: 'my-feature' } });
    fireEvent.click(screen.getByRole('button', { name: /^create & open$/i }));

    const retry = await screen.findByRole('button', { name: /^open worktree$/i });
    await waitFor(() => expect(retry).toHaveFocus());
  });

  it('typing in the New tab input does not steal focus back to the tab strip', async () => {
    render(<NewSessionDialog />);
    openDialog();
    await screen.findByRole('heading', { name: /add worktree/i });

    const nameInput = screen.getByLabelText(/branch \/ worktree name/i);
    nameInput.focus();
    expect(nameInput).toHaveFocus();

    fireEvent.change(nameInput, { target: { value: 'feature-x' } });
    expect(nameInput).toHaveFocus();
  });
});
