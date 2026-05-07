import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { StatusIcon } from './StatusIcon';
import type { DisplayStatus } from '@/store/session-store';

describe('StatusIcon', () => {
  const cases: Array<Exclude<DisplayStatus, 'idle'>> = ['starting', 'working', 'awaiting', 'attention', 'exited', 'error'];

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

  it('hides the icon from assistive tech when no title is supplied', () => {
    // Mixing aria-label with aria-hidden=true is a contradiction; the
    // sidebar tab itself is the labelled element, so an unlabelled icon
    // should be silently decorative.
    render(<StatusIcon status="working" />);
    const svg = screen.getByTestId('status-icon-working');
    expect(svg).toHaveAttribute('aria-hidden', 'true');
    expect(svg).not.toHaveAttribute('aria-label');
    expect(svg).not.toHaveAttribute('role');
  });

  it('promotes to role=img and exposes aria-label when titled', () => {
    render(<StatusIcon status="working" title="Working" />);
    const svg = screen.getByTestId('status-icon-working');
    expect(svg).toHaveAttribute('aria-label', 'Working');
    expect(svg).toHaveAttribute('role', 'img');
    expect(svg).not.toHaveAttribute('aria-hidden');
  });
});
