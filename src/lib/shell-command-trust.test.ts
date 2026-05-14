import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { repoCommandAllowOnce, repoCommandTrust, resetBridgeMocks, shellCommandPreview } from '@/lib/tauri-bridge.mock';
import {
  ensureShellCommandTrusted,
  resetShellCommandTrustPromptStateForTest,
  setShellCommandTrustPromptAdapterForTest,
} from '@/lib/shell-command-trust';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

describe('ensureShellCommandTrusted', () => {
  beforeEach(() => {
    resetBridgeMocks();
    resetShellCommandTrustPromptStateForTest();
  });

  afterEach(() => {
    resetShellCommandTrustPromptStateForTest();
    vi.restoreAllMocks();
  });

  it('continues without prompting when no repo command needs trust', async () => {
    shellCommandPreview.mockResolvedValueOnce({ targetWorktreePath: '/repo/wt', commands: [], trustRecords: [], trustRequired: false });
    const prompt = vi.fn();
    setShellCommandTrustPromptAdapterForTest(prompt);

    await expect(ensureShellCommandTrusted({ kind: 'worktreeCreate', name: 'feat-x' })).resolves.toBe(true);

    expect(prompt).not.toHaveBeenCalled();
    expect(repoCommandTrust).not.toHaveBeenCalled();
    expect(repoCommandAllowOnce).not.toHaveBeenCalled();
  });

  it('stores trust after the prompt approves remembering the exact command', async () => {
    const preview = {
      targetWorktreePath: '/repo/.arborist/.worktrees/feat-x',
      commands: [
        {
          kind: 'worktreePrep' as const,
          source: 'repoSettings' as const,
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
          kind: 'worktreePrep' as const,
          command: 'pnpm install',
          trustedAt: 0,
        },
      ],
      trustRequired: true,
    };
    shellCommandPreview.mockResolvedValueOnce(preview);
    const prompt = vi.fn().mockResolvedValue('always');
    setShellCommandTrustPromptAdapterForTest(prompt);

    await expect(ensureShellCommandTrusted({ kind: 'worktreeCreate', name: 'feat-x' })).resolves.toBe(true);

    expect(prompt).toHaveBeenCalledWith(preview);
    expect(repoCommandAllowOnce).not.toHaveBeenCalled();
    expect(repoCommandTrust).toHaveBeenCalledWith({ intent: { kind: 'worktreeCreate', name: 'feat-x' } });
  });

  it('allows a single run without storing persistent trust', async () => {
    const preview = {
      targetWorktreePath: '/repo/wt',
      commands: [
        {
          kind: 'aiLaunch' as const,
          source: 'repoSettings' as const,
          command: 'repo-claude',
          targetWorktreePath: '/repo/wt',
          sourcePath: '/repo/.arborist/settings.json',
          trusted: false,
        },
      ],
      trustRecords: [],
      trustRequired: true,
    };
    shellCommandPreview.mockResolvedValueOnce(preview);
    setShellCommandTrustPromptAdapterForTest(vi.fn().mockResolvedValue('once'));

    await expect(ensureShellCommandTrusted({ kind: 'sessionRestart', sessionId: 'sid-1' })).resolves.toBe(true);

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
    setShellCommandTrustPromptAdapterForTest(vi.fn().mockResolvedValue('cancel'));

    await expect(ensureShellCommandTrusted({ kind: 'sessionRestart', sessionId: 'sid-1' })).resolves.toBe(false);

    expect(repoCommandTrust).not.toHaveBeenCalled();
    expect(repoCommandAllowOnce).not.toHaveBeenCalled();
  });
});
