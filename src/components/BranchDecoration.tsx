import type { CSSProperties } from 'react';

// Git-branch style "trunk + diagonal branch + node" decoration drawn on
// the left edge of a sidebar child row. Mirrors the branch motif used in
// the Arborist logo so AI-session and sub-session rows visually nest
// under their parent worktree header.
//
// Layout (relative to the host `<li>`, which is `position: relative`):
//
//   non-last child            last child of group
//
//   ┃                         ┃
//   ┃╲                        ┃╲
//   ┃ ●  (45° branch + node)   ╲
//   ┃                           ●
//   ┃
//
// Every row has the same diagonal "branch" peeling off the trunk into
// its node. The trunk overhangs li-top and li-bottom by 2px on non-last
// rows so the 2px flex `gap` between siblings doesn't break the rail.
// On the last row the trunk stops at the diagonal's junction point so
// the rail visibly terminates rather than dangling past the final node.
//
// All elements are `aria-hidden`; assistive tech reads the row's button
// directly.

// Geometry constants (px). All strokes (trunk + diagonal) are `STROKE`px
// thick; the diagonal travels `DIAG` px on each axis from the trunk's
// centre line down-and-right to the node centre.
const STROKE = 4;
const DIAG = 12;
// Trunk centre-line x in px (so the diagonal originates on the trunk's
// centre, not its left edge).
const TRUNK_CX = STROKE / 2;
// Node centre x in px.
const NODE_CX = TRUNK_CX + DIAG;

interface BranchDecorationProps {
  isLastInGroup: boolean;
  /**
   * Vertical position (CSS length) inside the host `<li>` where the
   * diagonal branch and node should land. Defaults to `'50%'` (the row's
   * vertical centre, correct for single-line rows like sub-session
   * tabs). AI-session rows pass a px value matching the icon's centre
   * so the branch peels off into the icon, not into the gap between
   * the icon and the metrics line below it.
   */
  anchorTop?: string;
}

export function BranchDecoration({ isLastInGroup, anchorTop = '50%' }: BranchDecorationProps): JSX.Element {
  // Trunk styling: full-height for non-last children (overhangs the 2px
  // flex gap on both ends so consecutive rails read as one continuous
  // line). For the last child, terminate at the diagonal's junction
  // point — `DIAG` px above the anchor.
  const trunkStyle: CSSProperties = isLastInGroup
    ? { width: `${STROKE}px`, top: '-2px', height: `calc(${anchorTop} - ${DIAG - 2}px)` }
    : { width: `${STROKE}px`, top: '-2px', bottom: '-2px' };

  return (
    <>
      <span aria-hidden="true" className="pointer-events-none absolute left-0 bg-slate-300 dark:bg-slate-600" style={trunkStyle} />
      {/* Diagonal SVG: anchored so its (TRUNK_CX, 0) sits at the trunk's
          centre `DIAG` px above the anchor, and (NODE_CX, DIAG) lands on
          the node centre at (NODE_CX, anchorTop). `stroke-linecap="round"`
          gives smooth joints with both the trunk above and the node. */}
      <svg
        aria-hidden="true"
        className="pointer-events-none absolute left-0 text-slate-300 dark:text-slate-600"
        style={{
          top: `calc(${anchorTop} - ${DIAG}px)`,
          width: `${NODE_CX + STROKE}px`,
          height: `${DIAG + STROKE}px`,
        }}
        viewBox={`0 0 ${NODE_CX + STROKE} ${DIAG + STROKE}`}
      >
        <line x1={TRUNK_CX} y1={0} x2={NODE_CX} y2={DIAG} stroke="currentColor" strokeWidth={STROKE} strokeLinecap="round" />
      </svg>
      <span
        aria-hidden="true"
        className="pointer-events-none absolute h-2.5 w-2.5 rounded-full bg-slate-400 ring-2 ring-slate-50 dark:bg-slate-400 dark:ring-slate-900"
        style={{ left: `${NODE_CX}px`, top: anchorTop, transform: 'translate(-50%, -50%)' }}
      />
    </>
  );
}
