// Tests for the CloseConfirmDialog busy-state behavior (issue #47).
// Verifies that controls are disabled, a spinner appears, and repeated clicks / Esc are blocked while close is in flight.

import { act, fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSessionStore } from '@/store/session-store';
import type { SessionView } from '@/types/arborist';

import { CloseConfirmDialog } from './CloseConfirmDialog';

type CloseResult = { worktreeDeleteError: null };

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
  useSessionStore.setState({ sessions: [makeView(sessionId)], activeId: sessionId, pendingClose: sessionId, isHydrated: true });
}

/** Make `sessionClose` hang until the returned `resolve` is called. */
function hangClose(): { resolve: (v: CloseResult) => void } {
  let resolve!: (v: CloseResult) => void;
  bridgeMock.sessionClose.mockImplementation(
    () =>
      new Promise<CloseResult>((r) => {
        resolve = r;
      }),
  );
  return {
    get resolve() {
      return resolve;
    },
  };
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
  const proto = HTMLDialogElement.prototype as unknown as { showModal?: () => void; close?: () => void };
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
    const pending = hangClose();
    seed('s1');
    render(<CloseConfirmDialog />);

    const terminateBtn = screen.getByRole('button', { name: /terminate/i });
    const cancelBtn = screen.getByRole('button', { name: /cancel/i });
    const checkbox = screen.getByRole('checkbox');

    // All enabled before click.
    expect(terminateBtn).not.toBeDisabled();
    expect(cancelBtn).not.toBeDisabled();
    expect(checkbox).not.toBeDisabled();

    await act(async () => {
      fireEvent.click(terminateBtn);
    });

    expect(terminateBtn).toBeDisabled();
    expect(cancelBtn).toBeDisabled();
    expect(checkbox).toBeDisabled();
    expect(screen.getByRole('status')).toBeInTheDocument();
    expect(screen.getByRole('dialog')).toHaveAttribute('aria-busy', 'true');

    await act(async () => {
      pending.resolve({ worktreeDeleteError: null });
    });
  });

  it('shows spinner with accessible label while busy', async () => {
    const pending = hangClose();
    seed('s1');
    render(<CloseConfirmDialog />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /terminate/i }));
    });

    expect(screen.getByRole('status')).toHaveAttribute('aria-label', 'Closing…');

    await act(async () => {
      pending.resolve({ worktreeDeleteError: null });
    });
  });

  it('prevents duplicate close calls on repeated clicks', async () => {
    const pending = hangClose();
    seed('s1');
    render(<CloseConfirmDialog />);

    const terminateBtn = screen.getByRole('button', { name: /terminate/i });

    await act(async () => {
      fireEvent.click(terminateBtn);
    });
    await act(async () => {
      fireEvent.click(terminateBtn);
    }); // no-op — button disabled + guard

    expect(bridgeMock.sessionClose).toHaveBeenCalledTimes(1);

    await act(async () => {
      pending.resolve({ worktreeDeleteError: null });
    });
  });

  it('blocks Esc (dialog cancel event) while busy', async () => {
    const pending = hangClose();
    seed('s1');
    render(<CloseConfirmDialog />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /terminate/i }));
    });

    fireEvent(screen.getByRole('dialog'), new Event('cancel', { bubbles: false, cancelable: true }));
    expect(useSessionStore.getState().pendingClose).toBe('s1');

    await act(async () => {
      pending.resolve({ worktreeDeleteError: null });
    });
  });

  it('does not show spinner when not busy', () => {
    seed('s1');
    render(<CloseConfirmDialog />);
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });
});
