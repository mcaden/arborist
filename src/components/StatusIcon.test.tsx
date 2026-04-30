import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { StatusIcon } from './StatusIcon';
import type { DisplayStatus } from '@/store/session-store';

describe('StatusIcon', () => {
  const cases: Array<Exclude<DisplayStatus, 'idle'>> = [
    'starting',
    'working',
    'awaiting',
    'attention',
    'exited',
    'error',
  ];

  it.each(cases)('renders the %s glyph with a stable testid', (status) => {
    render(<StatusIcon status={status} />);
    expect(screen.getByTestId(`status-icon-${status}`)).toBeInTheDocument();
  });

  it('renders nothing for idle (quiescent sessions stay quiet)', () => {
    const { container } = render(<StatusIcon status="idle" />);
    expect(container).toBeEmptyDOMElement();
  });

  it('forwards the className to the rendered svg', () => {
    render(<StatusIcon status="working" className="text-emerald-500" />);
    const svg = screen.getByTestId('status-icon-working');
    expect(svg).toHaveClass('text-emerald-500');
  });

  it('emits the title as an accessible <title> element for tooltips', () => {
    render(<StatusIcon status="awaiting" title="Awaiting input" />);
    const svg = screen.getByTestId('status-icon-awaiting');
    expect(svg.querySelector('title')?.textContent).toBe('Awaiting input');
  });
});
