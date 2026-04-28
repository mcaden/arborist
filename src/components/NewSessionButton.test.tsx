import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { NewSessionButton } from './NewSessionButton';

describe('NewSessionButton', () => {
  it('renders an accessible "+" button (Phase 10 will wire it up)', () => {
    render(<NewSessionButton />);
    const btn = screen.getByRole('button', { name: /new session/i });
    expect(btn).toBeInTheDocument();
    btn.focus();
    expect(btn).toHaveFocus();
  });
});
