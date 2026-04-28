import { fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';

import { NewSessionButton } from './NewSessionButton';
import { useNewSessionDialog } from '@/store/new-session-dialog-store';

beforeEach(() => {
  useNewSessionDialog.setState({ isOpen: false });
});

describe('NewSessionButton', () => {
  it('renders an accessible "+" button', () => {
    render(<NewSessionButton />);
    const btn = screen.getByRole('button', { name: /new session/i });
    expect(btn).toBeInTheDocument();
    btn.focus();
    expect(btn).toHaveFocus();
  });

  it('clicking the button opens the new-session dialog store', () => {
    render(<NewSessionButton />);
    expect(useNewSessionDialog.getState().isOpen).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: /new session/i }));
    expect(useNewSessionDialog.getState().isOpen).toBe(true);
  });
});
