import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { StatusIcon } from './StatusIcon';
import type { DisplayStatus } from '@/store/session-store';

describe('StatusIcon', () => {
  // Every non-idle DisplayStatus variant must have a glyph; the test ids
  // below are kebab-case (the camelCase enum values are normalised in
  // `StatusIcon.tsx`'s `STATUS_META` entries via their `testIdSuffix` values).
  const cases: ReadonlyArray<{ status: Exclude<DisplayStatus, 'idle'>; testIdSuffix: string }> = [
    { status: 'starting', testIdSuffix: 'starting' },
    { status: 'working', testIdSuffix: 'working' },
    { status: 'awaiting', testIdSuffix: 'awaiting' },
    { status: 'attention', testIdSuffix: 'attention' },
    { status: 'awaitingPermission', testIdSuffix: 'awaiting-permission' },
    { status: 'runningTool', testIdSuffix: 'running-tool' },
    { status: 'thinking', testIdSuffix: 'thinking' },
    { status: 'exited', testIdSuffix: 'exited' },
    { status: 'error', testIdSuffix: 'error' },
  ];

  it.each(cases)('renders the $status glyph with a stable testid', ({ status, testIdSuffix }) => {
    render(<StatusIcon status={status} />);
    expect(screen.getByTestId(`status-icon-${testIdSuffix}`)).toBeInTheDocument();
  });

  it('renders nothing for idle (quiescent sessions stay quiet)', () => {
    const { container } = render(<StatusIcon status="idle" />);
    expect(container).toBeEmptyDOMElement();
  });

  it.each(cases)('emits a single-codepoint Nerd Font codicon glyph for $status', ({ status, testIdSuffix }) => {
    render(<StatusIcon status={status} />);
    const text = screen.getByTestId(`status-icon-${testIdSuffix}`).textContent ?? '';
    // Single codepoint: surrogate-pair-aware so accidental swaps to a
    // higher-plane glyph (e.g. material-design icons U+F0001+) still
    // count as one codepoint.
    expect([...text]).toHaveLength(1);
    const codepoint = text.codePointAt(0);
    expect(codepoint).toBeDefined();
    // Codicons live in the U+EA60–U+EC1E PUA range. Asserting the range
    // catches accidental fall-back to ASCII (e.g. typing the literal
    // character instead of the escape) and warns if a future edit pulls
    // a glyph from the wrong Nerd Font block.
    expect(codepoint!).toBeGreaterThanOrEqual(0xea60);
    expect(codepoint!).toBeLessThanOrEqual(0xec1e);
  });

  it('emits a distinct glyph per non-idle status (no collisions)', () => {
    const seen = new Map<string, DisplayStatus>();
    for (const { status, testIdSuffix } of cases) {
      const { unmount } = render(<StatusIcon status={status} />);
      const text = screen.getByTestId(`status-icon-${testIdSuffix}`).textContent ?? '';
      expect(seen.has(text), `glyph for "${status}" collides with "${seen.get(text) ?? ''}"`).toBe(false);
      seen.set(text, status);
      unmount();
    }
  });

  it('forwards the className to the rendered element', () => {
    render(<StatusIcon status="working" className="text-emerald-500" />);
    const el = screen.getByTestId('status-icon-working');
    expect(el).toHaveClass('text-emerald-500');
  });

  it('pins the glyph box to a 1em square so the icon column does not shift on state changes', () => {
    render(<StatusIcon status="working" />);
    const el = screen.getByTestId('status-icon-working');
    // `inline-block w-[1em] text-center leading-none` is what keeps the
    // unread-overlay dot anchored — assert each piece so accidental
    // removal of a class is caught here, not in a manual visual check.
    expect(el).toHaveClass('inline-block');
    expect(el).toHaveClass('w-[1em]');
    expect(el).toHaveClass('text-center');
    expect(el).toHaveClass('leading-none');
  });

  it('pins the glyph to the dedicated CaskaydiaCove NF icon family (not the body sans stack)', () => {
    // `font-icon` is a Tailwind family containing only `CaskaydiaCove NF`
    // (no system fallback) — see `tailwind.config.js`. Asserting it
    // explicitly catches accidental regressions back to `font-sans`,
    // whose stack reorganisation could silently swap to a font that
    // lacks the codicon PUA glyphs.
    render(<StatusIcon status="working" />);
    expect(screen.getByTestId('status-icon-working')).toHaveClass('font-icon');
  });

  it('emits the title as a native title attribute for hover tooltips', () => {
    render(<StatusIcon status="awaiting" title="Awaiting input" />);
    expect(screen.getByTestId('status-icon-awaiting')).toHaveAttribute('title', 'Awaiting input');
  });

  it('hides the icon from assistive tech when no title is supplied', () => {
    // Mixing aria-label with aria-hidden=true is a contradiction; the
    // sidebar tab itself is the labelled element, so an unlabelled icon
    // should be silently decorative.
    render(<StatusIcon status="working" />);
    const el = screen.getByTestId('status-icon-working');
    expect(el).toHaveAttribute('aria-hidden', 'true');
    expect(el).not.toHaveAttribute('aria-label');
    expect(el).not.toHaveAttribute('role');
  });

  it('promotes to role=img and exposes aria-label when titled', () => {
    render(<StatusIcon status="working" title="Working" />);
    const el = screen.getByTestId('status-icon-working');
    expect(el).toHaveAttribute('aria-label', 'Working');
    expect(el).toHaveAttribute('role', 'img');
    expect(el).not.toHaveAttribute('aria-hidden');
  });

  it('treats an empty-string title as decorative (no contradictory aria signals)', () => {
    // `??` does not coerce empty strings to undefined, so the previous
    // `aria-label={title ?? undefined}` paired with `aria-hidden={title ? … : true}`
    // emitted both `aria-label=""` and `aria-hidden="true"` for `title=""`,
    // which screen readers warn on. Treat empty as absent.
    render(<StatusIcon status="working" title="" />);
    const el = screen.getByTestId('status-icon-working');
    expect(el).toHaveAttribute('aria-hidden', 'true');
    expect(el).not.toHaveAttribute('aria-label');
    expect(el).not.toHaveAttribute('role');
    expect(el).not.toHaveAttribute('title');
  });

  it('treats a whitespace-only title as decorative', () => {
    render(<StatusIcon status="working" title="   " />);
    const el = screen.getByTestId('status-icon-working');
    expect(el).toHaveAttribute('aria-hidden', 'true');
    expect(el).not.toHaveAttribute('aria-label');
    expect(el).not.toHaveAttribute('role');
    expect(el).not.toHaveAttribute('title');
  });

  it('trims surrounding whitespace from a meaningful title', () => {
    render(<StatusIcon status="working" title="  Working  " />);
    const el = screen.getByTestId('status-icon-working');
    expect(el).toHaveAttribute('aria-label', 'Working');
    expect(el).toHaveAttribute('title', 'Working');
    expect(el).toHaveAttribute('role', 'img');
    expect(el).not.toHaveAttribute('aria-hidden');
  });
});
