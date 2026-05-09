// Git-branch style "trunk + branch + node" decoration drawn on the left
// edge of a sidebar child row. Mirrors the branch motif used in the
// Arborist logo so AI-session and sub-session rows visually nest under
// their parent worktree header.
//
// Layout (relative to the host `<li>`, which is `position: relative`):
//
//                  ┌─── li top
//   trunk ─────────┤
//                  │   ●─── connector + node (vertical centre)
//                  │
//                  └─── li bottom (or stops at the node when isLastInGroup)
//
// The trunk overhangs li-top and li-bottom by 2px so the 2px flex `gap`
// between sibling rows doesn't break the rail. When the row is the last
// child of its group, the trunk stops at the node centre instead — that
// gives the "└" elbow shape that signals end-of-group.
//
// All elements are `aria-hidden`; assistive tech reads the row's button
// directly.

interface BranchDecorationProps {
  isLastInGroup: boolean;
}

export function BranchDecoration({ isLastInGroup }: BranchDecorationProps): JSX.Element {
  // Trunk: full-height for non-last children (overhangs the 2px gap on
  // both ends so consecutive rails read as one continuous line). For the
  // last child, terminate at the node centre — the rail becomes an "└".
  const trunkClasses = isLastInGroup
    ? 'pointer-events-none absolute left-0 -top-0.5 w-px h-[calc(50%+2px)] bg-slate-300 dark:bg-slate-600'
    : 'pointer-events-none absolute left-0 -top-0.5 -bottom-0.5 w-px bg-slate-300 dark:bg-slate-600';

  return (
    <>
      <span aria-hidden="true" className={trunkClasses} />
      <span aria-hidden="true" className="pointer-events-none absolute left-0 top-1/2 h-px w-3 -translate-y-1/2 bg-slate-300 dark:bg-slate-600" />
      <span
        aria-hidden="true"
        className="pointer-events-none absolute left-3 top-1/2 h-1.5 w-1.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-slate-400 ring-2 ring-slate-50 dark:bg-slate-400 dark:ring-slate-900"
      />
    </>
  );
}
