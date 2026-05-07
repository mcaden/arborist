// Tests for the CloseConfirmDialog busy-state behavior (issue #47).
// Verifies that controls are disabled, a spinner appears, and repeated
// clicks / Esc are blocked while the close action is in flight.

import { act, fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import type { SessionView } from '@/types/arborist';

import { CloseConfirmDialog } from './CloseConfirmDialog';

function makeView(id: string, overrides: Partial<SessionView> = {}): SessionView {
  return {
    id,
    tool: 'claude',
    worktreePath: `/repo/${id}`,
    worktreeName: id,
    label: id,
    instructionSetId: 'default-claude',
    status: 'running',
    createdAt: 1_700_000_000_000,
    tabIndex: 0,
    ...overrides,
  };
}

function seed(sessionId: string): void {
  const view = makeView(sessionId);
  useSessionStore.setState({
    sessions: [view],
    activeId: sessionId,
    pendingClose: sessionId,
    isHydrated: true,
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    pendingClose: undefined,
    isHydrated: false,
    statusMessages: {},
    hasUnread: {},
    activity: {},
    metrics: {},
  });

  // jsdom shims for <dialog>
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

describe('CloseConfirmDialog busy state', () => {
  it('disables buttons and checkbox while close is in flight', async () => {
    // Make sessionClose hang until we resolve it manually.
    let resolveClose!: (v: { worktreeDeleteError: null }) => void;
    bridgeMock.sessionClose.mockImplementation(
      () =>
        new Promise((r) => {
          resolveClose = r;
        }),
    );
    seed('s1');
    render(<CloseConfirmDialog />);

    const terminateBtn = screen.getByRole('button', { name: /terminate/i });
    const cancelBtn = screen.getByRole('button', { name: /cancel/i });
    const checkbox = screen.getByRole('checkbox');

    // All enabled before click.
    expect(terminateBtn).not.toBeDisabled();
    expect(cancelBtn).not.toBeDisabled();
    expect(checkbox).not.toBeDisabled();

    // Click terminate — enters busy state.
    await act(async () => {
      fireEvent.click(terminateBtn);
    });

    expect(terminateBtn).toBeDisabled();
    expect(cancelBtn).toBeDisabled();
    expect(checkbox).toBeDisabled();

    // Spinner visible.
    expect(screen.getByRole('status')).toBeInTheDocument();

    // aria-busy set on dialog.
    expect(screen.getByRole('dialog')).toHaveAttribute('aria-busy', 'true');

    // Resolve to clean up.
    await act(async () => {
      resolveClose({ worktreeDeleteError: null });
    });
  });

  it('shows spinner with accessible label while busy', async () => {
    let resolveClose!: (v: { worktreeDeleteError: null }) => void;
    bridgeMock.sessionClose.mockImplementation(
      () =>
        new Promise((r) => {
          resolveClose = r;
        }),
    );
    seed('s1');
    render(<CloseConfirmDialog />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /terminate/i }));
    });

    const spinner = screen.getByRole('status');
    expect(spinner).toHaveAttribute('aria-label', 'Closing…');

    await act(async () => {
      resolveClose({ worktreeDeleteError: null });
    });
  });

  it('prevents duplicate close calls on repeated clicks', async () => {
    let resolveClose!: (v: { worktreeDeleteError: null }) => void;
    bridgeMock.sessionClose.mockImplementation(
      () =>
        new Promise((r) => {
          resolveClose = r;
        }),
    );
    seed('s1');
    render(<CloseConfirmDialog />);

    const terminateBtn = screen.getByRole('button', { name: /terminate/i });

    // First click triggers the close.
    await act(async () => {
      fireEvent.click(terminateBtn);
    });
    // Second click is a no-op (button is disabled, guard returns early).
    await act(async () => {
      fireEvent.click(terminateBtn);
    });

    expect(bridgeMock.sessionClose).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveClose({ worktreeDeleteError: null });
    });
  });

  it('blocks Esc (dialog cancel event) while busy', async () => {
    let resolveClose!: (v: { worktreeDeleteError: null }) => void;
    bridgeMock.sessionClose.mockImplementation(
      () =>
        new Promise((r) => {
          resolveClose = r;
        }),
    );
    seed('s1');
    render(<CloseConfirmDialog />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /terminate/i }));
    });

    // Simulate the native Esc → cancel event on the dialog.
    const dialog = screen.getByRole('dialog');
    fireEvent(dialog, new Event('cancel', { bubbles: false, cancelable: true }));

    // Dialog should still be open (pendingClose not cleared).
    expect(useSessionStore.getState().pendingClose).toBe('s1');

    await act(async () => {
      resolveClose({ worktreeDeleteError: null });
    });
  });

  it('does not show spinner when not busy', () => {
    seed('s1');
    render(<CloseConfirmDialog />);

    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });
});
