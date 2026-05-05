import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { WorkspacePicker } from './WorkspacePicker';
import { pickDirectory, resetBridgeMocks, workspaceValidate } from '@/lib/tauri-bridge.mock';

vi.mock('@/lib/tauri-bridge', () => import('@/lib/tauri-bridge.mock'));

beforeEach(() => {
  resetBridgeMocks();
  // shouldAdvanceTime keeps wall-clock-based polling (waitFor / findBy*)
  // working while we still control the debounce timer manually.
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  vi.useRealTimers();
});

async function flushDebounce(): Promise<void> {
  await act(async () => {
    vi.advanceTimersByTime(300);
  });
  // Let pending microtasks (promise resolutions) drain.
  await act(async () => {
    await Promise.resolve();
  });
}

describe('WorkspacePicker — first-boot mode', () => {
  it('renders the heading and an empty input', () => {
    render(<WorkspacePicker mode="first-boot" onConfirm={vi.fn()} />);
    expect(screen.getByRole('heading', { name: /choose your workspace/i })).toBeInTheDocument();
    expect(screen.getByLabelText(/workspace path/i)).toHaveValue('');
  });

  it('disables Continue until validation succeeds', async () => {
    workspaceValidate.mockResolvedValue({ valid: true });
    render(<WorkspacePicker mode="first-boot" onConfirm={vi.fn()} />);
    const button = screen.getByRole('button', { name: /continue/i });
    expect(button).toBeDisabled();

    fireEvent.change(screen.getByLabelText(/workspace path/i), { target: { value: '/repo' } });
    await flushDebounce();
    await waitFor(() => expect(button).not.toBeDisabled());
  });

  it('shows the inline error when validation fails', async () => {
    workspaceValidate.mockResolvedValue({ valid: false, error: 'not a git repository' });
    render(<WorkspacePicker mode="first-boot" onConfirm={vi.fn()} />);
    fireEvent.change(screen.getByLabelText(/workspace path/i), { target: { value: '/nope' } });
    await flushDebounce();
    expect(await screen.findByRole('alert')).toHaveTextContent(/not a git repository/i);
    expect(screen.getByRole('button', { name: /continue/i })).toBeDisabled();
  });

  it('Browse… populates the input with the picked path', async () => {
    pickDirectory.mockResolvedValue('/picked/path');
    workspaceValidate.mockResolvedValue({ valid: true });
    render(<WorkspacePicker mode="first-boot" onConfirm={vi.fn()} />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /browse/i }));
      await Promise.resolve();
    });
    expect(screen.getByLabelText(/workspace path/i)).toHaveValue('/picked/path');
  });

  it('calls onConfirm with the trimmed path and shows submission errors', async () => {
    workspaceValidate.mockResolvedValue({ valid: true });
    const onConfirm = vi
      .fn<(path: string) => Promise<void>>()
      .mockRejectedValueOnce(new Error('save failed'));
    render(<WorkspacePicker mode="first-boot" onConfirm={onConfirm} />);

    fireEvent.change(screen.getByLabelText(/workspace path/i), {
      target: { value: '  /repo  ' },
    });
    await flushDebounce();

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: /continue/i }));
      await Promise.resolve();
    });

    expect(onConfirm).toHaveBeenCalledWith('/repo');
    expect(await screen.findByRole('alert')).toHaveTextContent(/save failed/i);
  });

  it('ignores stale validation responses', async () => {
    let resolveFirst!: (v: { valid: boolean; error?: string }) => void;
    workspaceValidate.mockImplementationOnce(
      () =>
        new Promise((res) => {
          resolveFirst = res;
        }),
    );
    workspaceValidate.mockResolvedValueOnce({ valid: true });
    render(<WorkspacePicker mode="first-boot" onConfirm={vi.fn()} />);

    fireEvent.change(screen.getByLabelText(/workspace path/i), { target: { value: '/a' } });
    await flushDebounce();
    fireEvent.change(screen.getByLabelText(/workspace path/i), { target: { value: '/b' } });
    await flushDebounce();

    // The stale response for `/a` arrives last. It must NOT downgrade
    // the current "valid" state for `/b`.
    await act(async () => {
      resolveFirst({ valid: false, error: 'stale!' });
      await Promise.resolve();
    });

    expect(screen.queryByText(/stale!/i)).not.toBeInTheDocument();
  });
  it('shows the "already open in another window" advisory warning when the probe reports contention', async () => {
    workspaceValidate.mockResolvedValue({
      valid: true,
      alreadyOpenInAnotherInstance: true,
    });
    render(<WorkspacePicker mode="first-boot" onConfirm={vi.fn()} />);

    fireEvent.change(screen.getByLabelText(/workspace path/i), { target: { value: '/repo' } });
    await flushDebounce();

    const warning = await screen.findByTestId('picker-already-open-warning');
    expect(warning).toHaveTextContent(/already be open in another arborist window/i);
    // Confirm button must remain enabled — the probe is advisory only.
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /continue/i })).not.toBeDisabled(),
    );
  });

  it('does not show the advisory warning when the probe reports the lock is free', async () => {
    workspaceValidate.mockResolvedValue({
      valid: true,
      alreadyOpenInAnotherInstance: false,
    });
    render(<WorkspacePicker mode="first-boot" onConfirm={vi.fn()} />);
    fireEvent.change(screen.getByLabelText(/workspace path/i), { target: { value: '/repo' } });
    await flushDebounce();
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /continue/i })).not.toBeDisabled(),
    );
    expect(screen.queryByTestId('picker-already-open-warning')).not.toBeInTheDocument();
  });
});

describe('WorkspacePicker — change mode', () => {
  it('exposes a Cancel button that calls onCancel', () => {
    const onCancel = vi.fn();
    render(
      <WorkspacePicker mode="change" initialPath="/old" onConfirm={vi.fn()} onCancel={onCancel} />,
    );
    expect(screen.getByLabelText(/workspace path/i)).toHaveValue('/old');
    fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(onCancel).toHaveBeenCalled();
  });

  it('suppresses the "already open" warning when the candidate equals the current bound workspace', async () => {
    // Regression: workspace_validate's lock probe targets the same .lock
    // file this process already holds. On Windows LockFileEx is per-handle,
    // so the probe always reports contention for the currently bound
    // workspace — the picker would otherwise flash a misleading
    // "open in another instance" warning the moment the change-mode
    // dialog opens (since initialPath seeds the input with that path).
    workspaceValidate.mockResolvedValue({
      valid: true,
      alreadyOpenInAnotherInstance: true,
    });
    render(<WorkspacePicker mode="change" initialPath="/current" onConfirm={vi.fn()} />);
    await flushDebounce();
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /switch workspace/i })).not.toBeDisabled(),
    );
    expect(screen.queryByTestId('picker-already-open-warning')).not.toBeInTheDocument();
  });

  it('still shows the "already open" warning after the user edits to a different path', async () => {
    workspaceValidate.mockResolvedValue({
      valid: true,
      alreadyOpenInAnotherInstance: true,
    });
    render(<WorkspacePicker mode="change" initialPath="/current" onConfirm={vi.fn()} />);
    await flushDebounce();
    fireEvent.change(screen.getByLabelText(/workspace path/i), {
      target: { value: '/other-workspace' },
    });
    await flushDebounce();
    const warning = await screen.findByTestId('picker-already-open-warning');
    expect(warning).toHaveTextContent(/already be open in another arborist window/i);
  });
});
