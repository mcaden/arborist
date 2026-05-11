// Behavioural tests for `SubTabContextMenu` — the ⋮-button menu for
// sub-session tabs introduced alongside issue #49.

import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import * as bridgeMock from '@/lib/tauri-bridge.mock';
import { useSubSessionStore } from '@/store/sub-session-store';
import type { SubSession, SubSessionId } from '@/types/arborist';

import { SubTabContextMenu } from './SubTabContextMenu';

const PARENT = 'tab-parent';

type SubOverrides = Partial<Omit<SubSession, 'id' | 'pid'>> &
  Pick<SubSession, 'id'> & {
    pid?: number | undefined;
  };

function makeSub(overrides: SubOverrides): SubSession {
  const { pid, ...rest } = overrides;
  const sub: SubSession = {
    parentWorktreeTabId: PARENT,
    defId: 'shell',
    kind: 'terminal',
    label: 'Shell',
    status: 'running',
    composedCommand: 'sh -i',
    createdAt: 0,
    ...rest,
  };
  if (pid !== undefined) sub.pid = pid;
  return sub;
}

function id(suffix: string): SubSessionId {
  return ('22222222-2222-2222-2222-2222222222' + suffix) as SubSessionId;
}

const noop = () => {};

beforeEach(() => {
  bridgeMock.resetBridgeMocks();
  useSubSessionStore.setState({
    subSessions: [],
    statusMessages: {},
    pendingClose: undefined,
    isHydrated: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('SubTabContextMenu', () => {
  it('renders Restart and Close menu items', () => {
    const sub = makeSub({ id: id('01') });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={noop} />);

    expect(screen.getByRole('menuitem', { name: /restart/i })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /close/i })).toBeInTheDocument();
  });

  it('Restart invokes subSessionRelaunch and dismisses the menu', () => {
    const sub = makeSub({ id: id('02') });
    useSubSessionStore.setState({ subSessions: [sub] });
    bridgeMock.subSessionRelaunch.mockResolvedValueOnce(sub);
    const onClose = vi.fn();

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /restart/i }));

    expect(bridgeMock.subSessionRelaunch).toHaveBeenCalledWith(sub.id);
    expect(onClose).toHaveBeenCalled();
  });

  it('Close on a terminal sub-session calls subSessionClose immediately', () => {
    const sub = makeSub({ id: id('03'), kind: 'terminal' });
    useSubSessionStore.setState({ subSessions: [sub] });
    const onClose = vi.fn();

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /close/i }));

    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
    expect(onClose).toHaveBeenCalled();
  });

  it('Close on a running application sub-session uses requestClose (confirm dialog), not immediate close', () => {
    const sub = makeSub({ id: id('04'), kind: 'application', status: 'running', pid: 42 });
    useSubSessionStore.setState({ subSessions: [sub] });
    const onClose = vi.fn();

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /close/i }));

    expect(bridgeMock.subSessionClose).not.toHaveBeenCalled();
    expect(useSubSessionStore.getState().pendingClose).toBe(sub.id);
    expect(onClose).toHaveBeenCalled();
  });

  it('Close on an exited application sub-session closes immediately (no dialog)', () => {
    const sub = makeSub({ id: id('05'), kind: 'application', status: 'exited', pid: undefined });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    fireEvent.click(screen.getByRole('menuitem', { name: /close/i }));

    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(sub.id, undefined);
    expect(useSubSessionStore.getState().pendingClose).toBeUndefined();
  });

  it('Escape dismisses the menu', () => {
    const sub = makeSub({ id: id('06') });
    useSubSessionStore.setState({ subSessions: [sub] });
    const onClose = vi.fn();

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.keyDown(screen.getByTestId('sub-tab-context-menu'), { key: 'Escape' });

    expect(onClose).toHaveBeenCalled();
  });

  it('Tab dismisses the menu', () => {
    const sub = makeSub({ id: id('07') });
    useSubSessionStore.setState({ subSessions: [sub] });
    const onClose = vi.fn();

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={onClose} />);
    fireEvent.keyDown(screen.getByTestId('sub-tab-context-menu'), { key: 'Tab' });

    expect(onClose).toHaveBeenCalled();
  });

  it('ArrowDown moves focus from Restart to Close', () => {
    const sub = makeSub({ id: id('08') });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    const close = screen.getByRole('menuitem', { name: /close/i });
    fireEvent.keyDown(screen.getByTestId('sub-tab-context-menu'), { key: 'ArrowDown' });

    expect(close).toHaveAttribute('tabindex', '0');
  });

  it('ArrowUp from Restart wraps to Close', () => {
    const sub = makeSub({ id: id('09') });
    useSubSessionStore.setState({ subSessions: [sub] });

    render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={noop} />);
    const close = screen.getByRole('menuitem', { name: /close/i });
    fireEvent.keyDown(screen.getByTestId('sub-tab-context-menu'), { key: 'ArrowUp' });

    expect(close).toHaveAttribute('tabindex', '0');
  });

  it('mousedown outside the menu dismisses it', () => {
    const sub = makeSub({ id: id('0a') });
    useSubSessionStore.setState({ subSessions: [sub] });
    const onClose = vi.fn();

    render(
      <div>
        <button data-testid="outside">outside</button>
        <SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={onClose} />
      </div>,
    );

    fireEvent.mouseDown(screen.getByTestId('outside'));

    expect(onClose).toHaveBeenCalled();
  });

  it('restores focus to the supplied trigger element on close', () => {
    const sub = makeSub({ id: id('0b') });
    useSubSessionStore.setState({ subSessions: [sub] });
    const trigger = document.createElement('button');
    trigger.setAttribute('data-testid', 'trigger');
    document.body.appendChild(trigger);
    const focusSpy = vi.spyOn(trigger, 'focus');

    try {
      render(<SubTabContextMenu subSessionId={sub.id} anchor={{ x: 10, y: 10 }} onClose={noop} restoreFocusTo={trigger} />);
      fireEvent.keyDown(screen.getByTestId('sub-tab-context-menu'), { key: 'Escape' });

      return new Promise<void>((resolve) => {
        requestAnimationFrame(() => {
          expect(focusSpy).toHaveBeenCalled();
          resolve();
        });
      });
    } finally {
      document.body.removeChild(trigger);
    }
  });

  it('handles missing sub-session without crashing and still handles Close', () => {
    // Sub-session not added to the store — `useSubSessionById` returns undefined.
    const onClose = vi.fn();

    render(<SubTabContextMenu subSessionId={id('0c')} anchor={{ x: 10, y: 10 }} onClose={onClose} />);

    // Menu still renders; clicking Close calls subSessionClose immediately
    // (the requestClose path requires kind === 'application' which is false
    // when sub is missing).
    fireEvent.click(screen.getByRole('menuitem', { name: /close/i }));

    expect(bridgeMock.subSessionClose).toHaveBeenCalledWith(id('0c'), undefined);
    expect(onClose).toHaveBeenCalled();
  });
});
