import { act, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

vi.mock('@/hooks/use-terminal', () => ({
  forceRefitAllTerminals: vi.fn(),
  captureTerminalDebugSnapshot: vi.fn(() => ({
    capturedAt: '2026-04-30T00:00:00.000Z',
    windowInnerWidth: 1280,
    windowInnerHeight: 800,
    devicePixelRatio: 2,
    documentVisibility: 'visible',
    documentHasFocus: true,
    fontsStatus: 'loaded',
    darkMode: false,
    registrySize: 1,
    entries: [
      {
        sessionId: 's1',
        isAttached: true,
        hostConnected: true,
        wrapperConnected: true,
        termCols: 80,
        termRows: 24,
        lastReportedCols: 80,
        lastReportedRows: 24,
        fontFamily: 'monospace',
        fontSize: undefined,
        hostRect: { width: 600, height: 400, top: 0, left: 0 },
        wrapperRect: { width: 600, height: 400, top: 0, left: 0 },
        screenRect: { width: 600, height: 400, top: 0, left: 0 },
        approxCellWidth: 7.5,
        approxCellHeight: 16.67,
        hostDisplay: 'block',
        hostVisibility: 'visible',
        ancestors: [],
      },
    ],
  })),
}));

import { captureTerminalDebugSnapshot, forceRefitAllTerminals } from '@/hooks/use-terminal';
import { useSessionStore } from '@/store/session-store';

import { FitDebugButton } from './FitDebugButton';

beforeEach(() => {
  vi.useFakeTimers();
  useSessionStore.setState({
    sessions: [
      {
        id: 's1',
        tool: 'claude',
        worktreePath: '/wt',
        worktreeName: 'wt',
        label: 'wt',
        instructionSetId: 'default',
        status: 'running',
        createdAt: 0,
        tabIndex: 0,
      },
    ],
    activeId: 's1',
    isHydrated: true,
  });
  vi.mocked(captureTerminalDebugSnapshot).mockClear();
  vi.mocked(forceRefitAllTerminals).mockClear();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('FitDebugButton', () => {
  it('renders an idle "Fit" label by default', () => {
    render(<FitDebugButton />);
    expect(screen.getByTestId('fit-debug-button')).toHaveTextContent('Fit');
  });

  it('exposes the visible label as the accessible name (no static aria-label that would mask it)', () => {
    // Regression for the original sidebar-debug PR: the button used a
    // static aria-label, so screen readers always heard "Force-fit every
    // terminal..." regardless of whether the visible text changed to
    // "Copied ✓" / "Copy failed". The fix drops aria-label so the SR
    // accessible name is the visible label, and the live region on the
    // label span announces the transient state.
    render(<FitDebugButton />);
    const btn = screen.getByRole('button', { name: /^fit$/i });
    expect(btn).toBe(screen.getByTestId('fit-debug-button'));
    expect(btn.hasAttribute('aria-label')).toBe(false);
    // Tooltip stays on `title`.
    expect(btn.getAttribute('title')).toMatch(/force-fit/i);
    // The label span itself is the live region so SRs announce updates.
    const labelSpan = btn.querySelector<HTMLElement>('[aria-live="polite"]');
    expect(labelSpan).not.toBeNull();
    expect(labelSpan!.textContent).toBe('Fit');
    expect(labelSpan!.getAttribute('aria-atomic')).toBe('true');
  });

  it('captures snapshot, forces refit, and copies bundle to clipboard', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText },
      configurable: true,
    });

    render(<FitDebugButton />);
    const btn = screen.getByTestId('fit-debug-button');

    await act(async () => {
      btn.click();
      // Drain microtasks for the clipboard promise to resolve.
      await Promise.resolve();
    });

    // Two snapshots: before + after refit.
    expect(captureTerminalDebugSnapshot).toHaveBeenCalledTimes(2);
    expect(forceRefitAllTerminals).toHaveBeenCalledTimes(1);

    // Refit happens between the two snapshots.
    const captureOrder = vi.mocked(captureTerminalDebugSnapshot).mock.invocationCallOrder;
    const refitOrder = vi.mocked(forceRefitAllTerminals).mock.invocationCallOrder;
    expect(captureOrder[0]!).toBeLessThan(refitOrder[0]!);
    expect(refitOrder[0]!).toBeLessThan(captureOrder[1]!);

    expect(writeText).toHaveBeenCalledTimes(1);
    const payload = writeText.mock.calls[0]![0] as string;
    const parsed = JSON.parse(payload) as Record<string, unknown>;
    expect(parsed).toHaveProperty('before');
    expect(parsed).toHaveProperty('after');
    expect(parsed).toHaveProperty('sessions');
    expect((parsed.sessions as unknown[]).length).toBe(1);

    expect(btn).toHaveTextContent('Copied ✓');
    act(() => {
      vi.advanceTimersByTime(2100);
    });
    expect(btn).toHaveTextContent('Fit');
  });

  it('shows "Copy failed" when clipboard rejects', async () => {
    const writeText = vi.fn().mockRejectedValue(new Error('denied'));
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText },
      configurable: true,
    });
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});

    render(<FitDebugButton />);
    const btn = screen.getByTestId('fit-debug-button');
    await act(async () => {
      btn.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    // Refit still happened — that's the "keep working" promise.
    expect(forceRefitAllTerminals).toHaveBeenCalledTimes(1);
    expect(btn).toHaveTextContent('Copy failed');
    warnSpy.mockRestore();
  });

  it('still copies an after-snapshot when forceRefitAllTerminals throws', async () => {
    vi.mocked(forceRefitAllTerminals).mockImplementationOnce(() => {
      throw new Error('boom');
    });
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText },
      configurable: true,
    });
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});

    render(<FitDebugButton />);
    await act(async () => {
      screen.getByTestId('fit-debug-button').click();
      await Promise.resolve();
    });

    expect(captureTerminalDebugSnapshot).toHaveBeenCalledTimes(2);
    expect(writeText).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId('fit-debug-button')).toHaveTextContent('Copied ✓');
    warnSpy.mockRestore();
  });

  it('does not setState after unmount when the clipboard resolves late', async () => {
    let resolveWrite: () => void = () => {};
    const writeText = vi.fn().mockReturnValue(
      new Promise<void>((resolve) => {
        resolveWrite = resolve;
      }),
    );
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText },
      configurable: true,
    });
    const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});

    const { unmount } = render(<FitDebugButton />);
    act(() => {
      screen.getByTestId('fit-debug-button').click();
    });
    expect(writeText).toHaveBeenCalledTimes(1);

    // Unmount BEFORE the clipboard promise resolves — simulates the user
    // tearing the sidebar down (e.g. navigating, app hot-reloading) mid-flight.
    unmount();

    await act(async () => {
      resolveWrite();
      await Promise.resolve();
    });

    // No "setState on unmounted component" warnings should have fired.
    expect(errorSpy).not.toHaveBeenCalled();

    // No leaked timers either: advancing past the 2s flash window must not
    // throw or trigger any further work.
    act(() => {
      vi.advanceTimersByTime(2100);
    });
    errorSpy.mockRestore();
  });
});
