import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

import { App } from './App';

describe('App', () => {
  it('mounts the Sidebar and a placeholder main area', () => {
    render(<App />);
    expect(screen.getByRole('tablist', { name: /sessions/i })).toBeInTheDocument();
    expect(screen.getByText(/no session selected/i)).toBeInTheDocument();
  });
});
