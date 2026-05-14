import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { ShellCommandTrustDialogHost } from './ShellCommandTrustDialog';
import { requestShellCommandTrustChoice, resetShellCommandTrustPromptStateForTest } from '@/lib/shell-command-trust';
import type { ShellCommandPreview } from '@/types/arborist';

function preview(command: string): ShellCommandPreview {
  return {
    targetWorktreePath: '/repo/.arborist/.worktrees/feat-x',
    commands: [
      {
        kind: 'worktreePrep',
        source: 'repoSettings',
        command,
        targetWorktreePath: '/repo/.arborist/.worktrees/feat-x',
        sourcePath: '/repo/.arborist/settings.json',
        trusted: false,
      },
    ],
    trustRecords: [],
    trustRequired: true,
  };
}

afterEach(() => {
  resetShellCommandTrustPromptStateForTest();
});

describe('ShellCommandTrustDialogHost', () => {
  it('renders repo command details and resolves remember choices', async () => {
    render(<ShellCommandTrustDialogHost />);
    let pending: Promise<string>;
    act(() => {
      pending = requestShellCommandTrustChoice(preview('pnpm install'));
    });

    expect(await screen.findByRole('dialog', { name: /trust repository command/i })).toBeInTheDocument();
    expect(screen.getByText(/\/repo\/\.arborist\/\.worktrees\/feat-x/)).toBeInTheDocument();
    expect(screen.getByText(/\/repo\/\.arborist\/settings\.json/)).toBeInTheDocument();
    expect(screen.getByText(/pnpm install/)).toBeInTheDocument();
    expect(screen.getAllByText(/this exact command/).length).toBeGreaterThan(0);

    let result: string | undefined;
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /don't ask again for this exact command/i }));
      result = await pending!;
    });

    expect(result).toBe('always');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('cancels on backdrop click and restores focus', async () => {
    render(
      <>
        <button type="button">Before prompt</button>
        <ShellCommandTrustDialogHost />
      </>,
    );
    const before = screen.getByRole('button', { name: /before prompt/i });
    before.focus();

    let pending: Promise<string>;
    act(() => {
      pending = requestShellCommandTrustChoice(preview('pnpm install'));
    });
    expect(await screen.findByRole('button', { name: /run once/i })).toHaveFocus();

    let result: string | undefined;
    await act(async () => {
      fireEvent.mouseDown(screen.getByTestId('shell-command-trust-backdrop'));
      result = await pending!;
    });

    expect(result).toBe('cancel');
    await waitFor(() => expect(before).toHaveFocus());
  });

  it('queues concurrent prompts', async () => {
    render(<ShellCommandTrustDialogHost />);
    let first: Promise<string>;
    let second: Promise<string>;
    act(() => {
      first = requestShellCommandTrustChoice(preview('first command'));
      second = requestShellCommandTrustChoice(preview('second command'));
    });

    expect(await screen.findByText(/first command/)).toBeInTheDocument();
    expect(screen.queryByText(/second command/)).not.toBeInTheDocument();
    let firstResult: string | undefined;
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /run once/i }));
      firstResult = await first!;
    });
    expect(firstResult).toBe('once');

    expect(await screen.findByText(/second command/)).toBeInTheDocument();
    let secondResult: string | undefined;
    await act(async () => {
      fireEvent.keyDown(document, { key: 'Escape' });
      secondResult = await second!;
    });
    expect(secondResult).toBe('cancel');
  });
});
