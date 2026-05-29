import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { WorktreeCloseBanner } from './WorktreeCloseBanner';
import { useWorktreeCloseStore } from '@/store/worktree-close-store';
import type { WorktreeTabId } from '@/types/arborist';

const TAB_A = 'tab-a' as WorktreeTabId;
const TAB_B = 'tab-b' as WorktreeTabId;

beforeEach(() => {
  useWorktreeCloseStore.getState()._resetForTest();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('WorktreeCloseBanner', () => {
  it('renders nothing when no closes are tracked', () => {
    const { container } = render(<WorktreeCloseBanner />);
    expect(container.firstChild).toBeNull();
  });

  it('shows a running banner while a close is in flight', () => {
    render(<WorktreeCloseBanner />);
    act(() => {
      useWorktreeCloseStore.getState().markStarted({
        tabId: TAB_A,
        worktreePath: '/repo/.arborist/.worktrees/feature-x',
        willDelete: true,
      });
    });
    const running = screen.getByTestId('worktree-close-banner-running');
    expect(running.textContent).toContain('Closing and deleting');
    expect(running.textContent).toContain('feature-x');
  });

  it('coalesces multiple in-flight closes into a single counter banner', () => {
    render(<WorktreeCloseBanner />);
    act(() => {
      useWorktreeCloseStore.getState().markStarted({
        tabId: TAB_A,
        worktreePath: '/repo/.arborist/.worktrees/feature-x',
        willDelete: false,
      });
      useWorktreeCloseStore.getState().markStarted({
        tabId: TAB_B,
        worktreePath: '/repo/.arborist/.worktrees/feature-y',
        willDelete: false,
      });
    });
    const running = screen.getByTestId('worktree-close-banner-running');
    expect(running.textContent).toContain('(+1 more)');
  });

  it('shows a sticky failure banner with the backend message verbatim', () => {
    render(<WorktreeCloseBanner />);
    act(() => {
      useWorktreeCloseStore.getState().markStarted({
        tabId: TAB_A,
        worktreePath: '/repo/.arborist/.worktrees/feature-x',
        willDelete: true,
      });
      useWorktreeCloseStore.getState().markCompleted({
        tabId: TAB_A,
        status: 'failure',
        message: 'directory still pinned by helper process',
      });
    });
    const banner = screen.getByTestId('worktree-close-banner-failure');
    expect(banner.textContent).toContain('Close failed for feature-x');
    expect(banner.textContent).toContain('directory still pinned by helper process');
  });

  it('auto-dismisses successful close banners after the timer', () => {
    render(<WorktreeCloseBanner />);
    act(() => {
      useWorktreeCloseStore.getState().markStarted({
        tabId: TAB_A,
        worktreePath: '/repo/.arborist/.worktrees/feature-x',
        willDelete: false,
      });
      useWorktreeCloseStore.getState().markCompleted({ tabId: TAB_A, status: 'success' });
    });
    expect(screen.getByTestId('worktree-close-banner-success')).toBeTruthy();
    act(() => {
      vi.advanceTimersByTime(6_000);
    });
    expect(screen.queryByTestId('worktree-close-banner-success')).toBeNull();
  });

  it('keeps attention banners until the user dismisses them', () => {
    render(<WorktreeCloseBanner />);
    act(() => {
      useWorktreeCloseStore.getState().markStarted({
        tabId: TAB_A,
        worktreePath: '/repo/.arborist/.worktrees/feature-x',
        willDelete: false,
      });
      useWorktreeCloseStore.getState().markCompleted({
        tabId: TAB_A,
        status: 'attention',
        message: 'application sub-session left detached',
      });
    });
    const banner = screen.getByTestId('worktree-close-banner-attention');
    expect(banner.textContent).toContain('with warnings');
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    // Still present after the success auto-dismiss interval.
    expect(screen.getByTestId('worktree-close-banner-attention')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: /dismiss close notification/i }));
    expect(screen.queryByTestId('worktree-close-banner-attention')).toBeNull();
  });
});
