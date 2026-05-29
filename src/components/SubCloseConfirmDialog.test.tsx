// Behavioural tests for SubCloseConfirmDialog — the 4-button (Cancel + 3 close intents) dialog
// introduced for issue #132. The intents map 1:1 onto `subSessionClose`'s `SubSessionCloseIntent`
// enum, and the outcome-aware alert copy comes from `formatSubCloseOutcome` in
// `@/lib/close-outcomes` (shared with `WorktreeCloseConfirmDialog`'s cascade summary so single-sub
// and cascade alerts stay phrased identically). The tests below cover both the dispatch side
// (each button → right intent) and the readback side (each outcome → expected alert).

import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SubSession, SubSessionCloseIntent, SubSessionCloseResult, SubSessionId } from '@/types/arborist';

import { SubCloseConfirmDialog } from './SubCloseConfirmDialog';

const PARENT = 'tab-parent';
const SUB_ID = '33333333-3333-3333-3333-333333333301' as SubSessionId;

function makeAppSub(): SubSession {
  return {
    id: SUB_ID,
    parentWorktreeTabId: PARENT,
    defId: 'code',
    kind: 'application',
    label: 'VS Code',
    status: 'running',
    composedCommand: 'code .',
    createdAt: 0,
    pid: 4242,
  };
}

function seed(sub: SubSession = makeAppSub()): void {
  useSubSessionStore.setState({
    subSessions: [sub],
    statusMessages: {},
    pendingClose: sub.id,
    isHydrated: true,
  });
}

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  // Default: success-confirmed so the close button → no alert path can be reused per test.
  bridgeMock.subSessionClose.mockResolvedValue({ outcome: 'tabRemoved', status: 'confirmed' });
});

afterEach(() => {
  vi.clearAllMocks();
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
    pendingClose: undefined,
    isHydrated: true,
  });
});

describe('SubCloseConfirmDialog', () => {
  it('renders all four buttons when there is a pending close', () => {
    seed();
    act(() => {
      render(<SubCloseConfirmDialog />);
    });

    expect(screen.getByRole('button', { name: /cancel/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /close tab only/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /close tab .* app window/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /force kill process/i })).toBeInTheDocument();
  });

  it('renders nothing when there is no pending close', () => {
    useSubSessionStore.setState({
      subSessions: [makeAppSub()],
      statusMessages: {},
      pendingClose: undefined,
      isHydrated: true,
    });
    const { container } = render(<SubCloseConfirmDialog />);
    expect(container.firstChild).toBeNull();
  });

  // The button → intent table that drives the rest of the assertions.
  const cases: ReadonlyArray<{ label: RegExp; intent: SubSessionCloseIntent }> = [
    { label: /close tab only/i, intent: 'tabOnly' },
    { label: /close tab .* app window/i, intent: 'requestAppClose' },
    { label: /force kill process/i, intent: 'forceKill' },
  ];

  for (const { label, intent } of cases) {
    it(`invokes subSessionClose with intent='${intent}' when the matching button is clicked`, async () => {
      seed();
      await act(async () => {
        render(<SubCloseConfirmDialog />);
      });

      await act(async () => {
        fireEvent.click(screen.getByRole('button', { name: label }));
      });

      expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(SUB_ID, intent);
    });
  }

  it('clears the pendingClose state when Cancel is clicked (without invoking the bridge)', () => {
    seed();
    act(() => {
      render(<SubCloseConfirmDialog />);
    });

    act(() => {
      fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    });

    expect(useSubSessionStore.getState().pendingClose).toBeUndefined();
    expect(bridgeMock.subSessionClose).not.toHaveBeenCalled();
  });

  // Outcome → alert copy: confirmed never alerts; unsupported / unavailable / refusedShared /
  // unconfirmed each carry a distinct user-facing sentence the UI must surface.
  type AlertCase = {
    name: string;
    result: SubSessionCloseResult;
    expectAlert: RegExp | null;
  };

  const alertCases: ReadonlyArray<AlertCase> = [
    { name: 'confirmed tab-removed is silent', result: { outcome: 'tabRemoved', status: 'confirmed' }, expectAlert: null },
    {
      name: 'unsupported polite close warns the OS does not support it',
      result: { outcome: 'politeClose', status: 'unsupported' },
      expectAlert: /doesn.?t support requesting an app close/i,
    },
    {
      name: 'unavailable polite close warns we could not identify the window',
      result: { outcome: 'politeClose', status: 'unavailable' },
      expectAlert: /couldn.?t identify the exact app window/i,
    },
    {
      name: 'refusedShared force-kill warns the editor was not killed',
      result: { outcome: 'forceKill', status: 'refusedShared', pid: 9001 },
      expectAlert: /refused to terminate a shared editor process \(pid 9001\)/i,
    },
    {
      name: 'unconfirmed force-kill warns the OS did not confirm exit',
      result: { outcome: 'forceKill', status: 'unconfirmed', pid: 9002 },
      expectAlert: /force-kill signal sent \(pid 9002\), but the operating system didn.?t confirm/i,
    },
    {
      name: 'unconfirmed polite close warns the app may be showing a save prompt',
      result: { outcome: 'politeClose', status: 'unconfirmed', pid: 9003 },
      expectAlert: /asked the app to close \(pid 9003\), but it.?s still running/i,
    },
    {
      name: 'unconfirmed terminal kill warns the PTY child may still be alive and surfaces the rust detail message',
      result: {
        outcome: 'terminalKill',
        status: 'unconfirmed',
        pid: 9004,
        message: 'PTY kill issued but the OS did not confirm exit; pid 9004 may still be alive',
      },
      expectAlert:
        /terminal close issued \(pid 9004\), but the operating system didn.?t confirm the PTY child exited.*PTY kill issued but the OS did not confirm exit/i,
    },
    {
      name: 'unconfirmed terminal kill surfaces a PTY kill failure detail message',
      result: { outcome: 'terminalKill', status: 'unconfirmed', pid: 9005, message: 'PTY kill failed: process not found' },
      expectAlert: /terminal close issued \(pid 9005\),.*PTY kill failed: process not found/i,
    },
  ];

  for (const { name, result, expectAlert } of alertCases) {
    it(name, async () => {
      seed();
      bridgeMock.subSessionClose.mockReset();
      bridgeMock.subSessionClose.mockResolvedValue(result);
      const alertSpy = vi.spyOn(window, 'alert').mockImplementation(() => {});

      await act(async () => {
        render(<SubCloseConfirmDialog />);
      });
      await act(async () => {
        fireEvent.click(screen.getByRole('button', { name: /close tab only/i }));
      });

      if (expectAlert === null) {
        expect(alertSpy).not.toHaveBeenCalled();
      } else {
        expect(alertSpy).toHaveBeenCalledTimes(1);
        expect(alertSpy.mock.calls[0]?.[0]).toMatch(expectAlert);
      }
      alertSpy.mockRestore();
    });
  }

  it('surfaces a bridge error via window.alert without leaving pendingClose set', async () => {
    seed();
    bridgeMock.subSessionClose.mockReset();
    bridgeMock.subSessionClose.mockRejectedValue(new Error('bridge boom'));
    const alertSpy = vi.spyOn(window, 'alert').mockImplementation(() => {});

    await act(async () => {
      render(<SubCloseConfirmDialog />);
    });
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /force kill process/i }));
    });

    expect(alertSpy).toHaveBeenCalledTimes(1);
    expect(alertSpy.mock.calls[0]?.[0]).toMatch(/close request failed/i);
    expect(alertSpy.mock.calls[0]?.[0]).toMatch(/bridge boom/);
    expect(useSubSessionStore.getState().pendingClose).toBeUndefined();
    alertSpy.mockRestore();
  });
});
