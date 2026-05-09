// Behavioural tests for `BranchDecoration`. The component is purely
// decorative (`aria-hidden`) so we assert on the DOM structure and the
// inline geometry styles that drive the visual rail — those are what
// would silently regress if the trunk/diagonal/node geometry drifted.

import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { BranchDecoration } from './BranchDecoration';

function renderDecoration(props: { isLastInGroup: boolean; anchorTop?: string }): HTMLElement {
  // Wrap in a relatively-positioned host so the decoration's `absolute`
  // children resolve their geometry against a known container.
  const { container } = render(
    <div style={{ position: 'relative', width: 200, height: 60 }}>
      <BranchDecoration {...props} />
    </div>,
  );
  return container.firstElementChild as HTMLElement;
}

describe('BranchDecoration', () => {
  it('renders trunk, diagonal SVG, and node, all aria-hidden', () => {
    const host = renderDecoration({ isLastInGroup: false });
    const children = Array.from(host.children) as HTMLElement[];
    expect(children).toHaveLength(3);
    for (const el of children) {
      expect(el.getAttribute('aria-hidden')).toBe('true');
    }
    // Second child is the diagonal SVG; first and third are spans for trunk + node.
    expect(children[0]!.tagName).toBe('SPAN');
    expect(children[1]!.tagName).toBe('svg');
    expect(children[2]!.tagName).toBe('SPAN');
    // Diagonal goes from trunk centre at top to node centre `DIAG` below — i.e. 45°.
    const line = children[1]!.querySelector('line')!;
    const x1 = Number(line.getAttribute('x1'));
    const y1 = Number(line.getAttribute('y1'));
    const x2 = Number(line.getAttribute('x2'));
    const y2 = Number(line.getAttribute('y2'));
    expect(x2 - x1).toBe(y2 - y1);
    expect(y2 - y1).toBeGreaterThan(0);
    expect(line.getAttribute('stroke-width')).toBe('2');
  });

  it('non-last child draws a full-height trunk that bridges the flex gap on both ends', () => {
    const host = renderDecoration({ isLastInGroup: false });
    const trunk = host.children[0] as HTMLElement;
    expect(trunk.style.top).toBe('-2px');
    expect(trunk.style.bottom).toBe('-2px');
    // No explicit height — top+bottom alone span the row.
    expect(trunk.style.height).toBe('');
  });

  it('last child terminates the trunk at the diagonal junction (anchor - DIAG)', () => {
    const host = renderDecoration({ isLastInGroup: true });
    const trunk = host.children[0] as HTMLElement;
    expect(trunk.style.top).toBe('-2px');
    // Trunk does NOT extend to the bottom — it stops where the diagonal starts.
    expect(trunk.style.bottom).toBe('');
    // Default anchorTop is 50%; trunk height = anchorTop - (DIAG - 2)px = 50% - 6px.
    expect(trunk.style.height).toBe('calc(50% - 6px)');
  });

  it('honours a non-default anchorTop for both the diagonal SVG and the node', () => {
    const host = renderDecoration({ isLastInGroup: false, anchorTop: '18px' });
    const svg = host.children[1] as SVGElement;
    const node = host.children[2] as HTMLElement;
    // SVG sits `DIAG` (8) px above the anchor so its diagonal lands on it.
    // jsdom normalises `calc(18px - 8px)` to `calc(10px)`.
    expect(svg.style.top).toBe('calc(10px)');
    // Node centred on the anchor (translate handles the centring).
    expect(node.style.top).toBe('18px');
    expect(node.style.transform).toContain('translate(-50%, -50%)');
  });

  it('positions the node at the trunk-centre + DIAG horizontally', () => {
    const host = renderDecoration({ isLastInGroup: false });
    const node = host.children[2] as HTMLElement;
    // STROKE/2 (1) + DIAG (8) = 9px from the host's left edge.
    expect(node.style.left).toBe('9px');
  });
});
