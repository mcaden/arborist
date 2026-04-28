// Cross-platform path utilities used by the worktree UI. Kept in a
// dedicated module so the helpers can be unit-tested without dragging in
// React.
//
// We deliberately do *not* try to be a full path library — that would
// pull in node-only deps. Instead we implement the narrow comparisons
// the UI needs, with explicit handling for both POSIX and Windows
// conventions.

/**
 * Heuristic: is the path a Windows-style absolute path (drive letter or
 * UNC)? Used to decide whether comparisons should be case-insensitive.
 * Pure POSIX paths are always compared case-sensitively.
 */
function isWindowsLikePath(p: string): boolean {
  return /^[A-Za-z]:[\\/]/.test(p) || /^[\\/]{2}/.test(p);
}

/** Normalize separators to `/` and strip trailing slashes. */
function normalize(p: string): string {
  // Replacing backslashes globally is safe enough for our purpose:
  // backslashes in literal POSIX file names are rare, and the inputs to
  // this helper come from `git worktree list --porcelain` and our own
  // workspace-root config — neither produces such names in practice.
  return p.replace(/\\/g, '/').replace(/\/+$/, '');
}

/**
 * `true` iff `child` is *strictly* inside `<root>/.worktrees/` — i.e.
 * `<root>/.worktrees/<at least one component>`. Both `/` and `\`
 * separators are accepted on either side, and on Windows-style paths
 * the comparison is case-insensitive (to match Windows filesystem
 * semantics where `C:\Repo` and `c:\repo` refer to the same directory).
 */
export function isInsideWorktreesDir(root: string, child: string): boolean {
  const r = normalize(root);
  const c = normalize(child);
  const prefix = `${r}/.worktrees/`;
  const winLike = isWindowsLikePath(r) || isWindowsLikePath(c);
  if (winLike) {
    const cl = c.toLowerCase();
    const pl = prefix.toLowerCase();
    return cl.startsWith(pl) && cl.length > pl.length;
  }
  return c.startsWith(prefix) && c.length > prefix.length;
}
