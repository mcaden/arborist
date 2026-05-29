// Direct unit tests for `summariseSubCloseOutcomes` exist because the cascade-summary path it powers in `WorktreeCloseConfirmDialog` has no
// dialog-level coverage today, and PR #221 review feedback specifically called out the `terminalKill/unconfirmed` row that surfaces there. The
// single-sub formatter path is exercised end-to-end via `SubCloseConfirmDialog.test.tsx`.

import { describe, expect, it } from 'vitest';

import { summariseSubCloseOutcomes } from '@/lib/close-outcomes';
import type { SubSessionCloseResult } from '@/types/arborist';

describe('summariseSubCloseOutcomes', () => {
  it('returns the empty string when subOutcomes is undefined', () => {
    expect(summariseSubCloseOutcomes(undefined)).toBe('');
  });

  it('returns the empty string when every sub closed cleanly (no follow-up needed)', () => {
    const subOutcomes: Record<string, SubSessionCloseResult> = {
      'sub-1234-aaaa': { outcome: 'tabRemoved', status: 'confirmed' },
      'sub-1234-bbbb': { outcome: 'terminalKill', status: 'confirmed', pid: 4242 },
    };
    expect(summariseSubCloseOutcomes(subOutcomes)).toBe('');
  });

  it('emits a bullet row with the rust detail message for terminalKill/unconfirmed (cascade case from PR #221 review)', () => {
    const subOutcomes: Record<string, SubSessionCloseResult> = {
      'sub-abcdef0123': {
        outcome: 'terminalKill',
        status: 'unconfirmed',
        pid: 7777,
        message: 'PTY kill issued but the OS did not confirm exit; pid 7777 may still be alive',
      },
    };
    const summary = summariseSubCloseOutcomes(subOutcomes);
    expect(summary).toMatch(
      /^• sub-abcd…: Terminal close issued \(pid 7777\), but the operating system didn.?t confirm the PTY child exited.*PTY kill issued but the OS did not confirm exit; pid 7777 may still be alive\.$/,
    );
  });

  it('omits the em-dash detail clause when terminalKill/unconfirmed carries no message', () => {
    const subOutcomes: Record<string, SubSessionCloseResult> = {
      'sub-noMs': { outcome: 'terminalKill', status: 'unconfirmed', pid: 8001 },
    };
    const summary = summariseSubCloseOutcomes(subOutcomes);
    expect(summary).toBe(
      `• sub-noMs: Terminal close issued (pid 8001), but the operating system didn't confirm the PTY child exited within the grace window.`,
    );
  });

  it('surfaces a PTY kill failure as a bullet row with the failure detail', () => {
    const subOutcomes: Record<string, SubSessionCloseResult> = {
      'sub-failureCase': { outcome: 'terminalKill', status: 'unconfirmed', pid: 9090, message: 'PTY kill failed: process not found' },
    };
    const summary = summariseSubCloseOutcomes(subOutcomes);
    expect(summary).toMatch(/^• sub-fail…: Terminal close issued \(pid 9090\),.*PTY kill failed: process not found\.$/);
  });

  it('skips confirmed rows while keeping unconfirmed rows in the same cascade summary', () => {
    const subOutcomes: Record<string, SubSessionCloseResult> = {
      'sub-clean-aaaaa': { outcome: 'tabRemoved', status: 'confirmed' },
      'sub-dirty-bbbbb': {
        outcome: 'terminalKill',
        status: 'unconfirmed',
        pid: 1001,
        message: 'PTY kill issued but the OS did not confirm exit; pid 1001 may still be alive',
      },
      'sub-shared-ccccc': { outcome: 'forceKill', status: 'refusedShared', pid: 2002 },
    };
    const lines = summariseSubCloseOutcomes(subOutcomes).split('\n');
    expect(lines).toHaveLength(2);
    expect(lines[0]).toMatch(/^• sub-dirt…: Terminal close issued \(pid 1001\)/);
    expect(lines[1]).toMatch(/^• sub-shar…: Refused to terminate a shared editor process \(pid 2002\)/);
  });
});
