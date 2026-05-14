import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen } from '@testing-library/react';

import { repoCommandAllowOnce, repoCommandTrust, resetBridgeMocks, shellCommandPreview } from '@/lib/tauri-bridge.mock';
import { ensureShellCommandTrusted } from '@/lib/shell-command-trust';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

describe('ensureShellCommandTrusted', () => {
  beforeEach(() => {
    resetBridgeMocks();
  });

  afterEach(() => {
    document.body.innerHTML = '';
    vi.restoreAllMocks();
  });

  it('continues without prompting when no repo command needs trust', async () => {
    shellCommandPreview.mockResolvedValueOnce({ targetWorktreePath: '/repo/wt', commands: [], trustRecords: [], trustRequired: false });

    await expect(ensureShellCommandTrusted({ kind: 'worktreeCreate', name: 'feat-x' })).resolves.toBe(true);

    expect(repoCommandTrust).not.toHaveBeenCalled();
    expect(repoCommandAllowOnce).not.toHaveBeenCalled();
  });

  it('shows source, target, command, and precise-command scope before storing trust', async () => {
    shellCommandPreview.mockResolvedValueOnce({
      targetWorktreePath: '/repo/.arborist/.worktrees/feat-x',
      commands: [
        {
          kind: 'worktreePrep',
          source: 'repoSettings',
          command: 'pnpm install',
          targetWorktreePath: '/repo/.arborist/.worktrees/feat-x',
          sourcePath: '/repo/.arborist/settings.json',
          trusted: false,
        },
      ],
      trustRecords: [
        {
          fingerprint: 'abc',
          workspaceRoot: '/repo',
          sourcePath: '/repo/.arborist/settings.json',
          kind: 'worktreePrep',
          command: 'pnpm install',
          trustedAt: 0,
        },
      ],
      trustRequired: true,
    });

    const pending = ensureShellCommandTrusted({ kind: 'worktreeCreate', name: 'feat-x' });
    expect(await screen.findByText(/\/repo\/\.arborist\/\.worktrees\/feat-x/)).toBeInTheDocument();
    expect(screen.getByText(/\/repo\/\.arborist\/settings\.json/)).toBeInTheDocument();
    expect(screen.getByText(/pnpm install/)).toBeInTheDocument();
    expect(screen.getAllByText(/this exact command/).length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole('button', { name: /don't ask again for this exact command/i }));

    await expect(pending).resolves.toBe(true);
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(repoCommandAllowOnce).not.toHaveBeenCalled();
    expect(repoCommandTrust).toHaveBeenCalledWith({ intent: { kind: 'worktreeCreate', name: 'feat-x' } });
    expect(document.body.textContent).not.toContain('pnpm install');
  });

  it('allows a single run without storing persistent trust', async () => {
    shellCommandPreview.mockResolvedValueOnce({
      targetWorktreePath: '/repo/wt',
      commands: [
        {
          kind: 'aiLaunch',
          source: 'repoSettings',
          command: 'repo-claude',
          targetWorktreePath: '/repo/wt',
          sourcePath: '/repo/.arborist/settings.json',
          trusted: false,
        },
      ],
      trustRecords: [],
      trustRequired: true,
    });

    const pending = ensureShellCommandTrusted({ kind: 'sessionRestart', sessionId: 'sid-1' });
    expect(await screen.findByText(/repo-claude/)).toBeInTheDocument();
    expect(screen.getAllByText(/this exact command/).length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole('button', { name: /run once/i }));

    await expect(pending).resolves.toBe(true);
    expect(repoCommandAllowOnce).toHaveBeenCalledWith({ intent: { kind: 'sessionRestart', sessionId: 'sid-1' } });
    expect(repoCommandTrust).not.toHaveBeenCalled();
  });

  it('does not authorize when the user cancels', async () => {
    shellCommandPreview.mockResolvedValueOnce({
      targetWorktreePath: '/repo/wt',
      commands: [
        {
          kind: 'aiLaunch',
          source: 'repoSettings',
          command: 'repo-claude',
          targetWorktreePath: '/repo/wt',
          sourcePath: '/repo/.arborist/settings.json',
          trusted: false,
        },
      ],
      trustRecords: [],
      trustRequired: true,
    });

    const pending = ensureShellCommandTrusted({ kind: 'sessionRestart', sessionId: 'sid-1' });
    fireEvent.click(await screen.findByRole('button', { name: /cancel/i }));

    await expect(pending).resolves.toBe(false);
    expect(repoCommandTrust).not.toHaveBeenCalled();
    expect(repoCommandAllowOnce).not.toHaveBeenCalled();
  });
});
