// Behavioural tests for `SubTerminalView`.
//
// Status (running / exited / error):
//   * running / starting → no exited bar; the xterm host fills the
//     viewport.
//   * exited / error → a slim non-modal status bar renders BELOW the
//     still-mounted xterm host with Relaunch / Close inline buttons.
//     Deliberately not a dialog (no modal backdrop, no role="dialog")
//     so it reads as part of the panel chrome rather than an
//     interruption. The terminal scrollback stays visible — the user
//     keeps the shell's final output (exit echo, error message, …).
//
// The `clear()` API fires only on the exited/error → starting edge so
// a relaunch starts fresh, defending against a PTY race where a stray
// byte from the just-killed child arrives after the new spawn begins
// (rubber-duck critique). It does NOT fire on the entering-exited edge
// because that would erase the very output the user wants to read.

import { act, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

const clearMock = vi.fn();

vi.mock('@/hooks/use-terminal', () => ({
  useSubTerminal: () => ({
    attach: vi.fn(),
    detach: vi.fn(),
    focus: vi.fn(),
    refit: vi.fn(),
    clear: clearMock,
  }),
}));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SubSession, SubSessionId, WorktreeTabId } from '@/types/arborist';

import { SubTerminalView } from './SubTerminalView';

const PARENT = 'tab-parent' as WorktreeTabId;

type SubOverrides = Partial<Omit<SubSession, 'id' | 'pid'>> &
  Pick<SubSession, 'id'> & {
    pid?: number | undefined;
  };

function makeSub(overrides: SubOverrides): SubSession {
  const { pid, ...restOverrides } = overrides;
  const sub: SubSession = {
    parentWorktreeTabId: PARENT,
    defId: 'shell',
    kind: 'terminal',
    label: 'Shell',
    status: 'running',
    composedCommand: 'sh -i',
    createdAt: 0,
    ...restOverrides,
  };
  if (pid !== undefined) sub.pid = pid;
  return sub;
}

function withStatus(sub: SubSession, status: SubSession['status']): SubSession {
  const { pid: _drop, ...rest } = sub;
  return { ...rest, status };
}

function id(suffix: string): SubSessionId {
  return ('22222222-2222-2222-2222-2222222222' + suffix) as SubSessionId;
}

beforeEach(() => {
  let rafId = 0;
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => {
    cb(performance.now());
    return ++rafId;
  });
  vi.stubGlobal('cancelAnimationFrame', vi.fn());
  bridgeMock.resetBridgeMocks();
  clearMock.mockReset();
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
    isHydrated: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

describe('SubTerminalView', () => {
  it('renders no exited bar while the sub-session is running', async () => {
    const sub = makeSub({ id: id('01'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(screen.queryByRole('status', { name: /sub-session ended/i })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /relaunch/i })).not.toBeInTheDocument();
  });

  it('shows the exited bar (non-dialog) when the sub-session has exited', async () => {
    const sub = makeSub({ id: id('02'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(screen.getByRole('status', { name: /sub-session ended/i })).toBeInTheDocument();
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /relaunch/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^close$/i })).toBeInTheDocument();
  });

  it('shows error-flavoured copy when the sub-session is in error state', async () => {
    const sub = makeSub({ id: id('03'), label: 'Shell', status: 'error', pid: undefined });
    useSubSessionStore.setState({
      subSessions: [sub],
      statusMessages: { [sub.id]: 'spawn failed: ENOENT' },
    });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(screen.getByRole('status', { name: /sub-session ended/i })).toBeInTheDocument();
    expect(screen.getByText(/ended with an error/i)).toBeInTheDocument();
  });

  it('does NOT clear the terminal on the running → exited transition (preserves final scrollback)', async () => {
    const sub = makeSub({ id: id('04'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    const { rerender } = render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(clearMock).not.toHaveBeenCalled();
    await act(async () => {
      useSubSessionStore.setState({
        subSessions: [withStatus(sub, 'exited')],
      });
      await new Promise((r) => setTimeout(r, 0));
    });
    rerender(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(clearMock).not.toHaveBeenCalled();
  });

  it('clears the terminal on the exited → starting transition (defends against late stray bytes)', async () => {
    const sub = makeSub({ id: id('05'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    const { rerender } = render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(clearMock).not.toHaveBeenCalled();
    await act(async () => {
      useSubSessionStore.setState({
        subSessions: [withStatus(sub, 'starting')],
      });
      await new Promise((r) => setTimeout(r, 0));
    });
    rerender(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(clearMock).toHaveBeenCalledTimes(1);
  });

  it('clicking Relaunch in the bar calls subSessionRelaunch with the sub id', async () => {
    const sub = makeSub({ id: id('06'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    await act(async () => {
      screen.getByRole('button', { name: /relaunch/i }).click();
    });
    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
  });

  it('clicking Close in the bar calls subSessionClose with default tabOnly intent', async () => {
    const sub = makeSub({ id: id('07'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    screen.getByRole('button', { name: /^close$/i }).click();
    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
    await act(async () => {});
  });

  it('dims the terminal pane content (opacity-50) when the sub has exited', async () => {
    const sub = makeSub({ id: id('08'), status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(screen.getByTestId('sub-terminal-host').className).toContain('opacity-50');
  });

  it('dims the terminal pane content (opacity-50) when the sub is in error state', async () => {
    const sub = makeSub({ id: id('09'), status: 'error', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(screen.getByTestId('sub-terminal-host').className).toContain('opacity-50');
  });

  it('does NOT dim the terminal pane content while the sub is running', async () => {
    const sub = makeSub({ id: id('0a'), status: 'running', pid: 100 });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(screen.getByTestId('sub-terminal-host').className).not.toContain('opacity-50');
  });

  it('does NOT dim the terminal pane content while the sub is starting', async () => {
    const sub = makeSub({ id: id('0b'), status: 'starting' });
    useSubSessionStore.setState({ subSessions: [sub] });
    render(<SubTerminalView subSessionId={sub.id} isActive />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(screen.getByTestId('sub-terminal-host').className).not.toContain('opacity-50');
  });
});
